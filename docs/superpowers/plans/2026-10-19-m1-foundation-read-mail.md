# M1: Foundation + Read Mail Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A macOS app, built on a shared Rust core, that can sign in to Microsoft 365 or a generic IMAP server, sync folders and messages into a local store, and show them in a native three-pane window. Remote content in messages is blocked.

**Architecture:**
- `core/wbmc-core` holds everything platform-neutral:
  - model types;
  - the SQLite store;
  - MIME parsing;
  - a `MailProvider` trait with IMAP and Microsoft Graph implementations;
  - a sync engine;
  - secrets and OAuth.
- `core/wbmc-ffi` exposes one UniFFI object, `WbmcCore`, to Swift.
- `apps/apple` is an XcodeGen project whose SwiftUI app calls `WbmcCore` through a generated Swift package.

Raw MIME is the single body format for every provider: IMAP `BODY.PEEK[]` and Graph `/messages/{id}/$value` both deliver it.

**Tech Stack:**
- Rust (stable; edition 2024) and tokio. Crates:
  - `rusqlite` (bundled), `async-imap` + `tokio-rustls`, `mail-parser`, `reqwest` (rustls);
  - `serde`, `thiserror`, `uuid` (v7), `keyring`, `sha2`, `base64`, `tracing`, `uniffi`;
  - dev only: `wiremock`, `tempfile`.
- Swift 6 and SwiftUI on macOS 26+, plus WebKit (`WKWebView`).
- XcodeGen.
- Docker GreenMail as the test IMAP/SMTP server.
- GitHub Actions for CI.

**Spec:** `docs/superpowers/specs/2026-10-19-wbmc-design.md`. Roadmap: `docs/superpowers/plans/2026-10-19-phase1-roadmap.md`.

## Global Constraints

- License: **MPL-2.0** for every source file. Add the MPL header comment to each new `.rs` and `.swift` file.
- Minimum OS: **macOS 26+**. Development host: macOS 27, Apple Silicon.
- Bundle identifier: `io.github.ajthom90.wbmc` (Claude's call; brand-neutral until a domain exists).
- Default layout: **refined three-pane** (sidebar → message list → reading pane), with a **native macOS look**: system materials, sidebar style, system accent color, no custom chrome.
- No WBMC server ever receives credentials or mail content. Credentials live **only** in the OS keychain (`keyring` crate, service `io.github.ajthom90.wbmc`).
- HTML mail: **JavaScript off, and no remote loads** (http/https blocked). Only `cid:` and `data:` images render.
- Local-first: every synced item is written to the store before the UI sees it. No data is held only in memory.
- Microsoft 365 uses **Graph only**; never EWS.
- Tests and agents touch **only dedicated test users and mailboxes** in the owner's M365 tenant.
- Never commit secrets. The Entra client ID is public and may be committed; tokens and passwords may not.
- Before each batch of builder dispatches, check usage with the `openusage-budget` skill. Pace work so no subscription is drained in a day.

## Review Focus

1. **Badly encoded or malformed mail** (RFC 2047 headers, ISO-8859-1/Shift-JIS bodies, broken MIME, missing Date or From) must show readable text with fallbacks ("(no subject)", "Unknown sender", received date) and never crash or skip the message. *Pinned in Task 4.*
2. **An IMAP UIDVALIDITY change** must drop that folder's cached messages and resync, with no duplicates and no stale messages. *Pinned in Task 6.*
3. **A network drop or expired token mid-sync** must resume from the last saved page. An expired access token refreshes silently. A revoked refresh token marks the account `NeedsSignIn`, with no crash or retry loop. *Pinned in Tasks 6 and 8.*
4. **Large folders (50k+ messages)** must show the newest page quickly, list paging must keep memory bounded, and the UI never loads a whole folder. *Pinned in Tasks 3 and 6.*
5. **HTML with remote images, scripts or forms** must load nothing remote and run no script. *Pinned in Task 10.*

---

## File Structure

```
LICENSE                          MPL-2.0 text
README.md                        what WBMC is, how to build
rust-toolchain.toml              channel = "stable"
Cargo.toml                       workspace: core/wbmc-core, core/wbmc-ffi
.github/workflows/ci.yml         fmt, clippy, tests (+ GreenMail service on Linux)
config/oauth-clients.toml        public OAuth client IDs (Microsoft)
dev/mail/docker-compose.yml      GreenMail test server
dev/mail/seed.py                 sends a fixed corpus into the test server
dev/mail/corpus/                 .eml fixtures used by seed.py and MIME tests
scripts/dev-mail-up.sh           start + seed the test server
scripts/build-apple.sh           build Rust for Apple, generate Swift bindings, xcframework
docs/setup/microsoft-entra-app.md   owner's step-by-step Entra registration
docs/setup/test-accounts.md         test users/mailboxes; safety rules
core/wbmc-core/src/
  lib.rs                         module wiring
  error.rs                       CoreError
  model.rs                       ids, Account, Folder, FolderRole, MessageSummary, Address
  store/mod.rs                   Store (open, migrations, queries)
  store/schema.sql               SQLite schema v1
  store/bodies.rs                BodyStore: raw MIME files on disk
  mime.rs                        parse_message(raw) -> ParsedMessage
  provider/mod.rs                MailProvider trait + RemoteFolder/RemoteMessage/FolderDelta/FolderSyncState
  provider/imap.rs               ImapProvider
  provider/graph.rs              GraphProvider
  provider/fake.rs               FakeProvider (cfg(test) + feature "test-util")
  secrets.rs                     SecretStore trait, KeyringSecrets, MemorySecrets
  auth/mod.rs                    OAuth types, TokenSet, TokenManager
  auth/pkce.rs                   PKCE verifier/challenge
  auth/loopback.rs               one-shot localhost redirect listener
  auth/microsoft.rs              Microsoft endpoints, scopes, code exchange, refresh
  sync.rs                        SyncEngine
  events.rs                      CoreEvent, EventSink
core/wbmc-core/tests/
  imap_greenmail.rs              integration (WBMC_TEST_IMAP=1)
  graph_live.rs                  integration (WBMC_TEST_M365=1), #[ignore]
core/wbmc-ffi/src/lib.rs         WbmcCore UniFFI object + FFI records
core/wbmc-ffi/src/bin/uniffi-bindgen.rs
apps/apple/project.yml           XcodeGen spec (target WBMC-macOS, tests)
apps/apple/WBMCCoreKit/          Swift package wrapping the xcframework + generated Swift
apps/apple/Shared/AppModel.swift         @Observable model over CoreClient
apps/apple/Shared/CoreClient.swift       protocol = generated WbmcCoreProtocol
apps/apple/macOS/WBMCApp.swift
apps/apple/macOS/Views/{Sidebar,MessageList,MessageView,AddAccount}.swift
apps/apple/macOS/Views/SafeHTMLView.swift  locked-down WKWebView
apps/apple/Tests/AppModelTests.swift
apps/apple/UITests/LaunchTests.swift
```

---

### Task 1: Repository foundation + CI

**Files:**
- Create: `LICENSE`, `README.md`, `rust-toolchain.toml`, `Cargo.toml`, `core/wbmc-core/Cargo.toml`, `core/wbmc-core/src/lib.rs`, `core/wbmc-core/src/error.rs`, `.github/workflows/ci.yml`
- Modify: `.gitignore` (add `target/`, `*.xcframework`, `apps/apple/*.xcodeproj`, `apps/apple/WBMCCoreKit/Sources/WBMCCore/Generated/`, `.DS_Store`)

**Interfaces:**
- Produces: `wbmc_core::error::CoreError`, an enum with these variants:
  - `Network(String)`, `Protocol(String)`, `Storage(String)`;
  - `NeedsSignIn { account_id: String }`;
  - `NotFound(String)`, `InvalidInput(String)`.
- Produces: `pub type Result<T> = std::result::Result<T, CoreError>`.

- [ ] **Step 1:** Run `rustup update stable`. Then create `rust-toolchain.toml` with `channel = "stable"` and `components = ["rustfmt","clippy"]`, and add the targets `aarch64-apple-darwin` and `x86_64-apple-darwin`.
- [ ] **Step 2: Write the failing test** in `error.rs`:
  ```rust
  #[test]
  fn needs_sign_in_displays_account() {
      let e = CoreError::NeedsSignIn { account_id: "a1".into() };
      assert_eq!(e.to_string(), "account a1 needs to sign in again");
  }
  ```
- [ ] **Step 3:** Run `cargo test -p wbmc-core`. Expected: FAIL, because the type is not defined.
- [ ] **Step 4:** Implement `CoreError` with `thiserror`. Add `From` conversions for `rusqlite::Error` → `Storage`, `std::io::Error` → `Storage`, and `reqwest::Error` → `Network`.
- [ ] **Step 5:** Run `cargo test -p wbmc-core`. Expected: PASS.
- [ ] **Step 6:** Write `ci.yml`. It has two jobs:
  - `rust-linux` on ubuntu-latest: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and `cargo test --workspace`, with a GreenMail service container, `WBMC_TEST_IMAP=1` and `WBMC_SECRETS=memory`.
  - `rust-macos` on macos-latest: `cargo test --workspace`.
  
  Add the `LICENSE` file (verbatim MPL-2.0 text) and a README with the one-paragraph pitch from spec §1 plus build instructions.
- [ ] **Step 7: Commit** with `git commit -m "chore: workspace, license, CI"`.

### Task 2: Owner setup guides (Entra app + test accounts)

The owner needs this early so the registration is ready by Task 8.

**Files:**
- Create: `docs/setup/microsoft-entra-app.md`, `docs/setup/test-accounts.md`, `config/oauth-clients.toml`

**Interfaces:**
- Produces: `config/oauth-clients.toml` with the following content. Task 7 loads it via `include_str!`.
  ```toml
  [microsoft]
  client_id = ""   # filled in by the owner
  authority = "https://login.microsoftonline.com/common"
  ```

- [ ] **Step 1:** Write `microsoft-entra-app.md` as numbered click-by-click steps in the Entra admin center:
  - **Register the app:**
    - name **WBMC**;
    - supported account types: "Accounts in any organizational directory and personal Microsoft accounts";
    - platform **Mobile and desktop applications**, with redirect URI `http://localhost`;
    - "Allow public client flows" = **Yes**.
  - **Delegated Graph permissions** (requested now so users never have to re-consent later): `offline_access`, `User.Read`, `Mail.ReadWrite`, `Mail.Send`, `MailboxSettings.ReadWrite`, `Calendars.ReadWrite`, `Contacts.ReadWrite`, `Tasks.ReadWrite`, `People.Read`.
  - **Admin consent:** how to grant it for the owner's tenant.
  - Where to copy the **Application (client) ID** into `config/oauth-clients.toml`.
- [ ] **Step 2:** Write `test-accounts.md`:
  - Create the test users `wbmc-test1@` and `wbmc-test2@`, a shared mailbox `wbmc-shared@`, and a room `wbmc-room@` in the owner's tenant.
  - Give the minimal license needed (Exchange Online Plan 1 or Business Basic).
  - State the rule: **automated tests and agents use only these mailboxes**.
  - List the env vars the live tests read: `WBMC_TEST_M365=1`, `WBMC_TEST_M365_USER`, `WBMC_TEST_M365_REFRESH_TOKEN`.
  - Explain how to capture the refresh token, using the debug menu item added in Task 10.
- [ ] **Step 3: Commit** with `git commit -m "docs: Entra app registration and test-account guides"`. Then tell the owner the guide is ready; registration runs in parallel with Tasks 3–7.

### Task 3: Model + SQLite store

**Files:**
- Create: `core/wbmc-core/src/model.rs`, `src/store/mod.rs`, `src/store/schema.sql`, `src/store/bodies.rs`

**Interfaces:**
- **Produces (model):**
  - `AccountId`, `FolderId`, `MessageId`: newtypes over `String` (uuid v7).
  - `enum ProviderKind { Imap, Microsoft }`.
  - `enum FolderRole { Inbox, Sent, Drafts, Trash, Archive, Junk, Other }`.
  - `struct Address { name: Option<String>, email: String }`.
  - `struct Account { id, kind: ProviderKind, email: String, display_name: String, config: serde_json::Value }`.
  - `struct Folder { id, account_id, remote_id: String, name: String, role: FolderRole, parent_id: Option<FolderId>, unread_count: u32, total_count: u32 }`.
  - `struct MessageSummary { id, account_id, folder_id, remote_id: String, subject: String, from: Address, to: Vec<Address>, cc: Vec<Address>, date_ms: i64, snippet: String, is_read: bool, is_flagged: bool, has_attachments: bool }`.
- **Produces (`Store`):**
  - `open(path: &Path) -> Result<Store>` (WAL mode, runs migrations);
  - `insert_account(&Account)`, `accounts() -> Vec<Account>`, `delete_account(&AccountId)` (cascade);
  - `upsert_folder(&Folder) -> Folder` (keyed on `(account_id, remote_id)`, keeping the existing id), `folders(&AccountId) -> Vec<Folder>`;
  - `folder_sync_state(&FolderId) -> Option<String>`, `set_folder_sync_state(&FolderId, &str)`;
  - `upsert_messages(&[MessageSummary])` (keyed on `(folder_id, remote_id)`), `delete_messages(&FolderId, &[String])`, `clear_folder(&FolderId)`;
  - `messages(&FolderId, offset: u32, limit: u32) -> Vec<MessageSummary>` (newest first);
  - `message(&MessageId) -> Option<MessageSummary>`;
  - `recount(&FolderId)`.
- **Produces:** `BodyStore::new(root: PathBuf)`, with `put(&AccountId, &MessageId, &[u8])`, `get(...) -> Option<Vec<u8>>`, `remove(...)`. Files live at `root/<account>/<message>.eml` and are written atomically (temp file + rename).

- [ ] **Step 1: Write failing tests** in `store/mod.rs`:
  - `upsert_messages_is_idempotent`: inserting the same `(folder, remote_id)` twice with a changed `is_read` leaves one row, with the new value.
  - `messages_are_newest_first_and_paged`: insert 250 messages with increasing `date_ms`. `messages(f, 0, 100)` returns 100 rows, the first with the largest date, and `messages(f, 200, 100)` returns 50.
  - `delete_account_cascades`: folders and messages are gone.
  - `upsert_folder_keeps_id`: the second upsert with the same remote_id returns the same `FolderId`.
  - `body_store_roundtrip_and_atomic`: `put` then `get` returns the same bytes, and no `.tmp` files remain.
- [ ] **Step 2:** Run `cargo test -p wbmc-core store`. Expected: FAIL to compile.
- [ ] **Step 3:** Implement the schema in `schema.sql`:
  - tables `accounts`, `folders`, `messages`, `schema_version`;
  - `messages` has `UNIQUE(folder_id, remote_id)` and an `INDEX(folder_id, date_ms DESC)`;
  - foreign keys `ON DELETE CASCADE`;
  - addresses stored as JSON text.
  
  Implement `Store` with a `Mutex<rusqlite::Connection>`.
- [ ] **Step 4:** Run `cargo test -p wbmc-core store`. Expected: PASS.
- [ ] **Step 5: Commit** with `git commit -m "feat(core): model types and SQLite store"`.

### Task 4: MIME parsing

**Files:**
- Create: `core/wbmc-core/src/mime.rs`, `dev/mail/corpus/*.eml`

**Interfaces:**
- Produces: `parse_message(raw: &[u8], fallback_date_ms: i64) -> ParsedMessage`. It is infallible and never panics.
- Produces: `ParsedMessage { subject: String, from: Address, to: Vec<Address>, cc: Vec<Address>, date_ms: i64, html: Option<String>, text: Option<String>, snippet: String, attachments: Vec<AttachmentMeta> }`.
- Produces: `AttachmentMeta { filename: String, mime_type: String, size: u64, content_id: Option<String>, inline: bool }`.
- Produces: `cid_part(raw: &[u8], content_id: &str) -> Option<(String /*mime*/, Vec<u8>)>`.

- [ ] **Step 1:** Create the corpus fixtures:
  - `plain.eml`, `html_with_inline_image.eml`, `rfc2047_subject.eml` (`=?UTF-8?B?...?=` → "Café ☕ meeting");
  - `latin1_body.eml`, `shift_jis.eml`;
  - `no_date_no_from.eml`, `truncated_multipart.eml`;
  - `attachment_pdf.eml`, `remote_images_and_script.eml`.
- [ ] **Step 2: Write failing tests**, one per fixture:
  - `rfc2047_subject` gives subject `"Café ☕ meeting"`;
  - `latin1_body` text contains `"Grüße"`;
  - `no_date_no_from` gives subject `"(no subject)"` when the header is missing, from email `""` with name `"Unknown sender"`, and `date_ms == fallback`;
  - `truncated_multipart` gives no panic and returns whatever text exists;
  - `snippet` is ≤ 200 chars, whitespace-collapsed, and taken from the text part, or from the HTML stripped of tags when there's no text part;
  - `attachment_pdf` gives one attachment with `mime_type == "application/pdf"`;
  - `cid_part` on `html_with_inline_image` returns `image/png` bytes.
- [ ] **Step 3:** Run `cargo test -p wbmc-core mime`. Expected: FAIL.
- [ ] **Step 4:** Implement with `mail-parser`, plus a regex-free tag stripper for snippets.
- [ ] **Step 5:** Run the tests. Expected: PASS.
- [ ] **Step 6: Commit** with `git commit -m "feat(core): MIME parsing with robust fallbacks"`.

### Task 5: Test mail server + IMAP provider

**Files:**
- Create: `dev/mail/docker-compose.yml`, `dev/mail/seed.py`, `scripts/dev-mail-up.sh`, `core/wbmc-core/src/provider/mod.rs`, `src/provider/imap.rs`, `tests/imap_greenmail.rs`

**Interfaces:**
- **Produces (provider types):**
  - the `#[async_trait] pub trait MailProvider: Send + Sync` with three methods:
    - `async fn list_folders(&self) -> Result<Vec<RemoteFolder>>`;
    - `async fn sync_folder(&self, folder: &RemoteFolder, state: Option<FolderSyncState>, page_size: u32) -> Result<FolderDelta>`;
    - `async fn fetch_raw(&self, folder_remote_id: &str, message_remote_id: &str) -> Result<Vec<u8>>`.
  - `RemoteFolder { remote_id: String, name: String, role: FolderRole, parent_remote_id: Option<String> }`.
  - `RemoteMessage { remote_id: String, subject, from, to, cc, date_ms, snippet, is_read, is_flagged, has_attachments }`.
  - `FolderDelta { upserts: Vec<RemoteMessage>, deleted_remote_ids: Vec<String>, new_state: FolderSyncState, more: bool, reset: bool }`. `reset` = the cache must be cleared before applying.
  - `#[serde(tag="kind")] enum FolderSyncState`, with two variants:
    - `Imap { uid_validity: u32, highest_uid: u32, lowest_uid: u32 }`;
    - `Graph { delta_link: Option<String>, next_link: Option<String> }`.
- **Produces:** `ImapConfig { host, port: u16, security: Security /*Tls|StartTls|None*/, username, smtp_host, smtp_port: u16, smtp_security: Security }`, which is serde and stored in `Account.config`.
- **Produces:** `ImapProvider::new(config: ImapConfig, password: String)`.
- **IMAP semantics:**
  - The first call with no state fetches the **newest** `page_size` UIDs (via `UID SEARCH ALL`) and returns `more = true` while older UIDs remain. Later calls with `more` page older messages, using `lowest_uid`.
  - Once backfilled, a call fetches UIDs > `highest_uid`, refreshes flags for known UIDs in batches, and reports UIDs that have disappeared. **Known cost:** refreshing flags is O(folder size) per sync. M5 replaces it with CONDSTORE/QRESYNC.
  - If the server rejects the partial fetch `BODY.PEEK[TEXT]<0.1024>`, fall back to `BODY.PEEK[HEADER]` for headers and leave the snippet empty until the body is fetched.
  - A `uid_validity` change returns `reset = true` and restarts from scratch.
  - Headers come from `FETCH (UID FLAGS INTERNALDATE ENVELOPE BODYSTRUCTURE BODY.PEEK[TEXT]<0.1024>)`. The snippet comes from that partial.
  - Folder roles come from SPECIAL-USE flags, with name heuristics as fallback (`Sent`, `Sent Items`, `Drafts`, `Trash`, `Deleted Items`, `Junk`, `Spam`, `Archive`; INBOX is always `Inbox`).

- [ ] **Step 1:** Write `docker-compose.yml` with the GreenMail standalone image, pinned to the newest `2.x` tag that `docker pull` confirms. Expose 3025 (SMTP), 3143 (IMAP), 3465 and 3993 (TLS). Set `GREENMAIL_OPTS=-Dgreenmail.setup.test.all -Dgreenmail.hostname=0.0.0.0 -Dgreenmail.users=alice:password@wbmc.test,bob:password@wbmc.test -Dgreenmail.users.login=email`.
- [ ] **Step 2:** Write `seed.py`, using only the Python stdlib. It:
  - sends every corpus `.eml` to alice;
  - then sends **500** generated messages with distinct subjects `Seed message 0000`…`0499` and increasing dates;
  - with `--big`, sends 50,000.
  
  `dev-mail-up.sh` runs `docker compose up -d`, waits for port 3143, and then seeds.
- [ ] **Step 3:** Run `scripts/dev-mail-up.sh && python3 -c "import imaplib;m=imaplib.IMAP4('localhost',3143);m.login('alice@wbmc.test','password');print(m.select('INBOX'))"`. Expected: `('OK', [b'5xx'])`, i.e. 500 + the corpus count.
- [ ] **Step 4: Write failing integration tests** in `tests/imap_greenmail.rs`. They are skipped unless `WBMC_TEST_IMAP=1`:
  - `lists_inbox_with_role`;
  - `first_page_is_newest`: page_size 100 returns 100 upserts, with the max date = `Seed message 0499`, and `more == true`;
  - `backfill_until_done`: loop until `more == false`; the total equals the INBOX count, with no duplicates;
  - `incremental_picks_up_new_mail`: after backfill, send one message via SMTP from the test; the next sync returns exactly 1 upsert;
  - `detects_deletion`: delete via `imaplib`-equivalent async-imap `UID STORE +FLAGS \Deleted` + `EXPUNGE`; the next sync lists that UID in `deleted_remote_ids`;
  - `fetch_raw_parses`: `parse_message(fetch_raw(..))` subject matches the summary.
- [ ] **Step 5:** Run `WBMC_TEST_IMAP=1 cargo test -p wbmc-core --test imap_greenmail`. Expected: FAIL.
- [ ] **Step 6:** Implement `ImapProvider` with `async-imap`, `tokio-rustls` (platform roots via `rustls-native-certs`) and STARTTLS. Open one connection per call in M1; connection pooling is M5.
- [ ] **Step 7:** Run the tests. Expected: PASS.
- [ ] **Step 8: Commit** with `git commit -m "feat(core): IMAP provider and GreenMail test server"`.

### Task 6: Sync engine + events

**Files:**
- Create: `core/wbmc-core/src/sync.rs`, `src/events.rs`, `src/provider/fake.rs`

**Interfaces:**
- **Consumes:** `Store` (Task 3), `MailProvider` + types (Task 5), `BodyStore`, `parse_message` (Task 4).
- **Produces (events):**
  - `enum CoreEvent`, with variants:
    - `AccountsChanged`;
    - `FoldersChanged { account_id }`;
    - `MessagesChanged { folder_id }`;
    - `SyncStatus { account_id, status: SyncStatus }`.
  - `enum SyncStatus`, with variants `Idle`, `Syncing { done: u32 }`, `Error { message: String }`, `NeedsSignIn`.
  - `trait EventSink: Send + Sync { fn emit(&self, e: CoreEvent); }`.
- **Produces:** `SyncEngine::new(store: Arc<Store>, bodies: Arc<BodyStore>, events: Arc<dyn EventSink>)`, with these methods:
  - `async fn sync_account(&self, account: &Account, provider: &dyn MailProvider) -> Result<()>`;
  - `async fn body(&self, account: &Account, provider: &dyn MailProvider, id: &MessageId) -> Result<Vec<u8>>`, which reads the cache first, otherwise fetches, stores, then returns.
- **Produces:** `FakeProvider`, built from in-memory folders and messages, with knobs: `fail_after_pages: Option<u32>`, `reset_next: bool`, `auth_error: bool`.
- **Algorithm:**
  1. Upsert all folders, resolving parents by remote id, and emit `FoldersChanged`.
  2. Sync folders in role order: Inbox, Sent, Drafts, then the rest.
  3. For each folder, loop on `sync_folder(page_size = 200)`:
     - if `reset`, call `clear_folder` first;
     - apply upserts and deletions, then `set_folder_sync_state` **after each page, in one transaction**;
     - call `recount` and emit `MessagesChanged`;
     - stop when `more == false`.
  4. Prefetch bodies for the newest 200 Inbox messages.
  5. Error handling:
     - `CoreError::NeedsSignIn` → emit `SyncStatus::NeedsSignIn` and stop, with no retry;
     - other errors → emit `Error` and return the error.

- [ ] **Step 1: Write failing tests** with `FakeProvider` and a temp store:
  - `syncs_all_pages_inbox_first`: 3 folders and 450 Inbox messages. The store ends with 450 Inbox rows, and the first `MessagesChanged` event is for the Inbox.
  - `resume_after_failure`: `fail_after_pages = Some(1)`. The first sync returns Err with 200 rows stored. The second sync completes with 450 and no duplicates.
  - `uidvalidity_reset_clears_cache`: after a full sync, set `reset_next`. The provider now returns 10 different messages, so the store has exactly 10.
  - `deletions_applied`.
  - `needs_sign_in_stops_without_retry`: `auth_error`. The event is `NeedsSignIn`, and the provider call count == 1.
  - `body_cached_after_first_fetch`: the second `body()` makes no provider call.
- [ ] **Step 2:** Run `cargo test -p wbmc-core sync`. Expected: FAIL.
- [ ] **Step 3:** Implement `SyncEngine`.
- [ ] **Step 4:** Run the tests. Expected: PASS.
- [ ] **Step 5: Commit** with `git commit -m "feat(core): resumable sync engine and core events"`.

### Task 7: Secrets + Microsoft OAuth

**Files:**
- Create: `core/wbmc-core/src/secrets.rs`, `src/auth/{mod,pkce,loopback,microsoft}.rs`

**Interfaces:**
- **Produces (secrets):**
  - `trait SecretStore: Send + Sync { fn get(&self, key: &str) -> Result<Option<String>>; fn set(&self, key: &str, value: &str) -> Result<()>; fn delete(&self, key: &str) -> Result<()>; }`.
  - Two implementations: `KeyringSecrets` (service `io.github.ajthom90.wbmc`) and `MemorySecrets`.
  - Keys: `"{account_id}:password"` and `"{account_id}:refresh_token"`.
- **Produces (PKCE and tokens):**
  - `pkce::Pkce::new() -> Pkce { verifier, challenge }` (S256, verifier 64 chars).
  - `TokenSet { access_token, refresh_token: Option<String>, expires_at_ms: i64 }`.
- **Produces (Microsoft sign-in):**
  - `microsoft::SignIn::start(client_id: &str) -> Result<SignIn>`. It binds `127.0.0.1:0` and builds the authorize URL with these parameters:
    - `redirect_uri = http://localhost:{port}`;
    - `response_type=code`;
    - the scopes from Task 2;
    - `prompt=select_account`;
    - PKCE `code_challenge` and a random `state`.
  - `SignIn::url(&self) -> String`.
  - `SignIn::finish(self, timeout: Duration) -> Result<(TokenSet, String /*email*/)>`. It:
    1. accepts one request;
    2. checks `state`;
    3. replies with a small "You can close this window" HTML page;
    4. exchanges the code at `{authority}/oauth2/v2.0/token`;
    5. reads the email from Graph `GET /me` (`mail`, falling back to `userPrincipalName`).
- **Produces:** `microsoft::refresh(client_id, refresh_token) -> Result<TokenSet>`. A response of `invalid_grant` → `CoreError::NeedsSignIn`.
- **Produces:** `TokenManager::new(secrets: Arc<dyn SecretStore>, client_id: String)`, with:
  - `async fn access_token(&self, account_id: &AccountId) -> Result<String>`, which refreshes when less than 5 minutes remain, saves the rotated refresh token, and caches the access token in memory;
  - `invalidate(&self, account_id)`.
- Base URLs are injectable (`Endpoints { authority, graph }`), so tests can point them at `wiremock`.

- [ ] **Step 1: Write failing tests:**
  - `pkce_challenge_matches_rfc7636_vector`: the verifier `dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk` gives the challenge `E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM`.
  - `authorize_url_has_required_params`.
  - `loopback_rejects_wrong_state`.
  - `finish_exchanges_code` (wiremock token + `/me`).
  - `refresh_rotates_refresh_token`.
  - `invalid_grant_maps_to_needs_sign_in`.
  - `token_manager_refreshes_only_when_near_expiry`.
- [ ] **Step 2:** Run `cargo test -p wbmc-core auth`. Expected: FAIL.
- [ ] **Step 3:** Implement. In `Cargo.toml`, enable the `keyring` platform backends (`apple-native`, `windows-native`, `sync-secret-service`); keyring 3.x has no default backend, and without these `KeyringSecrets` silently stores nothing. Load the client ID from `config/oauth-clients.toml` via `include_str!` + `toml`. If the ID is empty, `SignIn::start` returns `InvalidInput("Microsoft client ID not configured; see docs/setup/microsoft-entra-app.md")`.
- [ ] **Step 4:** Run the tests. Expected: PASS.
- [ ] **Step 5: Commit** with `git commit -m "feat(core): keychain secrets and Microsoft OAuth (PKCE loopback)"`.

### Task 8: Microsoft Graph provider

**Files:**
- Create: `core/wbmc-core/src/provider/graph.rs`, `tests/graph_live.rs`

**Interfaces:**
- **Consumes:** `TokenManager` (Task 7), the `MailProvider` types (Task 5).
- **Produces:** `GraphProvider::new(account_id: AccountId, tokens: Arc<TokenManager>, graph_base: String)`.
- **Folders:**
  - list with `GET /me/mailFolders?$top=100`, recursing into `childFolders` where `childFolderCount > 0`;
  - map roles by matching ids from `GET /me/mailFolders/{wellKnown}` for `inbox`, `sentitems`, `drafts`, `deleteditems`, `archive` and `junkemail`, cached per provider.
- **Message sync:**
  - `GET /me/mailFolders/{id}/messages/delta?$select=subject,from,toRecipients,ccRecipients,receivedDateTime,isRead,flag,hasAttachments,bodyPreview`, with the header `Prefer: odata.maxpagesize={page_size}`;
  - follow `@odata.nextLink` one page per call (`more = true`) and store `@odata.deltaLink` when done;
  - items with `@removed` go to `deleted_remote_ids`;
  - a `410 Gone` or `syncStateNotFound` response → `reset = true`, restarting without the link.
  
  The delta API doesn't guarantee an order. That's acceptable: the store sorts by date, and the Inbox lands first by folder order. **Known cost:** without `$orderby`, a 50k-message Inbox may not show its newest mail within seconds. M3 adds a parallel `GET /messages?$orderby=receivedDateTime desc&$top=200` first pass.
- **Bodies:** `fetch_raw` uses `GET /me/messages/{id}/$value`.
- **Errors:**
  - every request: on `401`, call `tokens.invalidate` and retry once;
  - a second `401` → `NeedsSignIn`;
  - `429`/`503`: honor `Retry-After` (cap 30 s, maximum 3 retries).

- [ ] **Step 1: Write failing wiremock tests:**
  - `folders_with_roles_and_children`;
  - `delta_pages_then_delta_link`: 2 pages, `more` true then false, and the state holds the `delta_link`;
  - `removed_items_reported`;
  - `gone_triggers_reset`;
  - `fetch_raw_returns_mime`;
  - `retries_once_on_401_then_needs_sign_in`;
  - `honors_retry_after`.
- [ ] **Step 2:** Run `cargo test -p wbmc-core graph`. Expected: FAIL.
- [ ] **Step 3:** Implement with `reqwest`.
- [ ] **Step 4:** Run the tests. Expected: PASS.
- [ ] **Step 5:** Write `tests/graph_live.rs` (`#[ignore]`, requires `WBMC_TEST_M365=1`, test user only). It refreshes from `WBMC_TEST_M365_REFRESH_TOKEN`, lists folders and asserts the Inbox role exists, then syncs one page of the Inbox and fetches one raw body.
- [ ] **Step 6: Commit** with `git commit -m "feat(core): Microsoft Graph mail provider"`.

### Task 9: FFI facade + Apple build pipeline

**Files:**
- Create: `core/wbmc-ffi/{Cargo.toml,src/lib.rs,src/bin/uniffi-bindgen.rs}`, `scripts/build-apple.sh`, `apps/apple/WBMCCoreKit/Package.swift`

**Interfaces:**
- **Consumes:** everything from Tasks 3–8.
- **Produces:** the UniFFI object `WbmcCore`, which Swift sees as `WbmcCoreProtocol`:
  - `#[uniffi::constructor] new(data_dir: String, listener: Box<dyn CoreListener>) -> Result<Arc<Self>, FfiError>`. It uses `KeyringSecrets`, unless the environment variable `WBMC_SECRETS=memory` is set, in which case it uses `MemorySecrets`. That's for tests and CI; Linux CI has no Secret Service, and unsigned test binaries must not touch the login keychain;
  - `accounts() -> Vec<AccountInfo>`;
  - `async add_imap_account(input: ImapAccountInput) -> Result<AccountInfo, FfiError>`, which verifies the login by calling `list_folders` before saving;
  - `begin_microsoft_sign_in() -> Result<Arc<MicrosoftSignIn>, FfiError>`, where `MicrosoftSignIn` has `url() -> String` and `async finish() -> Result<AccountInfo, FfiError>` (timeout 5 min);
  - `remove_account(account_id: String)`;
  - `folders(account_id: String) -> Vec<FolderInfo>`;
  - `messages(folder_id: String, offset: u32, limit: u32) -> Vec<MessageRow>`;
  - `async message_content(message_id: String) -> Result<MessageContent, FfiError>`;
  - `async sync_account(account_id: String) -> Result<(), FfiError>`;
  - `async sync_all()`, which runs the accounts concurrently;
  - `debug_refresh_token(account_id: String) -> Option<String>`, compiled only with feature `debug-tools`.
- **Produces (FFI records):**
  - `AccountInfo { id, kind: String /*"imap"|"microsoft"*/, email, display_name, status: String /*"idle"|"syncing"|"error"|"needs_sign_in"*/ }`;
  - `FolderInfo { id, account_id, name, role: String, parent_id: Option<String>, unread_count: u32, total_count: u32 }`;
  - `MessageRow { id, subject, from_name, from_email, date_ms: i64, snippet, is_read, is_flagged, has_attachments }`;
  - `MessageContent { subject, from_name, from_email, to: Vec<String>, cc: Vec<String>, date_ms, html: Option<String>, text: Option<String>, attachments: Vec<AttachmentInfo> }`, where `AttachmentInfo { filename, mime_type, size: u64 }`.
- **Produces:** the callback interface `CoreListener { fn on_event(&self, event: FfiEvent); }`, where `FfiEvent` is an enum mirroring `CoreEvent` with string ids.
- **Produces:** `ImapAccountInput { email, display_name, host, port: u16, security: String, username, password, smtp_host, smtp_port: u16, smtp_security: String }`.
- **HTML handling:** `message_content` inlines `cid:` images as `data:` URIs, so the web view never needs a custom scheme.
- **Build script:** `scripts/build-apple.sh` builds `aarch64-apple-darwin` and `x86_64-apple-darwin` in release, `lipo`s them into a universal static lib, generates Swift with the bindgen binary into `WBMCCoreKit/Sources/WBMCCore/Generated/`, and wraps the lib plus the modulemap in `WBMCCoreKit/WBMCCoreFFI.xcframework`.

- [ ] **Step 1: Write failing Rust tests** in `wbmc-ffi`, using a temp dir and an IMAP account pointed at GreenMail (skipped unless `WBMC_TEST_IMAP=1`):
  - `add_imap_account_then_sync_then_read`: `accounts().len() == 1`; the Inbox has `total_count >= 500`; `messages(inbox, 0, 50).len() == 50`; `message_content(first).subject` is non-empty.
  - `bad_password_is_rejected_and_not_saved`: `accounts()` is empty afterwards.
  - `listener_receives_messages_changed`.
- [ ] **Step 2:** Run `WBMC_TEST_IMAP=1 WBMC_SECRETS=memory cargo test -p wbmc-ffi`. Expected: FAIL. (CI sets `WBMC_SECRETS=memory` for every job.)
- [ ] **Step 3:** Implement. `WbmcCore` owns a tokio multi-thread runtime, `Store`, `BodyStore`, `SyncEngine`, `TokenManager` (with `KeyringSecrets`) and a provider factory keyed by `ProviderKind`.
- [ ] **Step 4:** Run the tests. Expected: PASS.
- [ ] **Step 5:** Run `scripts/build-apple.sh && swift build --package-path apps/apple/WBMCCoreKit`. Expected: both succeed, and `Generated/wbmc_ffi.swift` exists.
- [ ] **Step 6: Commit** with `git commit -m "feat(ffi): WbmcCore UniFFI facade and Apple build pipeline"`.

### Task 10: macOS app: three-pane reader

**Files:**
- Create: `apps/apple/project.yml`, `apps/apple/Shared/{AppModel,CoreClient}.swift`, `apps/apple/macOS/WBMCApp.swift`, `apps/apple/macOS/Views/{Sidebar,MessageList,MessageView,AddAccount,SafeHTMLView}.swift`, `apps/apple/Tests/AppModelTests.swift`, `apps/apple/Tests/SafeHTMLViewTests.swift`, `apps/apple/UITests/LaunchTests.swift`

**Interfaces:**
- **Consumes:** `WbmcCoreProtocol` and the records from Task 9.
- **Produces:** `@Observable @MainActor final class AppModel` with:
  - properties: `accounts: [AccountInfo]`, `folders: [String: [FolderInfo]]`, `selectedFolderID: String?`, `messages: [MessageRow]`, `selectedMessageID: String?`, `content: MessageContent?`;
  - methods:
    - `func load()`;
    - `func select(folder:)`, which loads the first 100 rows;
    - `func loadMore()`, which appends the next 100;
    - `func select(message:) async`;
    - `func refresh() async`.
  
  The model receives core events through a `CoreListener` adapter that hops to the main actor and reloads only what changed.
- **Produces:** `SafeHTMLView(html: String)`, an `NSViewRepresentable` `WKWebView` with these settings:
  - `WKWebpagePreferences.allowsContentJavaScript = false`;
  - a `WKContentRuleList` that blocks `url-filter: "^https?://"`;
  - non-persistent data store;
  - links open in the default browser via the navigation delegate.

**UI layout:**
- `NavigationSplitView` with three columns:
  - **sidebar:** each account as a section header, with role folders first (Inbox, Drafts, Sent, Archive, Junk, Trash) and then the others alphabetically, nested by parent; unread badges; and an account status icon (spinner, warning, or "Sign in again" button).
  - **message list:** sender, subject, snippet, date, an unread dot, and a flag/attachment glyph; infinite scroll calls `loadMore()`.
  - **reading pane:** header block plus `SafeHTMLView`, falling back to plain text; attachment chips (display only in M1).
- **Toolbar:** Refresh and Add Account. **Menu:** Debug ▸ "Copy refresh token…" (debug builds only, for `docs/setup/test-accounts.md`).
- **Add Account sheet:** two choices, "Microsoft 365 / Outlook.com" and "Other (IMAP)".
  - **Microsoft:** opens the URL with `NSWorkspace.shared.open`, then awaits `finish()` with a progress state and cancel.
  - **IMAP:** a form. In `#if DEBUG` builds only, its defaults are prefilled for `localhost:3143`/`3025`, security `None`, when the email ends in `@wbmc.test`.
- The data dir is `~/Library/Application Support/io.github.ajthom90.wbmc/`. `sync_all()` runs on launch and when Refresh is pressed.

**Steps:**
- [ ] **Step 1: Write failing tests** in `AppModelTests` with a `FakeCore: WbmcCoreProtocol`:
  - `selectFolderLoadsFirstPage`: 250 rows, so `messages.count == 100`; `loadMore` makes it 200.
  - `messagesChangedEventReloadsOnlyThatFolder`.
  - `needsSignInStatusSurfaces`.
  - `selectMessageLoadsContent`.
  
  And in `SafeHTMLViewTests`: load `remote_images_and_script.eml`'s HTML, then assert that `evaluateJavaScript` is unavailable or fails and that no navigation request for `http` reaches the delegate.
- [ ] **Step 2:** Run `xcodegen --spec apps/apple/project.yml && xcodebuild test -project apps/apple/WBMC.xcodeproj -scheme WBMC-macOS -destination 'platform=macOS'`. Expected: FAIL.
- [ ] **Step 3:** Implement the model, views and `WBMCApp`.
- [ ] **Step 4:** Run the tests. Expected: PASS. Then run the `LaunchTests` UI smoke test, which launches and expects the window with an "Add Account" button. Expected: PASS.
- [ ] **Step 5: Commit** with `git commit -m "feat(macos): three-pane reader app"`.

### Task 11: End-to-end verification + owner demo

**Files:**
- Create: `docs/milestones/m1-demo.md`, a checklist with results.

- [ ] **Step 1:** Run `scripts/dev-mail-up.sh`, build and run the app, and add `alice@wbmc.test`. Expected:
  - the Inbox shows 500+ messages;
  - the newest is at the top within 3 s of adding the account;
  - corpus messages render, including the Café subject, the Latin-1 and Shift-JIS bodies, and the inline image;
  - `remote_images_and_script` shows no remote images, confirmed in Console with no network requests.
- [ ] **Step 2:** Run `seed.py --big`, add the account again, and record:
  - the time to first page;
  - memory after a full backfill, in Activity Monitor; the target is < 500 MB;
  - that scrolling the list stays smooth.
- [ ] **Step 3:** **Owner step.** Once the Entra app exists, sign in with the M365 test user. Expected: folders with roles, Inbox mail, and HTML rendering. Then copy the refresh token via the debug menu and run `WBMC_TEST_M365=1 ... cargo test -p wbmc-core --test graph_live -- --ignored`. Expected: PASS.
- [ ] **Step 4:** Quit, relaunch with the network off, and confirm the app opens to the cached Inbox in under 1 s and that cached bodies are readable.
- [ ] **Step 5:** Write the results into `m1-demo.md`, then commit and push. Tell the owner M1 is ready to try.
