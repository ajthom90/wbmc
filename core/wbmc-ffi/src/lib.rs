// This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0. If a copy of the MPL was not distributed with this file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! UniFFI facade over `wbmc-core` for the native apps.
//!
//! `WbmcCore` owns a tokio runtime. Every async export runs its work on that runtime, so
//! the foreign caller's executor only awaits a join handle. Store and keychain calls made
//! from async code go through `spawn_blocking`.

mod html;

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use wbmc_core::CoreError;
use wbmc_core::auth::microsoft::{self, SignIn};
use wbmc_core::auth::{Endpoints, TokenManager};
use wbmc_core::events::{CoreEvent, EventSink, SyncStatus};
use wbmc_core::mime::parse_message;
use wbmc_core::model::{
    Account, AccountId, Address, Folder, FolderId, MessageId, MessageSummary, ProviderKind,
};
use wbmc_core::provider::{GraphProvider, ImapConfig, ImapProvider, MailProvider, Security};
use wbmc_core::secrets::{SecretStore, default_store, password_key, refresh_token_key};
use wbmc_core::store::{BodyStore, Store};
use wbmc_core::sync::SyncEngine;

pub(crate) use html::inline_cid_images;

uniffi::setup_scaffolding!();

const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(5 * 60);

// ---------------------------------------------------------------------------
// Errors, records, events
// ---------------------------------------------------------------------------

/// Mirrors `wbmc_core::CoreError` for foreign callers.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, uniffi::Error)]
pub enum FfiError {
    #[error("network error: {message}")]
    Network { message: String },
    #[error("protocol error: {message}")]
    Protocol { message: String },
    #[error("storage error: {message}")]
    Storage { message: String },
    #[error("account {account_id} needs to sign in again")]
    NeedsSignIn { account_id: String },
    #[error("not found: {message}")]
    NotFound { message: String },
    #[error("invalid input: {message}")]
    InvalidInput { message: String },
}

impl From<CoreError> for FfiError {
    fn from(error: CoreError) -> Self {
        match error {
            CoreError::Network(message) => Self::Network { message },
            CoreError::Protocol(message) => Self::Protocol { message },
            CoreError::Storage(message) => Self::Storage { message },
            CoreError::NeedsSignIn { account_id } => Self::NeedsSignIn { account_id },
            CoreError::NotFound(message) => Self::NotFound { message },
            CoreError::InvalidInput(message) => Self::InvalidInput { message },
        }
    }
}

fn invalid(message: impl Into<String>) -> FfiError {
    FfiError::InvalidInput {
        message: message.into(),
    }
}

fn not_found(message: impl Into<String>) -> FfiError {
    FfiError::NotFound {
        message: message.into(),
    }
}

fn task_failed(error: tokio::task::JoinError) -> FfiError {
    FfiError::Storage {
        message: format!("background task failed: {error}"),
    }
}

type FfiResult<T> = std::result::Result<T, FfiError>;

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct AccountInfo {
    pub id: String,
    /// `"imap"` or `"microsoft"`.
    pub kind: String,
    pub email: String,
    pub display_name: String,
    /// `"idle"`, `"syncing"`, `"error"` or `"needs_sign_in"`.
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct FolderInfo {
    pub id: String,
    pub account_id: String,
    pub name: String,
    pub role: String,
    pub parent_id: Option<String>,
    pub unread_count: u32,
    pub total_count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct MessageRow {
    pub id: String,
    pub subject: String,
    pub from_name: String,
    pub from_email: String,
    pub date_ms: i64,
    pub snippet: String,
    pub is_read: bool,
    pub is_flagged: bool,
    pub has_attachments: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct AttachmentInfo {
    pub filename: String,
    pub mime_type: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct MessageContent {
    pub subject: String,
    pub from_name: String,
    pub from_email: String,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub date_ms: i64,
    /// Sanitising is the web view's job (JavaScript off, remote loads blocked);
    /// `cid:` images are already inlined as `data:` URIs.
    pub html: Option<String>,
    pub text: Option<String>,
    pub attachments: Vec<AttachmentInfo>,
}

#[derive(Clone, PartialEq, Eq, uniffi::Record)]
pub struct ImapAccountInput {
    pub email: String,
    pub display_name: String,
    pub host: String,
    pub port: u16,
    /// `"tls"`, `"starttls"` or `"none"`.
    pub security: String,
    pub username: String,
    pub password: String,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub smtp_security: String,
}

impl std::fmt::Debug for ImapAccountInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImapAccountInput")
            .field("email", &self.email)
            .field("display_name", &self.display_name)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("security", &self.security)
            .field("username", &self.username)
            .field("password", &"<redacted>")
            .field("smtp_host", &self.smtp_host)
            .field("smtp_port", &self.smtp_port)
            .field("smtp_security", &self.smtp_security)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum FfiSyncStatus {
    Idle,
    Syncing { done: u32 },
    Error { message: String },
    NeedsSignIn,
}

impl From<SyncStatus> for FfiSyncStatus {
    fn from(status: SyncStatus) -> Self {
        match status {
            SyncStatus::Idle => Self::Idle,
            SyncStatus::Syncing { done } => Self::Syncing { done },
            SyncStatus::Error { message } => Self::Error { message },
            SyncStatus::NeedsSignIn => Self::NeedsSignIn,
        }
    }
}

fn status_name(status: &SyncStatus) -> &'static str {
    match status {
        SyncStatus::Idle => "idle",
        SyncStatus::Syncing { .. } => "syncing",
        SyncStatus::Error { .. } => "error",
        SyncStatus::NeedsSignIn => "needs_sign_in",
    }
}

/// Mirrors `wbmc_core::events::CoreEvent` with string ids.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum FfiEvent {
    AccountsChanged,
    FoldersChanged {
        account_id: String,
    },
    MessagesChanged {
        folder_id: String,
    },
    SyncStatus {
        account_id: String,
        status: FfiSyncStatus,
    },
}

impl From<CoreEvent> for FfiEvent {
    fn from(event: CoreEvent) -> Self {
        match event {
            CoreEvent::AccountsChanged => Self::AccountsChanged,
            CoreEvent::FoldersChanged { account_id } => Self::FoldersChanged {
                account_id: account_id.as_str().to_string(),
            },
            CoreEvent::MessagesChanged { folder_id } => Self::MessagesChanged {
                folder_id: folder_id.as_str().to_string(),
            },
            CoreEvent::SyncStatus { account_id, status } => Self::SyncStatus {
                account_id: account_id.as_str().to_string(),
                status: status.into(),
            },
        }
    }
}

/// Implemented by the app to receive core events. Called from background threads.
#[uniffi::export(foreign)]
pub trait CoreListener: Send + Sync {
    fn on_event(&self, event: FfiEvent);
}

// ---------------------------------------------------------------------------
// Event bridge: forwards events and tracks per-account status
// ---------------------------------------------------------------------------

struct EventBridge {
    listener: Arc<dyn CoreListener>,
    statuses: Mutex<HashMap<AccountId, &'static str>>,
    /// Accounts removed in this process. Late events from in-flight work are dropped.
    removed: Mutex<HashSet<AccountId>>,
}

impl EventBridge {
    fn statuses(&self) -> MutexGuard<'_, HashMap<AccountId, &'static str>> {
        self.statuses
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn status(&self, account_id: &AccountId) -> &'static str {
        self.statuses().get(account_id).copied().unwrap_or("idle")
    }

    fn set_status(&self, account_id: &AccountId, status: &'static str) {
        self.statuses().insert(account_id.clone(), status);
    }

    fn forget(&self, account_id: &AccountId) {
        self.statuses().remove(account_id);
    }

    fn removed(&self) -> MutexGuard<'_, HashSet<AccountId>> {
        self.removed
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn is_removed(&self, account_id: &AccountId) -> bool {
        self.removed().contains(account_id)
    }
}

impl EventSink for EventBridge {
    fn emit(&self, event: CoreEvent) {
        match &event {
            CoreEvent::SyncStatus { account_id, .. } | CoreEvent::FoldersChanged { account_id }
                if self.is_removed(account_id) =>
            {
                return;
            }
            CoreEvent::SyncStatus { account_id, status } => {
                self.set_status(account_id, status_name(status));
            }
            _ => {}
        }
        self.listener.on_event(event.into());
    }
}

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

struct Inner {
    store: Arc<Store>,
    bodies_root: PathBuf,
    engine: SyncEngine,
    secrets: Arc<dyn SecretStore>,
    tokens: Arc<TokenManager>,
    events: Arc<EventBridge>,
    /// Running syncs, with the task's abort handle once it is spawned.
    syncing: Mutex<HashMap<AccountId, Option<tokio::task::AbortHandle>>>,
    runtime: tokio::runtime::Handle,
}

/// Marks an account as syncing until dropped, so no error path leaves it stuck.
struct SyncGuard {
    inner: Arc<Inner>,
    account_id: AccountId,
}

impl Drop for SyncGuard {
    fn drop(&mut self) {
        // Runs on completion and on abort. A sync that outlived `remove_account` may have
        // written bodies or a rotated refresh token, so purge again before releasing.
        if self.inner.events.is_removed(&self.account_id) {
            self.inner.purge(&self.account_id);
        }
        self.inner.syncing().remove(&self.account_id);
    }
}

/// Runs a store or keychain call on the blocking pool.
async fn blocking<T, F>(work: F) -> FfiResult<T>
where
    T: Send + 'static,
    F: FnOnce() -> wbmc_core::Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(task_failed)?
        .map_err(Into::into)
}

impl Inner {
    fn syncing(&self) -> MutexGuard<'_, HashMap<AccountId, Option<tokio::task::AbortHandle>>> {
        self.syncing
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Registers a sync for the account, or returns `None` when one is already running.
    fn try_begin_sync(self: &Arc<Self>, account_id: &AccountId) -> Option<SyncGuard> {
        if self.events.is_removed(account_id) {
            return None;
        }
        {
            let mut syncing = self.syncing();
            if syncing.contains_key(account_id) {
                return None;
            }
            syncing.insert(account_id.clone(), None);
        }
        self.events.set_status(account_id, "syncing");
        Some(SyncGuard {
            inner: self.clone(),
            account_id: account_id.clone(),
        })
    }

    /// Runs `work` on the core's runtime and waits for it from any executor.
    async fn run<T, F>(&self, work: F) -> FfiResult<T>
    where
        T: Send + 'static,
        F: Future<Output = FfiResult<T>> + Send + 'static,
    {
        self.runtime.spawn(work).await.map_err(task_failed)?
    }

    /// Starts a background sync unless one is already running for the account.
    fn spawn_sync(self: &Arc<Self>, account_id: &AccountId) {
        if let Some(guard) = self.try_begin_sync(account_id) {
            drop(self.spawn_sync_task(guard));
        }
    }

    /// Spawns the sync and records its abort handle so `remove_account` can stop it.
    fn spawn_sync_task(
        self: &Arc<Self>,
        guard: SyncGuard,
    ) -> tokio::task::JoinHandle<FfiResult<()>> {
        let account_id = guard.account_id.clone();
        let task = self.runtime.spawn(self.clone().sync_with(guard));
        if let Some(slot) = self.syncing().get_mut(&account_id) {
            *slot = Some(task.abort_handle());
        }
        // Removal may have happened between spawning and recording the handle.
        if self.events.is_removed(&account_id) {
            task.abort();
        }
        task
    }

    /// Deletes everything an account leaves outside the store. Idempotent.
    fn purge(&self, account_id: &AccountId) {
        for key in [password_key(account_id), refresh_token_key(account_id)] {
            if let Err(error) = self.secrets.delete(&key) {
                tracing::warn!("could not delete a credential of a removed account: {error}");
            }
        }
        self.tokens.invalidate(account_id);
        // The id came from the store, so it is a plain path component.
        match std::fs::remove_dir_all(self.bodies_root.join(account_id.as_str())) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => tracing::warn!("could not delete cached bodies: {error}"),
        }
    }

    async fn sync_with(self: Arc<Self>, guard: SyncGuard) -> FfiResult<()> {
        let result = async {
            let account = self.account(&guard.account_id).await?;
            let provider = self.provider(&account).await?;
            self.engine
                .sync_account(&account, provider.as_ref())
                .await
                .map_err(|error| match error {
                    // Providers may report a username; callers need the account id.
                    CoreError::NeedsSignIn { .. } => FfiError::NeedsSignIn {
                        account_id: account.id.as_str().to_string(),
                    },
                    other => other.into(),
                })
        }
        .await;
        if let Err(error) = &result {
            // Failures before the engine starts would otherwise leave the status "syncing".
            if self.events.status(&guard.account_id) == "syncing" {
                let status = match error {
                    FfiError::NeedsSignIn { .. } => SyncStatus::NeedsSignIn,
                    other => SyncStatus::Error {
                        message: other.to_string(),
                    },
                };
                self.events.emit(CoreEvent::SyncStatus {
                    account_id: guard.account_id.clone(),
                    status,
                });
            }
        }
        drop(guard);
        result
    }

    async fn account(&self, account_id: &AccountId) -> FfiResult<Account> {
        let store = self.store.clone();
        let wanted = account_id.clone();
        blocking(move || {
            Ok(store
                .accounts()?
                .into_iter()
                .find(|account| account.id == wanted))
        })
        .await?
        .ok_or_else(|| not_found(format!("account {}", account_id.as_str())))
    }

    async fn provider(&self, account: &Account) -> FfiResult<Box<dyn MailProvider>> {
        match account.kind {
            ProviderKind::Imap => {
                let config: ImapConfig =
                    serde_json::from_value(account.config.clone()).map_err(|error| {
                        FfiError::Storage {
                            message: format!("invalid IMAP config: {error}"),
                        }
                    })?;
                let secrets = self.secrets.clone();
                let key = password_key(&account.id);
                let password = blocking(move || secrets.get(&key)).await?.ok_or_else(|| {
                    FfiError::NeedsSignIn {
                        account_id: account.id.as_str().to_string(),
                    }
                })?;
                Ok(Box::new(ImapProvider::new(config, password)))
            }
            ProviderKind::Microsoft => Ok(Box::new(GraphProvider::new(
                account.id.clone(),
                self.tokens.clone(),
                Endpoints::default().graph,
            ))),
        }
    }

    fn account_info(&self, account: &Account) -> AccountInfo {
        AccountInfo {
            id: account.id.as_str().to_string(),
            kind: account.kind.as_str().to_string(),
            email: account.email.clone(),
            display_name: account.display_name.clone(),
            status: self.events.status(&account.id).to_string(),
        }
    }

    async fn message_content(self: Arc<Self>, message_id: MessageId) -> FfiResult<MessageContent> {
        let store = self.store.clone();
        let id = message_id.clone();
        let (message, account) = blocking(move || {
            let message = store
                .message(&id)?
                .ok_or_else(|| CoreError::NotFound(format!("message {}", id.as_str())))?;
            let account = store
                .accounts()?
                .into_iter()
                .find(|account| account.id == message.account_id)
                .ok_or_else(|| {
                    CoreError::NotFound(format!("account {}", message.account_id.as_str()))
                })?;
            Ok((message, account))
        })
        .await?;
        // Serve cached bodies without credentials so offline/locked-keychain reads work.
        let inner = self.clone();
        let (cache_account, cache_id) = (account.clone(), message_id.clone());
        let cached = blocking(move || inner.engine.cached_body(&cache_account, &cache_id)).await?;
        let raw = match cached {
            Some(raw) => Ok(raw),
            None => {
                let provider = self.provider(&account).await?;
                self.engine
                    .body(&account, provider.as_ref(), &message_id)
                    .await
            }
        };
        if self.events.is_removed(&account.id) {
            // The fetch may have re-created the body directory after removal.
            let inner = self.clone();
            let id = account.id.clone();
            tokio::task::spawn_blocking(move || inner.purge(&id))
                .await
                .map_err(task_failed)?;
            return Err(not_found(format!("account {}", account.id.as_str())));
        }
        let raw = raw?;
        tokio::task::spawn_blocking(move || build_content(&message, &raw))
            .await
            .map_err(task_failed)
    }

    async fn add_imap_account(self: Arc<Self>, input: ImapAccountInput) -> FfiResult<AccountInfo> {
        let config = imap_config(&input)?;
        // Verify the login before anything is saved. A rejected login is bad input here,
        // not an existing account that needs to sign in again.
        ImapProvider::new(config.clone(), input.password.clone())
            .list_folders()
            .await
            .map_err(|error| match error {
                CoreError::NeedsSignIn { .. } => {
                    invalid("the IMAP server rejected the username or password")
                }
                other => other.into(),
            })?;
        let email = input.email.trim().to_string();
        let display_name = match input.display_name.trim() {
            "" => email.clone(),
            name => name.to_string(),
        };
        let host = config.host.clone();
        let username = config.username.clone();
        let store = self.store.clone();
        let wanted_email = email.clone();
        let existing = blocking(move || {
            Ok(store.accounts()?.into_iter().find(|account| {
                account.kind == ProviderKind::Imap
                    && account.email.eq_ignore_ascii_case(&wanted_email)
                    && imap_host_user(account).is_some_and(|(stored_host, stored_user)| {
                        stored_host.eq_ignore_ascii_case(&host) && stored_user == username
                    })
            }))
        })
        .await?;
        if let Some(mut account) = existing {
            // Same mailbox: keep the id, replace the password, and store the new
            // settings. A second row would sync in parallel and duplicate every folder.
            account.display_name = display_name;
            account.config = serde_json::to_value(&config).map_err(|error| FfiError::Storage {
                message: error.to_string(),
            })?;
            let secrets = self.secrets.clone();
            let store = self.store.clone();
            let key = password_key(&account.id);
            let password = input.password;
            let saved = account.clone();
            blocking(move || {
                secrets.set(&key, &password)?;
                store.update_account(&saved)
            })
            .await?;
            self.events.set_status(&account.id, "idle");
            self.events.emit(CoreEvent::AccountsChanged);
            self.spawn_sync(&account.id);
            return Ok(self.account_info(&account));
        }
        let config = serde_json::to_value(&config).map_err(|error| FfiError::Storage {
            message: error.to_string(),
        })?;
        let account = Account {
            id: AccountId::new(),
            kind: ProviderKind::Imap,
            email,
            display_name,
            config,
        };
        let store = self.store.clone();
        let secrets = self.secrets.clone();
        let saved = account.clone();
        let password = input.password;
        blocking(move || {
            let key = password_key(&saved.id);
            secrets.set(&key, &password)?;
            if let Err(error) = store.insert_account(&saved) {
                let _ = secrets.delete(&key);
                return Err(error);
            }
            Ok(())
        })
        .await?;
        self.events.emit(CoreEvent::AccountsChanged);
        self.spawn_sync(&account.id);
        Ok(self.account_info(&account))
    }

    async fn finish_microsoft(self: Arc<Self>, sign_in: SignIn) -> FfiResult<AccountInfo> {
        let (tokens, email) = sign_in.finish(SIGN_IN_TIMEOUT).await?;
        let store = self.store.clone();
        let manager = self.tokens.clone();
        let account = blocking(move || {
            save_microsoft_account(&store, email, |id| manager.store(id, &tokens))
        })
        .await?;
        // A re-sign-in clears any previous NeedsSignIn state.
        self.events.set_status(&account.id, "idle");
        self.events.emit(CoreEvent::AccountsChanged);
        self.spawn_sync(&account.id);
        Ok(self.account_info(&account))
    }

    async fn remove_account(self: Arc<Self>, account_id: AccountId) -> FfiResult<()> {
        let store = self.store.clone();
        let wanted = account_id.clone();
        let exists =
            blocking(move || Ok(store.accounts()?.iter().any(|account| account.id == wanted)))
                .await?;
        if !exists {
            return Err(not_found(format!("account {}", account_id.as_str())));
        }
        // Mark first: from here on late events are dropped and finishing work re-purges.
        self.events.removed().insert(account_id.clone());
        let running = self.syncing().get(&account_id).cloned().flatten();
        if let Some(task) = running {
            task.abort();
        }
        let store = self.store.clone();
        let id = account_id.clone();
        if let Err(error) = blocking(move || store.delete_account(&id)).await {
            self.events.removed().remove(&account_id);
            return Err(error);
        }
        // Keychain deletes and removing every cached body must not run on the caller thread.
        let purger = Arc::clone(&self);
        let id = account_id.clone();
        tokio::task::spawn_blocking(move || purger.purge(&id))
            .await
            .map_err(task_failed)?;
        self.events.forget(&account_id);
        self.events.emit(CoreEvent::AccountsChanged);
        Ok(())
    }
}

/// Re-uses the Microsoft account with this email (case-insensitive) or inserts a new one,
/// then stores its tokens. A newly inserted row is deleted again if storing tokens fails.
fn save_microsoft_account(
    store: &Store,
    email: String,
    store_tokens: impl FnOnce(&AccountId) -> wbmc_core::Result<()>,
) -> wbmc_core::Result<Account> {
    let existing = store.accounts()?.into_iter().find(|account| {
        account.kind == ProviderKind::Microsoft && account.email.eq_ignore_ascii_case(&email)
    });
    if let Some(account) = existing {
        store_tokens(&account.id)?;
        return Ok(account);
    }
    let account = Account {
        id: AccountId::new(),
        kind: ProviderKind::Microsoft,
        display_name: email.clone(),
        email,
        config: serde_json::json!({}),
    };
    store.insert_account(&account)?;
    if let Err(error) = store_tokens(&account.id) {
        if let Err(cleanup) = store.delete_account(&account.id) {
            tracing::warn!("could not roll back a new Microsoft account: {cleanup}");
        }
        return Err(error);
    }
    Ok(account)
}

fn parse_security(field: &str, value: &str) -> FfiResult<Security> {
    match value.trim().to_ascii_lowercase().as_str() {
        "tls" => Ok(Security::Tls),
        "starttls" => Ok(Security::StartTls),
        "none" => Ok(Security::None),
        other => Err(invalid(format!(
            "{field} must be \"tls\", \"starttls\" or \"none\", not {other:?}"
        ))),
    }
}

fn imap_host_user(account: &Account) -> Option<(String, String)> {
    let config: ImapConfig = serde_json::from_value(account.config.clone()).ok()?;
    Some((config.host, config.username))
}

fn imap_config(input: &ImapAccountInput) -> FfiResult<ImapConfig> {
    for (field, value) in [
        ("email", &input.email),
        ("host", &input.host),
        ("username", &input.username),
    ] {
        if value.trim().is_empty() {
            return Err(invalid(format!("{field} is required")));
        }
    }
    if input.port == 0 {
        return Err(invalid("port must be greater than 0"));
    }
    let security = parse_security("security", &input.security)?;
    let smtp_security = if input.smtp_security.trim().is_empty() {
        security
    } else {
        parse_security("smtp_security", &input.smtp_security)?
    };
    let host = input.host.trim().to_string();
    let smtp_host = match input.smtp_host.trim() {
        "" => host.clone(),
        smtp => smtp.to_string(),
    };
    Ok(ImapConfig {
        host,
        port: input.port,
        security,
        username: input.username.trim().to_string(),
        smtp_host,
        smtp_port: input.smtp_port,
        smtp_security,
    })
}

fn format_address(address: &Address) -> String {
    match address.name.as_deref() {
        Some(name) if !name.is_empty() && !address.email.is_empty() => {
            format!("{name} <{}>", address.email)
        }
        Some(name) if !name.is_empty() => name.to_string(),
        _ => address.email.clone(),
    }
}

fn build_content(message: &MessageSummary, raw: &[u8]) -> MessageContent {
    let parsed = parse_message(raw, message.date_ms);
    let html = parsed.html.map(|html| inline_cid_images(&html, raw));
    MessageContent {
        subject: parsed.subject,
        from_name: parsed.from.name.unwrap_or_default(),
        from_email: parsed.from.email,
        to: parsed.to.iter().map(format_address).collect(),
        cc: parsed.cc.iter().map(format_address).collect(),
        date_ms: parsed.date_ms,
        html,
        text: parsed.text,
        attachments: parsed
            .attachments
            .into_iter()
            .filter(|attachment| !attachment.inline)
            .map(|attachment| AttachmentInfo {
                filename: attachment.filename,
                mime_type: attachment.mime_type,
                size: attachment.size,
            })
            .collect(),
    }
}

fn folder_info(folder: Folder) -> FolderInfo {
    FolderInfo {
        id: folder.id.as_str().to_string(),
        account_id: folder.account_id.as_str().to_string(),
        name: folder.name,
        role: folder.role.as_str().to_string(),
        parent_id: folder.parent_id.map(|id| id.as_str().to_string()),
        unread_count: folder.unread_count,
        total_count: folder.total_count,
    }
}

fn message_row(message: MessageSummary) -> MessageRow {
    MessageRow {
        id: message.id.as_str().to_string(),
        subject: message.subject,
        from_name: message.from.name.unwrap_or_default(),
        from_email: message.from.email,
        date_ms: message.date_ms,
        snippet: message.snippet,
        is_read: message.is_read,
        is_flagged: message.is_flagged,
        has_attachments: message.has_attachments,
    }
}

// ---------------------------------------------------------------------------
// Exported objects
// ---------------------------------------------------------------------------

/// The core facade. One per app process.
#[derive(uniffi::Object)]
pub struct WbmcCore {
    inner: Arc<Inner>,
    runtime: Option<tokio::runtime::Runtime>,
}

impl Drop for WbmcCore {
    fn drop(&mut self) {
        // `shutdown_background` is allowed even when dropped inside an async context.
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

impl WbmcCore {
    /// Builds a core with an explicit secret store (tests use `MemorySecrets`).
    pub(crate) fn with_secrets(
        data_dir: String,
        listener: Arc<dyn CoreListener>,
        secrets: Arc<dyn SecretStore>,
    ) -> FfiResult<Arc<Self>> {
        let data_dir = PathBuf::from(data_dir);
        std::fs::create_dir_all(&data_dir).map_err(CoreError::from)?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .thread_name("wbmc-core")
            .enable_all()
            .build()
            .map_err(CoreError::from)?;
        let store = Arc::new(Store::open(&data_dir.join("wbmc.sqlite"))?);
        let bodies_root = data_dir.join("bodies");
        let bodies = Arc::new(BodyStore::new(bodies_root.clone()));
        let events = Arc::new(EventBridge {
            listener,
            statuses: Mutex::new(HashMap::new()),
            removed: Mutex::new(HashSet::new()),
        });
        // An unconfigured client id only matters once a Microsoft account is used.
        let client_id = microsoft::client_id().unwrap_or_default();
        let tokens = Arc::new(TokenManager::new(secrets.clone(), client_id));
        let engine = SyncEngine::new(store.clone(), bodies, events.clone());
        let inner = Arc::new(Inner {
            store,
            bodies_root,
            engine,
            secrets,
            tokens,
            events,
            syncing: Mutex::new(HashMap::new()),
            runtime: runtime.handle().clone(),
        });
        Ok(Arc::new(Self {
            inner,
            runtime: Some(runtime),
        }))
    }

    /// Whether a sync is running for the account (test hook; not exported).
    #[cfg(test)]
    pub(crate) fn is_syncing(&self, account_id: &str) -> bool {
        self.inner
            .syncing()
            .contains_key(&AccountId::from(account_id.to_string()))
    }
}

#[uniffi::export(async_runtime = "tokio")]
impl WbmcCore {
    /// Opens (or creates) the store under `data_dir`. Credentials go to the OS keychain,
    /// or to memory when `WBMC_SECRETS=memory` (tests and CI).
    #[uniffi::constructor]
    pub fn new(data_dir: String, listener: Arc<dyn CoreListener>) -> FfiResult<Arc<Self>> {
        Self::with_secrets(data_dir, listener, default_store())
    }

    pub fn accounts(&self) -> Vec<AccountInfo> {
        match self.inner.store.accounts() {
            Ok(accounts) => accounts
                .iter()
                .map(|account| self.inner.account_info(account))
                .collect(),
            Err(error) => {
                tracing::warn!("accounts: {error}");
                Vec::new()
            }
        }
    }

    /// Verifies the login (by listing folders) before saving the account, then starts a
    /// background sync and returns without waiting for it.
    pub async fn add_imap_account(&self, input: ImapAccountInput) -> FfiResult<AccountInfo> {
        let inner = self.inner.clone();
        self.inner.run(inner.add_imap_account(input)).await
    }

    pub fn begin_microsoft_sign_in(&self) -> FfiResult<Arc<MicrosoftSignIn>> {
        let client_id = microsoft::client_id()?;
        let sign_in = SignIn::start(&client_id)?;
        Ok(Arc::new(MicrosoftSignIn {
            inner: self.inner.clone(),
            url: sign_in.url(),
            sign_in: Mutex::new(Some(sign_in)),
        }))
    }

    /// Deletes the account, its credentials and its cached message bodies.
    /// Keychain and body-directory work runs on the blocking pool.
    pub async fn remove_account(&self, account_id: String) -> FfiResult<()> {
        let inner = self.inner.clone();
        self.inner
            .run(inner.remove_account(AccountId::from(account_id)))
            .await
    }

    pub fn folders(&self, account_id: String) -> Vec<FolderInfo> {
        match self.inner.store.folders(&AccountId::from(account_id)) {
            Ok(folders) => folders.into_iter().map(folder_info).collect(),
            Err(error) => {
                tracing::warn!("folders: {error}");
                Vec::new()
            }
        }
    }

    /// Newest first.
    pub fn messages(&self, folder_id: String, offset: u32, limit: u32) -> Vec<MessageRow> {
        match self
            .inner
            .store
            .messages(&FolderId::from(folder_id), offset, limit)
        {
            Ok(messages) => messages.into_iter().map(message_row).collect(),
            Err(error) => {
                tracing::warn!("messages: {error}");
                Vec::new()
            }
        }
    }

    /// Fetches the body if it is not cached yet.
    pub async fn message_content(&self, message_id: String) -> FfiResult<MessageContent> {
        let inner = self.inner.clone();
        self.inner
            .run(inner.message_content(MessageId::from(message_id)))
            .await
    }

    /// Syncs one account. Returns `Ok` at once if that account is already syncing.
    pub async fn sync_account(&self, account_id: String) -> FfiResult<()> {
        let account_id = AccountId::from(account_id);
        let Some(guard) = self.inner.try_begin_sync(&account_id) else {
            return Ok(());
        };
        match self.inner.spawn_sync_task(guard).await {
            Ok(result) => result,
            Err(error) if error.is_cancelled() => Err(not_found(format!(
                "account {} was removed",
                account_id.as_str()
            ))),
            Err(error) => Err(task_failed(error)),
        }
    }

    /// Syncs every account concurrently. Per-account errors arrive as `SyncStatus` events.
    pub async fn sync_all(&self) {
        let inner = self.inner.clone();
        let result = self
            .inner
            .run(async move {
                let store = inner.store.clone();
                let accounts = blocking(move || store.accounts()).await?;
                let runs: Vec<_> = accounts
                    .iter()
                    .filter_map(|account| inner.try_begin_sync(&account.id))
                    .map(|guard| inner.spawn_sync_task(guard))
                    .collect();
                for run in futures::future::join_all(runs).await {
                    match run {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => tracing::warn!("sync failed: {error}"),
                        Err(error) => tracing::warn!("sync task failed: {error}"),
                    }
                }
                Ok(())
            })
            .await;
        if let Err(error) = result {
            tracing::warn!("sync_all: {error}");
        }
    }
}

#[cfg(feature = "debug-tools")]
#[uniffi::export]
impl WbmcCore {
    /// Debug builds only: the stored refresh token, for capturing test credentials.
    pub fn debug_refresh_token(&self, account_id: String) -> Option<String> {
        self.inner
            .secrets
            .get(&refresh_token_key(&AccountId::from(account_id)))
            .ok()
            .flatten()
    }
}

/// An interactive Microsoft sign-in: open `url()` in the browser, then await `finish()`.
#[derive(uniffi::Object)]
pub struct MicrosoftSignIn {
    inner: Arc<Inner>,
    url: String,
    sign_in: Mutex<Option<SignIn>>,
}

#[uniffi::export(async_runtime = "tokio")]
impl MicrosoftSignIn {
    pub fn url(&self) -> String {
        self.url.clone()
    }

    /// Waits up to five minutes for the browser redirect, then adds (or re-authorises)
    /// the account and starts a background sync.
    pub async fn finish(&self) -> FfiResult<AccountInfo> {
        let sign_in = self
            .sign_in
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
            .ok_or_else(|| invalid("this sign-in has already finished"))?;
        let inner = self.inner.clone();
        self.inner.run(inner.finish_microsoft(sign_in)).await
    }
}

#[cfg(test)]
mod tests;
