// This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0. If a copy of the MPL was not distributed with this file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Gmail REST API mail provider.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine;
use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
use futures::{StreamExt, TryStreamExt, stream};
use mail_parser::{MessageParser, MimeHeaders};
use serde::Deserialize;
use serde::de::DeserializeOwned;

use super::{FolderDelta, FolderSyncState, MailProvider, RemoteFolder, RemoteMessage};
use crate::auth::TokenManager;
use crate::model::{AccountId, Address, FolderRole, ProviderKind};
use crate::{CoreError, Result};

const METADATA_HEADERS: &[&str] = &["Subject", "From", "To", "Cc", "Content-Type"];
// Includes the base64 expansion of a large raw message.
const MAX_RESPONSE_BYTES: usize = 70 * 1024 * 1024;

type SleepFn = Arc<dyn Fn(Duration) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

pub struct GmailProvider {
    account_id: AccountId,
    tokens: Arc<TokenManager>,
    gmail_base: url::Url,
    http: reqwest::Client,
    sleep: SleepFn,
}

impl GmailProvider {
    pub fn new(
        account_id: AccountId,
        tokens: Arc<TokenManager>,
        gmail_base: String,
    ) -> Result<Self> {
        Self::with_sleeper(
            account_id,
            tokens,
            gmail_base,
            Arc::new(|duration| Box::pin(tokio::time::sleep(duration))),
        )
    }

    fn with_sleeper(
        account_id: AccountId,
        tokens: Arc<TokenManager>,
        gmail_base: String,
        sleep: SleepFn,
    ) -> Result<Self> {
        let gmail_base = url::Url::parse(&gmail_base)
            .map_err(|_| CoreError::InvalidInput("gmail base is not a valid URL".into()))?;
        if !matches!(gmail_base.scheme(), "http" | "https")
            || gmail_base.host_str().is_none()
            || !gmail_base.username().is_empty()
            || gmail_base.password().is_some()
            || gmail_base.query().is_some()
            || gmail_base.fragment().is_some()
        {
            return Err(CoreError::InvalidInput(
                "gmail base must be an HTTP(S) URL without credentials, query, or fragment".into(),
            ));
        }
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(network)?;
        Ok(Self {
            account_id,
            tokens,
            gmail_base,
            http,
            sleep,
        })
    }

    fn url(&self, segments: &[&str]) -> Result<url::Url> {
        let mut url = self.gmail_base.clone();
        {
            let mut path = url.path_segments_mut().map_err(|_| {
                CoreError::InvalidInput("gmail base must be an absolute URL".into())
            })?;
            path.pop_if_empty();
            path.extend(["users", "me"]);
            path.extend(segments);
        }
        Ok(url)
    }

    async fn get(&self, url: url::Url) -> Result<(reqwest::StatusCode, Vec<u8>)> {
        let mut unauthorized = 0u8;
        let mut delayed = 0u8;
        loop {
            let token = self
                .tokens
                .access_token(&self.account_id, ProviderKind::Google)
                .await?;
            let response = self
                .http
                .get(url.clone())
                .bearer_auth(&token)
                .header(reqwest::header::ACCEPT, "application/json")
                .send()
                .await
                .map_err(network)?;
            let status = response.status();
            if status.as_u16() == 401 {
                self.tokens.invalidate(&self.account_id);
                if unauthorized >= 1 {
                    return Err(CoreError::NeedsSignIn {
                        account_id: self.account_id.as_str().to_string(),
                    });
                }
                unauthorized += 1;
                continue;
            }
            if status.is_server_error() {
                return Err(CoreError::Network(format!(
                    "gmail HTTP {}",
                    status.as_u16()
                )));
            }
            let wait = retry_after(response.headers(), delayed);
            let bytes = read_limited(response).await?;
            if (status.as_u16() == 429 || (status.as_u16() == 403 && is_rate_limit(&bytes)))
                && delayed < 3
            {
                (self.sleep)(wait).await;
                delayed += 1;
                continue;
            }
            return Ok((status, bytes));
        }
    }

    /// 404 has specific meanings for metadata (deleted) and history (expired).
    async fn optional_json<T: DeserializeOwned>(&self, url: url::Url) -> Result<Option<T>> {
        let (status, bytes) = self.get(url).await?;
        if status.as_u16() == 404 {
            return Ok(None);
        }
        if !status.is_success() {
            // Never include server-supplied messages or request URLs in errors.
            return Err(CoreError::Protocol(format!(
                "gmail HTTP {}",
                status.as_u16()
            )));
        }
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|_| CoreError::Protocol("gmail invalid JSON".into()))
    }

    async fn json<T: DeserializeOwned>(&self, url: url::Url) -> Result<T> {
        self.optional_json(url)
            .await?
            .ok_or_else(|| CoreError::NotFound("gmail resource".into()))
    }

    async fn metadata(&self, ids: Vec<String>) -> Result<Vec<(String, Option<GmailMessage>)>> {
        let mut rows: Vec<_> = stream::iter(ids.into_iter().enumerate())
            .map(|(index, id)| async move {
                let mut url = self.url(&["messages", &id])?;
                url.query_pairs_mut().append_pair("format", "metadata");
                for name in METADATA_HEADERS {
                    url.query_pairs_mut().append_pair("metadataHeaders", name);
                }
                let message: Option<GmailMessage> = self.optional_json(url).await?;
                if message.as_ref().is_some_and(|message| message.id != id) {
                    return Err(CoreError::Protocol("gmail metadata id mismatch".into()));
                }
                Ok((index, id, message))
            })
            .buffer_unordered(10)
            .try_collect()
            .await?;
        // Preserve Gmail's newest-first list order despite concurrent GETs.
        rows.sort_by_key(|(index, _, _)| *index);
        Ok(rows
            .into_iter()
            .map(|(_, id, message)| (id, message))
            .collect())
    }

    async fn backfill(
        &self,
        folder: &RemoteFolder,
        history_id: Option<String>,
        page_token: Option<String>,
        page_size: u32,
    ) -> Result<FolderDelta> {
        let history_id = match history_id {
            Some(id) => id,
            None => {
                if page_token.is_some() {
                    return Err(CoreError::InvalidInput(
                        "gmail page token requires a history id".into(),
                    ));
                }
                let profile: Profile = self.json(self.url(&["profile"])?).await?;
                profile.history_id
            }
        };
        let mut url = self.url(&["messages"])?;
        url.query_pairs_mut()
            .append_pair("labelIds", &folder.remote_id)
            .append_pair("maxResults", &page_size.min(100).to_string());
        if let Some(token) = page_token {
            url.query_pairs_mut().append_pair("pageToken", &token);
        }
        let page: MessagePage = self.json(url).await?;
        let mut delta = empty_delta(
            Some(history_id),
            page.next_page_token.clone(),
            page.next_page_token.is_none(),
        );
        delta.more = page.next_page_token.is_some();
        self.populate(
            &mut delta,
            folder,
            page.messages
                .into_iter()
                .map(|message| message.id)
                .collect(),
        )
        .await?;
        Ok(delta)
    }

    async fn incremental(&self, folder: &RemoteFolder, history_id: String) -> Result<FolderDelta> {
        let mut page_token: Option<String> = None;
        let mut seen_pages = HashSet::new();
        let mut ids = Vec::new();
        let mut seen_ids = HashSet::new();
        let latest_history;
        loop {
            let mut url = self.url(&["history"])?;
            url.query_pairs_mut()
                .append_pair("startHistoryId", &history_id)
                .append_pair("labelId", &folder.remote_id)
                .append_pair("maxResults", "500");
            if let Some(token) = page_token {
                url.query_pairs_mut().append_pair("pageToken", &token);
            }
            let Some(page): Option<HistoryPage> = self.optional_json(url).await? else {
                let mut delta = empty_delta(None, None, false);
                delta.reset = true;
                delta.more = true;
                return Ok(delta);
            };
            for history in page.history {
                for change in history
                    .messages_added
                    .into_iter()
                    .chain(history.messages_deleted)
                    .chain(history.labels_added)
                    .chain(history.labels_removed)
                {
                    if seen_ids.insert(change.message.id.clone()) {
                        ids.push(change.message.id);
                    }
                }
            }
            match page.next_page_token {
                Some(token) => {
                    if !seen_pages.insert(token.clone()) {
                        return Err(CoreError::Protocol(
                            "gmail history repeated a page token".into(),
                        ));
                    }
                    page_token = Some(token);
                }
                None => {
                    latest_history = page.history_id;
                    break;
                }
            }
        }
        let mut delta = empty_delta(Some(latest_history), None, true);
        self.populate(&mut delta, folder, ids).await?;
        Ok(delta)
    }

    async fn populate(
        &self,
        delta: &mut FolderDelta,
        folder: &RemoteFolder,
        ids: Vec<String>,
    ) -> Result<()> {
        for (id, message) in self.metadata(ids).await? {
            match message {
                Some(message) if message.label_ids.contains(&folder.remote_id) => {
                    delta.upserts.push(map_message(message))
                }
                _ => delta.deleted_remote_ids.push(id),
            }
        }
        Ok(())
    }
}

impl fmt::Debug for GmailProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GmailProvider")
            .field("account_id", &self.account_id)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl MailProvider for GmailProvider {
    async fn list_folders(&self) -> Result<Vec<RemoteFolder>> {
        let page: LabelPage = self.json(self.url(&["labels"])?).await?;
        Ok(map_labels(page.labels))
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
        match state {
            None => self.backfill(folder, None, None, page_size).await,
            Some(FolderSyncState::Gmail {
                history_id,
                page_token,
                backfilled: false,
            }) => {
                self.backfill(folder, history_id, page_token, page_size)
                    .await
            }
            Some(FolderSyncState::Gmail {
                history_id: Some(id),
                page_token: None,
                backfilled: true,
            }) => self.incremental(folder, id).await,
            _ => Err(CoreError::InvalidInput(
                "valid gmail sync state required".into(),
            )),
        }
    }

    async fn fetch_raw(&self, _folder_remote_id: &str, message_remote_id: &str) -> Result<Vec<u8>> {
        let mut url = self.url(&["messages", message_remote_id])?;
        url.query_pairs_mut().append_pair("format", "raw");
        let raw: RawMessage = self
            .optional_json(url)
            .await?
            .ok_or_else(|| CoreError::NotFound(message_remote_id.to_string()))?;
        URL_SAFE_NO_PAD
            .decode(&raw.raw)
            .or_else(|_| URL_SAFE.decode(&raw.raw))
            .map_err(|_| CoreError::Protocol("gmail raw is not valid base64url".into()))
    }
}

#[derive(Deserialize)]
struct LabelPage {
    #[serde(default)]
    labels: Vec<Label>,
}

#[derive(Deserialize)]
struct Label {
    id: String,
    name: String,
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Profile {
    history_id: String,
}

#[derive(Deserialize)]
struct MessageId {
    id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MessagePage {
    #[serde(default)]
    messages: Vec<MessageId>,
    next_page_token: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct HistoryPage {
    history_id: String,
    #[serde(default)]
    history: Vec<History>,
    next_page_token: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct History {
    messages_added: Vec<Change>,
    messages_deleted: Vec<Change>,
    labels_added: Vec<Change>,
    labels_removed: Vec<Change>,
}

#[derive(Deserialize)]
struct Change {
    message: MessageId,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GmailMessage {
    id: String,
    internal_date: String,
    #[serde(default)]
    snippet: String,
    #[serde(default)]
    label_ids: Vec<String>,
    #[serde(default)]
    payload: Payload,
}

#[derive(Default, Deserialize)]
struct Payload {
    #[serde(default)]
    headers: Vec<Header>,
}

#[derive(Deserialize)]
struct Header {
    name: String,
    value: String,
}

#[derive(Deserialize)]
struct RawMessage {
    raw: String,
}

fn map_labels(labels: Vec<Label>) -> Vec<RemoteFolder> {
    let names: HashMap<_, _> = labels
        .iter()
        .filter(|label| label.kind == "user")
        .map(|label| (label.name.clone(), label.id.clone()))
        .collect();
    labels
        .into_iter()
        .filter_map(|label| {
            let role = match label.id.as_str() {
                "INBOX" => FolderRole::Inbox,
                "SENT" => FolderRole::Sent,
                "DRAFT" => FolderRole::Drafts,
                "TRASH" => FolderRole::Trash,
                "SPAM" => FolderRole::Junk,
                _ if label.kind == "user" => FolderRole::Other,
                _ => return None,
            };
            let parent_remote_id = if label.kind == "user" {
                label
                    .name
                    .rsplit_once('/')
                    .and_then(|(parent, _)| names.get(parent))
                    .cloned()
            } else {
                None
            };
            Some(RemoteFolder {
                remote_id: label.id,
                name: label.name,
                role,
                parent_remote_id,
            })
        })
        .collect()
}

fn empty_delta(
    history_id: Option<String>,
    page_token: Option<String>,
    backfilled: bool,
) -> FolderDelta {
    FolderDelta {
        upserts: vec![],
        deleted_remote_ids: vec![],
        flag_updates: vec![],
        server_remote_ids: None,
        new_state: FolderSyncState::Gmail {
            history_id,
            page_token,
            backfilled,
        },
        more: false,
        reset: false,
    }
}

fn addresses(value: Option<&mail_parser::Address<'_>>) -> Vec<Address> {
    value
        .map(|list| {
            list.iter()
                .map(|address| Address {
                    name: address
                        .name()
                        .filter(|name| !name.is_empty())
                        .map(str::to_string),
                    email: address.address().unwrap_or_default().to_string(),
                })
                .collect()
        })
        .unwrap_or_default()
}

fn map_message(message: GmailMessage) -> RemoteMessage {
    // Reuse the RFC 5322 parser for encoded words, quoted names, and address lists.
    let mut headers = String::new();
    for header in message.payload.headers {
        if let Some(name) = METADATA_HEADERS
            .iter()
            .find(|name| name.eq_ignore_ascii_case(&header.name))
        {
            headers.push_str(name);
            headers.push_str(": ");
            headers.push_str(&header.value.replace(['\r', '\n'], " "));
            headers.push_str("\r\n");
        }
    }
    headers.push_str("\r\n");
    let parsed = MessageParser::default().parse_headers(&headers);
    let subject = parsed
        .as_ref()
        .and_then(|message| message.subject())
        .map(str::trim)
        .filter(|subject| !subject.is_empty())
        .unwrap_or("(no subject)")
        .to_string();
    let from = addresses(parsed.as_ref().and_then(|message| message.from()))
        .into_iter()
        .next()
        .unwrap_or(Address {
            name: Some("Unknown sender".into()),
            email: String::new(),
        });
    let has_attachments = parsed
        .as_ref()
        .and_then(|message| message.content_type())
        .is_some_and(|content_type| {
            content_type.ctype().eq_ignore_ascii_case("multipart")
                && content_type
                    .subtype()
                    .is_some_and(|subtype| subtype.eq_ignore_ascii_case("mixed"))
        });
    RemoteMessage {
        remote_id: message.id,
        subject,
        from,
        to: addresses(parsed.as_ref().and_then(|message| message.to())),
        cc: addresses(parsed.as_ref().and_then(|message| message.cc())),
        date_ms: message.internal_date.parse().unwrap_or(0),
        snippet: decode_entities(&message.snippet),
        is_read: !message.label_ids.iter().any(|id| id == "UNREAD"),
        is_flagged: message.label_ids.iter().any(|id| id == "STARRED"),
        has_attachments,
    }
}

/// Decodes once; preserves unknown, incomplete, and invalid numeric entities.
fn decode_entities(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(index) = rest.find('&') {
        out.push_str(&rest[..index]);
        rest = &rest[index..];
        // Bound scans to avoid quadratic work on malformed snippets.
        let end = rest
            .as_bytes()
            .iter()
            .take(16)
            .position(|byte| *byte == b';');
        let decoded = end.and_then(|end| {
            let entity = &rest[1..end];
            match entity {
                "amp" => Some('&'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "nbsp" => Some('\u{a0}'),
                _ => {
                    let number = entity
                        .strip_prefix("#x")
                        .or_else(|| entity.strip_prefix("#X"))
                        .and_then(|hex| u32::from_str_radix(hex, 16).ok())
                        .or_else(|| {
                            entity
                                .strip_prefix('#')
                                .and_then(|decimal| decimal.parse().ok())
                        });
                    number
                        .filter(|number| *number != 0)
                        .and_then(char::from_u32)
                }
            }
        });
        if let (Some(end), Some(ch)) = (end, decoded) {
            out.push(ch);
            rest = &rest[end + 1..];
        } else {
            out.push('&');
            rest = &rest[1..];
        }
    }
    out.push_str(rest);
    out
}

fn is_rate_limit(bytes: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(bytes)
        .ok()
        .and_then(|body| body.get("error")?.get("errors")?.as_array().cloned())
        .is_some_and(|errors| {
            errors.iter().any(|error| {
                matches!(
                    error.get("reason").and_then(|reason| reason.as_str()),
                    Some("rateLimitExceeded" | "userRateLimitExceeded")
                )
            })
        })
}

fn retry_after(headers: &reqwest::header::HeaderMap, attempt: u8) -> Duration {
    let seconds = headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(1 << attempt);
    Duration::from_secs(seconds.min(30))
}

fn network(error: reqwest::Error) -> CoreError {
    CoreError::Network(format!("gmail: {}", error.without_url()))
}

async fn read_limited(mut response: reqwest::Response) -> Result<Vec<u8>> {
    let too_large = || CoreError::Protocol("gmail response exceeds size limit".into());
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err(too_large());
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(network)? {
        if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(too_large());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use serde_json::{Value, json};
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::{
        GmailMessage, GmailProvider, Label, SleepFn, decode_entities, is_rate_limit, map_labels,
        map_message, retry_after,
    };
    use crate::CoreError;
    use crate::auth::{TokenManager, google};
    use crate::model::{AccountId, FolderRole, ProviderKind};
    use crate::provider::{FolderSyncState, MailProvider, RemoteFolder};
    use crate::secrets::{MemorySecrets, SecretStore, refresh_token_key};

    const ROOT: &str = "/gmail/v1/users/me";

    struct Env {
        server: MockServer,
        provider: GmailProvider,
        account: AccountId,
        slept: Arc<Mutex<Vec<Duration>>>,
    }

    fn token(access: &str) -> ResponseTemplate {
        ResponseTemplate::new(200)
            .set_body_json(json!({"access_token": access, "expires_in": 3600}))
    }

    async fn env() -> Env {
        let server = MockServer::start().await;
        let secrets = Arc::new(MemorySecrets::new());
        let account = AccountId::from("acct-gmail".to_string());
        secrets.set(&refresh_token_key(&account), "rt-1").unwrap();
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(token("at-1"))
            .mount(&server)
            .await;
        let client = google::client_with(
            "google-client",
            "google-secret",
            &format!("{}/authorize", server.uri()),
            &format!("{}/token", server.uri()),
        )
        .unwrap();
        let tokens = Arc::new(TokenManager::new(
            secrets,
            HashMap::from([(ProviderKind::Google, client)]),
        ));
        let slept = Arc::new(Mutex::new(Vec::new()));
        let log = slept.clone();
        let sleeper: SleepFn = Arc::new(move |duration| {
            log.lock().unwrap().push(duration);
            Box::pin(async {})
        });
        let provider = GmailProvider::with_sleeper(
            account.clone(),
            tokens,
            format!("{}/gmail/v1", server.uri()),
            sleeper,
        )
        .unwrap();
        Env {
            server,
            provider,
            account,
            slept,
        }
    }

    async fn get(env: &Env, suffix: &str, body: Value) {
        Mock::given(method("GET"))
            .and(path(format!("{ROOT}/{suffix}")))
            .and(header("authorization", "Bearer at-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(&env.server)
            .await;
    }

    fn inbox() -> RemoteFolder {
        RemoteFolder {
            remote_id: "INBOX".into(),
            name: "INBOX".into(),
            role: FolderRole::Inbox,
            parent_remote_id: None,
        }
    }

    fn state(id: &str) -> FolderSyncState {
        FolderSyncState::Gmail {
            history_id: Some(id.into()),
            page_token: None,
            backfilled: true,
        }
    }

    fn message(id: &str, labels: &[&str]) -> Value {
        json!({"id": id, "internalDate": "1700000000123", "labelIds": labels,
        "snippet": "It&#39;s &amp; &#x1F600;",
        "payload": {"headers": [
            {"name": "Subject", "value": "=?UTF-8?B?Q2Fmw6k=?="},
            {"name": "From", "value": "\"Sender, A\" <a@example.com>"},
            {"name": "To", "value": "B <b@example.com>, C <c@example.com>"},
            {"name": "Cc", "value": "D <d@example.com>"},
            {"name": "Content-Type", "value": "multipart/mixed; boundary=x"}
        ]}})
    }

    async fn metadata(env: &Env, id: &str, labels: &[&str]) {
        Mock::given(method("GET"))
            .and(path(format!("{ROOT}/messages/{id}")))
            .and(query_param("format", "metadata"))
            .and(query_param("metadataHeaders", "Subject"))
            .and(query_param("metadataHeaders", "From"))
            .and(query_param("metadataHeaders", "To"))
            .and(query_param("metadataHeaders", "Cc"))
            .and(query_param("metadataHeaders", "Content-Type"))
            .respond_with(ResponseTemplate::new(200).set_body_json(message(id, labels)))
            .expect(1)
            .mount(&env.server)
            .await;
    }

    #[tokio::test]
    async fn labels_map_roles_and_skip_system() {
        let env = env().await;
        let ids = [
            "INBOX",
            "SENT",
            "DRAFT",
            "TRASH",
            "SPAM",
            "UNREAD",
            "STARRED",
            "IMPORTANT",
            "CHAT",
            "CATEGORY_PERSONAL",
            "CATEGORY_SOCIAL",
            "ALL",
        ];
        let mut labels: Vec<Value> = ids
            .iter()
            .map(|id| json!({"id": id, "name": id, "type": "system"}))
            .collect();
        labels.push(json!({"id": "Label_1", "name": "Work", "type": "user"}));
        get(&env, "labels", json!({"labels": labels})).await;
        let folders = env.provider.list_folders().await.unwrap();
        assert_eq!(folders.len(), 6);
        for (id, role) in [
            ("INBOX", FolderRole::Inbox),
            ("SENT", FolderRole::Sent),
            ("DRAFT", FolderRole::Drafts),
            ("TRASH", FolderRole::Trash),
            ("SPAM", FolderRole::Junk),
            ("Label_1", FolderRole::Other),
        ] {
            assert_eq!(
                folders.iter().find(|f| f.remote_id == id).unwrap().role,
                role
            );
        }
    }

    #[tokio::test]
    async fn nested_user_label() {
        let env = env().await;
        get(
            &env,
            "labels",
            json!({"labels": [
            {"id":"child", "name":"A/B", "type":"user"},
            {"id":"orphan", "name":"Missing/C", "type":"user"},
            {"id":"parent", "name":"A", "type":"user"}]}),
        )
        .await;
        let folders = env.provider.list_folders().await.unwrap();
        assert_eq!(folders[0].parent_remote_id.as_deref(), Some("parent"));
        assert_eq!(folders[1].parent_remote_id, None);
    }

    #[tokio::test]
    async fn first_page_newest_then_backfill_then_history() {
        let env = env().await;
        Mock::given(method("GET"))
            .and(path(format!("{ROOT}/profile")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"historyId":"100"})))
            .expect(1)
            .mount(&env.server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("{ROOT}/messages")))
            .and(query_param("labelIds", "INBOX"))
            .and(query_param("maxResults", "100"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"messages":[{"id":"new"}], "nextPageToken":"older"})),
            )
            .with_priority(2)
            .expect(1)
            .mount(&env.server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("{ROOT}/messages")))
            .and(query_param("pageToken", "older"))
            .and(query_param("maxResults", "100"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"messages":[{"id":"old"}]})),
            )
            .with_priority(1)
            .expect(1)
            .mount(&env.server)
            .await;
        metadata(&env, "new", &["INBOX"]).await;
        metadata(&env, "old", &["INBOX"]).await;
        metadata(&env, "added", &["INBOX"]).await;
        Mock::given(method("GET")).and(path(format!("{ROOT}/history")))
            .and(query_param("startHistoryId", "100")).and(query_param("labelId", "INBOX"))
            .and(query_param("maxResults", "500"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"historyId":"110", "history":[{"messagesAdded":[{"message":{"id":"added"}}]}]})))
            .expect(1).mount(&env.server).await;
        let first = env.provider.sync_folder(&inbox(), None, 200).await.unwrap();
        assert!(first.more);
        assert_eq!(first.upserts[0].remote_id, "new");
        assert_eq!(
            first.new_state,
            FolderSyncState::Gmail {
                history_id: Some("100".into()),
                page_token: Some("older".into()),
                backfilled: false
            }
        );
        let second = env
            .provider
            .sync_folder(&inbox(), Some(first.new_state), 200)
            .await
            .unwrap();
        assert!(!second.more);
        assert_eq!(second.upserts[0].remote_id, "old");
        assert_eq!(second.new_state, state("100"));
        let third = env
            .provider
            .sync_folder(&inbox(), Some(second.new_state), 200)
            .await
            .unwrap();
        assert_eq!(third.upserts[0].remote_id, "added");
        assert_eq!(third.new_state, state("110"));
        assert!(!third.reset);
        assert_eq!(third.server_remote_ids, None);
    }

    #[tokio::test]
    async fn history_label_removed_is_deletion() {
        let env = env().await;
        get(&env, "history", json!({"historyId":"101", "history":[{"labelsRemoved":[{"message":{"id":"gone"}, "labelIds":["INBOX"]}]}]})).await;
        metadata(&env, "gone", &["SENT"]).await;
        let delta = env
            .provider
            .sync_folder(&inbox(), Some(state("100")), 50)
            .await
            .unwrap();
        assert_eq!(delta.deleted_remote_ids, vec!["gone"]);
        assert!(delta.upserts.is_empty());
    }

    #[tokio::test]
    async fn history_404_resets() {
        let env = env().await;
        Mock::given(method("GET"))
            .and(path(format!("{ROOT}/history")))
            .respond_with(ResponseTemplate::new(404))
            .mount(&env.server)
            .await;
        let delta = env
            .provider
            .sync_folder(&inbox(), Some(state("stale")), 50)
            .await
            .unwrap();
        assert!(delta.reset);
        assert!(delta.upserts.is_empty());
        assert_eq!(
            delta.new_state,
            FolderSyncState::Gmail {
                history_id: None,
                page_token: None,
                backfilled: false
            }
        );
    }

    #[tokio::test]
    async fn flags_from_labels() {
        let env = env().await;
        get(&env, "profile", json!({"historyId":"100"})).await;
        get(
            &env,
            "messages",
            json!({"messages":[{"id":"unread"},{"id":"read"}]}),
        )
        .await;
        metadata(&env, "unread", &["INBOX", "UNREAD", "STARRED"]).await;
        metadata(&env, "read", &["INBOX"]).await;
        let delta = env.provider.sync_folder(&inbox(), None, 50).await.unwrap();
        let unread = delta
            .upserts
            .iter()
            .find(|m| m.remote_id == "unread")
            .unwrap();
        assert!(!unread.is_read);
        assert!(unread.is_flagged);
        assert!(unread.has_attachments);
        assert_eq!(unread.date_ms, 1700000000123);
        assert_eq!(unread.snippet, "It's & 😀");
        assert_eq!(unread.subject, "Café");
        assert_eq!(unread.from.email, "a@example.com");
        assert_eq!(unread.from.name.as_deref(), Some("Sender, A"));
        assert_eq!(unread.to.len(), 2);
        assert_eq!(unread.cc[0].email, "d@example.com");
        let read = delta
            .upserts
            .iter()
            .find(|m| m.remote_id == "read")
            .unwrap();
        assert!(read.is_read);
        assert!(!read.is_flagged);
    }

    #[tokio::test]
    async fn fetch_raw_decodes_base64url() {
        let env = env().await;
        let raw = b"From: a@example.com\r\nSubject: Hi\r\n\r\nBody\r\n\xfb\xff";
        Mock::given(method("GET"))
            .and(path(format!("{ROOT}/messages/a%2Fb")))
            .and(query_param("format", "raw"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"raw": URL_SAFE_NO_PAD.encode(raw)})),
            )
            .mount(&env.server)
            .await;
        assert_eq!(env.provider.fetch_raw("ignored", "a/b").await.unwrap(), raw);
    }

    #[tokio::test]
    async fn retries_once_on_401_then_needs_sign_in() {
        let env = env().await;
        for (priority, access) in [(1, "at-1"), (2, "at-2"), (3, "at-3")] {
            Mock::given(method("POST"))
                .and(path("/token"))
                .respond_with(token(access))
                .with_priority(priority)
                .up_to_n_times(1)
                .mount(&env.server)
                .await;
        }
        Mock::given(method("GET"))
            .and(path(format!("{ROOT}/labels")))
            .and(header("authorization", "Bearer at-1"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&env.server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("{ROOT}/labels")))
            .and(header("authorization", "Bearer at-2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"labels":[]})))
            .up_to_n_times(1)
            .mount(&env.server)
            .await;
        assert!(env.provider.list_folders().await.unwrap().is_empty());
        Mock::given(method("GET"))
            .and(path(format!("{ROOT}/labels")))
            .respond_with(ResponseTemplate::new(401).set_body_string("secret at-2 rt-1"))
            .with_priority(1)
            .mount(&env.server)
            .await;
        let error = env.provider.list_folders().await.unwrap_err();
        assert!(
            matches!(&error, CoreError::NeedsSignIn { account_id } if account_id == env.account.as_str())
        );
        assert!(!error.to_string().contains("at-2"));
        assert!(!error.to_string().contains("rt-1"));
        let requests = env.server.received_requests().await.unwrap();
        assert_eq!(
            requests.iter().filter(|r| r.url.path() == "/token").count(),
            3
        );
    }

    #[tokio::test]
    async fn backs_off_on_429() {
        let env = env().await;
        get(&env, "labels", json!({"labels":[]})).await;
        Mock::given(method("GET"))
            .and(path(format!("{ROOT}/labels")))
            .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "2"))
            .with_priority(1)
            .up_to_n_times(1)
            .mount(&env.server)
            .await;
        env.provider.list_folders().await.unwrap();
        assert_eq!(*env.slept.lock().unwrap(), vec![Duration::from_secs(2)]);
        env.slept.lock().unwrap().clear();
        Mock::given(method("GET"))
            .and(path(format!("{ROOT}/labels")))
            .respond_with(ResponseTemplate::new(429).set_body_string("secret rt-1"))
            .with_priority(1)
            .expect(4)
            .mount(&env.server)
            .await;
        let error = env.provider.list_folders().await.unwrap_err();
        assert!(!error.to_string().contains("rt-1"));
        assert_eq!(
            *env.slept.lock().unwrap(),
            vec![
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(4)
            ]
        );
    }

    #[test]
    fn snippet_entities_decoded() {
        assert_eq!(
            decode_entities("&#39; &amp; &quot; &lt; &gt; &#65; &#x1f600; &#X1F600;"),
            "' & \" < > A 😀 😀"
        );
        assert_eq!(
            decode_entities("&amp;lt; &unknown; &#xD800; &#1114112; &#0; &#nope; &broken"),
            "&lt; &unknown; &#xD800; &#1114112; &#0; &#nope; &broken"
        );
        assert_eq!(decode_entities("café &amp; 茶 &&#39;"), "café & 茶 &'");
    }

    #[test]
    fn metadata_maps_headers_flags_and_attachment_heuristic() {
        let value: GmailMessage =
            serde_json::from_value(message("m", &["INBOX", "UNREAD", "STARRED"])).unwrap();
        let mapped = map_message(value);
        assert_eq!(mapped.subject, "Café");
        assert_eq!(mapped.from.email, "a@example.com");
        assert_eq!(mapped.from.name.as_deref(), Some("Sender, A"));
        assert_eq!(mapped.to.len(), 2);
        assert_eq!(mapped.cc[0].email, "d@example.com");
        assert_eq!(mapped.date_ms, 1700000000123);
        assert_eq!(mapped.snippet, "It's & 😀");
        assert!(!mapped.is_read);
        assert!(mapped.is_flagged);
        assert!(mapped.has_attachments);
        let mut value = message("m", &["INBOX"]);
        value["payload"]["headers"] = json!([
            {"name":"subject", "value":"  hello  "},
            {"name":"content-type", "value":"MULTIPART/ALTERNATIVE; boundary=x"}
        ]);
        let mapped = map_message(serde_json::from_value(value).unwrap());
        assert_eq!(mapped.subject, "hello");
        assert!(mapped.is_read);
        assert!(!mapped.is_flagged);
        assert!(!mapped.has_attachments);
    }

    #[test]
    fn labels_and_state_mapping_without_network() {
        let labels: Vec<Label> = serde_json::from_value(json!([
            {"id":"INBOX","name":"Inbox","type":"system"},
            {"id":"STARRED","name":"Starred","type":"system"},
            {"id":"CATEGORY_SOCIAL","name":"Social","type":"system"},
            {"id":"child","name":"A/B","type":"user"},
            {"id":"parent","name":"A","type":"user"}
        ]))
        .unwrap();
        let folders = map_labels(labels);
        assert_eq!(folders.len(), 3);
        assert_eq!(folders[0].role, FolderRole::Inbox);
        assert_eq!(folders[1].parent_remote_id.as_deref(), Some("parent"));
        for state in [
            state("100"),
            FolderSyncState::Gmail {
                history_id: Some("99".into()),
                page_token: Some("next".into()),
                backfilled: false,
            },
        ] {
            let json = serde_json::to_value(&state).unwrap();
            assert_eq!(json["kind"], "Gmail");
            assert_eq!(
                serde_json::from_value::<FolderSyncState>(json).unwrap(),
                state
            );
        }
    }

    #[test]
    fn rate_limit_reasons_and_retry_delays() {
        for reason in ["rateLimitExceeded", "userRateLimitExceeded"] {
            let body =
                serde_json::to_vec(&json!({"error":{"errors":[{"reason":reason}]}})).unwrap();
            assert!(is_rate_limit(&body));
        }
        assert!(!is_rate_limit(
            br#"{"error":{"errors":[{"reason":"forbidden"}]}}"#
        ));
        assert!(!is_rate_limit(b"not json"));
        let mut headers = reqwest::header::HeaderMap::new();
        for (attempt, seconds) in [(0, 1), (1, 2), (2, 4)] {
            assert_eq!(retry_after(&headers, attempt), Duration::from_secs(seconds));
        }
        headers.insert(reqwest::header::RETRY_AFTER, "45".parse().unwrap());
        assert_eq!(retry_after(&headers, 0), Duration::from_secs(30));
    }

    #[tokio::test]
    async fn history_follows_pages_deduplicates_changes_and_handles_missing_messages() {
        let env = env().await;
        Mock::given(method("GET"))
            .and(path(format!("{ROOT}/history")))
            .and(query_param("startHistoryId", "100"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "historyId":"110", "nextPageToken":"next", "history":[{
                    "messagesAdded":[{"message":{"id":"added"}}],
                    "messagesDeleted":[{"message":{"id":"missing"}}],
                    "labelsAdded":[{"message":{"id":"changed"}}],
                    "labelsRemoved":[{"message":{"id":"changed"}}]
                }]
            })))
            .with_priority(2)
            .expect(1)
            .mount(&env.server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("{ROOT}/history")))
            .and(query_param("pageToken", "next"))
            .and(query_param("startHistoryId", "100"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "historyId":"111", "history":[{"messagesAdded":[{"message":{"id":"added"}}]}]
            })))
            .with_priority(1)
            .expect(1)
            .mount(&env.server)
            .await;
        metadata(&env, "added", &["INBOX"]).await;
        metadata(&env, "changed", &["INBOX", "STARRED"]).await;
        Mock::given(method("GET"))
            .and(path(format!("{ROOT}/messages/missing")))
            .respond_with(ResponseTemplate::new(404))
            .expect(1)
            .mount(&env.server)
            .await;
        let delta = env
            .provider
            .sync_folder(&inbox(), Some(state("100")), 50)
            .await
            .unwrap();
        assert_eq!(delta.upserts.len(), 2);
        assert!(
            delta
                .upserts
                .iter()
                .find(|m| m.remote_id == "changed")
                .unwrap()
                .is_flagged
        );
        assert_eq!(delta.deleted_remote_ids, vec!["missing"]);
        assert_eq!(delta.new_state, state("111"));
        assert!(!delta.more);
        assert!(delta.flag_updates.is_empty());
    }

    #[tokio::test]
    async fn rate_limit_403_retries_but_other_403_and_5xx_do_not() {
        let env = env().await;
        get(&env, "labels", json!({"labels":[]})).await;
        for reason in ["rateLimitExceeded", "userRateLimitExceeded"] {
            Mock::given(method("GET"))
                .and(path(format!("{ROOT}/labels")))
                .respond_with(
                    ResponseTemplate::new(403)
                        .insert_header("Retry-After", "3")
                        .set_body_json(json!({"error":{"errors":[{"reason":reason}]}})),
                )
                .with_priority(1)
                .up_to_n_times(1)
                .mount(&env.server)
                .await;
            env.provider.list_folders().await.unwrap();
        }
        assert_eq!(*env.slept.lock().unwrap(), vec![Duration::from_secs(3); 2]);
        for status in [403, 500, 503] {
            Mock::given(method("GET"))
                .and(path(format!("{ROOT}/labels")))
                .respond_with(ResponseTemplate::new(status).set_body_json(
                    json!({"error":{"message":"secret rt-1", "errors":[{"reason":"forbidden"}]}}),
                ))
                .with_priority(1)
                .up_to_n_times(1)
                .expect(1)
                .mount(&env.server)
                .await;
            let error = env.provider.list_folders().await.unwrap_err();
            assert!(!error.to_string().contains("rt-1"));
            if status == 403 {
                assert!(matches!(error, CoreError::Protocol(_)));
            } else {
                assert!(matches!(error, CoreError::Network(_)));
            }
        }
        assert_eq!(env.slept.lock().unwrap().len(), 2);
    }
}
