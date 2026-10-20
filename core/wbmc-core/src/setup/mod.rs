// This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0. If a copy of the MPL was not distributed with this file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Turn a typed email address into account settings.
//!
//! A bundled profile wins immediately. Otherwise MX, the Microsoft user-realm probe,
//! Thunderbird ISPDB, the domain's autoconfig file, and DNS SRV run together. A match
//! is returned as soon as every higher-priority source has finished. A source that is
//! still running at the deadline counts as no match. Only the domain leaves the
//! machine; the user-realm probe sends `probe@{domain}`. Login names are written into
//! the returned settings and are not sent.

mod autoconfig;
mod dns;
mod profiles;

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::future::FutureExt;
use futures::stream::{FuturesUnordered, StreamExt};

use crate::provider::ImapConfig;
use crate::{CoreError, Result};

#[cfg(any(test, feature = "test-util"))]
pub use dns::FakeResolver;
pub use dns::{HickoryResolver, Resolver};
pub use profiles::BundledImap;

const ISPDB_BASE: &str = "https://autoconfig.thunderbird.net/v1.1";
const REALM_BASE: &str = "https://login.microsoftonline.com/common/userrealm";
const AUTOCONFIG_SUBDOMAIN: &str = "https://autoconfig.{domain}/mail/config-v1.1.xml";
const AUTOCONFIG_WELL_KNOWN: &str = "https://{domain}/.well-known/autoconfig/mail/config-v1.1.xml";
const DEADLINE: Duration = Duration::from_secs(8);

/// What to do with an address the user typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountSetup {
    Microsoft,
    Google,
    Imap {
        provider_name: String,
        config: ImapConfig,
        username_alternatives: Vec<String>,
        app_password_url: Option<String>,
    },
    Unknown,
}

/// Network endpoints and the clock for one detection.
///
/// `autoconfig_url_templates` build the two domain-autoconfig URLs. `{domain}` is
/// replaced with the email domain. The defaults are the public https URLs; tests
/// point them at a local server. `allow_http` is set only by tests; [`DetectEnv::system`]
/// never fetches `http://`. The client does not follow redirects.
pub struct DetectEnv {
    pub resolver: Arc<dyn Resolver>,
    pub(crate) http: reqwest::Client,
    pub(crate) allow_http: bool,
    pub ispdb_base: String,
    pub realm_base: String,
    pub deadline: Duration,
    pub(crate) autoconfig_url_templates: [String; 2],
}

impl DetectEnv {
    #[cfg(any(test, feature = "test-util"))]
    pub fn for_test(
        resolver: Arc<dyn Resolver>,
        base: &str,
        deadline: Duration,
        autoconfig_url_templates: [String; 2],
    ) -> Self {
        Self {
            resolver,
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("test HTTP client"),
            allow_http: true,
            ispdb_base: base.to_string(),
            realm_base: base.to_string(),
            deadline,
            autoconfig_url_templates,
        }
    }

    /// System DNS, Thunderbird ISPDB, Microsoft user-realm, and an 8 second deadline.
    /// Redirects are not followed, and `http://` URLs are not fetched.
    pub fn system() -> Result<Self> {
        Ok(Self {
            resolver: Arc::new(HickoryResolver::system()?),
            http: reqwest::Client::builder()
                .connect_timeout(DEADLINE)
                .timeout(DEADLINE)
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|err| CoreError::Network(format!("HTTP client: {err}")))?,
            allow_http: false,
            ispdb_base: ISPDB_BASE.to_string(),
            realm_base: REALM_BASE.to_string(),
            deadline: DEADLINE,
            autoconfig_url_templates: [
                AUTOCONFIG_SUBDOMAIN.to_string(),
                AUTOCONFIG_WELL_KNOWN.to_string(),
            ],
        })
    }
}

pub fn bundled_imap_profiles() -> &'static [BundledImap] {
    profiles::bundled_imap_profiles()
}

/// Detect settings for `email`. An address without exactly one `@` and a domain
/// that is a hostname is [`CoreError::InvalidInput`].
pub async fn detect(email: &str, env: &DetectEnv) -> Result<AccountSetup> {
    let domain = email_domain(email)?;
    let setup = if let Some(setup) = profiles::for_domain(&domain) {
        setup
    } else {
        discover(env, &domain).await
    };
    Ok(realize_usernames(setup, email))
}

/// Style tokens stay inside detection. The returned settings hold the typed address.
fn realize_usernames(mut setup: AccountSetup, email: &str) -> AccountSetup {
    let AccountSetup::Imap {
        config,
        username_alternatives,
        ..
    } = &mut setup
    else {
        return setup;
    };
    let Some((local, domain)) = email.split_once('@') else {
        return setup;
    };
    let full = format!("{local}@{}", domain.to_ascii_lowercase());
    let names = expand_usernames(username_alternatives, local, &full);
    config.username = names[0].clone();
    *username_alternatives = names;
    setup
}

fn expand_usernames(tokens: &[String], local: &str, full: &str) -> Vec<String> {
    let mut names = Vec::with_capacity(tokens.len());
    for token in tokens {
        match token.as_str() {
            "email" => names.push(full.to_string()),
            "local" => names.push(local.to_string()),
            "local_then_email" => {
                names.push(local.to_string());
                names.push(full.to_string());
            }
            other => names.push(other.to_string()),
        }
    }
    if names.is_empty() {
        names.push(full.to_string());
    }
    names
}

fn email_domain(email: &str) -> Result<String> {
    let Some((local, domain)) = email.split_once('@') else {
        return Err(invalid_address());
    };
    if local.contains('@') || domain.contains('@') {
        return Err(invalid_address());
    }
    let domain = domain.to_ascii_lowercase();
    if !is_hostname(&domain) {
        return Err(invalid_address());
    }
    Ok(domain)
}

fn invalid_address() -> CoreError {
    CoreError::InvalidInput(
        "email address must contain exactly one @ and a non-empty domain".into(),
    )
}

pub(crate) fn is_hostname(domain: &str) -> bool {
    if domain.is_empty()
        || domain.len() > 253
        || domain.parse::<std::net::IpAddr>().is_ok()
        || domain
            .rsplit('.')
            .next()
            .is_some_and(|label| label.bytes().all(|b| b.is_ascii_digit()))
    {
        return false;
    }
    domain.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    })
}

/// Run every network source together. Index 0 is the highest priority.
/// A match is returned once every higher-priority source has finished; a source
/// still pending at the deadline is a miss.
async fn discover(env: &DetectEnv, domain: &str) -> AccountSetup {
    let mut pending = FuturesUnordered::new();
    pending.push(indexed(0, dns::from_mx(env.resolver.as_ref(), domain)).boxed());
    pending.push(
        indexed(
            1,
            autoconfig::from_realm(&env.http, &env.realm_base, domain, env.allow_http),
        )
        .boxed(),
    );
    pending.push(
        indexed(
            2,
            autoconfig::from_ispdb(&env.http, &env.ispdb_base, domain, env.allow_http),
        )
        .boxed(),
    );
    pending.push(
        indexed(
            3,
            autoconfig::from_domain(
                &env.http,
                &env.autoconfig_url_templates,
                domain,
                env.allow_http,
            ),
        )
        .boxed(),
    );
    pending.push(indexed(4, dns::from_srv(env.resolver.as_ref(), domain)).boxed());

    let slots = Mutex::new([None, None, None, None, None]);
    let race = async {
        while let Some((index, setup)) = pending.next().await {
            let mut guard = slots.lock().expect("setup slots");
            guard[index] = Some(setup);
            if let Some(winner) = decided(&mut *guard) {
                return winner;
            }
        }
        AccountSetup::Unknown
    };
    match tokio::time::timeout(env.deadline, race).await {
        Ok(setup) => setup,
        Err(_) => decided_at_deadline(&mut *slots.lock().expect("setup slots")),
    }
}

async fn indexed(
    index: usize,
    lookup: impl Future<Output = Option<AccountSetup>>,
) -> (usize, Option<AccountSetup>) {
    (index, lookup.await)
}

/// Highest-priority finished match, once every source above it has finished.
/// `None` means a higher-priority source is still running.
fn decided(slots: &mut [Option<Option<AccountSetup>>]) -> Option<AccountSetup> {
    for slot in slots.iter_mut() {
        match slot.as_mut() {
            None => return None,
            Some(None) => {}
            Some(found) => return found.take(),
        }
    }
    Some(AccountSetup::Unknown)
}

fn decided_at_deadline(slots: &mut [Option<Option<AccountSetup>>]) -> AccountSetup {
    for slot in slots.iter_mut() {
        if let Some(setup) = slot.as_mut().and_then(|found| found.take()) {
            return setup;
        }
    }
    AccountSetup::Unknown
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::provider::Security;

    const ISPDB_EXAMPLE: &str = include_str!("testdata/ispdb_example.xml");
    const STARTTLS_ONLY: &str = include_str!("testdata/starttls_only.xml");
    const PLAIN_ONLY: &str = include_str!("testdata/plain_only.xml");
    const MALFORMED: &str = include_str!("testdata/malformed.xml");
    const CAPTIVE_PORTAL: &str = include_str!("testdata/captive_portal.html");

    struct Lab {
        resolver: Arc<FakeResolver>,
        server: MockServer,
    }

    impl Lab {
        async fn start() -> Self {
            Self {
                resolver: Arc::new(FakeResolver::new()),
                server: MockServer::start().await,
            }
        }

        fn env(&self, deadline: Duration) -> DetectEnv {
            let base = self.server.uri();
            DetectEnv {
                resolver: Arc::clone(&self.resolver) as Arc<dyn Resolver>,
                http: reqwest::Client::builder()
                    .timeout(Duration::from_secs(60))
                    .redirect(reqwest::redirect::Policy::none())
                    .build()
                    .expect("http client"),
                allow_http: true,
                ispdb_base: format!("{base}/ispdb"),
                realm_base: format!("{base}/common/userrealm"),
                deadline,
                autoconfig_url_templates: [
                    format!("{base}/autoconfig.{{domain}}/mail/config-v1.1.xml"),
                    format!("{base}/{{domain}}/.well-known/autoconfig/mail/config-v1.1.xml"),
                ],
            }
        }
    }

    fn usual(lab: &Lab) -> DetectEnv {
        lab.env(Duration::from_secs(2))
    }

    #[tokio::test]
    async fn profile_gmail_is_google_without_network() {
        let lab = Lab::start().await;
        let setup = detect("Ada@Gmail.com", &usual(&lab)).await.unwrap();
        assert_eq!(setup, AccountSetup::Google);
        assert!(lab.resolver.calls().is_empty());
        let requests = lab.server.received_requests().await.unwrap();
        assert!(requests.is_empty());
    }

    #[test]
    fn autoconfig_rejects_ip_and_numeric_last_labels() {
        for host in [
            "127.0.0.1",
            "10.0.0.1",
            "192.168.1.1",
            "::1",
            "::ffff:127.0.0.1",
            "mail.123",
            "127.1",
            "2130706433",
        ] {
            assert!(!is_hostname(host), "{host}");
            assert!(
                autoconfig::parse_config(&autoconfig_document(host, "smtp.example.com")).is_none(),
                "{host}"
            );
            assert!(
                autoconfig::parse_config(&autoconfig_document("imap.example.com", host)).is_none(),
                "{host}"
            );
        }
        assert!(is_hostname("localhost"));
        assert!(is_hostname("imap.example.com"));
    }

    #[test]
    fn autoconfig_rejects_plain_even_on_localhost() {
        let xml = autoconfig_document("localhost", "localhost");
        assert!(autoconfig::parse_config(&xml).is_some());
        for xml in [
            xml.replace("SSL", "plain"),
            xml.replace("STARTTLS", "plain"),
            xml.replace("SSL", "plain").replace("STARTTLS", "plain"),
        ] {
            assert!(autoconfig::parse_config(&xml).is_none());
        }
    }

    #[tokio::test]
    async fn bundled_localhost_plain_bypasses_autoconfig() {
        let resolver = Arc::new(FakeResolver::new());
        let env = DetectEnv::for_test(
            resolver.clone(),
            "http://localhost",
            Duration::from_millis(1),
            [String::new(), String::new()],
        );
        let AccountSetup::Imap { config, .. } = detect("ada@wbmc.test", &env).await.unwrap() else {
            panic!("expected bundled IMAP profile");
        };
        assert_eq!(config.host, "localhost");
        assert_eq!(config.security, Security::None);
        assert_eq!(config.smtp_security, Security::None);
        assert_eq!(config.username, "ada@wbmc.test");
        assert!(resolver.calls().is_empty());
    }

    #[test]
    fn profile_imap_entries_load() {
        let entries = bundled_imap_profiles();
        assert!(entries.len() >= 4);
        let mut saw_dev = false;
        for entry in entries {
            assert!(!entry.source.is_empty(), "{}", entry.provider_name);
            assert!(!entry.domains.is_empty(), "{}", entry.provider_name);
            let dev = entry.domains.iter().any(|domain| domain == "wbmc.test");
            if dev {
                saw_dev = true;
                assert!(entry.app_password_url.is_none());
                assert_eq!(entry.source, "dev/mail/docker-compose.yml");
                assert_eq!(entry.config.host, "localhost");
                assert_eq!(entry.config.port, 3143);
                assert_eq!(entry.config.security, Security::None);
                assert_eq!(entry.config.smtp_host, "localhost");
                assert_eq!(entry.config.smtp_port, 3025);
                assert_eq!(entry.config.smtp_security, Security::None);
                assert_eq!(entry.config.username, "email");
                assert_eq!(entry.username_alternatives, ["email"]);
            } else {
                let url = entry.app_password_url.as_deref().unwrap_or("");
                assert!(
                    url.starts_with("https://"),
                    "{} missing app_password_url",
                    entry.provider_name
                );
                assert_ne!(entry.config.security, Security::None);
                assert_ne!(entry.config.smtp_security, Security::None);
            }
            assert!(
                entry.username_alternatives == ["email"]
                    || entry.username_alternatives == ["local"]
                    || entry.username_alternatives == ["local", "email"],
                "{} username {:?}",
                entry.provider_name,
                entry.username_alternatives
            );
        }
        assert!(saw_dev);
        assert!(
            entries
                .iter()
                .any(|entry| entry.domains.iter().any(|d| d == "yahoo.com"))
        );
        assert!(
            entries
                .iter()
                .any(|entry| entry.domains.iter().any(|d| d == "aol.com"))
        );
        assert!(entries.iter().any(|entry| {
            entry.domains.iter().any(|d| d == "icloud.com")
                && entry.username_alternatives == ["local", "email"]
                && entry.config.username == "local"
        }));
    }

    #[tokio::test]
    async fn mx_google_workspace() {
        let lab = Lab::start().await;
        lab.resolver
            .set_mx("workspace.example", &["aspmx.l.google.com"]);
        let setup = detect("ada@workspace.example", &usual(&lab)).await.unwrap();
        assert_eq!(setup, AccountSetup::Google);
    }

    #[tokio::test]
    async fn mx_m365() {
        let lab = Lab::start().await;
        lab.resolver.set_mx(
            "contoso.example",
            &["contoso-com.mail.protection.outlook.com"],
        );
        let setup = detect("ada@contoso.example", &usual(&lab)).await.unwrap();
        assert_eq!(setup, AccountSetup::Microsoft);
    }

    #[tokio::test]
    async fn mx_icloud() {
        let lab = Lab::start().await;
        lab.resolver
            .set_mx("custom.example", &["mx02.mail.icloud.com"]);
        let setup = detect("ada@custom.example", &usual(&lab)).await.unwrap();
        match setup {
            AccountSetup::Imap {
                provider_name,
                config,
                username_alternatives,
                app_password_url,
            } => {
                assert_eq!(provider_name, "iCloud");
                assert_eq!(config.host, "imap.mail.me.com");
                assert_eq!(config.port, 993);
                assert_eq!(config.security, Security::Tls);
                assert_eq!(config.smtp_host, "smtp.mail.me.com");
                assert_eq!(config.smtp_port, 587);
                assert_eq!(config.smtp_security, Security::StartTls);
                assert_eq!(config.username, "ada@custom.example");
                assert_eq!(username_alternatives, ["ada@custom.example"]);
                assert!(
                    app_password_url
                        .unwrap()
                        .starts_with("https://support.apple.com/")
                );
            }
            other => panic!("expected iCloud imap, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn realm_managed_is_microsoft() {
        let lab = Lab::start().await;
        Mock::given(method("GET"))
            .and(path("/common/userrealm/"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "NameSpaceType": "Managed",
                "DomainName": "contoso.example",
            })))
            .mount(&lab.server)
            .await;
        let setup = detect("not-the-probe@contoso.example", &usual(&lab))
            .await
            .unwrap();
        assert_eq!(setup, AccountSetup::Microsoft);

        let requests = lab.server.received_requests().await.unwrap();
        let realm = requests
            .iter()
            .find(|request| request.url.path().contains("userrealm"))
            .expect("user-realm request");
        let target = match realm.url.query() {
            Some(query) => format!("{}?{query}", realm.url.path()),
            None => realm.url.path().to_string(),
        };
        assert!(
            target.contains("probe@contoso.example"),
            "probe missing from {target}"
        );
        assert!(
            !target.contains("not-the-probe"),
            "real local part leaked in {target}"
        );
    }

    #[tokio::test]
    async fn ispdb_parses_imap_settings() {
        let lab = Lab::start().await;
        Mock::given(method("GET"))
            .and(path("/ispdb/ispdb.example"))
            .respond_with(ResponseTemplate::new(200).set_body_string(ISPDB_EXAMPLE))
            .mount(&lab.server)
            .await;
        let setup = detect("ada@ispdb.example", &usual(&lab)).await.unwrap();
        assert_eq!(
            setup,
            AccountSetup::Imap {
                provider_name: "Example".into(),
                config: ImapConfig {
                    host: "imap.example.com".into(),
                    port: 993,
                    security: Security::Tls,
                    username: "ada@ispdb.example".into(),
                    smtp_host: "smtp.example.com".into(),
                    smtp_port: 587,
                    smtp_security: Security::StartTls,
                },
                username_alternatives: vec!["ada@ispdb.example".into()],
                app_password_url: None,
            }
        );
    }

    #[tokio::test]
    async fn domain_autoconfig_fallback() {
        let lab = Lab::start().await;
        Mock::given(method("GET"))
            .and(path("/autoconfig.starttls.example/mail/config-v1.1.xml"))
            .respond_with(ResponseTemplate::new(200).set_body_string(STARTTLS_ONLY))
            .mount(&lab.server)
            .await;
        let setup = detect("ada@starttls.example", &usual(&lab)).await.unwrap();
        assert_eq!(
            setup,
            AccountSetup::Imap {
                provider_name: "StartTls Mail".into(),
                config: ImapConfig {
                    host: "imap.starttls.example".into(),
                    port: 143,
                    security: Security::StartTls,
                    username: "ada".into(),
                    smtp_host: "smtp.starttls.example".into(),
                    smtp_port: 587,
                    smtp_security: Security::StartTls,
                },
                username_alternatives: vec!["ada".into()],
                app_password_url: None,
            }
        );
    }

    #[tokio::test]
    async fn srv_fallback() {
        let lab = Lab::start().await;
        lab.resolver
            .set_srv("_imaps._tcp.srv.example", &[("imap.srv.example", 993)]);
        lab.resolver
            .set_srv("_submission._tcp.srv.example", &[("smtp.srv.example", 587)]);
        let setup = detect("ada@srv.example", &usual(&lab)).await.unwrap();
        assert_eq!(
            setup,
            AccountSetup::Imap {
                provider_name: "srv.example".into(),
                config: ImapConfig {
                    host: "imap.srv.example".into(),
                    port: 993,
                    security: Security::Tls,
                    username: "ada@srv.example".into(),
                    smtp_host: "smtp.srv.example".into(),
                    smtp_port: 587,
                    smtp_security: Security::StartTls,
                },
                username_alternatives: vec!["ada@srv.example".into()],
                app_password_url: None,
            }
        );
    }

    #[tokio::test]
    async fn plain_rejected() {
        let lab = Lab::start().await;
        Mock::given(method("GET"))
            .and(path("/ispdb/plain.example"))
            .respond_with(ResponseTemplate::new(200).set_body_string(PLAIN_ONLY))
            .mount(&lab.server)
            .await;
        let setup = detect("ada@plain.example", &usual(&lab)).await.unwrap();
        assert_eq!(setup, AccountSetup::Unknown);
    }

    #[tokio::test]
    async fn malformed_and_html_ignored() {
        let lab = Lab::start().await;
        Mock::given(method("GET"))
            .and(path("/ispdb/bad.example"))
            .respond_with(ResponseTemplate::new(200).set_body_string(MALFORMED))
            .mount(&lab.server)
            .await;
        Mock::given(method("GET"))
            .and(path("/autoconfig.bad.example/mail/config-v1.1.xml"))
            .respond_with(ResponseTemplate::new(200).set_body_string(CAPTIVE_PORTAL))
            .mount(&lab.server)
            .await;
        Mock::given(method("GET"))
            .and(path(
                "/bad.example/.well-known/autoconfig/mail/config-v1.1.xml",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_string(CAPTIVE_PORTAL))
            .mount(&lab.server)
            .await;
        let setup = detect("ada@bad.example", &usual(&lab)).await.unwrap();
        assert_eq!(setup, AccountSetup::Unknown);
    }

    #[tokio::test]
    async fn slow_sources_respect_deadline() {
        let lab = Lab::start().await;
        Mock::given(method("GET"))
            .and(path("/ispdb/slow.example"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_secs(30))
                    .set_body_string(ISPDB_EXAMPLE),
            )
            .mount(&lab.server)
            .await;
        let env = lab.env(Duration::from_millis(300));
        let started = Instant::now();
        let setup = detect("ada@slow.example", &env).await.unwrap();
        let elapsed = started.elapsed();
        assert_eq!(setup, AccountSetup::Unknown);
        assert!(
            elapsed < Duration::from_secs(1),
            "detection took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn icloud_usernames_are_real() {
        let lab = Lab::start().await;
        let setup = detect("ada@icloud.com", &usual(&lab)).await.unwrap();
        match setup {
            AccountSetup::Imap {
                provider_name,
                config,
                username_alternatives,
                ..
            } => {
                assert_eq!(provider_name, "iCloud");
                assert_eq!(config.username, "ada");
                assert_eq!(username_alternatives, ["ada", "ada@icloud.com"]);
            }
            other => panic!("expected iCloud, got {other:?}"),
        }
        assert!(lab.resolver.calls().is_empty());
    }

    #[tokio::test]
    async fn oversized_body_is_no_match() {
        let lab = Lab::start().await;
        let body = "x".repeat(2 * 1024 * 1024 + 1);
        Mock::given(method("GET"))
            .and(path("/ispdb/huge.example"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&lab.server)
            .await;
        let setup = detect("ada@huge.example", &usual(&lab)).await.unwrap();
        assert_eq!(setup, AccountSetup::Unknown);
    }

    #[tokio::test]
    async fn redirect_to_http_is_no_match() {
        let lab = Lab::start().await;
        let location = format!("{}/ispdb/followed.example", lab.server.uri());
        assert!(location.starts_with("http://"), "{location}");
        Mock::given(method("GET"))
            .and(path("/ispdb/redir.example"))
            .respond_with(ResponseTemplate::new(302).insert_header("Location", location.as_str()))
            .mount(&lab.server)
            .await;
        Mock::given(method("GET"))
            .and(path("/ispdb/followed.example"))
            .respond_with(ResponseTemplate::new(200).set_body_string(ISPDB_EXAMPLE))
            .mount(&lab.server)
            .await;
        let setup = detect("ada@redir.example", &usual(&lab)).await.unwrap();
        assert_eq!(setup, AccountSetup::Unknown);
    }

    #[tokio::test]
    async fn mx_match_does_not_wait_for_slow_autoconfig() {
        let lab = Lab::start().await;
        lab.resolver
            .set_mx("priority.example", &["aspmx.l.google.com"]);
        Mock::given(method("GET"))
            .and(path("/autoconfig.priority.example/mail/config-v1.1.xml"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_secs(30))
                    .set_body_string(ISPDB_EXAMPLE),
            )
            .mount(&lab.server)
            .await;
        let env = lab.env(Duration::from_secs(5));
        let started = Instant::now();
        let setup = detect("ada@priority.example", &env).await.unwrap();
        let elapsed = started.elapsed();
        assert_eq!(setup, AccountSetup::Google);
        assert!(
            elapsed < Duration::from_secs(1),
            "detection took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn srv_target_must_belong_to_domain() {
        let lab = Lab::start().await;
        lab.resolver
            .set_srv("_imaps._tcp.corp.test", &[("evil.example", 993)]);
        lab.resolver
            .set_srv("_submission._tcp.corp.test", &[("evil.example", 587)]);
        let rejected = detect("ada@corp.test", &usual(&lab)).await.unwrap();
        assert_eq!(rejected, AccountSetup::Unknown);

        lab.resolver
            .set_srv("_imaps._tcp.corp.test", &[("mail.corp.test", 993)]);
        lab.resolver
            .set_srv("_submission._tcp.corp.test", &[("evil.example", 587)]);
        let one_sided = detect("ada@corp.test", &usual(&lab)).await.unwrap();
        assert_eq!(one_sided, AccountSetup::Unknown);

        lab.resolver
            .set_srv("_imaps._tcp.corp.test", &[("mail.corp.test", 993)]);
        lab.resolver
            .set_srv("_submission._tcp.corp.test", &[("smtp.corp.test", 587)]);
        let setup = detect("ada@corp.test", &usual(&lab)).await.unwrap();
        assert_eq!(
            setup,
            AccountSetup::Imap {
                provider_name: "corp.test".into(),
                config: ImapConfig {
                    host: "mail.corp.test".into(),
                    port: 993,
                    security: Security::Tls,
                    username: "ada@corp.test".into(),
                    smtp_host: "smtp.corp.test".into(),
                    smtp_port: 587,
                    smtp_security: Security::StartTls,
                },
                username_alternatives: vec!["ada@corp.test".into()],
                app_password_url: None,
            }
        );
    }

    #[tokio::test]
    async fn invalid_autoconfig_hostname_is_no_match() {
        let lab = Lab::start().await;
        for (domain, host) in [
            ("space.example", "evil host"),
            ("colon.example", "a:25"),
            ("empty.example", ""),
        ] {
            Mock::given(method("GET"))
                .and(path(format!("/ispdb/{domain}")))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_string(autoconfig_document(host, "smtp.example.com")),
                )
                .mount(&lab.server)
                .await;
            let setup = detect(&format!("ada@{domain}"), &usual(&lab))
                .await
                .unwrap();
            assert_eq!(setup, AccountSetup::Unknown, "{host:?}");
        }
    }

    #[tokio::test]
    async fn invalid_srv_hostname_is_no_match() {
        let lab = Lab::start().await;
        for (domain, host) in [
            ("space-srv.example", "evil host"),
            ("colon-srv.example", "a:25"),
            ("empty-srv.example", ""),
        ] {
            lab.resolver
                .set_srv(&format!("_imaps._tcp.{domain}"), &[(host, 993)]);
            lab.resolver.set_srv(
                &format!("_submission._tcp.{domain}"),
                &[(&format!("smtp.{domain}"), 587)],
            );
            let setup = detect(&format!("ada@{domain}"), &usual(&lab))
                .await
                .unwrap();
            assert_eq!(setup, AccountSetup::Unknown, "{host:?}");
        }
    }

    fn autoconfig_document(imap_host: &str, smtp_host: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<clientConfig version="1.1">
  <emailProvider id="host.example">
    <displayName>Host Test</displayName>
    <incomingServer type="imap">
      <hostname>{imap_host}</hostname>
      <port>993</port>
      <socketType>SSL</socketType>
      <username>%EMAILADDRESS%</username>
    </incomingServer>
    <outgoingServer type="smtp">
      <hostname>{smtp_host}</hostname>
      <port>587</port>
      <socketType>STARTTLS</socketType>
      <username>%EMAILADDRESS%</username>
    </outgoingServer>
  </emailProvider>
</clientConfig>"#
        )
    }

    #[tokio::test]
    async fn invalid_address_rejected() {
        let lab = Lab::start().await;
        let env = usual(&lab);
        for bad in [
            "",
            "no-at",
            "a@b@c",
            "@",
            "local@",
            "a@@b",
            "a@not a domain",
        ] {
            let err = detect(bad, &env).await.expect_err(bad);
            assert!(matches!(err, CoreError::InvalidInput(_)), "{bad} -> {err}");
        }
        assert!(lab.resolver.calls().is_empty());
    }

    #[tokio::test]
    #[ignore]
    async fn live_detect() {
        if std::env::var("WBMC_TEST_DETECT").ok().as_deref() != Some("1") {
            eprintln!("set WBMC_TEST_DETECT=1 to query real DNS");
            return;
        }
        let env = DetectEnv::system().expect("system resolver");

        let gmail = env.resolver.mx("gmail.com").await.expect("gmail mx");
        assert!(
            gmail
                .iter()
                .any(|host| { host.ends_with(".google.com") || host.ends_with(".googlemail.com") }),
            "{gmail:?}"
        );
        let outlook = env.resolver.mx("outlook.com").await.expect("outlook mx");
        assert!(
            outlook
                .iter()
                .any(|host| host.ends_with(".protection.outlook.com")),
            "{outlook:?}"
        );
        let icloud = env.resolver.mx("icloud.com").await.expect("icloud mx");
        assert!(
            icloud
                .iter()
                .any(|host| { host == "mx01.mail.icloud.com" || host == "mx02.mail.icloud.com" }),
            "{icloud:?}"
        );

        assert_eq!(
            detect("person@gmail.com", &env).await.unwrap(),
            AccountSetup::Google
        );
        assert_eq!(
            detect("person@outlook.com", &env).await.unwrap(),
            AccountSetup::Microsoft
        );
        match detect("person@icloud.com", &env).await.unwrap() {
            AccountSetup::Imap { config, .. } => {
                assert_eq!(config.host, "imap.mail.me.com");
                assert_eq!(config.port, 993);
                assert_eq!(config.security, Security::Tls);
            }
            other => panic!("expected iCloud profile, got {other:?}"),
        }
    }
}
