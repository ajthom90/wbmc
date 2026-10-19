// This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0. If a copy of the MPL was not distributed with this file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Rewrites `cid:` image references as `data:` URIs so the web view needs no custom scheme.

use std::collections::HashMap;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use mail_parser::{MessageParser, MimeHeaders};

/// Total decoded image bytes inlined into one message's HTML, counted per reference.
/// Once the budget is spent, remaining references stay `cid:` (and so do not render).
pub(crate) const MAX_INLINE_TOTAL_BYTES: usize = 10 * 1024 * 1024;

/// Replaces `src="cid:X"` and `src='cid:X'` (the `cid:` scheme matched case-insensitively)
/// with a `data:` URI when `X` names an `image/*` part of `raw`, within a total budget of
/// [`MAX_INLINE_TOTAL_BYTES`] per message. The message is parsed once and each image is
/// encoded at most once. Other references are left unchanged.
pub(crate) fn inline_cid_images(html: &str, raw: &[u8]) -> String {
    // ASCII lowercasing keeps byte offsets identical to `html`.
    let lower = html.to_ascii_lowercase();
    if !lower.contains("cid:") {
        return html.to_string();
    }
    let mut images = ImageParts::new(raw);
    let mut budget = MAX_INLINE_TOTAL_BYTES;
    let mut out = String::with_capacity(html.len());
    let mut copied = 0;
    let mut search = 0;
    while let Some(found) = lower[search..].find("cid:") {
        let start = search + found;
        search = start + 4;
        let Some(quote) = quote_after_src(&lower, start) else {
            continue;
        };
        let value_start = start + 4;
        let Some(len) = html[value_start..].find(quote) else {
            continue;
        };
        let value_end = value_start + len;
        let Some((size, uri)) = images.data_uri(&html[value_start..value_end]) else {
            continue;
        };
        if size > budget {
            continue;
        }
        budget -= size;
        out.push_str(&html[copied..start]);
        out.push_str(uri);
        copied = value_end;
        search = value_end;
    }
    out.push_str(&html[copied..]);
    out
}

/// `image/` followed by a non-empty `[a-z0-9.+-]` subtype; anything else is never inlined.
pub(crate) fn is_safe_image_mime(mime: &str) -> bool {
    mime.strip_prefix("image/").is_some_and(|subtype| {
        !subtype.is_empty()
            && subtype.bytes().all(|b| {
                b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'+' | b'-')
            })
    })
}

/// The message's image parts by Content-ID, with each data URI built on first use.
struct ImageParts {
    parts: HashMap<String, (String, Vec<u8>)>,
    encoded: HashMap<String, String>,
}

impl ImageParts {
    fn new(raw: &[u8]) -> Self {
        let mut parts = HashMap::new();
        if let Some(message) = MessageParser::default().parse(raw) {
            for part in &message.parts {
                let Some(cid) = part.content_id() else {
                    continue;
                };
                let Some(mime) = part.content_type().map(|c| match c.subtype() {
                    Some(sub) => format!("{}/{}", c.ctype(), sub).to_ascii_lowercase(),
                    None => c.ctype().to_ascii_lowercase(),
                }) else {
                    continue;
                };
                if !is_safe_image_mime(&mime) {
                    continue;
                }
                parts
                    .entry(normalise_cid(cid))
                    .or_insert_with(|| (mime, part.contents().to_vec()));
            }
        }
        Self {
            parts,
            encoded: HashMap::new(),
        }
    }

    /// The decoded size and data URI for a `cid:` value, encoding it at most once.
    fn data_uri(&mut self, value: &str) -> Option<(usize, &str)> {
        let cid = normalise_cid(&percent_decode(value.trim()));
        let (mime, bytes) = self.parts.get(&cid)?;
        let size = bytes.len();
        if size > MAX_INLINE_TOTAL_BYTES {
            return None;
        }
        let uri = self
            .encoded
            .entry(cid)
            .or_insert_with(|| format!("data:{mime};base64,{}", STANDARD.encode(bytes)));
        Some((size, uri.as_str()))
    }
}

fn normalise_cid(cid: &str) -> String {
    cid.trim()
        .trim_matches(|c| c == '<' || c == '>')
        .to_string()
}

/// Returns the quote character when `cid:` at `start` begins a quoted `src` attribute value.
fn quote_after_src(lower: &str, start: usize) -> Option<char> {
    let before = &lower.as_bytes()[..start];
    let (&quote, before) = before.split_last()?;
    if quote != b'"' && quote != b'\'' {
        return None;
    }
    let before = trim_ascii_end(before);
    let (&eq, before) = before.split_last()?;
    if eq != b'=' {
        return None;
    }
    let before = trim_ascii_end(before);
    if !before.ends_with(b"src") {
        return None;
    }
    // `src` must be a whole attribute name (not e.g. `data-src`).
    match before.len().checked_sub(4).map(|i| before[i]) {
        None => Some(char::from(quote)),
        Some(c) if c.is_ascii_whitespace() => Some(char::from(quote)),
        Some(_) => None,
    }
}

fn trim_ascii_end(mut bytes: &[u8]) -> &[u8] {
    while let Some((last, rest)) = bytes.split_last() {
        if !last.is_ascii_whitespace() {
            break;
        }
        bytes = rest;
    }
    bytes
}

/// `cid:` URLs may percent-encode the Content-ID (RFC 2392).
fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(byte) = bytes
                .get(i + 1..i + 3)
                .and_then(|hex| std::str::from_utf8(hex).ok())
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
