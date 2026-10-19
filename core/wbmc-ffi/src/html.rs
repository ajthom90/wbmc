// This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0. If a copy of the MPL was not distributed with this file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Rewrites `cid:` image references as `data:` URIs so the web view needs no custom scheme.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use wbmc_core::mime::cid_part;

/// Images larger than this stay as `cid:` references (and so do not render).
pub(crate) const MAX_INLINE_IMAGE_BYTES: usize = 10 * 1024 * 1024;

/// Replaces `src="cid:X"` and `src='cid:X'` (the `cid:` scheme matched case-insensitively)
/// with a `data:` URI when `X` names an `image/*` part of `raw` no larger than
/// [`MAX_INLINE_IMAGE_BYTES`]. Other references are left unchanged.
pub(crate) fn inline_cid_images(html: &str, raw: &[u8]) -> String {
    // ASCII lowercasing keeps byte offsets identical to `html`.
    let lower = html.to_ascii_lowercase();
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
        let Some(uri) = data_uri(raw, &html[value_start..value_end]) else {
            continue;
        };
        out.push_str(&html[copied..start]);
        out.push_str(&uri);
        copied = value_end;
        search = value_end;
    }
    out.push_str(&html[copied..]);
    out
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

fn data_uri(raw: &[u8], content_id: &str) -> Option<String> {
    let content_id = percent_decode(content_id.trim());
    let (mime, bytes) = cid_part(raw, &content_id)?;
    if !mime.starts_with("image/") || bytes.len() > MAX_INLINE_IMAGE_BYTES {
        return None;
    }
    Some(format!("data:{mime};base64,{}", STANDARD.encode(bytes)))
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
