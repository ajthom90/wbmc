// This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0. If a copy of the MPL was not distributed with this file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Credential storage. Production uses the OS keychain; tests use `MemorySecrets`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::model::AccountId;
use crate::{CoreError, Result};

/// Keychain service name for every WBMC credential.
pub const SERVICE: &str = "io.github.ajthom90.wbmc";

/// Stores small secrets (passwords, refresh tokens) by key.
pub trait SecretStore: Send + Sync {
    fn get(&self, key: &str) -> Result<Option<String>>;
    fn set(&self, key: &str, value: &str) -> Result<()>;
    fn delete(&self, key: &str) -> Result<()>;
}

/// Key for an account's IMAP/SMTP password.
pub fn password_key(account_id: &AccountId) -> String {
    format!("{}:password", account_id.as_str())
}

/// Key for an account's OAuth refresh token.
pub fn refresh_token_key(account_id: &AccountId) -> String {
    format!("{}:refresh_token", account_id.as_str())
}

/// The store to use at runtime: `MemorySecrets` when `WBMC_SECRETS=memory`
/// (CI and tests), the OS keychain otherwise.
pub fn default_store() -> Arc<dyn SecretStore> {
    if std::env::var("WBMC_SECRETS").as_deref() == Ok("memory") {
        Arc::new(MemorySecrets::new())
    } else {
        Arc::new(KeyringSecrets::new())
    }
}

/// OS keychain (macOS Keychain, Windows Credential Manager, Secret Service on Linux).
#[derive(Debug, Clone)]
pub struct KeyringSecrets {
    service: String,
}

impl KeyringSecrets {
    pub fn new() -> Self {
        Self {
            service: SERVICE.to_string(),
        }
    }

    fn entry(&self, key: &str) -> Result<keyring::Entry> {
        keyring::Entry::new(&self.service, key).map_err(keyring_error)
    }
}

impl Default for KeyringSecrets {
    fn default() -> Self {
        Self::new()
    }
}

// keyring errors describe the failure, never the stored secret.
fn keyring_error(e: keyring::Error) -> CoreError {
    CoreError::Storage(format!("keychain: {e}"))
}

impl SecretStore for KeyringSecrets {
    fn get(&self, key: &str) -> Result<Option<String>> {
        match self.entry(key)?.get_password() {
            Ok(value) => Ok(Some(value)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(keyring_error(e)),
        }
    }

    fn set(&self, key: &str, value: &str) -> Result<()> {
        self.entry(key)?.set_password(value).map_err(keyring_error)
    }

    fn delete(&self, key: &str) -> Result<()> {
        match self.entry(key)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(keyring_error(e)),
        }
    }
}

/// In-process store for tests and CI. Values are lost when dropped.
#[derive(Default)]
pub struct MemorySecrets {
    values: Mutex<HashMap<String, String>>,
}

impl MemorySecrets {
    pub fn new() -> Self {
        Self::default()
    }

    fn values(&self) -> std::sync::MutexGuard<'_, HashMap<String, String>> {
        self.values
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl std::fmt::Debug for MemorySecrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemorySecrets")
            .field("len", &self.values().len())
            .finish()
    }
}

impl SecretStore for MemorySecrets {
    fn get(&self, key: &str) -> Result<Option<String>> {
        Ok(self.values().get(key).cloned())
    }

    fn set(&self, key: &str, value: &str) -> Result<()> {
        self.values().insert(key.to_string(), value.to_string());
        Ok(())
    }

    fn delete(&self, key: &str) -> Result<()> {
        self.values().remove(key);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_secrets_round_trip() {
        let store = MemorySecrets::new();
        assert_eq!(store.get("a:password").unwrap(), None);
        store.set("a:password", "hunter2").unwrap();
        assert_eq!(store.get("a:password").unwrap().as_deref(), Some("hunter2"));
        store.set("a:password", "changed").unwrap();
        assert_eq!(store.get("a:password").unwrap().as_deref(), Some("changed"));
        store.delete("a:password").unwrap();
        assert_eq!(store.get("a:password").unwrap(), None);
        store.delete("a:password").unwrap();
    }

    #[test]
    fn memory_secrets_debug_hides_values() {
        let store = MemorySecrets::new();
        store.set("k", "super-secret").unwrap();
        assert!(!format!("{store:?}").contains("super-secret"));
    }

    #[test]
    fn keys_follow_convention() {
        let id = AccountId::from("acct-1".to_string());
        assert_eq!(password_key(&id), "acct-1:password");
        assert_eq!(refresh_token_key(&id), "acct-1:refresh_token");
    }
}
