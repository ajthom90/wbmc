# Set up the WBMC project in Google Cloud

WBMC signs in to Gmail with OAuth 2.0 and uses the Gmail, Calendar, People and Tasks APIs. For that it needs one **Desktop app** OAuth client in a Google Cloud project that you own. The **client ID** is public and is committed to the repo. The **client secret** is not truly confidential for a desktop app, but WBMC still keeps it out of git: it goes in a git-ignored local file and is compiled into your builds.

Time needed: about 20 minutes. Role needed: any Google account that can create a Cloud project.

Portal labels change from time to time (Google has been moving this area to "Google Auth Platform"). If something below is not where it says, look for the same wording nearby; the intent of each step is stated.

## 1. Create the project and enable the APIs

1. Open the Google Cloud console: <https://console.cloud.google.com>.
2. Use the project picker in the top bar, select **New project**, set **Project name** to `WBMC`, and select **Create**. Then switch to the new project.
3. Go to **APIs & Services** > **Library**.
4. Search for and **Enable** each of these:
   - **Gmail API**
   - **Google Calendar API**
   - **People API**
   - **Google Tasks API**

## 2. Configure the OAuth consent screen

1. Go to **APIs & Services** > **OAuth consent screen** (newer layout: **Google Auth Platform** > **Branding**, **Audience** and **Data Access**). Select **Get started** if prompted.
2. **App name**: `WBMC`. **User support email**: your email address.
3. **Audience** / **User type**: choose **External**.
4. **Developer contact information**: your email address. Finish the wizard.
5. Open **Data Access** (older layout: **Edit app** > **Scopes**) and select **Add or remove scopes**. Add every scope below. If a scope is not in the list, paste it into **Manually add scopes**.
   - `openid`
   - `email`
   - `profile`
   - `https://mail.google.com/`
   - `https://www.googleapis.com/auth/calendar`
   - `https://www.googleapis.com/auth/contacts`
   - `https://www.googleapis.com/auth/contacts.other.readonly`
   - `https://www.googleapis.com/auth/tasks`
6. Select **Update**, then **Save**. Gmail's `https://mail.google.com/` is a **restricted** scope; Calendar, Contacts and Tasks are **sensitive**. That is expected.
7. Open **Audience** (older layout: **Test users**) and under **Test users** select **Add users**. Add the Gmail address you will test with (see [test-accounts.md](test-accounts.md)). Save.

## 3. Publish the app

1. On **Audience** (older layout: **OAuth consent screen**), find **Publishing status**.
2. Select **Publish app** and confirm, so the status reads **In production**.

Why: while the status is **Testing**, Google expires refresh tokens after 7 days, which breaks the live tests and daily use. In production, refresh tokens last until revoked.

What to expect until the app is verified:

- Sign-in shows a "Google hasn't verified this app" screen. Choose **Advanced** > **Go to WBMC (unsafe)** to continue.
- An unverified app that uses restricted scopes is capped at **100 users**.

That is fine until release.

## 4. Create the Desktop client

1. Go to **APIs & Services** > **Credentials** (newer layout: **Google Auth Platform** > **Clients**).
2. Select **Create credentials** > **OAuth client ID** (newer layout: **Create client**).
3. **Application type**: **Desktop app**.
4. **Name**: `WBMC macOS`. Select **Create**.
5. Copy the **Client ID** and the **Client secret** from the dialog (you can reopen the client later to see them again).

Do not register a redirect URI. WBMC uses the loopback redirect `http://127.0.0.1:{port}` on a free port at sign-in time, and Desktop clients allow loopback redirects automatically.

## 5. Put the values in the repo

1. Open `config/oauth-clients.toml` and paste the client ID between the quotes:

   ```toml
   [google]
   client_id = "1234567890-abcdefg.apps.googleusercontent.com"   # your real client ID
   ```

   Commit this. The client ID is public.
2. Create `config/oauth-clients.local.toml` (it is git-ignored) with the client secret:

   ```toml
   [google]
   client_secret = "paste-secret-here"
   ```

   Never commit this file. Check with `git status` that it does not appear.
3. Build as usual. `core/wbmc-core/build.rs` reads the local file and compiles the secret into the app. Without the file, it falls back to the `WBMC_GOOGLE_CLIENT_SECRET` environment variable (useful in CI). If neither is set, the build still succeeds but Google sign-in is unavailable.

If the secret leaks, open the client in the console and reset or rotate the secret, then update the local file.

## 6. Before public release (owner task)

The unverified-app screen and the 100-user cap are not acceptable for a public release. Before releasing you must:

1. Submit the app for **Google OAuth verification** (needs a homepage, privacy policy, and a demo video of each scope in use).
2. Pass the **CASA** (Cloud Application Security Assessment) by an authorized assessor for the restricted Gmail scope, and renew it **every year**.

See section 10 of the design spec, `docs/superpowers/specs/2026-10-19-wbmc-design.md`.

## Troubleshooting

- `Error 400: redirect_uri_mismatch`: the client type is not **Desktop app** (a **Web application** client requires registered redirect URIs). Create a Desktop client.
- `Error 403: access_denied` / "app not verified" with no continue option: the Google account is not listed under **Test users** while the app is still in Testing, or the app is not published.
- Sign-in works but stops after about a week: the app is still in **Testing**. Publish it (step 3), then sign in again.
- `invalid_client` at token exchange: the client secret was not compiled in. Check `config/oauth-clients.local.toml` or `WBMC_GOOGLE_CLIENT_SECRET`, then rebuild.

## Sources (Google)

- Configure the OAuth consent screen: <https://support.google.com/cloud/answer/10311615>
- OAuth 2.0 for desktop apps (loopback redirect): <https://developers.google.com/identity/protocols/oauth2/native-app>
- Gmail API scopes: <https://developers.google.com/gmail/api/auth/scopes>
- Restricted scope verification and CASA: <https://developers.google.com/identity/protocols/oauth2/production-readiness/restricted-scope-verification>
