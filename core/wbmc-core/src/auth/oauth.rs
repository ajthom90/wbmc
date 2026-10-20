// This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0. If a copy of the MPL was not distributed with this file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Provider-neutral authorization code + PKCE flow and token refresh.

use std::time::Duration;

use tokio::sync::watch;

use super::TokenSet;
use super::loopback::Loopback;
use super::pkce::Pkce;
use crate::model::ProviderKind;
use crate::{CoreError, Result};

/// Provider-specific details for an OAuth client.
#[derive(Clone, PartialEq, Eq)]
pub struct OAuthClient {
    pub kind: ProviderKind,
    pub client_id: String,
    pub client_secret: Option<String>,
    pub authorize_url: String,
    pub token_url: String,
    pub scopes: Vec<String>,
    pub redirect_host: &'static str,
    pub extra_authorize_params: Vec<(String, String)>,
}

impl std::fmt::Debug for OAuthClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthClient")
            .field("kind", &self.kind)
            .field("client_id", &self.client_id)
            .field(
                "client_secret",
                &self.client_secret.as_ref().map(|_| "<redacted>"),
            )
            .field("authorize_url", &self.authorize_url)
            .field("token_url", &self.token_url)
            .field("scopes", &self.scopes)
            .field("redirect_host", &self.redirect_host)
            .field("extra_authorize_params", &self.extra_authorize_params)
            .finish()
    }
}

pub(crate) fn not_configured(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::Microsoft => super::microsoft::NOT_CONFIGURED,
        ProviderKind::Google => super::google::NOT_CONFIGURED,
        ProviderKind::Imap => "IMAP OAuth sign-in is not configured",
    }
}

/// Cancels a sign-in, including before `finish` starts waiting.
#[derive(Clone)]
pub struct CancelHandle {
    sender: watch::Sender<bool>,
}

impl CancelHandle {
    pub fn cancel(&self) {
        self.sender.send_replace(true);
    }

    async fn cancelled(&self) {
        let mut receiver = self.sender.subscribe();
        let _ = receiver.wait_for(|cancelled| *cancelled).await;
    }
}

/// An in-progress interactive sign-in.
pub struct SignIn {
    client: OAuthClient,
    loopback: Loopback,
    pkce: Pkce,
    state: String,
    redirect_uri: String,
    url: String,
    cancel: CancelHandle,
}

impl std::fmt::Debug for SignIn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SignIn")
            .field("redirect_uri", &self.redirect_uri)
            .finish_non_exhaustive()
    }
}

impl SignIn {
    pub fn start(mut client: OAuthClient, login_hint: Option<&str>) -> Result<SignIn> {
        client.client_id = client.client_id.trim().to_string();
        if client.client_id.is_empty() {
            return Err(CoreError::InvalidInput(not_configured(client.kind).into()));
        }
        let loopback = Loopback::bind()?;
        let redirect_uri = loopback.redirect_uri(client.redirect_host);
        let pkce = Pkce::new()?;
        let state = super::random_base64url(32)?;
        let scope = client.scopes.join(" ");
        let mut url = url::Url::parse_with_params(
            &client.authorize_url,
            [
                ("client_id", client.client_id.as_str()),
                ("response_type", "code"),
                ("redirect_uri", redirect_uri.as_str()),
                ("response_mode", "query"),
                ("scope", scope.as_str()),
                ("state", state.as_str()),
                ("code_challenge", pkce.challenge.as_str()),
                ("code_challenge_method", "S256"),
            ],
        )
        .map_err(|e| CoreError::InvalidInput(format!("invalid authorize URL: {e}")))?;
        url.query_pairs_mut()
            .extend_pairs(&client.extra_authorize_params);
        if let Some(hint) = login_hint {
            url.query_pairs_mut().append_pair("login_hint", hint);
        }
        let (sender, _) = watch::channel(false);
        Ok(SignIn {
            client,
            loopback,
            pkce,
            state,
            redirect_uri,
            url: url.to_string(),
            cancel: CancelHandle { sender },
        })
    }

    pub fn url(&self) -> String {
        self.url.clone()
    }

    pub fn cancel_handle(&self) -> CancelHandle {
        self.cancel.clone()
    }

    /// Waits for the callback and exchanges the authorization code for tokens.
    pub async fn finish(self, timeout: Duration) -> Result<TokenSet> {
        let SignIn {
            client,
            loopback,
            pkce,
            state,
            redirect_uri,
            cancel,
            ..
        } = self;
        let cancelled = || CoreError::InvalidInput("Sign-in was cancelled.".into());
        let (code, responder) = tokio::select! {
            biased;
            () = cancel.cancelled() => return Err(cancelled()),
            callback = loopback.wait_for_code(&state, timeout) => callback?,
        };
        let exchange = async {
            let http = http_client()?;
            let scope = client.scopes.join(" ");
            request_tokens(
                &http,
                &client,
                &[
                    ("client_id", client.client_id.as_str()),
                    ("grant_type", "authorization_code"),
                    ("code", code.as_str()),
                    ("redirect_uri", redirect_uri.as_str()),
                    ("code_verifier", pkce.verifier.as_str()),
                    ("scope", scope.as_str()),
                ],
            )
            .await
            .map_err(|e| match e {
                TokenError::Grant(code) => {
                    CoreError::InvalidInput(format!("sign-in was rejected: {code}"))
                }
                TokenError::Other(e) => e,
            })
        };
        let result = tokio::select! {
            biased;
            () = cancel.cancelled() => Err(cancelled()),
            result = exchange => result,
        };
        responder.finish(result.is_ok()).await;
        result
    }
}

/// Refreshes tokens; unusable grants yield `NeedsSignIn` with an empty account ID.
pub async fn refresh(client: &OAuthClient, refresh_token: &str) -> Result<TokenSet> {
    if client.client_id.trim().is_empty() {
        return Err(CoreError::InvalidInput(not_configured(client.kind).into()));
    }
    let scope = client.scopes.join(" ");
    request_tokens(
        &http_client()?,
        client,
        &[
            ("client_id", client.client_id.as_str()),
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

/// HTTP client for OAuth and profile calls (rustls, bounded timeouts, no redirects
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
    client: &OAuthClient,
    form: &[(&str, &str)],
) -> std::result::Result<TokenSet, TokenError> {
    let mut form = form.to_vec();
    if let Some(secret) = client.client_secret.as_deref() {
        form.push(("client_secret", secret));
    }
    let response = http
        .post(&client.token_url)
        .header(reqwest::header::ACCEPT, "application/json")
        .form(&form)
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

/// OAuth error codes are short ASCII identifiers; anything else is dropped.
fn sanitize_code(code: &str) -> String {
    code.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
        .take(64)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{CancelHandle, OAuthClient, SignIn};
    use crate::auth::loopback::tests::send_get;
    use crate::auth::microsoft::{self, Endpoints};
    use crate::auth::{TokenSet, google};
    use crate::{CoreError, Result};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn client(server: &MockServer) -> OAuthClient {
        microsoft::client_with(
            "test-client",
            Endpoints {
                authority: server.uri(),
                graph: server.uri(),
            },
        )
        .unwrap()
    }

    async fn callback(sign_in: SignIn) -> (Result<TokenSet>, String) {
        let url = url::Url::parse(&sign_in.url()).unwrap();
        let query: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        let port = url::Url::parse(&query["redirect_uri"])
            .unwrap()
            .port()
            .unwrap();
        let handle = tokio::spawn(sign_in.finish(Duration::from_secs(5)));
        let response = send_get(port, &format!("/?code=auth-code&state={}", query["state"])).await;
        (handle.await.unwrap(), response)
    }

    #[tokio::test]
    async fn token_exchange_sends_secret_only_when_configured() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/oauth2/v2.0/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "at", "refresh_token": "rt", "expires_in": 3600
            })))
            .expect(2)
            .mount(&server)
            .await;
        let microsoft = client(&server);
        let google = google::client_with(
            "test-client",
            "test-client-secret",
            &microsoft.authorize_url,
            &microsoft.token_url,
        )
        .unwrap();
        for client in [microsoft, google] {
            let sign_in = SignIn::start(client, None).unwrap();
            let url = url::Url::parse(&sign_in.url()).unwrap();
            let query: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
            assert_eq!(query["code_challenge"], sign_in.pkce.challenge);
            assert!(!sign_in.url().contains(&sign_in.pkce.verifier));
            let (result, response) = callback(sign_in).await;
            result.unwrap();
            assert!(response.contains("Signed in"));
        }
        let requests = server.received_requests().await.unwrap();
        let forms: Vec<std::collections::HashMap<String, String>> = requests
            .iter()
            .map(|r| url::form_urlencoded::parse(&r.body).into_owned().collect())
            .collect();
        assert!(!forms[0].contains_key("client_secret"));
        assert_eq!(forms[1]["client_secret"], "test-client-secret");
    }

    #[tokio::test]
    async fn cancel_finishes_promptly() {
        fn send_sync_clone<T: Send + Sync + Clone>() {}
        send_sync_clone::<CancelHandle>();
        let client = microsoft::client_with("test-client", Endpoints::default()).unwrap();
        let sign_in = SignIn::start(client.clone(), None).unwrap();
        sign_in.cancel_handle().cancel();
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            sign_in.finish(Duration::from_secs(30)),
        )
        .await
        .unwrap();
        assert!(
            matches!(result, Err(CoreError::InvalidInput(message)) if message == "Sign-in was cancelled.")
        );

        let sign_in = SignIn::start(client, None).unwrap();
        let cancel = sign_in.cancel_handle();
        let handle = tokio::spawn(sign_in.finish(Duration::from_secs(30)));
        tokio::task::yield_now().await;
        cancel.clone().cancel();
        let result = tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(result, Err(CoreError::InvalidInput(message)) if message == "Sign-in was cancelled.")
        );
    }

    #[tokio::test]
    async fn success_page_after_exchange() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_json(serde_json::json!({"error": "invalid_grant"})),
            )
            .mount(&server)
            .await;
        let (result, response) = callback(SignIn::start(client(&server), None).unwrap()).await;
        assert!(matches!(result, Err(CoreError::InvalidInput(_))));
        assert!(
            response.contains("Sign-in failed. Return to WBMC."),
            "{response}"
        );
        assert!(!response.contains("Signed in"));
    }

    #[tokio::test]
    async fn cancel_during_exchange_answers_failure_promptly() {
        let server = MockServer::start().await;
        let (started, mut received) = tokio::sync::mpsc::unbounded_channel();
        Mock::given(method("POST"))
            .respond_with(move |_: &wiremock::Request| {
                started.send(()).unwrap();
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"access_token": "at", "expires_in": 3600}))
                    .set_delay(Duration::from_secs(5))
            })
            .mount(&server)
            .await;
        let sign_in = SignIn::start(client(&server), None).unwrap();
        let cancel = sign_in.cancel_handle();
        let handle = tokio::spawn(callback(sign_in));
        tokio::time::timeout(Duration::from_secs(2), received.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(!handle.is_finished());
        cancel.cancel();
        let (result, response) = tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(result, Err(CoreError::InvalidInput(message)) if message == "Sign-in was cancelled.")
        );
        assert!(response.contains("Sign-in failed. Return to WBMC."));
        assert!(!response.contains("Signed in"));
    }

    #[tokio::test]
    async fn success_page_waits_for_exchange() {
        let loopback = crate::auth::loopback::Loopback::bind().unwrap();
        let port = loopback.port();
        let mut browser =
            tokio::spawn(async move { send_get(port, "/?code=code&state=state").await });
        let (code, responder) = loopback
            .wait_for_code("state", Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(code, "code");
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut browser)
                .await
                .is_err()
        );
        responder.finish(true).await;
        let response = browser.await.unwrap();
        assert!(response.contains("Signed in"));
    }

    #[test]
    fn oauth_client_debug_redacts_secret() {
        let mut client = microsoft::client_with("test-client", Endpoints::default()).unwrap();
        client.client_secret = Some("never-print-this-secret".into());
        let debug = format!("{client:?}");
        assert!(!debug.contains("never-print-this-secret"));
        assert!(debug.contains("<redacted>"));
    }
}
