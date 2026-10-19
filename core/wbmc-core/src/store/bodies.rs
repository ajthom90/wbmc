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
    let mut temporary = final_path.as_os_str().to_os_string();
    temporary.push(".tmp");
    PathBuf::from(temporary)
}
