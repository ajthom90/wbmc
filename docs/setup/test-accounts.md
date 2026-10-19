# Test accounts for WBMC live tests

> **SAFETY RULE: automated tests and AI agents use ONLY the dedicated test mailboxes: `wbmc-test1@`, `wbmc-test2@`, `wbmc-shared@`, and `wbmc-room@`. They must never read, send, move, or delete real users' mail, and never use a real person's credentials or tokens.**
>
> Corollary: never add the test users to company distribution lists, groups, or Teams. No mail-flow (transport) rule is needed or wanted; the isolation comes from the accounts being separate and unlisted.

You create these in your own Microsoft 365 tenant (replace `contoso.com` below with your domain). Role needed: User Administrator or License Administrator for the users, Exchange Administrator for the shared and room mailboxes. Finish [microsoft-entra-app.md](microsoft-entra-app.md) first.

## 1. Create the two test users

Do this once for `wbmc-test1` and once for `wbmc-test2`.

1. Sign in to the Microsoft 365 admin center: <https://admin.cloud.microsoft> (also reachable at admin.microsoft.com).
2. Go to **Users** > **Active users** and select **Add a user**. (In Simplified view, select **Add user**.)
3. **Set up the basics**: first name `WBMC`, last name `Test1` (or `Test2`), display name `WBMC Test1`, username `wbmc-test1`, domain your primary domain. Choose a strong password and clear **Require this user to change their password when they first sign in** if you want to skip that step (or sign in once yourself and change it). Select **Next**.
4. **Assign product licenses**: pick your location and assign the cheapest license that includes a mailbox: **Exchange Online (Plan 1)** or **Microsoft 365 Business Basic**. Nothing else is needed. Select **Next**.
5. Skip **Optional settings** (no admin roles), then **Finish adding**.
6. Repeat for `wbmc-test2`.
7. Sign in once as each user at <https://outlook.office.com> in a private window to confirm the mailbox exists and to complete any first-run prompts. If your tenant enforces MFA, finish MFA registration now. Do not exempt these users from your security policies without a reason.

## 2. Create the shared mailbox

1. In the Microsoft 365 admin center go to **Teams & Groups** > **Shared mailboxes** (use **Show all** in the left navigation if it is hidden). Alternatively use the Exchange admin center, <https://admin.exchange.microsoft.com>, under **Recipients** > **Mailboxes**.
2. Select **Add a shared mailbox**, name it `wbmc-shared`, check the email address is `wbmc-shared@contoso.com`, then **Save changes**. It can take a few minutes before members can be added.
3. Select **Add members to this mailbox** > **Add members**, tick `wbmc-test1` and `wbmc-test2`, then **Add**.
4. In the Exchange admin center go to **Recipients** > **Mailboxes**, select `wbmc-shared`, then **Manage mailbox delegation** and confirm both test users have **Read and manage (Full Access)** and **Send as**. Add them if missing.

No license is needed for a shared mailbox (under 50 GB, no archive, no litigation hold). Shared mailboxes have sign-in blocked by default; leave it that way. Tests reach it as a delegate through a test user's token.

## 3. Create the room mailbox

1. In the Exchange admin center, <https://admin.exchange.microsoft.com>, go to **Recipients** > **Resources**.
2. Select **Add a room resource**.
3. **Name**: `wbmc-room`. **Resource mail address**: `wbmc-room` at your domain. Select **Next**.
4. Optional properties (capacity, location): set capacity to `4` if you like, then **Next**.
5. **Booking delegate settings**: keep **Automatically accept/decline meeting requests** (the default). **Next**.
6. **Create**, then **Done**.

A room mailbox needs no license. Leave it out of any room lists your users see.

## 4. Environment variables the live tests read

| Variable | Value |
| --- | --- |
| `WBMC_TEST_M365` | `1` to enable the live Microsoft 365 tests; unset or anything else skips them |
| `WBMC_TEST_M365_USER` | the sign-in address of the test user the token belongs to, e.g. `wbmc-test1@contoso.com` |
| `WBMC_TEST_M365_REFRESH_TOKEN` | that user's OAuth refresh token (see section 5) |

**Refresh tokens are secrets.** Anyone holding one can read and send that mailbox's mail without a password. Keep them in a local, untracked file, for example `~/.config/wbmc/test.env`:

```sh
mkdir -p ~/.config/wbmc && chmod 700 ~/.config/wbmc
cat > ~/.config/wbmc/test.env <<'EOT'
export WBMC_TEST_M365=1
export WBMC_TEST_M365_USER="wbmc-test1@contoso.com"
export WBMC_TEST_M365_REFRESH_TOKEN="paste-token-here"
EOT
chmod 600 ~/.config/wbmc/test.env
```

Then `source ~/.config/wbmc/test.env` before running the live tests. Never commit this file, paste the token into an issue, chat, or CI log, or store it inside the repo. If a token leaks, revoke it (sign out all sessions for that test user in the Entra admin center under **Users** > the user > **Revoke sessions**) and capture a new one.

## 5. Capture a refresh token

This depends on the macOS app, which is not built yet. The steps below are available once the macOS app lands (Task 10 of the M1 plan).

1. Build and run a **Debug** build of the macOS app (the menu item is not in Release builds).
2. Add `wbmc-test1@contoso.com` as an account and complete the Microsoft sign-in.
3. In the menu bar choose **Debug** > **Copy refresh token...** with that account selected. The token is copied to the clipboard.
4. Paste it into `~/.config/wbmc/test.env` as `WBMC_TEST_M365_REFRESH_TOKEN`, then clear the clipboard.

Refresh tokens expire after a long period of inactivity (typically 90 days) or when the user's sessions are revoked or the password changes. If the live tests start failing with an invalid-grant error, repeat this section.

## Sources (Microsoft Learn)

- Add users and assign licenses: <https://learn.microsoft.com/en-us/microsoft-365/admin/add-users/add-users>
- Create a shared mailbox: <https://learn.microsoft.com/en-us/microsoft-365/admin/email/create-a-shared-mailbox>
- Manage resource (room) mailboxes in Exchange Online: <https://learn.microsoft.com/en-us/exchange/recipients-in-exchange-online/manage-resource-mailboxes>
