// This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0. If a copy of the MPL was not distributed with this file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Bundled provider profiles. A domain hit returns immediately, with no network.

use std::collections::HashMap;
use std::sync::OnceLock;

use serde::Deserialize;

use super::AccountSetup;
use crate::provider::{ImapConfig, Security};

const PROFILES_TOML: &str = include_str!("profiles.toml");

/// One IMAP profile from `profiles.toml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BundledImap {
    pub domains: Vec<String>,
    pub provider_name: String,
    pub config: ImapConfig,
    pub username_alternatives: Vec<String>,
    pub app_password_url: Option<String>,
    pub source: String,
}

#[derive(Clone, Copy)]
enum Kind {
    Google,
    Microsoft,
    Imap(usize),
}

struct Table {
    by_domain: HashMap<String, Kind>,
    imap: Vec<BundledImap>,
}

pub fn bundled_imap_profiles() -> &'static [BundledImap] {
    &table().imap
}

pub(crate) fn for_domain(domain: &str) -> Option<AccountSetup> {
    let table = table();
    match table.by_domain.get(domain)? {
        Kind::Google => Some(AccountSetup::Google),
        Kind::Microsoft => Some(AccountSetup::Microsoft),
        Kind::Imap(index) => Some(table.imap[*index].to_setup()),
    }
}

/// iCloud's servers, but the username is the full address. Custom domains on iCloud
/// do not accept the local part the way `@icloud.com` does.
pub(crate) fn icloud_mx() -> Option<AccountSetup> {
    let table = table();
    let profile = table
        .imap
        .iter()
        .find(|profile| profile.domains.iter().any(|domain| domain == "icloud.com"))?;
    let mut setup = profile.to_setup();
    if let AccountSetup::Imap {
        config,
        username_alternatives,
        ..
    } = &mut setup
    {
        config.username = "email".to_string();
        *username_alternatives = vec!["email".to_string()];
    }
    Some(setup)
}

fn table() -> &'static Table {
    static TABLE: OnceLock<Table> = OnceLock::new();
    TABLE.get_or_init(|| Table::load().expect("profiles.toml"))
}

impl BundledImap {
    fn to_setup(&self) -> AccountSetup {
        AccountSetup::Imap {
            provider_name: self.provider_name.clone(),
            config: self.config.clone(),
            username_alternatives: self.username_alternatives.clone(),
            app_password_url: self.app_password_url.clone(),
        }
    }
}

impl Table {
    fn load() -> std::result::Result<Self, String> {
        let file: ProfileFile =
            toml::from_str(PROFILES_TOML).map_err(|err| format!("parse profiles.toml: {err}"))?;
        let mut by_domain = HashMap::new();
        let mut imap = Vec::new();
        for row in file.profile {
            match row.kind.as_str() {
                "google" => insert_kind(&mut by_domain, &row.domains, Kind::Google)?,
                "microsoft" => insert_kind(&mut by_domain, &row.domains, Kind::Microsoft)?,
                "imap" => {
                    let profile = row.into_imap()?;
                    let index = imap.len();
                    insert_kind(&mut by_domain, &profile.domains, Kind::Imap(index))?;
                    imap.push(profile);
                }
                other => return Err(format!("unknown profile kind {other}")),
            }
        }
        Ok(Self { by_domain, imap })
    }
}

fn insert_kind(
    by_domain: &mut HashMap<String, Kind>,
    domains: &[String],
    kind: Kind,
) -> std::result::Result<(), String> {
    if domains.is_empty() {
        return Err("profile has no domains".into());
    }
    for domain in domains {
        let domain = domain.to_ascii_lowercase();
        if by_domain.contains_key(&domain) {
            return Err(format!("duplicate profile domain {domain}"));
        }
        by_domain.insert(domain, kind);
    }
    Ok(())
}

#[derive(Deserialize)]
struct ProfileFile {
    profile: Vec<ProfileRow>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileRow {
    domains: Vec<String>,
    kind: String,
    name: Option<String>,
    host: Option<String>,
    port: Option<u16>,
    security: Option<String>,
    smtp_host: Option<String>,
    smtp_port: Option<u16>,
    smtp_security: Option<String>,
    username: Option<String>,
    app_password_url: Option<String>,
    source: Option<String>,
}

impl ProfileRow {
    fn into_imap(self) -> std::result::Result<BundledImap, String> {
        let name = self.name.ok_or("imap profile missing name")?;
        let host = self.host.ok_or_else(|| format!("{name} missing host"))?;
        let port = self.port.ok_or_else(|| format!("{name} missing port"))?;
        let security = parse_security(self.security.as_deref(), &name, "security")?;
        let smtp_host = self
            .smtp_host
            .ok_or_else(|| format!("{name} missing smtp_host"))?;
        let smtp_port = self
            .smtp_port
            .ok_or_else(|| format!("{name} missing smtp_port"))?;
        let smtp_security = parse_security(self.smtp_security.as_deref(), &name, "smtp_security")?;
        allow_plain(&host, security)?;
        allow_plain(&smtp_host, smtp_security)?;
        let style = self
            .username
            .ok_or_else(|| format!("{name} missing username"))?;
        let username_alternatives = alternatives(&style)?;
        let source = self
            .source
            .filter(|source| !source.is_empty())
            .ok_or_else(|| format!("{name} missing source"))?;
        if self.app_password_url.is_none()
            && !self.domains.iter().any(|domain| domain == "wbmc.test")
        {
            return Err(format!("{name} missing app_password_url"));
        }
        Ok(BundledImap {
            domains: self.domains,
            provider_name: name,
            config: ImapConfig {
                host,
                port,
                security,
                username: username_alternatives[0].clone(),
                smtp_host,
                smtp_port,
                smtp_security,
            },
            username_alternatives,
            app_password_url: self.app_password_url,
            source,
        })
    }
}

fn parse_security(
    value: Option<&str>,
    name: &str,
    field: &str,
) -> std::result::Result<Security, String> {
    match value {
        Some("tls") => Ok(Security::Tls),
        Some("starttls") => Ok(Security::StartTls),
        Some("none") => Ok(Security::None),
        Some(other) => Err(format!(
            "{name} {field} {other} is not tls, starttls, or none"
        )),
        None => Err(format!("{name} missing {field}")),
    }
}

fn allow_plain(host: &str, security: Security) -> std::result::Result<(), String> {
    if security == Security::None && !host.eq_ignore_ascii_case("localhost") {
        return Err(format!(
            "plain security is only allowed for localhost, not {host}"
        ));
    }
    Ok(())
}

fn alternatives(style: &str) -> std::result::Result<Vec<String>, String> {
    let values: &[&str] = match style {
        "email" => &["email"],
        "local" => &["local"],
        "local_then_email" => &["local", "email"],
        other => return Err(format!("unknown username style {other}")),
    };
    Ok(values.iter().map(|value| (*value).to_string()).collect())
}
