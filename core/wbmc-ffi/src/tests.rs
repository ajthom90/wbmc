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

    let id = AccountId::from(first.id.clone());
    let guard = h.core.inner.try_begin_sync(&id).expect("old password sync");
    let generation = h.core.inner.events.generation(&id);
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
    assert!(h.core.inner.events.generation(&id) > generation);
    assert!(h.core.inner.events.sync_is_stale(&id));
    assert!(!h.core.inner.restart_after_store().contains(&id));
    h.core.inner.events.emit(CoreEvent::SyncStatus {
        account_id: id,
        status: SyncStatus::NeedsSignIn,
    });
    assert_eq!(h.core.accounts()[0].status, "idle");
    drop(guard);
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
    for (field, value) in [("host", ""), ("email", ""), ("security", "ssl")] {
        let mut bad = input("password");
        match field {
            "host" => bad.host = value.into(),
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
fn sync_cannot_begin_until_credentials_are_stored() {
    let h = harness();
    let id = AccountId::new();
    h.core.inner.restart_after_store().insert(id.clone());
    h.core.inner.events.bump_generation(&id);
    assert!(h.core.inner.try_begin_sync(&id).is_none());
    assert!(!h.core.is_syncing(id.as_str()));
    h.core.inner.restart_after_store().remove(&id);
    let guard = h.core.inner.try_begin_sync(&id).expect("sync after store");
    drop(guard);
}

#[test]
fn new_oauth_account_requires_refresh_token_but_resign_in_keeps_it() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("wbmc.sqlite")).unwrap();
    let secrets = Arc::new(MemorySecrets::new());
    let manager = TokenManager::new(secrets.clone(), HashMap::new());
    for kind in [ProviderKind::Microsoft, ProviderKind::Google] {
        let mut tokens = wbmc_core::auth::TokenSet {
            access_token: "access".into(),
            refresh_token: None,
            expires_at_ms: i64::MAX,
        };
        let email = format!("{}@example.com", kind.as_str());
        for refresh_token in [None, Some(String::new())] {
            tokens.refresh_token = refresh_token;
            let error = save_oauth_account(&store, kind, email.clone(), &tokens, |_| {
                panic!("must not store tokens")
            })
            .unwrap_err();
            assert!(
                matches!(error, CoreError::InvalidInput(message) if message == "The provider did not return a refresh token; please try signing in again.")
            );
            assert!(!store.accounts().unwrap().iter().any(|a| a.kind == kind));
        }
        tokens.refresh_token = Some("stored-refresh".into());
        let first = save_oauth_account(&store, kind, email.clone(), &tokens, |id| {
            manager.store(id, &tokens)
        })
        .unwrap();
        tokens.refresh_token = None;
        let again = save_oauth_account(&store, kind, email.to_uppercase(), &tokens, |id| {
            manager.store(id, &tokens)
        })
        .unwrap();
        assert_eq!(again.id, first.id);
        assert_eq!(
            secrets
                .get(&refresh_token_key(&first.id))
                .unwrap()
                .as_deref(),
            Some("stored-refresh")
        );
    }
}

#[test]
fn new_microsoft_account_is_rolled_back_when_tokens_fail() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("wbmc.sqlite")).unwrap();
    let failed = save_oauth_account(
        &store,
        wbmc_core::model::ProviderKind::Microsoft,
        "ms@example.com".into(),
        &wbmc_core::auth::TokenSet {
            access_token: "access".into(),
            refresh_token: Some("refresh".into()),
            expires_at_ms: i64::MAX,
        },
        |_| Err(wbmc_core::CoreError::Storage("keychain locked".into())),
    );
    assert!(failed.is_err());
    assert!(store.accounts().unwrap().is_empty());

    let first = save_oauth_account(
        &store,
        wbmc_core::model::ProviderKind::Microsoft,
        "ms@example.com".into(),
        &wbmc_core::auth::TokenSet {
            access_token: "access".into(),
            refresh_token: Some("refresh".into()),
            expires_at_ms: i64::MAX,
        },
        |_| Ok(()),
    )
    .unwrap();
    let again = save_oauth_account(
        &store,
        wbmc_core::model::ProviderKind::Microsoft,
        "MS@example.com".into(),
        &wbmc_core::auth::TokenSet {
            access_token: "access".into(),
            refresh_token: None,
            expires_at_ms: i64::MAX,
        },
        |_| Ok(()),
    )
    .unwrap();
    assert_eq!(first.id, again.id);
    // A failed re-sign-in keeps the existing account.
    let failed_again = save_oauth_account(
        &store,
        wbmc_core::model::ProviderKind::Microsoft,
        "ms@example.com".into(),
        &wbmc_core::auth::TokenSet {
            access_token: "access".into(),
            refresh_token: Some("refresh".into()),
            expires_at_ms: i64::MAX,
        },
        |_| Err(wbmc_core::CoreError::Storage("keychain locked".into())),
    );
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

#[tokio::test]
async fn detect_account_maps_profile() {
    let h = harness();
    h.core.set_detect_env(DetectEnv::for_test(
        Arc::new(wbmc_core::setup::FakeResolver::new()),
        "",
        Duration::from_millis(200),
        [String::new(), String::new()],
    ));
    let setup = h.core.detect_account("x@wbmc.test".into()).await.unwrap();
    assert_eq!(setup.kind, "imap");
    assert_eq!(setup.provider_name, "WBMC Test");
    assert!(setup.app_password_url.is_none());
    let imap = setup.imap.expect("imap settings");
    assert_eq!(imap.host, "localhost");
    assert_eq!(imap.port, 3143);
    assert_eq!(imap.security, "none");
    // Core already expanded the profile token to the typed address.
    assert_eq!(imap.username, "x@wbmc.test");
    assert_eq!(imap.smtp_host, "localhost");
    assert_eq!(imap.smtp_port, 3025);
    assert_eq!(imap.smtp_security, "none");
}

#[tokio::test]
async fn detect_account_uses_injected_env() {
    let h = harness();
    let base = "";
    h.core.set_detect_env(wbmc_core::setup::DetectEnv::for_test(
        std::sync::Arc::new(wbmc_core::setup::FakeResolver::new()),
        base,
        Duration::from_millis(200),
        [
            format!("{base}/autoconfig.{{domain}}"),
            format!("{base}/well-known.{{domain}}"),
        ],
    ));
    let setup = h
        .core
        .detect_account("a@unknown.example".into())
        .await
        .unwrap();
    assert_eq!(setup.kind, "unknown");
    assert!(setup.imap.is_none());
    assert!(h.core.accounts().is_empty());
}

#[test]
fn imap_login_falls_back_to_email_without_alternatives() {
    for alternatives in [vec![], vec![" ".into()]] {
        assert_eq!(
            imap_login_candidates(" ada@example.com ", &alternatives),
            ["ada@example.com"]
        );
    }
    assert_eq!(
        imap_login_candidates("ada@example.com", &["ada".into(), "ada@example.com".into()]),
        ["ada", "ada@example.com"]
    );
}

#[test]
fn username_candidates_pass_through_literal_logins() {
    // "email" and "local" are real mailbox names here, not detection tokens.
    assert_eq!(
        username_candidates(&["local".into(), "email".into()]),
        vec!["local".to_string(), "email".to_string()]
    );
    assert_eq!(
        username_candidates(&[
            " email ".into(),
            "email".into(),
            String::new(),
            "Ada".into(),
            "Ada".into(),
        ]),
        vec!["email".to_string(), "Ada".to_string()]
    );
}

#[test]
fn begin_sign_in_unknown_provider_rejected() {
    let h = harness();
    let error = match h.core.begin_sign_in("yahoo".into(), None) {
        Err(error) => error,
        Ok(_) => panic!("unknown provider was accepted"),
    };
    assert!(
        matches!(error, FfiError::InvalidInput { ref message } if message == "unknown sign-in provider: yahoo"),
        "{error:?}"
    );
}

fn token_json(access: &str, refresh: &str) -> serde_json::Value {
    serde_json::json!({
        "access_token": access,
        "refresh_token": refresh,
        "expires_in": 3600
    })
}

async fn mount_google_sign_in(server: &wiremock::MockServer, profile_email: &str) {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(token_json("test-access", "test-refresh")),
        )
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/gmail/v1/users/me/profile"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"emailAddress": profile_email})),
        )
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/gmail/v1/users/me/labels"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"labels": []})))
        .mount(server)
        .await;
}

fn test_endpoints(server: &wiremock::MockServer) -> TestEndpoints {
    let base = server.uri();
    TestEndpoints {
        microsoft: wbmc_core::auth::Endpoints {
            authority: format!("{base}/common"),
            graph: format!("{base}/v1.0"),
        },
        google_authorize_url: format!("{base}/authorize"),
        google_token_url: format!("{base}/token"),
        gmail_base: format!("{base}/gmail/v1"),
    }
}

async fn oauth_harness() -> (Harness, wiremock::MockServer) {
    let server = wiremock::MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let events = Arc::new(Recorder::default());
    let secrets = Arc::new(MemorySecrets::new());
    let core = WbmcCore::with_endpoints(
        dir.path().to_str().unwrap().to_string(),
        events.clone(),
        secrets.clone(),
        test_endpoints(&server),
    )
    .unwrap();
    (
        Harness {
            dir,
            core,
            events,
            secrets,
        },
        server,
    )
}

fn callback_target(sign_in_url: &str) -> (u16, String) {
    let url = url::Url::parse(sign_in_url).unwrap();
    let query: std::collections::HashMap<String, String> = url.query_pairs().into_owned().collect();
    let redirect = url::Url::parse(&query["redirect_uri"]).unwrap();
    (redirect.port().unwrap(), query["state"].clone())
}

fn post_callback(sign_in_url: &str) {
    use std::io::{Read, Write};
    let (port, state) = callback_target(sign_in_url);
    let request = format!(
        "GET /?code=auth-code&state={state} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    );
    let started = Instant::now();
    loop {
        match std::net::TcpStream::connect(("127.0.0.1", port)) {
            Ok(mut stream) => {
                stream.write_all(request.as_bytes()).unwrap();
                let mut buf = Vec::new();
                let _ = stream.read_to_end(&mut buf);
                return;
            }
            Err(_) if started.elapsed() < Duration::from_secs(5) => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => panic!("callback connect: {error}"),
        }
    }
}

async fn finish_google(core: &WbmcCore, hint: &str) -> AccountInfo {
    let flow = core
        .begin_sign_in("google".into(), Some(hint.into()))
        .unwrap();
    let url = flow.url();
    let encoded = hint.replace('@', "%40");
    assert!(
        url.contains(&format!("login_hint={encoded}"))
            || url.contains(&format!("login_hint={hint}")),
        "{url}"
    );
    assert!(!url.contains("test-secret"), "{url}");
    let handle = tokio::spawn(async move { flow.finish().await });
    let callback = tokio::task::spawn_blocking(move || post_callback(&url));
    let account = handle.await.unwrap().unwrap();
    callback.await.unwrap();
    account
}

#[tokio::test]
async fn google_sign_in_saves_real_email_not_hint() {
    let (h, server) = oauth_harness().await;
    mount_google_sign_in(&server, "real@gmail.com").await;
    let account = finish_google(&h.core, "typed@gmail.com").await;
    assert_eq!(account.kind, "google");
    assert_eq!(account.email, "real@gmail.com");
    assert_eq!(h.core.accounts().len(), 1);
    assert_eq!(h.core.accounts()[0].email, "real@gmail.com");
    assert_eq!(
        h.secrets
            .get(&refresh_token_key(&AccountId::from(account.id)))
            .unwrap()
            .as_deref(),
        Some("test-refresh")
    );
}

#[tokio::test]
async fn google_re_sign_in_reuses_account() {
    let (h, server) = oauth_harness().await;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};
    let calls = Arc::new(AtomicUsize::new(0));
    let profile_calls = calls.clone();
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(token_json("test-access", "test-refresh")),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/gmail/v1/users/me/profile"))
        .respond_with(move |_: &wiremock::Request| {
            let email = if profile_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                "real@gmail.com"
            } else {
                "Real@Gmail.com"
            };
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"emailAddress": email}))
        })
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/gmail/v1/users/me/labels"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"labels": []})))
        .mount(&server)
        .await;

    let first = finish_google(&h.core, "typed@gmail.com").await;
    let second = finish_google(&h.core, "other@gmail.com").await;
    assert_eq!(first.id, second.id);
    assert_eq!(h.core.accounts().len(), 1);
    assert_eq!(h.core.accounts()[0].email, "real@gmail.com");
    assert_eq!(h.core.accounts()[0].kind, "google");
}

#[tokio::test]
async fn cancel_aborts_finish() {
    let (h, _server) = oauth_harness().await;
    let flow = h.core.begin_sign_in("google".into(), None).unwrap();
    flow.cancel();
    let error = tokio::time::timeout(Duration::from_secs(2), flow.finish())
        .await
        .expect("cancel did not finish")
        .unwrap_err();
    assert!(
        matches!(error, FfiError::InvalidInput { ref message } if message == "Sign-in was cancelled."),
        "{error:?}"
    );

    let flow = h.core.begin_sign_in("google".into(), None).unwrap();
    let finishing = flow.clone();
    let handle = tokio::spawn(async move { finishing.finish().await });
    tokio::task::yield_now().await;
    flow.cancel();
    let error = tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("cancel did not abort finish")
        .unwrap()
        .unwrap_err();
    assert!(
        matches!(error, FfiError::InvalidInput { ref message } if message == "Sign-in was cancelled."),
        "{error:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn unified_messages_include_account_id() {
    if !imap_tests_enabled() {
        return;
    }
    let h = harness();
    let alice = h.core.add_imap_account(input("password")).await.unwrap();
    wait_for_sync(&h.core, &alice.id).await;
    let mut bob_input = input("password");
    bob_input.email = "bob@wbmc.test".into();
    bob_input.display_name = "Bob".into();
    bob_input.username = "bob@wbmc.test".into();
    let bob = h.core.add_imap_account(bob_input).await.unwrap();
    wait_for_sync(&h.core, &bob.id).await;
    smtp_send("alice@wbmc.test", "bob@wbmc.test", "Hello Bob");
    h.core.sync_account(bob.id.clone()).await.unwrap();

    let rows = h.core.unified_messages(0, 2_000);
    assert!(
        rows.iter().any(|row| row.account_id == alice.id),
        "alice missing from unified inbox"
    );
    assert!(
        rows.iter().any(|row| row.account_id == bob.id),
        "bob missing from unified inbox"
    );
    let inbox = inbox(&h.core, &alice.id);
    let folder_rows = h.core.messages(inbox.id, 0, 10);
    assert!(!folder_rows.is_empty());
    assert!(folder_rows.iter().all(|row| row.account_id == alice.id));
}

fn smtp_send(from: &str, to: &str, subject: &str) {
    use std::io::{Read, Write};
    let port = env_port("WBMC_SMTP_PORT", 3025);
    let host = imap_host();
    let mut stream = std::net::TcpStream::connect((host.as_str(), port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut banner = [0_u8; 256];
    let _ = stream.read(&mut banner);
    let body = format!(
        "EHLO localhost\r\nMAIL FROM:<{from}>\r\nRCPT TO:<{to}>\r\nDATA\r\n\
         From: {from}\r\nTo: {to}\r\nSubject: {subject}\r\n\
         Date: Tue, 20 Oct 2026 15:00:00 +0000\r\n\r\n{subject}\r\n.\r\nQUIT\r\n"
    );
    stream.write_all(body.as_bytes()).unwrap();
    let mut response = Vec::new();
    let _ = stream.read_to_end(&mut response);
    let text = String::from_utf8_lossy(&response);
    assert!(
        text.contains("250") || text.contains("354"),
        "smtp rejected the message: {text}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn empty_username_tries_detected_alternative() {
    if !imap_tests_enabled() {
        return;
    }
    let h = harness();
    let mut blank = input("password");
    blank.username.clear();
    let account = h.core.add_imap_account(blank).await.unwrap();
    assert_eq!(account.email, "alice@wbmc.test");
    let stored = h.core.inner.store.accounts().unwrap();
    assert_eq!(stored[0].config["username"], "alice@wbmc.test");
    wait_for_sync(&h.core, &account.id).await;
}

#[tokio::test]
async fn resign_in_during_failing_sync_ends_idle() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, ResponseTemplate};

    let (h, server) = oauth_harness().await;
    // First code exchange is the original sign-in. Everything after that, including
    // refresh, succeeds with the new access token.
    let token_calls = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .and(path("/token"))
        .respond_with(move |_: &wiremock::Request| {
            let call = token_calls.fetch_add(1, Ordering::SeqCst);
            let (access, refresh) = if call == 0 {
                ("old-access", "old-refresh")
            } else {
                ("new-access", "new-refresh")
            };
            ResponseTemplate::new(200).set_body_json(token_json(access, refresh))
        })
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/gmail/v1/users/me/profile"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            serde_json::json!({"emailAddress": "real@gmail.com", "historyId": "100"}),
        ))
        .mount(&server)
        .await;
    let inbox = serde_json::json!({"labels":[
        {"id":"INBOX","name":"INBOX","type":"system"}
    ]});
    // Held open across re-sign-in so the stale sync emits Syncing{done} afterwards.
    Mock::given(method("GET"))
        .and(path("/gmail/v1/users/me/labels"))
        .and(header("authorization", "Bearer old-access"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(5))
                .set_body_json(inbox.clone()),
        )
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/gmail/v1/users/me/labels"))
        .respond_with(ResponseTemplate::new(200).set_body_json(inbox))
        .with_priority(5)
        .mount(&server)
        .await;
    let message_lists = Arc::new(AtomicUsize::new(0));
    Mock::given(method("GET"))
        .and(path("/gmail/v1/users/me/messages"))
        .respond_with(move |_: &wiremock::Request| {
            let call = message_lists.fetch_add(1, Ordering::SeqCst);
            let body = if call == 0 {
                serde_json::json!({"messages":[{"id":"m1"}]})
            } else {
                serde_json::json!({"messages":[]})
            };
            ResponseTemplate::new(200).set_body_json(body)
        })
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/gmail/v1/users/me/messages/m1"))
        .and(query_param("format", "metadata"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "m1",
            "internalDate": "1700000000123",
            "labelIds": ["INBOX"],
            "snippet": "hi",
            "payload": {"headers": [{"name": "Subject", "value": "Hi"}]}
        })))
        .mount(&server)
        .await;
    // After the progress event, the stale sync fails. The fresh sync lists no messages.
    Mock::given(method("GET"))
        .and(path("/gmail/v1/users/me/messages/m1"))
        .and(query_param("format", "raw"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;

    let account = finish_google(&h.core, "typed@gmail.com").await;
    let started = Instant::now();
    loop {
        let requests = server.received_requests().await.expect("request log");
        let labels_in_flight = requests.iter().any(|request| {
            request.url.path() == "/gmail/v1/users/me/labels"
                && request
                    .headers
                    .get("authorization")
                    .and_then(|value| value.to_str().ok())
                    == Some("Bearer old-access")
        });
        if labels_in_flight {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "stale sync did not request labels"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let again = finish_google(&h.core, "typed@gmail.com").await;
    assert_eq!(again.id, account.id);
    assert!(
        h.core.is_syncing(&account.id),
        "stale sync ended before re-sign-in returned"
    );
    let during_sign_in = server.received_requests().await.expect("request log");
    assert!(
        !during_sign_in
            .iter()
            .any(|request| request.url.path() == "/gmail/v1/users/me/messages"),
        "stale sync listed messages before re-sign-in returned"
    );

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let status = h
            .core
            .accounts()
            .first()
            .map(|account| account.status.clone());
        let syncing = h.core.is_syncing(&account.id);
        let requests = server.received_requests().await.expect("request log");
        let saw_new_token = requests.iter().any(|request| {
            request.url.path() == "/gmail/v1/users/me/labels"
                && request
                    .headers
                    .get("authorization")
                    .and_then(|value| value.to_str().ok())
                    == Some("Bearer new-access")
        });
        if status.as_deref() == Some("idle") && !syncing && saw_new_token {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "status={status:?} syncing={syncing} saw_new_token={saw_new_token} events={:?}",
            h.events.events()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(h.core.accounts().len(), 1);
    assert_eq!(h.core.accounts()[0].status, "idle");
    assert!(!h.core.is_syncing(&account.id));
    assert!(
        !h.events.events().iter().any(|event| matches!(
            event,
            FfiEvent::SyncStatus {
                status: FfiSyncStatus::NeedsSignIn,
                ..
            }
        )),
        "stale NeedsSignIn reached the listener: {:?}",
        h.events.events()
    );
}
