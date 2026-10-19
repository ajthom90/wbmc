// This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0. If a copy of the MPL was not distributed with this file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! PKCE (RFC 7636) with the S256 challenge method.

use crate::Result;

/// Length of the generated code verifier (RFC 7636 allows 43..=128).
pub const VERIFIER_LEN: usize = 64;

/// RFC 3986 unreserved characters, the verifier alphabet (RFC 7636 §4.1).
const UNRESERVED: &[u8; 66] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";

/// A PKCE verifier and its S256 challenge.
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

impl std::fmt::Debug for Pkce {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The verifier is a one-time secret; only the public challenge is shown.
        f.debug_struct("Pkce")
            .field("verifier", &"<redacted>")
            .field("challenge", &self.challenge)
            .finish()
    }
}

impl Pkce {
    /// Generates a new random verifier from the OS CSPRNG.
    pub fn new() -> Result<Self> {
        // Rejection sampling keeps the 66-character distribution uniform:
        // bytes >= 198 (3 * 66) are discarded.
        let mut verifier = String::with_capacity(VERIFIER_LEN);
        let mut buf = [0u8; 96];
        while verifier.len() < VERIFIER_LEN {
            super::os_random(&mut buf)?;
            for &byte in &buf {
                if verifier.len() == VERIFIER_LEN {
                    break;
                }
                if (byte as usize) < 3 * UNRESERVED.len() {
                    verifier.push(UNRESERVED[byte as usize % UNRESERVED.len()] as char);
                }
            }
        }
        Ok(Self::from_verifier(&verifier))
    }

    /// Builds the pair for a given verifier.
    pub fn from_verifier(verifier: &str) -> Self {
        use base64::Engine;
        use sha2::Digest;
        let digest = sha2::Sha256::digest(verifier.as_bytes());
        Self {
            verifier: verifier.to_string(),
            challenge: base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_matches_rfc7636_vector() {
        let pkce = Pkce::from_verifier("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk");
        assert_eq!(
            pkce.challenge,
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn pkce_new_generates_unreserved_verifier() {
        let a = Pkce::new().unwrap();
        let b = Pkce::new().unwrap();
        assert_eq!(a.verifier.len(), VERIFIER_LEN);
        assert!(
            a.verifier
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-._~".contains(&c))
        );
        assert_ne!(a.verifier, b.verifier);
        assert_eq!(a.challenge, Pkce::from_verifier(&a.verifier).challenge);
    }

    #[test]
    fn pkce_debug_hides_verifier() {
        let pkce = Pkce::from_verifier("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk");
        assert!(!format!("{pkce:?}").contains("dBjftJeZ"));
    }
}
