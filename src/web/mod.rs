// file: src/web/mod.rs
// version: 1.3.0
// guid: 5a0df256-3b3d-47c1-8da7-af41189b6592
// last-edited: 2026-10-11

//! The built-in HTTPS dashboard.

pub mod auth;
pub mod db;
pub mod render;
pub mod routes;
pub mod tls;

use std::collections::HashMap;
use std::io;
use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;
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
}

/// A dashboard form, validated but not yet checked against cluster state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebAction {
    Approve {
        version: String,
        accept_warnings: bool,
    },
    Cancel {
        reason: String,
    },
    Finalize {
        confirm_version: String,
    },
    Restart,
}

pub fn parse_action(name: &str, form: &HashMap<String, String>) -> Result<WebAction, String> {
    let field = |key: &str| form.get(key).map(|value| value.trim()).unwrap_or("");
    match name {
        "approve" if !field("version").is_empty() => Ok(WebAction::Approve {
            version: field("version").to_string(),
            accept_warnings: field("accept_warnings") == "on",
        }),
        "approve" => Err("the approval form is missing the version".to_string()),
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

pub async fn serve(
    listener: TcpListener,
    state: routes::AppState,
    tls: rustls::ServerConfig,
) -> io::Result<()> {
    listener.set_nonblocking(true)?;
    let app = routes::router(state).into_make_service_with_connect_info::<SocketAddr>();
    axum_server::from_tcp_rustls(listener, RustlsConfig::from_config(Arc::new(tls)))?
        .serve(app)
        .await
}

/// Binds on the caller's thread, so a bad address or a port in use is
/// reported at startup, then serves on a dedicated thread with its own
/// runtime. The daemon never waits on it.
pub fn spawn(
    listen: SocketAddr,
    state: routes::AppState,
    tls: rustls::ServerConfig,
) -> io::Result<thread::JoinHandle<()>> {
    let listener = TcpListener::bind(listen)?;
    thread::Builder::new()
        .name("web-ui".to_string())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
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

    const PASSWORD: &str = "correct horse battery";

    struct FakeBackend {
        hash: String,
        view_error: Option<String>,
        sessions: Mutex<HashMap<String, auth::Session>>,
        acted: Mutex<Vec<WebAction>>,
    }

    impl FakeBackend {
        fn new(view_error: Option<&str>) -> Arc<Self> {
            Arc::new(Self {
                hash: auth::hash_password(PASSWORD).unwrap(),
                view_error: view_error.map(str::to_string),
                sessions: Mutex::default(),
                acted: Mutex::default(),
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
        let state = routes::AppState {
            backend: fake,
            limiter: Arc::new(auth::LoginLimiter::new()),
            session_ttl: Duration::from_secs(3600),
        };
        tokio::spawn(serve(listener, state, tls));
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
            .form(&[("password", PASSWORD)])
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
            .form(&[("password", PASSWORD)])
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            429,
            "locked out even with the right password"
        );
    }

    #[tokio::test]
    async fn correct_password_sets_a_strict_session_cookie() {
        let (base, client) = start(FakeBackend::new(None)).await;
        let response = client
            .post(format!("{base}/login"))
            .form(&[("password", PASSWORD)])
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
        assert!(parse_action("drop-table", &form(&[])).is_err());
    }
}
