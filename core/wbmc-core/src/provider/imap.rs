// This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0. If a copy of the MPL was not distributed with this file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use async_imap::imap_proto::{BodyContentCommon, BodyStructure, NameAttribute};
use async_imap::types::{Fetch, Flag, Name};
use async_imap::{Client, Session};
use async_trait::async_trait;
use futures::TryStreamExt;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls::pki_types::ServerName;

use super::{FolderDelta, FolderSyncState, MailProvider, RemoteFolder, RemoteMessage};
use crate::mime::parse_message;
use crate::model::{Address, FolderRole};
use crate::{CoreError, Result};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const SUMMARY_HEADERS: &str = "(UID FLAGS INTERNALDATE ENVELOPE BODYSTRUCTURE BODY.PEEK[HEADER])";
/// GreenMail answers `BODY[TEXT]<0>{n}` with no space before the literal, which
/// `imap-proto` rejects. The partial body is read on its own connection.
const PARTIAL_TEXT: &str = "BODY.PEEK[TEXT]<0.1024>";
const FLAG_BATCH: usize = 200;

/// How the IMAP and SMTP connections are secured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Security {
    Tls,
    StartTls,
    None,
}

/// Settings stored in `Account.config` for an IMAP account.
///
/// The password is not part of this struct; it lives in the keychain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImapConfig {
    pub host: String,
    pub port: u16,
    pub security: Security,
    pub username: String,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub smtp_security: Security,
}

#[derive(Clone)]
pub struct ImapProvider {
    config: ImapConfig,
    password: String,
}

impl fmt::Debug for ImapProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ImapProvider")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl ImapProvider {
    pub fn new(config: ImapConfig, password: String) -> Self {
        Self { config, password }
    }

    async fn with_session<T>(
        &self,
        work: impl AsyncFnOnce(&mut Session<MailStream>) -> Result<T>,
    ) -> Result<T> {
        let mut session = self.login().await?;
        let result = work(&mut session).await;
        drain(&session);
        let _ = session.logout().await;
        result
    }

    async fn login(&self) -> Result<Session<MailStream>> {
        let security = self.config.security;
        let stream = self.open_stream().await?;
        let mut client = Client::new(stream);
        if security != Security::StartTls {
            let greeting = tokio::time::timeout(CONNECT_TIMEOUT, client.read_response())
                .await
                .map_err(|_| CoreError::Network("timed out waiting for the IMAP greeting".into()))?
                .map_err(|err| CoreError::Network(err.to_string()))?;
            if greeting.is_none() {
                return Err(CoreError::Network(
                    "IMAP server closed before the greeting".into(),
                ));
            }
        }
        client
            .login(&self.config.username, &self.password)
            .await
            .map_err(|(err, _client)| map_login(err, &self.config.username))
    }

    async fn open_stream(&self) -> Result<MailStream> {
        match self.config.security {
            Security::None => Ok(MailStream::Plain(self.tcp().await?)),
            Security::Tls => {
                let tcp = self.tcp().await?;
                Ok(MailStream::Tls(Box::new(self.tls_connect(tcp).await?)))
            }
            Security::StartTls => self.starttls().await,
        }
    }

    async fn tcp(&self) -> Result<TcpStream> {
        let addr = (self.config.host.as_str(), self.config.port);
        tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr))
            .await
            .map_err(|_| {
                CoreError::Network(format!(
                    "timed out connecting to {}:{}",
                    self.config.host, self.config.port
                ))
            })?
            .map_err(|err| CoreError::Network(err.to_string()))
    }

    async fn starttls(&self) -> Result<MailStream> {
        let mut tcp = self.tcp().await?;
        let _greeting = read_line(&mut tcp).await?;
        tcp.write_all(b"A001 STARTTLS\r\n")
            .await
            .map_err(|err| CoreError::Network(err.to_string()))?;
        tcp.flush()
            .await
            .map_err(|err| CoreError::Network(err.to_string()))?;
        let line = read_tagged(&mut tcp, b"A001").await?;
        if !tagged_ok(&line, b"A001") {
            return Err(CoreError::Protocol(format!(
                "STARTTLS failed: {}",
                String::from_utf8_lossy(&line).trim()
            )));
        }
        Ok(MailStream::Tls(Box::new(self.tls_connect(tcp).await?)))
    }

    async fn tls_connect(&self, tcp: TcpStream) -> Result<TlsStream<TcpStream>> {
        let name = ServerName::try_from(self.config.host.as_str())
            .map_err(|_| {
                CoreError::InvalidInput(format!("invalid TLS server name: {}", self.config.host))
            })?
            .to_owned();
        tls_connector()?
            .connect(name, tcp)
            .await
            .map_err(|err| CoreError::Network(err.to_string()))
    }
}

#[async_trait]
impl MailProvider for ImapProvider {
    async fn list_folders(&self) -> Result<Vec<RemoteFolder>> {
        self.with_session(async |session| list_folders(session).await)
            .await
    }

    async fn sync_folder(
        &self,
        folder: &RemoteFolder,
        state: Option<FolderSyncState>,
        page_size: u32,
    ) -> Result<FolderDelta> {
        let config = self.config.clone();
        let password = self.password.clone();
        self.with_session(async |session| {
            sync_folder(&config, &password, session, folder, state, page_size).await
        })
        .await
    }

    async fn fetch_raw(&self, folder_remote_id: &str, message_remote_id: &str) -> Result<Vec<u8>> {
        self.with_session(async |session| {
            fetch_raw(session, folder_remote_id, message_remote_id).await
        })
        .await
    }
}

async fn list_folders(session: &mut Session<MailStream>) -> Result<Vec<RemoteFolder>> {
    let names = session
        .list(Some(""), Some("*"))
        .await
        .map_err(map_imap)?
        .try_collect::<Vec<Name>>()
        .await
        .map_err(map_imap)?;
    Ok(names.iter().map(remote_folder).collect())
}

fn remote_folder(name: &Name) -> RemoteFolder {
    let remote_id = name.name().to_string();
    let (short, parent_remote_id) = split_mailbox(&remote_id, name.delimiter());
    RemoteFolder {
        role: folder_role(&remote_id, &short, name.attributes()),
        name: short,
        remote_id,
        parent_remote_id,
    }
}

fn split_mailbox(full: &str, delimiter: Option<&str>) -> (String, Option<String>) {
    let Some(delimiter) = delimiter.filter(|value| !value.is_empty()) else {
        return (full.to_string(), None);
    };
    match full.rsplit_once(delimiter) {
        Some((parent, name)) if !parent.is_empty() && !name.is_empty() => {
            (name.to_string(), Some(parent.to_string()))
        }
        _ => (full.to_string(), None),
    }
}

fn folder_role(full: &str, short: &str, attributes: &[NameAttribute<'_>]) -> FolderRole {
    if full.eq_ignore_ascii_case("INBOX") || short.eq_ignore_ascii_case("INBOX") {
        return FolderRole::Inbox;
    }
    if let Some(role) = role_from_attributes(attributes) {
        return role;
    }
    role_from_name(short).unwrap_or(FolderRole::Other)
}

fn role_from_attributes(attributes: &[NameAttribute<'_>]) -> Option<FolderRole> {
    attributes.iter().find_map(|attribute| match attribute {
        NameAttribute::Sent => Some(FolderRole::Sent),
        NameAttribute::Drafts => Some(FolderRole::Drafts),
        NameAttribute::Trash => Some(FolderRole::Trash),
        NameAttribute::Junk => Some(FolderRole::Junk),
        NameAttribute::Archive => Some(FolderRole::Archive),
        NameAttribute::Extension(name) if name.eq_ignore_ascii_case("\\Inbox") => {
            Some(FolderRole::Inbox)
        }
        _ => None,
    })
}

fn role_from_name(name: &str) -> Option<FolderRole> {
    match name.to_ascii_lowercase().as_str() {
        "sent" | "sent items" => Some(FolderRole::Sent),
        "drafts" => Some(FolderRole::Drafts),
        "trash" | "deleted items" => Some(FolderRole::Trash),
        "junk" | "spam" => Some(FolderRole::Junk),
        "archive" => Some(FolderRole::Archive),
        _ => None,
    }
}

async fn sync_folder(
    config: &ImapConfig,
    password: &str,
    session: &mut Session<MailStream>,
    folder: &RemoteFolder,
    state: Option<FolderSyncState>,
    page_size: u32,
) -> Result<FolderDelta> {
    if page_size == 0 {
        return Err(CoreError::InvalidInput(
            "page_size must be greater than 0".into(),
        ));
    }
    let mailbox = session.examine(&folder.remote_id).await.map_err(|err| {
        if matches!(err, async_imap::error::Error::No(_)) {
            CoreError::NotFound(folder.remote_id.clone())
        } else {
            map_imap(err)
        }
    })?;
    let uid_validity = mailbox.uid_validity.ok_or_else(|| {
        CoreError::Protocol(format!("mailbox {} has no UIDVALIDITY", folder.remote_id))
    })?;
    let mut uids: Vec<u32> = session
        .uid_search("ALL")
        .await
        .map_err(map_imap)?
        .into_iter()
        .collect();
    uids.sort_unstable();

    let plan = page_plan(&uids, state, uid_validity, page_size as usize)?;
    let mut rows = fetch_summaries(session, &plan.uids).await?;
    if plan.refresh_flags {
        let known: Vec<u32> = uids
            .iter()
            .copied()
            .filter(|uid| {
                plan.known_lowest > 0 && (plan.known_lowest..=plan.known_highest).contains(uid)
            })
            .collect();
        if !known.is_empty() {
            let snaps = refresh_flags(session, &known).await?;
            // New-mail upserts already carry flags. Known messages are not
            // re-emitted: FolderSyncState has no prior flag snapshot, and an
            // incremental sync must return only mail that is new or gone.
            overlay_flags(&mut rows, &snaps);
        }
    }
    let fetched: Vec<u32> = rows
        .iter()
        .filter_map(|row| row.message.remote_id.parse().ok())
        .collect();
    let texts = partial_texts(config, password, &folder.remote_id, &fetched).await;
    for row in &mut rows {
        let Ok(uid) = row.message.remote_id.parse::<u32>() else {
            continue;
        };
        if let Some(text) = texts.get(&uid) {
            let header = (!row.header.is_empty()).then_some(row.header.as_slice());
            row.message.snippet = snippet_from(header, Some(text.as_slice()));
        }
    }
    rows.sort_by_key(|row| std::cmp::Reverse(row.message.remote_id.parse::<u32>().unwrap_or(0)));
    let upserts: Vec<RemoteMessage> = rows.into_iter().map(|row| row.message).collect();

    Ok(FolderDelta {
        upserts,
        deleted_remote_ids: plan.deleted,
        new_state: FolderSyncState::Imap {
            uid_validity,
            highest_uid: plan.highest_uid,
            lowest_uid: plan.lowest_uid,
        },
        more: plan.more,
        reset: plan.reset,
    })
}

struct PagePlan {
    uids: Vec<u32>,
    more: bool,
    lowest_uid: u32,
    highest_uid: u32,
    known_lowest: u32,
    known_highest: u32,
    deleted: Vec<String>,
    refresh_flags: bool,
    reset: bool,
}

fn page_plan(
    uids: &[u32],
    state: Option<FolderSyncState>,
    uid_validity: u32,
    page_size: usize,
) -> Result<PagePlan> {
    let (previous, reset) = match state {
        None => (None, false),
        Some(FolderSyncState::Imap {
            uid_validity: previous,
            highest_uid,
            lowest_uid,
        }) if previous == uid_validity => (Some((lowest_uid, highest_uid)), false),
        Some(FolderSyncState::Imap { .. }) => (None, true),
        Some(FolderSyncState::Graph { .. }) => {
            return Err(CoreError::InvalidInput(
                "IMAP sync received Graph folder state".into(),
            ));
        }
    };

    let Some((lowest_uid, highest_uid)) = previous else {
        let (page, more) = newest_page(uids, page_size);
        let lowest = page.first().copied().unwrap_or(0);
        let highest = page.last().copied().unwrap_or(0);
        return Ok(PagePlan {
            uids: page,
            more,
            lowest_uid: lowest,
            highest_uid: highest,
            known_lowest: 0,
            known_highest: 0,
            deleted: Vec::new(),
            refresh_flags: false,
            reset,
        });
    };

    let older: Vec<u32> = uids
        .iter()
        .copied()
        .filter(|uid| *uid < lowest_uid)
        .collect();
    let (older_page, more_older) = newest_page(&older, page_size);
    let mut newer: Vec<u32> = uids
        .iter()
        .copied()
        .filter(|uid| *uid > highest_uid)
        .collect();
    let more_newer = newer.len() > page_size;
    newer.truncate(page_size);

    let mut fetch = older_page.clone();
    fetch.extend(&newer);
    fetch.sort_unstable();
    fetch.dedup();

    let lowest = if older_page.is_empty() {
        if lowest_uid == 0 {
            newer.first().copied().unwrap_or(0)
        } else {
            lowest_uid
        }
    } else {
        older_page[0]
    };
    let highest = newer
        .last()
        .copied()
        .unwrap_or(highest_uid)
        .max(highest_uid);

    Ok(PagePlan {
        uids: fetch,
        more: more_older || more_newer,
        lowest_uid: lowest,
        highest_uid: highest,
        known_lowest: lowest_uid,
        known_highest: highest_uid,
        deleted: missing_uids(uids, lowest_uid, highest_uid),
        refresh_flags: older.is_empty(),
        reset: false,
    })
}

fn newest_page(uids: &[u32], page_size: usize) -> (Vec<u32>, bool) {
    if uids.len() <= page_size {
        (uids.to_vec(), false)
    } else {
        (uids[uids.len() - page_size..].to_vec(), true)
    }
}

fn missing_uids(present: &[u32], lowest: u32, highest: u32) -> Vec<String> {
    if lowest == 0 || highest < lowest {
        return Vec::new();
    }
    let present: HashSet<u32> = present.iter().copied().collect();
    (lowest..=highest)
        .filter(|uid| !present.contains(uid))
        .map(|uid| uid.to_string())
        .collect()
}

async fn fetch_raw(
    session: &mut Session<MailStream>,
    folder_remote_id: &str,
    message_remote_id: &str,
) -> Result<Vec<u8>> {
    let uid: u32 = message_remote_id.parse().map_err(|_| {
        CoreError::InvalidInput(format!("message id {message_remote_id} is not an IMAP UID"))
    })?;
    session
        .examine(folder_remote_id)
        .await
        .map_err(|err| map_mailbox_error(err, folder_remote_id))?;
    let fetches = fetch_query(session, &uid.to_string(), "BODY.PEEK[]")
        .await
        .map_err(map_imap)?;
    fetches
        .iter()
        .find_map(|fetch| fetch.body().map(<[u8]>::to_vec))
        .ok_or_else(|| CoreError::NotFound(message_remote_id.to_string()))
}

struct SummaryRow {
    message: RemoteMessage,
    header: Vec<u8>,
}

async fn fetch_summaries(
    session: &mut Session<MailStream>,
    uids: &[u32],
) -> Result<Vec<SummaryRow>> {
    if uids.is_empty() {
        return Ok(Vec::new());
    }
    let fetches = fetch_query(session, &uid_set(uids), SUMMARY_HEADERS)
        .await
        .map_err(map_imap)?;
    Ok(fetches.iter().filter_map(message_from_fetch).collect())
}

async fn refresh_flags(session: &mut Session<MailStream>, uids: &[u32]) -> Result<Vec<FlagSnap>> {
    let mut snaps = Vec::new();
    for chunk in uids.chunks(FLAG_BATCH) {
        let fetches = fetch_query(session, &uid_set(chunk), "(UID FLAGS)")
            .await
            .map_err(map_imap)?;
        for fetch in fetches {
            let Some(uid) = fetch.uid else { continue };
            let (is_read, is_flagged) = flag_state(&fetch);
            snaps.push(FlagSnap {
                uid,
                is_read,
                is_flagged,
            });
        }
    }
    Ok(snaps)
}

struct FlagSnap {
    uid: u32,
    is_read: bool,
    is_flagged: bool,
}

fn overlay_flags(rows: &mut [SummaryRow], snaps: &[FlagSnap]) {
    for row in rows {
        let Ok(uid) = row.message.remote_id.parse::<u32>() else {
            continue;
        };
        if let Some(snap) = snaps.iter().find(|snap| snap.uid == uid) {
            row.message.is_read = snap.is_read;
            row.message.is_flagged = snap.is_flagged;
        }
    }
}

async fn fetch_query(
    session: &mut Session<MailStream>,
    uid_set: &str,
    query: &str,
) -> std::result::Result<Vec<Fetch>, async_imap::error::Error> {
    drain(session);
    session.uid_fetch(uid_set, query).await?.try_collect().await
}

fn message_from_fetch(fetch: &Fetch) -> Option<SummaryRow> {
    let uid = fetch.uid?;
    let envelope = fetch.envelope();
    let (is_read, is_flagged) = flag_state(fetch);
    let from = envelope
        .and_then(|value| value.from.as_ref())
        .and_then(|items| items.first())
        .and_then(convert_address)
        .unwrap_or_else(unknown_sender);
    Some(SummaryRow {
        header: fetch.header().unwrap_or_default().to_vec(),
        message: RemoteMessage {
            remote_id: uid.to_string(),
            subject: subject_of(envelope),
            from,
            to: convert_list(envelope.and_then(|value| value.to.as_ref())),
            cc: convert_list(envelope.and_then(|value| value.cc.as_ref())),
            date_ms: date_ms(fetch),
            snippet: String::new(),
            is_read,
            is_flagged,
            has_attachments: fetch.bodystructure().is_some_and(has_attachments),
        },
    })
}

fn flag_state(fetch: &Fetch) -> (bool, bool) {
    let mut is_read = false;
    let mut is_flagged = false;
    for flag in fetch.flags() {
        match flag {
            Flag::Seen => is_read = true,
            Flag::Flagged => is_flagged = true,
            _ => {}
        }
    }
    (is_read, is_flagged)
}

fn subject_of(envelope: Option<&async_imap::imap_proto::Envelope<'_>>) -> String {
    envelope
        .and_then(|value| value.subject.as_deref())
        .map(lossy)
        .map(|value| decode_phrase(value.trim()))
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "(no subject)".into())
}

fn date_ms(fetch: &Fetch) -> i64 {
    if let Some(raw) = fetch.envelope().and_then(|value| value.date.as_deref())
        && let Some(millis) = date_from_header_value(&lossy(raw))
    {
        return millis;
    }
    if let Some(header) = fetch.header() {
        let parsed = parse_message(header, i64::MIN);
        if parsed.date_ms != i64::MIN {
            return parsed.date_ms;
        }
    }
    fetch
        .internal_date()
        .map(|date| date.timestamp_millis())
        .unwrap_or(0)
}

fn date_from_header_value(value: &str) -> Option<i64> {
    let raw = format!("Date: {}\r\n\r\n", value.trim());
    let parsed = parse_message(raw.as_bytes(), i64::MIN);
    (parsed.date_ms != i64::MIN).then_some(parsed.date_ms)
}

fn snippet_from(header: Option<&[u8]>, text: Option<&[u8]>) -> String {
    let Some(text) = text.filter(|text| !text.is_empty()) else {
        return String::new();
    };
    if looks_like_mime_structure(text) {
        return String::new();
    }
    let mut raw = Vec::with_capacity(header.map_or(0, <[u8]>::len) + text.len() + 4);
    if let Some(header) = header {
        raw.extend_from_slice(header);
        if !(raw.ends_with(b"\n\n") || raw.ends_with(b"\r\n\r\n")) {
            if !raw.ends_with(b"\n") {
                raw.extend_from_slice(b"\r\n");
            }
            raw.extend_from_slice(b"\r\n");
        }
    } else {
        raw.extend_from_slice(b"Subject: (snippet)\r\n\r\n");
    }
    raw.extend_from_slice(text);
    parse_message(&raw, 0).snippet
}

fn looks_like_mime_structure(text: &[u8]) -> bool {
    let start = trim_ascii_start(text);
    if start.is_empty() {
        return false;
    }
    let head = &start[..start.len().min(1024)];
    let lower = String::from_utf8_lossy(head).to_ascii_lowercase();
    let trimmed = lower.trim();
    trimmed.starts_with("--")
        || trimmed.starts_with("content-type:")
        || trimmed.starts_with("content-transfer-encoding:")
        || trimmed.starts_with("mime-version:")
        || trimmed.starts_with("this is a multi-part message")
        || trimmed.starts_with("this is a multipart message")
        || (trimmed.contains("\ncontent-type:") && trimmed.contains("\n--"))
}

fn has_attachments(structure: &BodyStructure<'_>) -> bool {
    fn walk(part: &BodyStructure<'_>) -> bool {
        match part {
            BodyStructure::Multipart { bodies, .. } => bodies.iter().any(walk),
            BodyStructure::Text { common, .. } => disposition_attachment(common),
            BodyStructure::Basic { common, .. } => {
                disposition_attachment(common) || leaf_has_filename(common)
            }
            BodyStructure::Message { common, body, .. } => {
                disposition_attachment(common) || leaf_has_filename(common) || walk(body)
            }
        }
    }
    walk(structure)
}

fn disposition_attachment(common: &BodyContentCommon<'_>) -> bool {
    common
        .disposition
        .as_ref()
        .is_some_and(|disposition| disposition.ty.eq_ignore_ascii_case("attachment"))
}

fn leaf_has_filename(common: &BodyContentCommon<'_>) -> bool {
    named_param(
        common
            .disposition
            .as_ref()
            .map(|disposition| &disposition.params),
    ) || named_param(Some(&common.ty.params))
}

fn named_param(params: Option<&async_imap::imap_proto::BodyParams<'_>>) -> bool {
    params
        .and_then(|params| params.as_ref())
        .is_some_and(|pairs| {
            pairs.iter().any(|(key, value)| {
                !value.is_empty()
                    && (key.eq_ignore_ascii_case("filename") || key.eq_ignore_ascii_case("name"))
            })
        })
}

fn convert_list(list: Option<&Vec<async_imap::imap_proto::Address<'_>>>) -> Vec<Address> {
    list.map(|items| items.iter().filter_map(convert_address).collect())
        .unwrap_or_default()
}

fn convert_address(address: &async_imap::imap_proto::Address<'_>) -> Option<Address> {
    let mailbox = address
        .mailbox
        .as_deref()
        .map(lossy)
        .filter(|value| !value.is_empty());
    let host = address
        .host
        .as_deref()
        .map(lossy)
        .filter(|value| !value.is_empty());
    let email = match (mailbox, host) {
        (Some(mailbox), Some(host)) => format!("{mailbox}@{host}"),
        (Some(mailbox), None) => mailbox,
        _ => return None,
    };
    let name = address
        .name
        .as_deref()
        .map(lossy)
        .map(|value| decode_phrase(value.trim()))
        .filter(|value| !value.is_empty());
    Some(Address { name, email })
}

fn unknown_sender() -> Address {
    Address {
        name: Some("Unknown sender".into()),
        email: String::new(),
    }
}

fn decode_phrase(value: &str) -> String {
    if !value.contains("=?") {
        return value.to_string();
    }
    let raw = format!("Subject: {value}\r\n\r\n");
    let parsed = parse_message(raw.as_bytes(), 0);
    if parsed.subject == "(no subject)" {
        value.to_string()
    } else {
        parsed.subject
    }
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn uid_set(uids: &[u32]) -> String {
    if uids.is_empty() {
        return String::new();
    }
    let mut sorted = uids.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let mut parts = Vec::new();
    let mut start = sorted[0];
    let mut previous = sorted[0];
    for uid in sorted.into_iter().skip(1) {
        if uid == previous + 1 {
            previous = uid;
            continue;
        }
        parts.push(format_range(start, previous));
        start = uid;
        previous = uid;
    }
    parts.push(format_range(start, previous));
    parts.join(",")
}

fn format_range(start: u32, end: u32) -> String {
    if start == end {
        start.to_string()
    } else {
        format!("{start}:{end}")
    }
}

async fn partial_texts(
    config: &ImapConfig,
    password: &str,
    mailbox: &str,
    uids: &[u32],
) -> HashMap<u32, Vec<u8>> {
    if uids.is_empty() {
        return HashMap::new();
    }
    partial_texts_inner(config, password, mailbox, uids)
        .await
        .unwrap_or_default()
}

async fn partial_texts_inner(
    config: &ImapConfig,
    password: &str,
    mailbox: &str,
    uids: &[u32],
) -> Result<HashMap<u32, Vec<u8>>> {
    let provider = ImapProvider {
        config: config.clone(),
        password: password.to_string(),
    };
    let mut stream = provider.open_stream().await?;
    if config.security != Security::StartTls {
        let _greeting = read_line(&mut stream).await?;
    }
    write_cmd(
        &mut stream,
        &format!(
            "A001 LOGIN {} {}",
            quote_imap(&config.username),
            quote_imap(password)
        ),
    )
    .await?;
    let login = read_tagged(&mut stream, b"A001").await?;
    if !tagged_ok(&login, b"A001") {
        return Err(CoreError::Protocol(format!(
            "partial fetch login failed: {}",
            String::from_utf8_lossy(&login).trim()
        )));
    }
    write_cmd(
        &mut stream,
        &format!("A002 EXAMINE {}", quote_imap(mailbox)),
    )
    .await?;
    let examined = read_tagged(&mut stream, b"A002").await?;
    if !tagged_ok(&examined, b"A002") {
        return Err(CoreError::Protocol(format!(
            "partial fetch examine failed: {}",
            String::from_utf8_lossy(&examined).trim()
        )));
    }

    let mut texts = HashMap::new();
    for (index, chunk) in uids.chunks(50).enumerate() {
        let tag = format!("B{index:04}");
        let set = uid_set(chunk);
        write_cmd(
            &mut stream,
            &format!("{tag} UID FETCH {set} (UID {PARTIAL_TEXT})"),
        )
        .await?;
        texts.extend(read_partial_bodies(&mut stream, tag.as_bytes()).await?);
    }
    let _ = write_cmd(&mut stream, "A003 LOGOUT").await;
    Ok(texts)
}

async fn read_partial_bodies(stream: &mut MailStream, tag: &[u8]) -> Result<HashMap<u32, Vec<u8>>> {
    let mut bodies = HashMap::new();
    let mut current_uid = None;
    // Some servers put UID after the literal, on the bytes that follow it.
    let mut pending_body = None;
    loop {
        let line = read_line(stream).await?;
        if line.starts_with(tag) && line.get(tag.len()) == Some(&b' ') {
            if !tagged_ok(&line, tag) {
                return Err(CoreError::Protocol(format!(
                    "partial body fetch failed: {}",
                    String::from_utf8_lossy(&line).trim()
                )));
            }
            return Ok(bodies);
        }
        if line.starts_with(b"* ") {
            current_uid = None;
            pending_body = None;
        }
        if let Some(uid) = uid_on_line(&line) {
            current_uid = Some(uid);
            if let Some(body) = pending_body.take() {
                bodies.insert(uid, body);
            }
        }
        let text_section = contains_ignore_ascii_case(&line, b"BODY[TEXT]");
        if let Some(len) = literal_len(&line) {
            if len > 64 * 1024 {
                return Err(CoreError::Protocol(
                    "IMAP literal is larger than 64 KiB".into(),
                ));
            }
            let mut body = vec![0u8; len];
            tokio::time::timeout(CONNECT_TIMEOUT, stream.read_exact(&mut body))
                .await
                .map_err(|_| CoreError::Network("timed out reading an IMAP literal".into()))?
                .map_err(|err| CoreError::Network(err.to_string()))?;
            if text_section {
                match current_uid {
                    Some(uid) => {
                        bodies.insert(uid, body);
                    }
                    None => pending_body = Some(body),
                }
            }
        }
    }
}

fn uid_on_line(line: &[u8]) -> Option<u32> {
    let index = line
        .windows(4)
        .position(|window| window.eq_ignore_ascii_case(b"UID "))?;
    let rest = line.get(index + 4..)?;
    let digits: Vec<u8> = rest
        .iter()
        .copied()
        .take_while(|byte| byte.is_ascii_digit())
        .collect();
    std::str::from_utf8(&digits).ok()?.parse().ok()
}

fn literal_len(line: &[u8]) -> Option<usize> {
    let line = line.strip_suffix(b"\r\n")?;
    let start = line.iter().rposition(|byte| *byte == b'{')?;
    let digits = line.get(start + 1..)?;
    let digits = digits.strip_suffix(b"}")?;
    if digits.is_empty() || !digits.iter().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    std::str::from_utf8(digits).ok()?.parse().ok()
}

fn contains_ignore_ascii_case(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|window| {
        window
            .iter()
            .zip(needle)
            .all(|(left, right)| left.eq_ignore_ascii_case(right))
    })
}

fn quote_imap(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn map_mailbox_error(err: async_imap::error::Error, mailbox: &str) -> CoreError {
    if matches!(err, async_imap::error::Error::No(_)) {
        CoreError::NotFound(mailbox.to_string())
    } else {
        map_imap(err)
    }
}

fn map_imap(err: async_imap::error::Error) -> CoreError {
    match err {
        async_imap::error::Error::Io(err) => CoreError::Network(err.to_string()),
        async_imap::error::Error::ConnectionLost => {
            CoreError::Network("IMAP connection lost".into())
        }
        other => CoreError::Protocol(other.to_string()),
    }
}

fn map_login(err: async_imap::error::Error, username: &str) -> CoreError {
    match err {
        async_imap::error::Error::No(_) => CoreError::NeedsSignIn {
            account_id: username.to_string(),
        },
        other => map_imap(other),
    }
}

fn drain(session: &Session<MailStream>) {
    while session.unsolicited_responses.try_recv().is_ok() {}
}

fn tls_connector() -> Result<TlsConnector> {
    let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
    let mut roots = tokio_rustls::rustls::RootCertStore::empty();
    let loaded = rustls_native_certs::load_native_certs();
    for cert in loaded.certs {
        roots
            .add(cert)
            .map_err(|err| CoreError::Network(format!("tls root: {err}")))?;
    }
    if roots.is_empty() {
        let detail = loaded
            .errors
            .iter()
            .map(|err| err.to_string())
            .collect::<Vec<_>>()
            .join("; ");
        return Err(CoreError::Network(format!(
            "no native TLS roots available: {detail}"
        )));
    }
    let config = tokio_rustls::rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(TlsConnector::from(std::sync::Arc::new(config)))
}

async fn write_cmd(stream: &mut (impl AsyncWrite + Unpin), command: &str) -> Result<()> {
    stream
        .write_all(command.as_bytes())
        .await
        .map_err(|err| CoreError::Network(err.to_string()))?;
    if !command.ends_with("\r\n") {
        stream
            .write_all(b"\r\n")
            .await
            .map_err(|err| CoreError::Network(err.to_string()))?;
    }
    stream
        .flush()
        .await
        .map_err(|err| CoreError::Network(err.to_string()))
}

async fn read_line(stream: &mut (impl AsyncRead + Unpin)) -> Result<Vec<u8>> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let read = tokio::time::timeout(CONNECT_TIMEOUT, stream.read(&mut byte))
            .await
            .map_err(|_| CoreError::Network("timed out reading IMAP".into()))?
            .map_err(|err| CoreError::Network(err.to_string()))?;
        if read == 0 {
            return Err(CoreError::Network(
                "IMAP server closed the connection".into(),
            ));
        }
        line.push(byte[0]);
        if line.ends_with(b"\r\n") {
            return Ok(line);
        }
        if line.len() > 64 * 1024 {
            return Err(CoreError::Protocol("IMAP response line too long".into()));
        }
    }
}

async fn read_tagged(stream: &mut (impl AsyncRead + Unpin), tag: &[u8]) -> Result<Vec<u8>> {
    loop {
        let line = read_line(stream).await?;
        if line.starts_with(tag) && line.get(tag.len()) == Some(&b' ') {
            return Ok(line);
        }
    }
}

fn tagged_ok(line: &[u8], tag: &[u8]) -> bool {
    let Some(rest) = line.strip_prefix(tag) else {
        return false;
    };
    let rest = trim_ascii_start(rest);
    rest.len() >= 2 && rest[..2].eq_ignore_ascii_case(b"OK")
}

fn trim_ascii_start(bytes: &[u8]) -> &[u8] {
    let offset = bytes
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(bytes.len());
    &bytes[offset..]
}

enum MailStream {
    Plain(TcpStream),
    Tls(Box<TlsStream<TcpStream>>),
}

impl fmt::Debug for MailStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Plain(_) => f.write_str("MailStream::Plain"),
            Self::Tls(_) => f.write_str("MailStream::Tls"),
        }
    }
}

impl AsyncRead for MailStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_read(cx, buf),
            Self::Tls(stream) => Pin::new(stream).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for MailStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_write(cx, buf),
            Self::Tls(stream) => Pin::new(stream).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_flush(cx),
            Self::Tls(stream) => Pin::new(stream).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_shutdown(cx),
            Self::Tls(stream) => Pin::new(stream).poll_shutdown(cx),
        }
    }
}
