// This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0. If a copy of the MPL was not distributed with this file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Microsoft identity platform sign-in (authorization code + PKCE, loopback redirect)
//! and token refresh for Microsoft Graph.

use std::time::Duration;

use super::TokenSet;
use super::loopback::Loopback;
use super::pkce::Pkce;
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
const NOT_CONFIGURED: &str =
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

/// An in-progress interactive sign-in.
pub struct SignIn {
    client_id: String,
    endpoints: Endpoints,
    loopback: Loopback,
    pkce: Pkce,
    state: String,
    redirect_uri: String,
    url: String,
}

impl std::fmt::Debug for SignIn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SignIn")
            .field("redirect_uri", &self.redirect_uri)
            .finish_non_exhaustive()
    }
}

impl SignIn {
    /// Starts a sign-in against the configured authority.
    pub fn start(client_id: &str) -> Result<SignIn> {
        Self::start_with(client_id, Endpoints::default())
    }

    /// Starts a sign-in against explicit endpoints.
    pub fn start_with(client_id: &str, endpoints: Endpoints) -> Result<SignIn> {
        let client_id = client_id.trim();
        if client_id.is_empty() {
            return Err(CoreError::InvalidInput(NOT_CONFIGURED.into()));
        }
        let loopback = Loopback::bind()?;
        let redirect_uri = loopback.redirect_uri();
        let pkce = Pkce::new()?;
        let state = super::random_base64url(32)?;
        let scope = SCOPES.join(" ");
        let url = url::Url::parse_with_params(
            &endpoints.authorize_url(),
            [
                ("client_id", client_id),
                ("response_type", "code"),
                ("redirect_uri", redirect_uri.as_str()),
                ("response_mode", "query"),
                ("scope", scope.as_str()),
                ("state", state.as_str()),
                ("code_challenge", pkce.challenge.as_str()),
                ("code_challenge_method", "S256"),
                ("prompt", "select_account"),
            ],
        )
        .map_err(|e| CoreError::InvalidInput(format!("invalid authority URL: {e}")))?
        .to_string();
        Ok(SignIn {
            client_id: client_id.to_string(),
            endpoints,
            loopback,
            pkce,
            state,
            redirect_uri,
            url,
        })
    }

    /// The authorize URL to open in the user's browser.
    pub fn url(&self) -> String {
        self.url.clone()
    }

    /// Waits for the redirect, exchanges the code, and looks up the account's email.
    pub async fn finish(self, timeout: Duration) -> Result<(TokenSet, String)> {
        let SignIn {
            client_id,
            endpoints,
            loopback,
            pkce,
            state,
            redirect_uri,
            url: _,
        } = self;
        let code = loopback.wait_for_code(&state, timeout).await?;
        let http = http_client()?;
        let scope = SCOPES.join(" ");
        let tokens = request_tokens(
            &http,
            &endpoints,
            &[
                ("client_id", client_id.as_str()),
                ("grant_type", "authorization_code"),
                ("code", code.as_str()),
                ("redirect_uri", redirect_uri.as_str()),
                ("code_verifier", pkce.verifier.as_str()),
                ("scope", scope.as_str()),
            ],
        )
        .await
        .map_err(|e| match e {
            // A rejected code at sign-in is not a stale account; report it as a failed attempt.
            TokenError::Grant(code) => {
                CoreError::InvalidInput(format!("sign-in was rejected: {code}"))
            }
            TokenError::Other(e) => e,
        })?;
        let email = fetch_email(&http, &endpoints, &tokens.access_token).await?;
        Ok((tokens, email))
    }
}

/// Refreshes tokens with the configured authority.
///
/// An `invalid_grant` or `interaction_required` response yields `CoreError::NeedsSignIn`
/// with an empty `account_id`; callers that know the account fill it in.
pub async fn refresh(client_id: &str, refresh_token: &str) -> Result<TokenSet> {
    refresh_with(&Endpoints::default(), client_id, refresh_token).await
}

/// Refreshes tokens against explicit endpoints.
pub async fn refresh_with(
    endpoints: &Endpoints,
    client_id: &str,
    refresh_token: &str,
) -> Result<TokenSet> {
    refresh_using(&http_client()?, endpoints, client_id, refresh_token).await
}

pub(crate) async fn refresh_using(
    http: &reqwest::Client,
    endpoints: &Endpoints,
    client_id: &str,
    refresh_token: &str,
) -> Result<TokenSet> {
    let scope = SCOPES.join(" ");
    request_tokens(
        http,
        endpoints,
        &[
            ("client_id", client_id),
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("scope", scope.as_str()),
        ],
    )
    .await
    .map_err(|e| match e {
        TokenError::Grant(_) => CoreError::NeedsSignIn {
            account_id: String::new(),
        },
        TokenError::Other(e) => e,
    })
}

/// HTTP client for identity and Graph calls (rustls, bounded timeouts, no redirects
/// that could carry a bearer token elsewhere).
pub(crate) fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| CoreError::Network(format!("HTTP client: {e}")))
}

enum TokenError {
    /// The grant is no longer usable (`invalid_grant` / `interaction_required`).
    Grant(String),
    Other(CoreError),
}

// Not `Debug`: the token response carries secrets.
#[derive(serde::Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<serde_json::Value>,
    error: Option<String>,
}

/// POSTs a form to the token endpoint. Error messages carry only the OAuth error code.
async fn request_tokens(
    http: &reqwest::Client,
    endpoints: &Endpoints,
    form: &[(&str, &str)],
) -> std::result::Result<TokenSet, TokenError> {
    let response = http
        .post(endpoints.token_url())
        .header(reqwest::header::ACCEPT, "application/json")
        .form(form)
        .send()
        .await
        .map_err(|e| {
            TokenError::Other(CoreError::Network(format!(
                "token request: {}",
                e.without_url()
            )))
        })?;
    let status = response.status();
    let body = response.bytes().await.map_err(|e| {
        TokenError::Other(CoreError::Network(format!(
            "token response: {}",
            e.without_url()
        )))
    })?;
    let parsed: Option<TokenResponse> = serde_json::from_slice(&body).ok();

    if let Some(error) = parsed.as_ref().and_then(|p| p.error.as_deref()) {
        let code = sanitize_code(error);
        return Err(match code.as_str() {
            "invalid_grant" | "interaction_required" => TokenError::Grant(code),
            _ => TokenError::Other(CoreError::Protocol(format!("token endpoint error: {code}"))),
        });
    }
    if !status.is_success() {
        return Err(TokenError::Other(CoreError::Protocol(format!(
            "token endpoint returned HTTP {}",
            status.as_u16()
        ))));
    }
    let malformed = |what: &str| {
        TokenError::Other(CoreError::Protocol(format!(
            "token endpoint response missing {what}"
        )))
    };
    let parsed = parsed.ok_or_else(|| malformed("JSON body"))?;
    let access_token = parsed
        .access_token
        .filter(|t| !t.is_empty())
        .ok_or_else(|| malformed("access_token"))?;
    let expires_in = match parsed.expires_in {
        Some(serde_json::Value::Number(n)) => n.as_i64(),
        Some(serde_json::Value::String(s)) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
    .filter(|&s| s >= 0)
    .ok_or_else(|| malformed("expires_in"))?;
    Ok(TokenSet {
        access_token,
        refresh_token: parsed.refresh_token.filter(|t| !t.is_empty()),
        expires_at_ms: super::now_ms().saturating_add(expires_in.saturating_mul(1000)),
    })
}

/// Looks up the signed-in user's address: `mail`, falling back to `userPrincipalName`.
async fn fetch_email(
    http: &reqwest::Client,
    endpoints: &Endpoints,
    access_token: &str,
) -> Result<String> {
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Me {
        mail: Option<String>,
        user_principal_name: Option<String>,
    }
    let url = format!(
        "{}/me?$select=mail,userPrincipalName",
        endpoints.graph.trim_end_matches('/')
    );
    let response = http
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

/// OAuth error codes are short ASCII identifiers; anything else is dropped.
fn sanitize_code(code: &str) -> String {
    code.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .take(64)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::loopback::tests::send_get;
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
        let err = SignIn::start("").unwrap_err();
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
        let sign_in = SignIn::start_with("test-client", endpoints).unwrap();
        let url = url::Url::parse(&sign_in.url()).unwrap();
        assert_eq!(url.scheme(), "https");
        assert_eq!(url.host_str(), Some("login.example.test"));
        assert_eq!(url.path(), "/common/oauth2/v2.0/authorize");

        let q = query_map(&sign_in.url());
        assert_eq!(q["client_id"], "test-client");
        assert_eq!(q["response_type"], "code");
        assert_eq!(
            q["redirect_uri"],
            format!("http://localhost:{}", sign_in.loopback.port())
        );
        assert_eq!(q["scope"], SCOPES.join(" "));
        assert_eq!(q["prompt"], "select_account");
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["code_challenge"], sign_in.pkce.challenge);
        assert_eq!(q["code_challenge"].len(), 43);
        assert_eq!(q["state"].len(), 43);
        assert!(!sign_in.url().contains(&sign_in.pkce.verifier));
    }

    async fn run_sign_in(server: &MockServer) -> Result<(TokenSet, String)> {
        let sign_in = SignIn::start_with("test-client", endpoints(server)).unwrap();
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
        handle.await.unwrap()
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
        let tokens = refresh_with(&endpoints(&server), "test-client", "rt-old")
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
            let err = refresh_with(&endpoints(&server), "test-client", "rt-old")
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
        let err = refresh_with(&endpoints(&server), "test-client", "rt-secret-value")
            .await
            .unwrap_err();
        assert!(matches!(err, CoreError::Protocol(_)), "{err:?}");
        assert!(err.to_string().contains("invalid_client"));
        assert!(!err.to_string().contains("rt-secret-value"));
    }
}
