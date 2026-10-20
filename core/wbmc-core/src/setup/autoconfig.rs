// This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0. If a copy of the MPL was not distributed with this file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Thunderbird config-v1.1 documents and the Microsoft user-realm probe.
//!
//! A transport error, a non-success status, non-XML, or a document with no usable
//! IMAP server is "no match". Plain sockets are refused unless the host is localhost.

use quick_xml::events::Event;
use quick_xml::{Reader, XmlVersion};
use serde::Deserialize;

use super::AccountSetup;
use crate::provider::{ImapConfig, Security};

const MAX_BODY: usize = 1024 * 1024;

#[derive(Deserialize)]
struct RealmBody {
    #[serde(rename = "NameSpaceType")]
    name_space_type: Option<String>,
}

/// `Managed` and `Federated` are Microsoft 365 tenants, including ones behind a mail filter.
pub(crate) async fn from_realm(
    http: &reqwest::Client,
    realm_base: &str,
    domain: &str,
    allow_http: bool,
) -> Option<AccountSetup> {
    // The local part of the typed address never leaves the machine.
    let url = format!(
        "{}/?user=probe@{domain}&api-version=2.1",
        realm_base.trim_end_matches('/')
    );
    let body = fetch_body(http, &url, allow_http).await?;
    let realm: RealmBody = serde_json::from_str(&body).ok()?;
    match realm.name_space_type.as_deref() {
        Some("Managed") | Some("Federated") => Some(AccountSetup::Microsoft),
        _ => None,
    }
}

pub(crate) async fn from_ispdb(
    http: &reqwest::Client,
    ispdb_base: &str,
    domain: &str,
    allow_http: bool,
) -> Option<AccountSetup> {
    let url = format!("{}/{}", ispdb_base.trim_end_matches('/'), domain);
    let body = fetch_body(http, &url, allow_http).await?;
    parse_config(&body)
}

/// Tries `autoconfig.{domain}` first, then the well-known URL. `{domain}` is the only placeholder.
pub(crate) async fn from_domain(
    http: &reqwest::Client,
    templates: &[String; 2],
    domain: &str,
    allow_http: bool,
) -> Option<AccountSetup> {
    for template in templates {
        let url = template.replace("{domain}", domain);
        let Some(body) = fetch_body(http, &url, allow_http).await else {
            continue;
        };
        if let Some(setup) = parse_config(&body) {
            return Some(setup);
        }
    }
    None
}

fn url_allowed(url: &str, allow_http: bool) -> bool {
    let bytes = url.as_bytes();
    if bytes.len() >= 8 && bytes[..8].eq_ignore_ascii_case(b"https://") {
        return true;
    }
    allow_http && bytes.len() >= 7 && bytes[..7].eq_ignore_ascii_case(b"http://")
}

async fn fetch_body(http: &reqwest::Client, url: &str, allow_http: bool) -> Option<String> {
    if !url_allowed(url, allow_http) {
        tracing::debug!(url, "autoconfig url scheme rejected");
        return None;
    }
    let mut response = match http.get(url).send().await {
        Ok(response) => response,
        Err(err) => {
            tracing::debug!(url, %err, "autoconfig fetch failed");
            return None;
        }
    };
    if !response.status().is_success() {
        tracing::debug!(url, status = %response.status(), "autoconfig fetch missed");
        return None;
    }
    if let Some(len) = response.content_length()
        && len > MAX_BODY as u64
    {
        tracing::debug!(url, len, "autoconfig body too large");
        return None;
    }
    let mut body = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                if body.len().saturating_add(chunk.len()) > MAX_BODY {
                    tracing::debug!(url, "autoconfig body too large");
                    return None;
                }
                body.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(err) => {
                tracing::debug!(url, %err, "autoconfig body failed");
                return None;
            }
        }
    }
    String::from_utf8(body).ok()
}

#[derive(Clone)]
struct Draft {
    imap: bool,
    host: String,
    port: String,
    socket: String,
    username: String,
    saw_username: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Field {
    None,
    Host,
    Port,
    Socket,
    Username,
    Display,
}

#[derive(Clone)]
struct Built {
    imap: bool,
    host: String,
    port: u16,
    security: Security,
    username: &'static str,
}

/// Prefer IMAP SSL over STARTTLS over plain. Plain on any host but localhost is no match.
pub(crate) fn parse_config(xml: &str) -> Option<AccountSetup> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text_start = true;
    reader.config_mut().trim_text_end = true;
    reader.config_mut().expand_empty_elements = true;

    let mut skip: u32 = 0;
    let mut field = Field::None;
    let mut text = String::new();
    let mut display = String::new();
    let mut draft: Option<Draft> = None;
    let mut servers: Vec<Draft> = Vec::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(start)) => {
                if skip > 0 {
                    skip += 1;
                    continue;
                }
                match tag(start.local_name().as_ref()) {
                    Tag::Incoming => {
                        let kind = attr(&start, "type").unwrap_or_default();
                        if kind.eq_ignore_ascii_case("imap") {
                            draft = Some(empty_draft(true));
                        } else {
                            skip = 1;
                        }
                    }
                    Tag::Outgoing => {
                        let kind = attr(&start, "type").unwrap_or_else(|| "smtp".to_string());
                        if kind.eq_ignore_ascii_case("smtp") {
                            draft = Some(empty_draft(false));
                        } else {
                            skip = 1;
                        }
                    }
                    Tag::Host => field = Field::Host,
                    Tag::Port => field = Field::Port,
                    Tag::Socket => field = Field::Socket,
                    Tag::Username => field = Field::Username,
                    Tag::Display => field = Field::Display,
                    Tag::Other => {}
                }
            }
            Ok(Event::Text(chunk)) => {
                if field != Field::None {
                    text.push_str(chunk.xml10_content().as_ref());
                }
            }
            Ok(Event::End(end)) => {
                if skip > 0 {
                    skip -= 1;
                    continue;
                }
                match tag(end.local_name().as_ref()) {
                    Tag::Host | Tag::Port | Tag::Socket | Tag::Username | Tag::Display => {
                        commit(&mut draft, &mut display, field, &text);
                        field = Field::None;
                        text.clear();
                    }
                    Tag::Incoming | Tag::Outgoing => {
                        if let Some(server) = draft.take() {
                            servers.push(server);
                        }
                    }
                    Tag::Other => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(err) => {
                tracing::debug!(%err, "autoconfig xml rejected");
                return None;
            }
            _ => {}
        }
    }

    let built: Vec<Built> = servers.iter().filter_map(build_server).collect();
    let imap = best(built.iter().filter(|server| server.imap))?.clone();
    let smtp = best(built.iter().filter(|server| !server.imap))?.clone();
    if !plain_allowed(&imap) || !plain_allowed(&smtp) {
        tracing::debug!("autoconfig plain socket rejected");
        return None;
    }
    let username = imap.username.to_string();
    Some(AccountSetup::Imap {
        provider_name: if display.is_empty() {
            "IMAP".to_string()
        } else {
            display
        },
        config: ImapConfig {
            host: imap.host,
            port: imap.port,
            security: imap.security,
            username: username.clone(),
            smtp_host: smtp.host,
            smtp_port: smtp.port,
            smtp_security: smtp.security,
        },
        username_alternatives: vec![username],
        app_password_url: None,
    })
}

fn empty_draft(imap: bool) -> Draft {
    Draft {
        imap,
        host: String::new(),
        port: String::new(),
        socket: String::new(),
        username: String::new(),
        saw_username: false,
    }
}

fn commit(draft: &mut Option<Draft>, display: &mut String, field: Field, text: &str) {
    let value = text.trim();
    match field {
        Field::Display => {
            if display.is_empty() && !value.is_empty() {
                *display = value.to_string();
            }
        }
        Field::Host => {
            if let Some(draft) = draft {
                draft.host = value.to_string();
            }
        }
        Field::Port => {
            if let Some(draft) = draft {
                draft.port = value.to_string();
            }
        }
        Field::Socket => {
            if let Some(draft) = draft {
                draft.socket = value.to_string();
            }
        }
        Field::Username => {
            if let Some(draft) = draft {
                draft.username = value.to_string();
                draft.saw_username = true;
            }
        }
        Field::None => {}
    }
}

fn build_server(draft: &Draft) -> Option<Built> {
    if !super::is_hostname(&draft.host) {
        return None;
    }
    let port = draft.port.parse().ok()?;
    if port == 0 {
        return None;
    }
    let security = match draft.socket.to_ascii_lowercase().as_str() {
        "ssl" => Security::Tls,
        "starttls" => Security::StartTls,
        "plain" => Security::None,
        _ => return None,
    };
    let username = if !draft.saw_username || draft.username.eq_ignore_ascii_case("%EMAILADDRESS%") {
        "email"
    } else if draft.username.eq_ignore_ascii_case("%EMAILLOCALPART%") {
        "local"
    } else {
        return None;
    };
    Some(Built {
        imap: draft.imap,
        host: draft.host.clone(),
        port,
        security,
        username,
    })
}

/// SSL, then STARTTLS, then plain. `min_by_key` keeps the first server at the best rank.
fn best<'a>(servers: impl Iterator<Item = &'a Built>) -> Option<&'a Built> {
    servers.min_by_key(|server| match server.security {
        Security::Tls => 0,
        Security::StartTls => 1,
        Security::None => 2,
    })
}

fn plain_allowed(server: &Built) -> bool {
    server.security != Security::None || server.host.eq_ignore_ascii_case("localhost")
}

#[derive(Clone, Copy)]
enum Tag {
    Incoming,
    Outgoing,
    Host,
    Port,
    Socket,
    Username,
    Display,
    Other,
}

fn tag(name: &str) -> Tag {
    if name.eq_ignore_ascii_case("incomingServer") {
        Tag::Incoming
    } else if name.eq_ignore_ascii_case("outgoingServer") {
        Tag::Outgoing
    } else if name.eq_ignore_ascii_case("hostname") {
        Tag::Host
    } else if name.eq_ignore_ascii_case("port") {
        Tag::Port
    } else if name.eq_ignore_ascii_case("socketType") {
        Tag::Socket
    } else if name.eq_ignore_ascii_case("username") {
        Tag::Username
    } else if name.eq_ignore_ascii_case("displayName") {
        Tag::Display
    } else {
        Tag::Other
    }
}

fn attr(start: &quick_xml::events::BytesStart<'_>, name: &str) -> Option<String> {
    for attr in start.attributes().flatten() {
        if attr.key.local_name().as_ref() == name {
            return Some(
                attr.normalized_value(XmlVersion::Implicit1_0)
                    .ok()?
                    .into_owned(),
            );
        }
    }
    None
}
