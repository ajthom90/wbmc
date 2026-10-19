// This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0. If a copy of the MPL was not distributed with this file, You can obtain one at https://mozilla.org/MPL/2.0/.

#[cfg(any(test, feature = "test-util"))]
pub mod fake;

pub mod graph;
mod imap;

pub use graph::GraphProvider;
pub use imap::{ImapConfig, ImapProvider, Security};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::Result;
use crate::model::{Address, FolderRole};

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
/// Stored as JSON in `folders.sync_state`. `kind` is `Imap` or `Graph`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum FolderSyncState {
    Imap {
        uid_validity: u32,
        highest_uid: u32,
        lowest_uid: u32,
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
/// by providers that name removals themselves (Graph). IMAP leaves it empty and
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
}
