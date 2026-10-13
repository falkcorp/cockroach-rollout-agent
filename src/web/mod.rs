// file: src/web/mod.rs
// version: 1.6.0
// guid: 5a0df256-3b3d-47c1-8da7-af41189b6592
// last-edited: 2026-10-12

//! The built-in HTTPS dashboard.

pub mod auth;
pub mod db;
pub mod render;
pub mod routes;
pub mod tls;

use std::collections::HashMap;
use std::io;
use std::net::{SocketAddr, TcpListener};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use axum_server::tls_rustls::RustlsConfig;

use crate::cluster_view::ClusterView;
use crate::ops::Actor;

/// Everything the routes need from the outside world. Methods are blocking;
/// routes call them only through `blocking`. The DB implementation is
/// `db::DbBackend`; tests use an in-memory one.
pub trait Backend: Send + Sync + 'static {
    fn password_hash(&self) -> Result<Option<String>, String>;
    fn create_session(
        &self,
        token_sha256: &str,
        csrf_token: &str,
        ttl: Duration,
        client_ip: &str,
    ) -> Result<(), String>;
    /// The session for this token hash, if it exists and has not expired.
    fn session(&self, token_sha256: &str) -> Result<Option<auth::Session>, String>;
    fn delete_session(&self, token_sha256: &str) -> Result<(), String>;
    fn record_event(&self, kind: &str, detail: &str) -> Result<(), String>;
    fn view(&self) -> Result<ClusterView, String>;
    fn act(&self, action: WebAction, actor: &Actor) -> Result<String, String>;
    /// A stored UI certificate's file, or `None` if no certificate has this
    /// fingerprint. A key read is recorded as an event before it returns.
    fn ui_cert_file(
        &self,
        fingerprint: &str,
        file: CertFile,
        actor: &Actor,
    ) -> Result<Option<String>, String>;
}

/// The files a stored UI certificate can be downloaded as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CertFile {
    Cert,
    Key,
    /// The UI CA certificate the leaf chains to; this is what browsers trust.
    Ca,
}

impl CertFile {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "cert.pem" => Some(Self::Cert),
            "key.pem" => Some(Self::Key),
            "ca.crt" => Some(Self::Ca),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Cert => "cert.pem",
            Self::Key => "key.pem",
            Self::Ca => "ca.crt",
        }
    }
}

/// SHA-256 fingerprints as this code writes them: 64 lowercase hex digits.
pub fn is_fingerprint(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// A dashboard form, validated but not yet checked against cluster state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebAction {
    Approve {
        version: String,
        accept_warnings: bool,
    },
    Decline {
        rollout_id: String,
        reason: String,
    },
    Cancel {
        reason: String,
    },
    Finalize {
        confirm_version: String,
    },
    Restart,
    CheckReleases,
    UiCertDecide {
        fingerprint: String,
        approve: bool,
    },
    UiCertUpload {
        cert_pem: String,
        key_pem: String,
    },
}

pub fn parse_action(name: &str, form: &HashMap<String, String>) -> Result<WebAction, String> {
    let field = |key: &str| form.get(key).map(|value| value.trim()).unwrap_or("");
    match name {
        "approve" if !field("version").is_empty() => Ok(WebAction::Approve {
            version: field("version").to_string(),
            accept_warnings: field("accept_warnings") == "on",
        }),
        "approve" => Err("the approval form is missing the version".to_string()),
        "decline" if !field("rollout_id").is_empty() => Ok(WebAction::Decline {
            rollout_id: field("rollout_id").to_string(),
            reason: match field("reason") {
                "" => "declined from the dashboard".to_string(),
                reason => reason.to_string(),
            },
        }),
        "decline" => Err("the decline form is missing the rollout id".to_string()),
        "cancel" if !field("reason").is_empty() => Ok(WebAction::Cancel {
            reason: field("reason").to_string(),
        }),
        "cancel" => Err("a reason is required to cancel".to_string()),
        "finalize" if !field("confirm_version").is_empty() => Ok(WebAction::Finalize {
            confirm_version: field("confirm_version").to_string(),
        }),
        "finalize" => Err("type the target version to confirm finalizing".to_string()),
        "restart" if field("confirm") == "restart" => Ok(WebAction::Restart),
        "restart" => Err("type restart to confirm a rolling restart".to_string()),
        "check-releases" => Ok(WebAction::CheckReleases),
        "ui-cert-approve" | "ui-cert-reject" if is_fingerprint(field("fingerprint")) => {
            Ok(WebAction::UiCertDecide {
                fingerprint: field("fingerprint").to_string(),
                approve: name == "ui-cert-approve",
            })
        }
        "ui-cert-approve" | "ui-cert-reject" => {
            Err("the form is missing the certificate's fingerprint".to_string())
        }
        "ui-cert-upload" if !field("cert_pem").is_empty() && !field("key_pem").is_empty() => {
            Ok(WebAction::UiCertUpload {
                cert_pem: field("cert_pem").to_string(),
                key_pem: field("key_pem").to_string(),
            })
        }
        "ui-cert-upload" => Err("paste both the certificate and its key".to_string()),
        other => Err(format!("unknown action: {other}")),
    }
}

/// Runs blocking work (DB, argon2) off the async workers. A panic inside
/// becomes an error string instead of taking the connection down.
pub(crate) async fn blocking<T, F>(work: F) -> Result<T, String>
where
    F: FnOnce() -> Result<T, String> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| format!("internal error: {error}"))?
}

/// The running listener's TLS config, which the daemon swaps when a new
/// UI certificate is approved. New connections get the new certificate;
/// open ones finish on the old one.
pub struct UiServer {
    tls: RustlsConfig,
    served: Mutex<Option<String>>,
}

impl UiServer {
    pub fn new(tls: rustls::ServerConfig, served_fingerprint: Option<String>) -> Self {
        Self {
            tls: RustlsConfig::from_config(Arc::new(tls)),
            served: Mutex::new(served_fingerprint),
        }
    }

    /// Fingerprint of the certificate new connections receive.
    pub fn served(&self) -> Option<String> {
        self.served
            .lock()
            .map(|served| served.clone())
            .unwrap_or_default()
    }

    /// Builds the new config first, so a bad pair leaves the old one serving.
    pub fn install(&self, cert_pem: &str, key_pem: &str, fingerprint: &str) -> Result<(), String> {
        let config = tls::server_config(cert_pem.as_bytes(), key_pem.as_bytes())?;
        self.tls.reload_from_config(Arc::new(config));
        if let Ok(mut served) = self.served.lock() {
            *served = Some(fingerprint.to_string());
        }
        Ok(())
    }
}

pub async fn serve(
    listener: TcpListener,
    state: routes::AppState,
    tls: RustlsConfig,
) -> io::Result<()> {
    listener.set_nonblocking(true)?;
    let app = routes::router(state).into_make_service_with_connect_info::<SocketAddr>();
    axum_server::from_tcp_rustls(listener, tls)?
        .serve(app)
        .await
}

/// Binds on the caller's thread, so a bad address or a port in use is
/// reported at startup, then serves on a dedicated thread with its own
/// runtime. The daemon never waits on it.
pub fn spawn(
    listen: SocketAddr,
    state: routes::AppState,
    server: Arc<UiServer>,
) -> io::Result<thread::JoinHandle<()>> {
    let listener = TcpListener::bind(listen)?;
    let tls = server.tls.clone();
    thread::Builder::new()
        .name("web-ui".to_string())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                // Bounds DB connections and argon2 memory under a request flood.
                .max_blocking_threads(16)
                .thread_name("web-ui-worker")
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    eprintln!("ui disabled: could not start the runtime: {error}");
                    return;
                }
            };
            if let Err(error) = runtime.block_on(serve(listener, state, tls)) {
                eprintln!("ui stopped: {error}");
            }
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Generated per run so no credential literal lives in the source.
    fn password() -> &'static str {
        static PASSWORD: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        PASSWORD.get_or_init(auth::random_token)
    }

    struct FakeBackend {
        hash: String,
        view_error: Option<String>,
        session_error: Option<String>,
        sessions: Mutex<HashMap<String, auth::Session>>,
        acted: Mutex<Vec<WebAction>>,
        downloads: Mutex<Vec<String>>,
    }

    impl FakeBackend {
        fn new(view_error: Option<&str>) -> Arc<Self> {
            Arc::new(Self {
                hash: auth::hash_password(password()).unwrap(),
                view_error: view_error.map(str::to_string),
                session_error: None,
                sessions: Mutex::default(),
                acted: Mutex::default(),
                downloads: Mutex::default(),
            })
        }
    }

    impl Backend for FakeBackend {
        fn password_hash(&self) -> Result<Option<String>, String> {
            Ok(Some(self.hash.clone()))
        }
        fn create_session(
            &self,
            key: &str,
            csrf: &str,
            ttl: Duration,
            _ip: &str,
        ) -> Result<(), String> {
            self.sessions.lock().unwrap().insert(
                key.to_string(),
                auth::Session {
                    csrf_token: csrf.to_string(),
                    expires_unix: routes::now_unix() + ttl.as_secs() as i64,
                },
            );
            Ok(())
        }
        fn session(&self, key: &str) -> Result<Option<auth::Session>, String> {
            if let Some(error) = &self.session_error {
                return Err(error.clone());
            }
            Ok(self.sessions.lock().unwrap().get(key).cloned())
        }
        fn delete_session(&self, key: &str) -> Result<(), String> {
            self.sessions.lock().unwrap().remove(key);
            Ok(())
        }
        fn record_event(&self, _kind: &str, _detail: &str) -> Result<(), String> {
            Ok(())
        }
        fn view(&self) -> Result<ClusterView, String> {
            match &self.view_error {
                Some(error) => Err(error.clone()),
                None => Ok(ClusterView::sample()),
            }
        }
        fn act(&self, action: WebAction, _actor: &Actor) -> Result<String, String> {
            self.acted.lock().unwrap().push(action);
            Ok("done".to_string())
        }
        fn ui_cert_file(
            &self,
            fingerprint: &str,
            file: CertFile,
            _actor: &Actor,
        ) -> Result<Option<String>, String> {
            if fingerprint != "cd".repeat(32) {
                return Ok(None);
            }
            self.downloads
                .lock()
                .unwrap()
                .push(format!("{fingerprint} {}", file.name()));
            Ok(Some(format!("{} of {fingerprint}", file.name())))
        }
    }

    async fn start(fake: Arc<FakeBackend>) -> (String, reqwest::Client) {
        let cert = rcgen::generate_simple_self_signed(vec!["127.0.0.1".to_string()]).unwrap();
        let cert_pem = cert.cert.pem();
        let tls = tls::server_config(
            cert_pem.as_bytes(),
            cert.signing_key.serialize_pem().as_bytes(),
        )
        .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let state = routes::AppState::new(fake, Duration::from_secs(3600));
        tokio::spawn(serve(
            listener,
            state,
            RustlsConfig::from_config(Arc::new(tls)),
        ));
        let client = reqwest::Client::builder()
            .add_root_certificate(reqwest::Certificate::from_pem(cert_pem.as_bytes()).unwrap())
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        (format!("https://{addr}"), client)
    }

    /// Logs in and returns the `crra_session=…` cookie pair.
    async fn login(base: &str, client: &reqwest::Client) -> String {
        let response = client
            .post(format!("{base}/login"))
            .form(&[("password", password())])
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 303);
        cookie_pair(&response, auth::SESSION_COOKIE)
    }

    fn cookie_pair(response: &reqwest::Response, name: &str) -> String {
        response
            .headers()
            .get_all("set-cookie")
            .iter()
            .filter_map(|value| value.to_str().ok())
            .find(|value| value.starts_with(&format!("{name}=")))
            .unwrap_or_else(|| panic!("no {name} cookie"))
            .split(';')
            .next()
            .unwrap()
            .to_string()
    }

    fn csrf_from(html: &str) -> String {
        let marker = "name=\"csrf\" value=\"";
        let start = html.find(marker).expect("page has a csrf field") + marker.len();
        html[start..].split('"').next().unwrap().to_string()
    }

    #[tokio::test]
    async fn anonymous_dashboard_redirects_to_login_with_security_headers() {
        let (base, client) = start(FakeBackend::new(None)).await;
        let response = client.get(format!("{base}/")).send().await.unwrap();
        assert_eq!(response.status(), 303);
        assert_eq!(response.headers()["location"], "/login");
        for header in [
            "content-security-policy",
            "x-frame-options",
            "referrer-policy",
            "x-content-type-options",
            "cache-control",
        ] {
            assert!(response.headers().contains_key(header), "{header} missing");
        }
    }

    #[tokio::test]
    async fn wrong_passwords_get_401_then_429() {
        let (base, client) = start(FakeBackend::new(None)).await;
        for attempt in 1..=5 {
            let response = client
                .post(format!("{base}/login"))
                .form(&[("password", "wrong password here")])
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 401, "attempt {attempt}");
        }
        let response = client
            .post(format!("{base}/login"))
            .form(&[("password", password())])
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            429,
            "locked out even with the right password"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_wrong_passwords_cannot_exceed_the_lockout() {
        let (base, client) = start(FakeBackend::new(None)).await;
        let attempts = (0..20).map(|_| {
            let client = client.clone();
            let url = format!("{base}/login");
            tokio::spawn(async move {
                client
                    .post(url)
                    .form(&[("password", "wrong password here")])
                    .send()
                    .await
                    .unwrap()
                    .status()
                    .as_u16()
            })
        });
        let mut statuses = Vec::new();
        for attempt in attempts.collect::<Vec<_>>() {
            statuses.push(attempt.await.unwrap());
        }
        let checked = statuses.iter().filter(|status| **status == 401).count();
        assert!(
            checked <= 5,
            "{checked} passwords were checked: {statuses:?}"
        );
        assert!(
            statuses
                .iter()
                .all(|status| [401, 429, 503].contains(status)),
            "{statuses:?}"
        );
    }

    #[tokio::test]
    async fn correct_password_sets_a_strict_session_cookie() {
        let (base, client) = start(FakeBackend::new(None)).await;
        let response = client
            .post(format!("{base}/login"))
            .form(&[("password", password())])
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 303);
        assert_eq!(response.headers()["location"], "/");
        let set_cookie = response.headers()["set-cookie"]
            .to_str()
            .unwrap()
            .to_string();
        for part in [
            "crra_session=",
            "Secure",
            "HttpOnly",
            "SameSite=Strict",
            "Path=/",
        ] {
            assert!(
                set_cookie.contains(part),
                "{part} missing from {set_cookie}"
            );
        }
        let cookie = cookie_pair(&response, auth::SESSION_COOKIE);
        let page = client
            .get(format!("{base}/"))
            .header("cookie", cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(page.status(), 200);
        let html = page.text().await.unwrap();
        assert!(html.contains("Sign out"));
        assert!(html.contains("Rolling restart"));
    }

    #[tokio::test]
    async fn posts_without_csrf_are_forbidden() {
        let fake = FakeBackend::new(None);
        let (base, client) = start(fake.clone()).await;
        let cookie = login(&base, &client).await;
        let response = client
            .post(format!("{base}/actions/cancel"))
            .header("cookie", cookie)
            .form(&[("reason", "x")])
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 403);
        assert!(fake.acted.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn posts_with_an_unknown_session_go_to_login() {
        let fake = FakeBackend::new(None);
        let (base, client) = start(fake.clone()).await;
        let response = client
            .post(format!("{base}/actions/restart"))
            .header("cookie", "crra_session=deadbeef")
            .form(&[("csrf", "x"), ("confirm", "restart")])
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 303);
        assert_eq!(response.headers()["location"], "/login");
        assert!(fake.acted.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_valid_post_runs_the_action_and_flashes_the_result() {
        let fake = FakeBackend::new(None);
        let (base, client) = start(fake.clone()).await;
        let cookie = login(&base, &client).await;
        let html = client
            .get(format!("{base}/"))
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        let response = client
            .post(format!("{base}/actions/cancel"))
            .header("cookie", &cookie)
            .form(&[("csrf", csrf_from(&html).as_str()), ("reason", "testing")])
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 303);
        assert_eq!(response.headers()["location"], "/");
        assert_eq!(
            *fake.acted.lock().unwrap(),
            vec![WebAction::Cancel {
                reason: "testing".to_string()
            }]
        );
        let flash = cookie_pair(&response, "crra_flash");
        let page = client
            .get(format!("{base}/"))
            .header("cookie", format!("{cookie}; {flash}"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(page.contains("<p class=\"flash\">done</p>"), "{page}");
    }

    #[tokio::test]
    async fn key_downloads_need_a_session_and_are_never_cached() {
        let fake = FakeBackend::new(None);
        let (base, client) = start(fake.clone()).await;
        let fp = "cd".repeat(32);
        let url = format!("{base}/ui-cert/{fp}/key.pem");
        let anonymous = client.get(&url).send().await.unwrap();
        assert_eq!(anonymous.status(), 303);
        assert!(fake.downloads.lock().unwrap().is_empty());

        let cookie = login(&base, &client).await;
        let response = client
            .get(&url)
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["cache-control"], "no-store");
        let disposition = response.headers()["content-disposition"].to_str().unwrap();
        assert!(disposition.starts_with("attachment;"), "{disposition}");
        assert_eq!(response.text().await.unwrap(), format!("key.pem of {fp}"));
        assert_eq!(
            *fake.downloads.lock().unwrap(),
            vec![format!("{fp} key.pem")]
        );

        let missing = client
            .get(format!("{base}/ui-cert/{}/key.pem", "ee".repeat(32)))
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(missing.status(), 404);
        let bad = client
            .get(format!("{base}/ui-cert/{fp}/passwd"))
            .header("cookie", &cookie)
            .send()
            .await
            .unwrap();
        assert_eq!(bad.status(), 404);
    }

    #[tokio::test]
    async fn database_errors_show_a_banner_without_actions() {
        let (base, client) = start(FakeBackend::new(Some("connection refused"))).await;
        let cookie = login(&base, &client).await;
        let html = client
            .get(format!("{base}/"))
            .header("cookie", cookie)
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(html.contains("database unreachable: connection refused"));
        assert!(!html.contains("/actions/"));
    }

    /// The real backend reads the session from the DB first, so an outage
    /// fails there: the page script swaps in any page with `#main`, even a
    /// 503, so the banner replaces the stale dashboard.
    #[tokio::test]
    async fn an_outage_during_session_lookup_serves_the_banner_as_main() {
        let fake = Arc::new(FakeBackend {
            session_error: Some("connection refused".to_string()),
            ..Arc::try_unwrap(FakeBackend::new(None)).ok().unwrap()
        });
        let (base, client) = start(fake).await;
        let response = client
            .get(format!("{base}/"))
            .header("cookie", "crra_session=abc")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 503);
        let html = response.text().await.unwrap();
        assert!(html.contains("<main id=\"main\">"), "{html}");
        assert!(html.contains("database unreachable"), "{html}");
        assert!(!html.contains("/actions/"));
    }

    #[tokio::test]
    async fn an_installed_certificate_serves_new_connections() {
        use crate::uicert;
        let now = time::OffsetDateTime::now_utc();
        let hosts = uicert::hosts_from(&[], &["127.0.0.1:1".to_string()], &["ui.test".to_string()]);
        // Loopback is never put in a UI certificate, so name the leaf for
        // ui.test and resolve that name to the listener.
        let ca = uicert::generate_ca(&hosts, now).unwrap();
        let leaf = uicert::issue_leaf(&ca, &hosts, now, 30).unwrap();
        let old = rcgen::generate_simple_self_signed(vec!["ui.test".to_string()]).unwrap();
        let server = Arc::new(UiServer::new(
            tls::server_config(
                old.cert.pem().as_bytes(),
                old.signing_key.serialize_pem().as_bytes(),
            )
            .unwrap(),
            Some("old".to_string()),
        ));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let state = routes::AppState::new(FakeBackend::new(None), Duration::from_secs(3600));
        tokio::spawn(serve(listener, state, server.tls.clone()));
        let client = reqwest::Client::builder()
            .add_root_certificate(reqwest::Certificate::from_pem(ca.cert.as_bytes()).unwrap())
            .resolve("ui.test", addr)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let url = format!("https://ui.test:{}/login", addr.port());
        assert!(client.get(&url).send().await.is_err(), "still the old cert");
        assert!(server.install("not a cert", "nor a key", "bad").is_err());
        assert_eq!(
            server.served().as_deref(),
            Some("old"),
            "a bad pair changes nothing"
        );
        server.install(&leaf.cert, &leaf.key, "new").unwrap();
        let fresh = client.get(&url).send().await.unwrap();
        assert_eq!(fresh.status(), 200);
        assert_eq!(server.served().as_deref(), Some("new"));
    }

    #[test]
    fn actions_parse_from_form_fields() {
        let form = |pairs: &[(&str, &str)]| -> HashMap<String, String> {
            pairs
                .iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect()
        };
        assert_eq!(
            parse_action(
                "approve",
                &form(&[("version", "25.4.17"), ("accept_warnings", "on")])
            ),
            Ok(WebAction::Approve {
                version: "25.4.17".to_string(),
                accept_warnings: true
            })
        );
        assert_eq!(
            parse_action("approve", &form(&[("version", "25.4.17")])),
            Ok(WebAction::Approve {
                version: "25.4.17".to_string(),
                accept_warnings: false
            })
        );
        assert!(parse_action("cancel", &form(&[("reason", "  ")])).is_err());
        assert!(parse_action("finalize", &form(&[])).is_err());
        assert!(parse_action("restart", &form(&[("confirm", "yes")])).is_err());
        assert_eq!(
            parse_action("restart", &form(&[("confirm", "restart")])),
            Ok(WebAction::Restart)
        );
        assert_eq!(
            parse_action(
                "decline",
                &form(&[("rollout_id", "r1"), ("reason", " not yet ")])
            ),
            Ok(WebAction::Decline {
                rollout_id: "r1".to_string(),
                reason: "not yet".to_string()
            })
        );
        assert_eq!(
            parse_action("decline", &form(&[("rollout_id", "r1")])),
            Ok(WebAction::Decline {
                rollout_id: "r1".to_string(),
                reason: "declined from the dashboard".to_string()
            }),
            "the reason is optional"
        );
        assert!(parse_action("decline", &form(&[("reason", "x")])).is_err());
        assert_eq!(
            parse_action("check-releases", &form(&[])),
            Ok(WebAction::CheckReleases)
        );
        let fp = "ab".repeat(32);
        assert_eq!(
            parse_action("ui-cert-approve", &form(&[("fingerprint", fp.as_str())])),
            Ok(WebAction::UiCertDecide {
                fingerprint: fp.clone(),
                approve: true
            })
        );
        assert_eq!(
            parse_action("ui-cert-reject", &form(&[("fingerprint", fp.as_str())])),
            Ok(WebAction::UiCertDecide {
                fingerprint: fp.clone(),
                approve: false
            })
        );
        assert!(parse_action("ui-cert-approve", &form(&[("fingerprint", "abc")])).is_err());
        assert!(
            parse_action(
                "ui-cert-approve",
                &form(&[("fingerprint", &"AB".repeat(32))])
            )
            .is_err()
        );
        assert!(parse_action("ui-cert-upload", &form(&[("cert_pem", "c")])).is_err());
        assert_eq!(
            parse_action(
                "ui-cert-upload",
                &form(&[("cert_pem", "c"), ("key_pem", "k")])
            ),
            Ok(WebAction::UiCertUpload {
                cert_pem: "c".to_string(),
                key_pem: "k".to_string()
            })
        );
        assert!(parse_action("drop-table", &form(&[])).is_err());
    }
}
