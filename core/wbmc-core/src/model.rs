// This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0. If a copy of the MPL was not distributed with this file, You can obtain one at https://mozilla.org/MPL/2.0/.

use serde::{Deserialize, Serialize};

use crate::{CoreError, Result};

macro_rules! id_newtype {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            // Each call mints a new id, so this is not `Default` (that implies a stable value).
            #[allow(clippy::new_without_default)]
            pub fn new() -> Self {
                Self(uuid::Uuid::now_v7().to_string())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }
    };
}

id_newtype!(AccountId);
id_newtype!(FolderId);
id_newtype!(MessageId);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    Imap,
    Microsoft,
}

impl ProviderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Imap => "imap",
            Self::Microsoft => "microsoft",
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "imap" => Ok(Self::Imap),
            "microsoft" => Ok(Self::Microsoft),
            other => Err(CoreError::InvalidInput(format!(
                "unknown provider kind: {other}"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FolderRole {
    Inbox,
    Sent,
    Drafts,
    Trash,
    Archive,
    Junk,
    Other,
}

impl FolderRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Inbox => "inbox",
            Self::Sent => "sent",
            Self::Drafts => "drafts",
            Self::Trash => "trash",
            Self::Archive => "archive",
            Self::Junk => "junk",
            Self::Other => "other",
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "inbox" => Ok(Self::Inbox),
            "sent" => Ok(Self::Sent),
            "drafts" => Ok(Self::Drafts),
            "trash" => Ok(Self::Trash),
            "archive" => Ok(Self::Archive),
            "junk" => Ok(Self::Junk),
            "other" => Ok(Self::Other),
            other => Err(CoreError::InvalidInput(format!(
                "unknown folder role: {other}"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Address {
    pub name: Option<String>,
    pub email: String,
}

/// `config` is a JSON value, which is not [`Eq`] or [`Hash`], so this struct
/// derives [`PartialEq`] only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Account {
    pub id: AccountId,
    pub kind: ProviderKind,
    pub email: String,
    pub display_name: String,
    pub config: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Folder {
    pub id: FolderId,
    pub account_id: AccountId,
    pub remote_id: String,
    pub name: String,
    pub role: FolderRole,
    pub parent_id: Option<FolderId>,
    pub unread_count: u32,
    pub total_count: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MessageSummary {
    pub id: MessageId,
    pub account_id: AccountId,
    pub folder_id: FolderId,
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
