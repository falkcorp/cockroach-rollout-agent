// file: src/web/routes.rs
// version: 1.0.0
// guid: f0fcb0b6-3ed3-41ea-a74c-a624cd821a36
// last-edited: 2026-10-11

//! HTTP handlers. Each handler checks the session, then CSRF on POST, and
//! reaches the backend only through `blocking`.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::extract::{ConnectInfo, Form, Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use tower_http::catch_panic::CatchPanicLayer;

use super::auth::{self, LoginLimiter, Session};
use super::{Backend, blocking, parse_action, render};
use crate::ops::{Actor, available_actions};

const FLASH_COOKIE: &str = "crra_flash";
/// Hex doubles the size; this keeps the cookie under the 4 KiB browser limit.
const FLASH_MAX_BYTES: usize = 1500;

#[derive(Clone)]
pub struct AppState {
    pub backend: Arc<dyn Backend>,
    pub limiter: Arc<LoginLimiter>,
    pub session_ttl: Duration,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(dashboard))
        .route("/login", get(login_form).post(login))
        .route("/logout", post(logout))
        .route("/actions/{name}", post(action))
        .route("/static/app.css", get(css))
        .route("/static/app.js", get(js))
        .layer(CatchPanicLayer::new())
        .layer(middleware::from_fn(security_headers))
        .with_state(state)
}

pub(crate) fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

async fn security_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    for (name, value) in [
        (
            "content-security-policy",
            "default-src 'self'; form-action 'self'; frame-ancestors 'none'",
        ),
        ("x-frame-options", "DENY"),
        ("referrer-policy", "no-referrer"),
        ("x-content-type-options", "nosniff"),
        ("cache-control", "no-store"),
    ] {
        headers.insert(name, HeaderValue::from_static(value));
    }
    response
}

fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find_map(|value| auth::cookie_value(value, name))
}

fn set_cookie(name: &str, value: &str, max_age: Option<u64>) -> String {
    let max_age = max_age
        .map(|secs| format!("; Max-Age={secs}"))
        .unwrap_or_default();
    format!("{name}={value}; Secure; HttpOnly; SameSite=Strict; Path=/{max_age}")
}

fn flash_cookie(message: &str) -> String {
    let mut end = message.len().min(FLASH_MAX_BYTES);
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    set_cookie(FLASH_COOKIE, &hex::encode(&message[..end]), None)
}

fn read_flash(headers: &HeaderMap) -> Option<String> {
    cookie(headers, FLASH_COOKIE)
        .and_then(|value| hex::decode(value).ok())
        .and_then(|bytes| String::from_utf8(bytes).ok())
}

fn append_cookie(response: &mut Response, value: &str) {
    if let Ok(value) = HeaderValue::from_str(value) {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
}

/// 303 See Other, with any cookies to set.
fn see_other(location: &str, cookies: &[String]) -> Response {
    let mut response = Redirect::to(location).into_response();
    for value in cookies {
        append_cookie(&mut response, value);
    }
    response
}

fn page(status: StatusCode, html: String) -> Response {
    (status, Html(html)).into_response()
}

/// The live session behind the request's cookie, with its storage key.
async fn session_of(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<Option<(String, Session)>, String> {
    let Some(token) = cookie(headers, auth::SESSION_COOKIE) else {
        return Ok(None);
    };
    let key = auth::token_sha256(token);
    let backend = state.backend.clone();
    let lookup = key.clone();
    let session = blocking(move || backend.session(&lookup)).await?;
    Ok(session
        .filter(|session| auth::session_live(session, now_unix()))
        .map(|session| (key, session)))
}

async fn dashboard(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let session = match session_of(&state, &headers).await {
        Ok(Some((_, session))) => session,
        Ok(None) => return see_other("/login", &[]),
        Err(error) => {
            return page(
                StatusCode::SERVICE_UNAVAILABLE,
                render::db_unreachable(&error),
            );
        }
    };
    let flash = read_flash(&headers);
    let backend = state.backend.clone();
    let html = match blocking(move || backend.view()).await {
        Ok(view) => render::dashboard(
            &view,
            &available_actions(&view),
            &session.csrf_token,
            flash.as_deref(),
        ),
        Err(error) => render::db_unreachable(&error),
    };
    let mut response = page(StatusCode::OK, html);
    if flash.is_some() {
        append_cookie(&mut response, &set_cookie(FLASH_COOKIE, "", Some(0)));
    }
    response
}

async fn login_form(State(state): State<AppState>) -> Response {
    let backend = state.backend.clone();
    match blocking(move || backend.password_hash()).await {
        Ok(Some(_)) => page(StatusCode::OK, render::login_page(None)),
        Ok(None) => page(StatusCode::OK, render::password_unset_page()),
        Err(error) => page(
            StatusCode::SERVICE_UNAVAILABLE,
            render::db_unreachable(&error),
        ),
    }
}

#[derive(Deserialize)]
struct LoginForm {
    password: String,
}

async fn login(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Form(form): Form<LoginForm>,
) -> Response {
    let ip = peer.ip();
    if state.limiter.blocked(ip, Instant::now()) {
        return page(
            StatusCode::TOO_MANY_REQUESTS,
            render::login_page(Some(
                "too many failed attempts; wait a minute and try again",
            )),
        );
    }
    let backend = state.backend.clone();
    let checked = blocking(move || {
        let Some(hash) = backend.password_hash()? else {
            return Ok(None);
        };
        Ok(Some(auth::verify_password(&form.password, &hash)))
    })
    .await;
    match checked {
        Err(error) => page(
            StatusCode::SERVICE_UNAVAILABLE,
            render::db_unreachable(&error),
        ),
        Ok(None) => page(StatusCode::FORBIDDEN, render::password_unset_page()),
        Ok(Some(false)) => {
            state.limiter.record_failure(ip, Instant::now());
            page(
                StatusCode::UNAUTHORIZED,
                render::login_page(Some("wrong password")),
            )
        }
        Ok(Some(true)) => {
            state.limiter.record_success(ip);
            let token = auth::random_token();
            let csrf = auth::random_token();
            let key = auth::token_sha256(&token);
            let ttl = state.session_ttl;
            let client_ip = ip.to_string();
            let backend = state.backend.clone();
            let created = blocking(move || {
                backend.create_session(&key, &csrf, ttl, &client_ip)?;
                backend.record_event("login", &format!("ip={client_ip}"))
            })
            .await;
            match created {
                Ok(()) => see_other(
                    "/",
                    &[set_cookie(
                        auth::SESSION_COOKIE,
                        &token,
                        Some(ttl.as_secs()),
                    )],
                ),
                Err(error) => page(
                    StatusCode::SERVICE_UNAVAILABLE,
                    render::db_unreachable(&error),
                ),
            }
        }
    }
}

fn forbidden() -> Response {
    page(
        StatusCode::FORBIDDEN,
        render::message_page(
            "Forbidden",
            "The form was stale or forged. Reload the dashboard and try again.",
        ),
    )
}

async fn action(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Path(name): Path<String>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let session = match session_of(&state, &headers).await {
        Ok(Some((_, session))) => session,
        Ok(None) => return see_other("/login", &[]),
        Err(error) => {
            return page(
                StatusCode::SERVICE_UNAVAILABLE,
                render::db_unreachable(&error),
            );
        }
    };
    let ip = peer.ip().to_string();
    let given = form.get("csrf").map(String::as_str).unwrap_or("");
    if !auth::csrf_matches(&session.csrf_token, given) {
        let backend = state.backend.clone();
        let detail = format!("ip={ip} action={name}");
        // Best effort: the refusal stands even if the event cannot be written.
        let _ = blocking(move || backend.record_event("csrf_rejected", &detail)).await;
        return forbidden();
    }
    let message = match parse_action(&name, &form) {
        Err(error) => error,
        Ok(web_action) => {
            let backend = state.backend.clone();
            let actor = Actor::Web { ip };
            blocking(move || backend.act(web_action, &actor))
                .await
                .unwrap_or_else(|error| error)
        }
    };
    see_other("/", &[flash_cookie(&message)])
}

async fn logout(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    let clear = set_cookie(auth::SESSION_COOKIE, "", Some(0));
    let (key, session) = match session_of(&state, &headers).await {
        Ok(Some(found)) => found,
        Ok(None) => return see_other("/login", &[clear]),
        Err(error) => {
            return page(
                StatusCode::SERVICE_UNAVAILABLE,
                render::db_unreachable(&error),
            );
        }
    };
    let given = form.get("csrf").map(String::as_str).unwrap_or("");
    if !auth::csrf_matches(&session.csrf_token, given) {
        return forbidden();
    }
    let backend = state.backend.clone();
    if let Err(error) = blocking(move || backend.delete_session(&key)).await {
        return page(
            StatusCode::SERVICE_UNAVAILABLE,
            render::db_unreachable(&error),
        );
    }
    see_other("/login", &[clear])
}

async fn css() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        render::APP_CSS,
    )
}

async fn js() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        render::APP_JS,
    )
}
