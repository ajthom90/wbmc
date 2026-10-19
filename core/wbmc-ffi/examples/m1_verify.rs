// This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0. If a copy of the MPL was not distributed with this file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Headless M1 measurements against a local IMAP server.
//!
//! ```text
//! m1_verify [--host H] [--port P]                  # normal seed
//! m1_verify [--host H] [--port P] --big            # 50k mailbox
//! m1_verify --offline <dir>                        # reopen a data dir; no sync
//! ```
//!
//! Host and port default to `WBMC_IMAP_HOST` / `WBMC_IMAP_PORT`, then
//! `127.0.0.1:3143`. Prints one JSON object per run on stdout.
//! `WBMC_SECRETS=memory` is set before the core opens its secret store.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use wbmc_ffi::{
    AccountInfo, CoreListener, FfiEvent, FfiSyncStatus, FolderInfo, ImapAccountInput,
    MessageContent, MessageRow, WbmcCore,
};

const FIRST_PAGE_TARGET_MS: u64 = 3_000;
const FIRST_PAGE_ROWS: u32 = 50;
/// `seed.py` numbers generated messages from 0 and formats the subject with
/// `{index:04d}`, so the newest normal seed is 0499 and the newest `--big`
/// seed is 49999.
const NEWEST_DEFAULT: &str = "Seed message 0499";
const NEWEST_BIG: &str = "Seed message 49999";
/// Decoded subject of `dev/mail/corpus/rfc2047_subject.eml`.
const CAFE_SUBJECT: &str = "Café ☕ meeting";
const OFFLINE_TARGET_MS: u64 = 1_000;
const RSS_TARGET_BYTES: u64 = 500 * 1024 * 1024;
const ONLINE_TIMEOUT: Duration = Duration::from_secs(45 * 60);
const POLL: Duration = Duration::from_millis(10);

struct Cli {
    big: bool,
    offline: Option<String>,
    host: String,
    port: u16,
}

fn main() {
    let started = Instant::now();
    force_memory_secrets();
    let cli = parse_cli();
    let result = if let Some(dir) = cli.offline {
        run_offline(&dir, started)
    } else {
        run_online(cli.big, &cli.host, cli.port)
    };
    finish(result);
}

fn parse_cli() -> Cli {
    let mut big = false;
    let mut offline = None;
    let mut host = None;
    let mut port = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--big" => big = true,
            "--offline" => offline = Some(args.next().unwrap_or_else(|| usage())),
            "--host" => host = Some(args.next().unwrap_or_else(|| usage())),
            "--port" => {
                let raw = args.next().unwrap_or_else(|| usage());
                port = Some(raw.parse().unwrap_or_else(|_| usage()));
            }
            "--help" | "-h" => usage(),
            other => {
                eprintln!("unknown argument {other:?}");
                usage();
            }
        }
    }
    if big && offline.is_some() {
        usage();
    }
    Cli {
        big,
        offline,
        host: host.unwrap_or_else(|| env_string("WBMC_IMAP_HOST", "127.0.0.1")),
        port: port.unwrap_or_else(|| env_port("WBMC_IMAP_PORT", 3143)),
    }
}

fn usage() -> ! {
    eprintln!("usage: m1_verify [--host HOST] [--port PORT] [--big | --offline <data dir>]");
    std::process::exit(2);
}

fn finish(result: Value) {
    let pass = result.get("pass").and_then(Value::as_bool).unwrap_or(false);
    println!("{result}");
    std::process::exit(if pass { 0 } else { 1 });
}

/// The secret store is chosen when the core is constructed.
fn force_memory_secrets() {
    // SAFETY: called once from `main` before any other thread exists.
    // `set_var` races with concurrent environment reads.
    unsafe { std::env::set_var("WBMC_SECRETS", "memory") };
}

struct Listener {
    state: Mutex<ListenState>,
}

#[derive(Clone)]
struct ListenState {
    origin: Option<Instant>,
    idle: Option<Duration>,
    error: Option<String>,
    saw_sync: bool,
}

impl Listener {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(ListenState {
                origin: None,
                idle: None,
                error: None,
                saw_sync: false,
            }),
        })
    }

    fn begin(&self) {
        self.lock().origin = Some(Instant::now());
    }

    fn snapshot(&self) -> ListenState {
        self.lock().clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ListenState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl CoreListener for Listener {
    fn on_event(&self, event: FfiEvent) {
        let mut state = self.lock();
        let Some(origin) = state.origin else {
            return;
        };
        if let FfiEvent::SyncStatus { status, .. } = event {
            state.saw_sync = true;
            match status {
                FfiSyncStatus::Idle => {
                    if state.idle.is_none() {
                        state.idle = Some(origin.elapsed());
                    }
                }
                FfiSyncStatus::Error { message } => state.error = Some(message),
                FfiSyncStatus::NeedsSignIn => state.error = Some("needs_sign_in".into()),
                FfiSyncStatus::Syncing { .. } => {}
            }
        }
    }
}

#[derive(Clone)]
struct FirstPage {
    elapsed: Duration,
    newest_first: bool,
    expected_subject: String,
    first_subject: String,
    subject_ok: bool,
    page_len: usize,
    page_len_ok: bool,
}

fn run_online(big: bool, host: &str, port: u16) -> Value {
    let dir = tempfile::tempdir().expect("temp data dir");
    let data_dir = dir.keep();
    eprintln!(
        "data dir kept at {}; delete it when you are done",
        data_dir.display()
    );
    let listener = Listener::new();
    let core = match WbmcCore::new(data_dir.display().to_string(), listener.clone()) {
        Ok(core) => core,
        Err(error) => {
            return failed(big, &data_dir, error.to_string());
        }
    };

    let first = Arc::new(Mutex::new(None));
    let cancel = Arc::new(AtomicBool::new(false));
    let watcher = {
        let core = Arc::clone(&core);
        let listener = Arc::clone(&listener);
        let first = Arc::clone(&first);
        let cancel = Arc::clone(&cancel);
        let expected = expected_newest(big);
        thread::spawn(move || watch_first_page(core, listener, first, cancel, expected))
    };

    listener.begin();
    let added = match futures::executor::block_on(core.add_imap_account(imap_input(host, port))) {
        Ok(account) => account,
        Err(error) => {
            cancel.store(true, Ordering::Relaxed);
            return failed(big, &data_dir, error.to_string());
        }
    };

    let sync = wait_for_sync(&core, &listener, &added);
    cancel.store(true, Ordering::Relaxed);
    let _ = watcher.join();
    let first_page = first.lock().unwrap_or_else(|p| p.into_inner()).clone();
    let rss = peak_rss_bytes();

    let mut result = json!({
        "mode": mode_name(big),
        "imap_host": host,
        "imap_port": port,
        "data_dir": data_dir,
        "account_id": added.id,
        "email": added.email,
        "peak_rss_bytes": rss,
        "peak_rss_mib": rss / (1024 * 1024),
        "ru_maxrss_unit": "bytes",
    });

    let Some(page) = first_page else {
        result["pass"] = json!(false);
        result["error"] = json!("inbox first page was not available before sync ended");
        if let Err(error) = &sync {
            result["sync_error"] = json!(error);
        }
        return result;
    };
    let first_page_ms = ms(page.elapsed);
    result["first_page_ms"] = json!(first_page_ms);
    result["first_page_under_3s"] = json!(first_page_ms <= FIRST_PAGE_TARGET_MS);
    result["newest_first"] = json!(page.newest_first);
    result["expected_newest"] = json!(page.expected_subject);
    result["newest_subject"] = json!(page.first_subject);
    result["newest_subject_ok"] = json!(page.subject_ok);
    result["first_page_len"] = json!(page.page_len);
    result["first_page_len_ok"] = json!(page.page_len_ok);

    if let Err(error) = sync {
        result["pass"] = json!(false);
        result["sync_error"] = json!(error);
        return result;
    }
    let backfill_ms = listener
        .snapshot()
        .idle
        .map(ms)
        .expect("idle sync recorded a duration");
    result["backfill_ms"] = json!(backfill_ms);

    let Some(folder) = inbox(&core, &added.id) else {
        result["pass"] = json!(false);
        result["error"] = json!("no inbox after sync");
        return result;
    };
    result["inbox_count"] = json!(folder.total_count);
    result["inbox_at_least_500"] = json!(folder.total_count >= 500);

    if big {
        let large = folder.total_count >= 10_000;
        let under = rss < RSS_TARGET_BYTES;
        result["large_mailbox"] = json!(large);
        result["under_500mb"] = json!(under);
        result["pass"] = json!(
            page.newest_first
                && page.subject_ok
                && page.page_len_ok
                && large
                && under
                && first_page_ms <= FIRST_PAGE_TARGET_MS
        );
        return result;
    }

    let corpus = corpus_checks(&core, &folder);
    let corpus_ok = corpus["pass"].as_bool().unwrap_or(false);
    result["corpus"] = corpus;
    result["pass"] = json!(
        page.newest_first
            && page.subject_ok
            && page.page_len_ok
            && folder.total_count >= 500
            && first_page_ms <= FIRST_PAGE_TARGET_MS
            && corpus_ok
    );
    result
}

fn failed(big: bool, data_dir: &Path, error: String) -> Value {
    json!({
        "mode": mode_name(big),
        "pass": false,
        "error": error,
        "data_dir": data_dir,
    })
}

fn mode_name(big: bool) -> &'static str {
    if big { "big" } else { "default" }
}

fn expected_newest(big: bool) -> &'static str {
    if big { NEWEST_BIG } else { NEWEST_DEFAULT }
}

fn watch_first_page(
    core: Arc<WbmcCore>,
    listener: Arc<Listener>,
    slot: Arc<Mutex<Option<FirstPage>>>,
    cancel: Arc<AtomicBool>,
    expected: &str,
) {
    let deadline = Instant::now() + ONLINE_TIMEOUT;
    while Instant::now() < deadline && !cancel.load(Ordering::Relaxed) {
        let Some(origin) = listener.snapshot().origin else {
            thread::sleep(POLL);
            continue;
        };
        if let Some(account) = core.accounts().into_iter().next()
            && let Some(folder) = inbox(&core, &account.id)
        {
            let rows = core.messages(folder.id, 0, FIRST_PAGE_ROWS);
            if let Some(page) = ready_page(&rows, folder.total_count, expected, origin.elapsed()) {
                *slot.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(page);
                return;
            }
        }
        thread::sleep(POLL);
    }
}

/// The clock stops only once the visible page is the newest mail and is long
/// enough: at least `min(50, total)` rows, not the first non-empty read.
fn ready_page(
    rows: &[MessageRow],
    total: u32,
    expected: &str,
    elapsed: Duration,
) -> Option<FirstPage> {
    if total == 0 || rows.is_empty() {
        return None;
    }
    let need = usize::try_from(total.min(FIRST_PAGE_ROWS)).unwrap_or(usize::MAX);
    let subject_ok = rows[0].subject == expected;
    let page_len_ok = rows.len() >= need;
    let newest_first = is_newest_first(rows);
    if !subject_ok || !page_len_ok || !newest_first {
        return None;
    }
    Some(FirstPage {
        elapsed,
        newest_first,
        expected_subject: expected.to_string(),
        first_subject: rows[0].subject.clone(),
        subject_ok,
        page_len: rows.len(),
        page_len_ok,
    })
}

fn wait_for_sync(
    core: &WbmcCore,
    listener: &Listener,
    account: &AccountInfo,
) -> Result<(), String> {
    let deadline = Instant::now() + ONLINE_TIMEOUT;
    let mut last_log = Instant::now();
    loop {
        let state = listener.snapshot();
        if let Some(error) = state.error {
            return Err(error);
        }
        if state.idle.is_some() {
            return Ok(());
        }
        if Instant::now() > deadline {
            return Err("sync timed out".into());
        }
        if last_log.elapsed() >= Duration::from_secs(5) {
            let count = inbox(core, &account.id).map(|folder| folder.total_count);
            let elapsed = state.origin.map(|origin| origin.elapsed().as_secs());
            eprintln!("syncing inbox_count={count:?} elapsed_s={elapsed:?}");
            last_log = Instant::now();
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn is_newest_first(rows: &[MessageRow]) -> bool {
    rows.windows(2)
        .all(|pair| pair[0].date_ms >= pair[1].date_ms)
}

fn inbox(core: &WbmcCore, account_id: &str) -> Option<FolderInfo> {
    core.folders(account_id.to_string())
        .into_iter()
        .find(|folder| folder.role == "inbox")
}

fn corpus_checks(core: &WbmcCore, folder: &FolderInfo) -> Value {
    let cafe = find_subject(core, &folder.id, folder.total_count, "Café");
    let latin = find_subject(core, &folder.id, folder.total_count, "Latin1");
    let sjis = find_subject(core, &folder.id, folder.total_count, "Shift-JIS");
    let inline = find_subject(core, &folder.id, folder.total_count, "Inline image");
    let remote = find_subject(core, &folder.id, folder.total_count, "Remote content");

    let cafe_ok = cafe.as_ref().is_some_and(|row| row.subject == CAFE_SUBJECT);
    let latin_body = body_text(core, latin.as_ref());
    let sjis_body = body_text(core, sjis.as_ref());
    let inline_html = body_html(core, inline.as_ref());
    let remote_html = body_html(core, remote.as_ref());

    let latin_ok = latin_body
        .as_deref()
        .is_some_and(|text| text.contains("Grüße"));
    let sjis_ok = sjis_body
        .as_deref()
        .is_some_and(|text| text.contains("こんにちは世界"));
    let inline_ok = inline_html
        .as_deref()
        .is_some_and(|html| html.contains("data:image/png;base64,"));
    let remote_urls = remote_html.as_deref().is_some_and(|html| {
        html.contains("https://tracker.example.com/p.gif")
            && html.contains("https://evil.example.com")
    });
    let remote_not_inlined = remote_html
        .as_deref()
        .is_some_and(|html| !html.contains("data:"));
    let pass = cafe_ok && latin_ok && sjis_ok && inline_ok && remote_urls && remote_not_inlined;

    json!({
        "pass": pass,
        "cafe_subject": cafe.as_ref().map(|row| row.subject.clone()),
        "cafe_ok": cafe_ok,
        "latin1_ok": latin_ok,
        "latin1_excerpt": excerpt(latin_body.as_deref()),
        "shift_jis_ok": sjis_ok,
        "shift_jis_excerpt": excerpt(sjis_body.as_deref()),
        "inline_image_ok": inline_ok,
        "remote_urls_preserved": remote_urls,
        "remote_not_inlined": remote_not_inlined,
        "remote_fetch": "core HTML still has the https URLs and no data: rewrite; WKWebView blocking is an app test",
    })
}

fn find_subject(core: &WbmcCore, folder_id: &str, total: u32, needle: &str) -> Option<MessageRow> {
    let mut offset = 0u32;
    while offset < total.max(1) {
        let page = core.messages(folder_id.to_string(), offset, 100);
        if page.is_empty() {
            break;
        }
        if let Some(row) = page.into_iter().find(|row| row.subject.contains(needle)) {
            return Some(row);
        }
        offset = offset.saturating_add(100);
    }
    None
}

fn body_text(core: &WbmcCore, row: Option<&MessageRow>) -> Option<String> {
    content(core, row).and_then(|content| content.text)
}

fn body_html(core: &WbmcCore, row: Option<&MessageRow>) -> Option<String> {
    content(core, row).and_then(|content| content.html)
}

fn content(core: &WbmcCore, row: Option<&MessageRow>) -> Option<MessageContent> {
    let row = row?;
    match futures::executor::block_on(core.message_content(row.id.clone())) {
        Ok(content) => Some(content),
        Err(error) => {
            eprintln!("message_content {}: {error}", row.subject);
            None
        }
    }
}

fn excerpt(text: Option<&str>) -> Option<String> {
    text.map(|text| {
        let mut short: String = text.chars().take(80).collect();
        if text.chars().count() > 80 {
            short.push('…');
        }
        short
    })
}

fn run_offline(dir: &str, started: Instant) -> Value {
    let path = Path::new(dir);
    let listener = Listener::new();
    // `WbmcCore::new` opens the store and does not start a sync. With
    // `WBMC_SECRETS=memory` this process also has no IMAP password, so a cache
    // miss cannot open a connection. `sync_started` is still measured: any
    // sync status event, or an account left in "syncing".
    let core = match WbmcCore::new(dir.to_string(), listener.clone()) {
        Ok(core) => core,
        Err(error) => {
            return json!({
                "mode": "offline",
                "pass": false,
                "data_dir": dir,
                "error": error.to_string(),
                "elapsed_ms": ms(started.elapsed()),
            });
        }
    };
    let accounts = core.accounts();
    let account = accounts.first();
    let folder = account.and_then(|account| inbox(&core, &account.id));
    let page = folder
        .as_ref()
        .map(|folder| core.messages(folder.id.clone(), 0, 20))
        .unwrap_or_default();
    let body = page
        .first()
        .map(|row| futures::executor::block_on(core.message_content(row.id.clone())));
    let elapsed_ms = ms(started.elapsed());
    let cached_body_files = count_eml(path);
    let (body_readable, body_subject, body_error) = match &body {
        Some(Ok(content)) => (true, Some(content.subject.clone()), None),
        Some(Err(error)) => (false, None, Some(error.to_string())),
        None => (
            false,
            None,
            Some("no cached inbox message to open".to_string()),
        ),
    };
    let account_status = account.map(|account| account.status.clone());
    let sync_started = listener.snapshot().saw_sync || account_status.as_deref() == Some("syncing");
    let store_readable = !accounts.is_empty() && folder.is_some() && !page.is_empty();
    json!({
        "mode": "offline",
        "data_dir": dir,
        "account_status": account_status,
        "sync_started": sync_started,
        "elapsed_ms": elapsed_ms,
        "under_1000ms": elapsed_ms < OFFLINE_TARGET_MS && store_readable && body_readable && !sync_started,
        "accounts": accounts.len(),
        "inbox_count": folder.as_ref().map(|folder| folder.total_count),
        "first_page_len": page.len(),
        "store_readable": store_readable,
        "body_readable": body_readable,
        "body_subject": body_subject,
        "body_error": body_error,
        "cached_body_files": cached_body_files,
        "pass": elapsed_ms < OFFLINE_TARGET_MS && store_readable && body_readable && !sync_started,
    })
}

fn imap_input(host: &str, port: u16) -> ImapAccountInput {
    ImapAccountInput {
        email: "alice@wbmc.test".into(),
        display_name: "Alice".into(),
        host: host.to_string(),
        port,
        security: "none".into(),
        username: "alice@wbmc.test".into(),
        password: "password".into(),
        smtp_host: env_string("WBMC_SMTP_HOST", "127.0.0.1"),
        smtp_port: env_port("WBMC_SMTP_PORT", 3025),
        smtp_security: "none".into(),
    }
}

fn env_string(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

fn env_port(name: &str, default: u16) -> u16 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn count_eml(data_dir: &Path) -> u64 {
    let mut count = 0u64;
    let Ok(accounts) = std::fs::read_dir(data_dir.join("bodies")) else {
        return 0;
    };
    for account in accounts.flatten() {
        let Ok(files) = std::fs::read_dir(account.path()) else {
            continue;
        };
        count += files
            .flatten()
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "eml"))
            .count() as u64;
    }
    count
}

fn peak_rss_bytes() -> u64 {
    // SAFETY: `usage` is zeroed, and `getrusage` writes a valid `rusage` into it.
    let mut usage = unsafe { std::mem::zeroed::<libc::rusage>() };
    let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) };
    if rc != 0 {
        return 0;
    }
    // macOS and iOS report bytes. Other Unix platforms report kibibytes.
    let rss = u64::try_from(usage.ru_maxrss).unwrap_or(0);
    if cfg!(any(target_os = "macos", target_os = "ios")) {
        rss
    } else {
        rss.saturating_mul(1024)
    }
}

fn ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}
