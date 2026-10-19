// This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0. If a copy of the MPL was not distributed with this file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! OAuth sign-in and token lifecycle.

pub mod loopback;
pub mod microsoft;
pub mod pkce;
mod tokens;

pub use microsoft::Endpoints;
pub use tokens::TokenManager;

use crate::{CoreError, Result};

/// Tokens returned by an OAuth token endpoint.
///
/// `Debug` is implemented by hand so tokens never reach logs or error output.
#[derive(Clone, PartialEq, Eq)]
pub struct TokenSet {
    pub access_token: String,
    pub refresh_token: Option<String>,
    /// Unix epoch milliseconds at which the access token expires.
    pub expires_at_ms: i64,
}

impl std::fmt::Debug for TokenSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenSet")
            .field("access_token", &"<redacted>")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "<redacted>"),
            )
            .field("expires_at_ms", &self.expires_at_ms)
            .finish()
    }
}

/// Current time as Unix epoch milliseconds.
pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Fills `buf` from the operating system's CSPRNG.
pub(crate) fn os_random(buf: &mut [u8]) -> Result<()> {
    use rand::TryRngCore;
    rand::rngs::OsRng
        .try_fill_bytes(buf)
        .map_err(|e| CoreError::Protocol(format!("OS random generator failed: {e}")))
}

/// `len` random bytes from the OS CSPRNG, base64url-encoded without padding.
pub(crate) fn random_base64url(len: usize) -> Result<String> {
    use base64::Engine;
    let mut bytes = vec![0u8; len];
    os_random(&mut bytes)?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_set_debug_redacts_tokens() {
        let tokens = TokenSet {
            access_token: "eyJ-access-secret".into(),
            refresh_token: Some("M.R3-refresh-secret".into()),
            expires_at_ms: 1_700_000_000_000,
        };
        let debug = format!("{tokens:?}");
        assert!(!debug.contains("access-secret"), "{debug}");
        assert!(!debug.contains("refresh-secret"), "{debug}");
        assert!(debug.contains("1700000000000"), "{debug}");
        assert!(debug.contains("<redacted>"), "{debug}");
    }

    #[test]
    fn random_state_is_32_bytes_base64url() {
        let a = random_base64url(32).unwrap();
        let b = random_base64url(32).unwrap();
        assert_eq!(a.len(), 43);
        assert!(!a.contains('=') && !a.contains('+') && !a.contains('/'));
        assert_ne!(a, b);
    }
}
