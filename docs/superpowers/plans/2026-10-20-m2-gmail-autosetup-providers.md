# M2: Gmail + Auto-Setup + More Providers Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The owner types an email address and WBMC sets the account up:
- Gmail and Google Workspace through one browser sign-in, synced with the Gmail API;
- Microsoft 365 and Outlook.com through one browser sign-in;
- Yahoo, AOL and iCloud with an app password, with the server settings filled in automatically;
- any other address through auto-discovery, with a manual form as the last resort.

Mail from every account also appears in one "All Inboxes" view.

**Architecture:**
- **OAuth becomes provider-neutral.** One `OAuthClient` description per provider, plus one generic sign-in and refresh flow. Microsoft and Google differ only in configuration and in how the account's email address is looked up.
- **Gmail gets its own provider.** `GmailProvider` implements the existing `MailProvider` trait. Each Gmail label is a `Folder`, and each (label, message) pair is one row, so the schema doesn't change.
- **New detection module.** `wbmc-core::setup` turns an email address into an `AccountSetup` using bundled profiles, MX records, the Microsoft user-realm check, Thunderbird's ISPDB, the domain's own autoconfig file and DNS SRV records.
- **Unified inbox.** The unified inbox is a store query across every folder with role Inbox, backed by a new date index (schema v2).

**Tech Stack:** M1's stack, plus:
- `hickory-resolver` (system DNS config) for MX/SRV;
- `quick-xml` for autoconfig XML.

**Spec:** `docs/superpowers/specs/2026-10-19-wbmc-design.md` (§1 "It works", §7 Setup, §10 Providers). Roadmap: `docs/superpowers/plans/2026-10-19-phase1-roadmap.md` (M2 row). Carry-overs come from `docs/milestones/m1-final-review.md` (recommendation 4, R-M2, and T7/T9/T10 "M2" rows).

## Global Constraints

Everything in M1's Global Constraints still applies:
- MPL headers;
- macOS 26+;
- bundle id `io.github.ajthom90.wbmc`;
- keychain-only credentials;
- no remote loads in mail HTML;
- local-first;
- Graph only for Microsoft;
- test mailboxes only;
- no committed secrets;
- `openusage-budget` before each dispatch batch.

New for M2:
- **Google uses the Gmail API**, not IMAP.
- **Google client secret:** Google's "Desktop app" OAuth clients require a `client_secret` at the token endpoint even with PKCE. Google documents that secret as non-confidential for installed apps.
  - **[Claude's call]** It still isn't committed. It's read at build time from the git-ignored `config/oauth-clients.local.toml` or the `WBMC_GOOGLE_CLIENT_SECRET` build env var.
  - A build without it compiles. Google sign-in then returns `InvalidInput("Google sign-in is not configured; see docs/setup/google-cloud-project.md")`.
  - The owner may later decide to commit it (Thunderbird does).
- **Google redirect URI** is `http://127.0.0.1:{port}`; Microsoft keeps `http://localhost:{port}`.
- **Detection privacy:** only the **domain** of the typed address leaves the machine during detection, never the local part.
  - The Microsoft user-realm probe sends `probe@{domain}`.
  - Every detection HTTP request is https.
  - Total detection deadline: **8 s**.
- **No server-settings screen when detection succeeds** (spec §2 anti-goal). Manual settings stay reachable behind a "Show server settings" disclosure, and they are the main path only when detection returns `Unknown`.
- **Provider facts are data, not code.** Yahoo/AOL/iCloud hosts, ports, security, username style and app-password help URLs live in `core/wbmc-core/src/setup/profiles.toml`, with the source URL next to each entry. The implementer verifies each value against its source page before committing. Tests assert that the file is loaded and used, not that remembered values are true.
- **Google OAuth scopes** (Ruling, as R4 for Microsoft: request what M4/M6 need now so users never re-consent):
  - `openid`, `email`, `profile`;
  - `https://mail.google.com/`;
  - `https://www.googleapis.com/auth/calendar`;
  - `https://www.googleapis.com/auth/contacts`;
  - `https://www.googleapis.com/auth/contacts.other.readonly`;
  - `https://www.googleapis.com/auth/tasks`.
  
  Authorize params also include `access_type=offline` and `prompt=consent`. Without them Google returns no refresh token.

## Review Focus

1. **A temporary IMAP refusal** must show a sync error and retry on the next sync, never "sign in again". Examples: Yahoo or iCloud answering `NO [UNAVAILABLE]`, `NO [LIMIT]` or "too many simultaneous connections" during the first-sync burst. *Pinned in Task 2.*
2. **A stale Gmail `historyId`** (HTTP 404) or **a label deleted on the server** must reset or remove that folder cleanly, with no duplicates and no stale rows. *Pinned in Task 4.*
3. **Detection on a bad network** must return within 8 s and fall back to the next source, never hanging the Add Account sheet. Cases:
   - ISPDB down;
   - DNS timeout;
   - a captive portal answering autoconfig URLs with HTML;
   - malformed XML.
   
   *Pinned in Task 5.*
4. **Google sign-in edge cases** must leave one working account keyed by the real address:
   - the refresh response omits `refresh_token` (Google never rotates it), so the stored token stays;
   - the user picks a different Google account in the browser than the address they typed.
   
   *Pinned in Tasks 3 and 7.*
5. **The owner's existing M1 database** (schema v1) must open after upgrade with every account, folder and message intact. A database from a newer version must fail with a clear error. *Pinned in Task 6.*

---

## File Structure

```
config/oauth-clients.toml               + [google] client_id (public)
config/oauth-clients.local.toml         git-ignored; [google] client_secret
docs/setup/google-cloud-project.md      owner's step-by-step Google Cloud setup
docs/setup/test-accounts.md             + Gmail, and optional Yahoo/AOL/iCloud test accounts
core/wbmc-core/build.rs                 exports WBMC_GOOGLE_CLIENT_SECRET to rustc if present
core/wbmc-core/src/
  model.rs                              ProviderKind::Google ("google")
  auth/oauth.rs                         OAuthClient, generic SignIn (cancel, login_hint), refresh
  auth/loopback.rs                      [::1] too; wrong state → 400 and keep waiting; host param
  auth/microsoft.rs                     client() + account_email(); flow code moves to oauth.rs
  auth/google.rs                        client() + account_email() via Gmail getProfile
  auth/tokens.rs                        TokenManager keyed by account, client chosen by ProviderKind
  provider/mod.rs                       FolderSyncState::Gmail; MailProvider::fetch_raw_many
  provider/imap.rs                      narrowed NeedsSignIn; fetch_raw_many over one session
  provider/gmail.rs                     GmailProvider
  sync.rs                               prefetch via fetch_raw_many
  setup/mod.rs                          detect(email) -> AccountSetup; source ordering + deadline
  setup/profiles.rs + profiles.toml     bundled provider profiles
  setup/autoconfig.rs                   Thunderbird config-v1.1 XML parser + fetchers
  setup/dns.rs                          Resolver trait; hickory impl; MX/SRV rules
  store/mod.rs, store/migrations.rs     incremental migrations; schema v2; unified_messages
core/wbmc-core/tests/gmail_live.rs      #[ignore], WBMC_TEST_GMAIL=1
core/wbmc-ffi/src/lib.rs                detect_account, begin_sign_in, OAuthSignIn.cancel,
                                        unified_messages, MessageRow.account_id
apps/apple/macOS/Views/AddAccount.swift email-first flow
apps/apple/macOS/Views/Sidebar.swift    All Inboxes row; Sign in again for every OAuth kind
apps/apple/Shared/{AppModel,CoreClient}.swift
docs/milestones/m2-demo.md
```

---

### Task 1: Owner guide for Google Cloud + client configuration plumbing

The owner needs this early, so the Google project is ready by Task 7. It runs in parallel with Tasks 2–6.

**Files:**
- Create: `docs/setup/google-cloud-project.md`, `core/wbmc-core/build.rs`
- Modify: `config/oauth-clients.toml`, `docs/setup/test-accounts.md`, `.gitignore` (add `config/oauth-clients.local.toml`)

**Interfaces:**
- Produces `config/oauth-clients.toml` with a new table:
  ```toml
  [google]
  client_id = ""   # filled in by the owner; public
  ```
  The file header gains one line: "The Google client secret goes in oauth-clients.local.toml (git-ignored), never here."
- Produces: `build.rs` reads `config/oauth-clients.local.toml` (`[google] client_secret`) if it exists. Otherwise it reads the `WBMC_GOOGLE_CLIENT_SECRET` env var. It emits `cargo:rustc-env=WBMC_GOOGLE_CLIENT_SECRET=…` only when a value is found, plus `cargo:rerun-if-changed` / `rerun-if-env-changed`. Rust reads it with `option_env!("WBMC_GOOGLE_CLIENT_SECRET")`.

- [ ] **Step 1:** Write `google-cloud-project.md` as numbered click-by-click steps in the Google Cloud console:
  - **Project and APIs:** create the project **WBMC**, then enable the Gmail API, Google Calendar API, People API and Tasks API.
  - **OAuth consent screen:**
    - type External, app name WBMC, owner's support email;
    - add every scope from Global Constraints;
    - add the owner's test Gmail address under **Test users**.
  - **Publishing status:** set it to **In production**. In "Testing" status Google expires refresh tokens after 7 days. Unverified production apps with restricted scopes show a "Google hasn't verified this app" screen (Advanced → continue) and are capped at 100 users. That's fine until release.
  - **Client:** Credentials → Create OAuth client ID → **Desktop app**, named WBMC macOS. Copy the client ID into `config/oauth-clients.toml` and the client secret into `config/oauth-clients.local.toml`.
  - **Before public release (owner task):** Google verification plus the yearly CASA assessment for the restricted Gmail scope (spec §10).
- [ ] **Step 2:** Update `test-accounts.md`:
  - **Gmail:** the env vars `WBMC_TEST_GMAIL=1`, `WBMC_TEST_GMAIL_USER`, `WBMC_TEST_GMAIL_REFRESH_TOKEN`. Capture the token with Debug ▸ "Copy refresh token…" (it already works for any account).
  - **Optional Yahoo, AOL and iCloud accounts:** throwaway, with app passwords; env vars `WBMC_TEST_{YAHOO,AOL,ICLOUD}_USER` / `_APP_PASSWORD`.
  - **Rule:** automated tests use only these accounts.
- [ ] **Step 3:** Write `build.rs`. Task 3 reads the value with `option_env!("WBMC_GOOGLE_CLIENT_SECRET")`.
- [ ] **Step 4:** Verify:
  - `cargo build -p wbmc-core` succeeds with no local file;
  - with a local file holding `client_secret = "x"`, `cargo build -vv -p wbmc-core 2>&1 | grep -c 'rustc-env=WBMC_GOOGLE_CLIENT_SECRET'` prints `1`;
  - `git status` shows the local file ignored.
- [ ] **Step 5: Commit** with `git commit -m "docs: Google Cloud setup guide; build-time Google client secret"`. Then tell the owner the guide is ready, and ask for optional Yahoo/AOL/iCloud test accounts.

### Task 2: IMAP hardening before more IMAP providers

**Files:**
- Modify: `core/wbmc-core/src/provider/{mod,imap,graph,fake}.rs`, `src/sync.rs`, `core/wbmc-ffi/src/lib.rs` (re-add path), `core/wbmc-ffi/src/tests.rs`, `tests/imap_greenmail.rs`

**Interfaces:**
- Produces a new trait method with a default body that calls `fetch_raw` per id:
  ```rust
  async fn fetch_raw_many(&self, folder_remote_id: &str, message_remote_ids: &[String])
      -> Vec<(String, Result<Vec<u8>>)>;
  ```
  Results come back in input order.
  - `ImapProvider` overrides it: one session and one `EXAMINE`, then `UID FETCH <set> BODY.PEEK[]` in chunks of **20** UIDs, each chunk under `COMMAND_TIMEOUT`, keeping only the requested UIDs. A chunk failure marks only that chunk's ids `Err`.
  - `GraphProvider` keeps the default.
- Produces: `SyncEngine::prefetch` groups uncached Inbox messages and calls `fetch_raw_many` once. It stores each `Ok` body. It returns `NeedsSignIn` if any result is `NeedsSignIn`, and logs and skips other errors.
- Produces: `map_login` maps to `NeedsSignIn` only for `NO [AUTHENTICATIONFAILED]`, `NO [AUTHORIZATIONFAILED]`, or a `NO` with no response code. Every other `NO`/`BAD` becomes `CoreError::Network("The mail server is busy or limiting connections; will retry")`. That covers `[UNAVAILABLE]`, `[LIMIT]`, `[INUSE]`, `[EXPIRED]` and `[SERVERBUG]`, plus any text containing "too many" or "try again".
- Produces (R-M2): when `add_imap_account` matches an existing account, it also replaces `Account.config` and `display_name` with the new input, via a new `Store::update_account(&Account) -> Result<()>`.

- [ ] **Step 1: Write failing tests:**
  - `map_login` unit tests:
    - `[AUTHENTICATIONFAILED]` and a bare `NO` → `NeedsSignIn`;
    - `NO [UNAVAILABLE] Temporary`, `NO [LIMIT]` and `NO Too many simultaneous connections` → `Network`.
  - `prefetch_uses_one_batch`: with `FakeProvider` counting calls, 200 Inbox messages → `fetch_raw_many` called once and `fetch_raw` never called.
  - GreenMail: `fetch_raw_many_one_session` (25 UIDs come back in order and parse; one login, observed through a test-only login counter on `ImapProvider`) and `fetch_raw_many_skips_missing_uid` (a nonexistent UID → that entry is `Err`, the others are `Ok`).
  - FFI: `re_add_updates_settings`: re-add with a changed `smtp_port` → the stored config has the new port and the account id is unchanged.
- [ ] **Step 2:** Run `cargo test -p wbmc-core` and `WBMC_TEST_IMAP=1 WBMC_SECRETS=memory cargo test --workspace`. Expected: the new tests FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run both commands again. Expected: PASS. Run `cargo run -p wbmc-ffi --example m1_verify --release` and record the time to idle for the normal seed in the commit body. Before the change it was ~21.7 s; expect a large drop.
- [ ] **Step 5: Commit** with `git commit -m "fix(core): narrow IMAP NeedsSignIn, batch prefetch over one session, re-add updates settings"`.

### Task 3: Provider-neutral OAuth + Google sign-in

**Files:**
- Create: `core/wbmc-core/src/auth/{oauth,google}.rs`
- Modify: `auth/{mod,loopback,microsoft,tokens}.rs`, `model.rs`, `provider/graph.rs` (token call site), `core/wbmc-ffi/src/lib.rs` (compile fixes only)

**Interfaces:**
- Produces: `ProviderKind::Google`, whose string is `"google"`.
- Produces the client description:
  ```rust
  pub struct OAuthClient { pub kind: ProviderKind, pub client_id: String,
      pub client_secret: Option<String>, pub authorize_url: String, pub token_url: String,
      pub scopes: Vec<String>, pub redirect_host: &'static str /* "localhost" | "127.0.0.1" */,
      pub extra_authorize_params: Vec<(String, String)> }
  ```
- Produces the generic sign-in, `oauth::SignIn`:
  - `start(client: OAuthClient, login_hint: Option<&str>) -> Result<SignIn>`. An empty client id gives the provider's "not configured" `InvalidInput`.
  - `url(&self) -> String`.
  - `cancel_handle(&self) -> CancelHandle`, where `CancelHandle: Clone + Send + Sync` has `cancel()`.
  - `async finish(self, timeout) -> Result<TokenSet>`. A cancel makes it return `InvalidInput("Sign-in was cancelled.")` promptly.
- Produces: `oauth::refresh(client: &OAuthClient, refresh_token: &str) -> Result<TokenSet>`. `invalid_grant` or `interaction_required` → `NeedsSignIn`.
- Produces the provider modules:
  - `microsoft::client() -> Result<OAuthClient>` and `microsoft::account_email(graph_base: &str, access_token: &str) -> Result<String>` (current `/me` logic);
  - `google::client() -> Result<OAuthClient>`: authorize `https://accounts.google.com/o/oauth2/v2/auth`, token `https://oauth2.googleapis.com/token`, the scopes and extra params from Global Constraints;
  - `google::account_email(gmail_base: &str, access_token: &str) -> Result<String>`, using `GET {gmail_base}/users/me/profile` → `emailAddress`. The default `gmail_base` is `https://gmail.googleapis.com/gmail/v1`.
- Produces: `TokenManager::new(secrets, clients: HashMap<ProviderKind, OAuthClient>)` and `access_token(&self, account_id: &AccountId, kind: ProviderKind) -> Result<String>`. When a refresh response has no `refresh_token`, the stored one is kept.
- Produces (loopback):
  - `Loopback::bind()` binds `127.0.0.1:0`, then tries `[::1]` on the same port and ignores failure there.
  - `redirect_uri(&self, host: &str)`.
  - A request with a wrong or missing `state`, or any path other than `/`, is answered `400` and the listener keeps waiting.
  - The success page is sent only **after** the code exchange succeeds. On failure it shows "Sign-in failed. Return to WBMC."

- [ ] **Step 1: Write failing tests:**
  - All existing Microsoft auth tests pass again after being ported to `oauth::SignIn` with `microsoft::client()` and wiremock endpoints.
  - `google_authorize_url_has_offline_consent_and_hint`: the URL has `access_type=offline`, `prompt=consent`, `login_hint=a@example.com`, `redirect_uri=http://127.0.0.1:{port}`, and `code_challenge_method=S256`.
  - `token_exchange_sends_secret_only_when_configured`: wiremock asserts that the form has `client_secret` for a client that has one and lacks it for Microsoft.
  - `refresh_without_refresh_token_keeps_stored_one`.
  - `loopback_wrong_state_gets_400_then_accepts_right_state`.
  - `loopback_answers_on_ipv6` (skip if `[::1]` cannot bind).
  - `cancel_finishes_promptly`: cancel → `finish` returns within 1 s.
  - `success_page_after_exchange`: when the exchange fails, the browser response body contains "Sign-in failed".
- [ ] **Step 2:** Run `cargo test -p wbmc-core auth`. Expected: FAIL.
- [ ] **Step 3:** Implement. Move the flow out of `microsoft.rs` into `oauth.rs`, leaving `microsoft.rs` with config, scopes and `account_email`. `GraphProvider` calls `access_token(&id, ProviderKind::Microsoft)`.
- [ ] **Step 4:** Run `cargo test --workspace`. Expected: PASS.
- [ ] **Step 5: Commit** with `git commit -m "feat(core): provider-neutral OAuth with Google sign-in, cancel and loopback fixes"`.

### Task 4: Gmail API provider

**Files:**
- Create: `core/wbmc-core/src/provider/gmail.rs`, `tests/gmail_live.rs`
- Modify: `provider/mod.rs`

**Interfaces:**
- **Consumes:** `TokenManager::access_token(id, ProviderKind::Google)` (Task 3).
- **Produces:**
  - `GmailProvider::new(account_id: AccountId, tokens: Arc<TokenManager>, gmail_base: String) -> Result<GmailProvider>` (fallible; no panics on client build);
  - `FolderSyncState::Gmail { history_id: Option<String>, page_token: Option<String>, backfilled: bool }`.
- **Folders:**
  - `GET /users/me/labels`. Keep `INBOX`→Inbox, `SENT`→Sent, `DRAFT`→Drafts, `TRASH`→Trash, `SPAM`→Junk, and every `type == "user"` label → Other.
  - Skip `UNREAD`, `STARRED`, `IMPORTANT`, `CHAT`, and every `CATEGORY_*`.
  - Skip All Mail. Gmail exposes no All Mail label, and syncing it would double storage. That's M3's search scope.
  - A user label named `A/B` is nested under the label named `A` when it exists.
  - `remote_id` is the label id.
- **Initial sync and backfill** (state has `backfilled == false`):
  1. On the first call only, read `historyId` from `GET /users/me/profile`.
  2. `GET /users/me/messages?labelIds={id}&maxResults={min(page_size,100)}&pageToken=…`. Gmail returns newest first.
  3. For each id: `GET /users/me/messages/{id}?format=metadata&metadataHeaders=Subject&metadataHeaders=From&metadataHeaders=To&metadataHeaders=Cc&metadataHeaders=Content-Type`, with at most **10** requests in flight.
  4. `more` stays true while `nextPageToken` exists. When it doesn't, set `backfilled = true`.
- **Field mapping:**
  - `date_ms` = `internalDate`;
  - `snippet` = Gmail's `snippet`, HTML-entity-decoded;
  - `is_read` = no `UNREAD` label; `is_flagged` = has `STARRED`;
  - `has_attachments` = the top-level `Content-Type` is `multipart/mixed`. **Ruling:** metadata format returns no parts, and this heuristic is good enough for the list glyph.
- **Incremental sync** (`backfilled == true`):
  1. `GET /users/me/history?startHistoryId={history_id}&labelId={id}&maxResults=500`, following `nextPageToken` within the call.
  2. Collect every message id named in `messagesAdded`, `messagesDeleted`, `labelsAdded` and `labelsRemoved`.
  3. Re-get each id's metadata:
     - 404 → deleted;
     - labels no longer include this folder's label → deleted;
     - otherwise → upsert (this also refreshes flags).
  4. Store the response's `historyId`.
  5. **History 404 → `reset = true`** and restart with no state.
- **Label deleted on the server:** `list_folders` no longer returns it. The engine's existing folder reconciliation removes it. Verify that the reconciliation exists; if it doesn't, add `Store::delete_folders_not_in(account, remote_ids)` and call it from `sync_account` in this task.
- **Bodies:** `fetch_raw` uses `GET /users/me/messages/{id}?format=raw` and base64url-decodes `raw`. `folder_remote_id` is ignored.
  - **Known cost (Ruling):** a message under 3 labels has 3 rows and may be fetched 3×. Prefetch touches only the Inbox, and M3's full-offline work revisits this with a body store keyed by `(account, remote_id)`.
- **Errors:**
  - 401 → `invalidate` and retry once; a second 401 → `NeedsSignIn`.
  - 429, or 403 with reason `rateLimitExceeded`/`userRateLimitExceeded` → back off per `Retry-After`, or 1/2/4 s, at most 3 retries.
  - Other 5xx → `Network`.
- **Known cost:** metadata costs ~5 quota units per message, so a 50k-label backfill takes tens of minutes in the background. The first page stays fast.

- [ ] **Step 1: Write failing wiremock tests:**
  - `labels_map_roles_and_skip_system`;
  - `nested_user_label`;
  - `first_page_newest_then_backfill_then_history`: 2 list pages, then `backfilled`, then a history call that returns 1 add.
  - `history_label_removed_is_deletion`.
  - `history_404_resets`.
  - `flags_from_labels`.
  - `fetch_raw_decodes_base64url`.
  - `retries_once_on_401_then_needs_sign_in`.
  - `backs_off_on_429`.
  - `deleted_label_folder_removed` (with `FakeProvider` in `sync.rs`).
- [ ] **Step 2:** Run `cargo test -p wbmc-core gmail`. Expected: FAIL.
- [ ] **Step 3:** Implement with `reqwest` and `futures::stream::buffer_unordered(10)`.
- [ ] **Step 4:** Run the tests. Expected: PASS.
- [ ] **Step 5:** Write `tests/gmail_live.rs`: `#[ignore]`, requires `WBMC_TEST_GMAIL=1`, test account only. It refreshes from the env token, lists labels and asserts the Inbox role, syncs one Inbox page, then fetches and parses one raw body.
- [ ] **Step 6: Commit** with `git commit -m "feat(core): Gmail API provider"`.

### Task 5: Account auto-detection

**Files:**
- Create: `core/wbmc-core/src/setup/{mod,profiles,autoconfig,dns}.rs`, `src/setup/profiles.toml`, `src/setup/testdata/*.xml`
- Modify: `core/wbmc-core/Cargo.toml` (`hickory-resolver`, `quick-xml`)

**Interfaces:**
- Produces the result type:
  ```rust
  pub enum AccountSetup {
      Microsoft,
      Google,
      Imap { provider_name: String, config: ImapConfig, username_alternatives: Vec<String>,
             app_password_url: Option<String> },
      Unknown,
  }
  ```
- Produces: `pub async fn detect(email: &str, env: &DetectEnv) -> Result<AccountSetup>`. An address without exactly one `@` and a non-empty domain → `InvalidInput`.
- Produces: `DetectEnv { resolver: Arc<dyn Resolver>, http: reqwest::Client, ispdb_base: String, realm_base: String, deadline: Duration }`. `DetectEnv::system()` uses:
  - `https://autoconfig.thunderbird.net/v1.1`;
  - `https://login.microsoftonline.com/common/userrealm`;
  - an 8 s deadline;
  - `HickoryResolver`.
- Produces: `#[async_trait] trait Resolver: Send + Sync { async fn mx(&self, domain: &str) -> Result<Vec<String>>; async fn srv(&self, name: &str) -> Result<Vec<(String /*host*/, u16)>>; }`, plus a `FakeResolver` for tests.
- **Profiles (`profiles.toml`):**
  - entries keyed by domain list: `gmail.com`, `googlemail.com` → google; `outlook.com`, `hotmail.com`, `live.com`, `msn.com` → microsoft; Yahoo domains, AOL domains, and `icloud.com`, `me.com`, `mac.com` → imap.
  - Each imap entry has `host`, `port`, `security`, `smtp_host`, `smtp_port`, `smtp_security`, `username` (`"email"` | `"local"` | `"local_then_email"`), `app_password_url` and `source`.
  - A `#[cfg(any(debug_assertions, feature = "test-util"))]` entry for `wbmc.test` points at `localhost:3143`/`3025`, security none. It replaces the Swift `#if DEBUG` prefill.
- **Source order:** the first match wins.
  1. A bundled profile for the domain. If it matches, return at once with no network.
  2. **Run these concurrently under the deadline, then pick by this priority:**
     1. MX:
        - any host ending in `.google.com` or `.googlemail.com` → Google;
        - ending in `.protection.outlook.com` → Microsoft;
        - `mx01.mail.icloud.com` or `mx02.mail.icloud.com` → the iCloud profile with username `email`.
     2. The user-realm check, `GET {realm_base}/?user=probe@{domain}&api-version=2.1`: `NameSpaceType` of `Managed` or `Federated` → Microsoft. This catches M365 tenants behind third-party mail filters.
     3. ISPDB `GET {ispdb_base}/{domain}` → parse config-v1.1.
     4. The domain's own `https://autoconfig.{domain}/mail/config-v1.1.xml` and `https://{domain}/.well-known/autoconfig/mail/config-v1.1.xml`.
     5. SRV records `_imaps._tcp.{domain}` and `_submission._tcp.{domain}` (RFC 6186).
  3. Otherwise → `Unknown`.
  
  Any source that errors, times out, returns non-XML, or has no IMAP server counts as "no match". **Ruling:** Microsoft's Exchange autodiscover service is not used. It's from the Exchange Web Services era, which the spec makes a non-goal. MX plus user-realm covers M365.
- **Autoconfig parsing:**
  - prefer `incomingServer type="imap"` with SSL over STARTTLS;
  - `%EMAILADDRESS%` → `"email"`, `%EMAILLOCALPART%` → `"local"`;
  - `socketType` `SSL` → Tls, `STARTTLS` → StartTls, `plain` → None.
  - **Reject `plain` for anything but `localhost`**: return no match rather than plaintext passwords.

- [ ] **Step 1:** Write fixtures: `ispdb_example.xml` (SSL IMAP plus STARTTLS SMTP), `starttls_only.xml`, `plain_only.xml`, `malformed.xml`, `captive_portal.html`.
- [ ] **Step 2: Write failing tests**, all with `FakeResolver` + wiremock and no real network:
  - `profile_gmail_is_google_without_network`: the resolver and HTTP each record zero calls.
  - `profile_imap_entries_load` (every imap entry parses and has `source` and `app_password_url`).
  - `mx_google_workspace`.
  - `mx_m365`.
  - `realm_managed_is_microsoft`: assert that the request path contains `probe@` and not the real local part.
  - `ispdb_parses_imap_settings`.
  - `domain_autoconfig_fallback`.
  - `srv_fallback`.
  - `plain_rejected`.
  - `malformed_and_html_ignored`.
  - `slow_sources_respect_deadline`: ISPDB delays 30 s and the deadline is 300 ms → returns `Unknown` in < 1 s.
  - `invalid_address_rejected`.
  - `live_detect` (`#[ignore]`, `WBMC_TEST_DETECT=1`): real DNS for `gmail.com`, `outlook.com` and `icloud.com`.
- [ ] **Step 3:** Run `cargo test -p wbmc-core setup`. Expected: FAIL.
- [ ] **Step 4:** Implement. Verify each Yahoo/AOL/iCloud profile value against its `source` URL (fetch the page) and record the check date in a comment.
- [ ] **Step 5:** Run the tests. Expected: PASS.
- [ ] **Step 6: Commit** with `git commit -m "feat(core): account auto-detection (profiles, MX, user realm, ISPDB, autoconfig, SRV)"`.

### Task 6: Schema v2 + unified inbox query

**Files:**
- Create: `core/wbmc-core/src/store/migrations.rs`
- Modify: `store/mod.rs`, `store/schema.sql`

**Interfaces:**
- **Produces:** `migrate` runs incrementally. A fresh database gets `schema.sql` (v1), then each step up to `SCHEMA_VERSION = 2`, all in one transaction.
  - A database at v1 runs only the steps above 1.
  - A version above `SCHEMA_VERSION` → `Storage("This mail database was created by a newer version of WBMC.")`.
  - Step 2 is `CREATE INDEX idx_messages_date ON messages (date_ms DESC);`.
- **Produces:** `Store::unified_messages(role: FolderRole, offset: u32, limit: u32) -> Result<Vec<MessageSummary>>`. It returns messages in any folder with that role, across all accounts, newest first, with ties broken by `id`.

- [ ] **Step 1: Write failing tests:**
  - `v1_database_upgrades_intact`: build a v1 database with M1's `schema.sql` and version row 1, insert 1 account, 2 folders and 3 messages, open it with the new `Store`. The version is 2, the rows are intact, and the index exists.
  - `newer_version_refused`: version 99 → that error message.
  - `unified_merges_inboxes_newest_first`: 2 accounts with Inbox + Sent each → only Inbox rows, interleaved by date, paged correctly.
  - `unified_perf` (`#[ignore]`, release): 3 accounts × 50k messages, 40% Inbox → `unified_messages(Inbox, 0, 100)` < 20 ms.
- [ ] **Step 2:** Run `cargo test -p wbmc-core store`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run the tests and `cargo test -p wbmc-core --release unified_perf -- --ignored`. Expected: PASS. Record the timing in the commit body.
- [ ] **Step 5: Commit** with `git commit -m "feat(core): incremental migrations, schema v2, unified inbox query"`.

### Task 7: FFI for detection, generic sign-in, Google accounts and unified inbox

**Files:**
- Modify: `core/wbmc-ffi/src/lib.rs`, `core/wbmc-ffi/src/tests.rs`, `core/wbmc-ffi/examples/m1_verify.rs` (compile fixes)

**Interfaces:**
- **Consumes:** Tasks 2–6.
- **Produces these `WbmcCore` changes:**
  - `async detect_account(email: String) -> FfiResult<AccountSetupInfo>`.
  - `begin_sign_in(provider: String /*"microsoft"|"google"*/, login_hint: Option<String>) -> FfiResult<Arc<OAuthSignIn>>`. It replaces `begin_microsoft_sign_in`, and `MicrosoftSignIn` is renamed `OAuthSignIn` with `url()`, `async finish() -> FfiResult<AccountInfo>` and a new `cancel()`.
  - `unified_messages(offset: u32, limit: u32) -> Vec<MessageRow>`.
  - `MessageRow` gains `account_id: String`.
  - `add_imap_account` tries each of `username_alternatives` in order when the input's `username` is empty. That's how iCloud's `local_then_email` works.
- **Produces the setup records:**
  - `AccountSetupInfo { kind: String /*"microsoft"|"google"|"imap"|"unknown"*/, provider_name: String, imap: Option<ImapSettings>, app_password_url: Option<String> }`;
  - `ImapSettings { host, port: u16, security: String, username: String, smtp_host, smtp_port: u16, smtp_security: String }`. `username` is the first alternative.
- **Google accounts:** `finish` for Google saves through the existing Microsoft path, generalized to `save_oauth_account(store, kind, email, …)`. The email is the one `google::account_email` returns, **not** the login hint. The same kind plus a case-insensitive email match re-uses the existing account.
- **Provider factory:** `ProviderKind::Google` → `GmailProvider::new(...)?`.
- **Status:** a successful sign-in sets that account's status to `idle` **and** clears any pending `needs_sign_in`, even while a failing sync is still running (T9 carry-over). Ignore stale `NeedsSignIn` events from a sync that started before the sign-in by stamping the sync with a generation number.

- [ ] **Step 1: Write failing tests:**
  - `detect_account_maps_profile`: `x@wbmc.test` → kind `imap`, host `localhost`, port 3143.
  - `begin_sign_in_unknown_provider_rejected`.
  - `google_sign_in_saves_real_email_not_hint`: wiremock token + profile returns `real@gmail.com`, hint `typed@gmail.com` → one account `real@gmail.com`.
  - `google_re_sign_in_reuses_account`.
  - `cancel_aborts_finish`.
  - `unified_messages_include_account_id` (two GreenMail accounts: alice, bob).
  - `resign_in_during_failing_sync_ends_idle`.
- [ ] **Step 2:** Run `WBMC_TEST_IMAP=1 WBMC_SECRETS=memory cargo test -p wbmc-ffi`. Expected: FAIL.
- [ ] **Step 3:** Implement. Inject the wiremock base URLs for tests the same way M1 did for Graph (a `cfg(test)` constructor taking `Endpoints`).
- [ ] **Step 4:** Run the tests. Then run `scripts/build-apple.sh && swift build --package-path apps/apple/WBMCCoreKit`. Expected: PASS and build OK.
- [ ] **Step 5: Commit** with `git commit -m "feat(ffi): account detection, generic OAuth sign-in, Google accounts, unified inbox"`.

### Task 8: macOS app: email-first Add Account + All Inboxes

**Files:**
- Modify: `apps/apple/macOS/Views/{AddAccount,Sidebar,MessageList}.swift`, `apps/apple/Shared/{AppModel,CoreClient}.swift`, `apps/apple/macOS/WBMCApp.swift`, `apps/apple/Tests/{AppModelTests,SafeHTMLViewTests}.swift`

**Interfaces:**
- **Consumes:** Task 7's FFI.
- **Produces:** `SignInStarter.start(provider: String, loginHint: String?) throws -> any OAuthSignInFlow`. `OAuthSignInFlow` adds `cancel()`.
- **Produces:** `AppModel.unifiedInboxID = "unified:inbox"`.
  - `select(folder: unifiedInboxID)` loads `core.unifiedMessages`.
  - `.messagesChanged` for any folder with role `inbox` reloads the unified list when it is selected.
  - `showsUnifiedInbox` is `accounts.count >= 2`.
- **Add Account flow (one sheet, three states):**
  1. **Email:** one field, "Continue" (default), "Cancel". Running `detectAccount` shows "Looking up your account…".
  2. **Result:**
     - `microsoft` or `google`: the text "Continue in your browser to sign in to {provider_name}." with a "Sign In" button. It calls `start(provider:loginHint: email)`, opens the URL, and awaits `finish`. Cancel calls `flow.cancel()`.
     - `imap` with `app_password_url`: "{provider_name} needs an app password, not your regular password." with a "Create an app password" link button (opens in the browser), a password field, and "Add Account".
     - `imap` without one: a password field and "Add Account".
     - `unknown`: "We couldn't find settings for {domain}." with three buttons: "Microsoft 365", "Google", and "Other (IMAP)". "Other (IMAP)" opens the manual form.
  3. **Server settings:** a "Show server settings" disclosure under every imap state, prefilled from `ImapSettings`. That's the M1 form, kept.
  
  Remove the `#if DEBUG` `@wbmc.test` prefill; detection now supplies it.
- **Sidebar:**
  - an "All Inboxes" row (`tray.2`) above the account sections when `showsUnifiedInbox`, with the summed unread badge;
  - `offersSignInAgain` is true for `microsoft` and `google`, and "Sign in again" starts that provider with `loginHint: account.email`;
  - the needs-sign-in message checks OAuth kinds, not only `"microsoft"`.
- **Message list:** in the unified view each row shows a small account-colored dot (a stable hue from `account_id`) with the account email as help text.

- [ ] **Step 1: Write failing tests** in `AppModelTests`:
  - `unifiedInboxLoadsAcrossAccounts`;
  - `inboxChangeReloadsUnified`;
  - `unifiedHiddenWithOneAccount`;
  - `googleNeedsSignInOffersSignInAgain`;
  - `cancelCancelsRustFlow` (the fake flow records `cancel()`);
  - `detectionResultDrivesState` (each of the 4 kinds → the expected sheet state, by testing a pure `AddAccountState.from(_ setup:)`).
  
  In `SafeHTMLViewTests` (T10 carry-over): `remoteSubresourceNeverReachesServer`. An `NWListener` on `127.0.0.1:0` counts connections. Load HTML that references `http://127.0.0.1:{port}/x.png` in `<img>`, CSS `url()`, `<link>` and `srcset`, wait 2 s, and assert the count is 0.
- [ ] **Step 2:** Run `xcodegen --spec apps/apple/project.yml && xcodebuild test -project apps/apple/WBMC.xcodeproj -scheme WBMC-macOS -destination 'platform=macOS'`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run the tests and `LaunchTests`. Expected: PASS. Then take one screenshot of each Add Account state with the GreenMail account and attach them to the task report.
- [ ] **Step 5: Commit** with `git commit -m "feat(macos): email-first account setup and All Inboxes"`.

### Task 9: End-to-end verification + owner demo

**Files:**
- Create: `docs/milestones/m2-demo.md`

- [ ] **Step 1:** Run `scripts/dev-mail-up.sh`, then the app. Add `alice@wbmc.test` by typing only the address and the password. Expected: no server settings shown; Inbox syncs. Add `bob@wbmc.test`. Expected: "All Inboxes" appears and interleaves both by date.
- [ ] **Step 2:** Rerun `m1_verify` (normal and `--big`) and record first page, time to idle, and peak memory against M1 (557 ms / 880 ms / 22 MiB). Expected: no regression beyond 10%, and time to idle for the normal seed is clearly lower than M1's.
- [ ] **Step 3:** Run `WBMC_TEST_DETECT=1 cargo test -p wbmc-core live_detect -- --ignored`. Expected: PASS.
- [ ] **Step 4: Owner step (Gmail).** Once the Google project exists, type the test Gmail address. Expected:
  - a browser sign-in;
  - labels with roles;
  - Inbox newest first;
  - HTML renders with remote content blocked.
  
  Then copy the refresh token and run `WBMC_TEST_GMAIL=1 … cargo test -p wbmc-core --test gmail_live -- --ignored`. Expected: PASS.
- [ ] **Step 5: Owner step (optional; only for accounts the owner provides).** Type the Yahoo, AOL and iCloud addresses. For each, record:
  - detection found the right provider;
  - the app-password link opened the right page;
  - the account synced;
  - no "sign in again" during the first sync.
  
  Correct `profiles.toml` from what's observed.
- [ ] **Step 6: Owner step (M365, carried from M1).** Once the Entra client ID exists, type the M365 test user's address. Expected: it's detected as Microsoft, with one browser sign-in.
- [ ] **Step 7:** Write the results into `m2-demo.md` and commit. Tell the owner M2 is ready to try.

---

## Not in M2

| Item | Where it goes |
|---|---|
| Yahoo/AOL OAuth (both need partner approval for IMAP OAuth; app passwords work today) | Later |
| IMAP for Gmail | Later; the Gmail API is primary per the spec |
| CalDAV/CardDAV for iCloud and Yahoo | M6 |
| All Mail and server search | M3 |
| Self-signed and custom-CA IMAP servers | M8, with the trust UI |
| Tier 2 providers beyond what ISPDB already covers | Phase 1/2 as time allows |
| A feature switch for the unified inbox | M8, Features page |
