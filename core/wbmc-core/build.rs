// This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0. If a copy of the MPL was not distributed with this file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Build-time plumbing for the Google OAuth client secret.
//!
//! The secret is read from `config/oauth-clients.local.toml` (`[google]
//! client_secret`, git-ignored) if that file exists, otherwise from the
//! `WBMC_GOOGLE_CLIENT_SECRET` environment variable. When a value is found it is
//! exported to rustc so the crate can read it with `option_env!`.

use std::path::Path;

const VAR: &str = "WBMC_GOOGLE_CLIENT_SECRET";

fn main() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set");
    let local = Path::new(&manifest_dir).join("../../config/oauth-clients.local.toml");

    println!("cargo:rerun-if-changed={}", local.display());
    println!("cargo:rerun-if-env-changed={VAR}");

    let from_file = std::fs::read_to_string(&local)
        .ok()
        .and_then(|text| google_client_secret(&text));
    let value = from_file.or_else(|| std::env::var(VAR).ok().filter(|v| !v.trim().is_empty()));

    if let Some(secret) = value {
        println!("cargo:rustc-env={VAR}={secret}");
    }
}

/// Minimal extraction of `client_secret` from the `[google]` table.
fn google_client_secret(text: &str) -> Option<String> {
    let mut in_google = false;
    for raw in text.lines() {
        let line = raw.trim();
        if line.starts_with('[') {
            in_google = line.split('#').next().unwrap_or("").trim() == "[google]";
            continue;
        }
        if !in_google {
            continue;
        }
        let Some((key, rest)) = line.split_once('=') else {
            continue;
        };
        if key.trim() != "client_secret" {
            continue;
        }
        let rest = rest.trim();
        let quote = rest.chars().next().filter(|c| *c == '"' || *c == '\'')?;
        let inner = &rest[1..];
        let end = inner.find(quote)?;
        let secret = inner[..end].trim();
        return (!secret.is_empty()).then(|| secret.to_string());
    }
    None
}
