# WBMC (World's Best Mail Client): Product Design

*Status: draft for owner review · 2026-10-19*

Items marked **[Claude's call]** are decisions the owner handed to Claude. Everything else was stated or approved by the owner.

---

## 1. Intent

WBMC is a free, open-source mail, calendar and contacts app for **everyone who hates Outlook**. Most of those people are office workers whose company runs Microsoft 365.

The owner is the IT person who sets up office computers. Outlook is painful to set up and looks like "a Windows 98 app updated the bare minimum." Its search is bad, its layout is bad, and it's convoluted. WBMC should be the app you install instead and never think about again. It should:

- set itself up from just an email address and one sign-in;
- be fast;
- look like it belongs on a modern computer;
- do everything people use Outlook for every day.

WBMC also has a feature no mainstream client has: a **built-in MCP server**. It lets AI agents and bots read and manage the user's mailbox, calendar and contacts, with per-tool permissions the user controls.

### Success = "v1 is ready"

The owner would install WBMC on coworkers' machines in place of Outlook. That requires:

1. **It works.** Setup succeeds for Microsoft 365, Outlook.com, Gmail, Yahoo, iCloud and generic IMAP without manual server settings.
2. **It's fast.** See the performance targets in §9.
3. **It covers about 99% of what Outlook users do daily.** See the v1 checklist in §5.
4. **It's reliable.** No lost mail or drafts, stable sync, very few crashes.

## 2. Goals, non-goals, anti-goals

**Goals**
- Replace Outlook for Microsoft 365 office users, while also handling their personal accounts.
- Setup in one step, for end users and for IT (desktop deployment config, mobile MDM).
- Search that returns results as you type, across all accounts, even on very large mailboxes.
- Beautiful by default; customizable; any non-core area can be hidden with a feature switch.
- Agent access through MCP, under fine-grained user control.
- Native apps on macOS, Windows, Linux, iOS and iPadOS, with Android after them.
- Free and open source (MPL-2.0).

**Non-goals (v1)**
- Self-hosted Exchange Server (EWS). It's a possible later addition.
- Tuta and HEY. Neither allows access from third-party clients.
- A plugin or extension API. Design so one can be added later, but don't build it.
- Fully custom themes. v1 offers accent, density and font size only.
- Built-in AI features such as summaries or triage. Agents reach WBMC through MCP; WBMC doesn't ship its own AI.
- Making money. WBMC is free; the owner pays for the push relay.

**Anti-goals: these make it a failure even if it "works"**
- **No WBMC-run server ever holds user credentials or mail content.** The only exception is the opt-in "credentialed watcher" (§7), which is clearly labeled and also self-hostable.
- No ribbon. No "repair account" or profile-wizard flows. No setup screens asking for server names when auto-discovery can find them.
- An agent must never act on instructions found inside an email (prompt injection, §8).
- No telemetry without explicit opt-in.
- Never lose a draft or a message.

## 3. Platforms and build order

Owner-approved order. Each phase ships usable software.

| Phase | Deliverable | Notes |
|---|---|---|
| **1** | **Rust core + macOS app** | Usable daily on the owner's Mac with Microsoft 365, Gmail and IMAP accounts. Owner wants to watch progress here. **The first implementation plan covers Phase 1 only.** |
| 2 | Windows app | Where most Outlook users are. Includes the MSI installer and deployment config. |
| 3 | iOS and iPadOS apps + push relay | Mobile needs push to feel right, so they ship together. Includes MDM managed config and Intune. |
| 4 | Linux app | |
| 5 | Android app | Shares the core; uses FCM for push. |

Enterprise features (deployment config, MDM, Intune app protection) are added on each platform as it ships, not saved for the end.

### Phase 1 scope (owner-approved)

**In Phase 1:**
- Mail for Microsoft 365, Gmail and IMAP: read, search, compose, send, folders/labels, flags, categories.
- Calendar basics: views, create and edit, invites both ways, Teams links.
- Contacts and autocomplete.
- Basic Tasks (§5).
- The MCP server with permissions, approval prompts and the activity log.
- Feature switches.

**Deferred to Phase 2 (before anything ships to coworkers):**
- Scheduling assistant and room booking.
- Delegated and shared calendars; shared mailboxes and send-as/on-behalf.
- S/MIME and PGP; rules.
- .msg/mbox import.

### macOS distribution (owner decision)

**Direct download**, signed with Developer ID and notarized; there is no Mac App Store build. The background helper is a login item (SMAppService), and the `wbmc-mcp` command reaches it over a local socket. iOS and iPadOS go through the App Store.

### Accounts and test infrastructure

**The owner has:**
- an Apple Developer account;
- admin rights on a Microsoft 365 tenant;
- a Gmail account for testing.

**The owner will create** the Entra app registration and the Google Cloud project, following step-by-step instructions that Claude writes.

**IMAP/SMTP testing** uses a local Docker mail server (e.g. Stalwart or docker-mailserver/Dovecot), which also serves CalDAV/CardDAV where possible (otherwise add Radicale).

**Tenant safety [Claude's call].** The Microsoft 365 tenant may be a real organization. Automated tests and agents touch **only dedicated test users, a test shared mailbox and a test room**, never real users' mail. Destructive tests are limited to those mailboxes.

**Repository:** public on GitHub at `ajthom90/wbmc` from day one.

Remaining provider coverage (Yahoo, iCloud, the Tier 2 providers) can arrive in Phase 1 or 2, as the plan sees fit. All Tier 1 providers must be in place before the Windows release, since that release is what goes to coworkers.

**Dev environment:** the primary machine is an Apple Silicon Mac on macOS 27 "Golden Gate". A Windows 11 x64 VM on the owner's separate Hyper-V server is available on request. Ask the owner before Phase 2 work needs it.

## 4. Architecture [Claude's call]

```
┌───────────── Native UI per platform ─────────────┐
│ SwiftUI/AppKit (macOS) · SwiftUI (iOS/iPadOS)    │
│ WinUI 3 / C# (Windows) · GTK4+libadwaita (Linux) │
│ Jetpack Compose (Android)                         │
└───────────────▲──────────────────────────────────┘
                │ generated bindings (UniFFI; C# via uniffi-bindgen-cs or csbindgen)
┌───────────────┴───── wbmc-core (Rust) ───────────┐
│ Accounts & auto-config · OAuth                    │
│ Provider adapters: Microsoft Graph, Gmail API,    │
│   IMAP/SMTP (+IDLE, CONDSTORE/QRESYNC), JMAP,     │
│   CalDAV/CardDAV                                  │
│ Sync engine (local-first, durable action queue)   │
│ Store: SQLite (metadata) + message bodies on disk │
│ Search: Tantivy full-text index                   │
│ MCP server (stdio + local HTTP; remote via relay) │
│ Rules engine · S/MIME · PGP · HTML sanitizer      │
│ Feature switches & managed policy                 │
└──────────────────────────────────────────────────┘
        ▲ desktop: background helper (login item / service)
        │ keeps live connections, runs MCP, raises notifications
┌───────┴─────── wbmc-relay (Rust, owner-hosted, self-hostable) ┐
│ Push "doorbell" for iOS/Android · encrypted MCP tunnel         │
│ Opt-in credentialed watcher (clearly labeled)                  │
└────────────────────────────────────────────────────────────────┘
```

- **Local-first.** Every user action is written to the local store and an action queue first, then synced. This is the main protection against lost drafts and mail.
- **Provider APIs over IMAP where available.** Microsoft 365 and Outlook.com use **Microsoft Graph**. EWS was retired for Exchange Online on 2026-10-01, so Graph is the only option. Gmail uses the **Gmail API**, which gives native labels, history-based sync and push. IMAP is the fallback for Gmail and the main protocol for everyone else.
- **Credentials** live in the OS keychain or credential store. Local data can be encrypted at rest.
- **HTML mail** is rendered in a sandboxed web view. Scripts never run, remote content is blocked or proxied, and tracking pixels are stripped.
- Library choices inside the core (IMAP crate, HTML sanitizer and so on) are the builder's call.
- **Offline storage [Claude's call].**
  - By default, all message bodies are stored locally. Attachments are downloaded on demand, and small ones (≤ 1 MB) are fetched ahead of time.
  - A per-account "keep mail offline for: all / 1 year / 3 months" setting exists, and admin policy can set it.
  - Search is local first. For mail outside the offline window it falls back to server search (Graph `$search`, Gmail `q`, IMAP SEARCH), with server results clearly marked.
- **Encryption at rest [Claude's call].**
  - v1 relies on OS full-disk encryption (FileVault, BitLocker) and keeps credentials in the keychain.
  - Admin policy can require that disk encryption is on.
  - The storage layer is built behind an interface so that app-level encryption (SQLCipher plus encrypted body and index files) can be added later without a rewrite.
- **Minimum OS versions [Claude's call]:** macOS 26+, iOS/iPadOS 26+, Windows 11 22H2+ (x64 and ARM64).

## 5. v1 feature checklist ("99% of daily Outlook use")

The owner approved this list.

**Mail**
- Unified inbox plus per-account inboxes; conversation threading (can be turned off).
- Folders; Gmail labels; flags; Outlook **categories** (color tags synced with Microsoft 365).
- Focused / Other inbox, using Microsoft's flag where available.
- Search: instant, across all accounts, with filters (from:, has:attachment, date and so on). Searches can be saved as smart folders.
- Rules, run both on the device and on the server where supported (Graph inbox rules, Sieve).
- Signatures per identity; aliases; **shared mailboxes**; send as / send on behalf.
- Out-of-office replies.
- Attachments with preview; drag and drop in and out; an attachment browser.
- Read receipts; print; **undo send**; **send later**; snooze; reminders ("nudge me if no reply").
- Open .eml and .msg files; import from mbox and EML.
- Unsubscribe and bulk-sender tools.
- Keyboard shortcuts, including an optional Outlook-compatible set; a command palette (⌘K / Ctrl+K).
- Offline: read, search, compose and act with no connection; changes sync on reconnect.

**Calendar**
- Day, week, work-week and month views; several calendars overlaid.
- Invites both ways: send, accept, tentative, decline, propose a new time. This must work with Outlook and Gmail invitations (iTIP/iMIP and Graph).
- **Scheduling assistant** showing attendees' free/busy; **room and resource booking**.
- Teams and Zoom/Meet links: a join button, plus creating Teams meetings for Microsoft 365 accounts.
- Recurring events with exceptions; reminders and notifications; time zones.
- **Shared and delegated calendars** ("open my boss's calendar").

**Contacts**
- Personal contacts, synced both ways (Graph, Google People, CardDAV); merged duplicates.
- Company directory (Microsoft's Global Address List); contact groups and distribution lists.
- Address autocomplete that learns from sent and received mail.

**Privacy and security**
- Remote images blocked or proxied by default; tracking pixels stripped.
- Sender checks: SPF/DKIM/DMARC results, lookalike-domain warnings, a "first time sender" badge.
- S/MIME (important for Microsoft 365 workplaces) and PGP.
- Accessibility (screen readers, dynamic type, high contrast) and localization, including right-to-left languages.

**Tasks (owner-approved, basic).** "Flag for follow-up" plus a simple task list (title, due date, reminder, done), synced with Microsoft To Do (Graph) and Google Tasks. It can be hidden with a feature switch.

## 6. Look and feel

- **Default layout: refined three-pane** (owner-approved). Sidebar with Mail, Calendar and Contacts at the top and each account's folders below, then the message list, then the reading pane. Outlook users should know where everything is. Other layouts (reading pane at the bottom, no reading pane, a compact list) are available in settings.
- **Native look on each platform** (owner-approved). Apple's design and materials on macOS, iOS and iPadOS, using the system accent color. Fluent design with Mica and Segoe UI on Windows 11. libadwaita on Linux **[Claude's call: GNOME-first; KDE gets a GTK app]**. Light and dark follow the system.
- **Personality is in the details [Claude's call]:** generous spacing, clean reading typography, good avatars and sender logos, smooth motion, an uncluttered toolbar.
- **v1 customization [Claude's call]:** accent color, density (comfortable or compact), font size, list layout, reading-pane position.
- **Feature switches** (owner requirement). Every non-core area can be hidden with one toggle in a single "Features" settings page: Calendar, Contacts, MCP/agents, Focused inbox, categories, snooze, the attachment browser, the unified inbox and so on. A hidden feature leaves no empty space in the interface. An admin policy can force any switch on or off.

Visual mockups from the brainstorm are in `docs/design/mockups/`: `layout.html` (option A was chosen) and `visual-style.html` (option 1 was chosen).

## 7. Setup, deployment and push

**Setup.** The user types an email address and WBMC finds the provider:
- Built-in profiles for the Tier 1 providers.
- Thunderbird's ISP database (autoconfig).
- Microsoft autodiscover.
- DNS SRV records and MX-based guessing.

Then one browser sign-in (OAuth), or an app password where the provider requires one (for example iCloud). Mail, calendar and contacts are all set up from that one step.

**Desktop deployment.**
- A config file or Windows MSI properties (and a macOS configuration profile) can set the company domain, default settings, feature switches and MCP policy.
- With that in place, the user sees only a sign-in button.
- Policies set this way are **locked**: users can't override them.

**Mobile MDM.**
- Managed App Configuration (Apple's managed config, Android managed configurations) with the same keys as desktop.
- **Microsoft Intune App Protection (MAM SDK)**, because many Microsoft 365 tenants block unmanaged apps through Conditional Access.

**Push (owner agreed to run and pay for the relay).**
- **Desktop:** a background helper keeps live connections (Graph/Gmail push or polling, IMAP IDLE). No server needed.
- **iOS/iPadOS: the "doorbell" relay.**
  1. The device sets up provider change notifications itself, using its own token: Gmail `users.watch` to a Pub/Sub topic in WBMC's Google Cloud project, and Graph subscriptions with the relay as the notification URL.
  2. The relay receives only "mailbox X changed." It maps that to the device's APNs token and sends a content-free push (mutable-content).
  3. A Notification Service Extension on the device fetches the message directly from the provider and builds the notification locally.
  4. The device renews subscriptions whenever it wakes.
  5. The relay stores only a subscription/mailbox ID and a device token: no credentials, no content.
- **Providers without webhooks** (Yahoo, AOL, iCloud, generic IMAP) have two options:
  - iOS background refresh (default; delayed);
  - an **opt-in credentialed watcher** that holds the login, clearly labeled. Self-hostable for companies.
- **Self-hosting.** The relay is open source and deployable with Docker. A self-hosted relay still sends pushes through the owner's APNs/FCM gateway, because push keys are tied to the published app.
- **Android:** the same design using FCM.

## 8. MCP: agent access

**Coverage.** Agents can do anything the user can do in the app. Tools are grouped into permission areas:

| Area | Examples |
|---|---|
| Read | search, read message/thread, list accounts/folders, get attachment |
| Organize | move, archive, flag, label/category, mark read, snooze |
| Draft | create/edit draft, reply/forward as draft |
| Send | send, schedule send, send a draft |
| Delete | trash, permanently delete, empty folder |
| Calendar | read, create/update/delete events, respond to invites, find free time |
| Contacts | read, create, update, delete |
| Rules & settings | filters, out-of-office, signatures |

**Permissions.**
- Each area, and each individual tool, can be set to **Allow**, **Ask me first** or **Off**.
- Permissions can be scoped **per account** and **per agent**.
- **Default for new installs [Claude's call]:**
  - Allow: Read, Organize, Draft, Calendar read, Contacts read.
  - Ask me first: Send, Delete, calendar writes, contact writes, rules and settings.
  - **Remote access is off.**

**"Ask me first"** shows a native prompt with the exact action (for example the full email) and **Approve / Edit / Deny** buttons. On mobile, approvals arrive as actionable notifications.

**Activity log.** Every agent action is recorded with the agent's identity, timestamp, arguments and outcome. Undo is offered where possible (move, archive, delete-to-trash, unsent scheduled sends).

**Prompt-injection safety.**
- Email content is always returned as clearly marked untrusted data, never as instructions.
- Some actions require approval even when the area is set to Allow **[Claude's call]**: sending to an address the user has never written to, sending mail with attachments to an outside domain, and bulk deletes or moves above a threshold. These guard rails can be relaxed per agent by the user, unless an admin policy locks them.

**Transports.**
- **Local (default on):** stdio through a `wbmc-mcp` command, plus a localhost streamable-HTTP endpoint. Agents are paired once in settings.
- **Remote (opt-in):** streamable HTTP with MCP-standard OAuth, a separate grant per agent, revocable anytime. The connection is tunneled through the relay end-to-end encrypted, so users don't need to open ports. Direct exposure is an advanced option.
- The MCP server runs **on desktop only**, inside the background helper. Remote agents can reach mail only while one of the user's desktops is on. Mobile manages permissions and approvals but doesn't host the server.

**Admin policy** can force any area or tool Off or Ask, and can disable remote access entirely.

## 9. Performance and reliability targets [Claude's call, owner reviewed]

| Target | Value |
|---|---|
| Cold start to a usable inbox | < 1 s |
| Search results while typing | < 100 ms at 1M+ messages |
| Switching between messages | < 50 ms (bodies cached locally) |
| Sync of a change made elsewhere | seconds (push/IDLE); worst case one poll interval |
| Initial sync | newest mail visible within seconds; backfill in the background |
| Data loss | zero: local-first writes, a durable action queue, idempotent replay |
| Crash-free sessions | > 99.9% |

**Testing [Claude's call].**
- Unit tests in the core.
- Protocol tests against local servers (e.g. Dovecot, Stalwart, Radicale) in containers.
- An **integration suite against real Microsoft 365, Gmail and IMAP test accounts**.
- Performance benchmarks on a synthetic 1M-message mailbox, run in CI.
- UI smoke tests on each platform.

## 10. Providers

| Tier | Providers | How |
|---|---|---|
| 1 (v1 required) | Microsoft 365, Outlook.com, Gmail/Workspace, Yahoo, AOL, iCloud, generic IMAP/SMTP | Graph · Gmail API · IMAP (+ CalDAV/CardDAV for iCloud/Yahoo) |
| 2 (v1 nice-to-have) | Fastmail (JMAP), Proton (via Proton Bridge), Zoho, GMX/Web.de/mail.com, mailbox.org, Posteo | JMAP / IMAP + auto-config |
| 3 (later) | Yandex, Mail.ru, QQ/163, self-hosted Exchange (EWS) | as demand appears |
| Not possible | Tuta, HEY | no third-party access |

Other providers are reached through generic auto-configuration.

**External dependencies the owner must sort out:**
- **Google:** restricted Gmail scopes need verification and a yearly **CASA** security assessment before public release. Unverified apps are capped at 100 test users.
- **Microsoft:** a multi-tenant Entra app registration for Graph. Many tenants require admin consent, so IT-facing docs must explain it.
- **Apple:** a Developer Program membership (signing, notarization, APNs, App Store).
- **Windows:** code signing (e.g. Azure Trusted Signing). Microsoft Store distribution is optional.

## 11. License

**MPL-2.0** (Claude's recommendation). It's file-level copyleft: changes to WBMC's own files stay open. It's compatible with the App Store, unlike GPL, and companies can still embed the core. Every component (core, apps, relay) uses MPL-2.0.

## 12. How it gets built

- **AI agents write 100% of the code.** Claude orchestrates and is the reviewer of record. Build work goes to Claude subagents and to the Grok Build, Cursor and Codex CLIs.
- **Check usage limits** (`openusage-budget`) before each batch of dispatches. Pace work so no subscription is drained in a day; the owner also uses these subscriptions for their day job.
- **Progress the owner can see [Claude's call].**
  - Plans are cut into thin end-to-end slices, each ending in a macOS build the owner can run. For example: (1) sign in and read Microsoft 365 mail, (2) instant search, (3) compose and send, and so on.
  - The 1M-message benchmark runs locally on the Mac until CI runners exist.
- **Plans must be split into independent, well-specified, separately reviewable tasks** with clear acceptance tests, so they can be handed to different builders.
- The repository is a single monorepo **[Claude's call]**: `core/`, `apps/macos`, `apps/windows`, `apps/ios`, `apps/linux`, `apps/android`, `relay/`, `docs/`.

## 13. Left to the builder

- Crate and library choices; the database schema; the search index layout; the binding generator for C#.
- Internal module boundaries in the core; MCP tool names and argument schemas, as long as they follow §8.
- Relay hosting provider and infrastructure. The owner pays; keep it cheap.
- CI setup (GitHub Actions is assumed).
