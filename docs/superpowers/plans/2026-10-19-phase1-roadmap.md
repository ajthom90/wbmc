# Phase 1 Roadmap: Rust core + macOS app

**Spec:** `docs/superpowers/specs/2026-10-19-wbmc-design.md` (§3 "Phase 1 scope")

Phase 1 is too large for one plan. It is split into milestones. Each milestone:
- is a thin end-to-end slice;
- ends with a macOS build the owner can run;
- gets its own detailed plan, written when the previous milestone lands. Later plans build on the real code, not on guesses.

| # | Milestone | Owner can see | Plan |
|---|---|---|---|
| M1 | **Foundation + read mail** (IMAP + Microsoft 365) | Sign in to M365 or the local test server; browse folders and read mail in the three-pane app | `2026-10-19-m1-foundation-read-mail.md` |
| M2 | **Gmail + auto-setup + more providers** | Add Gmail by typing the address; Yahoo/AOL/iCloud via auto-config + app passwords; unified inbox | TBD |
| M3 | **Instant search** | Results as you type across all accounts; full offline bodies; server fallback; 1M-message benchmark | TBD |
| M4 | **Act on mail** | Compose, reply and forward; send (SMTP, Graph, Gmail); drafts sync; move, archive, delete, flag, categories; offline action queue; undo send; send later; snooze | TBD |
| M5 | **Always up to date** | Background login-item helper; IMAP IDLE/CONDSTORE; Graph and Gmail change tracking; notifications | TBD |
| M6 | **Calendar, contacts, tasks** | Calendar views, invites both ways, Teams links; contacts + autocomplete; basic tasks (To Do, Google Tasks, CalDAV/CardDAV) | TBD |
| M7 | **MCP** | `wbmc-mcp` stdio + localhost; permission areas; Ask-me-first prompts; activity log + undo; injection guards | TBD |
| M8 | **Polish + ship to owner** | Feature switches page; customization; keyboard shortcuts + ⌘K; sender checks; signed + notarized Developer ID build; perf targets verified | TBD |

**Order rationale:**
- M1–M4 make WBMC a usable mail app.
- M5 makes it trustworthy as a daily driver.
- M6 completes the "Outlook replacement" surface.
- M7 adds the differentiator on top of a stable core, so agent tools wrap tested operations.
- M8 hardens everything.

M6 and M7 are independent once M4 lands, so they can run in parallel across builders.
