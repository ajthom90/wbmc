// This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0. If a copy of the MPL was not distributed with this file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use crate::events::{CoreEvent, EventSink, SyncStatus};
use crate::model::{Account, Folder, FolderId, FolderRole, MessageId, MessageSummary};
use crate::provider::{FolderSyncState, MailProvider, RemoteFolder, RemoteMessage};
use crate::store::{BodyStore, FlagChange, PageChanges, Store};
use crate::{CoreError, Result};

const PAGE_SIZE: u32 = 200;
/// Bounds single-shot provider calls (folder listing, body fetch) so a stalled server
/// cannot pin the single-flight slot.
const PROVIDER_CALL_TIMEOUT: Duration = Duration::from_secs(120);

async fn call_provider<T>(work: impl Future<Output = Result<T>>) -> Result<T> {
    provider_call_with_timeout(PROVIDER_CALL_TIMEOUT, work).await
}

async fn provider_call_with_timeout<T>(
    timeout: Duration,
    work: impl Future<Output = Result<T>>,
) -> Result<T> {
    match tokio::time::timeout(timeout, work).await {
        Ok(result) => result,
        Err(_) => Err(CoreError::Network(
            "the mail server did not respond in time".into(),
        )),
    }
}

pub struct SyncEngine {
    store: Arc<Store>,
    bodies: Arc<BodyStore>,
    events: Arc<dyn EventSink>,
}

impl SyncEngine {
    pub fn new(store: Arc<Store>, bodies: Arc<BodyStore>, events: Arc<dyn EventSink>) -> Self {
        Self {
            store,
            bodies,
            events,
        }
    }

    pub async fn sync_account(&self, account: &Account, provider: &dyn MailProvider) -> Result<()> {
        self.status(account, SyncStatus::Syncing { done: 0 });
        let result = self.sync(account, provider).await;
        let status = match &result {
            Ok(()) => SyncStatus::Idle,
            Err(CoreError::NeedsSignIn { .. }) => SyncStatus::NeedsSignIn,
            Err(error) => SyncStatus::Error {
                message: error.to_string(),
            },
        };
        self.status(account, status);
        result
    }

    /// Returns the cached raw body without touching the network or credentials.
    pub fn cached_body(&self, account: &Account, id: &MessageId) -> Result<Option<Vec<u8>>> {
        self.bodies.get(&account.id, id)
    }

    /// Return raw RFC 5322 bytes, caching the first successful fetch.
    pub async fn body(
        &self,
        account: &Account,
        provider: &dyn MailProvider,
        id: &MessageId,
    ) -> Result<Vec<u8>> {
        let message = self
            .store
            .message(id)?
            .filter(|message| message.account_id == account.id)
            .ok_or_else(|| CoreError::NotFound(format!("message {}", id.as_str())))?;
        if let Some(raw) = self.bodies.get(&account.id, id)? {
            return Ok(raw);
        }
        let folder = self
            .store
            .folders(&account.id)?
            .into_iter()
            .find(|folder| folder.id == message.folder_id)
            .ok_or_else(|| CoreError::NotFound(format!("folder {}", message.folder_id.as_str())))?;
        let raw = call_provider(provider.fetch_raw(&folder.remote_id, &message.remote_id)).await?;
        self.bodies.put(&account.id, id, &raw)?;
        Ok(raw)
    }

    async fn sync(&self, account: &Account, provider: &dyn MailProvider) -> Result<()> {
        let remote_folders = call_provider(provider.list_folders()).await?;
        let mut folders = self.upsert_folders(account, remote_folders)?;
        self.events.emit(CoreEvent::FoldersChanged {
            account_id: account.id.clone(),
        });
        folders.sort_by_key(|(folder, _)| role_order(folder.role));
        let mut done = 0;
        // One folder's failure must not skip the rest or the Inbox prefetch.
        // NeedsSignIn is account-wide and still stops the pass.
        let mut failed = Vec::new();
        for (folder, remote) in &folders {
            match self
                .sync_folder(account, provider, folder, remote, &mut done)
                .await
            {
                Ok(()) => {}
                Err(error @ CoreError::NeedsSignIn { .. }) => return Err(error),
                Err(error) => {
                    let label = if folder.name.is_empty() {
                        remote.remote_id.clone()
                    } else {
                        folder.name.clone()
                    };
                    failed.push((label, error));
                }
            }
        }
        for (folder, _) in &folders {
            if folder.role == FolderRole::Inbox {
                self.prefetch(account, provider, &folder.id).await?;
            }
        }
        if failed.is_empty() {
            Ok(())
        } else {
            let detail = failed
                .iter()
                .map(|(name, error)| format!("{name}: {error}"))
                .collect::<Vec<_>>()
                .join(", ");
            Err(CoreError::Protocol(format!("failed to sync {detail}")))
        }
    }

    fn upsert_folders(
        &self,
        account: &Account,
        remote_folders: Vec<RemoteFolder>,
    ) -> Result<Vec<(Folder, RemoteFolder)>> {
        // First obtain all stable local ids; the provider may list children first.
        let mut folders = Vec::with_capacity(remote_folders.len());
        let mut ids = HashMap::new();
        for remote in remote_folders {
            let folder = self.store.upsert_folder(&Folder {
                id: FolderId::new(),
                account_id: account.id.clone(),
                remote_id: remote.remote_id.clone(),
                name: remote.name.clone(),
                role: remote.role,
                parent_id: None,
                unread_count: 0,
                total_count: 0,
            })?;
            ids.insert(remote.remote_id.clone(), folder.id.clone());
            folders.push((folder, remote));
        }
        for (folder, remote) in &mut folders {
            if let Some(parent) = &remote.parent_remote_id {
                // Unlisted parents (such as Gmail's nonselectable container)
                // leave their selectable children at the top level.
                folder.parent_id = ids.get(parent).cloned();
                *folder = self.store.upsert_folder(folder)?;
            }
        }
        Ok(folders)
    }

    async fn sync_folder(
        &self,
        account: &Account,
        provider: &dyn MailProvider,
        folder: &Folder,
        remote: &RemoteFolder,
        done: &mut u32,
    ) -> Result<()> {
        let mut state: Option<FolderSyncState> = self
            .store
            .folder_sync_state(&folder.id)?
            .map(|state| serde_json::from_str(&state))
            .transpose()
            .map_err(|error| CoreError::Storage(format!("invalid folder sync state: {error}")))?;
        loop {
            // No overall cap here: a healthy final page refreshes flags for every known
            // message, which can exceed any fixed deadline on very large folders. Each
            // provider command inside is already timed individually.
            let delta = provider.sync_folder(remote, state, PAGE_SIZE).await?;
            if delta.reset {
                self.remove_bodies(account, self.store.reset_folder(&folder.id)?)?;
            }
            let upserts: Vec<_> = delta
                .upserts
                .into_iter()
                .map(|message| message_summary(folder, message))
                .collect();
            let flags: Vec<_> = delta
                .flag_updates
                .into_iter()
                .map(|flag| FlagChange {
                    remote_id: flag.remote_id,
                    is_read: flag.is_read,
                    is_flagged: flag.is_flagged,
                })
                .collect();
            let sync_state = serde_json::to_string(&delta.new_state)
                .map_err(|error| CoreError::Storage(error.to_string()))?;
            let applied = self.store.apply_sync_page(
                &folder.id,
                &PageChanges {
                    upserts: &upserts,
                    deleted_remote_ids: &delta.deleted_remote_ids,
                    flag_updates: &flags,
                    retain_only_remote_ids: delta.server_remote_ids.as_deref(),
                    sync_state: &sync_state,
                },
            )?;
            self.remove_bodies(account, applied.removed_ids)?;
            *done =
                done.saturating_add(u32::try_from(applied.stored_ids.len()).unwrap_or(u32::MAX));
            self.events.emit(CoreEvent::MessagesChanged {
                folder_id: folder.id.clone(),
            });
            self.status(account, SyncStatus::Syncing { done: *done });
            if !delta.more {
                return Ok(());
            }
            state = Some(delta.new_state);
        }
    }

    fn remove_bodies(&self, account: &Account, ids: Vec<MessageId>) -> Result<()> {
        for id in ids {
            match self.bodies.remove(&account.id, &id) {
                Ok(()) | Err(CoreError::NotFound(_)) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    async fn prefetch(
        &self,
        account: &Account,
        provider: &dyn MailProvider,
        inbox: &FolderId,
    ) -> Result<()> {
        let mut pending = Vec::new();
        for message in self.store.messages(inbox, 0, PAGE_SIZE)? {
            if self.cached_body(account, &message.id)?.is_none() {
                pending.push(message);
            }
        }
        if pending.is_empty() {
            return Ok(());
        }
        let folder = self
            .store
            .folders(&account.id)?
            .into_iter()
            .find(|folder| folder.id == *inbox)
            .ok_or_else(|| CoreError::NotFound(format!("folder {}", inbox.as_str())))?;
        let ids: Vec<String> = pending
            .iter()
            .map(|message| message.remote_id.clone())
            .collect();
        let fetched = provider.fetch_raw_many(&folder.remote_id, &ids).await;
        let mut needs_sign_in = None;
        for (message, (_remote_id, result)) in pending.iter().zip(fetched) {
            match result {
                Ok(raw) => self.bodies.put(&account.id, &message.id, &raw)?,
                Err(error @ CoreError::NeedsSignIn { .. }) => needs_sign_in = Some(error),
                Err(error) => {
                    tracing::warn!("body prefetch failed for {}: {error}", message.id.as_str());
                }
            }
        }
        if let Some(error) = needs_sign_in {
            return Err(error);
        }
        Ok(())
    }

    fn status(&self, account: &Account, status: SyncStatus) {
        self.events.emit(CoreEvent::SyncStatus {
            account_id: account.id.clone(),
            status,
        });
    }
}

fn role_order(role: FolderRole) -> u8 {
    match role {
        FolderRole::Inbox => 0,
        FolderRole::Sent => 1,
        FolderRole::Drafts => 2,
        _ => 3,
    }
}

fn message_summary(folder: &Folder, message: RemoteMessage) -> MessageSummary {
    MessageSummary {
        id: MessageId::new(),
        account_id: folder.account_id.clone(),
        folder_id: folder.id.clone(),
        remote_id: message.remote_id,
        subject: message.subject,
        from: message.from,
        to: message.to,
        cc: message.cc,
        date_ms: message.date_ms,
        snippet: message.snippet,
        is_read: message.is_read,
        is_flagged: message.is_flagged,
        has_attachments: message.has_attachments,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use super::SyncEngine;
    use crate::CoreError;
    use crate::events::{CoreEvent, EventSink, SyncStatus};
    use crate::model::{Account, AccountId, Address, Folder, FolderRole, ProviderKind};
    use crate::provider::fake::FakeProvider;
    use crate::provider::{FlagUpdate, FolderDelta, FolderSyncState, RemoteFolder, RemoteMessage};
    use crate::store::{BodyStore, Store};

    #[derive(Default)]
    struct Events(Mutex<Vec<CoreEvent>>);

    impl EventSink for Events {
        fn emit(&self, event: CoreEvent) {
            self.0.lock().unwrap().push(event);
        }
    }

    struct Harness {
        _dir: tempfile::TempDir,
        account: Account,
        store: Arc<Store>,
        bodies: Arc<BodyStore>,
        events: Arc<Events>,
        engine: SyncEngine,
    }

    impl Harness {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let store = Arc::new(Store::open(&dir.path().join("mail.db")).unwrap());
            let bodies = Arc::new(BodyStore::new(dir.path().join("bodies")));
            let events = Arc::new(Events::default());
            let account = Account {
                id: AccountId::new(),
                kind: ProviderKind::Imap,
                email: "test@example.com".into(),
                display_name: "Test".into(),
                config: serde_json::json!({}),
            };
            store.insert_account(&account).unwrap();
            let engine = SyncEngine::new(store.clone(), bodies.clone(), events.clone());
            Self {
                _dir: dir,
                account,
                store,
                bodies,
                events,
                engine,
            }
        }

        fn folder(&self, remote_id: &str) -> Folder {
            self.store
                .folders(&self.account.id)
                .unwrap()
                .into_iter()
                .find(|folder| folder.remote_id == remote_id)
                .unwrap()
        }

        fn statuses(&self) -> Vec<SyncStatus> {
            self.events
                .0
                .lock()
                .unwrap()
                .iter()
                .filter_map(|event| match event {
                    CoreEvent::SyncStatus { account_id, status } => {
                        assert_eq!(account_id, &self.account.id);
                        Some(status.clone())
                    }
                    _ => None,
                })
                .collect()
        }
    }

    fn remote_folder(remote_id: &str, role: FolderRole) -> RemoteFolder {
        RemoteFolder {
            remote_id: remote_id.into(),
            name: remote_id.into(),
            role,
            parent_remote_id: None,
        }
    }

    fn remote_message(number: u32) -> RemoteMessage {
        RemoteMessage {
            remote_id: number.to_string(),
            subject: format!("Subject {number}"),
            from: Address {
                name: Some("Sender".into()),
                email: "sender@example.com".into(),
            },
            to: vec![],
            cc: vec![],
            date_ms: i64::from(number),
            snippet: "Snippet".into(),
            is_read: false,
            is_flagged: false,
            has_attachments: false,
        }
    }

    fn provider(count: u32, role: FolderRole) -> FakeProvider {
        FakeProvider::new(
            vec![remote_folder("inbox", role)],
            HashMap::from([("inbox".into(), (0..count).map(remote_message).collect())]),
        )
    }

    fn delta() -> FolderDelta {
        FolderDelta {
            upserts: vec![],
            deleted_remote_ids: vec![],
            flag_updates: vec![],
            server_remote_ids: None,
            new_state: FolderSyncState::Graph {
                delta_link: Some("done".into()),
                next_link: None,
            },
            more: false,
            reset: false,
        }
    }

    #[tokio::test]
    async fn syncs_all_pages_inbox_first() {
        let h = Harness::new();
        let provider = FakeProvider::new(
            vec![
                remote_folder("drafts", FolderRole::Drafts),
                remote_folder("sent", FolderRole::Sent),
                remote_folder("inbox", FolderRole::Inbox),
            ],
            HashMap::from([("inbox".into(), (0..450).map(remote_message).collect())]),
        );
        h.engine.sync_account(&h.account, &provider).await.unwrap();
        let inbox = h.folder("inbox");
        let messages = h.store.messages(&inbox.id, 0, 500).unwrap();
        assert_eq!(messages.len(), 450);
        assert_eq!(inbox.total_count, 450);
        assert_eq!(inbox.unread_count, 450);
        assert_eq!(messages[0].remote_id, "449");
        let events = h.events.0.lock().unwrap();
        let changed: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                CoreEvent::MessagesChanged { folder_id } => Some(folder_id.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            changed,
            vec![
                inbox.id.clone(),
                inbox.id.clone(),
                inbox.id.clone(),
                h.folder("sent").id,
                h.folder("drafts").id
            ]
        );
        assert!(matches!(events[1], CoreEvent::FoldersChanged { .. }));
        drop(events);
        assert_eq!(
            h.statuses(),
            vec![
                SyncStatus::Syncing { done: 0 },
                SyncStatus::Syncing { done: 200 },
                SyncStatus::Syncing { done: 400 },
                SyncStatus::Syncing { done: 450 },
                SyncStatus::Syncing { done: 450 },
                SyncStatus::Syncing { done: 450 },
                SyncStatus::Idle
            ]
        );
        let cached = messages
            .iter()
            .filter(|message| h.bodies.get(&h.account.id, &message.id).unwrap().is_some())
            .count();
        assert_eq!(cached, 200);
        assert!(
            h.bodies
                .get(&h.account.id, &messages[199].id)
                .unwrap()
                .is_some()
        );
        assert!(
            h.bodies
                .get(&h.account.id, &messages[200].id)
                .unwrap()
                .is_none()
        );
        // list + 5 pages + one batched prefetch of the newest 200.
        assert_eq!(provider.provider_calls(), 7);
    }

    #[tokio::test]
    async fn resume_after_failure() {
        let h = Harness::new();
        let provider = provider(450, FolderRole::Inbox);
        provider.set_fail_after_pages(Some(1));
        assert!(h.engine.sync_account(&h.account, &provider).await.is_err());
        let inbox = h.folder("inbox");
        let before = h.store.messages(&inbox.id, 0, 500).unwrap();
        assert_eq!(before.len(), 200);
        assert!(h.store.folder_sync_state(&inbox.id).unwrap().is_some());
        assert!(matches!(
            h.statuses().last(),
            Some(SyncStatus::Error { .. })
        ));
        // list + the successful page + the failed page, then one batched prefetch.
        assert_eq!(provider.provider_calls(), 4);
        provider.set_fail_after_pages(None);
        h.engine.sync_account(&h.account, &provider).await.unwrap();
        let after = h.store.messages(&inbox.id, 0, 500).unwrap();
        assert_eq!(after.len(), 450);
        for original in before {
            assert_eq!(
                h.store
                    .message_id_for_remote(&inbox.id, &original.remote_id)
                    .unwrap(),
                Some(original.id)
            );
        }
        assert_eq!(h.statuses().last(), Some(&SyncStatus::Idle));
        // A resumed pass takes two pages, not three. The newest 200 are already cached.
        assert_eq!(provider.provider_calls(), 7);
    }

    #[tokio::test]
    async fn uidvalidity_reset_clears_cache() {
        let h = Harness::new();
        let provider = provider(450, FolderRole::Inbox);
        h.engine.sync_account(&h.account, &provider).await.unwrap();
        let inbox = h.folder("inbox");
        let old = h.store.messages(&inbox.id, 0, 500).unwrap();
        provider.set_messages("inbox", (1000..1010).map(remote_message).collect());
        provider.set_reset_next(true);
        h.engine.sync_account(&h.account, &provider).await.unwrap();
        assert_eq!(h.folder("inbox").id, inbox.id);
        let rows = h.store.messages(&inbox.id, 0, 500).unwrap();
        assert_eq!(rows.len(), 10);
        assert!(rows.iter().all(|row| row.date_ms >= 1000));
        for message in old {
            assert!(h.bodies.get(&h.account.id, &message.id).unwrap().is_none());
        }
        assert_eq!(h.folder("inbox").total_count, 10);
    }

    #[tokio::test]
    async fn deletions_applied() {
        let h = Harness::new();
        let provider = provider(3, FolderRole::Inbox);
        h.engine.sync_account(&h.account, &provider).await.unwrap();
        let inbox = h.folder("inbox");
        let removed = h
            .store
            .message_id_for_remote(&inbox.id, "1")
            .unwrap()
            .unwrap();
        let mut changes = delta();
        changes.deleted_remote_ids = vec!["1".into(), "missing".into()];
        provider.queue_delta("inbox", changes);
        h.engine.sync_account(&h.account, &provider).await.unwrap();
        assert_eq!(h.folder("inbox").total_count, 2);
        assert!(h.store.message(&removed).unwrap().is_none());
        assert!(h.bodies.get(&h.account.id, &removed).unwrap().is_none());
    }

    #[tokio::test]
    async fn needs_sign_in_stops_without_retry() {
        let h = Harness::new();
        let provider = provider(3, FolderRole::Inbox);
        provider.set_auth_error(true);
        let error = h
            .engine
            .sync_account(&h.account, &provider)
            .await
            .unwrap_err();
        assert!(matches!(error, CoreError::NeedsSignIn { .. }));
        assert_eq!(h.statuses().last(), Some(&SyncStatus::NeedsSignIn));
        assert_eq!(provider.provider_calls(), 1);
        assert!(h.store.folders(&h.account.id).unwrap().is_empty());
    }

    #[tokio::test]
    async fn body_cached_after_first_fetch() {
        let h = Harness::new();
        let provider = provider(1, FolderRole::Other);
        h.engine.sync_account(&h.account, &provider).await.unwrap();
        let message = h
            .store
            .messages(&h.folder("inbox").id, 0, 1)
            .unwrap()
            .remove(0);
        let before = provider.provider_calls();
        let first = h
            .engine
            .body(&h.account, &provider, &message.id)
            .await
            .unwrap();
        assert_eq!(provider.provider_calls(), before + 1);
        assert!(!first.is_empty());
        let second = h
            .engine
            .body(&h.account, &provider, &message.id)
            .await
            .unwrap();
        assert_eq!(first, second);
        assert_eq!(provider.provider_calls(), before + 1);
    }

    #[tokio::test]
    async fn flag_and_retain_changes_applied() {
        let h = Harness::new();
        let provider = provider(3, FolderRole::Inbox);
        h.engine.sync_account(&h.account, &provider).await.unwrap();
        let inbox = h.folder("inbox");
        let kept = h
            .store
            .message_id_for_remote(&inbox.id, "1")
            .unwrap()
            .unwrap();
        let removed = h
            .store
            .message_id_for_remote(&inbox.id, "0")
            .unwrap()
            .unwrap();
        let mut changes = delta();
        changes.flag_updates = vec![FlagUpdate {
            remote_id: "1".into(),
            is_read: true,
            is_flagged: true,
        }];
        changes.server_remote_ids = Some(vec!["1".into(), "2".into()]);
        provider.queue_delta("inbox", changes);
        h.engine.sync_account(&h.account, &provider).await.unwrap();
        let message = h.store.message(&kept).unwrap().unwrap();
        assert!(message.is_read && message.is_flagged);
        assert!(h.store.message(&removed).unwrap().is_none());
        assert!(h.bodies.get(&h.account.id, &removed).unwrap().is_none());
        let folder = h.folder("inbox");
        assert_eq!((folder.total_count, folder.unread_count), (2, 1));
    }

    #[tokio::test]
    async fn resolves_parents_and_keeps_ids_on_repeated_sync() {
        let h = Harness::new();
        let mut child = remote_folder("inbox", FolderRole::Inbox);
        child.parent_remote_id = Some("parent".into());
        let provider = FakeProvider::new(
            vec![child, remote_folder("parent", FolderRole::Other)],
            HashMap::from([("inbox".into(), vec![remote_message(1)])]),
        );
        h.engine.sync_account(&h.account, &provider).await.unwrap();
        let inbox = h.folder("inbox");
        let id = h.store.messages(&inbox.id, 0, 1).unwrap().remove(0).id;
        assert_eq!(inbox.parent_id, Some(h.folder("parent").id));
        let before = provider.provider_calls();
        h.engine.sync_account(&h.account, &provider).await.unwrap();
        assert_eq!(h.folder("inbox").id, inbox.id);
        assert_eq!(h.store.messages(&inbox.id, 0, 1).unwrap()[0].id, id);
        assert!(h.bodies.get(&h.account.id, &id).unwrap().is_some());
        assert_eq!(provider.provider_calls(), before + 3);
    }

    #[tokio::test]
    async fn missing_parent_becomes_top_level() {
        let h = Harness::new();
        let mut child = remote_folder("[Gmail]/Sent Mail", FolderRole::Sent);
        child.parent_remote_id = Some("[Gmail]".into());
        let provider = FakeProvider::new(
            vec![child],
            HashMap::from([("[Gmail]/Sent Mail".into(), vec![remote_message(1)])]),
        );

        h.engine.sync_account(&h.account, &provider).await.unwrap();

        let folder = h.folder("[Gmail]/Sent Mail");
        assert_eq!(folder.parent_id, None);
        assert_eq!(folder.total_count, 1);
        assert_eq!(h.statuses().last(), Some(&SyncStatus::Idle));
    }

    #[tokio::test]
    async fn prefetch_uses_one_batch() {
        let h = Harness::new();
        let provider = provider(200, FolderRole::Inbox);
        h.engine.sync_account(&h.account, &provider).await.unwrap();
        assert_eq!(provider.fetch_raw_many_calls(), 1);
        assert_eq!(provider.fetch_raw_calls(), 0);
        let rows = h.store.messages(&h.folder("inbox").id, 0, 200).unwrap();
        assert_eq!(rows.len(), 200);
        for row in &rows {
            assert!(
                h.bodies.get(&h.account.id, &row.id).unwrap().is_some(),
                "missing body for {}",
                row.remote_id
            );
        }
    }

    #[tokio::test]
    async fn prefetch_stops_on_needs_sign_in() {
        let h = Harness::new();
        let provider = provider(3, FolderRole::Inbox);
        provider.set_auth_error_on_fetch(true);

        let result = h.engine.sync_account(&h.account, &provider).await;

        // One list call, one page, and one batched prefetch that needs sign-in.
        assert_eq!(provider.provider_calls(), 3);
        assert!(matches!(result, Err(CoreError::NeedsSignIn { .. })));
        assert_eq!(
            h.statuses(),
            vec![
                SyncStatus::Syncing { done: 0 },
                SyncStatus::Syncing { done: 3 },
                SyncStatus::NeedsSignIn,
            ]
        );
        let rows = h.store.messages(&h.folder("inbox").id, 0, 10).unwrap();
        assert_eq!(rows.len(), 3);
        for row in rows {
            assert!(h.bodies.get(&h.account.id, &row.id).unwrap().is_none());
        }
    }

    #[tokio::test]
    async fn folder_failure_syncs_the_rest_and_prefetches() {
        let h = Harness::new();
        let provider = FakeProvider::new(
            vec![
                remote_folder("drafts", FolderRole::Drafts),
                remote_folder("sent", FolderRole::Sent),
                remote_folder("inbox", FolderRole::Inbox),
            ],
            HashMap::from([
                ("inbox".into(), vec![remote_message(1)]),
                ("sent".into(), vec![remote_message(2)]),
                ("drafts".into(), vec![remote_message(3)]),
            ]),
        );
        provider.set_fail_sync_folder(Some("sent"));

        let error = h
            .engine
            .sync_account(&h.account, &provider)
            .await
            .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("sent"), "{message}");
        assert!(!message.contains("drafts"), "{message}");
        assert!(matches!(
            h.statuses().last(),
            Some(SyncStatus::Error { message }) if message.contains("sent")
        ));

        let inbox_rows = h.store.messages(&h.folder("inbox").id, 0, 10).unwrap();
        assert_eq!(inbox_rows.len(), 1);
        assert!(
            h.bodies
                .get(&h.account.id, &inbox_rows[0].id)
                .unwrap()
                .is_some(),
            "inbox prefetch did not run"
        );
        assert!(
            h.store
                .messages(&h.folder("sent").id, 0, 10)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            h.store
                .messages(&h.folder("drafts").id, 0, 10)
                .unwrap()
                .len(),
            1
        );
        // list + inbox + failed sent + drafts + one body fetch.
        assert_eq!(provider.provider_calls(), 5);
    }

    #[tokio::test]
    async fn needs_sign_in_on_a_folder_stops_immediately() {
        let h = Harness::new();
        let provider = FakeProvider::new(
            vec![
                remote_folder("inbox", FolderRole::Inbox),
                remote_folder("sent", FolderRole::Sent),
                remote_folder("drafts", FolderRole::Drafts),
            ],
            HashMap::from([
                ("inbox".into(), vec![remote_message(1)]),
                ("sent".into(), vec![remote_message(2)]),
                ("drafts".into(), vec![remote_message(3)]),
            ]),
        );
        provider.set_needs_sign_in_folder(Some("sent"));

        let error = h
            .engine
            .sync_account(&h.account, &provider)
            .await
            .unwrap_err();
        assert!(matches!(error, CoreError::NeedsSignIn { .. }));
        assert_eq!(h.statuses().last(), Some(&SyncStatus::NeedsSignIn));
        let inbox_rows = h.store.messages(&h.folder("inbox").id, 0, 10).unwrap();
        assert_eq!(inbox_rows.len(), 1);
        assert!(
            h.bodies
                .get(&h.account.id, &inbox_rows[0].id)
                .unwrap()
                .is_none()
        );
        assert_eq!(h.folder("drafts").total_count, 0);
        // list + inbox page + the NeedsSignIn folder. Drafts and prefetch do not run.
        assert_eq!(provider.provider_calls(), 3);
    }

    #[tokio::test]
    async fn provider_call_timeout_maps_to_network() {
        let started = std::time::Instant::now();
        let error = super::provider_call_with_timeout(
            std::time::Duration::from_millis(20),
            std::future::pending::<crate::Result<()>>(),
        )
        .await
        .unwrap_err();
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "timed out in {:?}",
            started.elapsed()
        );
        assert!(
            matches!(error, CoreError::Network(ref message) if message.contains("did not respond")),
            "{error}"
        );
    }

    #[tokio::test]
    async fn prefetch_failure_is_skipped() {
        let h = Harness::new();
        let provider = provider(3, FolderRole::Inbox);
        provider.set_fetch_error("inbox", "2", true);
        h.engine.sync_account(&h.account, &provider).await.unwrap();
        let inbox = h.folder("inbox");
        let rows = h.store.messages(&inbox.id, 0, 10).unwrap();
        assert_eq!(rows.len(), 3);
        for row in rows {
            assert_eq!(
                h.bodies.get(&h.account.id, &row.id).unwrap().is_some(),
                row.remote_id != "2"
            );
        }
        assert_eq!(h.statuses().last(), Some(&SyncStatus::Idle));
        // list + one page + one batched prefetch.
        assert_eq!(provider.provider_calls(), 3);
    }

    #[tokio::test]
    async fn body_rejects_other_account_before_provider_call() {
        let h = Harness::new();
        let provider = provider(1, FolderRole::Inbox);
        h.engine.sync_account(&h.account, &provider).await.unwrap();
        let message = h
            .store
            .messages(&h.folder("inbox").id, 0, 1)
            .unwrap()
            .remove(0);
        let mut other = h.account.clone();
        other.id = AccountId::new();
        let before = provider.provider_calls();
        assert!(h.engine.body(&other, &provider, &message.id).await.is_err());
        assert_eq!(provider.provider_calls(), before);
    }
}
