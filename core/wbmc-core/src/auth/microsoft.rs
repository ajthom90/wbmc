// This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0. If a copy of the MPL was not distributed with this file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Microsoft OAuth configuration and Graph account identity.

use super::oauth::{OAuthClient, http_client};
use crate::model::ProviderKind;
use crate::{CoreError, Result};

/// Delegated scopes requested at sign-in (Ruling R4). Graph scopes are resource-qualified;
/// the OpenID Connect scopes stay bare.
pub const SCOPES: &[&str] = &[
    "offline_access",
    "openid",
    "profile",
    "https://graph.microsoft.com/User.Read",
    "https://graph.microsoft.com/Mail.ReadWrite",
    "https://graph.microsoft.com/Mail.Send",
    "https://graph.microsoft.com/MailboxSettings.ReadWrite",
    "https://graph.microsoft.com/Calendars.ReadWrite",
    "https://graph.microsoft.com/Contacts.ReadWrite",
    "https://graph.microsoft.com/Tasks.ReadWrite",
    "https://graph.microsoft.com/People.Read",
    "https://graph.microsoft.com/Mail.ReadWrite.Shared",
    "https://graph.microsoft.com/Mail.Send.Shared",
    "https://graph.microsoft.com/Calendars.ReadWrite.Shared",
    "https://graph.microsoft.com/Contacts.ReadWrite.Shared",
];

/// Bundled public client configuration (no secrets; see the file header).
const OAUTH_CLIENTS_TOML: &str = include_str!("../../../../config/oauth-clients.toml");

const DEFAULT_AUTHORITY: &str = "https://login.microsoftonline.com/common";
const DEFAULT_GRAPH: &str = "https://graph.microsoft.com/v1.0";
pub(crate) const NOT_CONFIGURED: &str =
    "Microsoft client ID not configured; see docs/setup/microsoft-entra-app.md";

/// Base URLs for the identity platform and Graph; injectable so tests can use `wiremock`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoints {
    /// e.g. `https://login.microsoftonline.com/common` (no trailing slash).
    pub authority: String,
    /// e.g. `https://graph.microsoft.com/v1.0` (no trailing slash).
    pub graph: String,
}

impl Default for Endpoints {
    /// The configured authority (falling back to `/common`) and Graph v1.0.
    fn default() -> Self {
        let authority = config()
            .map(|c| c.authority)
            .unwrap_or_else(|_| DEFAULT_AUTHORITY.to_string());
        Self {
            authority,
            graph: DEFAULT_GRAPH.to_string(),
        }
    }
}

impl Endpoints {
    fn token_url(&self) -> String {
        format!("{}/oauth2/v2.0/token", self.authority.trim_end_matches('/'))
    }

    fn authorize_url(&self) -> String {
        format!(
            "{}/oauth2/v2.0/authorize",
            self.authority.trim_end_matches('/')
        )
    }
}

/// The `[microsoft]` table of `config/oauth-clients.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MicrosoftConfig {
    pub client_id: String,
    pub authority: String,
}

/// Parses the bundled `config/oauth-clients.toml`.
pub fn config() -> Result<MicrosoftConfig> {
    #[derive(serde::Deserialize)]
    struct File {
        microsoft: Table,
    }
    #[derive(serde::Deserialize)]
    struct Table {
        #[serde(default)]
        client_id: String,
        #[serde(default)]
        authority: Option<String>,
    }
    let file: File = toml::from_str(OAUTH_CLIENTS_TOML)
        .map_err(|e| CoreError::InvalidInput(format!("config/oauth-clients.toml: {e}")))?;
    Ok(MicrosoftConfig {
        client_id: file.microsoft.client_id.trim().to_string(),
        authority: file
            .microsoft
            .authority
            .filter(|a| !a.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_AUTHORITY.to_string()),
    })
}

/// The bundled Microsoft client ID (may be empty if the owner has not configured it).
pub fn client_id() -> Result<String> {
    Ok(config()?.client_id)
}

/// Builds the configured Microsoft public client.
pub fn client() -> Result<OAuthClient> {
    client_with(&client_id()?, Endpoints::default())
}

/// Builds a Microsoft client with an explicit public ID and endpoints.
pub fn client_with(client_id: &str, endpoints: Endpoints) -> Result<OAuthClient> {
    if client_id.trim().is_empty() {
        return Err(CoreError::InvalidInput(NOT_CONFIGURED.into()));
    }
    Ok(OAuthClient {
        kind: ProviderKind::Microsoft,
        client_id: client_id.trim().to_string(),
        client_secret: None,
        authorize_url: endpoints.authorize_url(),
        token_url: endpoints.token_url(),
        scopes: SCOPES.iter().map(|s| (*s).to_string()).collect(),
        redirect_host: "localhost",
        extra_authorize_params: vec![
            ("prompt".into(), "select_account".into()),
            ("response_mode".into(), "query".into()),
        ],
    })
}

/// Looks up the signed-in user's address: `mail`, falling back to `userPrincipalName`.
pub async fn account_email(graph_base: &str, access_token: &str) -> Result<String> {
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Me {
        mail: Option<String>,
        user_principal_name: Option<String>,
    }
    let url = format!(
        "{}/me?$select=mail,userPrincipalName",
        graph_base.trim_end_matches('/')
    );
    let response = http_client()?
        .get(url)
        .bearer_auth(access_token)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(|e| CoreError::Network(format!("Graph /me: {}", e.without_url())))?;
    let status = response.status();
    if !status.is_success() {
        return Err(CoreError::Protocol(format!(
            "Graph /me returned HTTP {}",
            status.as_u16()
        )));
    }
    let me: Me = response
        .json()
        .await
        .map_err(|e| CoreError::Protocol(format!("Graph /me: {}", e.without_url())))?;
    me.mail
        .filter(|m| !m.trim().is_empty())
        .or(me.user_principal_name.filter(|u| !u.trim().is_empty()))
        .map(|e| e.trim().to_string())
        .ok_or_else(|| {
            CoreError::Protocol("Graph /me returned no mail or userPrincipalName".into())
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::TokenSet;
    use crate::auth::loopback::tests::send_get;
    use crate::auth::oauth::{SignIn, refresh};
    use std::time::Duration;
    use wiremock::matchers::{body_string_contains, header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn endpoints(server: &MockServer) -> Endpoints {
        Endpoints {
            authority: format!("{}/common", server.uri()),
            graph: format!("{}/v1.0", server.uri()),
        }
    }

    fn query_map(url: &str) -> std::collections::HashMap<String, String> {
        url::Url::parse(url)
            .unwrap()
            .query_pairs()
            .into_owned()
            .collect()
    }

    #[test]
    fn bundled_config_parses() {
        let config = config().unwrap();
        assert_eq!(config.authority, DEFAULT_AUTHORITY);
        assert_eq!(Endpoints::default().graph, DEFAULT_GRAPH);
    }

    #[test]
    fn scopes_are_graph_qualified() {
        // offline_access + openid + profile + the 12 registered Graph scopes.
        assert_eq!(SCOPES.len(), 15);
        for scope in &SCOPES[..3] {
            assert!(!scope.contains('/'));
        }
        for scope in &SCOPES[3..] {
            assert!(scope.starts_with("https://graph.microsoft.com/"), "{scope}");
        }
    }

    #[test]
    fn start_rejects_empty_client_id() {
        let err = SignIn::start(
            {
                let mut client = client_with("test-client", Endpoints::default()).unwrap();
                client.client_id.clear();
                client
            },
            None,
        )
        .unwrap_err();
        match err {
            CoreError::InvalidInput(message) => assert_eq!(message, NOT_CONFIGURED),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn authorize_url_has_required_params() {
        let endpoints = Endpoints {
            authority: "https://login.example.test/common".into(),
            graph: "https://graph.example.test/v1.0".into(),
        };
        let sign_in = SignIn::start(
            client_with("test-client", endpoints).unwrap(),
            Some("a@example.com"),
        )
        .unwrap();
        let url = url::Url::parse(&sign_in.url()).unwrap();
        assert_eq!(url.scheme(), "https");
        assert_eq!(url.host_str(), Some("login.example.test"));
        assert_eq!(url.path(), "/common/oauth2/v2.0/authorize");

        let q = query_map(&sign_in.url());
        assert_eq!(q["client_id"], "test-client");
        assert_eq!(q["response_type"], "code");
        assert_eq!(q["response_mode"], "query");
        assert_eq!(
            q["redirect_uri"],
            format!(
                "http://localhost:{}",
                url::Url::parse(&q["redirect_uri"]).unwrap().port().unwrap()
            )
        );
        assert_eq!(q["scope"], SCOPES.join(" "));
        assert_eq!(q["prompt"], "select_account");
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["login_hint"], "a@example.com");
        assert_eq!(q["code_challenge"].len(), 43);
        assert_eq!(q["state"].len(), 43);
    }

    async fn run_sign_in(server: &MockServer) -> Result<(TokenSet, String)> {
        let sign_in =
            SignIn::start(client_with("test-client", endpoints(server)).unwrap(), None).unwrap();
        let q = query_map(&sign_in.url());
        let port: u16 = q["redirect_uri"]
            .rsplit(':')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        let handle = tokio::spawn(sign_in.finish(Duration::from_secs(10)));
        let response = send_get(port, &format!("/?code=auth-code-1&state={}", q["state"])).await;
        assert!(response.contains("You can close this window"), "{response}");
        let tokens = handle.await.unwrap()?;
        let email = account_email(&endpoints(server).graph, &tokens.access_token).await?;
        Ok((tokens, email))
    }

    #[tokio::test]
    async fn finish_exchanges_code() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/common/oauth2/v2.0/token"))
            .and(header("content-type", "application/x-www-form-urlencoded"))
            .and(body_string_contains("grant_type=authorization_code"))
            .and(body_string_contains("client_id=test-client"))
            .and(body_string_contains("code=auth-code-1"))
            .and(body_string_contains("code_verifier="))
            .and(body_string_contains(
                "redirect_uri=http%3A%2F%2Flocalhost%3A",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "token_type": "Bearer",
                "access_token": "at-1",
                "refresh_token": "rt-1",
                "expires_in": 3600,
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me"))
            .and(query_param("$select", "mail,userPrincipalName"))
            .and(header("authorization", "Bearer at-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "mail": null,
                "userPrincipalName": "alice@contoso.test",
            })))
            .expect(1)
            .mount(&server)
            .await;

        let before = crate::auth::now_ms();
        let (tokens, email) = run_sign_in(&server).await.unwrap();
        assert_eq!(tokens.access_token, "at-1");
        assert_eq!(tokens.refresh_token.as_deref(), Some("rt-1"));
        assert!(tokens.expires_at_ms >= before + 3_600_000);
        assert!(tokens.expires_at_ms <= crate::auth::now_ms() + 3_600_000);
        assert_eq!(email, "alice@contoso.test");
    }

    #[tokio::test]
    async fn finish_prefers_mail_over_upn() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/common/oauth2/v2.0/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "at-1",
                "refresh_token": "rt-1",
                "expires_in": 3600,
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "mail": "Alice@Contoso.test",
                "userPrincipalName": "alice_upn@contoso.test",
            })))
            .mount(&server)
            .await;
        let (_, email) = run_sign_in(&server).await.unwrap();
        assert_eq!(email, "Alice@Contoso.test");
    }

    #[tokio::test]
    async fn refresh_rotates_refresh_token() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/common/oauth2/v2.0/token"))
            .and(body_string_contains("grant_type=refresh_token"))
            .and(body_string_contains("refresh_token=rt-old"))
            .and(body_string_contains("client_id=test-client"))
            .and(body_string_contains("scope="))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "at-new",
                "refresh_token": "rt-new",
                "expires_in": "3599",
            })))
            .expect(1)
            .mount(&server)
            .await;
        let tokens = refresh(
            &client_with("test-client", endpoints(&server)).unwrap(),
            "rt-old",
        )
        .await
        .unwrap();
        assert_eq!(tokens.access_token, "at-new");
        assert_eq!(tokens.refresh_token.as_deref(), Some("rt-new"));
        assert!(tokens.expires_at_ms > crate::auth::now_ms() + 3_500_000);
    }

    #[tokio::test]
    async fn invalid_grant_maps_to_needs_sign_in() {
        for code in ["invalid_grant", "interaction_required"] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(path("/common/oauth2/v2.0/token"))
                .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                    "error": code,
                    "error_description": "AADSTS70000: the grant is expired",
                })))
                .mount(&server)
                .await;
            let err = refresh(
                &client_with("test-client", endpoints(&server)).unwrap(),
                "rt-old",
            )
            .await
            .unwrap_err();
            assert!(matches!(err, CoreError::NeedsSignIn { .. }), "{err:?}");
        }
    }

    #[tokio::test]
    async fn other_token_errors_are_protocol_errors_without_secrets() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/common/oauth2/v2.0/token"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": "invalid_client",
            })))
            .mount(&server)
            .await;
        let err = refresh(
            &client_with("test-client", endpoints(&server)).unwrap(),
            "rt-secret-value",
        )
        .await
        .unwrap_err();
        assert!(matches!(err, CoreError::Protocol(_)), "{err:?}");
        assert!(err.to_string().contains("invalid_client"));
        assert!(!err.to_string().contains("rt-secret-value"));
    }
}
