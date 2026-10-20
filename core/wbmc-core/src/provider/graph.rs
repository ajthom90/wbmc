// This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0. If a copy of the MPL was not distributed with this file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Microsoft Graph mail provider.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;

use super::{FolderDelta, FolderSyncState, MailProvider, RemoteFolder, RemoteMessage};
use crate::auth::TokenManager;
use crate::model::{AccountId, Address, FolderRole, ProviderKind};
use crate::{CoreError, Result};

const MAX_RAW_BYTES: usize = 50 * 1024 * 1024;
const SELECT_FIELDS: &str = "subject,from,toRecipients,ccRecipients,receivedDateTime,isRead,flag,hasAttachments,bodyPreview";
const WELL_KNOWN: &[(&str, FolderRole)] = &[
    ("inbox", FolderRole::Inbox),
    ("sentitems", FolderRole::Sent),
    ("drafts", FolderRole::Drafts),
    ("deleteditems", FolderRole::Trash),
    ("archive", FolderRole::Archive),
    ("junkemail", FolderRole::Junk),
];

type SleepFn = Arc<dyn Fn(Duration) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

pub struct GraphProvider {
    account_id: AccountId,
    tokens: Arc<TokenManager>,
    graph_base: String,
    http: reqwest::Client,
    roles: Mutex<Option<HashMap<String, FolderRole>>>,
    sleep: SleepFn,
    max_raw_bytes: usize,
}

impl GraphProvider {
    pub fn new(account_id: AccountId, tokens: Arc<TokenManager>, graph_base: String) -> Self {
        Self::with_sleeper(
            account_id,
            tokens,
            graph_base,
            Arc::new(|duration| Box::pin(tokio::time::sleep(duration))),
        )
    }

    fn with_sleeper(
        account_id: AccountId,
        tokens: Arc<TokenManager>,
        graph_base: String,
        sleep: SleepFn,
    ) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_else(|error| panic!("graph HTTP client: {}", error.without_url()));
        Self {
            account_id,
            tokens,
            graph_base,
            http,
            roles: Mutex::new(None),
            sleep,
            max_raw_bytes: MAX_RAW_BYTES,
        }
    }

    fn roles_cache(&self) -> std::sync::MutexGuard<'_, Option<HashMap<String, FolderRole>>> {
        self.roles
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    async fn roles(&self) -> Result<HashMap<String, FolderRole>> {
        if let Some(roles) = self.roles_cache().clone() {
            return Ok(roles);
        }
        let loaded = self.load_roles().await?;
        *self.roles_cache() = Some(loaded.clone());
        Ok(loaded)
    }

    async fn load_roles(&self) -> Result<HashMap<String, FolderRole>> {
        let mut roles = HashMap::new();
        for &(name, role) in WELL_KNOWN {
            let url = self.url(&["me", "mailFolders", name])?;
            let Some(folder) = self.optional_folder(url.as_str()).await? else {
                continue;
            };
            if let Some(id) = nonempty(folder.id) {
                roles.insert(id, role);
            }
        }
        Ok(roles)
    }

    fn url(&self, segments: &[&str]) -> Result<url::Url> {
        let base = self.graph_base.trim_end_matches('/');
        let mut url = url::Url::parse(base)
            .map_err(|_| CoreError::InvalidInput("graph base is not a valid URL".into()))?;
        {
            let mut path = url.path_segments_mut().map_err(|_| {
                CoreError::InvalidInput("graph base must be an absolute URL".into())
            })?;
            path.pop_if_empty();
            for segment in segments {
                path.push(segment);
            }
        }
        Ok(url)
    }

    fn collection_url(&self, segments: &[&str]) -> Result<String> {
        let mut url = self.url(segments)?;
        url.query_pairs_mut().append_pair("$top", "100");
        Ok(url.to_string())
    }

    fn initial_delta_url(&self, folder_id: &str) -> Result<String> {
        let mut url = self.url(&["me", "mailFolders", folder_id, "messages", "delta"])?;
        url.query_pairs_mut().append_pair("$select", SELECT_FIELDS);
        Ok(url.to_string())
    }

    /// Rejects links that would send the bearer token to another host.
    fn resolve_url(&self, raw: &str) -> Result<url::Url> {
        let base = url::Url::parse(self.graph_base.trim_end_matches('/'))
            .map_err(|_| CoreError::InvalidInput("graph base is not a valid URL".into()))?;
        let absolute = if raw.starts_with("http://") || raw.starts_with("https://") {
            url::Url::parse(raw).map_err(|_| CoreError::Protocol("graph link is invalid".into()))?
        } else {
            let directory = format!("{}/", self.graph_base.trim_end_matches('/'));
            let directory = url::Url::parse(&directory)
                .map_err(|_| CoreError::InvalidInput("graph base is not a valid URL".into()))?;
            directory
                .join(raw)
                .map_err(|_| CoreError::Protocol("graph link is invalid".into()))?
        };
        let same_origin = absolute.scheme() == base.scheme()
            && absolute.host_str() == base.host_str()
            && absolute.port_or_known_default() == base.port_or_known_default();
        if !same_origin {
            return Err(CoreError::Protocol(
                "graph link is not on the graph host".into(),
            ));
        }
        Ok(absolute)
    }

    async fn send(
        &self,
        url: &str,
        prefer: Option<&str>,
        accept_json: bool,
    ) -> Result<reqwest::Response> {
        let url = self.resolve_url(url)?;
        let mut unauthorized = 0u8;
        let mut delayed = 0u8;
        loop {
            let token = self
                .tokens
                .access_token(&self.account_id, ProviderKind::Microsoft)
                .await?;
            let mut request = self.http.get(url.clone()).bearer_auth(&token);
            if accept_json {
                request = request.header(reqwest::header::ACCEPT, "application/json");
            }
            if let Some(prefer) = prefer {
                request = request.header("Prefer", prefer);
            }
            let response = request.send().await.map_err(network)?;
            let status = response.status().as_u16();
            if status == 401 {
                self.tokens.invalidate(&self.account_id);
                if unauthorized >= 1 {
                    return Err(CoreError::NeedsSignIn {
                        account_id: self.account_id.as_str().to_string(),
                    });
                }
                unauthorized += 1;
                continue;
            }
            if status == 429 || status == 503 {
                if delayed >= 3 {
                    let bytes = read_limited(response, self.max_raw_bytes).await?;
                    return Err(protocol_error(status, error_code(&bytes).as_deref()));
                }
                let wait = retry_after(response.headers());
                drop(response);
                (self.sleep)(wait).await;
                delayed += 1;
                continue;
            }
            return Ok(response);
        }
    }

    async fn optional_folder(&self, url: &str) -> Result<Option<GraphFolder>> {
        let response = self.send(url, None, true).await?;
        let status = response.status();
        if status.as_u16() == 404 {
            return Ok(None);
        }
        let bytes = read_limited(response, self.max_raw_bytes).await?;
        if !status.is_success() {
            return Err(protocol_error(
                status.as_u16(),
                error_code(&bytes).as_deref(),
            ));
        }
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|_| CoreError::Protocol(format!("graph {} invalidJSON", status.as_u16())))
    }

    async fn get_page(&self, url: &str) -> Result<GraphPage> {
        let response = self.send(url, None, true).await?;
        let status = response.status();
        let bytes = read_limited(response, self.max_raw_bytes).await?;
        if !status.is_success() {
            return Err(protocol_error(
                status.as_u16(),
                error_code(&bytes).as_deref(),
            ));
        }
        serde_json::from_slice(&bytes)
            .map_err(|_| CoreError::Protocol(format!("graph {} invalidJSON", status.as_u16())))
    }

    fn collect_folders<'a>(
        &'a self,
        page_url: String,
        parent: Option<String>,
        roles: &'a HashMap<String, FolderRole>,
        out: &'a mut Vec<RemoteFolder>,
        seen_ids: &'a mut HashSet<String>,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            let mut next = Some(page_url);
            let mut seen_urls = HashSet::new();
            while let Some(page_url) = next.take() {
                if !seen_urls.insert(page_url.clone()) {
                    return Err(CoreError::Protocol(
                        "graph folder list repeated a page link".into(),
                    ));
                }
                let page = self.get_page(&page_url).await?;
                next = nonempty(page.next_link);
                for value in page.value {
                    let Ok(folder) = serde_json::from_value::<GraphFolder>(value) else {
                        continue;
                    };
                    let Some(id) = nonempty(folder.id) else {
                        continue;
                    };
                    if !seen_ids.insert(id.clone()) {
                        continue;
                    }
                    let has_children = folder.child_folder_count.unwrap_or(0) > 0;
                    out.push(RemoteFolder {
                        remote_id: id.clone(),
                        name: folder.display_name.unwrap_or_default(),
                        role: roles.get(&id).copied().unwrap_or(FolderRole::Other),
                        parent_remote_id: parent.clone(),
                    });
                    if has_children {
                        let child_url = self.collection_url(&[
                            "me",
                            "mailFolders",
                            id.as_str(),
                            "childFolders",
                        ])?;
                        self.collect_folders(child_url, Some(id), roles, out, seen_ids)
                            .await?;
                    }
                }
            }
            Ok(())
        })
    }
}

impl fmt::Debug for GraphProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GraphProvider")
            .field("account_id", &self.account_id)
            .field("graph_base", &self.graph_base)
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl MailProvider for GraphProvider {
    async fn list_folders(&self) -> Result<Vec<RemoteFolder>> {
        let roles = self.roles().await?;
        let mut folders = Vec::new();
        let mut seen_ids = HashSet::new();
        let url = self.collection_url(&["me", "mailFolders"])?;
        self.collect_folders(url, None, &roles, &mut folders, &mut seen_ids)
            .await?;
        Ok(folders)
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
        let (previous_delta, url, stored_link) = match state {
            Some(FolderSyncState::Graph {
                next_link: Some(next),
                delta_link,
            }) => (delta_link, next, true),
            Some(FolderSyncState::Graph {
                delta_link: Some(delta),
                ..
            }) => (Some(delta.clone()), delta, true),
            Some(FolderSyncState::Graph { .. }) | None => {
                (None, self.initial_delta_url(&folder.remote_id)?, false)
            }
            Some(FolderSyncState::Imap { .. } | FolderSyncState::Gmail { .. }) => {
                return Err(CoreError::InvalidInput("graph sync state required".into()));
            }
        };
        let prefer = format!("odata.maxpagesize={page_size}");
        let response = self.send(&url, Some(&prefer), true).await?;
        let status = response.status();
        let bytes = read_limited(response, self.max_raw_bytes).await?;
        let code = error_code(&bytes);
        if status.as_u16() == 410 || code.as_deref().is_some_and(is_resync) {
            if !stored_link {
                return Err(protocol_error(status.as_u16(), code.as_deref()));
            }
            return Ok(reset_delta());
        }
        if !status.is_success() {
            return Err(protocol_error(status.as_u16(), code.as_deref()));
        }
        let page: GraphPage = serde_json::from_slice(&bytes)
            .map_err(|_| CoreError::Protocol(format!("graph {} invalidJSON", status.as_u16())))?;
        Ok(map_delta(page, previous_delta))
    }

    async fn fetch_raw(&self, _folder_remote_id: &str, message_remote_id: &str) -> Result<Vec<u8>> {
        let url = self.url(&["me", "messages", message_remote_id, "$value"])?;
        let response = self.send(url.as_str(), None, false).await?;
        let status = response.status();
        if status.as_u16() == 404 {
            return Err(CoreError::NotFound(message_remote_id.to_string()));
        }
        let bytes = read_limited(response, self.max_raw_bytes).await?;
        if !status.is_success() {
            return Err(protocol_error(
                status.as_u16(),
                error_code(&bytes).as_deref(),
            ));
        }
        Ok(bytes)
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct GraphFolder {
    id: Option<String>,
    display_name: Option<String>,
    child_folder_count: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct GraphPage {
    #[serde(default)]
    value: Vec<serde_json::Value>,
    #[serde(default, rename = "@odata.nextLink")]
    next_link: Option<String>,
    #[serde(default, rename = "@odata.deltaLink")]
    delta_link: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct GraphMessage {
    id: Option<String>,
    subject: Option<String>,
    from: Option<GraphRecipient>,
    #[serde(rename = "toRecipients")]
    to_recipients: Option<Vec<GraphRecipient>>,
    #[serde(rename = "ccRecipients")]
    cc_recipients: Option<Vec<GraphRecipient>>,
    #[serde(rename = "receivedDateTime")]
    received_date_time: Option<String>,
    #[serde(rename = "isRead")]
    is_read: Option<bool>,
    flag: Option<GraphFlag>,
    #[serde(rename = "hasAttachments")]
    has_attachments: Option<bool>,
    #[serde(rename = "bodyPreview")]
    body_preview: Option<String>,
    #[serde(rename = "@removed")]
    removed: Option<serde_json::Value>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct GraphRecipient {
    #[serde(rename = "emailAddress")]
    email_address: Option<GraphEmail>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct GraphEmail {
    name: Option<String>,
    address: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct GraphFlag {
    #[serde(rename = "flagStatus")]
    flag_status: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GraphErrorBody {
    error: Option<GraphError>,
}

#[derive(Debug, Deserialize)]
struct GraphError {
    code: Option<String>,
}

fn reset_delta() -> FolderDelta {
    FolderDelta {
        upserts: Vec::new(),
        deleted_remote_ids: Vec::new(),
        flag_updates: Vec::new(),
        server_remote_ids: None,
        new_state: FolderSyncState::Graph {
            delta_link: None,
            next_link: None,
        },
        more: true,
        reset: true,
    }
}

fn map_delta(page: GraphPage, previous_delta: Option<String>) -> FolderDelta {
    let mut upserts = Vec::new();
    let mut deleted_remote_ids = Vec::new();
    for value in page.value {
        let Ok(message) = serde_json::from_value::<GraphMessage>(value) else {
            continue;
        };
        let Some(id) = nonempty(message.id) else {
            continue;
        };
        if message.removed.is_some() {
            deleted_remote_ids.push(id);
            continue;
        }
        upserts.push(RemoteMessage {
            remote_id: id,
            subject: message.subject.unwrap_or_default(),
            from: map_address(message.from),
            to: map_list(message.to_recipients),
            cc: map_list(message.cc_recipients),
            date_ms: message
                .received_date_time
                .as_deref()
                .and_then(parse_rfc3339_ms)
                .unwrap_or(0),
            snippet: snippet(message.body_preview),
            is_read: message.is_read.unwrap_or(false),
            is_flagged: message
                .flag
                .and_then(|flag| flag.flag_status)
                .is_some_and(|status| status == "flagged"),
            has_attachments: message.has_attachments.unwrap_or(false),
        });
    }
    let next_link = nonempty(page.next_link);
    FolderDelta {
        upserts,
        deleted_remote_ids,
        flag_updates: Vec::new(),
        server_remote_ids: None,
        new_state: FolderSyncState::Graph {
            delta_link: nonempty(page.delta_link).or(previous_delta),
            next_link: next_link.clone(),
        },
        more: next_link.is_some(),
        reset: false,
    }
}

fn map_address(recipient: Option<GraphRecipient>) -> Address {
    let email = recipient.and_then(|recipient| recipient.email_address);
    Address {
        name: email
            .as_ref()
            .and_then(|email| email.name.clone())
            .filter(|name| !name.trim().is_empty()),
        email: email.and_then(|email| email.address).unwrap_or_default(),
    }
}

fn map_list(recipients: Option<Vec<GraphRecipient>>) -> Vec<Address> {
    recipients
        .unwrap_or_default()
        .into_iter()
        .map(|recipient| map_address(Some(recipient)))
        .collect()
}

fn snippet(preview: Option<String>) -> String {
    preview.unwrap_or_default().chars().take(200).collect()
}

fn nonempty(value: Option<String>) -> Option<String> {
    value.filter(|value| !value.is_empty())
}

fn is_resync(code: &str) -> bool {
    code.eq_ignore_ascii_case("syncStateNotFound") || code.eq_ignore_ascii_case("resyncRequired")
}

fn protocol_error(status: u16, code: Option<&str>) -> CoreError {
    match code.and_then(sanitize_code) {
        Some(code) => CoreError::Protocol(format!("graph {status} {code}")),
        None => CoreError::Protocol(format!("graph {status}")),
    }
}

fn sanitize_code(code: &str) -> Option<String> {
    let cleaned: String = code
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .take(64)
        .collect();
    (!cleaned.is_empty()).then_some(cleaned)
}

fn error_code(bytes: &[u8]) -> Option<String> {
    serde_json::from_slice::<GraphErrorBody>(bytes)
        .ok()
        .and_then(|body| body.error)
        .and_then(|error| error.code)
}

fn retry_after(headers: &reqwest::header::HeaderMap) -> Duration {
    let seconds = headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(1);
    Duration::from_secs(seconds.min(30))
}

fn declared_length(response: &reqwest::Response) -> Option<u64> {
    response
        .headers()
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse().ok())
}

fn too_large(limit: usize) -> CoreError {
    CoreError::Protocol(format!("graph response exceeds {limit} bytes"))
}

fn network(error: reqwest::Error) -> CoreError {
    CoreError::Network(format!("graph: {}", error.without_url()))
}

async fn read_limited(mut response: reqwest::Response, limit: usize) -> Result<Vec<u8>> {
    if declared_length(&response).is_some_and(|len| len > limit as u64) {
        return Err(too_large(limit));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(network)? {
        if body.len().saturating_add(chunk.len()) > limit {
            return Err(too_large(limit));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn parse_rfc3339_ms(input: &str) -> Option<i64> {
    let (date, rest) = input.trim().split_once(['T', 't'])?;
    let mut parts = date.split('-');
    let year: i32 = parts.next()?.parse().ok()?;
    let month: u32 = parts.next()?.parse().ok()?;
    let day: u32 = parts.next()?.parse().ok()?;
    if parts.next().is_some()
        || !(1..=12).contains(&month)
        || day == 0
        || day > days_in_month(year, month)
    {
        return None;
    }
    let (time, offset) = split_offset(rest)?;
    let (time, fraction) = time
        .split_once('.')
        .map(|(time, fraction)| (time, Some(fraction)))
        .unwrap_or((time, None));
    let fraction_ms = match fraction {
        Some(fraction) => fraction_to_ms(fraction)?,
        None => 0,
    };
    let mut clock = time.split(':');
    let hour: u32 = clock.next()?.parse().ok()?;
    let minute: u32 = clock.next()?.parse().ok()?;
    let second: u32 = clock.next()?.parse().ok()?;
    if clock.next().is_some() || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    let days = days_from_civil(year, month, day);
    let local = days
        .checked_mul(86_400)?
        .checked_add(i64::from(hour) * 3_600)?
        .checked_add(i64::from(minute) * 60)?
        .checked_add(i64::from(second))?;
    let utc = local.checked_sub(parse_offset(offset)?)?;
    utc.checked_mul(1_000)?.checked_add(fraction_ms)
}

fn split_offset(rest: &str) -> Option<(&str, &str)> {
    if let Some(time) = rest.strip_suffix(['Z', 'z']) {
        return Some((time, "Z"));
    }
    let index = rest.rfind(['+', '-'])?;
    if index == 0 {
        return None;
    }
    Some((&rest[..index], &rest[index..]))
}

fn parse_offset(offset: &str) -> Option<i64> {
    if offset.eq_ignore_ascii_case("Z") {
        return Some(0);
    }
    let (sign, body) = match offset.as_bytes().first()? {
        b'+' => (1, &offset[1..]),
        b'-' => (-1, &offset[1..]),
        _ => return None,
    };
    let (hours, minutes) = if let Some((hours, minutes)) = body.split_once(':') {
        (hours, minutes)
    } else if body.len() == 4 {
        (&body[..2], &body[2..])
    } else {
        return None;
    };
    let hours: i64 = hours.parse().ok()?;
    let minutes: i64 = minutes.parse().ok()?;
    if !(0..=23).contains(&hours) || !(0..=59).contains(&minutes) {
        return None;
    }
    Some(sign * (hours * 3_600 + minutes * 60))
}

fn fraction_to_ms(fraction: &str) -> Option<i64> {
    if fraction.is_empty() || !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let mut millis = String::new();
    for (index, byte) in fraction.bytes().enumerate() {
        if index == 3 {
            break;
        }
        millis.push(char::from(byte));
    }
    while millis.len() < 3 {
        millis.push('0');
    }
    millis.parse().ok()
}

fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(year) => 29,
        2 => 28,
        _ => 0,
    }
}

fn is_leap(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

/// Days since 1970-01-01 for a Gregorian date. Howard Hinnant's `days_from_civil`.
fn days_from_civil(mut year: i32, month: u32, day: u32) -> i64 {
    year -= i32::from(month <= 2);
    let era = year.div_euclid(400);
    let year_of_era = (year - era * 400) as u32;
    let month_prime = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * month_prime + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    i64::from(era) * 146_097 + i64::from(day_of_era) - 719_468
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::time::Duration;

    use wiremock::matchers::{header, method, path, path_regex, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::auth::{Endpoints, microsoft};
    use crate::model::{Address, FolderRole};
    use crate::secrets::{MemorySecrets, SecretStore, refresh_token_key};

    const SELECT: &str = "subject,from,toRecipients,ccRecipients,receivedDateTime,isRead,flag,hasAttachments,bodyPreview";
    const DELTA_PATH: &str = "/v1.0/me/mailFolders/inbox-id/messages/delta";

    struct Env {
        server: MockServer,
        provider: GraphProvider,
        account: AccountId,
        slept: Arc<Mutex<Vec<Duration>>>,
    }

    fn token_response(access: &str) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": access,
            "refresh_token": "rt-1",
            "expires_in": 3600,
        }))
    }

    fn instant_sleep(slept: &Arc<Mutex<Vec<Duration>>>) -> SleepFn {
        let slept = Arc::clone(slept);
        Arc::new(move |duration| {
            slept.lock().expect("sleep log").push(duration);
            Box::pin(async {}) as Pin<Box<dyn Future<Output = ()> + Send>>
        })
    }

    async fn env() -> Env {
        let server = MockServer::start().await;
        let secrets = Arc::new(MemorySecrets::new());
        let account = AccountId::from("acct-graph".to_string());
        secrets.set(&refresh_token_key(&account), "rt-1").unwrap();
        Mock::given(method("POST"))
            .and(path("/common/oauth2/v2.0/token"))
            .respond_with(token_response("at-1"))
            .mount(&server)
            .await;
        let client = microsoft::client_with(
            "test-client",
            Endpoints {
                authority: format!("{}/common", server.uri()),
                graph: format!("{}/v1.0", server.uri()),
            },
        )
        .unwrap();
        let tokens = Arc::new(TokenManager::new(
            secrets,
            HashMap::from([(ProviderKind::Microsoft, client)]),
        ));
        let slept = Arc::new(Mutex::new(Vec::new()));
        let provider = GraphProvider::with_sleeper(
            account.clone(),
            tokens,
            format!("{}/v1.0", server.uri()),
            instant_sleep(&slept),
        );
        Env {
            server,
            provider,
            account,
            slept,
        }
    }

    fn inbox() -> RemoteFolder {
        RemoteFolder {
            remote_id: "inbox-id".into(),
            name: "Inbox".into(),
            role: FolderRole::Inbox,
            parent_remote_id: None,
        }
    }

    fn folder(id: &str, name: &str, children: i64) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "displayName": name,
            "childFolderCount": children,
        })
    }

    fn remote(id: &str, name: &str, role: FolderRole, parent: Option<&str>) -> RemoteFolder {
        RemoteFolder {
            remote_id: id.into(),
            name: name.into(),
            role,
            parent_remote_id: parent.map(str::to_string),
        }
    }

    fn message(id: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "subject": "Hello",
            "from": {"emailAddress": {"name": "Ada Lovelace", "address": "ada@example.com"}},
            "toRecipients": [
                {"emailAddress": {"name": "Andrew", "address": "andrew@example.com"}}
            ],
            "ccRecipients": [
                {"emailAddress": {"name": "", "address": "cc@example.com"}}
            ],
            "receivedDateTime": "2026-10-19T15:04:05.1234567Z",
            "isRead": true,
            "flag": {"flagStatus": "flagged"},
            "hasAttachments": true,
            "bodyPreview": "preview text",
        })
    }

    fn assert_full_message(message: &super::super::RemoteMessage, id: &str) {
        assert_eq!(message.remote_id, id);
        assert_eq!(message.subject, "Hello");
        assert_eq!(
            message.from,
            Address {
                name: Some("Ada Lovelace".into()),
                email: "ada@example.com".into(),
            }
        );
        assert_eq!(
            message.to,
            vec![Address {
                name: Some("Andrew".into()),
                email: "andrew@example.com".into(),
            }]
        );
        assert_eq!(
            message.cc,
            vec![Address {
                name: None,
                email: "cc@example.com".into(),
            }]
        );
        assert_eq!(message.date_ms, 1_792_422_245_123);
        assert_eq!(message.snippet, "preview text");
        assert!(message.is_read);
        assert!(message.is_flagged);
        assert!(message.has_attachments);
    }

    fn assert_no_flag_side_channel(delta: &FolderDelta) {
        assert!(delta.flag_updates.is_empty());
        assert!(delta.server_remote_ids.is_none());
    }

    async fn mount_well_known(server: &MockServer, name: &str, id: &str) {
        Mock::given(method("GET"))
            .and(path(format!("/v1.0/me/mailFolders/{name}")))
            .and(header("authorization", "Bearer at-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": id,
                "displayName": name,
            })))
            .with_priority(1)
            .expect(1)
            .mount(server)
            .await;
    }

    async fn mount_unknown_well_known_404(server: &MockServer) {
        Mock::given(method("GET"))
            .and(path_regex(r"^/v1\.0/me/mailFolders/[^/]+$"))
            .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
                "error": {"code": "ErrorItemNotFound", "message": "missing folder"}
            })))
            .mount(server)
            .await;
    }

    #[test]
    fn raw_body_limit_is_50_mebibytes() {
        assert_eq!(MAX_RAW_BYTES, 50 * 1024 * 1024);
    }

    #[tokio::test]
    async fn folders_with_roles_and_children() {
        let env = env().await;
        for (name, id) in [
            ("inbox", "inbox-id"),
            ("sentitems", "sent-id"),
            ("drafts", "drafts-id"),
            ("deleteditems", "trash-id"),
            ("archive", "archive-id"),
            ("junkemail", "junk-id"),
        ] {
            mount_well_known(&env.server, name, id).await;
        }
        Mock::given(method("GET"))
            .and(path("/v1.0/me/mailFolders"))
            .and(query_param("$top", "100"))
            .and(header("authorization", "Bearer at-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "value": [
                    folder("inbox-id", "Inbox", 1),
                    folder("sent-id", "Sent Items", 0),
                    folder("drafts-id", "Drafts", 0),
                    folder("trash-id", "Deleted", 0),
                    folder("archive-id", "Archive", 0),
                    folder("junk-id", "Junk Email", 0),
                    folder("proj-id", "Projects", 0),
                ]
            })))
            .expect(2)
            .mount(&env.server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/mailFolders/inbox-id/childFolders"))
            .and(query_param("$top", "100"))
            .and(header("authorization", "Bearer at-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "value": [folder("receipts-id", "Receipts", 0)]
            })))
            .expect(2)
            .mount(&env.server)
            .await;

        let expected = vec![
            remote("inbox-id", "Inbox", FolderRole::Inbox, None),
            remote(
                "receipts-id",
                "Receipts",
                FolderRole::Other,
                Some("inbox-id"),
            ),
            remote("sent-id", "Sent Items", FolderRole::Sent, None),
            remote("drafts-id", "Drafts", FolderRole::Drafts, None),
            remote("trash-id", "Deleted", FolderRole::Trash, None),
            remote("archive-id", "Archive", FolderRole::Archive, None),
            remote("junk-id", "Junk Email", FolderRole::Junk, None),
            remote("proj-id", "Projects", FolderRole::Other, None),
        ];
        let first = env.provider.list_folders().await.unwrap();
        let second = env.provider.list_folders().await.unwrap();
        assert_eq!(first, expected);
        assert_eq!(second, expected);
    }

    #[tokio::test]
    async fn well_known_404_is_skipped() {
        let env = env().await;
        mount_unknown_well_known_404(&env.server).await;
        mount_well_known(&env.server, "inbox", "inbox-id").await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/mailFolders/archive"))
            .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
                "error": {
                    "code": "ErrorItemNotFound",
                    "message": "The archive folder is gone"
                }
            })))
            .mount(&env.server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/mailFolders"))
            .and(query_param("$top", "100"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "value": [
                    folder("inbox-id", "Inbox", 0),
                    folder("archive-id", "Archive", 0),
                ]
            })))
            .mount(&env.server)
            .await;

        let folders = env.provider.list_folders().await.unwrap();
        assert_eq!(
            folders,
            vec![
                remote("inbox-id", "Inbox", FolderRole::Inbox, None),
                remote("archive-id", "Archive", FolderRole::Other, None),
            ]
        );
    }

    #[tokio::test]
    async fn next_link_followed_for_folder_lists() {
        let env = env().await;
        mount_unknown_well_known_404(&env.server).await;
        mount_well_known(&env.server, "inbox", "inbox-id").await;
        let root_next = format!("{}/v1.0/me/mailFolders?$skiptoken=f2", env.server.uri());
        let child_next = format!(
            "{}/v1.0/me/mailFolders/inbox-id/childFolders?$skiptoken=c2",
            env.server.uri()
        );
        Mock::given(method("GET"))
            .and(path("/v1.0/me/mailFolders"))
            .and(query_param("$top", "100"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "value": [folder("inbox-id", "Inbox", 1)],
                "@odata.nextLink": root_next,
            })))
            .mount(&env.server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/mailFolders"))
            .and(query_param("$skiptoken", "f2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "value": [folder("custom-id", "Later", 0)]
            })))
            .mount(&env.server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/mailFolders/inbox-id/childFolders"))
            .and(query_param("$top", "100"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "value": [folder("child-a", "A", 0)],
                "@odata.nextLink": child_next,
            })))
            .mount(&env.server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/mailFolders/inbox-id/childFolders"))
            .and(query_param("$skiptoken", "c2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "value": [folder("child-b", "B", 0)]
            })))
            .mount(&env.server)
            .await;

        let folders = env.provider.list_folders().await.unwrap();
        assert_eq!(
            folders,
            vec![
                remote("inbox-id", "Inbox", FolderRole::Inbox, None),
                remote("child-a", "A", FolderRole::Other, Some("inbox-id")),
                remote("child-b", "B", FolderRole::Other, Some("inbox-id")),
                remote("custom-id", "Later", FolderRole::Other, None),
            ]
        );
    }

    #[tokio::test]
    async fn delta_pages_then_delta_link() {
        let env = env().await;
        let page2 = format!(
            "{}/v1.0/me/mailFolders/inbox-id/messages/delta?$skiptoken=page2",
            env.server.uri()
        );
        let delta_link = format!(
            "{}/v1.0/me/mailFolders/inbox-id/messages/delta?$deltatoken=token1",
            env.server.uri()
        );
        let page3 = format!(
            "{}/v1.0/me/mailFolders/inbox-id/messages/delta?$skiptoken=page3",
            env.server.uri()
        );
        Mock::given(method("GET"))
            .and(path(DELTA_PATH))
            .and(query_param("$select", SELECT))
            .and(header("prefer", "odata.maxpagesize=25"))
            .and(header("authorization", "Bearer at-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "value": [message("m1")],
                "@odata.nextLink": page2,
            })))
            .mount(&env.server)
            .await;
        Mock::given(method("GET"))
            .and(path(DELTA_PATH))
            .and(query_param("$skiptoken", "page2"))
            .and(header("prefer", "odata.maxpagesize=25"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "value": [message("m2")],
                "@odata.deltaLink": delta_link,
            })))
            .mount(&env.server)
            .await;
        Mock::given(method("GET"))
            .and(path(DELTA_PATH))
            .and(query_param("$deltatoken", "token1"))
            .and(header("prefer", "odata.maxpagesize=25"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "value": [],
                "@odata.nextLink": page3,
            })))
            .mount(&env.server)
            .await;

        let first = env.provider.sync_folder(&inbox(), None, 25).await.unwrap();
        assert!(first.more);
        assert!(!first.reset);
        assert_no_flag_side_channel(&first);
        assert_eq!(first.deleted_remote_ids, Vec::<String>::new());
        assert_eq!(first.upserts.len(), 1);
        assert_full_message(&first.upserts[0], "m1");
        assert_eq!(
            first.new_state,
            FolderSyncState::Graph {
                delta_link: None,
                next_link: Some(page2.clone()),
            }
        );

        let second = env
            .provider
            .sync_folder(&inbox(), Some(first.new_state), 25)
            .await
            .unwrap();
        assert!(!second.more);
        assert!(!second.reset);
        assert_no_flag_side_channel(&second);
        assert_eq!(second.upserts.len(), 1);
        assert_full_message(&second.upserts[0], "m2");
        assert_eq!(
            second.new_state,
            FolderSyncState::Graph {
                delta_link: Some(delta_link.clone()),
                next_link: None,
            }
        );

        let third = env
            .provider
            .sync_folder(&inbox(), Some(second.new_state), 25)
            .await
            .unwrap();
        assert!(third.more);
        assert!(third.upserts.is_empty());
        assert_eq!(
            third.new_state,
            FolderSyncState::Graph {
                delta_link: Some(delta_link),
                next_link: Some(page3),
            }
        );
    }

    #[tokio::test]
    async fn removed_items_reported() {
        let env = env().await;
        let delta_link = format!(
            "{}/v1.0/me/mailFolders/inbox-id/messages/delta?$deltatoken=after-remove",
            env.server.uri()
        );
        Mock::given(method("GET"))
            .and(path(DELTA_PATH))
            .and(query_param("$select", SELECT))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "value": [
                    message("m1"),
                    {"id": "m-gone", "@removed": {"reason": "deleted"}},
                ],
                "@odata.deltaLink": delta_link,
            })))
            .mount(&env.server)
            .await;

        let delta = env.provider.sync_folder(&inbox(), None, 10).await.unwrap();
        assert_eq!(
            delta
                .upserts
                .iter()
                .map(|message| message.remote_id.as_str())
                .collect::<Vec<_>>(),
            vec!["m1"]
        );
        assert_eq!(delta.deleted_remote_ids, vec!["m-gone".to_string()]);
        assert_no_flag_side_channel(&delta);
        assert!(!delta.more);
        assert!(!delta.reset);
    }

    #[tokio::test]
    async fn missing_fields_use_defaults() {
        let env = env().await;
        let long = format!("{}ééEXTRA", "a".repeat(199));
        Mock::given(method("GET"))
            .and(path(DELTA_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "value": [
                    {"id": "bare"},
                    {
                        "id": "long",
                        "subject": "",
                        "bodyPreview": long,
                        "flag": {"flagStatus": "notFlagged"},
                        "from": {"emailAddress": {"address": "ada@example.com"}},
                        "receivedDateTime": "not-a-date",
                    },
                    {
                        "id": "offset",
                        "receivedDateTime": "2026-10-19T16:04:05+01:00",
                    },
                    {"subject": "no id"},
                ],
                "@odata.deltaLink": format!("{}/v1.0/delta-done", env.server.uri()),
            })))
            .mount(&env.server)
            .await;

        let delta = env.provider.sync_folder(&inbox(), None, 10).await.unwrap();
        assert_eq!(delta.upserts.len(), 3);
        let bare = &delta.upserts[0];
        assert_eq!(bare.remote_id, "bare");
        assert_eq!(bare.subject, "");
        assert_eq!(
            bare.from,
            Address {
                name: None,
                email: String::new(),
            }
        );
        assert!(bare.to.is_empty());
        assert!(bare.cc.is_empty());
        assert_eq!(bare.date_ms, 0);
        assert_eq!(bare.snippet, "");
        assert!(!bare.is_read);
        assert!(!bare.is_flagged);
        assert!(!bare.has_attachments);

        let truncated = &delta.upserts[1];
        let expected_snippet: String = format!("{}é", "a".repeat(199));
        assert_eq!(truncated.subject, "");
        assert_eq!(truncated.snippet, expected_snippet);
        assert_eq!(truncated.snippet.chars().count(), 200);
        assert!(!truncated.snippet.contains("EXTRA"));
        assert_eq!(truncated.date_ms, 0);
        assert!(!truncated.is_flagged);
        assert!(!truncated.is_read);
        assert_eq!(truncated.from.email, "ada@example.com");
        assert_eq!(truncated.from.name, None);

        assert_eq!(delta.upserts[2].remote_id, "offset");
        assert_eq!(delta.upserts[2].date_ms, 1_792_422_245_000);
    }

    #[tokio::test]
    async fn gone_triggers_reset() {
        let env = env().await;
        for (token, response) in [
            ("old", ResponseTemplate::new(410).set_body_string("")),
            (
                "code",
                ResponseTemplate::new(400).set_body_json(serde_json::json!({
                    "error": {
                        "code": "SyncStateNotFound",
                        "message": "dump-me secret-body"
                    }
                })),
            ),
            (
                "resync",
                ResponseTemplate::new(410).set_body_json(serde_json::json!({
                    "error": {
                        "code": "resyncRequired",
                        "message": "dump-me-too"
                    }
                })),
            ),
            (
                "lower",
                ResponseTemplate::new(409).set_body_json(serde_json::json!({
                    "error": {"code": "syncStateNotFound"}
                })),
            ),
        ] {
            Mock::given(method("GET"))
                .and(path(DELTA_PATH))
                .and(query_param("$deltatoken", token))
                .respond_with(response)
                .mount(&env.server)
                .await;
        }

        for token in ["old", "code", "resync", "lower"] {
            let link = format!(
                "{}/v1.0/me/mailFolders/inbox-id/messages/delta?$deltatoken={token}",
                env.server.uri()
            );
            let delta = env
                .provider
                .sync_folder(
                    &inbox(),
                    Some(FolderSyncState::Graph {
                        delta_link: Some(link),
                        next_link: None,
                    }),
                    10,
                )
                .await
                .unwrap();
            assert!(delta.reset, "{token}");
            assert!(delta.more, "{token}");
            assert!(delta.upserts.is_empty(), "{token}");
            assert!(delta.deleted_remote_ids.is_empty(), "{token}");
            assert_no_flag_side_channel(&delta);
            assert_eq!(
                delta.new_state,
                FolderSyncState::Graph {
                    delta_link: None,
                    next_link: None,
                }
            );
        }
    }

    #[tokio::test]
    async fn initial_delta_gone_is_error() {
        let env = env().await;
        Mock::given(method("GET"))
            .and(path(DELTA_PATH))
            .and(query_param("$select", SELECT))
            .respond_with(ResponseTemplate::new(410).set_body_json(serde_json::json!({
                "error": {
                    "code": "syncStateNotFound",
                    "message": "dump-me"
                }
            })))
            .mount(&env.server)
            .await;

        for state in [
            None,
            Some(FolderSyncState::Graph {
                delta_link: None,
                next_link: None,
            }),
        ] {
            let err = env
                .provider
                .sync_folder(&inbox(), state, 10)
                .await
                .unwrap_err();
            match err {
                CoreError::Protocol(message) => {
                    assert_eq!(message, "graph 410 syncStateNotFound");
                    assert!(!message.contains("dump-me"));
                }
                other => panic!("unexpected {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn fetch_raw_returns_mime() {
        let mut env = env().await;
        let mime = b"Subject: Hi\r\n\r\nBody\r\n".to_vec();
        Mock::given(method("GET"))
            .and(path("/v1.0/me/messages/a%2Fb/$value"))
            .and(header("authorization", "Bearer at-1"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(mime.clone(), "message/rfc822"))
            .mount(&env.server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/messages/missing/$value"))
            .respond_with(ResponseTemplate::new(404).set_body_json(serde_json::json!({
                "error": {"code": "ErrorItemNotFound", "message": "no such message"}
            })))
            .mount(&env.server)
            .await;

        let raw = env
            .provider
            .fetch_raw("folder/ignored", "a/b")
            .await
            .unwrap();
        assert_eq!(raw, mime);

        let missing = env
            .provider
            .fetch_raw("folder/ignored", "missing")
            .await
            .unwrap_err();
        match missing {
            CoreError::NotFound(id) => assert_eq!(id, "missing"),
            other => panic!("unexpected {other:?}"),
        }

        env.provider.max_raw_bytes = 4;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/messages/big/$value"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"hello".to_vec()))
            .mount(&env.server)
            .await;
        let oversized = env.provider.fetch_raw("inbox", "big").await.unwrap_err();
        match oversized {
            CoreError::Protocol(message) => {
                assert!(message.contains("exceeds"), "{message}");
                assert!(!message.contains("hello"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn retries_once_on_401_then_needs_sign_in() {
        let env = env().await;
        // Higher priority than the harness token mock, and exhausted in order.
        Mock::given(method("POST"))
            .and(path("/common/oauth2/v2.0/token"))
            .respond_with(token_response("at-1"))
            .with_priority(1)
            .up_to_n_times(1)
            .mount(&env.server)
            .await;
        Mock::given(method("POST"))
            .and(path("/common/oauth2/v2.0/token"))
            .respond_with(token_response("at-2"))
            .with_priority(2)
            .up_to_n_times(1)
            .mount(&env.server)
            .await;
        Mock::given(method("POST"))
            .and(path("/common/oauth2/v2.0/token"))
            .respond_with(token_response("at-3"))
            .with_priority(3)
            .mount(&env.server)
            .await;
        Mock::given(method("GET"))
            .and(path(DELTA_PATH))
            .and(header("authorization", "Bearer at-1"))
            .respond_with(ResponseTemplate::new(401))
            .with_priority(1)
            .up_to_n_times(1)
            .mount(&env.server)
            .await;
        Mock::given(method("GET"))
            .and(path(DELTA_PATH))
            .and(header("authorization", "Bearer at-2"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "value": [message("m1")],
                "@odata.deltaLink": format!("{}/v1.0/delta-ok", env.server.uri()),
            })))
            .with_priority(2)
            .mount(&env.server)
            .await;

        let delta = env.provider.sync_folder(&inbox(), None, 10).await.unwrap();
        assert_eq!(delta.upserts[0].remote_id, "m1");

        Mock::given(method("GET"))
            .and(path(DELTA_PATH))
            .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
                "error": {"code": "InvalidAuthenticationToken", "message": "at-2 leaked"}
            })))
            .with_priority(1)
            .mount(&env.server)
            .await;
        let err = env
            .provider
            .sync_folder(&inbox(), None, 10)
            .await
            .unwrap_err();
        let rendered = err.to_string();
        match err {
            CoreError::NeedsSignIn { account_id } => {
                assert_eq!(account_id, env.account.as_str());
            }
            other => panic!("unexpected {other:?}"),
        }
        assert!(!rendered.contains("at-1"), "{rendered}");
        assert!(!rendered.contains("at-2"), "{rendered}");
        assert!(!rendered.contains("at-3"), "{rendered}");
        assert!(!rendered.contains("rt-1"), "{rendered}");

        let requests = env.server.received_requests().await.unwrap();
        let token_calls = requests
            .iter()
            .filter(|request| request.url.path().ends_with("/token"))
            .count();
        assert_eq!(token_calls, 3);
    }

    #[tokio::test]
    async fn honors_retry_after() {
        let env = env().await;
        Mock::given(method("GET"))
            .and(path(DELTA_PATH))
            .and(query_param("$select", SELECT))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "value": [message("m1")],
                "@odata.deltaLink": format!("{}/v1.0/delta-ok", env.server.uri()),
            })))
            .mount(&env.server)
            .await;
        Mock::given(method("GET"))
            .and(path(DELTA_PATH))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("Retry-After", "45")
                    .set_body_json(serde_json::json!({
                        "error": {"code": "TooManyRequests", "message": "secret-retry-body"}
                    })),
            )
            .with_priority(1)
            .up_to_n_times(1)
            .mount(&env.server)
            .await;

        let delta = env.provider.sync_folder(&inbox(), None, 10).await.unwrap();
        assert_eq!(delta.upserts.len(), 1);
        assert_eq!(*env.slept.lock().unwrap(), vec![Duration::from_secs(30)]);

        env.slept.lock().unwrap().clear();
        Mock::given(method("GET"))
            .and(path(DELTA_PATH))
            .respond_with(
                ResponseTemplate::new(503)
                    .insert_header("Retry-After", "2")
                    .set_body_json(serde_json::json!({
                        "error": {
                            "code": "serviceUnavailable",
                            "message": "secret-retry-body"
                        }
                    })),
            )
            .with_priority(1)
            .up_to_n_times(4)
            .mount(&env.server)
            .await;
        let err = env
            .provider
            .sync_folder(&inbox(), None, 10)
            .await
            .unwrap_err();
        match err {
            CoreError::Protocol(message) => {
                assert_eq!(message, "graph 503 serviceUnavailable");
                assert!(!message.contains("secret-retry-body"));
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(*env.slept.lock().unwrap(), vec![Duration::from_secs(2); 3]);
    }
}
