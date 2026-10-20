// This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0. If a copy of the MPL was not distributed with this file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Google OAuth configuration and Gmail account identity.

use super::oauth::{OAuthClient, http_client};
use crate::model::ProviderKind;
use crate::{CoreError, Result};

pub const AUTHORIZE_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
pub const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
pub const DEFAULT_GMAIL_BASE: &str = "https://gmail.googleapis.com/gmail/v1";

pub const SCOPES: &[&str] = &[
    "openid",
    "email",
    "profile",
    "https://mail.google.com/",
    "https://www.googleapis.com/auth/calendar",
    "https://www.googleapis.com/auth/contacts",
    "https://www.googleapis.com/auth/contacts.other.readonly",
    "https://www.googleapis.com/auth/tasks",
];

pub(crate) const NOT_CONFIGURED: &str =
    "Google sign-in is not configured; see docs/setup/google-cloud-project.md";
const OAUTH_CLIENTS_TOML: &str = include_str!("../../../../config/oauth-clients.toml");

pub fn client() -> Result<OAuthClient> {
    #[derive(serde::Deserialize)]
    struct File {
        google: Table,
    }
    #[derive(serde::Deserialize)]
    struct Table {
        #[serde(default)]
        client_id: String,
    }
    let file: File = toml::from_str(OAUTH_CLIENTS_TOML)
        .map_err(|e| CoreError::InvalidInput(format!("config/oauth-clients.toml: {e}")))?;
    client_with(
        &file.google.client_id,
        option_env!("WBMC_GOOGLE_CLIENT_SECRET").unwrap_or(""),
        AUTHORIZE_URL,
        TOKEN_URL,
    )
}

/// Builds a Google client with explicit credentials and endpoints for testing.
pub fn client_with(
    client_id: &str,
    client_secret: &str,
    authorize_url: &str,
    token_url: &str,
) -> Result<OAuthClient> {
    if client_id.trim().is_empty() || client_secret.trim().is_empty() {
        return Err(CoreError::InvalidInput(NOT_CONFIGURED.into()));
    }
    Ok(OAuthClient {
        kind: ProviderKind::Google,
        client_id: client_id.trim().to_string(),
        client_secret: Some(client_secret.to_string()),
        authorize_url: authorize_url.to_string(),
        token_url: token_url.to_string(),
        scopes: SCOPES.iter().map(|s| (*s).to_string()).collect(),
        redirect_host: "127.0.0.1",
        extra_authorize_params: vec![
            ("access_type".into(), "offline".into()),
            ("prompt".into(), "consent".into()),
        ],
    })
}

/// Looks up the signed-in user's address using Gmail's getProfile endpoint.
pub async fn account_email(gmail_base: &str, access_token: &str) -> Result<String> {
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Profile {
        email_address: String,
    }
    let response = http_client()?
        .get(format!(
            "{}/users/me/profile",
            gmail_base.trim_end_matches('/')
        ))
        .bearer_auth(access_token)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(|e| CoreError::Network(format!("Gmail profile: {}", e.without_url())))?;
    let status = response.status();
    if !status.is_success() {
        return Err(CoreError::Protocol(format!(
            "Gmail profile returned HTTP {}",
            status.as_u16()
        )));
    }
    let profile: Profile = response
        .json()
        .await
        .map_err(|e| CoreError::Protocol(format!("Gmail profile: {}", e.without_url())))?;
    let email = profile.email_address.trim();
    if email.is_empty() {
        return Err(CoreError::Protocol(
            "Gmail profile returned no emailAddress".into(),
        ));
    }
    Ok(email.to_string())
}

#[cfg(test)]
mod tests {
    use super::{
        AUTHORIZE_URL, NOT_CONFIGURED, SCOPES, TOKEN_URL, account_email, client, client_with,
    };
    use crate::CoreError;
    use crate::auth::oauth::SignIn;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn google_authorize_url_has_offline_consent_and_hint() {
        let client = client_with("test-google", "test-secret", AUTHORIZE_URL, TOKEN_URL).unwrap();
        let sign_in = SignIn::start(client, Some("a@example.com")).unwrap();
        let url = url::Url::parse(&sign_in.url()).unwrap();
        let q: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(url.as_str().split('?').next().unwrap(), AUTHORIZE_URL);
        assert_eq!(q["access_type"], "offline");
        assert_eq!(q["prompt"], "consent");
        assert_eq!(q["login_hint"], "a@example.com");
        assert_eq!(q["code_challenge_method"], "S256");
        let redirect = url::Url::parse(&q["redirect_uri"]).unwrap();
        assert_eq!(redirect.host_str(), Some("127.0.0.1"));
        assert!(redirect.port().is_some());
        assert_eq!(q["scope"], SCOPES.join(" "));
        assert!(!sign_in.url().contains("test-secret"));
    }

    #[test]
    fn google_requires_client_id_and_secret() {
        for (id, secret) in [
            ("", "secret"),
            ("client", ""),
            ("  ", "secret"),
            ("client", "  "),
        ] {
            let err = client_with(id, secret, AUTHORIZE_URL, TOKEN_URL).unwrap_err();
            assert!(matches!(err, CoreError::InvalidInput(message) if message == NOT_CONFIGURED));
        }
        if option_env!("WBMC_GOOGLE_CLIENT_SECRET").is_none() {
            assert!(
                matches!(client(), Err(CoreError::InvalidInput(message)) if message == NOT_CONFIGURED)
            );
        }
    }

    #[tokio::test]
    async fn account_email_uses_gmail_profile() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/gmail/v1/users/me/profile"))
            .and(header("authorization", "Bearer test-access"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"emailAddress": "real@gmail.com"})),
            )
            .expect(1)
            .mount(&server)
            .await;
        assert_eq!(
            account_email(&format!("{}/gmail/v1/", server.uri()), "test-access")
                .await
                .unwrap(),
            "real@gmail.com"
        );
    }
}
