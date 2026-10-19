# M1 final whole-branch review: Foundation + read mail

- Range: `76a5286..7b6db10` (branch `m1-foundation-read-mail`, 24 commits, 70 files, +16,748 lines).
- Reviewer: senior code reviewer (final review). Read-only; the working tree, index, HEAD and branches were not touched.
- Authority: spec `docs/superpowers/specs/2026-10-19-wbmc-design.md` (§3 Phase 1), plan `docs/superpowers/plans/2026-10-19-m1-foundation-read-mail.md` (Global Constraints, Review Focus 1-5), roadmap, and ledger rulings R1-R21.

## Method

I reviewed the branch in passes, by area:

1. core store and sync (`store/`, `sync.rs`, `events.rs`, `model.rs`, `error.rs`);
2. providers (`provider/imap.rs`, `provider/graph.rs`, `provider/mod.rs`, `mime.rs`);
3. auth and secrets (`auth/*`, `secrets.rs`);
4. FFI (`wbmc-ffi/src/lib.rs`, `html.rs`, the tests list);
5. Swift app (`AppModel`, `CoreClient`, every view, `SafeHTMLView` and its tests, `project.yml`, entitlements);
6. dev and CI (`ci.yml`, compose, `build-apple.sh`, `m1-demo.md`, config).

Then I traced the seams between those areas: provider to engine, engine to FFI, FFI to Swift, removal racing sync, and sign-in to token manager to Graph.

Verification I ran:

- `cargo test --workspace` (no IMAP env): **pass**. wbmc-core has 99 unit tests; 7 IMAP tests pass vacuously because they are gated; `graph_live` is ignored; wbmc-ffi has 15 tests.
- `cargo clippy --workspace --all-targets -- -D warnings`: **pass**.
- I checked that every new `.rs`/`.swift` file has the MPL header (`Package.swift` has it on line 2, after the required tools-version line).
- I scanned the diff for committed secrets and found none.
- Scratch benchmark outside the repo (scratchpad crate, `wbmc-core` as a path dependency, repo `Cargo.lock`), M2 Ultra, release build:
  - backfilling a 50,000-message folder in 200-row `apply_sync_page` calls: 1.9 s total;
  - one final-incremental page (50k `FlagChange`s plus a 50k retain list): **80-103 ms** holding the store mutex;
  - `messages(0,100)`: 0.2 ms;
  - `messages(49900,100)`: 27 ms.

## Strengths

- **Store atomicity is right.** `apply_sync_page` applies upserts, deletions, flag updates, retain-pruning, the cursor and the recount in one transaction (`store/mod.rs:181-200`). `reset_folder` is atomic (`:202-209`). Message ids survive upserts (R1, `:285-297`). Foreign keys are on, so writes from a sync racing account removal fail on the FK instead of resurrecting rows. A crash at any point resumes from the last committed page.
- **IMAP deletion detection is bounded by message count, not UID span** (R7). UIDVALIDITY changes reset correctly, including body cleanup (`sync.rs:145-147`). Flag refresh lands in the same transaction (R6).
- **Graph handling is careful:**
  - same-origin enforcement on every `nextLink`/`deltaLink` before the bearer token is attached (`graph.rs:133-156`);
  - no redirects;
  - capped bodies (50 MB);
  - error messages built only from sanitized OData codes;
  - 401 retried once, then `NeedsSignIn`;
  - capped `Retry-After`;
  - an R12 reset that cannot loop (`graph.rs:341-346`).
- **Secrets hygiene is consistently good:**
  - hand-written redacting `Debug` for `TokenSet`, `Pkce`, `SignIn`, `ImapProvider`, `ImapAccountInput`, `MemorySecrets` and `TokenManager`;
  - `reqwest` errors stripped with `without_url()`;
  - token-endpoint errors reduced to the OAuth code;
  - the loopback listener never reflects input;
  - passwords and refresh tokens only in the keychain, with `Account.config` holding no secret.
- **OAuth is correct:**
  - PKCE S256 from the OS CSPRNG with rejection sampling;
  - 256-bit state, checked before `error`;
  - `interaction_required` maps to `NeedsSignIn`;
  - single-flight refresh, with the rotated refresh token persisted before the access token is handed out.
- **HTML rendering is defence in depth:**
  - JavaScript is off for all frames;
  - non-persistent data store;
  - a content rule list that blocks every URL except `data:`/`about:`, installed before the load, failing closed if it can't compile;
  - a CSP meta placed at byte 0 (or straight after a lone doctype);
  - DNS prefetch off;
  - every non-`data:` `<link>` removed, along with `<base>` and `meta refresh`;
  - sub-frame navigations cancelled;
  - links open externally only for http/https/mailto.
  
  The scanner survived four adversarial rounds and has 30 targeted tests.
- **cid inlining is bounded:** one parse, encode once, a 10 MB total budget, and a strict `image/*` subtype check before building a `data:` URI.
- **Removal racing sync was thought through:**
  - a "removed" mark goes in before the abort;
  - late events are dropped;
  - `SyncGuard::drop` and `message_content` re-purge residue.
- **Offline reads work without credentials** (`44e3a7c`). The demo measured 3 ms to accounts, folders, first page and a cached body.
- **Large mailboxes:** the first page in 1977 ms on a 50k Dovecot box, newest-first, with 22 MiB peak RSS. Paging keeps memory bounded, and the UI loads 100 rows at a time.

## Issues

### Critical

None.

### Important

#### I1. IMAP TLS handshake has no timeout. A stalled network leaves the account stuck "syncing" until relaunch.

- **Where:** `core/wbmc-core/src/provider/imap.rs:159-169` (`tls_connect`), reached from `open_stream` (`:116-125`) for `Security::Tls` and from `starttls` (`:140-157`). The TCP connect (`:127-138`) and every IMAP command are timed; the TLS handshake is not. This promotes the Task 5 deferred minor "TLS handshake/STARTTLS write untimed".
- **What is wrong:** a middlebox that accepts TCP and then goes silent hangs `connector.connect(...)` forever. Captive-portal and hotel Wi-Fi do exactly this on port 993. TCP retransmission does not rescue it: the ClientHello was ACKed, so nothing is outstanding.
- **Why it matters:** the hung future holds the single-flight slot (`wbmc-ffi/src/lib.rs:358-374`). From then on, `sync_all` and `sync_account` return immediately (`:929-933`, `:954`), the sidebar spinner never stops, and that account never syncs again, even after the laptop reaches a good network. Only quitting the app helps. An uncached `message_content` hangs the same way ("Loading message…" forever). This is a common office-laptop situation.
- **Fix:**
  - Wrap the handshake in `tokio::time::timeout(CONNECT_TIMEOUT, …)`, and the STARTTLS `write_all`/`flush` the same way.
  - Belt and braces: put an overall deadline on each provider call in the engine, or on `sync_with`, so no future can pin the single-flight slot indefinitely.
  - Add a unit test with a TCP listener that accepts and never answers.

#### I2. One failing folder aborts the whole account sync and skips the Inbox body prefetch, on every sync.

- **Where:** `core/wbmc-core/src/sync.rs:84-92`. `self.sync_folder(...).await?` runs in a loop over all folders, and `prefetch` runs only after every folder succeeds.
- **What is wrong:** any persistent per-folder error stops the pass and returns `Err`. Examples:
  - `EXAMINE` answering `NO` on a listed but ACL-restricted shared mailbox, which `map_mailbox_error` turns into `NotFound`;
  - a very large folder whose 200-row FETCH exceeds the 30 s command timeout (R21);
  - a Graph `403`/`404` on one folder.
- **Why it matters:**
  - Every folder after the failing one, which is in provider listing order within the "other" group, never syncs.
  - The Inbox body prefetch never runs, so offline reading of the newest Inbox mail (spec §4 offline storage, demo Step 4) silently stops working.
  - The account shows the warning triangle permanently.
  - Office mailboxes with shared folders are exactly where this happens.
  
  Per-task reviews couldn't see it: Task 5 errors are correct in isolation, and Task 6 tests use a single healthy fake.
- **Fix:**
  - Catch errors per folder. Keep going through the remaining folders, and still prefetch the Inbox.
  - Return or emit an aggregated `Error` status naming the failed folder(s).
  - Keep the early return for `NeedsSignIn`, since auth failure is account-wide.
  - Add a FakeProvider knob that fails one named folder, and test that the other folders and the prefetch still complete.

#### I3. An IMAP account whose password changes, or one added twice by mistake, cannot be fixed or removed from the app.

- **Where:**
  - `apps/apple/macOS/Views/Sidebar.swift:50-56`: "Sign in again" appears only for Microsoft. IMAP gets a triangle whose tooltip says "Check your account settings".
  - `apps/apple/Shared/AppModel.swift:104-108`: same text.
  - No view calls `removeAccount` (grep of `apps/apple/macOS` and `Shared` finds nothing).
  - `core/wbmc-ffi/src/lib.rs:554-599`: `add_imap_account` always inserts a new account. Contrast `save_microsoft_account` at `:644-671`, which de-duplicates by email.
- **What is wrong:** there are no account settings, and no way to re-enter a password or remove an account. Re-adding the same address creates a second account that syncs in parallel and duplicates every folder in the sidebar. Removing the duplicate is then impossible.
- **Why it matters:** password rotation is routine for the spec's audience (IT-managed office staff), and so is mistyping during setup. The only remedies today are deleting `~/Library/Application Support/io.github.ajthom90.wbmc` and the keychain items by hand. The spec's anti-goal "No 'repair account' … flows" is about not needing them, not about leaving the user stuck. The plan doesn't list a remove action, but a reasonable user expects to be able to fix or remove an account. I graded this on that basis rather than on plan scope.
- **Fix (minimal):**
  - Add a "Remove Account…" context menu on the account section header, with confirmation, calling the existing FFI. Make `remove_account` async with `spawn_blocking`: it currently does keychain deletes and `remove_dir_all` of up to 50k body files on the caller thread, which would be the main thread (`lib.rs:616-639`, `:410-423`).
  - Make `add_imap_account` update the stored password, and keep the account id, when an IMAP account with the same email, host and username already exists, mirroring the Microsoft re-use path.
  - Change the IMAP tooltip to say what to do ("Re-add the account with the new password").

### Minor

M1. **Fixed `.tmp` name makes concurrent body writes for the same message fail.**
- **Where:** `core/wbmc-core/src/store/bodies.rs:26`, `:37-40`, `:84-88`.
- **What is wrong:** the first sync's Inbox prefetch and the user's click on the newest message fetch the same body at the same time. Both write `<id>.eml.tmp`, the first rename wins, and the second rename fails with `NotFound`. `message_content` then reports "Couldn't access the mail store". It also opens a window where a reader can see a truncated file.
- **Why it matters:** this is the first thing the owner will do in the demo.
- **Fix:** use a unique temp name, such as `<id>.eml.<uuid>.tmp`. On rename failure, if the final file now exists, treat that as success.

M2. **A LOGOUT timeout throws away completed work.**
- **Where:** `imap.rs:79-90` (`with_session`), `:442-459` (`finish_session`/`logout_session`).
- **What is wrong:** a fetched page or body becomes `Err("IMAP logout timed out")`. The server already did the work.
- **Fix:** log and ignore logout failures once the work has succeeded.

M3. **Any `NO` to IMAP `LOGIN` is treated as a bad password, and the first sync does 200 sequential logins.**
- **Where:** `imap.rs:1125-1132` (`map_login`); `sync.rs:198-214` (prefetch calls `body()` per message); `imap.rs:219-224` (one session per `fetch_raw`).
- **What is wrong:** throttling and temporary responses (`[UNAVAILABLE]`, `[LIMIT]`, "too many logins") become `NeedsSignIn`, which shows "credentials unavailable". The burst of 200 TLS logins on first sync makes such throttling likely on Yahoo, AOL and iCloud. It is also slow: 21.7 s to idle for 509 messages even on localhost.
- **Fix:** map only `[AUTHENTICATIONFAILED]`, or a `NO` without a temporary response code, to `NeedsSignIn`. Prefetch over one session. This must land before M2 adds Yahoo and iCloud.

M4. **Synchronous reads on the main thread share the writer's mutex.**
- **Where:** `AppModel.swift:31-33`, `42`, `85`, `90`; `wbmc-ffi/src/lib.rs:860-918`; `store/mod.rs:29-31`, `348-379`.
- **Measured:**
  - 80-103 ms hold for the final incremental page of a 50k folder, once per sync of that folder;
  - about 7.5 ms per backfill page;
  - 27 ms for a deep-offset read.
- **Why it is Minor:** a 0.1 s hitch per sync is not Important by itself, but it scales with folder size and machine speed.
- **Fix:** a second read-only connection for queries (WAL permits concurrent readers), or async read exports with `spawn_blocking`. Debounce `MessagesChanged` reloads during backfill.

M5. **Total-request timeouts block large messages on slow links.**
- **Where:** IMAP `COMMAND_TIMEOUT` of 30 s covers the whole `BODY.PEEK[]` (`imap.rs:27`, `574`, `649`). Graph sets a 60 s total `reqwest` timeout (`graph.rs:61`).
- **What is wrong:** a 25 MB message over a 3-5 Mbps link can never be opened.
- **Fix:** use an inactivity (read-progress) timeout for body downloads.

M6. **No self-healing for messages pruned locally but still on the server.**
- **Where:** `imap.rs:510-553` (incremental pass looks only below `lowest_uid` and above `highest_uid`) and `:423-426`; `store/mod.rs:364-379`.
- **What is wrong:** if `UID SEARCH ALL` ever returns an empty or partial list on a final incremental page, the cache is pruned. Those UIDs lie inside `[lowest, highest]`, so they are never fetched again, until UIDVALIDITY changes.
- **Fix:** fetch UIDs that are present on the server but missing locally. Refuse to prune more than X% of a folder against an empty or very short server list.

M7. **The reading pane closes while reading.**
- **Where:** `AppModel.swift:82-88`.
- **What is wrong:** reloading a fixed-size window drops the selected row when newer mail lands ahead of it. This happens constantly during Graph's unordered initial delta, which clears the pane and makes the list jump.
- **Fix:** keep the selection and its content when the row moves out of the window, or reload by anchor.

M8. **Graph messages lack the review-focus fallbacks in the list.**
- **Where:** `graph.rs:476-477`, `509-518`.
- **What is wrong:** a missing `from` (drafts, some system mail) gives an empty sender, so the list row is blank (`MessageList.swift:41`), and the header shows `" <>"` (`MessageView.swift:15`). Review Focus #1 asks for "Unknown sender".
- **Fix:** apply the same fallback in `map_address`, and render the header conditionally.

M9. **A secure-render failure leaves a blank pane.**
- **Where:** `SafeHTMLView.swift:109-112`, `143-151`.
- **What is wrong:** `loadError` is set but never displayed.
- **Fix:** show it, and fall back to `content.text`.

M10. **Security "None" is offered for any host with no warning.**
- **Where:** `AddAccount.swift:81-87`; `imap.rs:118`.
- **What is wrong:** the password goes out in clear text over the network.
- **Fix:** warn, or allow "None" only for loopback hosts outside Debug builds.

M11. **The user can't see a link's destination before opening it.**
- **Where:** `SafeHTMLView.swift:127-135`.
- **What is wrong:** WKWebView on macOS has no status bar, and a click opens the browser immediately. Outlook shows the URL on hover; phishing relies on users not seeing it.
- **Fix:** show the href in a hover overlay, via `WKUIDelegate` `mouseDidMoveOverElement`, which is private, or a simple tooltip approach. Alternatively confirm when the link text and the href domain differ. Full lookalike checks are M8.

M12. **No `tracing` subscriber is installed.**
- **What is wrong:** every `tracing::warn!` (prefetch failures, purge failures, swallowed store errors in `accounts`/`folders`/`messages`, `lib.rs:867`, `899`, `914`) is invisible, so field issues can't be diagnosed.
- **Fix:** install an `os_log`/file subscriber in the FFI constructor. Secrets are already kept out of error strings.

M13. **`WBMC_SECRETS=memory` is honoured in release builds.**
- **Where:** `secrets.rs:33-39`.
- **What is wrong:** a stray environment variable silently stops credentials from persisting.
- **Fix:** gate it behind `cfg(debug_assertions)` or a test feature, like `WBMC_DATA_DIR`.

M14. **Ad-hoc signing makes keychain access prompt on every rebuild.**
- **Where:** `project.yml:12-13`.
- **What is wrong:** `CODE_SIGN_IDENTITY: "-"` gives every Debug build a new signature, so keychain ACLs prompt or deny each time.
- **Why it matters:** this will surprise the owner in demo Step 3.
- **Fix:** sign with the owner's Apple Development identity, or document the prompt in `m1-demo.md`.

M15. **Server-deleted or renamed folders stay in the sidebar with stale mail.**
- **Where:** `sync.rs:96-127` only upserts. IMAP renames create a new folder and leave a ghost.
- **Status:** this is the Task 6 deferred item. Its milestone is decided below.

M16. **Removal can leave orphaned files.**
- **What is wrong:** if the app quits while an aborted sync is still writing, `bodies/<removed-id>/` can be left behind, because the "removed" set lives only in memory.
- **Fix:** sweep `bodies/` for directories with no matching account at start-up.

## Deferred-minor adjudication

"Must fix before merge" means fix in this branch. "Can wait" names the milestone that owns it.

| Task | Deferred minor | Decision |
|---|---|---|
| T1 | CI GreenMail service has no health check | Can wait (M5, test-server swap). `seed.py` already retries the connection (`seed.py:73`, `:107`). |
| T1 | Apple targets downloaded on Ubuntu; redundant dtolnay action | Can wait (any CI cleanup). |
| T1 | `workspace.package` lacks `rust-version`/`repository` | Can wait (M8 packaging). |
| T3 | Index lacks `id DESC` tie-break; OFFSET paging is O(offset) | Can wait (M3). Measured 27 ms at offset 49,900. |
| T3 | Poisoned mutex never recovered | Can wait (M5 robustness). |
| T3 | Store is blocking; callers must use `spawn_blocking` | Can wait (M5). FFI wraps its own async paths. The engine and synchronous main-thread reads remain; see M4. |
| T3 | BodyStore: fixed `.tmp` name, no dir fsync, no stale `.tmp` cleanup | **Fixed `.tmp` name: must fix before merge** (M1, on the demo path). Dir fsync and stale-temp cleanup can wait (M4, drafts). |
| T3 | `upsert_messages` rewrites `account_id` on conflict without an account/folder check | Can wait (M4). |
| T3 | `apply_sync_page` doesn't assert upserts' `folder_id` equals the folder argument | Can wait (M4; add a debug assert). |
| T4 | Inline detection too loose | Can wait (M4, attachment UI). |
| T4 | MIME-type closure duplicated | Can wait (refactor anytime). |
| T4 | No test for a present but unparseable `Date` | Can wait (M2). |
| T4 | Snippet stripper edge cases (unquoted `/`, >64 KB `<style>`, `>` in quotes) | Can wait (M3, search work touches snippets). |
| T5 | `seed.py --big` skipped when the marker exists, unless `--force` | Can wait (dev tooling). |
| T5 | Logout timeout turns a good delta into `Err` | **Must fix before merge** (M2 above; two lines). |
| T5 | Fallback triggers on any `FetchError::Imap`, including I/O | Can wait (M5). |
| T5 | Fallback reuses first-session UIDs | Can wait (M5). |
| T5 | Raw-path timeout is per read, not per command | Can wait (M5; GreenMail-only path). |
| T5 | TLS handshake and STARTTLS write untimed | **Must fix before merge.** Promoted to Important I1. |
| T5 | NUL not rejected by async-imap `validate_str` | Can wait (M5; input is the user's own password). |
| T6 | Two-pass folder upsert not atomic | Can wait (M5). |
| T6 | `eprintln` instead of `tracing::warn` | Resolved (`sync.rs:208` uses `tracing::warn!`). |
| T6 | About 3 s of tests from fsync per body `put` | Can wait. |
| T6 | Server-deleted folders never removed locally | Can wait, owned by **M5** ("Always up to date"), not an open-ended "follow-up". M15 above. |
| T6 | `body()` loads all folders to find one | Can wait (M3). |
| T6 | `FlagChange`/`PageChanges` lack `Debug` | Can wait. |
| T7 | Bind `[::1]` too | Can wait (M2, before Gmail OAuth reuses the loopback). |
| T7 | Wrong-state request aborts sign-in | Can wait (M2: answer 400 and keep waiting before a second OAuth provider ships; the ledger's "before release" stays the hard deadline). |
| T7 | Single-connection loopback | Can wait (M2). |
| T7 | Success page sent before the code exchange | Can wait (M2; cosmetic, and the app shows the real error). |
| T7 | Keychain calls block the async runtime | Can wait (M5; multi-thread runtime, so other workers continue). |
| T7 | `reqwest` client rebuilt per refresh | Can wait. |
| T7 | Tokens with ≤300 s lifetime refresh on every call | Can wait (Microsoft issues tokens of 60 min or more). |
| T7 | `refresh()` returns an empty `account_id` | Can wait (TokenManager fills it in). |
| T7 | Stale refresh token kept after `invalid_grant` | Can wait (the next sign-in overwrites it). |
| T8 | Per-chunk 50 MB cap path untested | Can wait (M3). |
| T8 | Bad `receivedDateTime` becomes 1970 | Can wait (M3); fold into M8's fallback fix if convenient. |
| T8 | No child-folder depth limit | Can wait (`seen_ids` already prevents cycles). |
| T8 | 404 on delta becomes `Protocol` | Can wait; its impact disappears once I2 is fixed. |
| T8 | `GraphProvider::new` panics if the client build fails | Can wait. It runs inside runtime tasks, so a panic becomes a `JoinError`, not a crash; make it fallible in M2. |
| T9 | Attachment list hides all inline parts | Can wait (M4). |
| T9 | Re-sign-in during a failing sync shows `needs_sign_in` | Can wait (M2). |
| T9 | `sync_account` with an unknown id leaves an orphan status | Can wait. |
| T9 | `remove_account` keychain deletes on the caller thread | **Must fix together with I3**: once a UI calls it, it must not run on the main thread. Otherwise it can wait. |
| T9 | Single-flight test timing-dependent | Can wait. |
| T9 | No Microsoft re-use-path test | Can wait (M2). Add it alongside I3's IMAP re-use test if cheap. |
| T9 | Gated tests pass instead of skip | Can wait (Linux CI sets `WBMC_TEST_IMAP=1`, so they do run there). |
| T9 | `Package.swift` declares iOS without an iOS slice | Can wait (Phase 3). |
| T9 | cid encoded before the budget check | Can wait. |
| T9 | Purge runs on a tokio worker in `Drop` | Can wait (M5). |
| T9 | Status-map stragglers for removed ids | Can wait. |
| T9 | `finish_microsoft` racing removal writes a token under the removed id | Can wait (practically unreachable). |
| T9 | Stale abort-handle overwrite | Can wait. |
| T9 | End-to-end removal test could be flaky | Can wait. |
| T10 | Cancel doesn't cancel the Rust sign-in | Can wait (M2). |
| T10 | No direct proof of subresource blocking | The automated test can wait (M2: a localhost `NWListener` hit-counter test). **Before merge, the owner's Console check in `m1-demo.md` Step 1 must be done** so Review Focus #5 has end-to-end evidence. |
| T10 | Per-event `Task` ordering | Can wait (M5). |
| T10 | Debug-menu notification reaches all windows | Can wait. |
| T10 | Leading UTF-8 BOM before the doctype gives quirks mode | Can wait (safe). |
| T10 | Unclosed raw text strips link/meta/base to EOF | Can wait (fails safe). |
| T11 | `ready_page` can pass with 1 row | Can wait (harmless). |

## Declined to judge

- Periodic polling, IDLE and push (new mail appears only on launch or Refresh): the plan sets "`sync_all()` runs on launch and when Refresh is pressed"; the roadmap puts live sync in M5.
- Mark-as-read on open, and any write action: M4 ("Act on mail").
- Gmail API, auto-discovery, unified inbox, Yahoo/AOL/iCloud: M2.
- Search, and newest-first Graph first pass on large folders: M3 (the plan records this as a known cost).
- O(N) IMAP flag refresh per sync, and connection pooling: the plan names these as M1 costs, replaced by CONDSTORE/QRESYNC and pooling in M5. I graded only the login-burst side effect (M3).
- Adaptive page shrinking on slow servers: R21 defers it to M5.
- `debug_refresh_token` compiled into the shipped static library: R14, removed by M8.
- App Sandbox, Developer ID signing, notarization: M8 per the roadmap and spec §3.
- Remote-image "load anyway" option or proxy: the plan says no remote loads in M1.
- Self-signed or custom-CA IMAP servers: needs auto-config and trust UI, which is M2.
- Accessibility, localization and RTL beyond the existing labels: spec v1 checklist; no M1 plan item.
- Swift build and UI tests in CI: the plan's CI specifies Rust jobs only. Listed as a recommendation instead.
- Phishing and lookalike-domain warnings beyond showing the link target: spec puts sender checks in M8. Only the hover gap is graded (M11).
- Exact GreenMail 50k behaviour: R20 moved the benchmark to Dovecot. The GreenMail result is recorded.

## Recommendations

1. Fix I1-I3, plus M1 and M2 (must-fix deferred minors), in one short fix round. All are small and local.
2. Complete the owner checks in `m1-demo.md` (Console with no requests to `tracker.example.com`; smooth scrolling) before merging, so Review Focus #4 and #5 have GUI evidence and not only headless evidence.
3. Add a per-call deadline around provider futures in the engine, so no future protocol hang can pin the single-flight slot (generalizes I1).
4. Before M2 adds Yahoo and iCloud: reuse one IMAP session for prefetch, and narrow `NeedsSignIn` to real authentication failures (M3).
5. Add a read-only SQLite connection for UI queries, and debounce `MessagesChanged` (M4). This becomes more important at M3's 1M-message target.
6. Install a `tracing` subscriber that writes to `os_log` (M12). Keep the existing no-secrets-in-errors discipline, which is solid.
7. Add an `xcodebuild test` job on macOS CI so the 38 SafeHTMLView security tests run on every PR. They are the main guard for Review Focus #5.

## Assessment

**Ready to merge: With fixes.**

The core is well built. Sync atomicity, UIDVALIDITY handling, secrets hygiene, OAuth and the HTML lockdown all meet the plan, and I found nothing Critical. Three Important issues show up only at the seams, and the target user would hit each of them:

- a stalled TLS handshake permanently wedges an account's sync (I1);
- one bad folder silently stops the remaining folders and offline prefetch (I2);
- an IMAP password change, or a duplicate add, leaves no way out in the UI (I3).

Fix those three, and the cheap must-fix minors M1 (concurrent body write) and M2 (logout timeout). Then complete the owner GUI checks, and M1 can merge.

---

## Re-review: fix wave `744248b`

This is a scoped re-review of `7b6db10..744248b`: 9 files, +536/−54. I read every hunk and checked it against I1, I2, I3, M1 and M2. At HEAD 744248b, `cargo test --workspace` (no IMAP env) passes: 104 unit tests, 7 vacuous IMAP-gated tests, and 16 FFI tests. The coordinator reported clippy, the GreenMail IMAP tests and the 39 macOS tests as passing; I did not re-run those.

### The five findings

- **I1: addressed.**
  - The TLS handshake is now bounded by `connect_timeout` (`imap.rs`, `tls_connect`), and so is the STARTTLS write/flush.
  - A silent-peer test covers it and also checks that the error text contains no password.
  - The engine adds a 120 s deadline per provider call (`sync.rs:16`, `:187`), which also unpins the single-flight slot.
- **I2: addressed.**
  - Folder errors are collected, the remaining folders still sync, and the Inbox prefetch still runs.
  - The pass then returns `Protocol("failed to sync <folder>: …")`.
  - `NeedsSignIn` from any folder still returns at once, without prefetch. Both paths are tested.
  - The aggregate is reported as `Protocol` even when every underlying error was `Network`. Only the status text sees it, so this is acceptable.
- **I3: addressed.**
  - "Remove Account…" now sits on each account header, with a confirmation dialog.
  - `remove_account` is async: the store and the keychain/body purge run on the blocking pool, and the "removed" mark is still set before the abort and the delete.
  - Re-adding the same email, host and username keeps the id and replaces the password.
  - The tooltip now tells the user how to recover.
  - `AppModel.removeAccount` clears the selection when the open folder belonged to that account, and the `AccountsChanged` → `load()` path also covers this.
- **M1: addressed.** Temp files are now named `<id>.eml.<counter>.<pid>.tmp`. If the rename fails but the final file exists, the write counts as success. A concurrency test covers it.
- **M2: addressed.** After success, a failed logout is now logged and ignored, on the session path and on the raw-snippet path.

### New issues

**Important**

- **R-I1. The 120 s whole-call deadline also covers IMAP's full flag refresh, so very large folders can stop syncing for good.**
  - Where: `core/wbmc-core/src/sync.rs:187` wraps `provider.sync_folder(...)`. On the final incremental page, `sync_folder` runs `refresh_flags` across every known UID (`imap.rs:409`, `:649`), in 500-UID batches that each have their own 30 s limit, plus login, `EXAMINE`, `UID SEARCH ALL` and the new-mail fetch.
  - Why it matters: a healthy but large folder can need more than 120 s. A 200k–1M-message Gmail "All Mail" or Exchange folder needs 400–2000 round trips, and the spec targets mailboxes of 1M or more. Once a folder crosses that size, every incremental pass times out. The cursor never advances, so the folder never gets new mail, flag changes or deletions, and the account shows an error on every sync.
  - I1's root cause, the untimed handshake, is now fixed, and every IMAP command inside `sync_folder` was already timed. The overall cap therefore adds a size cliff without adding safety for IMAP.
  - Fix: either drop the overall deadline for `sync_folder`, or turn it into a no-progress deadline that resets per command or batch. Keep it for `list_folders` and `fetch_raw` if wanted.
  - Re-run `m1_verify --big` on Dovecot afterwards. The fix wave re-ran only the 509-message case.

**Minor**

- **R-M1. Re-adding an IMAP account while its sync is running leaves the old password in use.**
  - Where: the in-place re-add stores the new password and calls `spawn_sync` (`wbmc-ffi/src/lib.rs:593-595`), which does nothing while a sync is in flight.
  - Effect: the running sync built its `ImapProvider` with the old password (`provider()` reads the password once). It ends in `NeedsSignIn`, overwriting the just-set "idle", and no new sync is queued. The user sees "Sign-in failed" again until the next Refresh. The window is narrow, because a bad-password sync fails at `list_folders` within seconds.
  - Fix: abort the running task and restart it, or set a "re-run" flag that `SyncGuard::drop` honours.
- **R-M2. The in-place re-add ignores changed settings.**
  - Where: same code path.
  - What happens: the login is verified with the new settings, but the stored config keeps the old port, security and SMTP fields. A user who fixes a wrong port or security mode by re-adding gets "verified" and then keeps failing.
  - Fix: update `Account.config` (and the display name) in the same blocking call.
- **R-M3. When the network stalls mid-pass, I2's continue-on-error is slow.**
  - Where: `sync.rs:241-256`.
  - What happens: if `list_folders` succeeded and the network then silently stalls, each remaining folder pays the connect and TLS timeouts (about 40 s each). Prefetch then tries 200 bodies, each paying the same. The single-flight slot can be held for a very long time, and Refresh does nothing meanwhile.
  - Why it is Minor: most network drops fail fast. Only a silent stall triggers this.
  - Fix: stop prefetch after its first `Network` error, and stop the folder loop after two consecutive `Network` failures.
- **R-M4. A removal error appears as "Couldn't Load Message".**
  - Where: `AppModel.swift:82`. `removeAccount` failures go into `errorMessage`, which only the reading pane shows.
  - Also: `selectedBelongs` is captured before the await (`:78`), so a folder the user picked on another account during removal could be deselected (cosmetic).
  - Fix: show the error in an alert, or next to the sidebar.

### Re-review verdict

**With fixes.** All five original findings are fully addressed, and there are no Critical issues.

One new Important issue remains, R-I1: the 120 s whole-call deadline puts a permanent size cliff on IMAP incremental sync. The fix is small: exempt `sync_folder`, or use a no-progress deadline. After that, re-run the Dovecot 50k check.

R-M1 through R-M4 can wait to M2/M5.

## Controller rulings (M1)

- Ruling R1: Store::upsert_messages preserves the existing MessageId on (folder_id, remote_id) conflict (ON CONFLICT DO UPDATE never touches id) — body files and UI selection key on MessageId — if wrong, cached bodies orphan on every resync.
- Ruling R2: Task 7 implementer adds `libdbus-1-dev` + `pkg-config` install to the ubuntu CI job — keyring's secret-service backend links dbus — if wrong, Linux CI fails to build.
- Ruling R3: Builders that cannot commit (Cursor, Codex) leave changes uncommitted; controller commits after builder finishes, before review — keeps review-package ranges intact — cost: none.
- Ruling R4: add delegated Mail.ReadWrite.Shared, Mail.Send.Shared, Calendars.ReadWrite.Shared, Contacts.ReadWrite.Shared to the Entra guide now (13 scopes) and to Task 7's requested scope list — spec wants consent once; Phase 2 shared mailboxes/delegated calendars need them — if wrong, users see 4 extra consent lines; admin-consent prompt slightly longer.
- Ruling R5: Store API additions for Task 6 — upsert_messages -> Result<Vec<MessageId>> (stored ids, input order); delete_messages/clear_folder -> Result<Vec<MessageId>> (removed ids); new apply_sync_page(&FolderId, upserts, deleted_remote_ids, sync_state:&str) -> Result<PageApplied{stored_ids, removed_ids}> in ONE transaction incl. recount; new reset_folder(&FolderId) -> Result<Vec<MessageId>> clears messages+sync_state+recount atomically; new message_id_for_remote(&FolderId,&str) -> Result<Option<MessageId>> — reviewer finding; Task 6 needs atomic pages and real ids for bodies — if wrong, small API churn.
- Ruling R6: FolderDelta gains `flag_updates: Vec<FlagUpdate { remote_id: String, is_read: bool, is_flagged: bool }>` — IMAP fills it for all known UIDs on the final page of an incremental pass (no CONDSTORE until M5); Graph leaves it empty (delta upserts carry flags). Task 6 applies them inside the page transaction (UPDATE only where changed). — brief requires flag refresh; cheapest correct shape — if wrong: O(N) small UPDATE checks per sync until M5.
- Ruling R7: FolderDelta gains `server_remote_ids: Option<Vec<String>>` — IMAP sets Some(all UIDs currently on server, from UID SEARCH ALL) on the final page of an incremental pass; Task 6 deletes cached rows whose remote_id is absent. IMAP no longer computes deleted_remote_ids from the UID span (missing_uids removed); deleted_remote_ids stays for Graph @removed. — bounded by message count, not UID span — if wrong: 50k strings per sync.
- Ruling R8: snippet = main FETCH requests BODY.PEEK[TEXT]<0.1024>; on a parse/protocol error for that, the provider falls back for the rest of the session to the raw-connection path (kept for GreenMail), never per-page re-login otherwise. Multipart partials are passed through parse_message(header+partial) instead of bailing. — controller decision + real servers — cost if wrong: GreenMail tests still exercise fallback only.
- Ruling R9: also fix minors 7 (decode modified UTF-7 display name), 8 (skip \Noselect/\NonExistent), 9 (only top-level INBOX is Inbox) in this round — visible correctness bugs on real servers, cheap now.
- Ruling R10: Task 6 replaces Store::apply_sync_page's parameter list with `&PageChanges<'_> { upserts: &[MessageSummary], deleted_remote_ids: &[String], flag_updates: &[FlagChange { remote_id: String, is_read: bool, is_flagged: bool }], retain_only_remote_ids: Option<&[String]>, sync_state: &str }` (store-owned types; store does not depend on provider), applying all in ONE transaction; removed_ids include rows pruned by retain_only. SyncEngine deletes body files for removed ids. Store calls stay synchronous from SyncEngine in M1 (short WAL writes). — R6/R7 need atomic application — if wrong: API churn in store tests.
- Ruling R11: prefetch must stop and surface NeedsSignIn (emit SyncStatus::NeedsSignIn, return error) instead of swallowing it — plan Review Focus #3 (revoked token → NeedsSignIn, no loop) — fixed in same round.
- Ruling R12: Graph reset (410/syncStateNotFound/resyncRequired) must return more = true (empty upserts, cleared cursor) so the engine's page loop immediately re-syncs from scratch in the same pass — my earlier instruction said "engine calls again" but the engine only loops on more — if wrong: folder empty until next sync.
- Ruling R13: fold Task 9 minors #1 (redact ImapAccountInput Debug), #2 (validate image mime subtype for data URI), #4 (roll back new Microsoft account row if TokenManager::store fails) into fix round 1 — credential/security hygiene, cheap — cost if wrong: small extra diff.
- Ruling R14: build-apple.sh builds wbmc-ffi with feature debug-tools for M1 (owner-only builds) so the generated Swift has debugRefreshToken; the Swift Debug menu is compiled only under #if DEBUG. Release builds must drop debug-tools before any public distribution (M8). — Task 10 needs the method in generated bindings — cost if wrong: token-export API present in a release binary (not reachable from UI).
- Ruling R15: also fix minor #2 ("Sign in again" shown for IMAP and always starts Microsoft flow → only for microsoft accounts) and #4 (clear cached rule-compile failure) in this round — visible bug / cheap.
- Ruling R16: fix I1-I3 plus hoisted arrays + remove stale WBMCBlockRemoteContent-v1 list now; svg/foreign-content raw-text handling deferred — rule list blocks loads regardless, so it's defence in depth — cost if wrong: a meta inside svg title survives; WebKit ignores meta CSP outside head anyway.
- Ruling R17: after fix round 2 lands, apply both — (a) emit metadata as the very first bytes of the document (meta before any content goes into the implicit head; removes reliance on head detection), (b) drop EVERY <link> element whose href isn't data: (allowlist, not rel denylist; also covers entity-encoded rel values) — cost if wrong: mail using <link rel=stylesheet> to remote CSS loses styling, which was blocked anyway.
- Ruling R18: supersede R16 deferral — inside raw-text regions keep recognizing and stripping link/meta/base (over-strip literal text only, raw state unchanged); also place metadata after a single leading <!DOCTYPE ...> token (only a doctype, nothing else) to avoid quirks mode — cost if wrong: literal "<link" text in script/style gets eaten, harmless with JS off.
- Ruling R19: controller applied a 5-line fix directly instead of another builder round — single-branch change with an exact fix and test; cost if wrong: final review catches it.
- Ruling R20: run the 50k benchmark on Dovecot (compose profile "big") — GreenMail's per-UID fetch is O(mailbox) and unrepresentative; M5's planned server swap brought forward for the benchmark only — cost if wrong: Dovecot masks a core slowness that GreenMail exposed (mitigated: still record the GreenMail result).
- Ruling R21: defer adaptive page shrinking on IMAP command timeout (and per-page partial progress on timeout) to M5 robustness — M1 targets mainstream servers — cost if wrong: very slow servers show no mail until M5.
- Ruling R22: final fix wave = I1+I2+I3+M1+M2 in ONE Cursor dispatch (can run network tests and xcodebuild) — cost if wrong: one more scoped round.
- Ruling R23: controller removed the overall cap from sync_folder only (commands individually timed; Graph per-request 60 s); cap kept on list_folders/fetch_raw — cost if wrong: a protocol hang inside sync_folder not covered by per-command timeouts could pin the slot (covered by per-command timeouts today).
