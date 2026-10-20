// This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0. If a copy of the MPL was not distributed with this file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! FFI facade tests. The GreenMail tests no-op unless `WBMC_TEST_IMAP=1`.
//! Every test uses `MemorySecrets` directly, so none can touch the real keychain.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use wbmc_core::model::AccountId;
use wbmc_core::secrets::{MemorySecrets, SecretStore, password_key, refresh_token_key};

use super::*;
use wbmc_core::store::Store;

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
    // A cached body stays readable without credentials (offline, locked keychain).
    let key = password_key(&AccountId::from(account.id.clone()));
    h.secrets.delete(&key).unwrap();
    let cached = h.core.message_content(rows[0].id.clone()).await.unwrap();
    assert_eq!(cached.subject, content.subject);
    h.secrets.set(&key, "password").unwrap();
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
async fn adding_the_same_imap_account_updates_the_password() {
    if !imap_tests_enabled() {
        return;
    }
    let h = harness();
    let first = h.core.add_imap_account(input("password")).await.unwrap();
    wait_for_sync(&h.core, &first.id).await;
    let id = AccountId::from(first.id.clone());
    h.secrets.set(&password_key(&id), "stale-password").unwrap();
    h.core.inner.events.set_status(&id, "needs_sign_in");

    let mut again = input("password");
    again.email = "Alice@WBMC.test".into();
    again.display_name = "Renamed".into();
    let updated = h.core.add_imap_account(again).await.unwrap();
    assert_eq!(updated.id, first.id);
    assert!(h.core.is_syncing(&first.id));
    assert_eq!(h.core.accounts().len(), 1);
    assert_eq!(h.core.accounts()[0].email, "alice@wbmc.test");
    assert_eq!(h.core.accounts()[0].display_name, "Renamed");
    assert_ne!(h.core.accounts()[0].status, "needs_sign_in");
    assert_eq!(
        h.secrets.get(&password_key(&id)).unwrap().as_deref(),
        Some("password")
    );
    wait_for_sync(&h.core, &first.id).await;
    assert_eq!(h.core.accounts()[0].status, "idle");

    let rejected = h.core.add_imap_account(input("wrong-password")).await;
    assert!(
        matches!(rejected, Err(FfiError::InvalidInput { .. })),
        "bad password gave {rejected:?}"
    );
    assert_eq!(h.core.accounts().len(), 1);
    assert_eq!(h.core.accounts()[0].id, first.id);
    assert_eq!(
        h.secrets.get(&password_key(&id)).unwrap().as_deref(),
        Some("password")
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn re_add_updates_settings() {
    if !imap_tests_enabled() {
        return;
    }
    let h = harness();
    let first = h.core.add_imap_account(input("password")).await.unwrap();
    wait_for_sync(&h.core, &first.id).await;

    let mut again = input("password");
    again.smtp_port = 2525;
    again.display_name = "Renamed".into();
    let updated = h.core.add_imap_account(again).await.unwrap();
    assert_eq!(updated.id, first.id);
    assert_eq!(h.core.accounts().len(), 1);

    let stored = h.core.inner.store.accounts().unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].id.as_str(), first.id);
    assert_eq!(stored[0].display_name, "Renamed");
    assert_eq!(stored[0].config["smtp_port"], 2525);
    wait_for_sync(&h.core, &first.id).await;
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

    h.core.remove_account(account.id.clone()).await.unwrap();

    assert!(h.core.accounts().is_empty());
    assert!(h.core.folders(account.id.clone()).is_empty());
    assert_eq!(h.secrets.get(&password_key(&id)).unwrap(), None);
    assert_eq!(h.secrets.get(&refresh_token_key(&id)).unwrap(), None);
    assert!(!body_dir.exists());
    assert!(matches!(
        h.core.remove_account(account.id.clone()).await,
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

fn related_message(parts: &[(&str, &str, &[u8])]) -> Vec<u8> {
    use base64::Engine as _;
    let mut raw = String::from(
        "From: a@example.com\r\nSubject: cid\r\nMIME-Version: 1.0\r\n\
         Content-Type: multipart/related; boundary=\"b\"\r\n\r\n\
         --b\r\nContent-Type: text/html\r\n\r\n<p>x</p>\r\n",
    );
    for (cid, mime, bytes) in parts {
        let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
        raw.push_str(&format!(
            "--b\r\nContent-Type: {mime}\r\nContent-ID: <{cid}>\r\n\
             Content-Transfer-Encoding: base64\r\n\r\n"
        ));
        for line in encoded.as_bytes().chunks(76) {
            raw.push_str(std::str::from_utf8(line).unwrap());
            raw.push_str("\r\n");
        }
    }
    raw.push_str("--b--\r\n");
    raw.into_bytes()
}

#[test]
fn cid_inlining_has_a_total_budget_and_encodes_once() {
    let big = vec![0xA5_u8; 1024 * 1024];
    let small = vec![0x5A_u8; 16];
    let raw = related_message(&[
        ("big@x", "image/png", &big),
        ("small@x", "image/gif", &small),
    ]);
    let mut html = "<img src=\"cid:big@x\">".repeat(1000);
    html.push_str("<img src=\"cid:small@x\">");
    let started = Instant::now();
    let out = inline_cid_images(&html, &raw);
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "took {:?}",
        started.elapsed()
    );
    // 10 MiB of 1 MiB images fit; every later reference (even a tiny image) stays cid:.
    assert_eq!(out.matches("data:image/png;base64,").count(), 10);
    assert_eq!(out.matches("src=\"cid:big@x\"").count(), 990);
    assert!(out.contains("src=\"cid:small@x\""));
    assert!(out.len() <= html::MAX_INLINE_TOTAL_BYTES * 4 / 3 + html.len() + 10 * 64);
}

#[test]
fn cid_inlining_within_budget_inlines_distinct_images() {
    let raw = related_message(&[
        ("a@x", "image/png", b"aaaa"),
        ("b@x", "image/svg+xml", b"<svg/>"),
    ]);
    let out = inline_cid_images("<img src=\"cid:a@x\"><img src=\"cid:b%40x\">", &raw);
    assert!(!out.contains("cid:"), "{out}");
    assert!(out.contains("data:image/svg+xml;base64,"));
}

#[test]
fn only_plain_image_mime_types_are_safe() {
    for ok in [
        "image/png",
        "image/svg+xml",
        "image/vnd.microsoft.icon",
        "image/x-icon",
    ] {
        assert!(html::is_safe_image_mime(ok), "{ok}");
    }
    for bad in [
        "image/",
        "image",
        "text/html",
        "IMAGE/PNG",
        "image/png\"><script>",
        "image/png;base64",
        "image/png x",
        "application/image/png",
    ] {
        assert!(!html::is_safe_image_mime(bad), "{bad}");
    }
}

#[test]
fn imap_input_debug_redacts_password() {
    let debug = format!("{:?}", input("hunter2-secret"));
    assert!(!debug.contains("hunter2-secret"), "{debug}");
    assert!(debug.contains("<redacted>"));
    assert!(debug.contains("alice@wbmc.test"));
}

#[test]
fn new_microsoft_account_is_rolled_back_when_tokens_fail() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("wbmc.sqlite")).unwrap();
    let failed = save_microsoft_account(&store, "ms@example.com".into(), |_| {
        Err(wbmc_core::CoreError::Storage("keychain locked".into()))
    });
    assert!(failed.is_err());
    assert!(store.accounts().unwrap().is_empty());

    let first = save_microsoft_account(&store, "ms@example.com".into(), |_| Ok(())).unwrap();
    let again = save_microsoft_account(&store, "MS@example.com".into(), |_| Ok(())).unwrap();
    assert_eq!(first.id, again.id);
    // A failed re-sign-in keeps the existing account.
    let failed_again = save_microsoft_account(&store, "ms@example.com".into(), |_| {
        Err(wbmc_core::CoreError::Storage("keychain locked".into()))
    });
    assert!(failed_again.is_err());
    assert_eq!(store.accounts().unwrap().len(), 1);
}

#[test]
fn finishing_sync_of_removed_account_purges_residue() {
    let h = harness();
    let id = AccountId::new();
    h.core.inner.syncing().insert(id.clone(), None);
    let guard = SyncGuard {
        inner: h.core.inner.clone(),
        account_id: id.clone(),
    };
    h.core.inner.events.removed().insert(id.clone());
    // Residue written by in-flight work after `remove_account` purged.
    h.secrets.set(&password_key(&id), "pw").unwrap();
    h.secrets.set(&refresh_token_key(&id), "rotated").unwrap();
    let body_dir = h.dir.path().join("bodies").join(id.as_str());
    std::fs::create_dir_all(&body_dir).unwrap();
    std::fs::write(body_dir.join("m.eml"), b"x").unwrap();

    drop(guard);

    assert!(!body_dir.exists());
    assert_eq!(h.secrets.get(&password_key(&id)).unwrap(), None);
    assert_eq!(h.secrets.get(&refresh_token_key(&id)).unwrap(), None);
    assert!(!h.core.is_syncing(id.as_str()));
    // Removed accounts never start a new sync and their late events are dropped.
    assert!(h.core.inner.try_begin_sync(&id).is_none());
    h.core
        .inner
        .events
        .emit(wbmc_core::events::CoreEvent::SyncStatus {
            account_id: id.clone(),
            status: wbmc_core::events::SyncStatus::Idle,
        });
    assert!(h.events.events().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn remove_account_during_sync_leaves_no_residue() {
    if !imap_tests_enabled() {
        return;
    }
    let h = harness();
    let account = h.core.add_imap_account(input("password")).await.unwrap();
    let id = AccountId::from(account.id.clone());
    let body_dir = h.dir.path().join("bodies").join(&account.id);
    // Wait until the sync is prefetching bodies, then remove the account mid-flight.
    let started = Instant::now();
    while !body_dir.exists() {
        assert!(
            h.core.is_syncing(&account.id),
            "sync ended before prefetch was seen"
        );
        assert!(started.elapsed() < Duration::from_secs(120));
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    h.secrets.set(&refresh_token_key(&id), "token").unwrap();
    h.core.remove_account(account.id.clone()).await.unwrap();
    let removed_at = h
        .events
        .events()
        .iter()
        .rposition(|event| matches!(event, FfiEvent::AccountsChanged))
        .unwrap();

    wait_for_sync(&h.core, &account.id).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert!(!body_dir.exists(), "bodies re-created after removal");
    assert_eq!(h.secrets.get(&password_key(&id)).unwrap(), None);
    assert_eq!(h.secrets.get(&refresh_token_key(&id)).unwrap(), None);
    assert!(h.core.accounts().is_empty());
    let late: Vec<_> = h.events.events()[removed_at + 1..]
        .iter()
        .filter(|event| match event {
            FfiEvent::SyncStatus { account_id, .. } | FfiEvent::FoldersChanged { account_id } => {
                account_id == &account.id
            }
            _ => false,
        })
        .cloned()
        .collect();
    assert!(late.is_empty(), "late events: {late:?}");
    assert!(matches!(
        h.core.sync_account(account.id.clone()).await,
        Ok(()) | Err(FfiError::NotFound { .. })
    ));
}
