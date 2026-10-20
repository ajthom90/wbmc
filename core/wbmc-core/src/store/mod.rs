// This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0. If a copy of the MPL was not distributed with this file, You can obtain one at https://mozilla.org/MPL/2.0/.

mod bodies;
mod migrations;

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::de::DeserializeOwned;

use crate::model::{
    Account, AccountId, Folder, FolderId, FolderRole, MessageId, MessageSummary, ProviderKind,
};
use crate::{CoreError, Result};

pub use bodies::BodyStore;

const SCHEMA_SQL: &str = include_str!("schema.sql");
const SCHEMA_VERSION: i64 = migrations::STEPS[migrations::STEPS.len() - 1].0;

const ACCOUNT_COLUMNS: &str = "id, kind, email, display_name, config";
const FOLDER_COLUMNS: &str =
    "id, account_id, remote_id, name, role, parent_id, unread_count, total_count";
const MESSAGE_COLUMNS: &str = "\
messages.id, messages.account_id, messages.folder_id, messages.remote_id, messages.subject, \
messages.from_json, messages.to_json, messages.cc_json, messages.date_ms, messages.snippet, \
messages.is_read, messages.is_flagged, messages.has_attachments";

pub struct Store {
    conn: Mutex<Connection>,
}

/// Ids written and removed while applying one sync page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageApplied {
    pub stored_ids: Vec<MessageId>,
    pub removed_ids: Vec<MessageId>,
}

/// Provider-independent flag changes for a sync page.
pub struct FlagChange {
    pub remote_id: String,
    pub is_read: bool,
    pub is_flagged: bool,
}

/// All changes and the resumable cursor committed in one transaction.
pub struct PageChanges<'a> {
    pub upserts: &'a [MessageSummary],
    pub deleted_remote_ids: &'a [String],
    pub flag_updates: &'a [FlagChange],
    pub retain_only_remote_ids: Option<&'a [String]>,
    pub sync_state: &'a str,
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

    /// Updates the kind-independent columns (`email`, `display_name`, `config`).
    pub fn update_account(&self, account: &Account) -> Result<()> {
        let conn = self.lock()?;
        let config = json_text(&account.config)?;
        let changed = conn.execute(
            "UPDATE accounts SET email = ?2, display_name = ?3, config = ?4 WHERE id = ?1",
            params![
                account.id.as_str(),
                account.email,
                account.display_name,
                config,
            ],
        )?;
        if changed == 0 {
            return Err(CoreError::NotFound(format!(
                "account {}",
                account.id.as_str()
            )));
        }
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

    /// Removes folders absent from the server and returns their message ids for body cleanup.
    pub fn delete_folders_not_in(
        &self,
        account: &AccountId,
        remote_ids: &[String],
    ) -> Result<Vec<MessageId>> {
        let retained: HashSet<&str> = remote_ids.iter().map(String::as_str).collect();
        self.in_transaction(|tx| {
            let mut stmt = tx.prepare("SELECT id, remote_id FROM folders WHERE account_id = ?1")?;
            let rows = stmt.query_map([account.as_str()], |row| {
                Ok((
                    FolderId::from(row.get::<_, String>(0)?),
                    row.get::<_, String>(1)?,
                ))
            })?;
            let mut doomed = Vec::new();
            for row in rows {
                let (id, remote_id) = row?;
                if !retained.contains(remote_id.as_str()) {
                    doomed.push(id);
                }
            }
            // Detach children before deletion: the schema cascades parent deletes.
            // A child still present on the server must survive a removed parent.
            for id in &doomed {
                tx.execute(
                    "UPDATE folders SET parent_id = NULL WHERE parent_id = ?1",
                    [id.as_str()],
                )?;
            }
            let mut removed = Vec::new();
            for id in &doomed {
                removed.extend(clear_folder_tx(tx, id)?);
                tx.execute("DELETE FROM folders WHERE id = ?1", [id.as_str()])?;
            }
            Ok(removed)
        })
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
        self.in_transaction(|tx| set_folder_sync_state_tx(tx, folder_id, Some(state)))
    }

    pub fn upsert_messages(&self, msgs: &[MessageSummary]) -> Result<Vec<MessageId>> {
        self.in_transaction(|tx| upsert_messages_tx(tx, msgs))
    }

    pub fn delete_messages(
        &self,
        folder: &FolderId,
        remote_ids: &[String],
    ) -> Result<Vec<MessageId>> {
        self.in_transaction(|tx| delete_messages_tx(tx, folder, remote_ids))
    }

    pub fn clear_folder(&self, folder: &FolderId) -> Result<Vec<MessageId>> {
        self.in_transaction(|tx| clear_folder_tx(tx, folder))
    }

    pub fn apply_sync_page(
        &self,
        folder: &FolderId,
        changes: &PageChanges<'_>,
    ) -> Result<PageApplied> {
        self.in_transaction(|tx| {
            let stored_ids = upsert_messages_tx(tx, changes.upserts)?;
            let mut removed_ids = delete_messages_tx(tx, folder, changes.deleted_remote_ids)?;
            update_flags_tx(tx, folder, changes.flag_updates)?;
            if let Some(remote_ids) = changes.retain_only_remote_ids {
                removed_ids.extend(prune_messages_tx(tx, folder, remote_ids)?);
            }
            set_folder_sync_state_tx(tx, folder, Some(changes.sync_state))?;
            recount_tx(tx, folder)?;
            Ok(PageApplied {
                stored_ids,
                removed_ids,
            })
        })
    }

    pub fn reset_folder(&self, folder: &FolderId) -> Result<Vec<MessageId>> {
        self.in_transaction(|tx| {
            let removed_ids = clear_folder_tx(tx, folder)?;
            set_folder_sync_state_tx(tx, folder, None)?;
            recount_tx(tx, folder)?;
            Ok(removed_ids)
        })
    }

    pub fn message_id_for_remote(
        &self,
        folder: &FolderId,
        remote_id: &str,
    ) -> Result<Option<MessageId>> {
        let conn = self.lock()?;
        optional_message_id(conn.query_row(
            "SELECT id FROM messages WHERE folder_id = ?1 AND remote_id = ?2",
            params![folder.as_str(), remote_id],
            |row| row.get(0),
        ))
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

    /// Returns messages across all accounts in folders with the requested role.
    pub fn unified_messages(
        &self,
        role: FolderRole,
        offset: u32,
        limit: u32,
    ) -> Result<Vec<MessageSummary>> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {MESSAGE_COLUMNS} FROM messages
             JOIN folders ON messages.folder_id = folders.id
             WHERE folders.role = ?1
             ORDER BY messages.date_ms DESC, messages.id DESC
             LIMIT ?2 OFFSET ?3"
        ))?;
        let rows = stmt.query_map(
            params![role.as_str(), i64::from(limit), i64::from(offset)],
            map_message,
        )?;
        rows.collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn recount(&self, folder_id: &FolderId) -> Result<()> {
        self.in_transaction(|tx| recount_tx(tx, folder_id))
    }

    fn lock(&self) -> Result<MutexGuard<'_, Connection>> {
        self.conn
            .lock()
            .map_err(|err| CoreError::Storage(format!("store lock poisoned: {err}")))
    }

    fn in_transaction<T>(&self, body: impl FnOnce(&Transaction<'_>) -> Result<T>) -> Result<T> {
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;
        let value = body(&tx)?;
        tx.commit()?;
        Ok(value)
    }
}

fn upsert_messages_tx(tx: &Transaction<'_>, msgs: &[MessageSummary]) -> Result<Vec<MessageId>> {
    if msgs.is_empty() {
        return Ok(Vec::new());
    }
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
            has_attachments = excluded.has_attachments
         RETURNING id",
    )?;
    let mut stored_ids = Vec::with_capacity(msgs.len());
    for message in msgs {
        let from_json = json_text(&message.from)?;
        let to_json = json_text(&message.to)?;
        let cc_json = json_text(&message.cc)?;
        let id: String = stmt.query_row(
            params![
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
            ],
            |row| row.get(0),
        )?;
        stored_ids.push(MessageId::from(id));
    }
    Ok(stored_ids)
}

fn delete_messages_tx(
    tx: &Transaction<'_>,
    folder: &FolderId,
    remote_ids: &[String],
) -> Result<Vec<MessageId>> {
    if remote_ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut stmt =
        tx.prepare("DELETE FROM messages WHERE folder_id = ?1 AND remote_id = ?2 RETURNING id")?;
    let mut removed_ids = Vec::new();
    for remote_id in remote_ids {
        if let Some(id) = optional_message_id(
            stmt.query_row(params![folder.as_str(), remote_id], |row| row.get(0)),
        )? {
            removed_ids.push(id);
        }
    }
    Ok(removed_ids)
}

fn update_flags_tx(tx: &Transaction<'_>, folder: &FolderId, flags: &[FlagChange]) -> Result<()> {
    let mut stmt = tx.prepare(
        "UPDATE messages SET is_read = ?3, is_flagged = ?4
         WHERE folder_id = ?1 AND remote_id = ?2 AND (is_read != ?3 OR is_flagged != ?4)",
    )?;
    for flag in flags {
        stmt.execute(params![
            folder.as_str(),
            flag.remote_id,
            flag.is_read,
            flag.is_flagged
        ])?;
    }
    Ok(())
}

fn prune_messages_tx(
    tx: &Transaction<'_>,
    folder: &FolderId,
    remote_ids: &[String],
) -> Result<Vec<MessageId>> {
    // One JSON parameter avoids SQLite's bind-variable limit on large mailboxes.
    let remote_ids = json_text(&remote_ids)?;
    let mut stmt = tx.prepare(
        "DELETE FROM messages WHERE folder_id = ?1
         AND remote_id NOT IN (SELECT value FROM json_each(?2)) RETURNING id",
    )?;
    let rows = stmt.query_map(params![folder.as_str(), remote_ids], |row| {
        row.get::<_, String>(0)
    })?;
    rows.map(|id| Ok(MessageId::from(id?))).collect()
}

fn clear_folder_tx(tx: &Transaction<'_>, folder: &FolderId) -> Result<Vec<MessageId>> {
    let mut stmt = tx.prepare("DELETE FROM messages WHERE folder_id = ?1 RETURNING id")?;
    let rows = stmt.query_map([folder.as_str()], |row| row.get::<_, String>(0))?;
    rows.map(|id| Ok(MessageId::from(id?)))
        .collect::<Result<Vec<_>>>()
}

fn set_folder_sync_state_tx(
    tx: &Transaction<'_>,
    folder: &FolderId,
    state: Option<&str>,
) -> Result<()> {
    let changed = tx.execute(
        "UPDATE folders SET sync_state = ?2 WHERE id = ?1",
        params![folder.as_str(), state],
    )?;
    if changed == 0 {
        return Err(folder_not_found(folder));
    }
    Ok(())
}

fn recount_tx(tx: &Transaction<'_>, folder: &FolderId) -> Result<()> {
    let changed = tx.execute(
        "UPDATE folders SET
            total_count = (SELECT COUNT(*) FROM messages WHERE folder_id = ?1),
            unread_count = (
                SELECT COUNT(*) FROM messages WHERE folder_id = ?1 AND is_read = 0
            )
         WHERE id = ?1",
        [folder.as_str()],
    )?;
    if changed == 0 {
        return Err(folder_not_found(folder));
    }
    Ok(())
}

fn optional_message_id(result: rusqlite::Result<String>) -> Result<Option<MessageId>> {
    match result {
        Ok(id) => Ok(Some(MessageId::from(id))),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(err) => Err(err.into()),
    }
}

fn folder_not_found(folder: &FolderId) -> CoreError {
    let id = folder.as_str();
    CoreError::NotFound(format!("folder {id}"))
}

fn migrate(conn: &Connection) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    let has_version: i64 = tx.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'schema_version'",
        [],
        |row| row.get(0),
    )?;
    let version = if has_version == 0 {
        tx.execute_batch(SCHEMA_SQL)?;
        tx.execute("INSERT INTO schema_version (version) VALUES (1)", [])?;
        1
    } else {
        tx.query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_version",
            [],
            |row| row.get::<_, i64>(0),
        )?
    };
    if version > SCHEMA_VERSION {
        return Err(CoreError::Storage(
            "This mail database was created by a newer version of WBMC.".into(),
        ));
    }
    for &(step_version, sql) in migrations::STEPS {
        if step_version > version {
            tx.execute_batch(sql)?;
        }
    }
    if version < SCHEMA_VERSION {
        tx.execute("DELETE FROM schema_version", [])?;
        tx.execute(
            "INSERT INTO schema_version (version) VALUES (?1)",
            [SCHEMA_VERSION],
        )?;
    }
    tx.commit()?;
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

    use super::{BodyStore, FlagChange, PageChanges, Store};
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
    fn v1_database_upgrades_intact() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wbmc.db");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(super::SCHEMA_SQL).unwrap();
        conn.execute("INSERT INTO schema_version (version) VALUES (1)", [])
            .unwrap();
        let v1 = Store {
            conn: std::sync::Mutex::new(conn),
        };
        let account = account();
        v1.insert_account(&account).unwrap();
        let inbox = v1.upsert_folder(&folder(&account.id, "INBOX")).unwrap();
        let mut sent = folder(&account.id, "Sent");
        sent.role = FolderRole::Sent;
        let sent = v1.upsert_folder(&sent).unwrap();
        let messages = vec![
            message(&account.id, &inbox.id, "a", 1, false),
            message(&account.id, &sent.id, "b", 2, true),
            message(&account.id, &inbox.id, "c", 3, true),
        ];
        v1.upsert_messages(&messages).unwrap();
        v1.set_folder_sync_state(&inbox.id, "cursor-v1").unwrap();
        v1.recount(&inbox.id).unwrap();
        let folders = v1.folders(&account.id).unwrap();
        drop(v1);

        // Opening twice also checks that the migration is not reapplied.
        for _ in 0..2 {
            let store = Store::open(&path).unwrap();
            assert_eq!(store.accounts().unwrap(), vec![account.clone()]);
            assert_eq!(store.folders(&account.id).unwrap(), folders);
            for msg in &messages {
                assert_eq!(store.message(&msg.id).unwrap().as_ref(), Some(msg));
            }
            assert_eq!(
                store.folder_sync_state(&inbox.id).unwrap().as_deref(),
                Some("cursor-v1")
            );
            let conn = store.lock().unwrap();
            let versions: Vec<i64> = conn
                .prepare("SELECT version FROM schema_version")
                .unwrap()
                .query_map([], |row| row.get(0))
                .unwrap()
                .map(|row| row.unwrap())
                .collect();
            assert_eq!(versions, vec![2]);
            let index_sql: String = conn
                .query_row(
                    "SELECT sql FROM sqlite_master WHERE type = 'index' AND name = 'idx_messages_date'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                index_sql,
                "CREATE INDEX idx_messages_date ON messages (date_ms DESC)"
            );
        }
    }

    #[test]
    fn newer_version_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wbmc.db");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(super::SCHEMA_SQL).unwrap();
        conn.execute("INSERT INTO schema_version (version) VALUES (99)", [])
            .unwrap();
        drop(conn);

        let error = Store::open(&path)
            .err()
            .expect("version 99 must be refused");
        assert!(matches!(
            error,
            crate::CoreError::Storage(ref message)
                if message == "This mail database was created by a newer version of WBMC."
        ));
    }

    #[test]
    fn migration_failure_rolls_back_fresh_schema() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE blocker (date_ms INTEGER);
             CREATE INDEX idx_messages_date ON blocker (date_ms);",
        )
        .unwrap();

        assert!(super::migrate(&conn).is_err());
        let baseline_tables: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE name IN ('schema_version', 'accounts', 'folders', 'messages')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(baseline_tables, 0);
    }

    #[test]
    fn migration_failure_rolls_back_upgrade() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(super::SCHEMA_SQL).unwrap();
        conn.execute_batch(
            "INSERT INTO schema_version (version) VALUES (1);
             CREATE TRIGGER reject_version_change BEFORE DELETE ON schema_version
             BEGIN SELECT RAISE(ABORT, 'injected version failure'); END;",
        )
        .unwrap();

        assert!(super::migrate(&conn).is_err());
        let version: i64 = conn
            .query_row("SELECT version FROM schema_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 1);
        let index_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name = 'idx_messages_date'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(index_count, 0);
    }

    #[test]
    fn fresh_database_is_v2_and_unified_uses_date_index() {
        let (_dir, store) = temp_store();
        let conn = store.lock().unwrap();
        let versions: Vec<i64> = conn
            .prepare("SELECT version FROM schema_version")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(|row| row.unwrap())
            .collect();
        assert_eq!(versions, vec![2]);
        let plan: Vec<String> = conn
            .prepare(&format!(
                "EXPLAIN QUERY PLAN SELECT {} FROM messages
                 JOIN folders ON messages.folder_id = folders.id
                 WHERE folders.role = ?1
                 ORDER BY messages.date_ms DESC, messages.id DESC
                 LIMIT ?2 OFFSET ?3",
                super::MESSAGE_COLUMNS
            ))
            .unwrap()
            .query_map(
                rusqlite::params![FolderRole::Inbox.as_str(), 100, 0],
                |row| row.get(3),
            )
            .unwrap()
            .map(|row| row.unwrap())
            .collect();
        assert!(
            plan.iter()
                .any(|detail| detail.contains("idx_messages_date")),
            "unified query should use the date index: {plan:?}"
        );
    }

    #[test]
    fn unified_merges_inboxes_newest_first() {
        let (_dir, store) = temp_store();
        let mut expected = Vec::new();
        let mut sent_messages = Vec::new();
        for account_index in 0..2 {
            let mut account = account();
            account.email = format!("account{account_index}@example.com");
            store.insert_account(&account).unwrap();
            let inbox = store.upsert_folder(&folder(&account.id, "INBOX")).unwrap();
            let mut sent = folder(&account.id, "Sent");
            sent.role = FolderRole::Sent;
            let sent = store.upsert_folder(&sent).unwrap();
            for i in 0..3 {
                expected.push(message(
                    &account.id,
                    &inbox.id,
                    &format!("inbox-{i}"),
                    i * 2 + account_index,
                    i % 2 == 0,
                ));
            }
            // Equal dates across accounts must use descending message ids.
            expected.push(message(&account.id, &inbox.id, "tie", 10, false));
            sent_messages.push(message(&account.id, &sent.id, "sent", 100, true));
        }
        store.upsert_messages(&expected).unwrap();
        store.upsert_messages(&sent_messages).unwrap();
        expected.sort_by(|a, b| {
            b.date_ms
                .cmp(&a.date_ms)
                .then_with(|| b.id.as_str().cmp(a.id.as_str()))
        });
        assert_eq!(
            store.unified_messages(FolderRole::Inbox, 0, 100).unwrap(),
            expected
        );
        assert_eq!(
            store.unified_messages(FolderRole::Inbox, 0, 3).unwrap(),
            expected[..3]
        );
        assert_eq!(
            store.unified_messages(FolderRole::Inbox, 3, 3).unwrap(),
            expected[3..6]
        );
        assert_eq!(
            store.unified_messages(FolderRole::Inbox, 6, 3).unwrap(),
            expected[6..]
        );
        assert!(
            store
                .unified_messages(FolderRole::Inbox, 8, 3)
                .unwrap()
                .is_empty()
        );
        assert!(
            store
                .unified_messages(FolderRole::Inbox, 0, 0)
                .unwrap()
                .is_empty()
        );
        assert!(
            store
                .unified_messages(FolderRole::Trash, 0, 10)
                .unwrap()
                .is_empty()
        );
        let sent = store.unified_messages(FolderRole::Sent, 0, 10).unwrap();
        assert_eq!(sent.len(), 2);
        assert!(sent_messages.iter().all(|message| sent.contains(message)));
    }

    #[test]
    #[ignore]
    fn unified_perf() {
        let (_dir, store) = temp_store();
        for account_index in 0..3 {
            let mut account = account();
            account.email = format!("account{account_index}@example.com");
            store.insert_account(&account).unwrap();
            let inbox = store.upsert_folder(&folder(&account.id, "INBOX")).unwrap();
            let mut sent = folder(&account.id, "Sent");
            sent.role = FolderRole::Sent;
            let sent = store.upsert_folder(&sent).unwrap();
            let batch: Vec<_> = (0..50_000)
                .map(|i| {
                    let folder = if i % 5 < 2 { &inbox } else { &sent };
                    message(
                        &account.id,
                        &folder.id,
                        &format!("r{i}"),
                        i * 3 + account_index,
                        false,
                    )
                })
                .collect();
            store.upsert_messages(&batch).unwrap();
        }

        let start = std::time::Instant::now();
        let messages = store.unified_messages(FolderRole::Inbox, 0, 100).unwrap();
        let elapsed = start.elapsed();
        println!("unified_perf: {elapsed:?} (150,000 messages, 60,000 Inbox)");
        assert_eq!(messages.len(), 100);
        assert!(
            elapsed < std::time::Duration::from_millis(20),
            "unified_messages took {elapsed:?}, expected < 20 ms"
        );
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
    fn update_account_replaces_email_name_and_config() {
        let (_dir, store) = temp_store();
        let original = account();
        store.insert_account(&original).unwrap();
        let mut updated = original.clone();
        updated.kind = ProviderKind::Microsoft;
        updated.email = "new@example.com".into();
        updated.display_name = "New Name".into();
        updated.config = serde_json::json!({"smtp_port": 2525});

        store.update_account(&updated).unwrap();

        let stored = store.accounts().unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].id, original.id);
        assert_eq!(stored[0].kind, ProviderKind::Imap);
        assert_eq!(stored[0].email, "new@example.com");
        assert_eq!(stored[0].display_name, "New Name");
        assert_eq!(stored[0].config["smtp_port"], 2525);

        let missing = account();
        let error = store.update_account(&missing).unwrap_err();
        assert!(matches!(error, crate::CoreError::NotFound(_)), "{error}");
    }

    #[test]
    fn delete_folders_not_in_returns_messages_and_is_account_scoped() {
        let (_dir, store) = temp_store();
        let account = account();
        let mut other = account.clone();
        other.id = AccountId::new();
        store.insert_account(&account).unwrap();
        store.insert_account(&other).unwrap();
        let kept = store.upsert_folder(&folder(&account.id, "keep")).unwrap();
        let gone = store.upsert_folder(&folder(&account.id, "gone")).unwrap();
        let other_folder = store.upsert_folder(&folder(&other.id, "gone")).unwrap();
        let kept_msg = message(&account.id, &kept.id, "k", 1, false);
        let gone_msg = message(&account.id, &gone.id, "g", 1, false);
        let other_msg = message(&other.id, &other_folder.id, "o", 1, false);
        store
            .upsert_messages(&[kept_msg.clone(), gone_msg.clone(), other_msg.clone()])
            .unwrap();
        let removed = store
            .delete_folders_not_in(&account.id, &["keep".into()])
            .unwrap();
        assert_eq!(removed, vec![gone_msg.id.clone()]);
        assert!(store.message(&gone_msg.id).unwrap().is_none());
        assert!(store.message(&kept_msg.id).unwrap().is_some());
        assert!(store.message(&other_msg.id).unwrap().is_some());
        assert_eq!(store.folders(&account.id).unwrap(), vec![kept]);
        assert_eq!(store.folders(&other.id).unwrap(), vec![other_folder]);
        assert!(
            store
                .delete_folders_not_in(&account.id, &["keep".into()])
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            store.delete_folders_not_in(&account.id, &[]).unwrap(),
            vec![kept_msg.id]
        );
        assert!(store.folders(&account.id).unwrap().is_empty());
        assert!(store.message(&other_msg.id).unwrap().is_some());
    }

    #[test]
    fn delete_folders_not_in_preserves_retained_child_and_rolls_back() {
        let (_dir, store) = temp_store();
        let account = account();
        store.insert_account(&account).unwrap();
        let parent = store.upsert_folder(&folder(&account.id, "parent")).unwrap();
        let mut child = folder(&account.id, "child");
        child.parent_id = Some(parent.id.clone());
        let child = store.upsert_folder(&child).unwrap();
        let gone = message(&account.id, &parent.id, "gone", 1, false);
        let kept = message(&account.id, &child.id, "kept", 1, false);
        store
            .upsert_messages(&[gone.clone(), kept.clone()])
            .unwrap();
        store
            .lock()
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER reject_folder_delete BEFORE DELETE ON folders
             BEGIN SELECT RAISE(ABORT, 'injected folder failure'); END;",
            )
            .unwrap();
        assert!(
            store
                .delete_folders_not_in(&account.id, &["child".into()])
                .is_err()
        );
        assert!(store.message(&gone.id).unwrap().is_some());
        assert_eq!(
            store
                .folders(&account.id)
                .unwrap()
                .iter()
                .find(|f| f.id == child.id)
                .unwrap()
                .parent_id,
            Some(parent.id)
        );
        store
            .lock()
            .unwrap()
            .execute_batch("DROP TRIGGER reject_folder_delete")
            .unwrap();
        assert_eq!(
            store
                .delete_folders_not_in(&account.id, &["child".into()])
                .unwrap(),
            vec![gone.id]
        );
        let folders = store.folders(&account.id).unwrap();
        assert_eq!(folders.len(), 1);
        assert_eq!(folders[0].id, child.id);
        assert_eq!(folders[0].parent_id, None);
        assert!(store.message(&kept.id).unwrap().is_some());
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
                "SELECT sql FROM sqlite_master WHERE type = 'index' AND name = 'idx_messages_folder_date'",
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
        let a = message(&account.id, &folder.id, "a", 1, false);
        let b = message(&account.id, &folder.id, "b", 2, false);
        let c = message(&account.id, &folder.id, "c", 3, false);
        let a_id = a.id.clone();
        let b_id = b.id.clone();
        let c_id = c.id.clone();
        store.upsert_messages(&[a, b, c]).unwrap();

        let removed = store
            .delete_messages(&folder.id, &["a".to_string(), "missing".to_string()])
            .unwrap();
        assert_eq!(removed, vec![a_id]);
        let left = store.messages(&folder.id, 0, 10).unwrap();
        assert_eq!(left.len(), 2);
        assert!(left.iter().all(|row| row.remote_id != "a"));

        let cleared = store.clear_folder(&folder.id).unwrap();
        assert_eq!(cleared.len(), 2);
        assert!(cleared.contains(&b_id));
        assert!(cleared.contains(&c_id));
        assert!(store.messages(&folder.id, 0, 10).unwrap().is_empty());
    }

    #[test]
    fn upsert_messages_returns_stored_ids() {
        let (_dir, store) = temp_store();
        let account = account();
        store.insert_account(&account).unwrap();
        let folder = store.upsert_folder(&folder(&account.id, "INBOX")).unwrap();
        let first = message(&account.id, &folder.id, "r1", 1_000, false);
        let original_id = first.id.clone();
        assert_eq!(
            store.upsert_messages(std::slice::from_ref(&first)).unwrap(),
            vec![original_id.clone()]
        );

        let mut again = message(&account.id, &folder.id, "r1", 2_000, true);
        again.id = MessageId::new();
        let fresh = message(&account.id, &folder.id, "r2", 3_000, false);
        let fresh_id = fresh.id.clone();
        assert_ne!(again.id, original_id);
        assert_eq!(
            store.upsert_messages(&[again, fresh]).unwrap(),
            vec![original_id, fresh_id]
        );
    }

    #[test]
    fn apply_sync_page_is_atomic() {
        let (_dir, store) = temp_store();
        let account = account();
        store.insert_account(&account).unwrap();
        let folder = store.upsert_folder(&folder(&account.id, "INBOX")).unwrap();
        let keep = message(&account.id, &folder.id, "keep", 1_000, false);
        let doomed = message(&account.id, &folder.id, "doomed", 2_000, false);
        let keep_id = keep.id.clone();
        store.upsert_messages(&[keep, doomed]).unwrap();
        store.set_folder_sync_state(&folder.id, "before").unwrap();

        let mut updated = message(&account.id, &folder.id, "keep", 1_500, true);
        updated.id = MessageId::new();
        let mut bad = message(&account.id, &folder.id, "bad", 3_000, false);
        bad.folder_id = FolderId::new();

        let err = store
            .apply_sync_page(
                &folder.id,
                &PageChanges {
                    upserts: &[updated, bad],
                    deleted_remote_ids: &["doomed".to_string()],
                    flag_updates: &[],
                    retain_only_remote_ids: None,
                    sync_state: "after",
                },
            )
            .unwrap_err();
        assert!(matches!(err, crate::CoreError::Storage(_)));

        let rows = store.messages(&folder.id, 0, 10).unwrap();
        assert_eq!(rows.len(), 2);
        let kept = rows.iter().find(|row| row.remote_id == "keep").unwrap();
        assert_eq!(kept.id, keep_id);
        assert!(!kept.is_read);
        assert!(rows.iter().any(|row| row.remote_id == "doomed"));
        assert_eq!(
            store.folder_sync_state(&folder.id).unwrap().as_deref(),
            Some("before")
        );
    }

    #[test]
    fn apply_sync_page_applies_all() {
        let (_dir, store) = temp_store();
        let account = account();
        store.insert_account(&account).unwrap();
        let folder = store.upsert_folder(&folder(&account.id, "INBOX")).unwrap();
        let a = message(&account.id, &folder.id, "a", 1, false);
        let b = message(&account.id, &folder.id, "b", 2, true);
        let c = message(&account.id, &folder.id, "c", 3, false);
        let a_id = a.id.clone();
        let b_id = b.id.clone();
        store.upsert_messages(&[a, b, c]).unwrap();

        let mut b_updated = message(&account.id, &folder.id, "b", 4, false);
        b_updated.id = MessageId::new();
        let d = message(&account.id, &folder.id, "d", 5, false);
        let d_id = d.id.clone();

        let applied = store
            .apply_sync_page(
                &folder.id,
                &PageChanges {
                    upserts: &[b_updated, d],
                    deleted_remote_ids: &["a".to_string()],
                    flag_updates: &[],
                    retain_only_remote_ids: None,
                    sync_state: "cursor-1",
                },
            )
            .unwrap();
        assert_eq!(applied.stored_ids, vec![b_id.clone(), d_id.clone()]);
        assert_eq!(applied.removed_ids, vec![a_id]);

        let rows = store.messages(&folder.id, 0, 10).unwrap();
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().all(|row| row.remote_id != "a"));
        let stored_b = rows.iter().find(|row| row.remote_id == "b").unwrap();
        assert_eq!(stored_b.id, b_id);
        assert!(!stored_b.is_read);
        assert!(rows.iter().any(|row| row.id == d_id));
        assert_eq!(
            store.folder_sync_state(&folder.id).unwrap().as_deref(),
            Some("cursor-1")
        );
        let folders = store.folders(&account.id).unwrap();
        assert_eq!(folders[0].total_count, 3);
        assert_eq!(folders[0].unread_count, 3);
    }

    #[test]
    fn reset_folder_clears_messages_and_state() {
        let (_dir, store) = temp_store();
        let account = account();
        store.insert_account(&account).unwrap();
        let folder = store.upsert_folder(&folder(&account.id, "INBOX")).unwrap();
        let a = message(&account.id, &folder.id, "a", 1, false);
        let b = message(&account.id, &folder.id, "b", 2, true);
        let a_id = a.id.clone();
        let b_id = b.id.clone();
        store.upsert_messages(&[a, b]).unwrap();
        store.set_folder_sync_state(&folder.id, "cursor-1").unwrap();
        store.recount(&folder.id).unwrap();

        let removed = store.reset_folder(&folder.id).unwrap();
        assert_eq!(removed.len(), 2);
        assert!(removed.contains(&a_id));
        assert!(removed.contains(&b_id));
        assert!(store.messages(&folder.id, 0, 10).unwrap().is_empty());
        assert_eq!(store.folder_sync_state(&folder.id).unwrap(), None);
        let folders = store.folders(&account.id).unwrap();
        assert_eq!(folders[0].total_count, 0);
        assert_eq!(folders[0].unread_count, 0);
    }

    #[test]
    fn message_id_for_remote_finds_and_misses() {
        let (_dir, store) = temp_store();
        let account = account();
        store.insert_account(&account).unwrap();
        let folder = store.upsert_folder(&folder(&account.id, "INBOX")).unwrap();
        let msg = message(&account.id, &folder.id, "r1", 1_000, false);
        let id = msg.id.clone();
        store.upsert_messages(std::slice::from_ref(&msg)).unwrap();

        assert_eq!(
            store
                .message_id_for_remote(&folder.id, "r1")
                .unwrap()
                .as_ref(),
            Some(&id)
        );
        assert_eq!(
            store.message_id_for_remote(&folder.id, "missing").unwrap(),
            None
        );
    }
    #[test]
    fn flag_updates_applied() {
        let (_dir, store) = temp_store();
        let account = account();
        store.insert_account(&account).unwrap();
        let folder = store.upsert_folder(&folder(&account.id, "INBOX")).unwrap();
        let msg = message(&account.id, &folder.id, "a", 1, false);
        let id = msg.id.clone();
        store.upsert_messages(&[msg]).unwrap();
        // Reject redundant writes: unchanged flags must not issue an UPDATE.
        store.lock().unwrap().execute_batch(
            "CREATE TRIGGER reject_unchanged_flags BEFORE UPDATE OF is_read, is_flagged ON messages
             WHEN OLD.is_read = NEW.is_read AND OLD.is_flagged = NEW.is_flagged
             BEGIN SELECT RAISE(ABORT, 'redundant flag update'); END;"
        ).unwrap();
        let flags = [
            FlagChange {
                remote_id: "a".into(),
                is_read: true,
                is_flagged: true,
            },
            FlagChange {
                remote_id: "missing".into(),
                is_read: true,
                is_flagged: false,
            },
        ];
        let changes = PageChanges {
            upserts: &[],
            deleted_remote_ids: &[],
            flag_updates: &flags,
            retain_only_remote_ids: None,
            sync_state: "flagged",
        };
        store.apply_sync_page(&folder.id, &changes).unwrap();
        store.apply_sync_page(&folder.id, &changes).unwrap();
        let msg = store.message(&id).unwrap().unwrap();
        assert!(msg.is_read && msg.is_flagged);
        let folder = &store.folders(&account.id).unwrap()[0];
        assert_eq!((folder.total_count, folder.unread_count), (1, 0));
    }

    #[test]
    fn retain_only_prunes_missing() {
        let (_dir, store) = temp_store();
        let account = account();
        store.insert_account(&account).unwrap();
        let other = store.upsert_folder(&folder(&account.id, "other")).unwrap();
        let folder = store.upsert_folder(&folder(&account.id, "INBOX")).unwrap();
        let kept = message(&account.id, &folder.id, "a", 1, false);
        let removed = message(&account.id, &folder.id, "b", 2, false);
        let other_msg = message(&account.id, &other.id, "b", 2, false);
        let removed_id = removed.id.clone();
        store.upsert_messages(&[kept, removed, other_msg]).unwrap();
        let applied = store
            .apply_sync_page(
                &folder.id,
                &PageChanges {
                    upserts: &[],
                    deleted_remote_ids: &[],
                    flag_updates: &[],
                    retain_only_remote_ids: Some(&["a".into()]),
                    sync_state: "pruned",
                },
            )
            .unwrap();
        assert_eq!(applied.removed_ids, vec![removed_id]);
        assert_eq!(store.messages(&folder.id, 0, 10).unwrap().len(), 1);
        assert_eq!(store.messages(&other.id, 0, 10).unwrap().len(), 1);
        assert_eq!(
            store
                .folders(&account.id)
                .unwrap()
                .iter()
                .find(|f| f.id == folder.id)
                .unwrap()
                .total_count,
            1
        );
        let applied = store
            .apply_sync_page(
                &folder.id,
                &PageChanges {
                    upserts: &[],
                    deleted_remote_ids: &[],
                    flag_updates: &[],
                    retain_only_remote_ids: Some(&[]),
                    sync_state: "empty",
                },
            )
            .unwrap();
        assert_eq!(applied.removed_ids.len(), 1);
        assert!(store.messages(&folder.id, 0, 10).unwrap().is_empty());
    }

    #[test]
    fn page_changes_roll_back_together() {
        let (_dir, store) = temp_store();
        let account = account();
        store.insert_account(&account).unwrap();
        let folder = store.upsert_folder(&folder(&account.id, "INBOX")).unwrap();
        let initial = [
            message(&account.id, &folder.id, "a", 1, false),
            message(&account.id, &folder.id, "b", 2, false),
            message(&account.id, &folder.id, "c", 3, false),
        ];
        store.upsert_messages(&initial).unwrap();
        store.set_folder_sync_state(&folder.id, "before").unwrap();
        store.recount(&folder.id).unwrap();
        store
            .lock()
            .unwrap()
            .execute_batch(
                "CREATE TRIGGER reject_cursor BEFORE UPDATE OF sync_state ON folders
             BEGIN SELECT RAISE(ABORT, 'injected cursor failure'); END;",
            )
            .unwrap();
        let error = store
            .apply_sync_page(
                &folder.id,
                &PageChanges {
                    upserts: &[message(&account.id, &folder.id, "d", 4, false)],
                    deleted_remote_ids: &["b".into()],
                    flag_updates: &[FlagChange {
                        remote_id: "a".into(),
                        is_read: true,
                        is_flagged: true,
                    }],
                    retain_only_remote_ids: Some(&["a".into(), "d".into()]),
                    sync_state: "after",
                },
            )
            .unwrap_err();
        assert!(matches!(error, crate::CoreError::Storage(_)));
        let mut rows = store.messages(&folder.id, 0, 10).unwrap();
        rows.reverse();
        assert_eq!(rows, initial);
        assert_eq!(
            store.folder_sync_state(&folder.id).unwrap().as_deref(),
            Some("before")
        );
        let folder = &store.folders(&account.id).unwrap()[0];
        assert_eq!((folder.total_count, folder.unread_count), (3, 3));
    }
}
