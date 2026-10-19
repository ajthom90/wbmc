# M1 demo checklist

Date: 2026-10-19. Machine: Apple Silicon Mac (M2 Ultra, Mac14,14), macOS 27.0 (26A428).

Headless numbers are from `cargo run --release -p wbmc-ffi --example m1_verify` with `WBMC_SECRETS=memory`. The normal seed uses GreenMail (`alice@wbmc.test` / `password`, IMAP plain `127.0.0.1:3143`). The 50,000-message run uses Dovecot on `127.0.0.1:4143` (`scripts/dev-mail-up.sh --big`). No other Docker projects were stopped or modified. No real mailbox was used. GreenMail was left at **509** messages.

## Step 1 — Local account, normal seed

- [x] Inbox has 500+ messages. Measured inbox count **509**.
- [x] Newest message is at the top within 3 s of adding the account. The first page counted only once it held 50 rows headed by `Seed message 0499`: **557 ms** after `add_imap_account` started (login included). Those rows were newest-first. Full sync reached idle in **21695 ms**.
- [x] Corpus bodies, measured through `message_content` (the core decode, not the on-screen web view):
  - Café subject: `Café ☕ meeting`.
  - Latin-1 body contains `Viele Grüße`.
  - Shift-JIS body contains `こんにちは世界`.
  - Inline `cid:` image is rewritten to `data:image/png;base64,`.
  - `remote_images_and_script` HTML still contains `https://tracker.example.com/p.gif` and `https://evil.example.com`, and contains no `data:` URI. The core did not rewrite or inline those remote URLs.
- [ ] **Owner check — visual rendering and remote-image blocking.** The core HTML check above does not open WKWebView. In the app, open the Café, Latin-1, Shift-JIS, and inline-image messages and confirm they render. Open `Remote content` and confirm no remote image appears. In Console, confirm the process makes no request to `tracker.example.com`. WKWebView blocking is covered by `apps/apple/Tests/SafeHTMLViewTests.swift`.

## Step 2 — 50,000 messages

### GreenMail finding (recorded, not the benchmark)

GreenMail cannot serve this mailbox. A direct IMAP session against a 50,009-message GreenMail inbox took **6.781 s** for `UID SEARCH ALL` and **111.103 s** for one 200-message summary `UID FETCH` (`BODY.PEEK[TEXT]<0.1024>`, the query the core sends). The core's IMAP command timeout is 30 s, so `m1_verify --big` against GreenMail aborted at about **36 s** with `network error: IMAP fetch timed out` and stored no page. Peak RSS at that abort was **12,681,216 bytes (12 MiB)**, which is not a full-backfill sample. Adaptive paging is deferred to M5 (ruling R21). The large-mailbox benchmark moved to Dovecot.

### Dovecot (`127.0.0.1:4143`, profile `big`)

Maildir of **50009** messages (9 corpus + 50000 generated), written by `seed.py --dovecot-maildir`. `m1_verify --big --host 127.0.0.1 --port 4143`:

- [x] Time to first page: **1977 ms** (within 3 s). Newest subject `Seed message 49999`, newest-first. Inbox count **50009**.
- [x] Time to full backfill: sync reached idle in **622135 ms** (about 10.4 min). That clock includes the inbox body prefetch the engine runs before it reports idle.
- [x] Peak RSS during the run: **23,117,824 bytes (22 MiB)**. Target < 500 MB: pass. `ru_maxrss` on macOS is the process high-water mark in bytes, not a sample taken after sync.
- [ ] **Owner check — scrolling stays smooth.**
- [ ] **Owner check — Activity Monitor.** Headless peak was 22 MiB; confirm in Activity Monitor if you want the GUI sample.

## Step 3 — Microsoft 365

- [ ] **Owner check.** Needs the owner's Entra application (client) ID. Agents must not sign in or touch a real mailbox.

1. Register the public client and grant the delegated Graph permissions in [docs/setup/microsoft-entra-app.md](../setup/microsoft-entra-app.md). Put the Application (client) ID in `config/oauth-clients.toml`. Do not commit a token or a client secret.
2. Create `wbmc-test1` as in [docs/setup/test-accounts.md](../setup/test-accounts.md).
3. Build and run a **Debug** build (the Debug menu is not in Release). See "Build and add the local account" below.
4. Add Account, leave **Microsoft 365 / Outlook.com** selected, and choose **Sign In**. Finish the browser sign-in as `wbmc-test1@…`.
5. Expected: folders show roles, the Inbox lists mail, and an HTML message renders.
6. Menu bar **Debug** > **Copy refresh token…**. Select the Microsoft account, choose **Copy…**, and paste the token into `~/.config/wbmc/test.env` as `WBMC_TEST_M365_REFRESH_TOKEN`, next to `WBMC_TEST_M365=1` and `WBMC_TEST_M365_USER`. Clear the clipboard. Never commit that file.
7. Run:

   ```sh
   source ~/.config/wbmc/test.env
   cargo test -p wbmc-core --test graph_live -- --ignored
   ```

   Expected: PASS. The test skips unless `WBMC_TEST_M365=1`, and it reads `WBMC_TEST_M365_USER` and `WBMC_TEST_M365_REFRESH_TOKEN`.

## Step 4 — Offline relaunch

An earlier headless run failed here: `message_content` required the IMAP password before it would read a cached body, and `WBMC_SECRETS=memory` does not keep that password across processes. Commit `44e3a7c` serves a cached body without credentials.

Re-ran after a normal GreenMail sync (`m1_verify`, then `m1_verify --offline` on that data dir). Sync was not started. GreenMail was left running.

- [x] Accounts, folders, the Inbox first page, and one cached body were readable in **3 ms** from process start. **1** account, inbox **509**, first page **20** rows, body subject `Seed message 0499`, **204** cached `.eml` files. Target under 1000 ms: pass.
- [ ] **Owner check — app relaunch with the network off.** Quit WBMC, turn Wi-Fi off, and relaunch. The cached Inbox should be on screen in under 1 s, and a message that was already opened should still show its body. Do not stop unrelated Docker containers. Wi-Fi off does not make `127.0.0.1` unreachable.

## Build and add the local account

1. Start GreenMail and the normal seed (idempotent when `Seed message 0499` is already present):

   ```sh
   scripts/dev-mail-up.sh
   ```

2. Build the Rust core, generate the Xcode project, and open or build the app:

   ```sh
   scripts/build-apple.sh
   xcodegen --spec apps/apple/project.yml
   open apps/apple/WBMC.xcodeproj
   ```

   Or, without opening Xcode:

   ```sh
   xcodebuild -project apps/apple/WBMC.xcodeproj -scheme WBMC-macOS -configuration Debug build
   ```

   Run the **Debug** configuration. The **Debug** menu exists only in Debug builds.

3. In WBMC, choose **Add Account** (toolbar, or the empty-window button). Choose **Other (IMAP)**.
4. Email: `alice@wbmc.test`. In a Debug build, an address ending in `@wbmc.test` fills the server fields: IMAP host `localhost`, port `3143`, security **None**, SMTP host `localhost`, port `3025`, security **None**. Username follows the email (`alice@wbmc.test`). Password: `password` (the GreenMail user in `dev/mail/docker-compose.yml`).
5. Choose **Add Account**. The Inbox should show 500+ messages with the newest at the top within 3 s. Headless result: **557 ms**, **509** messages.

To repeat the large-mailbox case, use Dovecot rather than GreenMail:

```sh
scripts/dev-mail-up.sh --big
cargo run --release -p wbmc-ffi --example m1_verify -- --big --host 127.0.0.1 --port 4143
docker compose -p wbmc-dev -f dev/mail/docker-compose.yml --profile big stop dovecot
```

`scripts/dev-mail-up.sh --big` writes `dev/mail/dovecot-data/` (git-ignored) with `python3 dev/mail/seed.py --dovecot-maildir` and starts only the `dovecot` service (compose profile `big`, image `dovecot/dovecot:2.4.5`). Plain IMAP is `127.0.0.1:4143`, IMAPS is `127.0.0.1:4993`, user `alice@wbmc.test`, password `password`. `docker compose up` without the profile does not start Dovecot. GreenMail stays the 509-message demo.
