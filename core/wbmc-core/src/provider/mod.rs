// This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0. If a copy of the MPL was not distributed with this file, You can obtain one at https://mozilla.org/MPL/2.0/.

#[cfg(any(test, feature = "test-util"))]
pub mod fake;

pub mod gmail;
pub mod graph;
mod imap;

pub use gmail::GmailProvider;
pub use graph::GraphProvider;
pub use imap::{ImapConfig, ImapProvider, Security};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::model::{Address, FolderRole};
use crate::{CoreError, Result};

/// A mailbox on the remote account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteFolder {
    /// Full IMAP mailbox name, or the provider's own folder id.
    pub remote_id: String,
    pub name: String,
    pub role: FolderRole,
    pub parent_remote_id: Option<String>,
}

/// One message summary returned by a sync page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteMessage {
    /// IMAP UID as a decimal string, or the provider's own message id.
    pub remote_id: String,
    pub subject: String,
    pub from: Address,
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    pub date_ms: i64,
    pub snippet: String,
    pub is_read: bool,
    pub is_flagged: bool,
    pub has_attachments: bool,
}

/// Cursor for the next [`MailProvider::sync_folder`] call.
///
/// Stored as JSON in `folders.sync_state`. `kind` is `Imap`, `Graph`, or `Gmail`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum FolderSyncState {
    Imap {
        uid_validity: u32,
        highest_uid: u32,
        lowest_uid: u32,
    },
    Gmail {
        history_id: Option<String>,
        page_token: Option<String>,
        backfilled: bool,
    },
    Graph {
        delta_link: Option<String>,
        next_link: Option<String>,
    },
}

/// Read and flagged state for a message the client already has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlagUpdate {
    pub remote_id: String,
    pub is_read: bool,
    pub is_flagged: bool,
}

/// One page of remote changes.
///
/// `reset` means the local cache for the folder must be cleared before applying
/// `upserts`. `more` means another page remains. `deleted_remote_ids` is filled
/// by providers that name removals themselves (Graph and Gmail). IMAP leaves it empty and
/// instead sets `server_remote_ids` to every UID still on the server, on the
/// final page of an incremental pass. `flag_updates` is filled on that same page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderDelta {
    pub upserts: Vec<RemoteMessage>,
    pub deleted_remote_ids: Vec<String>,
    pub flag_updates: Vec<FlagUpdate>,
    pub server_remote_ids: Option<Vec<String>>,
    pub new_state: FolderSyncState,
    pub more: bool,
    pub reset: bool,
}

#[async_trait]
pub trait MailProvider: Send + Sync {
    async fn list_folders(&self) -> Result<Vec<RemoteFolder>>;

    async fn sync_folder(
        &self,
        folder: &RemoteFolder,
        state: Option<FolderSyncState>,
        page_size: u32,
    ) -> Result<FolderDelta>;

    async fn fetch_raw(&self, folder_remote_id: &str, message_remote_id: &str) -> Result<Vec<u8>>;

    /// Fetches raw RFC 5322 bytes for many messages.
    ///
    /// Results are in `message_remote_ids` order. The default calls
    /// [`fetch_raw`](Self::fetch_raw) once per id and stops at the first
    /// [`CoreError::NeedsSignIn`], repeating that error for the rest.
    /// Providers that can batch (IMAP) override this.
    async fn fetch_raw_many(
        &self,
        folder_remote_id: &str,
        message_remote_ids: &[String],
    ) -> Vec<(String, Result<Vec<u8>>)> {
        let mut results = Vec::with_capacity(message_remote_ids.len());
        for (index, id) in message_remote_ids.iter().enumerate() {
            let result = self.fetch_raw(folder_remote_id, id).await;
            let signed_out = matches!(result, Err(CoreError::NeedsSignIn { .. }));
            results.push((id.clone(), result));
            if signed_out {
                let error = match &results[index].1 {
                    Err(error) => error.clone(),
                    Ok(_) => unreachable!("NeedsSignIn branch"),
                };
                for rest in &message_remote_ids[index + 1..] {
                    results.push((rest.clone(), Err(error.clone())));
                }
                break;
            }
        }
        results
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;

    use super::{FolderSyncState, MailProvider, RemoteFolder};
    use crate::{CoreError, Result};

    /// Uses the trait default for `fetch_raw_many`.
    struct SignInOnFetch {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl MailProvider for SignInOnFetch {
        async fn list_folders(&self) -> Result<Vec<RemoteFolder>> {
            Ok(Vec::new())
        }

        async fn sync_folder(
            &self,
            _folder: &RemoteFolder,
            _state: Option<FolderSyncState>,
            _page_size: u32,
        ) -> Result<super::FolderDelta> {
            Err(CoreError::Protocol("unused".into()))
        }

        async fn fetch_raw(
            &self,
            _folder_remote_id: &str,
            _message_remote_id: &str,
        ) -> Result<Vec<u8>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Err(CoreError::NeedsSignIn {
                account_id: "acct".into(),
            })
        }
    }

    #[tokio::test]
    async fn fetch_raw_many_stops_after_needs_sign_in() {
        let provider = SignInOnFetch {
            calls: AtomicUsize::new(0),
        };
        let ids = vec!["1".into(), "2".into(), "3".into()];
        let results = provider.fetch_raw_many("inbox", &ids).await;
        assert_eq!(provider.calls.load(Ordering::Relaxed), 1);
        assert_eq!(results.len(), 3);
        for (_id, result) in results {
            match result {
                Err(CoreError::NeedsSignIn { account_id }) => assert_eq!(account_id, "acct"),
                other => panic!("expected NeedsSignIn, got {other:?}"),
            }
        }
    }
}
