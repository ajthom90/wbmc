// This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0. If a copy of the MPL was not distributed with this file, You can obtain one at https://mozilla.org/MPL/2.0/.

use crate::model::Address;
use mail_parser::{MessageParser, MimeHeaders, PartType};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentMeta {
    pub filename: String,
    pub mime_type: String,
    pub size: u64,
    pub content_id: Option<String>,
    pub inline: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedMessage {
    pub subject: String,
    pub from: Address,
    pub to: Vec<Address>,
    pub cc: Vec<Address>,
    pub date_ms: i64,
    pub html: Option<String>,
    pub text: Option<String>,
    pub snippet: String,
    pub attachments: Vec<AttachmentMeta>,
}

const SNIPPET_MAX_CHARS: usize = 200;

fn unknown_sender() -> Address {
    Address {
        name: Some("Unknown sender".into()),
        email: String::new(),
    }
}

fn convert_addrs(addr: Option<&mail_parser::Address<'_>>) -> Vec<Address> {
    addr.map(|a| {
        a.iter()
            .map(|x| Address {
                name: x.name().map(str::to_string).filter(|n| !n.is_empty()),
                email: x.address().unwrap_or_default().to_string(),
            })
            .collect()
    })
    .unwrap_or_default()
}

fn fallback_message(fallback_date_ms: i64) -> ParsedMessage {
    ParsedMessage {
        subject: "(no subject)".into(),
        from: unknown_sender(),
        to: vec![],
        cc: vec![],
        date_ms: fallback_date_ms,
        html: None,
        text: None,
        snippet: String::new(),
        attachments: vec![],
    }
}

/// Parses a raw RFC 5322 message. Infallible: anything unparseable yields
/// fallback values.
pub fn parse_message(raw: &[u8], fallback_date_ms: i64) -> ParsedMessage {
    let Some(msg) = MessageParser::default().parse(raw) else {
        return fallback_message(fallback_date_ms);
    };

    let subject = msg
        .subject()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("(no subject)")
        .to_string();
    let from = convert_addrs(msg.from())
        .into_iter()
        .next()
        .unwrap_or_else(unknown_sender);
    let date_ms = msg
        .date()
        .map(|d| d.to_timestamp().saturating_mul(1000))
        .unwrap_or(fallback_date_ms);

    // mail-parser synthesizes text from HTML (and vice versa); only keep
    // bodies that genuinely exist as that part type.
    let join = |ids: &[u32], want_html: bool| -> Option<String> {
        let parts: Vec<&str> = ids
            .iter()
            .filter_map(|&i| match &msg.part(i as _)?.body {
                PartType::Text(s) if !want_html => Some(s.as_ref()),
                PartType::Html(s) if want_html => Some(s.as_ref()),
                _ => None,
            })
            .collect();
        if parts.is_empty() {
            None
        } else {
            Some(parts.join("\n"))
        }
    };
    let text = join(&msg.text_body, false);
    let html = join(&msg.html_body, true);

    let snippet = match (&text, &html) {
        (Some(t), _) if !t.trim().is_empty() => make_snippet(t),
        (_, Some(h)) => make_snippet(&strip_html(h)),
        _ => String::new(),
    };

    let attachments = msg
        .attachments()
        .map(|p| {
            let content_id = p
                .content_id()
                .map(|c| c.trim_matches(|ch| ch == '<' || ch == '>').to_string());
            let inline = content_id.is_some()
                || p.content_disposition()
                    .is_some_and(|d| d.ctype().eq_ignore_ascii_case("inline"));
            AttachmentMeta {
                filename: p.attachment_name().unwrap_or("attachment").to_string(),
                mime_type: p
                    .content_type()
                    .map(|c| match c.subtype() {
                        Some(sub) => format!("{}/{}", c.ctype(), sub),
                        None => c.ctype().to_string(),
                    })
                    .unwrap_or_else(|| "application/octet-stream".into())
                    .to_ascii_lowercase(),
                size: p.contents().len() as u64,
                content_id,
                inline,
            }
        })
        .collect();

    ParsedMessage {
        subject,
        from,
        to: convert_addrs(msg.to()),
        cc: convert_addrs(msg.cc()),
        date_ms,
        html,
        text,
        snippet,
        attachments,
    }
}

/// Finds the part whose Content-ID matches `content_id` (angle brackets
/// optional) and returns its MIME type and decoded bytes.
pub fn cid_part(raw: &[u8], content_id: &str) -> Option<(String, Vec<u8>)> {
    let want = content_id.trim().trim_matches(|c| c == '<' || c == '>');
    let msg = MessageParser::default().parse(raw)?;
    msg.parts.iter().find_map(|p| {
        let cid = p.content_id()?.trim_matches(|c| c == '<' || c == '>');
        if cid != want {
            return None;
        }
        let mime = p
            .content_type()
            .map(|c| match c.subtype() {
                Some(sub) => format!("{}/{}", c.ctype(), sub),
                None => c.ctype().to_string(),
            })
            .unwrap_or_else(|| "application/octet-stream".into())
            .to_ascii_lowercase();
        Some((mime, p.contents().to_vec()))
    })
}

fn make_snippet(s: &str) -> String {
    let mut out = String::new();
    let mut count = 0;
    for word in s.split_whitespace() {
        if !out.is_empty() {
            if count + 1 >= SNIPPET_MAX_CHARS {
                break;
            }
            out.push(' ');
            count += 1;
        }
        for ch in word.chars() {
            if count >= SNIPPET_MAX_CHARS {
                return out;
            }
            out.push(ch);
            count += 1;
        }
        if count >= SNIPPET_MAX_CHARS {
            break;
        }
    }
    out
}

/// Minimal tag stripper: drops tags, `<style>`/`<script>` contents and
/// comments, then decodes a few basic entities.
fn strip_html(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(lt) = rest.find('<') {
        out.push_str(&rest[..lt]);
        rest = &rest[lt..];
        if rest.starts_with("<!--") {
            rest = rest.find("-->").map_or("", |i| &rest[i + 3..]);
            continue;
        }
        let Some(gt) = rest.find('>') else {
            rest = "";
            break;
        };
        let tag = &rest[1..gt];
        let name: String = tag
            .trim_start()
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_ascii_lowercase();
        rest = &rest[gt + 1..];
        if (name == "style" || name == "script") && !tag.trim_start().starts_with('/') {
            let close = format!("</{name}");
            let lower = rest.to_ascii_lowercase();
            rest = match lower.find(&close) {
                Some(i) => {
                    let after = &rest[i..];
                    after.find('>').map_or("", |g| &after[g + 1..])
                }
                None => "",
            };
        }
        out.push(' ');
    }
    out.push_str(rest);
    out.replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    const FALLBACK: i64 = 1_700_000_000_000;
    const PLAIN: &[u8] = include_bytes!("../../../dev/mail/corpus/plain.eml");
    const INLINE: &[u8] = include_bytes!("../../../dev/mail/corpus/html_with_inline_image.eml");
    const RFC2047: &[u8] = include_bytes!("../../../dev/mail/corpus/rfc2047_subject.eml");
    const LATIN1: &[u8] = include_bytes!("../../../dev/mail/corpus/latin1_body.eml");
    const SJIS: &[u8] = include_bytes!("../../../dev/mail/corpus/shift_jis.eml");
    const NOHDR: &[u8] = include_bytes!("../../../dev/mail/corpus/no_date_no_from.eml");
    const TRUNC: &[u8] = include_bytes!("../../../dev/mail/corpus/truncated_multipart.eml");
    const PDF: &[u8] = include_bytes!("../../../dev/mail/corpus/attachment_pdf.eml");
    const REMOTE: &[u8] = include_bytes!("../../../dev/mail/corpus/remote_images_and_script.eml");

    #[test]
    fn plain_message() {
        let m = parse_message(PLAIN, FALLBACK);
        assert_eq!(m.subject, "Plain hello");
        assert_eq!(m.from.email, "alice@example.com");
        assert_eq!(m.from.name.as_deref(), Some("Alice"));
        assert_eq!(m.to[0].email, "bob@example.com");
        assert_eq!(m.date_ms, 1_792_404_000_000);
        assert!(m.text.unwrap().contains("plain message"));
        assert_eq!(m.snippet, "Hello Bob, This is a plain message.");
    }

    #[test]
    fn rfc2047_subject_decodes() {
        assert_eq!(parse_message(RFC2047, FALLBACK).subject, "Café ☕ meeting");
    }

    #[test]
    fn latin1_body_decodes() {
        let m = parse_message(LATIN1, FALLBACK);
        assert!(m.text.unwrap().contains("Grüße"));
    }

    #[test]
    fn shift_jis_body_decodes() {
        let m = parse_message(SJIS, FALLBACK);
        assert!(m.text.unwrap().contains("こんにちは世界"));
    }

    #[test]
    fn missing_headers_use_fallbacks() {
        let m = parse_message(NOHDR, FALLBACK);
        assert_eq!(m.subject, "(no subject)");
        assert_eq!(m.from.email, "");
        assert_eq!(m.from.name.as_deref(), Some("Unknown sender"));
        assert_eq!(m.date_ms, FALLBACK);
    }

    #[test]
    fn truncated_multipart_returns_text() {
        let m = parse_message(TRUNC, FALLBACK);
        assert!(m.text.unwrap().contains("survives truncation"));
    }

    #[test]
    fn pdf_attachment() {
        let m = parse_message(PDF, FALLBACK);
        assert_eq!(m.attachments.len(), 1);
        let a = &m.attachments[0];
        assert_eq!(a.mime_type, "application/pdf");
        assert_eq!(a.filename, "report.pdf");
        assert!(a.size > 0);
        assert!(!a.inline);
    }

    #[test]
    fn cid_part_returns_png() {
        let (mime, bytes) = cid_part(INLINE, "logo123").unwrap();
        assert_eq!(mime, "image/png");
        assert_eq!(&bytes[..4], b"\x89PNG");
        assert!(cid_part(INLINE, "<logo123>").is_some());
        assert!(cid_part(INLINE, "nope").is_none());
    }

    #[test]
    fn inline_image_is_inline_attachment() {
        let m = parse_message(INLINE, FALLBACK);
        assert!(m.html.is_some());
        assert_eq!(m.attachments.len(), 1);
        assert!(m.attachments[0].inline);
        assert_eq!(m.attachments[0].content_id.as_deref(), Some("logo123"));
    }

    #[test]
    fn html_only_snippet_strips_tags_style_script() {
        let m = parse_message(REMOTE, FALLBACK);
        assert!(m.text.is_none() || m.text.as_deref() == Some(""));
        assert_eq!(m.snippet, "Visible text here.");
        assert!(m.html.unwrap().contains("tracker.example.com"));
    }

    #[test]
    fn snippet_limit_and_entities() {
        let body = format!(
            "From: a@b.c\r\nSubject: s\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<p>a &amp; b&nbsp;&lt;c&gt; &quot;d&quot; &#39;e&#39;</p><p>{}</p>",
            "é".repeat(500)
        );
        let m = parse_message(body.as_bytes(), FALLBACK);
        assert!(m.snippet.starts_with("a & b <c> \"d\" 'e' é"));
        assert_eq!(m.snippet.chars().count(), 200);
    }

    #[test]
    fn garbage_never_panics() {
        for raw in [
            &b""[..],
            b"\xff\xfe\x00garbage",
            b"\r\n\r\n",
            b"--x\r\n--x--",
        ] {
            let m = parse_message(raw, FALLBACK);
            assert_eq!(m.subject, "(no subject)");
            assert_eq!(m.from.name.as_deref(), Some("Unknown sender"));
            assert_eq!(m.date_ms, FALLBACK);
            assert!(m.snippet.chars().count() <= 200);
            let _ = cid_part(raw, "x");
        }
    }
}
