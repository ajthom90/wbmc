// This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0. If a copy of the MPL was not distributed with this file, You can obtain one at https://mozilla.org/MPL/2.0/.

mod bodies;

use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, params};
use serde::de::DeserializeOwned;

use crate::model::{
    Account, AccountId, Folder, FolderId, FolderRole, MessageId, MessageSummary, ProviderKind,
};
use crate::{CoreError, Result};

pub use bodies::BodyStore;

const SCHEMA_SQL: &str = include_str!("schema.sql");
const SCHEMA_VERSION: i64 = 1;

const ACCOUNT_COLUMNS: &str = "id, kind, email, display_name, config";
const FOLDER_COLUMNS: &str =
    "id, account_id, remote_id, name, role, parent_id, unread_count, total_count";
const MESSAGE_COLUMNS: &str = "\
id, account_id, folder_id, remote_id, subject, from_json, to_json, cc_json, \
date_ms, snippet, is_read, is_flagged, has_attachments";

pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let journal: String =
            conn.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))?;
        if !journal.eq_ignore_ascii_case("wal") {
            return Err(CoreError::Storage(format!(
                "expected WAL journal mode, got {journal}"
            )));
        }
        migrate(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn insert_account(&self, account: &Account) -> Result<()> {
        let conn = self.lock()?;
        let config = json_text(&account.config)?;
        conn.execute(
            "INSERT INTO accounts (id, kind, email, display_name, config) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                account.id.as_str(),
                account.kind.as_str(),
                account.email,
                account.display_name,
                config,
            ],
        )?;
        Ok(())
    }

    pub fn accounts(&self) -> Result<Vec<Account>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {ACCOUNT_COLUMNS} FROM accounts ORDER BY email, id"
        ))?;
        let rows = stmt.query_map([], map_account)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn delete_account(&self, account_id: &AccountId) -> Result<()> {
        let conn = self.lock()?;
        conn.execute("DELETE FROM accounts WHERE id = ?1", [account_id.as_str()])?;
        Ok(())
    }

    pub fn upsert_folder(&self, folder: &Folder) -> Result<Folder> {
        let conn = self.lock()?;
        let parent_id = folder.parent_id.as_ref().map(FolderId::as_str);
        // Conflict keeps the existing id, counts, and sync_state. `recount` owns counts.
        conn.query_row(
            &format!(
                "INSERT INTO folders ({FOLDER_COLUMNS})
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(account_id, remote_id) DO UPDATE SET
                    name = excluded.name,
                    role = excluded.role,
                    parent_id = excluded.parent_id
                 RETURNING {FOLDER_COLUMNS}"
            ),
            params![
                folder.id.as_str(),
                folder.account_id.as_str(),
                folder.remote_id,
                folder.name,
                folder.role.as_str(),
                parent_id,
                folder.unread_count,
                folder.total_count,
            ],
            map_folder,
        )
        .map_err(Into::into)
    }

    pub fn folders(&self, account_id: &AccountId) -> Result<Vec<Folder>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {FOLDER_COLUMNS} FROM folders WHERE account_id = ?1 ORDER BY name, id"
        ))?;
        let rows = stmt.query_map([account_id.as_str()], map_folder)?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn folder_sync_state(&self, folder_id: &FolderId) -> Result<Option<String>> {
        let conn = self.lock()?;
        match conn.query_row(
            "SELECT sync_state FROM folders WHERE id = ?1",
            [folder_id.as_str()],
            |row| row.get(0),
        ) {
            Ok(state) => Ok(state),
            Err(rusqlite::Error::QueryReturnedNoRows) => Err(CoreError::NotFound(format!(
                "folder {}",
                folder_id.as_str()
            ))),
            Err(err) => Err(err.into()),
        }
    }

    pub fn set_folder_sync_state(&self, folder_id: &FolderId, state: &str) -> Result<()> {
        let conn = self.lock()?;
        let changed = conn.execute(
            "UPDATE folders SET sync_state = ?2 WHERE id = ?1",
            params![folder_id.as_str(), state],
        )?;
        if changed == 0 {
            return Err(CoreError::NotFound(format!(
                "folder {}",
                folder_id.as_str()
            )));
        }
        Ok(())
    }

    pub fn upsert_messages(&self, messages: &[MessageSummary]) -> Result<()> {
        if messages.is_empty() {
            return Ok(());
        }
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO messages (
                    id, account_id, folder_id, remote_id, subject,
                    from_json, to_json, cc_json, date_ms, snippet,
                    is_read, is_flagged, has_attachments
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
                 ON CONFLICT(folder_id, remote_id) DO UPDATE SET
                    -- `id` stays put: body files and UI selection key on MessageId.
                    account_id = excluded.account_id,
                    subject = excluded.subject,
                    from_json = excluded.from_json,
                    to_json = excluded.to_json,
                    cc_json = excluded.cc_json,
                    date_ms = excluded.date_ms,
                    snippet = excluded.snippet,
                    is_read = excluded.is_read,
                    is_flagged = excluded.is_flagged,
                    has_attachments = excluded.has_attachments",
            )?;
            for message in messages {
                let from_json = json_text(&message.from)?;
                let to_json = json_text(&message.to)?;
                let cc_json = json_text(&message.cc)?;
                stmt.execute(params![
                    message.id.as_str(),
                    message.account_id.as_str(),
                    message.folder_id.as_str(),
                    message.remote_id,
                    message.subject,
                    from_json,
                    to_json,
                    cc_json,
                    message.date_ms,
                    message.snippet,
                    message.is_read,
                    message.is_flagged,
                    message.has_attachments,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn delete_messages(&self, folder_id: &FolderId, remote_ids: &[String]) -> Result<()> {
        if remote_ids.is_empty() {
            return Ok(());
        }
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        {
            let mut stmt =
                tx.prepare("DELETE FROM messages WHERE folder_id = ?1 AND remote_id = ?2")?;
            for remote_id in remote_ids {
                stmt.execute(params![folder_id.as_str(), remote_id])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn clear_folder(&self, folder_id: &FolderId) -> Result<()> {
        let conn = self.lock()?;
        conn.execute(
            "DELETE FROM messages WHERE folder_id = ?1",
            [folder_id.as_str()],
        )?;
        Ok(())
    }

    pub fn messages(
        &self,
        folder_id: &FolderId,
        offset: u32,
        limit: u32,
    ) -> Result<Vec<MessageSummary>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {MESSAGE_COLUMNS} FROM messages
             WHERE folder_id = ?1
             ORDER BY date_ms DESC, id DESC
             LIMIT ?2 OFFSET ?3"
        ))?;
        let rows = stmt.query_map(
            params![folder_id.as_str(), i64::from(limit), i64::from(offset)],
            map_message,
        )?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn message(&self, message_id: &MessageId) -> Result<Option<MessageSummary>> {
        let conn = self.lock()?;
        conn.query_row(
            &format!("SELECT {MESSAGE_COLUMNS} FROM messages WHERE id = ?1"),
            [message_id.as_str()],
            map_message,
        )
        .optional()
        .map_err(Into::into)
    }

    pub fn recount(&self, folder_id: &FolderId) -> Result<()> {
        let conn = self.lock()?;
        let changed = conn.execute(
            "UPDATE folders SET
                total_count = (SELECT COUNT(*) FROM messages WHERE folder_id = ?1),
                unread_count = (
                    SELECT COUNT(*) FROM messages WHERE folder_id = ?1 AND is_read = 0
                )
             WHERE id = ?1",
            [folder_id.as_str()],
        )?;
        if changed == 0 {
            return Err(CoreError::NotFound(format!(
                "folder {}",
                folder_id.as_str()
            )));
        }
        Ok(())
    }

    fn lock(&self) -> Result<MutexGuard<'_, Connection>> {
        self.conn
            .lock()
            .map_err(|err| CoreError::Storage(format!("store lock poisoned: {err}")))
    }
}

fn migrate(conn: &Connection) -> Result<()> {
    let has_version: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'schema_version'",
        [],
        |row| row.get(0),
    )?;
    if has_version == 0 {
        let tx = conn.unchecked_transaction()?;
        tx.execute_batch(SCHEMA_SQL)?;
        tx.execute(
            "INSERT INTO schema_version (version) VALUES (?1)",
            [SCHEMA_VERSION],
        )?;
        tx.commit()?;
        return Ok(());
    }

    let version: i64 = conn.query_row(
        "SELECT COALESCE(MAX(version), 0) FROM schema_version",
        [],
        |row| row.get(0),
    )?;
    if version != SCHEMA_VERSION {
        return Err(CoreError::Storage(format!(
            "unsupported schema version {version}"
        )));
    }
    Ok(())
}

fn json_text<T: serde::Serialize>(value: &T) -> Result<String> {
    serde_json::to_string(value).map_err(|err| CoreError::Storage(err.to_string()))
}

fn json_column<T: DeserializeOwned>(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<T> {
    let text: String = row.get(index)?;
    serde_json::from_str(&text).map_err(|err| {
        rusqlite::Error::FromSqlConversionFailure(index, rusqlite::types::Type::Text, Box::new(err))
    })
}

fn kind_column(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<ProviderKind> {
    let text: String = row.get(index)?;
    ProviderKind::parse(&text).map_err(|err| {
        rusqlite::Error::FromSqlConversionFailure(index, rusqlite::types::Type::Text, Box::new(err))
    })
}

fn role_column(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<FolderRole> {
    let text: String = row.get(index)?;
    FolderRole::parse(&text).map_err(|err| {
        rusqlite::Error::FromSqlConversionFailure(index, rusqlite::types::Type::Text, Box::new(err))
    })
}

fn map_account(row: &rusqlite::Row<'_>) -> rusqlite::Result<Account> {
    Ok(Account {
        id: AccountId::from(row.get::<_, String>(0)?),
        kind: kind_column(row, 1)?,
        email: row.get(2)?,
        display_name: row.get(3)?,
        config: json_column(row, 4)?,
    })
}

fn map_folder(row: &rusqlite::Row<'_>) -> rusqlite::Result<Folder> {
    let parent: Option<String> = row.get(5)?;
    Ok(Folder {
        id: FolderId::from(row.get::<_, String>(0)?),
        account_id: AccountId::from(row.get::<_, String>(1)?),
        remote_id: row.get(2)?,
        name: row.get(3)?,
        role: role_column(row, 4)?,
        parent_id: parent.map(FolderId::from),
        unread_count: row.get(6)?,
        total_count: row.get(7)?,
    })
}

fn map_message(row: &rusqlite::Row<'_>) -> rusqlite::Result<MessageSummary> {
    Ok(MessageSummary {
        id: MessageId::from(row.get::<_, String>(0)?),
        account_id: AccountId::from(row.get::<_, String>(1)?),
        folder_id: FolderId::from(row.get::<_, String>(2)?),
        remote_id: row.get(3)?,
        subject: row.get(4)?,
        from: json_column(row, 5)?,
        to: json_column(row, 6)?,
        cc: json_column(row, 7)?,
        date_ms: row.get(8)?,
        snippet: row.get(9)?,
        is_read: row.get(10)?,
        is_flagged: row.get(11)?,
        has_attachments: row.get(12)?,
    })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use super::{BodyStore, Store};
    use crate::model::{
        Account, AccountId, Address, Folder, FolderId, FolderRole, MessageId, MessageSummary,
        ProviderKind,
    };

    fn temp_store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("wbmc.db")).unwrap();
        (dir, store)
    }

    fn account() -> Account {
        Account {
            id: AccountId::new(),
            kind: ProviderKind::Imap,
            email: "ada@example.com".into(),
            display_name: "Ada".into(),
            config: serde_json::json!({"host": "imap.example.com"}),
        }
    }

    fn folder(account_id: &AccountId, remote_id: &str) -> Folder {
        Folder {
            id: FolderId::new(),
            account_id: account_id.clone(),
            remote_id: remote_id.into(),
            name: "Inbox".into(),
            role: FolderRole::Inbox,
            parent_id: None,
            unread_count: 0,
            total_count: 0,
        }
    }

    fn message(
        account_id: &AccountId,
        folder_id: &FolderId,
        remote_id: &str,
        date_ms: i64,
        is_read: bool,
    ) -> MessageSummary {
        MessageSummary {
            id: MessageId::new(),
            account_id: account_id.clone(),
            folder_id: folder_id.clone(),
            remote_id: remote_id.into(),
            subject: format!("Subject {remote_id}"),
            from: Address {
                name: Some("Ada".into()),
                email: "ada@example.com".into(),
            },
            to: vec![Address {
                name: None,
                email: "me@example.com".into(),
            }],
            cc: vec![],
            date_ms,
            snippet: "snippet".into(),
            is_read,
            is_flagged: false,
            has_attachments: false,
        }
    }

    fn assert_no_tmp(root: &Path) {
        fn walk(dir: &Path) {
            for entry in fs::read_dir(dir).unwrap() {
                let entry = entry.unwrap();
                let name = entry.file_name();
                let name = name.to_string_lossy();
                assert!(!name.contains(".tmp"), "temp file left behind: {name}");
                if entry.file_type().unwrap().is_dir() {
                    walk(&entry.path());
                }
            }
        }
        walk(root);
    }

    #[test]
    fn ids_are_uuid_v7_strings() {
        let id = MessageId::new();
        let parsed = uuid::Uuid::parse_str(id.as_str()).unwrap();
        assert_eq!(parsed.get_version_num(), 7);
        assert_eq!(MessageId::from(id.as_str().to_string()), id);
        assert_eq!(AccountId::new().as_str().len(), 36);
        assert_eq!(FolderId::from("folder-1".to_string()).as_str(), "folder-1");
    }

    #[test]
    fn upsert_messages_is_idempotent() {
        let (_dir, store) = temp_store();
        let account = account();
        store.insert_account(&account).unwrap();
        let folder = store.upsert_folder(&folder(&account.id, "INBOX")).unwrap();
        let first = message(&account.id, &folder.id, "r1", 1_000, false);
        store.upsert_messages(std::slice::from_ref(&first)).unwrap();

        let mut second = first;
        second.is_read = true;
        store.upsert_messages(&[second]).unwrap();

        let rows = store.messages(&folder.id, 0, 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].is_read);
    }

    #[test]
    fn upsert_messages_keeps_existing_id() {
        let (_dir, store) = temp_store();
        let account = account();
        store.insert_account(&account).unwrap();
        let folder = store.upsert_folder(&folder(&account.id, "INBOX")).unwrap();
        let first = message(&account.id, &folder.id, "r1", 1_000, false);
        let original_id = first.id.clone();
        store.upsert_messages(&[first]).unwrap();

        let second = message(&account.id, &folder.id, "r1", 2_000, true);
        let replacement_id = second.id.clone();
        assert_ne!(replacement_id, original_id);
        store.upsert_messages(&[second]).unwrap();

        let rows = store.messages(&folder.id, 0, 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, original_id);
        assert!(store.message(&original_id).unwrap().is_some());
        assert!(store.message(&replacement_id).unwrap().is_none());
    }

    #[test]
    fn messages_are_newest_first_and_paged() {
        let (_dir, store) = temp_store();
        let account = account();
        store.insert_account(&account).unwrap();
        let folder = store.upsert_folder(&folder(&account.id, "INBOX")).unwrap();
        let batch: Vec<_> = (0..250)
            .map(|i| message(&account.id, &folder.id, &format!("r{i}"), i, false))
            .collect();
        store.upsert_messages(&batch).unwrap();

        let page = store.messages(&folder.id, 0, 100).unwrap();
        assert_eq!(page.len(), 100);
        assert_eq!(page[0].date_ms, 249);
        assert!(
            page.windows(2)
                .all(|pair| pair[0].date_ms >= pair[1].date_ms)
        );

        let tail = store.messages(&folder.id, 200, 100).unwrap();
        assert_eq!(tail.len(), 50);
        assert_eq!(tail[0].date_ms, 49);
        assert_eq!(tail.last().unwrap().date_ms, 0);
    }

    #[test]
    fn delete_account_cascades() {
        let (_dir, store) = temp_store();
        let account = account();
        store.insert_account(&account).unwrap();
        let folder = store.upsert_folder(&folder(&account.id, "INBOX")).unwrap();
        let msg = message(&account.id, &folder.id, "r1", 1_000, false);
        let message_id = msg.id.clone();
        store.upsert_messages(&[msg]).unwrap();

        store.delete_account(&account.id).unwrap();

        assert!(store.accounts().unwrap().is_empty());
        assert!(store.folders(&account.id).unwrap().is_empty());
        assert!(store.messages(&folder.id, 0, 10).unwrap().is_empty());
        assert!(store.message(&message_id).unwrap().is_none());
    }

    #[test]
    fn upsert_folder_keeps_id() {
        let (_dir, store) = temp_store();
        let account = account();
        store.insert_account(&account).unwrap();
        let first = folder(&account.id, "INBOX");
        let stored = store.upsert_folder(&first).unwrap();
        assert_eq!(stored.id, first.id);

        let mut second = folder(&account.id, "INBOX");
        second.name = "Inbox renamed".into();
        second.role = FolderRole::Archive;
        assert_ne!(second.id, first.id);

        let again = store.upsert_folder(&second).unwrap();
        assert_eq!(again.id, first.id);
        assert_eq!(again.name, "Inbox renamed");
        assert_eq!(again.role, FolderRole::Archive);
    }

    #[test]
    fn upsert_folder_preserves_counts() {
        let (_dir, store) = temp_store();
        let account = account();
        store.insert_account(&account).unwrap();
        let folder = store.upsert_folder(&folder(&account.id, "INBOX")).unwrap();
        store
            .upsert_messages(&[
                message(&account.id, &folder.id, "a", 1, false),
                message(&account.id, &folder.id, "b", 2, true),
            ])
            .unwrap();
        store.recount(&folder.id).unwrap();

        let mut refreshed = folder.clone();
        refreshed.id = FolderId::new();
        refreshed.unread_count = 0;
        refreshed.total_count = 0;
        let again = store.upsert_folder(&refreshed).unwrap();
        assert_eq!(again.id, folder.id);
        assert_eq!(again.unread_count, 1);
        assert_eq!(again.total_count, 2);
    }

    #[test]
    fn body_store_roundtrip_and_atomic() {
        let dir = tempfile::tempdir().unwrap();
        let bodies = BodyStore::new(dir.path().to_path_buf());
        let account = AccountId::new();
        let message = MessageId::new();
        let bytes = b"From: ada@example.com\r\n\r\nhello";

        bodies.put(&account, &message, bytes).unwrap();
        assert_eq!(
            bodies.get(&account, &message).unwrap().as_deref(),
            Some(bytes.as_slice())
        );
        let path = dir
            .path()
            .join(account.as_str())
            .join(format!("{}.eml", message.as_str()));
        assert!(path.is_file());
        assert_no_tmp(dir.path());

        bodies.remove(&account, &message).unwrap();
        assert!(bodies.get(&account, &message).unwrap().is_none());
    }

    #[test]
    fn enums_roundtrip_as_lowercase_and_addresses_as_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wbmc.db");
        let account = account();
        let account_id = account.id.clone();
        let message_id;
        {
            let store = Store::open(&path).unwrap();
            store.insert_account(&account).unwrap();
            let stored = store.upsert_folder(&folder(&account.id, "INBOX")).unwrap();
            let msg = message(&account.id, &stored.id, "r1", 5, false);
            message_id = msg.id.clone();
            store.upsert_messages(&[msg]).unwrap();
            let loaded = store.message(&message_id).unwrap().unwrap();
            assert_eq!(loaded.from.email, "ada@example.com");
            assert_eq!(loaded.to[0].email, "me@example.com");
            assert_eq!(loaded.account_id, account_id);
        }

        let conn = rusqlite::Connection::open(&path).unwrap();
        let journal: String = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert_eq!(journal.to_lowercase(), "wal");

        let kind: String = conn
            .query_row("SELECT kind FROM accounts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(kind, "imap");
        let role: String = conn
            .query_row("SELECT role FROM folders", [], |row| row.get(0))
            .unwrap();
        assert_eq!(role, "inbox");
        let from_json: String = conn
            .query_row("SELECT from_json FROM messages", [], |row| row.get(0))
            .unwrap();
        assert!(from_json.starts_with('{'));
        assert!(from_json.contains("ada@example.com"));

        let message_sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'messages'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let message_sql_lower = message_sql.to_lowercase();
        assert!(message_sql_lower.contains("unique"));
        assert!(message_sql_lower.contains("folder_id"));
        assert!(message_sql_lower.contains("remote_id"));
        assert!(message_sql_lower.contains("on delete cascade"));

        let index_sql: String = conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'index' AND tbl_name = 'messages' AND sql IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let index_sql_lower = index_sql.to_lowercase();
        assert!(index_sql_lower.contains("folder_id"));
        assert!(index_sql_lower.contains("date_ms"));
    }

    #[test]
    fn recount_sets_total_and_unread() {
        let (_dir, store) = temp_store();
        let account = account();
        store.insert_account(&account).unwrap();
        let folder = store.upsert_folder(&folder(&account.id, "INBOX")).unwrap();
        store
            .upsert_messages(&[
                message(&account.id, &folder.id, "a", 1, false),
                message(&account.id, &folder.id, "b", 2, true),
                message(&account.id, &folder.id, "c", 3, false),
            ])
            .unwrap();
        store.recount(&folder.id).unwrap();

        let folders = store.folders(&account.id).unwrap();
        assert_eq!(folders.len(), 1);
        assert_eq!(folders[0].total_count, 3);
        assert_eq!(folders[0].unread_count, 2);
    }

    #[test]
    fn folder_sync_state_roundtrip() {
        let (_dir, store) = temp_store();
        let account = account();
        store.insert_account(&account).unwrap();
        let folder = store.upsert_folder(&folder(&account.id, "INBOX")).unwrap();
        assert_eq!(store.folder_sync_state(&folder.id).unwrap(), None);
        store.set_folder_sync_state(&folder.id, "delta-1").unwrap();
        assert_eq!(
            store.folder_sync_state(&folder.id).unwrap().as_deref(),
            Some("delta-1")
        );

        let mut renamed = folder.clone();
        renamed.id = FolderId::new();
        renamed.name = "Inbox renamed".into();
        let again = store.upsert_folder(&renamed).unwrap();
        assert_eq!(again.id, folder.id);
        assert_eq!(
            store.folder_sync_state(&again.id).unwrap().as_deref(),
            Some("delta-1")
        );
    }

    #[test]
    fn delete_messages_and_clear_folder() {
        let (_dir, store) = temp_store();
        let account = account();
        store.insert_account(&account).unwrap();
        let folder = store.upsert_folder(&folder(&account.id, "INBOX")).unwrap();
        store
            .upsert_messages(&[
                message(&account.id, &folder.id, "a", 1, false),
                message(&account.id, &folder.id, "b", 2, false),
                message(&account.id, &folder.id, "c", 3, false),
            ])
            .unwrap();

        store
            .delete_messages(&folder.id, &["a".to_string()])
            .unwrap();
        let left = store.messages(&folder.id, 0, 10).unwrap();
        assert_eq!(left.len(), 2);
        assert!(left.iter().all(|row| row.remote_id != "a"));

        store.clear_folder(&folder.id).unwrap();
        assert!(store.messages(&folder.id, 0, 10).unwrap().is_empty());
    }
}
