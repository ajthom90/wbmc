// This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0. If a copy of the MPL was not distributed with this file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Live Microsoft Graph test. It talks only to the dedicated test mailbox.
//!
//! Ignored by default. Run with `WBMC_TEST_M365=1` and
//! `--ignored`. The refresh token in `WBMC_TEST_M365_REFRESH_TOKEN` must belong
//! to `WBMC_TEST_M365_USER`. The public client id is read from
//! `config/oauth-clients.toml`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use wbmc_core::auth::{TokenManager, microsoft};
use wbmc_core::model::{AccountId, FolderRole, ProviderKind};
use wbmc_core::provider::{GraphProvider, MailProvider};
use wbmc_core::secrets::{MemorySecrets, SecretStore, refresh_token_key};

#[tokio::test]
#[ignore]
async fn graph_live_lists_inbox_and_fetches_one_body() {
    if std::env::var("WBMC_TEST_M365").ok().as_deref() != Some("1") {
        eprintln!("skipping: set WBMC_TEST_M365=1 to run the live Graph test");
        return;
    }
    let client_id = microsoft::client_id().expect("config/oauth-clients.toml");
    if client_id.is_empty() {
        eprintln!("skipping: Microsoft client ID is empty in config/oauth-clients.toml");
        return;
    }
    let user = std::env::var("WBMC_TEST_M365_USER").expect("WBMC_TEST_M365_USER");
    let refresh =
        std::env::var("WBMC_TEST_M365_REFRESH_TOKEN").expect("WBMC_TEST_M365_REFRESH_TOKEN");

    let secrets = Arc::new(MemorySecrets::new());
    let account = AccountId::from("graph-live".to_string());
    secrets
        .set(&refresh_token_key(&account), &refresh)
        .expect("store refresh token");
    let tokens = Arc::new(TokenManager::new(
        secrets,
        HashMap::from([(
            ProviderKind::Microsoft,
            microsoft::client().expect("configured Microsoft client"),
        )]),
    ));
    let access = tokens
        .access_token(&account, ProviderKind::Microsoft)
        .await
        .expect("refresh access token");
    assert_test_user(&access, &user).await;
    drop(access);

    let provider = GraphProvider::new(account, tokens, "https://graph.microsoft.com/v1.0".into());
    let folders = provider.list_folders().await.expect("list folders");
    let inbox = folders
        .iter()
        .find(|folder| folder.role == FolderRole::Inbox)
        .unwrap_or_else(|| panic!("inbox role missing for {user}"));
    let page = provider
        .sync_folder(inbox, None, 25)
        .await
        .expect("sync inbox");
    assert!(
        !page.upserts.is_empty(),
        "inbox for {user} returned no messages"
    );
    let raw = provider
        .fetch_raw(&inbox.remote_id, &page.upserts[0].remote_id)
        .await
        .expect("fetch raw");
    assert!(!raw.is_empty());
}

async fn assert_test_user(access_token: &str, expected: &str) {
    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(60))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("http client");
    let response = http
        .get("https://graph.microsoft.com/v1.0/me?$select=mail,userPrincipalName")
        .bearer_auth(access_token)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .unwrap_or_else(|error| panic!("Graph /me: {}", error.without_url()));
    let status = response.status();
    assert!(
        status.is_success(),
        "Graph /me returned HTTP {}",
        status.as_u16()
    );
    let body: serde_json::Value = response
        .json()
        .await
        .unwrap_or_else(|error| panic!("Graph /me: {}", error.without_url()));
    let mail = body
        .get("mail")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let upn = body
        .get("userPrincipalName")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    assert!(
        mail.eq_ignore_ascii_case(expected) || upn.eq_ignore_ascii_case(expected),
        "refresh token is for {mail} / {upn}, not the test user {expected}"
    );
}
