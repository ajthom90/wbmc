// This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0. If a copy of the MPL was not distributed with this file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Live Gmail test, only for a dedicated test account with a populated Inbox.
//!
//! Run with `WBMC_TEST_GMAIL=1 cargo test -p wbmc-core --test gmail_live -- --ignored`.
//! Requires `WBMC_TEST_GMAIL_USER`, `WBMC_TEST_GMAIL_REFRESH_TOKEN`, and a
//! configured Google client (`WBMC_GOOGLE_CLIENT_SECRET` at build time).
//! The refreshed account identity must match the dedicated test user.

use std::collections::HashMap;
use std::sync::Arc;

use wbmc_core::auth::{TokenManager, google};
use wbmc_core::mime::parse_message;
use wbmc_core::model::{AccountId, FolderRole, ProviderKind};
use wbmc_core::provider::{GmailProvider, MailProvider};
use wbmc_core::secrets::{MemorySecrets, SecretStore, refresh_token_key};

#[tokio::test]
#[ignore]
async fn gmail_live_lists_inbox_and_fetches_one_body() {
    if std::env::var("WBMC_TEST_GMAIL").ok().as_deref() != Some("1") {
        eprintln!("skipping: set WBMC_TEST_GMAIL=1 to run the live Gmail test");
        return;
    }
    let user =
        std::env::var("WBMC_TEST_GMAIL_USER").expect("WBMC_TEST_GMAIL_USER (test account only)");
    assert!(!user.trim().is_empty(), "dedicated test user is required");
    let refresh =
        std::env::var("WBMC_TEST_GMAIL_REFRESH_TOKEN").expect("WBMC_TEST_GMAIL_REFRESH_TOKEN");
    let account = AccountId::from("gmail-live".to_string());
    let secrets = Arc::new(MemorySecrets::new());
    secrets
        .set(&refresh_token_key(&account), &refresh)
        .expect("store test refresh token");
    let tokens = Arc::new(TokenManager::new(
        secrets,
        HashMap::from([(
            ProviderKind::Google,
            google::client().expect("configured Google client"),
        )]),
    ));
    let access = tokens
        .access_token(&account, ProviderKind::Google)
        .await
        .expect("refresh test access token");
    let actual = google::account_email(google::DEFAULT_GMAIL_BASE, &access)
        .await
        .expect("test account identity");
    assert!(
        actual.eq_ignore_ascii_case(&user),
        "token must belong to WBMC_TEST_GMAIL_USER"
    );
    drop(access);
    let provider = GmailProvider::new(account, tokens, google::DEFAULT_GMAIL_BASE.into())
        .expect("Gmail provider");
    let folders = provider.list_folders().await.expect("list labels");
    let inbox = folders
        .iter()
        .find(|folder| folder.role == FolderRole::Inbox)
        .expect("Inbox role");
    let delta = provider
        .sync_folder(inbox, None, 25)
        .await
        .expect("first Inbox page");
    let message = delta
        .upserts
        .first()
        .expect("populate the test Inbox before running");
    let raw = provider
        .fetch_raw(&inbox.remote_id, &message.remote_id)
        .await
        .expect("raw message");
    assert!(
        mail_parser::MessageParser::default().parse(&raw).is_some(),
        "raw RFC 5322 message must parse"
    );
    let parsed = parse_message(&raw, message.date_ms);
    assert_eq!(parsed.subject, message.subject);
}
