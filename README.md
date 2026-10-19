# WBMC

WBMC is a free, open-source mail, calendar and contacts app for everyone who hates Outlook, especially office workers whose company runs Microsoft 365. It sets itself up from just an email address and one sign-in, is fast, looks like it belongs on a modern computer, and does everything people use Outlook for every day. It also includes a built-in MCP server that lets AI agents read and manage your mailbox, calendar and contacts, with per-tool permissions you control.

The core is written in Rust, with native UIs on top (macOS first).

## Build

Requirements: a Rust toolchain via [rustup](https://rustup.rs). `rust-toolchain.toml` selects the stable channel and the required components automatically.

```sh
cargo build --workspace
cargo test --workspace
cargo fmt --check
cargo clippy --all-targets -- -D warnings
```

Integration tests against a local IMAP server use [GreenMail](https://greenmail-mail-test.github.io/greenmail/). CI runs `greenmail/standalone` as a service container; set `WBMC_TEST_IMAP=1` to enable those tests, and `WBMC_SECRETS=memory` to use an in-memory secret store instead of the OS keychain.

## License

[Mozilla Public License 2.0](LICENSE).
