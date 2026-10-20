// This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0. If a copy of the MPL was not distributed with this file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Turn a typed email address into account settings.
//!
//! A bundled profile wins immediately. Otherwise MX, the Microsoft user-realm probe,
//! Thunderbird ISPDB, the domain's autoconfig file, and DNS SRV run together until
//! the deadline. A source that is still running at the deadline counts as no match.
//! Only the domain leaves the machine; the user-realm probe sends `probe@{domain}`.

mod autoconfig;
mod dns;
mod profiles;

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::join;

use crate::provider::ImapConfig;
use crate::{CoreError, Result};

pub use dns::{FakeResolver, HickoryResolver, Resolver};
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
/// point them at a local server.
pub struct DetectEnv {
    pub resolver: Arc<dyn Resolver>,
    pub http: reqwest::Client,
    pub ispdb_base: String,
    pub realm_base: String,
    pub deadline: Duration,
    pub autoconfig_url_templates: [String; 2],
}

impl DetectEnv {
    /// System DNS, Thunderbird ISPDB, Microsoft user-realm, and an 8 second deadline.
    pub fn system() -> Result<Self> {
        Ok(Self {
            resolver: Arc::new(HickoryResolver::system()?),
            http: reqwest::Client::builder()
                .connect_timeout(DEADLINE)
                .timeout(DEADLINE)
                .build()
                .map_err(|err| CoreError::Network(format!("HTTP client: {err}")))?,
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
    if let Some(setup) = profiles::for_domain(&domain) {
        return Ok(setup);
    }
    Ok(discover(env, &domain).await)
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

fn is_hostname(domain: &str) -> bool {
    if domain.is_empty() || domain.len() > 253 {
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

/// Run every network source together. Results that land before the deadline are
/// kept; a source still pending when the clock fires is a miss.
async fn discover(env: &DetectEnv, domain: &str) -> AccountSetup {
    let mx = Mutex::new(None);
    let realm = Mutex::new(None);
    let ispdb = Mutex::new(None);
    let domain_config = Mutex::new(None);
    let srv = Mutex::new(None);

    let pending = async {
        join!(
            record(&mx, dns::from_mx(env.resolver.as_ref(), domain)),
            record(
                &realm,
                autoconfig::from_realm(&env.http, &env.realm_base, domain),
            ),
            record(
                &ispdb,
                autoconfig::from_ispdb(&env.http, &env.ispdb_base, domain),
            ),
            record(
                &domain_config,
                autoconfig::from_domain(&env.http, &env.autoconfig_url_templates, domain),
            ),
            record(&srv, dns::from_srv(env.resolver.as_ref(), domain)),
        );
    };
    let _ = tokio::time::timeout(env.deadline, pending).await;

    for slot in [&mx, &realm, &ispdb, &domain_config, &srv] {
        if let Some(setup) = slot.lock().expect("setup slot").take() {
            return setup;
        }
    }
    AccountSetup::Unknown
}

async fn record(
    slot: &Mutex<Option<AccountSetup>>,
    lookup: impl Future<Output = Option<AccountSetup>>,
) {
    if let Some(setup) = lookup.await {
        *slot.lock().expect("setup slot") = Some(setup);
    }
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
                    .build()
                    .expect("http client"),
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
                assert_eq!(config.username, "email");
                assert_eq!(username_alternatives, ["email"]);
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
                    username: "email".into(),
                    smtp_host: "smtp.example.com".into(),
                    smtp_port: 587,
                    smtp_security: Security::StartTls,
                },
                username_alternatives: vec!["email".into()],
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
                    username: "local".into(),
                    smtp_host: "smtp.starttls.example".into(),
                    smtp_port: 587,
                    smtp_security: Security::StartTls,
                },
                username_alternatives: vec!["local".into()],
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
                    username: "email".into(),
                    smtp_host: "smtp.srv.example".into(),
                    smtp_port: 587,
                    smtp_security: Security::StartTls,
                },
                username_alternatives: vec!["email".into()],
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
