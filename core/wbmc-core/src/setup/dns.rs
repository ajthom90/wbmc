// This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0. If a copy of the MPL was not distributed with this file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! MX and SRV lookups. A lookup error is "no match", same as an empty answer.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use futures::join;
use hickory_resolver::TokioResolver;
use hickory_resolver::net::NetError;
use hickory_resolver::proto::rr::RData;
use hickory_resolver::proto::rr::domain::Name;

use super::AccountSetup;
use super::profiles;
use crate::provider::{ImapConfig, Security};
use crate::{CoreError, Result};

/// DNS queries account detection is allowed to make.
#[async_trait]
pub trait Resolver: Send + Sync {
    async fn mx(&self, domain: &str) -> Result<Vec<String>>;
    async fn srv(&self, name: &str) -> Result<Vec<(String, u16)>>;
}

/// System resolver (`/etc/resolv.conf` on Unix, the registry on Windows).
pub struct HickoryResolver {
    inner: TokioResolver,
}

impl HickoryResolver {
    pub fn system() -> Result<Self> {
        let inner = TokioResolver::builder_tokio()
            .and_then(|builder| builder.build())
            .map_err(|err| CoreError::Network(err.to_string()))?;
        Ok(Self { inner })
    }
}

#[async_trait]
impl Resolver for HickoryResolver {
    async fn mx(&self, domain: &str) -> Result<Vec<String>> {
        let lookup = match self.inner.mx_lookup(domain).await {
            Ok(lookup) => lookup,
            Err(err) if no_records(&err) => return Ok(Vec::new()),
            Err(err) => return Err(CoreError::Network(err.to_string())),
        };
        let mut hosts = Vec::new();
        for record in lookup.answers() {
            if let RData::MX(mx) = &record.data {
                let host = normalize_name(&mx.exchange);
                if !host.is_empty() {
                    hosts.push(host);
                }
            }
        }
        Ok(hosts)
    }

    async fn srv(&self, name: &str) -> Result<Vec<(String, u16)>> {
        let lookup = match self.inner.srv_lookup(name).await {
            Ok(lookup) => lookup,
            Err(err) if no_records(&err) => return Ok(Vec::new()),
            Err(err) => return Err(CoreError::Network(err.to_string())),
        };
        let mut found = Vec::new();
        for record in lookup.answers() {
            if let RData::SRV(srv) = &record.data {
                let host = normalize_name(&srv.target);
                if host.is_empty() || srv.port == 0 {
                    continue;
                }
                found.push((srv.priority, srv.weight, host, srv.port));
            }
        }
        // Lower priority value is preferred. Weight breaks ties toward the heavier target.
        found.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
        Ok(found
            .into_iter()
            .map(|(_, _, host, port)| (host, port))
            .collect())
    }
}

fn no_records(err: &NetError) -> bool {
    err.is_no_records_found() || err.is_nx_domain()
}

fn normalize_name(name: &Name) -> String {
    name.to_ascii().trim_end_matches('.').to_ascii_lowercase()
}

/// Scripted resolver for tests. An unknown name is an empty answer, not an error.
#[derive(Default)]
pub struct FakeResolver {
    mx: Mutex<HashMap<String, Vec<String>>>,
    srv: Mutex<HashMap<String, Vec<(String, u16)>>>,
    calls: Mutex<Vec<String>>,
}

impl FakeResolver {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_mx(&self, domain: &str, hosts: &[&str]) {
        self.mx.lock().expect("mx map").insert(
            domain.to_ascii_lowercase(),
            hosts.iter().map(|host| (*host).to_string()).collect(),
        );
    }

    pub fn set_srv(&self, name: &str, records: &[(&str, u16)]) {
        self.srv.lock().expect("srv map").insert(
            name.to_ascii_lowercase(),
            records
                .iter()
                .map(|(host, port)| ((*host).to_string(), *port))
                .collect(),
        );
    }

    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("call log").clone()
    }

    fn note(&self, kind: &str, name: &str) {
        self.calls
            .lock()
            .expect("call log")
            .push(format!("{kind} {name}"));
    }
}

#[async_trait]
impl Resolver for FakeResolver {
    async fn mx(&self, domain: &str) -> Result<Vec<String>> {
        let domain = domain.to_ascii_lowercase();
        self.note("mx", &domain);
        Ok(self
            .mx
            .lock()
            .expect("mx map")
            .get(&domain)
            .cloned()
            .unwrap_or_default())
    }

    async fn srv(&self, name: &str) -> Result<Vec<(String, u16)>> {
        let name = name.to_ascii_lowercase();
        self.note("srv", &name);
        Ok(self
            .srv
            .lock()
            .expect("srv map")
            .get(&name)
            .cloned()
            .unwrap_or_default())
    }
}

/// Google, Microsoft 365, or iCloud, from MX host names. Anything else is no match.
pub(crate) fn classify_mx(hosts: &[String]) -> Option<AccountSetup> {
    let hosts: Vec<String> = hosts.iter().map(|host| normalize_host(host)).collect();
    if hosts
        .iter()
        .any(|host| host.ends_with(".google.com") || host.ends_with(".googlemail.com"))
    {
        return Some(AccountSetup::Google);
    }
    if hosts
        .iter()
        .any(|host| host.ends_with(".protection.outlook.com"))
    {
        return Some(AccountSetup::Microsoft);
    }
    if hosts
        .iter()
        .any(|host| host == "mx01.mail.icloud.com" || host == "mx02.mail.icloud.com")
    {
        return profiles::icloud_mx();
    }
    None
}

fn normalize_host(host: &str) -> String {
    host.trim_end_matches('.').to_ascii_lowercase()
}

/// An SRV target is usable only when it is the email domain or a subdomain of it.
fn srv_host_trusted(host: &str, domain: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if !super::is_hostname(&host) {
        return false;
    }
    let domain = domain.trim_end_matches('.').to_ascii_lowercase();
    host == domain || host.ends_with(&format!(".{domain}"))
}

pub(crate) async fn from_mx(resolver: &dyn Resolver, domain: &str) -> Option<AccountSetup> {
    match resolver.mx(domain).await {
        Ok(hosts) => classify_mx(&hosts),
        Err(err) => {
            tracing::debug!(%domain, %err, "mx lookup failed");
            None
        }
    }
}

/// RFC 6186: `_imaps` is implicit TLS, `_submission` is STARTTLS. Both are required.
pub(crate) async fn from_srv(resolver: &dyn Resolver, domain: &str) -> Option<AccountSetup> {
    let imap_name = format!("_imaps._tcp.{domain}");
    let smtp_name = format!("_submission._tcp.{domain}");
    let (imap, smtp) = join!(resolver.srv(&imap_name), resolver.srv(&smtp_name));
    let imap = match imap {
        Ok(records) => records,
        Err(err) => {
            tracing::debug!(%domain, %err, "imaps srv lookup failed");
            return None;
        }
    };
    let smtp = match smtp {
        Ok(records) => records,
        Err(err) => {
            tracing::debug!(%domain, %err, "submission srv lookup failed");
            return None;
        }
    };
    let (host, port) = imap.into_iter().next()?;
    let (smtp_host, smtp_port) = smtp.into_iter().next()?;
    if !srv_host_trusted(&host, domain) || !srv_host_trusted(&smtp_host, domain) {
        tracing::debug!(%domain, %host, %smtp_host, "srv target rejected");
        return None;
    }
    Some(AccountSetup::Imap {
        provider_name: domain.to_string(),
        config: ImapConfig {
            host,
            port,
            security: Security::Tls,
            username: "email".to_string(),
            smtp_host,
            smtp_port,
            smtp_security: Security::StartTls,
        },
        username_alternatives: vec!["email".to_string()],
        app_password_url: None,
    })
}
