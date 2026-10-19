// This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0. If a copy of the MPL was not distributed with this file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! GreenMail integration tests. They no-op unless `WBMC_TEST_IMAP=1`.
//! Mutating tests use a private mailbox so parallel runs can share one server.

use std::collections::HashSet;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_imap::Session;
use futures::TryStreamExt;
use tokio::net::TcpStream;
use wbmc_core::mime::parse_message;
use wbmc_core::model::FolderRole;
use wbmc_core::provider::{
    FolderSyncState, ImapConfig, ImapProvider, MailProvider, RemoteFolder, Security,
};

fn imap_tests_enabled() -> bool {
    matches!(std::env::var("WBMC_TEST_IMAP").as_deref(), Ok("1"))
}

fn imap_host() -> String {
    std::env::var("WBMC_IMAP_HOST").unwrap_or_else(|_| "127.0.0.1".into())
}

fn imap_port() -> u16 {
    std::env::var("WBMC_IMAP_PORT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(3143)
}

fn smtp_port() -> u16 {
    std::env::var("WBMC_SMTP_PORT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(3025)
}

fn test_config() -> ImapConfig {
    let host = imap_host();
    ImapConfig {
        host: host.clone(),
        port: imap_port(),
        security: Security::None,
        username: "alice@wbmc.test".into(),
        smtp_host: host,
        smtp_port: smtp_port(),
        smtp_security: Security::None,
    }
}

async fn connect_session() -> Session<TcpStream> {
    let stream = TcpStream::connect((imap_host(), imap_port()))
        .await
        .expect("tcp connect");
    let mut client = async_imap::Client::new(stream);
    client
        .read_response()
        .await
        .expect("greeting")
        .expect("greeting");
    client
        .login("alice@wbmc.test", "password")
        .await
        .map_err(|(err, _)| err)
        .expect("login")
}

async fn ready_provider() -> ImapProvider {
    let config = test_config();
    let started = std::time::Instant::now();
    loop {
        let provider = ImapProvider::new(config.clone(), "password".into());
        match provider.list_folders().await {
            Ok(_) => return ImapProvider::new(config, "password".into()),
            Err(err) if started.elapsed() < Duration::from_secs(30) => {
                eprintln!("waiting for imap: {err}");
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            Err(err) => panic!("imap not ready after 30s: {err}"),
        }
    }
}

async fn inbox(provider: &ImapProvider) -> RemoteFolder {
    provider
        .list_folders()
        .await
        .expect("list folders")
        .into_iter()
        .find(|folder| folder.remote_id.eq_ignore_ascii_case("INBOX"))
        .expect("INBOX")
}

async fn inbox_exists() -> u32 {
    let mut session = connect_session().await;
    let mailbox = session.select("INBOX").await.expect("select inbox");
    let exists = mailbox.exists;
    let _ = session.logout().await;
    exists
}

fn unique_mailbox() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    format!("WBMC-Test-{}-{nanos}", std::process::id())
}

fn rfc822(subject: &str, index: u32) -> Vec<u8> {
    let minute = index / 60;
    let second = index % 60;
    format!(
        "From: Tester <tester@wbmc.test>\r\n\
         To: Alice <alice@wbmc.test>\r\n\
         Date: 01 Nov 2026 00:{minute:02}:{second:02} +0000\r\n\
         Subject: {subject}\r\n\
         Message-ID: <{index}-{subject}@wbmc.test>\r\n\
         MIME-Version: 1.0\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\
         \r\n\
         Body {index} {subject}\r\n"
    )
    .into_bytes()
}

async fn append_message(mailbox: &str, subject: &str, index: u32) {
    let mut session = connect_session().await;
    session
        .append(mailbox, None, None, rfc822(subject, index))
        .await
        .expect("append");
    let _ = session.logout().await;
}

async fn backfill(
    provider: &ImapProvider,
    folder: &RemoteFolder,
    page_size: u32,
) -> FolderSyncState {
    let mut state = None;
    for _ in 0..100 {
        let delta = provider
            .sync_folder(folder, state, page_size)
            .await
            .expect("backfill page");
        state = Some(delta.new_state);
        if !delta.more {
            return state.expect("state");
        }
    }
    panic!("backfill did not finish");
}

#[tokio::test]
async fn lists_inbox_with_role() {
    if !imap_tests_enabled() {
        return;
    }
    let provider = ready_provider().await;
    let inbox = inbox(&provider).await;
    assert_eq!(inbox.role, FolderRole::Inbox);
    assert!(inbox.parent_remote_id.is_none());
}

#[tokio::test]
async fn first_page_is_newest() {
    if !imap_tests_enabled() {
        return;
    }
    let provider = ready_provider().await;
    let inbox = inbox(&provider).await;
    let delta = provider
        .sync_folder(&inbox, None, 100)
        .await
        .expect("first page");
    assert_eq!(delta.upserts.len(), 100);
    assert!(delta.more);
    assert!(!delta.reset);
    let newest = delta
        .upserts
        .iter()
        .max_by_key(|message| message.date_ms)
        .expect("page");
    assert_eq!(newest.subject, "Seed message 0499");
    // GreenMail omits the SP before a partial literal, so this snippet is only
    // present when the raw-connection fallback parsed BODY[TEXT]<0>{n}.
    assert!(
        newest.snippet.contains("Seed body 0499"),
        "snippet was {:?}",
        newest.snippet
    );
    assert!(delta.flag_updates.is_empty());
    assert!(delta.server_remote_ids.is_none());
}

#[tokio::test]
async fn backfill_until_done() {
    if !imap_tests_enabled() {
        return;
    }
    let provider = ready_provider().await;
    let inbox = inbox(&provider).await;
    let mut state = None;
    let mut seen = HashSet::new();
    for _ in 0..100 {
        let delta = provider
            .sync_folder(&inbox, state, 100)
            .await
            .expect("page");
        if delta.reset {
            seen.clear();
        }
        for message in delta.upserts {
            assert!(
                seen.insert(message.remote_id.clone()),
                "duplicate {}",
                message.remote_id
            );
        }
        state = Some(delta.new_state);
        if !delta.more {
            break;
        }
    }
    let exists = inbox_exists().await;
    assert_eq!(seen.len() as u32, exists);
    assert!(exists > 500, "expected the seeded inbox, got {exists}");
}

#[tokio::test]
async fn incremental_picks_up_new_mail() {
    if !imap_tests_enabled() {
        return;
    }
    let mailbox = unique_mailbox();
    let mut session = connect_session().await;
    session.create(&mailbox).await.expect("create");
    for index in 0..5 {
        session
            .append(
                &mailbox,
                None,
                None,
                rfc822(&format!("existing {index}"), index),
            )
            .await
            .expect("append existing");
    }
    let _ = session.logout().await;

    let provider = ready_provider().await;
    let folder = RemoteFolder {
        remote_id: mailbox.clone(),
        name: mailbox.clone(),
        role: FolderRole::Other,
        parent_remote_id: None,
    };
    let state = backfill(&provider, &folder, 2).await;
    append_message(&mailbox, "WBMC incremental new", 90).await;

    let delta = provider
        .sync_folder(&folder, Some(state), 50)
        .await
        .expect("incremental");
    assert!(!delta.reset);
    assert!(!delta.more);
    assert_eq!(delta.upserts.len(), 1);
    assert_eq!(delta.upserts[0].subject, "WBMC incremental new");
    assert!(delta.deleted_remote_ids.is_empty());
}

#[tokio::test]
async fn detects_deletion() {
    if !imap_tests_enabled() {
        return;
    }
    let mailbox = unique_mailbox();
    let mut session = connect_session().await;
    session.create(&mailbox).await.expect("create");
    for index in 0..3 {
        session
            .append(
                &mailbox,
                None,
                None,
                rfc822(&format!("keep {index}"), index),
            )
            .await
            .expect("append");
    }
    let _ = session.logout().await;

    let provider = ready_provider().await;
    let folder = RemoteFolder {
        remote_id: mailbox.clone(),
        name: mailbox.clone(),
        role: FolderRole::Other,
        parent_remote_id: None,
    };
    let mut state = None;
    let mut synced = Vec::new();
    loop {
        let delta = provider
            .sync_folder(&folder, state, 10)
            .await
            .expect("backfill");
        synced.extend(delta.upserts);
        state = Some(delta.new_state);
        if !delta.more {
            break;
        }
    }
    let target = synced
        .iter()
        .find(|message| message.subject == "keep 1")
        .expect("seeded message")
        .remote_id
        .clone();

    let mut session = connect_session().await;
    session.select(&mailbox).await.expect("select");
    session
        .uid_store(&target, "+FLAGS (\\Deleted)")
        .await
        .expect("uid store")
        .try_collect::<Vec<_>>()
        .await
        .expect("store response");
    session
        .expunge()
        .await
        .expect("expunge")
        .try_collect::<Vec<_>>()
        .await
        .expect("expunge response");
    let _ = session.logout().await;

    let delta = provider
        .sync_folder(&folder, state, 10)
        .await
        .expect("sync after delete");
    assert!(delta.deleted_remote_ids.is_empty());
    let present = delta
        .server_remote_ids
        .expect("final incremental page lists server uids");
    assert!(
        !present.iter().any(|id| id == &target),
        "deleted uid {target} still in {present:?}"
    );
    for message in synced.iter().filter(|message| message.remote_id != target) {
        assert!(
            present.iter().any(|id| id == &message.remote_id),
            "kept uid {} missing from {present:?}",
            message.remote_id
        );
    }
}

#[tokio::test]
async fn incremental_reports_flag_updates() {
    if !imap_tests_enabled() {
        return;
    }
    let mailbox = unique_mailbox();
    let mut session = connect_session().await;
    session.create(&mailbox).await.expect("create");
    for index in 0..2 {
        session
            .append(
                &mailbox,
                None,
                None,
                rfc822(&format!("flag {index}"), index),
            )
            .await
            .expect("append");
    }
    let _ = session.logout().await;

    let provider = ready_provider().await;
    let folder = RemoteFolder {
        remote_id: mailbox.clone(),
        name: mailbox.clone(),
        role: FolderRole::Other,
        parent_remote_id: None,
    };
    let state = backfill(&provider, &folder, 10).await;

    let mut session = connect_session().await;
    session.select(&mailbox).await.expect("select");
    let uids: Vec<u32> = session
        .uid_search("ALL")
        .await
        .expect("search")
        .into_iter()
        .collect();
    let target = uids[0].to_string();
    session
        .uid_store(&target, "+FLAGS (\\Seen)")
        .await
        .expect("uid store")
        .try_collect::<Vec<_>>()
        .await
        .expect("store response");
    let _ = session.logout().await;

    let delta = provider
        .sync_folder(&folder, Some(state), 10)
        .await
        .expect("incremental");
    assert!(!delta.more);
    let update = delta
        .flag_updates
        .iter()
        .find(|update| update.remote_id == target)
        .expect("flag update");
    assert!(update.is_read);
}

#[tokio::test]
async fn fetch_raw_parses() {
    if !imap_tests_enabled() {
        return;
    }
    let provider = ready_provider().await;
    let inbox = inbox(&provider).await;
    let delta = provider.sync_folder(&inbox, None, 100).await.expect("page");
    let summary = delta
        .upserts
        .iter()
        .find(|message| message.subject == "Seed message 0499")
        .expect("newest seed message");
    let raw = provider
        .fetch_raw(&inbox.remote_id, &summary.remote_id)
        .await
        .expect("fetch raw");
    let parsed = parse_message(&raw, 0);
    assert_eq!(parsed.subject, summary.subject);
}
