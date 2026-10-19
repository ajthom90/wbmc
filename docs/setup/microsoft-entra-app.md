# Register the WBMC app in Microsoft Entra

WBMC signs in to Microsoft 365 with OAuth 2.0 and talks to Microsoft Graph. For that it needs one **public client** app registration in your tenant. There is no client secret: WBMC is a native app, so it uses the authorization code flow with PKCE and a loopback redirect. The resulting **Application (client) ID** is public and is committed to the repo; nothing else from this guide is.

Time needed: about 10 minutes. Role needed: Application Developer (to register) plus Cloud Application Administrator or Global Administrator (to grant admin consent).

Portal labels change from time to time. If something below is not where it says, look for the same wording nearby; the intent of each step is stated.

## 1. Register the app

1. Sign in to the Microsoft Entra admin center: <https://entra.microsoft.com>.
2. If you belong to several tenants, use the **Settings** (gear) icon in the top bar, open **Directories + subscriptions**, and switch to your Microsoft 365 tenant.
3. In the left navigation go to **Entra ID** > **App registrations**, then select **New registration**.
4. **Name**: `WBMC`.
5. **Supported account types**: choose **Accounts in any organizational directory and personal Microsoft accounts**. (The drop-down may label this "Any Entra ID Tenant + Personal Microsoft accounts".)
6. Leave **Redirect URI** empty here; you set it in the next step (the form does offer "Public client/native (mobile & desktop)", but it is cleaner to add `http://localhost` under **Authentication**).
7. Select **Register**.
8. On the **Overview** page, copy the **Application (client) ID** (a GUID). You need it in step 5.

## 2. Add the redirect URI (Mobile and desktop applications)

1. In the app, open **Manage** > **Authentication**. (In the newer layout this page may be called **Authentication (Preview)** and have the tabs **Redirect URI configuration** and **Settings**.)
2. Select **Add a platform** (newer layout: **Add Redirect URI**) and choose **Mobile and desktop applications**.
3. Under custom redirect URIs, enter exactly `http://localhost` and select **Configure** (or **Save**).
4. Do not add a port. For loopback addresses Entra ignores the port when matching, so WBMC can listen on any free port at sign-in time.
5. Do not add other redirect URIs, and do not register several `localhost` URIs that differ only by port.

## 3. Allow public client flows

1. Still on **Authentication**, find **Advanced settings** > **Allow public client flows** (newer layout: the **Settings** tab).
2. Set **Enable the following mobile and desktop flows** to **Yes**.
3. Select **Save**.

## 4. Add the delegated Microsoft Graph permissions

Requesting everything now means users never get a second consent prompt when a later milestone needs calendar, contacts, or tasks.

1. Open **Manage** > **API permissions**. `User.Read` is already listed.
2. Select **Add a permission** > **Microsoft Graph** > **Delegated permissions**.
3. Search for and tick each of these:
   - `offline_access` (under **OpenId permissions**)
   - `User.Read`
   - `Mail.ReadWrite`
   - `Mail.Send`
   - `MailboxSettings.ReadWrite`
   - `Calendars.ReadWrite`
   - `Contacts.ReadWrite`
   - `Tasks.ReadWrite`
   - `People.Read`
   - `Mail.ReadWrite.Shared`
   - `Mail.Send.Shared`
   - `Calendars.ReadWrite.Shared`
   - `Contacts.ReadWrite.Shared`

   The `.Shared` scopes are for delegated access to shared mailboxes and delegated calendars (Phase 2), so you consent only once.
4. Select **Add permissions**.
5. Check that all 13 appear under **Configured permissions** with type **Delegated**. Do not add any **Application** permissions.

## 5. Grant admin consent for your tenant

None of these permissions strictly requires an admin, but granting consent once means your tenant's users (including the test users) are never prompted, and it avoids failures if your tenant restricts user consent.

1. On **API permissions**, select **Grant admin consent for \<your tenant name\>**, then **Yes**.
2. Select **Refresh** and confirm the **Status** column shows **Granted for \<your tenant name\>** for every permission.

Admin consent applies only to your tenant. Users in other organizations consent (or their admins do) the first time they sign in. Personal Microsoft accounts consent on their own sign-in.

## 6. Put the client ID in the repo

1. Open `config/oauth-clients.toml`.
2. Paste the Application (client) ID between the quotes:

   ```toml
   [microsoft]
   client_id = "00000000-0000-0000-0000-000000000000"   # your real GUID
   authority = "https://login.microsoftonline.com/common"
   ```
3. Leave `authority` as `https://login.microsoftonline.com/common` (it covers work, school, and personal accounts).
4. Commit it. The client ID is public. Never commit tokens, passwords, or a client secret (there is none).

## Troubleshooting

- `AADSTS50011` (reply URL mismatch): the redirect URI is not `http://localhost` under **Mobile and desktop applications**, or it was added under **Web** by mistake.
- `AADSTS7000218` (client assertion or secret required): **Allow public client flows** is not set to **Yes**.
- The portal refuses `http://127.0.0.1`: use `http://localhost`. Entra's portal text box rejects `http` with `127.0.0.1`; that form is only possible by editing the manifest, which is not needed.
- Personal Microsoft accounts rejected: re-check the supported account types in step 1 (**Authentication** > **Supported account types**, or the manifest `signInAudience` = `AzureADandPersonalMicrosoftAccount`).

## Sources (Microsoft Learn)

- Register an application: <https://learn.microsoft.com/en-us/entra/identity-platform/quickstart-register-app>
- Configure API permissions and admin consent: <https://learn.microsoft.com/en-us/entra/identity-platform/quickstart-configure-app-access-web-apis>
- Redirect URI rules, loopback and port handling: <https://learn.microsoft.com/en-us/entra/identity-platform/reply-url>
- Microsoft Graph permissions reference: <https://learn.microsoft.com/en-us/graph/permissions-reference>
