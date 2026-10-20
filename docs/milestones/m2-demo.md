# M2 demo checklist

Date: 2026-10-20. Machine: Apple Silicon Mac (M2 Ultra, Mac14,14), macOS 27.0 (26A428).

Headless numbers come from `cargo run --release -p wbmc-ffi --example m1_verify` with `WBMC_SECRETS=memory`. The setup is the same as M1 (see [m1-demo.md](m1-demo.md)): GreenMail at `127.0.0.1:3143` for the 509-message seed, and Dovecot at `127.0.0.1:4143` for the 50,000-message run.

**The machine was under load during these runs.** Another project's simulator UI tests were running, with load averages of 10–14. Backfill times are therefore much slower than M1's. Builds from earlier M2 tasks show the same slowdown when re-run now, so this is not a regression. First page and RSS are the numbers that matter. No other project's containers or processes were touched, and no real mailbox was used.

## Step 1 — Regression: M1 numbers still hold

| Run | First page | Backfill to idle | Peak RSS | M1 reference |
|---|---|---|---|---|
| GreenMail, 509 messages | **580 ms** (615 ms on a re-run) | ~17 s (loaded) | **15 MiB** | 557 ms / 21.7 s |
| Dovecot, 50,009 messages | **1026 ms** | 227,635 ms (~3.8 min, loaded) | **21 MiB** | 880 ms / 591,728 ms / 22 MiB |

- [x] `pass: true` on both runs. In both, the first page was newest-first and arrived within 3 s.
- [x] The 50,000-message backfill finished in well under half M1's time, from the batched body prefetch (`fetch_raw_many`, one IMAP session, 20 UIDs per chunk).

## Step 2 — Auto-setup (address only)

- [x] `WBMC_TEST_DETECT=1 cargo test -p wbmc-core live_detect -- --ignored` passes against real DNS: MX lookups for gmail.com, outlook.com and icloud.com, then `detect` returns Google for a gmail.com address, Microsoft for outlook.com, and the bundled iCloud profile (`imap.mail.me.com:993` TLS) for icloud.com.
- [ ] **Owner check — Add Account by address only.** In the app, choose Add Account and type only an address:
  - `alice@wbmc.test` should go straight to the password prompt with the local server filled in;
  - a gmail.com address should offer Google sign-in;
  - an outlook.com address should offer Microsoft sign-in;
  - a domain that cannot be detected should show the provider list.

## Step 3 — Unified inbox

- [x] `Store::unified_messages` and the FFI `unified_messages` are covered by tests: two accounts, newest-first across both, each row carrying `account_id`.
- [ ] **Owner check — All Inboxes.** Add `alice@wbmc.test` and `bob@wbmc.test` (password `password`). Then open **All Inboxes** and confirm that both accounts' mail appears together, newest first.

## Step 4 — Gmail

- [ ] **Owner check.** This needs your Google Cloud OAuth client. Agents must not create it or sign in.
  1. Follow [docs/setup/google-cloud-project.md](../setup/google-cloud-project.md): create the project, enable the Gmail API, and create a Desktop OAuth client.
  2. Put the client ID in `config/oauth-clients.toml` under `[google]`.
  3. Put the client secret only in the git-ignored `config/oauth-clients.local.toml`, or in `WBMC_GOOGLE_CLIENT_SECRET`. Never commit it.
  4. Rebuild with `scripts/build-apple.sh`. In the app, add your dedicated **test** Gmail address and finish sign-in in the browser. Confirm that labels show as folders and the inbox fills.
  5. Optional headless check against the test account: set `WBMC_TEST_GMAIL=1`, `WBMC_TEST_GMAIL_USER` (the test address) and `WBMC_TEST_GMAIL_REFRESH_TOKEN`, then run `cargo test -p wbmc-core --test gmail_live -- --ignored`. Keep the refresh token out of files and shell history.

## Step 5 — Yahoo / AOL / iCloud (optional)

- [ ] **Owner check.** Each needs an app password created on the provider's site (Yahoo SLN15241, Apple support 102654). Add the account by address and paste the app password. The bundled profiles, verified 2026-10-20, are:
  - Yahoo: `imap.mail.yahoo.com:993` TLS;
  - AOL: `imap.aol.com:993` TLS;
  - iCloud: `imap.mail.me.com:993` TLS (the username is tried as the local part first, then the full address).

## Step 6 — Microsoft 365

- [ ] **Owner check.** This is unchanged from M1 and still needs the Entra application (client) ID; see [m1-demo.md](m1-demo.md) Step 3. M2 moves Microsoft onto the shared OAuth flow, so re-run the sign-in once the ID is in `config/oauth-clients.toml`.

## Step 7 — macOS tests

- [x] `xcodebuild build-for-testing -scheme WBMC-macOS` builds with no Swift warnings.
- [x] The unit suite passed. This includes `AddAccountState` and `remoteSubresourceNeverReachesServer`, which runs a local NWListener and asserts that it never receives a connection.
- [ ] `WBMCLaunchTests` (UI) could not run. The test-automation daemon was held by another project's simulator tests, and the run failed with "Timed out while enabling automation mode". To re-run it once the machine is idle:
  ```
  xcodebuild test -project apps/apple/WBMC.xcodeproj -scheme WBMC-macOS -destination 'platform=macOS'
  ```

## Re-running

```
scripts/dev-mail-up.sh                # GreenMail, 509 messages
WBMC_SECRETS=memory cargo run --release -p wbmc-ffi --example m1_verify
scripts/dev-mail-up.sh --big          # adds Dovecot, 50,009 messages
WBMC_SECRETS=memory cargo run --release -p wbmc-ffi --example m1_verify -- --big --host 127.0.0.1 --port 4143
docker compose -p wbmc-dev -f dev/mail/docker-compose.yml --profile big stop dovecot
WBMC_TEST_DETECT=1 cargo test -p wbmc-core live_detect -- --ignored
```

## Known follow-ups (deferred from M2 reviews)

None of these block M2. They are listed so later milestones can pick them up.

- (T1) build.rs rerun-if-changed on a usually-missing file reruns build script every build (watch config dir instead)
- (T1) build.rs emits rustc-env unescaped; reject secrets containing \n/\r
- (T1) build.rs hand TOML parser untested; no escapes/quoted keys
- (T1) test-accounts.md "Sources (Microsoft Learn)" heading now after Gmail sections
- (T2) body-store put error aborts prefetch (should log+continue)
- (T2) cached_body reads full bytes for existence check in prefetch
- (T2) re-add writes keychain before row update
- (T2) FakeProvider double NeedsSignIn check
- (T3) microsoft authorize test redirect port check is tautological
- (T3) start_rejects_empty_client_id setup awkward
- (T3) loopback serial callback handling can delay real callback (not past timeout)
- (T6) empty schema_version row treated as v0 (harmless)
- (T6) no test that STEPS ascend/contiguous
- (T6) perf test only 40% inbox; sparse-inbox case slower (outside target)
- (T2) unreachable!() in default fetch_raw_many; map_login raw-string Alert check duplicated
- (T4) history_404_resets doesn't assert more==true; no engine-level reset→refill no-dup test
- (T4) reset empties folder before refill succeeds
- (T4) history loop gathers all pages into one transaction
- (T4) unparseable internalDate stored as 0
- (T4) mismatched Gmail states → InvalidInput forever (should reset)
- (T4) persistent non-token 400 on messages.list page → reset/backfill loop (unlikely)
- (T5) tests hard-code remembered iCloud values (mx_icloud, live_detect)
- (T5) no fallback-to-next-source test; FakeResolver has no delay; malformed.xml only truncated
- (T5) "wbmc.test" hard-coded in profiles.rs to exempt app_password_url
- (T5) userrealm Managed may include viral tenants (check IsViral?) — unverified
- (T5) BundledImap/bundled_imap_profiles pub → pub(crate)
- (T5) no test for chunked body cap without Content-Length
- (T7) duplicated google mock setup in tests; raw pipelined SMTP helper in tests; cancel test may not hit in-flight path
- (T7) notify_idle check-then-emit not atomic
- (T8) no positive control in remoteSubresourceNeverReachesServer (add rulesInstalled expect or plain-WKWebView control)
- (T8) manual(settings:) payload unused; email field in server settings can change address without re-detect
- (T8) startOAuth accepts http URLs (core always gives https)
- (T8) unnecessary @unchecked Sendable in test fakes; needsSignInMessage UI copy in CoreClient.swift
- (final N1) note_sync_generation after syncing lock release leaves a tiny M5 window— user-triggered sync during a keychain write only; costs one manual Refresh if hit.
- (final N2) keychain I/O under the shared token-map lock— serialises token lookups across accounts during a keychain call; costs brief UI sync latency with many accounts.
- (final N3) panic in keychain/store leaves account sync blocked until relaunch— panics there are not expected (errors are Results); costs a relaunch if it ever happens.
- (final N4) detect_account_uses_injected_env no longer proves injection— test weakness only; costs a missed regression in a test-only path.
- (final M8) Gmail body prefetch is sequential (default `fetch_raw_many`); add bounded concurrency.
