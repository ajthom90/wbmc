// This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0. If a copy of the MPL was not distributed with this file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::HashMap;
use std::fmt;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
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

use super::{FlagUpdate, FolderDelta, FolderSyncState, MailProvider, RemoteFolder, RemoteMessage};
use crate::mime::parse_message;
use crate::model::{Address, FolderRole};
use crate::{CoreError, Result};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const LOGOUT_TIMEOUT: Duration = Duration::from_secs(5);
const SUMMARY_HEADERS: &str = "(UID FLAGS INTERNALDATE ENVELOPE BODYSTRUCTURE BODY.PEEK[HEADER])";
const SUMMARY_WITH_TEXT: &str =
    "(UID FLAGS INTERNALDATE ENVELOPE BODYSTRUCTURE BODY.PEEK[HEADER] BODY.PEEK[TEXT]<0.1024>)";
/// GreenMail answers `BODY[TEXT]<0>{n}` with no space before the literal, which
/// `imap-proto` rejects. After that fetch fails, snippets are read once on a raw connection.
const PARTIAL_TEXT: &str = "BODY.PEEK[TEXT]<0.1024>";
const FLAG_BATCH: usize = 500;
/// UIDs per `UID FETCH` when reading full bodies. One round trip per chunk,
/// each under [`COMMAND_TIMEOUT`].
const BODY_BATCH: usize = 20;
const BUSY_SERVER: &str = "The mail server is busy or limiting connections; will retry";

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
    connect_timeout: Duration,
    /// Login attempts. Shared across clones so a test can observe one session.
    logins: Arc<AtomicUsize>,
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
        Self {
            config,
            password,
            connect_timeout: CONNECT_TIMEOUT,
            logins: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// How many IMAP logins this provider has started.
    ///
    /// Tests use this to prove a batch fetch shares one session. The counter is
    /// an atomic increment; it is not part of the mail-client API.
    #[doc(hidden)]
    pub fn login_count(&self) -> usize {
        self.logins.load(Ordering::Relaxed)
    }

    /// Shortens the TCP, STARTTLS-write, and TLS-handshake deadline. Tests use this
    /// so a silent peer fails quickly instead of waiting out `CONNECT_TIMEOUT`.
    #[cfg(test)]
    fn with_connect_timeout(mut self, connect_timeout: Duration) -> Self {
        self.connect_timeout = connect_timeout;
        self
    }

    async fn with_session<T>(
        &self,
        work: impl AsyncFnOnce(&mut Session<MailStream>) -> Result<T>,
    ) -> Result<T> {
        let mut session = self.login().await?;
        let result = work(&mut session).await;
        // Logout is best-effort. A timed-out LOGOUT must not discard a page or body
        // the server already returned.
        if result.is_ok() {
            note_logout(logout_session(&mut session).await);
        } else {
            let _ = logout_session(&mut session).await;
        }
        result
    }

    async fn login(&self) -> Result<Session<MailStream>> {
        self.logins.fetch_add(1, Ordering::Relaxed);
        let security = self.config.security;
        let stream = self.open_stream().await?;
        let mut client = Client::new(stream);
        if security != Security::StartTls {
            let greeting = tokio::time::timeout(COMMAND_TIMEOUT, client.read_response())
                .await
                .map_err(|_| CoreError::Network("IMAP read timed out".into()))?
                .map_err(|err| CoreError::Network(err.to_string()))?;
            if greeting.is_none() {
                return Err(CoreError::Network(
                    "IMAP server closed before the greeting".into(),
                ));
            }
        }
        tokio::time::timeout(
            COMMAND_TIMEOUT,
            client.login(&self.config.username, &self.password),
        )
        .await
        .map_err(|_| CoreError::Network("IMAP login timed out".into()))?
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
        tokio::time::timeout(self.connect_timeout, TcpStream::connect(addr))
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
        tokio::time::timeout(self.connect_timeout, async {
            tcp.write_all(b"A001 STARTTLS\r\n").await?;
            tcp.flush().await?;
            Ok::<(), std::io::Error>(())
        })
        .await
        .map_err(|_| {
            CoreError::Network(format!(
                "timed out sending STARTTLS to {}:{}",
                self.config.host, self.config.port
            ))
        })?
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
        let connector = tls_connector()?;
        tokio::time::timeout(self.connect_timeout, connector.connect(name, tcp))
            .await
            .map_err(|_| {
                CoreError::Network(format!(
                    "timed out during TLS handshake with {}:{}",
                    self.config.host, self.config.port
                ))
            })?
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
        if page_size == 0 {
            return Err(CoreError::InvalidInput(
                "page_size must be greater than 0".into(),
            ));
        }
        let mut session = self.login().await?;
        let prepared = match prepare_sync(&mut session, folder, state, page_size).await {
            Ok(prepared) => prepared,
            Err(err) => {
                let _ = logout_session(&mut session).await;
                return Err(err);
            }
        };
        match fetch_page(&mut session, &prepared.plan.uids).await {
            Ok(rows) => {
                let delta = complete_delta(&mut session, &prepared, rows).await;
                finish_session(&mut session, delta).await
            }
            Err(FetchError::TimedOut) => {
                let _ = logout_session(&mut session).await;
                Err(CoreError::Network("IMAP fetch timed out".into()))
            }
            Err(FetchError::Imap(_)) => {
                // A partial BODY[TEXT] can poison the parser (GreenMail omits the
                // required space). Drop this connection. The rest of this call uses
                // a headers fetch plus one raw snippet connection.
                drop(session);
                let mut session = self.login().await?;
                let delta = fallback_page(self, &mut session, folder, &prepared).await;
                finish_session(&mut session, delta).await
            }
        }
    }

    async fn fetch_raw(&self, folder_remote_id: &str, message_remote_id: &str) -> Result<Vec<u8>> {
        self.with_session(async |session| {
            fetch_raw(session, folder_remote_id, message_remote_id).await
        })
        .await
    }

    async fn fetch_raw_many(
        &self,
        folder_remote_id: &str,
        message_remote_ids: &[String],
    ) -> Vec<(String, Result<Vec<u8>>)> {
        if message_remote_ids.is_empty() {
            return Vec::new();
        }
        match self
            .with_session(async |session| {
                Ok(fetch_bodies(session, folder_remote_id, message_remote_ids).await)
            })
            .await
        {
            Ok(results) => results,
            Err(error) => message_remote_ids
                .iter()
                .map(|id| (id.clone(), Err(error.clone())))
                .collect(),
        }
    }
}

async fn list_folders(session: &mut Session<MailStream>) -> Result<Vec<RemoteFolder>> {
    let names = tokio::time::timeout(COMMAND_TIMEOUT, async {
        let listed = session.list(Some(""), Some("*")).await?;
        listed.try_collect::<Vec<Name>>().await
    })
    .await
    .map_err(|_| CoreError::Network("IMAP list timed out".into()))?
    .map_err(map_imap)?;
    Ok(names.iter().filter_map(remote_folder).collect())
}

fn remote_folder(name: &Name) -> Option<RemoteFolder> {
    folder_from_listing(name.name(), name.delimiter(), name.attributes())
}

fn folder_from_listing(
    raw_name: &str,
    delimiter: Option<&str>,
    attributes: &[NameAttribute<'_>],
) -> Option<RemoteFolder> {
    if mailbox_unselectable(attributes) {
        return None;
    }
    let remote_id = raw_name.to_string();
    let (short, parent_remote_id) = split_mailbox(&remote_id, delimiter);
    Some(RemoteFolder {
        role: folder_role(&remote_id, &short, attributes),
        name: decode_modified_utf7(&short),
        remote_id,
        parent_remote_id,
    })
}

fn mailbox_unselectable(attributes: &[NameAttribute<'_>]) -> bool {
    attributes.iter().any(|attribute| match attribute {
        NameAttribute::NoSelect => true,
        NameAttribute::Extension(name) if name.eq_ignore_ascii_case("\\NonExistent") => true,
        _ => false,
    })
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
    if full.eq_ignore_ascii_case("INBOX") {
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

struct Prepared {
    uids: Vec<u32>,
    uid_validity: u32,
    plan: PagePlan,
}

async fn prepare_sync(
    session: &mut Session<MailStream>,
    folder: &RemoteFolder,
    state: Option<FolderSyncState>,
    page_size: u32,
) -> Result<Prepared> {
    let mailbox = examine(session, &folder.remote_id).await?;
    let uid_validity = mailbox.uid_validity.ok_or_else(|| {
        CoreError::Protocol(format!("mailbox {} has no UIDVALIDITY", folder.remote_id))
    })?;
    let uids = search_all(session).await?;
    let plan = page_plan(&uids, state, uid_validity, page_size as usize)?;
    Ok(Prepared {
        uids,
        uid_validity,
        plan,
    })
}

async fn examine(
    session: &mut Session<MailStream>,
    mailbox: &str,
) -> Result<async_imap::types::Mailbox> {
    tokio::time::timeout(COMMAND_TIMEOUT, session.examine(mailbox))
        .await
        .map_err(|_| CoreError::Network("IMAP examine timed out".into()))?
        .map_err(|err| map_mailbox_error(err, mailbox))
}

async fn search_all(session: &mut Session<MailStream>) -> Result<Vec<u32>> {
    let mut uids: Vec<u32> = tokio::time::timeout(COMMAND_TIMEOUT, session.uid_search("ALL"))
        .await
        .map_err(|_| CoreError::Network("IMAP search timed out".into()))?
        .map_err(map_imap)?
        .into_iter()
        .collect();
    uids.sort_unstable();
    Ok(uids)
}

async fn fallback_page(
    provider: &ImapProvider,
    session: &mut Session<MailStream>,
    folder: &RemoteFolder,
    prepared: &Prepared,
) -> Result<FolderDelta> {
    examine(session, &folder.remote_id).await?;
    let mut rows = fetch_headers(session, &prepared.plan.uids).await?;
    let wanted: Vec<u32> = rows
        .iter()
        .filter_map(|row| row.message.remote_id.parse().ok())
        .collect();
    let texts = partial_texts(provider, &folder.remote_id, &wanted).await?;
    apply_snippets(&mut rows, &texts);
    complete_delta(session, prepared, rows).await
}

async fn complete_delta(
    session: &mut Session<MailStream>,
    prepared: &Prepared,
    rows: Vec<SummaryRow>,
) -> Result<FolderDelta> {
    let flag_updates = if prepared.plan.final_incremental {
        let known = known_uids(
            &prepared.uids,
            prepared.plan.known_lowest,
            prepared.plan.known_highest,
        );
        refresh_flags(session, &known).await?
    } else {
        Vec::new()
    };
    Ok(assemble_delta(prepared, rows, flag_updates))
}

fn known_uids(present: &[u32], lowest: u32, highest: u32) -> Vec<u32> {
    if lowest == 0 || highest < lowest {
        return Vec::new();
    }
    present
        .iter()
        .copied()
        .filter(|uid| (lowest..=highest).contains(uid))
        .collect()
}

fn apply_snippets(rows: &mut [SummaryRow], texts: &HashMap<u32, Vec<u8>>) {
    for row in rows {
        let Ok(uid) = row.message.remote_id.parse::<u32>() else {
            continue;
        };
        if let Some(text) = texts.get(&uid) {
            let header = (!row.header.is_empty()).then_some(row.header.as_slice());
            row.message.snippet = snippet_from(header, Some(text.as_slice()));
        }
    }
}

fn assemble_delta(
    prepared: &Prepared,
    mut rows: Vec<SummaryRow>,
    flag_updates: Vec<FlagUpdate>,
) -> FolderDelta {
    rows.sort_by_key(|row| std::cmp::Reverse(row.message.remote_id.parse::<u32>().unwrap_or(0)));
    let upserts = rows.into_iter().map(|row| row.message).collect();
    let server_remote_ids = prepared
        .plan
        .final_incremental
        .then(|| prepared.uids.iter().map(ToString::to_string).collect());
    FolderDelta {
        upserts,
        deleted_remote_ids: Vec::new(),
        flag_updates,
        server_remote_ids,
        new_state: FolderSyncState::Imap {
            uid_validity: prepared.uid_validity,
            highest_uid: prepared.plan.highest_uid,
            lowest_uid: prepared.plan.lowest_uid,
        },
        more: prepared.plan.more,
        reset: prepared.plan.reset,
    }
}

async fn finish_session(
    session: &mut Session<MailStream>,
    result: Result<FolderDelta>,
) -> Result<FolderDelta> {
    let logged_out = logout_session(session).await;
    match result {
        Err(err) => Err(err),
        Ok(delta) => {
            note_logout(logged_out);
            Ok(delta)
        }
    }
}

fn note_logout(result: Result<()>) {
    if let Err(error) = result {
        tracing::warn!("IMAP logout failed: {error}");
    }
}

async fn logout_session(session: &mut Session<MailStream>) -> Result<()> {
    drain(session);
    match tokio::time::timeout(LOGOUT_TIMEOUT, session.logout()).await {
        Err(_) => Err(CoreError::Network("IMAP logout timed out".into())),
        Ok(Err(error)) => Err(map_imap(error)),
        Ok(Ok(())) => Ok(()),
    }
}

struct PagePlan {
    uids: Vec<u32>,
    more: bool,
    lowest_uid: u32,
    highest_uid: u32,
    known_lowest: u32,
    known_highest: u32,
    /// Backfill is finished and this page has no further newer mail.
    final_incremental: bool,
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
            final_incremental: false,
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

    let more = more_older || more_newer;
    Ok(PagePlan {
        uids: fetch,
        more,
        lowest_uid: lowest,
        highest_uid: highest,
        known_lowest: lowest_uid,
        known_highest: highest_uid,
        final_incremental: older.is_empty() && !more,
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

async fn fetch_raw(
    session: &mut Session<MailStream>,
    folder_remote_id: &str,
    message_remote_id: &str,
) -> Result<Vec<u8>> {
    let uid: u32 = message_remote_id.parse().map_err(|_| {
        CoreError::InvalidInput(format!("message id {message_remote_id} is not an IMAP UID"))
    })?;
    examine(session, folder_remote_id).await?;
    let fetches = fetch_query(session, &uid.to_string(), "BODY.PEEK[]").await?;
    fetches
        .iter()
        .find_map(|fetch| fetch.body().map(<[u8]>::to_vec))
        .ok_or_else(|| CoreError::NotFound(message_remote_id.to_string()))
}

async fn fetch_bodies(
    session: &mut Session<MailStream>,
    folder_remote_id: &str,
    message_remote_ids: &[String],
) -> Vec<(String, Result<Vec<u8>>)> {
    let mut slots: Vec<Option<Result<Vec<u8>>>> =
        (0..message_remote_ids.len()).map(|_| None).collect();
    let mut pending = Vec::new();
    for (index, id) in message_remote_ids.iter().enumerate() {
        match id.parse::<u32>() {
            Ok(uid) => pending.push((index, uid)),
            Err(_) => {
                slots[index] = Some(Err(CoreError::InvalidInput(format!(
                    "message id {id} is not an IMAP UID"
                ))));
            }
        }
    }
    if pending.is_empty() {
        return finish_bodies(message_remote_ids, slots);
    }
    if let Err(error) = examine(session, folder_remote_id).await {
        for (index, _) in &pending {
            slots[*index] = Some(Err(error.clone()));
        }
        return finish_bodies(message_remote_ids, slots);
    }
    // A timeout or a dropped connection leaves the stream out of step, so later
    // chunks on this session would be garbage. Mark them with the same error.
    let mut blocked: Option<CoreError> = None;
    for chunk in pending.chunks(BODY_BATCH) {
        if let Some(error) = &blocked {
            for (index, _) in chunk {
                slots[*index] = Some(Err(error.clone()));
            }
            continue;
        }
        let uids: Vec<u32> = chunk.iter().map(|(_, uid)| *uid).collect();
        match fetch_query(session, &uid_set(&uids), "BODY.PEEK[]").await {
            Err(error) => {
                for (index, _) in chunk {
                    slots[*index] = Some(Err(error.clone()));
                }
                if matches!(error, CoreError::Network(_)) {
                    blocked = Some(error);
                }
            }
            Ok(fetches) => {
                let mut bodies: HashMap<u32, Vec<u8>> = HashMap::new();
                for fetch in fetches {
                    let Some(uid) = fetch.uid else { continue };
                    if !chunk.iter().any(|(_, wanted)| *wanted == uid) {
                        continue;
                    }
                    if let Some(body) = fetch.body() {
                        bodies.insert(uid, body.to_vec());
                    }
                }
                for (index, uid) in chunk {
                    slots[*index] = Some(match bodies.get(uid) {
                        Some(body) => Ok(body.clone()),
                        None => Err(CoreError::NotFound(message_remote_ids[*index].clone())),
                    });
                }
            }
        }
    }
    finish_bodies(message_remote_ids, slots)
}

fn finish_bodies(
    ids: &[String],
    slots: Vec<Option<Result<Vec<u8>>>>,
) -> Vec<(String, Result<Vec<u8>>)> {
    ids.iter()
        .zip(slots)
        .map(|(id, slot)| {
            (
                id.clone(),
                slot.unwrap_or_else(|| {
                    Err(CoreError::Protocol(format!(
                        "missing fetch result for {id}"
                    )))
                }),
            )
        })
        .collect()
}

struct SummaryRow {
    message: RemoteMessage,
    header: Vec<u8>,
}

enum FetchError {
    TimedOut,
    Imap(async_imap::error::Error),
}

async fn fetch_page(
    session: &mut Session<MailStream>,
    uids: &[u32],
) -> std::result::Result<Vec<SummaryRow>, FetchError> {
    if uids.is_empty() {
        return Ok(Vec::new());
    }
    let fetches = fetch_query_raw(session, &uid_set(uids), SUMMARY_WITH_TEXT).await?;
    Ok(fetches.iter().filter_map(message_from_fetch).collect())
}

async fn fetch_headers(session: &mut Session<MailStream>, uids: &[u32]) -> Result<Vec<SummaryRow>> {
    if uids.is_empty() {
        return Ok(Vec::new());
    }
    let fetches = fetch_query(session, &uid_set(uids), SUMMARY_HEADERS).await?;
    Ok(fetches.iter().filter_map(message_from_fetch).collect())
}

async fn refresh_flags(session: &mut Session<MailStream>, uids: &[u32]) -> Result<Vec<FlagUpdate>> {
    let mut updates = Vec::new();
    for chunk in uids.chunks(FLAG_BATCH) {
        let fetches = fetch_query(session, &uid_set(chunk), "(UID FLAGS)").await?;
        for fetch in fetches {
            let Some(uid) = fetch.uid else { continue };
            let (is_read, is_flagged) = flag_state(&fetch);
            updates.push(FlagUpdate {
                remote_id: uid.to_string(),
                is_read,
                is_flagged,
            });
        }
    }
    Ok(updates)
}

async fn fetch_query(
    session: &mut Session<MailStream>,
    uid_set: &str,
    query: &str,
) -> Result<Vec<Fetch>> {
    match fetch_query_raw(session, uid_set, query).await {
        Ok(fetches) => Ok(fetches),
        Err(FetchError::TimedOut) => Err(CoreError::Network("IMAP fetch timed out".into())),
        Err(FetchError::Imap(err)) => Err(map_imap(err)),
    }
}

async fn fetch_query_raw(
    session: &mut Session<MailStream>,
    uid_set: &str,
    query: &str,
) -> std::result::Result<Vec<Fetch>, FetchError> {
    drain(session);
    let pending = async {
        let stream = session.uid_fetch(uid_set, query).await?;
        stream.try_collect::<Vec<Fetch>>().await
    };
    match tokio::time::timeout(COMMAND_TIMEOUT, pending).await {
        Ok(Ok(fetches)) => Ok(fetches),
        Ok(Err(err)) => Err(FetchError::Imap(err)),
        Err(_) => Err(FetchError::TimedOut),
    }
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
            snippet: snippet_from(fetch.header(), fetch.text()),
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
    provider: &ImapProvider,
    mailbox: &str,
    uids: &[u32],
) -> Result<HashMap<u32, Vec<u8>>> {
    if uids.is_empty() {
        return Ok(HashMap::new());
    }
    let user = quote_imap(&provider.config.username)?;
    let password = quote_imap(&provider.password)?;
    let mailbox = quote_imap(mailbox)?;
    let mut stream = provider.open_stream().await?;
    if provider.config.security != Security::StartTls {
        let _greeting = read_line(&mut stream).await?;
    }
    write_cmd(&mut stream, &format!("A001 LOGIN {user} {password}")).await?;
    let login = command_deadline(read_tagged(&mut stream, b"A001")).await?;
    if !tagged_ok(&login, b"A001") {
        return Err(CoreError::Protocol(format!(
            "partial fetch login failed: {}",
            String::from_utf8_lossy(&login).trim()
        )));
    }
    write_cmd(&mut stream, &format!("A002 EXAMINE {mailbox}")).await?;
    let examined = command_deadline(read_tagged(&mut stream, b"A002")).await?;
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
        let bodies = command_deadline(read_partial_bodies(
            &mut stream,
            tag.as_bytes(),
            chunk.len(),
        ))
        .await?;
        // Keep only UIDs we asked for, so a misbehaving server cannot grow this map.
        texts.extend(bodies.into_iter().filter(|(uid, _)| chunk.contains(uid)));
    }
    // The bodies are already in hand. A LOGOUT failure must not drop them.
    note_logout(raw_logout(&mut stream).await);
    Ok(texts)
}

/// Bounds a whole raw command, not just each read, so a server that drips bytes
/// or streams endless untagged lines cannot hold a sync open indefinitely.
async fn command_deadline<T>(work: impl Future<Output = Result<T>>) -> Result<T> {
    command_deadline_within(COMMAND_TIMEOUT, work).await
}

async fn command_deadline_within<T>(
    limit: Duration,
    work: impl Future<Output = Result<T>>,
) -> Result<T> {
    tokio::time::timeout(limit, work)
        .await
        .map_err(|_| CoreError::Network("IMAP command timed out".into()))?
}

async fn raw_logout(stream: &mut MailStream) -> Result<()> {
    write_cmd(stream, "A003 LOGOUT").await?;
    tokio::time::timeout(LOGOUT_TIMEOUT, read_tagged(stream, b"A003"))
        .await
        .map_err(|_| CoreError::Network("IMAP logout timed out".into()))??;
    Ok(())
}

async fn read_partial_bodies(
    stream: &mut MailStream,
    tag: &[u8],
    max_bodies: usize,
) -> Result<HashMap<u32, Vec<u8>>> {
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
        if bodies.len() > max_bodies {
            return Err(CoreError::Protocol(
                "IMAP server returned more bodies than requested".into(),
            ));
        }
        let text_section = contains_ignore_ascii_case(&line, b"BODY[TEXT]");
        if let Some(len) = literal_len(&line) {
            if len > 64 * 1024 {
                return Err(CoreError::Protocol(
                    "IMAP literal is larger than 64 KiB".into(),
                ));
            }
            let mut body = vec![0u8; len];
            tokio::time::timeout(COMMAND_TIMEOUT, stream.read_exact(&mut body))
                .await
                .map_err(|_| CoreError::Network("IMAP read timed out".into()))?
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

fn quote_imap(value: &str) -> Result<String> {
    if value.chars().any(|ch| matches!(ch, '\r' | '\n' | '\0')) {
        return Err(CoreError::InvalidInput(
            "IMAP quoted string contains CR, LF, or NUL".into(),
        ));
    }
    Ok(format!(
        "\"{}\"",
        value.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}

fn decode_modified_utf7(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'&' {
            let ch = input[index..].chars().next().unwrap_or('\u{FFFD}');
            out.push(ch);
            index += ch.len_utf8();
            continue;
        }
        let encoded_at = index + 1;
        if encoded_at >= bytes.len() {
            out.push('&');
            break;
        }
        if bytes[encoded_at] == b'-' {
            out.push('&');
            index = encoded_at + 1;
            continue;
        }
        let end = bytes[encoded_at..]
            .iter()
            .position(|byte| *byte == b'-')
            .map(|offset| encoded_at + offset);
        let (encoded, next) = match end {
            Some(end) => (&input[encoded_at..end], end + 1),
            None => (&input[encoded_at..], bytes.len()),
        };
        match decode_modified_base64(encoded) {
            Some(raw) => out.push_str(&utf16be_lossy(&raw)),
            None => {
                out.push('&');
                out.push_str(encoded);
                if end.is_some() {
                    out.push('-');
                }
            }
        }
        index = next;
    }
    out
}

fn decode_modified_base64(input: &str) -> Option<Vec<u8>> {
    if input.is_empty() {
        return None;
    }
    let mut bits = 0u32;
    let mut bit_count = 0u32;
    let mut out = Vec::new();
    for ch in input.chars() {
        let value = match ch {
            'A'..='Z' => ch as u32 - 'A' as u32,
            'a'..='z' => ch as u32 - 'a' as u32 + 26,
            '0'..='9' => ch as u32 - '0' as u32 + 52,
            '+' => 62,
            ',' => 63,
            _ => return None,
        };
        bits = (bits << 6) | value;
        bit_count += 6;
        if bit_count >= 8 {
            bit_count -= 8;
            out.push((bits >> bit_count) as u8);
            bits &= (1 << bit_count) - 1;
        }
    }
    Some(out)
}

fn utf16be_lossy(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|chunk| u16::from_be_bytes(*chunk))
        .collect();
    String::from_utf16_lossy(&units)
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
        async_imap::error::Error::No(text) => map_login_response(&text, username, true),
        async_imap::error::Error::Bad(text) => map_login_response(&text, username, false),
        other => map_imap(other),
    }
}

/// `no_response` is true for `NO` and false for `BAD`.
///
/// NeedsSignIn is a bad password or a sign-in prompt: `NO [AUTHENTICATIONFAILED]`,
/// `NO [AUTHORIZATIONFAILED]`, `NO [ALERT]` (Gmail's "log in via your web browser"),
/// or a `NO` with no response code. Throttling (`[UNAVAILABLE]`, `[LIMIT]`,
/// `[INUSE]`, `[EXPIRED]`, `[SERVERBUG]`, or text containing "too many" /
/// "try again") is a retryable network error.
fn map_login_response(raw: &str, username: &str, no_response: bool) -> CoreError {
    let text = imap_info_text(raw);
    let code = response_code(&text);
    let needs_sign_in = || CoreError::NeedsSignIn {
        account_id: username.to_string(),
    };
    if no_response
        && matches!(
            code.as_deref(),
            Some("AUTHENTICATIONFAILED") | Some("AUTHORIZATIONFAILED")
        )
    {
        return needs_sign_in();
    }
    let lowered = text.to_ascii_lowercase();
    let throttled = lowered.contains("too many") || lowered.contains("try again");
    // `async-imap` formats a parsed ALERT as `code: Some(Alert)`, not `[ALERT]`.
    let alert_only = code.as_deref() == Some("ALERT") || raw.contains("code: Some(Alert)");
    let other_code = (code.is_some() && !alert_only)
        || (raw.contains("code: Some(") && !raw.contains("code: Some(Alert)"));
    if throttled || other_code || !no_response {
        return CoreError::Network(BUSY_SERVER.into());
    }
    needs_sign_in()
}

/// Pulls the human response text out of an `async-imap` `Error::No` / `Bad` string.
///
/// Those errors look like `code: None, info: Some("[UNAVAILABLE] Temporary")`.
/// A string that is already the response text is returned unchanged.
fn imap_info_text(raw: &str) -> String {
    const OPEN: &str = "info: Some(\"";
    if let Some(start) = raw.find(OPEN) {
        let rest = &raw[start + OPEN.len()..];
        let mut out = String::new();
        let mut chars = rest.chars();
        while let Some(ch) = chars.next() {
            if ch == '\\' {
                if let Some(next) = chars.next() {
                    out.push(match next {
                        'n' => '\n',
                        'r' => '\r',
                        't' => '\t',
                        other => other,
                    });
                }
                continue;
            }
            if ch == '"' {
                return out;
            }
            out.push(ch);
        }
        return out;
    }
    if raw.contains("info: None") {
        return String::new();
    }
    raw.trim().to_string()
}

/// Response code in `[CODE]` at the start of the text, uppercased.
fn response_code(text: &str) -> Option<String> {
    let text = text.trim();
    let text = text
        .strip_prefix("NO ")
        .or_else(|| text.strip_prefix("BAD "))
        .unwrap_or(text)
        .trim_start();
    let rest = text.strip_prefix('[')?;
    let end = rest.find(']')?;
    let code = &rest[..end];
    if code.is_empty() || !code.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        return None;
    }
    Some(code.to_ascii_uppercase())
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
        let read = tokio::time::timeout(COMMAND_TIMEOUT, stream.read(&mut byte))
            .await
            .map_err(|_| CoreError::Network("IMAP read timed out".into()))?
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

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use async_imap::imap_proto::NameAttribute;

    use std::time::{Duration, Instant};

    use super::{
        ImapConfig, ImapProvider, Security, decode_modified_utf7, folder_from_listing, literal_len,
        page_plan, quote_imap, snippet_from, uid_set,
    };
    use crate::CoreError;
    use crate::model::FolderRole;
    use crate::provider::{FolderSyncState, MailProvider};

    fn imap_state(uid_validity: u32, lowest_uid: u32, highest_uid: u32) -> FolderSyncState {
        FolderSyncState::Imap {
            uid_validity,
            lowest_uid,
            highest_uid,
        }
    }

    #[test]
    fn multipart_partial_yields_snippet() {
        let header = b"From: Alice <alice@wbmc.test>\r\n\
            Subject: Report\r\n\
            MIME-Version: 1.0\r\n\
            Content-Type: multipart/alternative; boundary=\"alt\"\r\n\
            \r\n";
        let partial = b"--alt\r\n\
            Content-Type: text/plain; charset=utf-8\r\n\
            \r\n\
            This text survives truncation.\r\n\
            --alt\r\n\
            Content-Type: text/html; charset=utf-8\r\n\
            \r\n\
            <html><body><p>cut off";
        let snippet = snippet_from(Some(header), Some(partial));
        assert!(
            snippet.contains("This text survives truncation"),
            "{snippet}"
        );
    }

    #[test]
    fn quote_imap_rejects_cr_lf_nul() {
        for value in ["a\rb", "a\nb", "a\0b", "\r\n"] {
            assert!(
                matches!(quote_imap(value), Err(CoreError::InvalidInput(_))),
                "{value:?}"
            );
        }
        assert_eq!(quote_imap("alice").unwrap(), "\"alice\"");
        assert_eq!(quote_imap("a\"b\\c").unwrap(), "\"a\\\"b\\\\c\"");
    }

    #[test]
    fn modified_utf7_decodes_mailbox_name() {
        let folder = folder_from_listing("Entw&APw-rfe", None, &[]).expect("folder");
        assert_eq!(folder.remote_id, "Entw&APw-rfe");
        assert_eq!(folder.name, "Entwürfe");
        assert_eq!(decode_modified_utf7("a&-b"), "a&b");
    }

    #[test]
    fn archive_inbox_is_not_inbox() {
        let nested = folder_from_listing("Archive/INBOX", Some("/"), &[]).expect("nested");
        assert_eq!(nested.remote_id, "Archive/INBOX");
        assert_eq!(nested.name, "INBOX");
        assert_eq!(nested.parent_remote_id.as_deref(), Some("Archive"));
        assert_ne!(nested.role, FolderRole::Inbox);

        let inbox = folder_from_listing("inbox", None, &[]).expect("inbox");
        assert_eq!(inbox.role, FolderRole::Inbox);
    }

    #[test]
    fn unselectable_mailboxes_are_skipped() {
        assert!(folder_from_listing("INBOX", None, &[NameAttribute::NoSelect]).is_none());
        assert!(
            folder_from_listing(
                "gone",
                None,
                &[NameAttribute::Extension(Cow::Borrowed("\\NonExistent"))]
            )
            .is_none()
        );
        assert!(folder_from_listing("INBOX", None, &[NameAttribute::NoInferiors]).is_some());
    }

    #[test]
    fn page_plan_uidvalidity_reset() {
        let plan = page_plan(&[1, 2, 3, 4, 5], Some(imap_state(1, 3, 9)), 2, 2).unwrap();
        assert!(plan.reset);
        assert!(!plan.final_incremental);
        assert!(plan.more);
        assert_eq!(plan.uids, vec![4, 5]);
        assert_eq!(plan.lowest_uid, 4);
        assert_eq!(plan.highest_uid, 5);
    }

    #[test]
    fn page_plan_mixes_older_and_newer() {
        let plan = page_plan(
            &[1, 2, 3, 4, 5, 10, 11, 12],
            Some(imap_state(7, 4, 5)),
            7,
            2,
        )
        .unwrap();
        assert!(!plan.reset);
        assert!(!plan.final_incremental);
        assert!(plan.more);
        assert_eq!(plan.uids, vec![2, 3, 10, 11]);
        assert_eq!(plan.lowest_uid, 2);
        assert_eq!(plan.highest_uid, 11);
    }

    #[test]
    fn page_plan_final_incremental_page() {
        let plan = page_plan(&[1, 2, 3, 4, 5, 9], Some(imap_state(7, 1, 5)), 7, 10).unwrap();
        assert!(plan.final_incremental);
        assert!(!plan.more);
        assert!(!plan.reset);
        assert_eq!(plan.uids, vec![9]);

        let finishing_backfill =
            page_plan(&[1, 2, 3, 4, 5], Some(imap_state(7, 3, 5)), 7, 10).unwrap();
        assert!(!finishing_backfill.final_incremental);
        assert!(!finishing_backfill.more);
        assert_eq!(finishing_backfill.uids, vec![1, 2]);
    }

    #[test]
    fn uid_set_collapses_ranges() {
        assert_eq!(uid_set(&[]), "");
        assert_eq!(uid_set(&[3, 1, 2, 2]), "1:3");
        assert_eq!(uid_set(&[1, 2, 4, 7, 8]), "1:2,4,7:8");
    }

    #[test]
    fn literal_len_reads_both_literal_forms() {
        assert_eq!(literal_len(b"* 1 FETCH (BODY[TEXT]<0>{15}\r\n"), Some(15));
        assert_eq!(literal_len(b"* 1 FETCH (BODY[TEXT]<0> {15}\r\n"), Some(15));
        assert_eq!(literal_len(b"* OK done\r\n"), None);
        assert_eq!(literal_len(b"* 1 FETCH (BODY[TEXT]<0>{15}"), None);
    }

    /// A peer that accepts TCP and then stays silent must not hang the handshake.
    #[tokio::test]
    async fn tls_handshake_times_out_when_peer_never_answers() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let _held = socket;
            std::future::pending::<()>().await;
        });
        let provider = ImapProvider::new(
            ImapConfig {
                host: "127.0.0.1".into(),
                port,
                security: Security::Tls,
                username: "alice".into(),
                smtp_host: "127.0.0.1".into(),
                smtp_port: 1,
                smtp_security: Security::None,
            },
            "secret-password".into(),
        )
        .with_connect_timeout(Duration::from_millis(200));
        let started = Instant::now();
        let error = provider.open_stream().await.unwrap_err();
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "handshake took {:?}",
            started.elapsed()
        );
        match error {
            CoreError::Network(message) => {
                assert!(
                    message.contains("timed out during TLS handshake"),
                    "{message}"
                );
                assert!(!message.contains("secret-password"), "{message}");
            }
            other => panic!("expected a network error, got {other}"),
        }
    }

    #[tokio::test]
    async fn raw_partial_fetch_rejects_more_bodies_than_requested() {
        use tokio::io::AsyncWriteExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            for uid in 1..=5u32 {
                let line = format!("* {uid} FETCH (UID {uid} BODY[TEXT]<0> {{2}}\r\nhi)\r\n");
                if socket.write_all(line.as_bytes()).await.is_err() {
                    return;
                }
            }
            let _ = socket.write_all(b"B0000 OK done\r\n").await;
            std::future::pending::<()>().await;
        });
        let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        let mut stream = super::MailStream::Plain(tcp);
        let error = super::read_partial_bodies(&mut stream, b"B0000", 2)
            .await
            .unwrap_err();
        assert!(matches!(error, CoreError::Protocol(_)), "{error}");
    }

    fn login_no(text: &str) -> async_imap::error::Error {
        async_imap::error::Error::No(text.into())
    }

    /// The string `async-imap` actually puts on `Error::No` / `Error::Bad`.
    fn wrapped_login_text(info: Option<&str>) -> String {
        let info = match info {
            Some(info) => format!("Some({info:?})"),
            None => "None".into(),
        };
        format!("code: None, info: {info}")
    }

    fn assert_needs_sign_in(err: async_imap::error::Error) {
        match super::map_login(err, "alice@wbmc.test") {
            CoreError::NeedsSignIn { account_id } => {
                assert_eq!(account_id, "alice@wbmc.test");
            }
            other => panic!("expected NeedsSignIn, got {other}"),
        }
    }

    fn assert_busy(err: async_imap::error::Error) {
        match super::map_login(err, "alice@wbmc.test") {
            CoreError::Network(message) => {
                assert_eq!(
                    message,
                    "The mail server is busy or limiting connections; will retry"
                );
            }
            other => panic!("expected a retryable network error, got {other}"),
        }
    }

    #[test]
    fn map_login_authentication_failed_needs_sign_in() {
        assert_needs_sign_in(login_no("[AUTHENTICATIONFAILED] Invalid credentials"));
        assert_needs_sign_in(login_no("[authenticationfailed] Invalid credentials"));
        assert_needs_sign_in(login_no(&wrapped_login_text(Some(
            "[AUTHENTICATIONFAILED] Invalid credentials",
        ))));
    }

    #[test]
    fn map_login_authorization_failed_needs_sign_in() {
        assert_needs_sign_in(login_no("[AUTHORIZATIONFAILED] Access denied"));
        assert_needs_sign_in(login_no(&wrapped_login_text(Some(
            "[AUTHORIZATIONFAILED] Access denied",
        ))));
    }

    #[test]
    fn map_login_bare_no_needs_sign_in() {
        assert_needs_sign_in(login_no("Login failed"));
        assert_needs_sign_in(login_no(&wrapped_login_text(None)));
        assert_needs_sign_in(login_no(&wrapped_login_text(Some("Authentication failed"))));
    }

    #[test]
    fn map_login_unavailable_is_network() {
        assert_busy(login_no("[UNAVAILABLE] Temporary"));
        assert_busy(login_no("[unavailable] Temporary"));
        assert_busy(login_no(&wrapped_login_text(Some(
            "[UNAVAILABLE] Temporary",
        ))));
    }

    #[test]
    fn map_login_limit_is_network() {
        assert_busy(login_no("[LIMIT]"));
        assert_busy(login_no(&wrapped_login_text(Some("[LIMIT] over quota"))));
        assert_busy(login_no("[INUSE] Mailbox locked"));
        assert_busy(login_no("[EXPIRED] Session expired"));
        assert_busy(login_no("[SERVERBUG] Internal error"));
        assert_busy(async_imap::error::Error::Bad(
            "[SERVERBUG] Internal error".into(),
        ));
        assert_busy(async_imap::error::Error::Bad("unknown command".into()));
    }

    #[test]
    fn map_login_too_many_connections_is_network() {
        assert_busy(login_no("Too many simultaneous connections"));
        assert_busy(login_no(&wrapped_login_text(Some(
            "Too many simultaneous connections",
        ))));
        assert_busy(login_no("Please try again later"));
        assert_busy(login_no("[ALERT] Please try again later"));
    }

    #[test]
    fn map_login_alert_needs_sign_in() {
        assert_needs_sign_in(login_no("[ALERT] Please log in via your web browser"));
        assert_needs_sign_in(login_no("[alert] Web login required"));
        assert_needs_sign_in(login_no(
            "code: Some(Alert), info: Some(\"Please log in via your web browser\")",
        ));
    }

    /// A dropped connection on the first body chunk must not start the next one.
    #[tokio::test]
    async fn fetch_bodies_stops_after_network_error() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let fetches = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = fetches.clone();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let _ = socket.write_all(b"* OK ready\r\n").await;
            loop {
                let mut line = Vec::new();
                let mut byte = [0u8; 1];
                loop {
                    if socket.read(&mut byte).await.unwrap_or(0) == 0 {
                        return;
                    }
                    line.push(byte[0]);
                    if line.ends_with(b"\r\n") {
                        break;
                    }
                }
                let text = String::from_utf8_lossy(&line);
                let tag = text.split(' ').next().unwrap_or("A0001");
                if text.contains("FETCH") {
                    seen.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    // An unfinished line plus EOF is an I/O error, not an empty fetch.
                    let _ = socket.write_all(b"* 1 FETCH (").await;
                    return;
                }
                let reply = if text.contains("EXAMINE") {
                    format!(
                        "* FLAGS (\\Seen)\r\n* 25 EXISTS\r\n* OK [UIDVALIDITY 1] UIDs valid\r\n{tag} OK [READ-ONLY] EXAMINE completed\r\n"
                    )
                } else {
                    format!("{tag} OK done\r\n")
                };
                if socket.write_all(reply.as_bytes()).await.is_err() {
                    return;
                }
            }
        });

        let provider = ImapProvider::new(
            ImapConfig {
                host: "127.0.0.1".into(),
                port,
                security: Security::None,
                username: "alice".into(),
                smtp_host: "127.0.0.1".into(),
                smtp_port: 1,
                smtp_security: Security::None,
            },
            "secret".into(),
        );
        let ids: Vec<String> = (1..=25).map(|uid| uid.to_string()).collect();
        let started = Instant::now();
        let fetched = provider.fetch_raw_many("INBOX", &ids).await;
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "batch took {:?}",
            started.elapsed()
        );
        assert_eq!(fetches.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert_eq!(fetched.len(), 25);
        for (id, result) in fetched {
            match result {
                Err(CoreError::Network(_)) => {}
                other => panic!("uid {id}: expected a network error, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn command_deadline_bounds_a_whole_command() {
        let error = super::command_deadline_within(
            Duration::from_millis(50),
            std::future::pending::<crate::Result<()>>(),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, CoreError::Network(_)), "{error}");
    }
}
