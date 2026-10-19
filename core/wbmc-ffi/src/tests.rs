// This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0. If a copy of the MPL was not distributed with this file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! FFI facade tests. The GreenMail tests no-op unless `WBMC_TEST_IMAP=1`.
//! Every test uses `MemorySecrets` directly, so none can touch the real keychain.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use wbmc_core::model::AccountId;
use wbmc_core::secrets::{MemorySecrets, SecretStore, password_key, refresh_token_key};

use super::*;

fn imap_tests_enabled() -> bool {
    matches!(std::env::var("WBMC_TEST_IMAP").as_deref(), Ok("1"))
}

fn imap_host() -> String {
    std::env::var("WBMC_IMAP_HOST").unwrap_or_else(|_| "127.0.0.1".into())
}

fn env_port(name: &str, default: u16) -> u16 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

#[derive(Default)]
struct Recorder(Mutex<Vec<FfiEvent>>);

impl Recorder {
    fn events(&self) -> Vec<FfiEvent> {
        self.0.lock().unwrap().clone()
    }
}

impl CoreListener for Recorder {
    fn on_event(&self, event: FfiEvent) {
        self.0.lock().unwrap().push(event);
    }
}

struct Harness {
    dir: tempfile::TempDir,
    core: Arc<WbmcCore>,
    events: Arc<Recorder>,
    secrets: Arc<MemorySecrets>,
}

fn harness() -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let events = Arc::new(Recorder::default());
    let secrets = Arc::new(MemorySecrets::new());
    let core = WbmcCore::with_secrets(
        dir.path().to_str().unwrap().to_string(),
        events.clone(),
        secrets.clone(),
    )
    .unwrap();
    Harness {
        dir,
        core,
        events,
        secrets,
    }
}

fn input(password: &str) -> ImapAccountInput {
    let host = imap_host();
    ImapAccountInput {
        email: "alice@wbmc.test".into(),
        display_name: "Alice".into(),
        host: host.clone(),
        port: env_port("WBMC_IMAP_PORT", 3143),
        security: "none".into(),
        username: "alice@wbmc.test".into(),
        password: password.into(),
        smtp_host: host,
        smtp_port: env_port("WBMC_SMTP_PORT", 3025),
        smtp_security: "none".into(),
    }
}

/// Waits for the background sync started by `add_imap_account` to finish.
async fn wait_for_sync(core: &WbmcCore, account_id: &str) {
    let started = Instant::now();
    while core.is_syncing(account_id) {
        assert!(
            started.elapsed() < Duration::from_secs(180),
            "sync did not finish within 180s"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn inbox(core: &WbmcCore, account_id: &str) -> FolderInfo {
    core.folders(account_id.to_string())
        .into_iter()
        .find(|folder| folder.role == "inbox")
        .expect("inbox folder")
}

#[tokio::test(flavor = "multi_thread")]
async fn add_imap_account_then_sync_then_read() {
    if !imap_tests_enabled() {
        return;
    }
    let h = harness();
    let account = h.core.add_imap_account(input("password")).await.unwrap();
    assert_eq!(account.kind, "imap");
    assert_eq!(account.email, "alice@wbmc.test");
    wait_for_sync(&h.core, &account.id).await;

    let accounts = h.core.accounts();
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0].status, "idle");
    let inbox = inbox(&h.core, &account.id);
    assert!(inbox.total_count >= 500, "inbox has {}", inbox.total_count);
    let rows = h.core.messages(inbox.id.clone(), 0, 50);
    assert_eq!(rows.len(), 50);
    let content = h.core.message_content(rows[0].id.clone()).await.unwrap();
    assert!(!content.subject.is_empty());
    // The stored config never contains the password.
    let stored = h.core.inner.store.accounts().unwrap();
    assert!(!stored[0].config.to_string().contains("password\""));
    assert_eq!(
        h.secrets
            .get(&password_key(&AccountId::from(account.id.clone())))
            .unwrap()
            .as_deref(),
        Some("password")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn bad_password_is_rejected_and_not_saved() {
    if !imap_tests_enabled() {
        return;
    }
    let h = harness();
    let result = h.core.add_imap_account(input("wrong-password")).await;
    assert!(
        matches!(result, Err(FfiError::InvalidInput { .. })),
        "bad password gave {result:?}"
    );
    // No secret was stored (MemorySecrets' Debug reports only the entry count).
    assert!(format!("{:?}", h.secrets).contains("len: 0"));
    assert!(h.core.accounts().is_empty());
    assert!(
        !h.events
            .events()
            .iter()
            .any(|event| matches!(event, FfiEvent::AccountsChanged))
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn listener_receives_messages_changed() {
    if !imap_tests_enabled() {
        return;
    }
    let h = harness();
    let account = h.core.add_imap_account(input("password")).await.unwrap();
    wait_for_sync(&h.core, &account.id).await;
    let inbox = inbox(&h.core, &account.id);
    let events = h.events.events();
    assert!(events.contains(&FfiEvent::AccountsChanged));
    assert!(events.contains(&FfiEvent::FoldersChanged {
        account_id: account.id.clone()
    }));
    assert!(events.contains(&FfiEvent::MessagesChanged {
        folder_id: inbox.id.clone()
    }));
    assert_eq!(
        events.last(),
        Some(&FfiEvent::SyncStatus {
            account_id: account.id.clone(),
            status: FfiSyncStatus::Idle
        })
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn remove_account_deletes_secrets_and_bodies() {
    if !imap_tests_enabled() {
        return;
    }
    let h = harness();
    let account = h.core.add_imap_account(input("password")).await.unwrap();
    wait_for_sync(&h.core, &account.id).await;
    let id = AccountId::from(account.id.clone());
    h.secrets.set(&refresh_token_key(&id), "stale").unwrap();
    let body_dir = h.dir.path().join("bodies").join(&account.id);
    assert!(body_dir.is_dir(), "inbox bodies were not prefetched");

    h.core.remove_account(account.id.clone()).unwrap();

    assert!(h.core.accounts().is_empty());
    assert!(h.core.folders(account.id.clone()).is_empty());
    assert_eq!(h.secrets.get(&password_key(&id)).unwrap(), None);
    assert_eq!(h.secrets.get(&refresh_token_key(&id)).unwrap(), None);
    assert!(!body_dir.exists());
    assert!(matches!(
        h.core.remove_account(account.id.clone()),
        Err(FfiError::NotFound { .. })
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn sync_account_is_single_flight() {
    if !imap_tests_enabled() {
        return;
    }
    let h = harness();
    let account = h.core.add_imap_account(input("password")).await.unwrap();
    // The background sync registers itself before `add_imap_account` returns.
    assert!(h.core.is_syncing(&account.id));
    let started = Instant::now();
    h.core.sync_account(account.id.clone()).await.unwrap();
    assert!(
        started.elapsed() < Duration::from_millis(500),
        "second sync waited for the first"
    );
    assert!(h.core.is_syncing(&account.id));
    assert_eq!(h.core.accounts()[0].status, "syncing");
    wait_for_sync(&h.core, &account.id).await;
    // Once idle, a new sync runs (and completes) normally.
    h.core.sync_account(account.id.clone()).await.unwrap();
    assert!(!h.core.is_syncing(&account.id));
    h.core.sync_all().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn invalid_imap_input_is_rejected_without_network() {
    let h = harness();
    for (field, value) in [
        ("host", ""),
        ("username", ""),
        ("email", ""),
        ("security", "ssl"),
    ] {
        let mut bad = input("password");
        match field {
            "host" => bad.host = value.into(),
            "username" => bad.username = value.into(),
            "email" => bad.email = value.into(),
            _ => bad.security = value.into(),
        }
        assert!(
            matches!(
                h.core.add_imap_account(bad).await,
                Err(FfiError::InvalidInput { .. })
            ),
            "{field} = {value:?} accepted"
        );
    }
    let mut bad = input("password");
    bad.port = 0;
    assert!(matches!(
        h.core.add_imap_account(bad).await,
        Err(FfiError::InvalidInput { .. })
    ));
    assert!(h.core.accounts().is_empty());
}

#[test]
fn ffi_error_mirrors_core_error() {
    use wbmc_core::CoreError;
    assert!(matches!(
        FfiError::from(CoreError::NeedsSignIn {
            account_id: "a".into()
        }),
        FfiError::NeedsSignIn { account_id } if account_id == "a"
    ));
    assert!(matches!(
        FfiError::from(CoreError::Network("down".into())),
        FfiError::Network { message } if message == "down"
    ));
}

#[test]
fn inlines_cid_images_as_data_uris() {
    let raw = concat!(
        "From: a@example.com\r\n",
        "Subject: cid\r\n",
        "MIME-Version: 1.0\r\n",
        "Content-Type: multipart/related; boundary=\"b\"\r\n",
        "\r\n",
        "--b\r\n",
        "Content-Type: text/html\r\n",
        "\r\n",
        "<img src=\"CID:logo@x\"><img src='cid:logo@x'><img src=\"cid:doc@x\"><img src=\"cid:missing\">\r\n",
        "--b\r\n",
        "Content-Type: image/png\r\n",
        "Content-ID: <logo@x>\r\n",
        "Content-Transfer-Encoding: base64\r\n",
        "\r\n",
        "iVBORw0KGgo=\r\n",
        "--b\r\n",
        "Content-Type: application/pdf\r\n",
        "Content-ID: <doc@x>\r\n",
        "\r\n",
        "%PDF\r\n",
        "--b--\r\n",
    );
    let html = "<img src=\"CID:logo@x\"><img src='cid:logo@x'><img src=\"cid:doc@x\"><img src=\"cid:missing\">";
    let out = inline_cid_images(html, raw.as_bytes());
    assert_eq!(
        out,
        "<img src=\"data:image/png;base64,iVBORw0KGgo=\"><img src='data:image/png;base64,iVBORw0KGgo='><img src=\"cid:doc@x\"><img src=\"cid:missing\">"
    );
}
