// This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0. If a copy of the MPL was not distributed with this file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::model::{AccountId, MessageId};
use crate::{CoreError, Result};

/// Raw message bytes on disk, one `{message}.eml` file per message.
#[derive(Debug, Clone)]
pub struct BodyStore {
    root: PathBuf,
}

impl BodyStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    pub fn put(&self, account_id: &AccountId, message_id: &MessageId, bytes: &[u8]) -> Result<()> {
        let path = self.message_path(account_id, message_id)?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let temporary = temporary_path(&path);
        let write_result: Result<()> = (|| {
            let mut file = File::create(&temporary)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            Ok(())
        })();
        if let Err(err) = write_result {
            let _ = fs::remove_file(&temporary);
            return Err(err);
        }
        if let Err(err) = fs::rename(&temporary, &path) {
            let _ = fs::remove_file(&temporary);
            // Another writer finished the same message first. The bytes are on disk.
            if path.exists() {
                return Ok(());
            }
            return Err(err.into());
        }
        Ok(())
    }

    pub fn get(&self, account_id: &AccountId, message_id: &MessageId) -> Result<Option<Vec<u8>>> {
        let path = self.message_path(account_id, message_id)?;
        match fs::read(&path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    pub fn remove(&self, account_id: &AccountId, message_id: &MessageId) -> Result<()> {
        let path = self.message_path(account_id, message_id)?;
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err.into()),
        }
    }

    fn message_path(&self, account_id: &AccountId, message_id: &MessageId) -> Result<PathBuf> {
        let account = path_component(account_id.as_str(), "account id")?;
        let message = path_component(message_id.as_str(), "message id")?;
        Ok(self.root.join(account).join(format!("{message}.eml")))
    }
}

fn path_component<'a>(id: &'a str, label: &str) -> Result<&'a str> {
    if id.is_empty()
        || id.contains('/')
        || id.contains('\\')
        || id.contains('\0')
        || id == "."
        || id == ".."
    {
        return Err(CoreError::InvalidInput(format!(
            "{label} must be a single path component"
        )));
    }
    Ok(id)
}

fn temporary_path(final_path: &Path) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
    let token = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut temporary = final_path.as_os_str().to_os_string();
    // `<id>.eml.<counter>.<pid>.tmp`, so two writers of the same message do not share a temp file.
    temporary.push(format!(".{token}.{}.tmp", std::process::id()));
    PathBuf::from(temporary)
}

#[cfg(test)]
mod tests {
    use std::thread;

    use super::BodyStore;
    use crate::model::{AccountId, MessageId};

    #[test]
    fn concurrent_puts_of_the_same_message_both_succeed() {
        let dir = tempfile::tempdir().unwrap();
        let store = BodyStore::new(dir.path().to_path_buf());
        let account = AccountId::from("acct".to_string());
        let message = MessageId::from("msg".to_string());
        let first = b"first-body".as_slice();
        let second = b"second-body".as_slice();
        thread::scope(|scope| {
            scope.spawn(|| store.put(&account, &message, first).unwrap());
            scope.spawn(|| store.put(&account, &message, second).unwrap());
        });
        let stored = store.get(&account, &message).unwrap().unwrap();
        assert!(
            stored.as_slice() == first || stored.as_slice() == second,
            "stored {stored:?}"
        );
        let leftovers: Vec<_> = std::fs::read_dir(dir.path().join("acct"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| name.to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );
    }
}
