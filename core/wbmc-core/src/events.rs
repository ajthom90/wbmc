// This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0. If a copy of the MPL was not distributed with this file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::model::{AccountId, FolderId};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoreEvent {
    AccountsChanged,
    FoldersChanged {
        account_id: AccountId,
    },
    MessagesChanged {
        folder_id: FolderId,
    },
    SyncStatus {
        account_id: AccountId,
        status: SyncStatus,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncStatus {
    Idle,
    Syncing { done: u32 },
    Error { message: String },
    NeedsSignIn,
}

/// Frontends receive events only after the corresponding store changes commit.
pub trait EventSink: Send + Sync {
    fn emit(&self, e: CoreEvent);
}
