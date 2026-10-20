// This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0. If a copy of the MPL was not distributed with this file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Per-account access-token cache with single-flight refresh.

use std::collections::HashMap;
use std::sync::Arc;

use super::oauth::{OAuthClient, not_configured, refresh};
use super::{TokenSet, now_ms};
use crate::model::{AccountId, ProviderKind};
use crate::secrets::{SecretStore, refresh_token_key};
use crate::{CoreError, Result};

/// Refresh when fewer than this many milliseconds of validity remain.
const REFRESH_MARGIN_MS: i64 = 5 * 60 * 1000;

#[derive(Default)]
struct Entry {
    /// Cached access token and its expiry (epoch ms).
    token: Option<(String, i64)>,
    /// Held while refreshing so concurrent callers share one refresh.
    refresh_lock: Arc<tokio::sync::Mutex<()>>,
}

/// Hands out OAuth access tokens, refreshing (and persisting the rotated refresh
/// token) when they are close to expiry.
pub struct TokenManager {
    secrets: Arc<dyn SecretStore>,
    clients: HashMap<ProviderKind, OAuthClient>,
    entries: std::sync::Mutex<HashMap<AccountId, Entry>>,
}

impl std::fmt::Debug for TokenManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenManager")
            .field("clients", &self.clients)
            .finish_non_exhaustive()
    }
}

impl TokenManager {
    pub fn new(secrets: Arc<dyn SecretStore>, clients: HashMap<ProviderKind, OAuthClient>) -> Self {
        Self {
            secrets,
            clients,
            entries: std::sync::Mutex::new(HashMap::new()),
        }
    }

    fn entries(&self) -> std::sync::MutexGuard<'_, HashMap<AccountId, Entry>> {
        self.entries
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The cached token if it has more than the refresh margin left.
    fn fresh_cached(&self, account_id: &AccountId) -> Option<String> {
        let entries = self.entries();
        match entries.get(account_id).and_then(|e| e.token.as_ref()) {
            Some((token, expires_at_ms)) if expires_at_ms - now_ms() > REFRESH_MARGIN_MS => {
                Some(token.clone())
            }
            _ => None,
        }
    }

    /// Returns a valid access token for the account, refreshing if needed.
    ///
    /// Concurrent callers for the same account perform at most one refresh.
    pub async fn access_token(&self, account_id: &AccountId, kind: ProviderKind) -> Result<String> {
        let client = self
            .clients
            .get(&kind)
            .ok_or_else(|| CoreError::InvalidInput(not_configured(kind).into()))?;
        if let Some(token) = self.fresh_cached(account_id) {
            return Ok(token);
        }
        let refresh_lock = self
            .entries()
            .entry(account_id.clone())
            .or_default()
            .refresh_lock
            .clone();
        let _guard = refresh_lock.lock().await;
        // Another caller may have refreshed while we waited.
        if let Some(token) = self.fresh_cached(account_id) {
            return Ok(token);
        }

        let needs_sign_in = || CoreError::NeedsSignIn {
            account_id: account_id.as_str().to_string(),
        };
        let key = refresh_token_key(account_id);
        let refresh_token = self
            .secrets
            .get(&key)?
            .filter(|t| !t.is_empty())
            .ok_or_else(needs_sign_in)?;
        let tokens = refresh(client, &refresh_token).await.map_err(|e| match e {
            CoreError::NeedsSignIn { .. } => needs_sign_in(),
            other => other,
        })?;
        // Persist the rotated refresh token before handing out the new access token.
        if let Some(rotated) = tokens.refresh_token.as_deref()
            && rotated != refresh_token
        {
            self.secrets.set(&key, rotated)?;
        }
        self.cache(account_id, &tokens);
        Ok(tokens.access_token)
    }

    /// Stores freshly issued tokens (e.g. after interactive sign-in): the refresh token
    /// goes to the secret store, the access token to the in-memory cache.
    pub fn store(&self, account_id: &AccountId, tokens: &TokenSet) -> Result<()> {
        if let Some(refresh_token) = tokens.refresh_token.as_deref() {
            self.secrets
                .set(&refresh_token_key(account_id), refresh_token)?;
        }
        self.cache(account_id, tokens);
        Ok(())
    }

    fn cache(&self, account_id: &AccountId, tokens: &TokenSet) {
        self.entries().entry(account_id.clone()).or_default().token =
            Some((tokens.access_token.clone(), tokens.expires_at_ms));
    }

    /// Drops the cached access token so the next call refreshes.
    pub fn invalidate(&self, account_id: &AccountId) {
        if let Some(entry) = self.entries().get_mut(account_id) {
            entry.token = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::microsoft::{Endpoints, client_with};
    use crate::model::ProviderKind;
    use crate::secrets::MemorySecrets;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn manager(server: &MockServer, secrets: Arc<MemorySecrets>) -> TokenManager {
        let client = client_with(
            "test-client",
            Endpoints {
                authority: format!("{}/common", server.uri()),
                graph: format!("{}/v1.0", server.uri()),
            },
        )
        .unwrap();
        TokenManager::new(secrets, HashMap::from([(ProviderKind::Microsoft, client)]))
    }

    fn token_response(access: &str, refresh: &str) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": access,
            "refresh_token": refresh,
            "expires_in": 3600,
        }))
    }

    #[tokio::test]
    async fn token_manager_refreshes_only_when_near_expiry() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/common/oauth2/v2.0/token"))
            .and(body_string_contains("refresh_token=rt-1"))
            .respond_with(token_response("at-2", "rt-2"))
            .expect(1)
            .mount(&server)
            .await;
        let secrets = Arc::new(MemorySecrets::new());
        let tm = manager(&server, secrets.clone());
        let account = AccountId::from("acct-1".to_string());

        // Plenty of validity left: served from cache, no refresh.
        tm.store(
            &account,
            &TokenSet {
                access_token: "at-1".into(),
                refresh_token: Some("rt-1".into()),
                expires_at_ms: now_ms() + 60 * 60 * 1000,
            },
        )
        .unwrap();
        assert_eq!(
            secrets
                .get(&refresh_token_key(&account))
                .unwrap()
                .as_deref(),
            Some("rt-1")
        );
        assert_eq!(
            tm.access_token(&account, ProviderKind::Microsoft)
                .await
                .unwrap(),
            "at-1"
        );
        assert_eq!(
            tm.access_token(&account, ProviderKind::Microsoft)
                .await
                .unwrap(),
            "at-1"
        );

        // Under five minutes left: refresh once, persist the rotated refresh token.
        tm.store(
            &account,
            &TokenSet {
                access_token: "at-1".into(),
                refresh_token: None,
                expires_at_ms: now_ms() + 4 * 60 * 1000,
            },
        )
        .unwrap();
        assert_eq!(
            tm.access_token(&account, ProviderKind::Microsoft)
                .await
                .unwrap(),
            "at-2"
        );
        assert_eq!(
            tm.access_token(&account, ProviderKind::Microsoft)
                .await
                .unwrap(),
            "at-2"
        );
        assert_eq!(
            secrets
                .get(&refresh_token_key(&account))
                .unwrap()
                .as_deref(),
            Some("rt-2")
        );
    }

    #[tokio::test]
    async fn concurrent_access_token_refreshes_once() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/common/oauth2/v2.0/token"))
            .respond_with(
                token_response("at-fresh", "rt-rotated")
                    .set_delay(std::time::Duration::from_millis(100)),
            )
            .expect(1)
            .mount(&server)
            .await;
        let secrets = Arc::new(MemorySecrets::new());
        let account = AccountId::from("acct-1".to_string());
        secrets
            .set(&refresh_token_key(&account), "rt-stored")
            .unwrap();
        let tm = Arc::new(manager(&server, secrets.clone()));

        let calls = (0..8).map(|_| {
            let tm = tm.clone();
            let account = account.clone();
            tokio::spawn(async move { tm.access_token(&account, ProviderKind::Microsoft).await })
        });
        for result in futures::future::join_all(calls).await {
            assert_eq!(result.unwrap().unwrap(), "at-fresh");
        }
        assert_eq!(
            secrets
                .get(&refresh_token_key(&account))
                .unwrap()
                .as_deref(),
            Some("rt-rotated")
        );
    }

    #[tokio::test]
    async fn invalidate_forces_refresh() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/common/oauth2/v2.0/token"))
            .respond_with(token_response("at-2", "rt-2"))
            .expect(1)
            .mount(&server)
            .await;
        let secrets = Arc::new(MemorySecrets::new());
        let tm = manager(&server, secrets.clone());
        let account = AccountId::from("acct-1".to_string());
        tm.store(
            &account,
            &TokenSet {
                access_token: "at-1".into(),
                refresh_token: Some("rt-1".into()),
                expires_at_ms: now_ms() + 60 * 60 * 1000,
            },
        )
        .unwrap();
        tm.invalidate(&account);
        assert_eq!(
            tm.access_token(&account, ProviderKind::Microsoft)
                .await
                .unwrap(),
            "at-2"
        );
    }

    #[tokio::test]
    async fn missing_refresh_token_needs_sign_in() {
        let server = MockServer::start().await;
        let tm = manager(&server, Arc::new(MemorySecrets::new()));
        let err = tm
            .access_token(
                &AccountId::from("acct-9".to_string()),
                ProviderKind::Microsoft,
            )
            .await
            .unwrap_err();
        match err {
            CoreError::NeedsSignIn { account_id } => assert_eq!(account_id, "acct-9"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn invalid_grant_needs_sign_in_for_account() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/common/oauth2/v2.0/token"))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_json(serde_json::json!({ "error": "invalid_grant" })),
            )
            .mount(&server)
            .await;
        let secrets = Arc::new(MemorySecrets::new());
        let account = AccountId::from("acct-2".to_string());
        secrets
            .set(&refresh_token_key(&account), "rt-dead")
            .unwrap();
        let tm = manager(&server, secrets);
        match tm
            .access_token(&account, ProviderKind::Microsoft)
            .await
            .unwrap_err()
        {
            CoreError::NeedsSignIn { account_id } => assert_eq!(account_id, "acct-2"),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn refresh_without_refresh_token_keeps_stored_one() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_string_contains("refresh_token=rt-stored"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "fresh", "expires_in": 3600
            })))
            .expect(2)
            .mount(&server)
            .await;
        let secrets = Arc::new(MemorySecrets::new());
        let account = AccountId::from("acct".to_string());
        secrets
            .set(&refresh_token_key(&account), "rt-stored")
            .unwrap();
        let tm = manager(&server, secrets.clone());
        for _ in 0..2 {
            assert_eq!(
                tm.access_token(&account, ProviderKind::Microsoft)
                    .await
                    .unwrap(),
                "fresh"
            );
            assert_eq!(
                secrets
                    .get(&refresh_token_key(&account))
                    .unwrap()
                    .as_deref(),
                Some("rt-stored")
            );
            tm.invalidate(&account);
        }
    }

    #[tokio::test]
    async fn token_manager_selects_provider_client() {
        let server = MockServer::start().await;
        let microsoft = client_with(
            "microsoft-client",
            Endpoints {
                authority: format!("{}/common", server.uri()),
                graph: server.uri(),
            },
        )
        .unwrap();
        let google = crate::auth::google::client_with(
            "google-client",
            "google-secret",
            &format!("{}/authorize", server.uri()),
            &format!("{}/google-token", server.uri()),
        )
        .unwrap();
        Mock::given(method("POST"))
            .and(path("/common/oauth2/v2.0/token"))
            .and(body_string_contains("client_id=microsoft-client"))
            .respond_with(token_response("microsoft-access", "microsoft-refresh"))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/google-token"))
            .and(body_string_contains("client_id=google-client"))
            .and(body_string_contains("client_secret=google-secret"))
            .respond_with(token_response("google-access", "google-refresh"))
            .expect(1)
            .mount(&server)
            .await;
        let secrets = Arc::new(MemorySecrets::new());
        let tm = TokenManager::new(
            secrets.clone(),
            HashMap::from([
                (ProviderKind::Microsoft, microsoft),
                (ProviderKind::Google, google),
            ]),
        );
        for (kind, expected) in [
            (ProviderKind::Microsoft, "microsoft-access"),
            (ProviderKind::Google, "google-access"),
        ] {
            let account = AccountId::from(kind.as_str().to_string());
            secrets.set(&refresh_token_key(&account), "stored").unwrap();
            assert_eq!(tm.access_token(&account, kind).await.unwrap(), expected);
        }
    }

    #[tokio::test]
    async fn missing_provider_client_is_not_configured() {
        let tm = TokenManager::new(Arc::new(MemorySecrets::new()), HashMap::new());
        let account = AccountId::from("acct".to_string());
        tm.store(
            &account,
            &TokenSet {
                access_token: "cached".into(),
                refresh_token: None,
                expires_at_ms: now_ms() + 3_600_000,
            },
        )
        .unwrap();
        for kind in [ProviderKind::Microsoft, ProviderKind::Google] {
            let error = tm.access_token(&account, kind).await.unwrap_err();
            assert!(
                matches!(error, CoreError::InvalidInput(message) if message == crate::auth::oauth::not_configured(kind))
            );
        }
    }
}
