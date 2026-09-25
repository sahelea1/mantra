//! The local HTTP(S) server (design §7): the embedded SPA, a tiny login API, `/ws`, `/cert.pem`
//! and `/config.js`. Every handler is cheap and never touches the App — commands go through the
//! WebSocket into the App task.

use super::auth::{self, Authenticator, LoginError};
use super::tls::TlsMaterial;
use super::WebRegistryHandle;
use crate::app::AppEvent;
use anyhow::Result;
use axum::body::Body;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::json;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;

mod embedded {
    include!(concat!(env!("OUT_DIR"), "/web_assets.rs"));
}

/// Served when the frontend bundle has no index.html yet (a build between packages).
const PLACEHOLDER_INDEX: &str = "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\"><title>Mantra</title></head><body style=\"font-family:system-ui;background:#0F1116;color:#E2E6EC;padding:2rem\"><h1>Mantra</h1><p>The web UI bundle is not built into this binary yet. The API is up: <code>/api/session</code>, <code>/ws</code>.</p></body></html>";

struct Asset {
    bytes: &'static [u8],
    etag: String,
    ctype: &'static str,
}

fn content_type(path: &str) -> &'static str {
    match path.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).as_deref() {
        Some("html") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js") | Some("mjs") => "text/javascript; charset=utf-8",
        Some("json") => "application/json",
        Some("webmanifest") => "application/manifest+json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("woff2") => "font/woff2",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// Embedded assets by path, with a strong ETag (SHA-256 of the bytes) computed once.
fn assets() -> &'static HashMap<&'static str, Asset> {
    static A: OnceLock<HashMap<&'static str, Asset>> = OnceLock::new();
    A.get_or_init(|| {
        let mut m: HashMap<&'static str, Asset> = embedded::ASSETS.iter().map(|(p, b)| (*p, Asset { bytes: b, etag: format!("\"{}\"", &auth::sha256_hex_bytes(b)[..32]), ctype: content_type(p) })).collect();
        m.entry("index.html").or_insert_with(|| Asset { bytes: PLACEHOLDER_INDEX.as_bytes(), etag: "\"placeholder\"".into(), ctype: "text/html; charset=utf-8" });
        m
    })
}

#[derive(Clone)]
pub struct ServerCtx {
    pub auth: Arc<dyn Authenticator>,
    pub reg: WebRegistryHandle,
    pub tx: UnboundedSender<AppEvent>,
    pub tls: bool,
    pub push: bool,
    /// The CA to install on devices (only for our own self-signed setup).
    pub ca_pem: Option<String>,
}

pub struct ServerHandle {
    handle: axum_server::Handle,
}

impl ServerHandle {
    pub fn shutdown(&self) {
        self.handle.graceful_shutdown(Some(Duration::from_millis(500)));
    }
}

/// Bind now (so a busy port is a startup error, not a log line) and serve in the background.
pub fn start(addr: SocketAddr, tls: Option<TlsMaterial>, ctx: ServerCtx) -> Result<ServerHandle> {
    let listener = std::net::TcpListener::bind(addr).map_err(|e| anyhow::anyhow!("--web: cannot listen on {addr}: {e}"))?;
    listener.set_nonblocking(true)?;
    let app = router(ctx).into_make_service_with_connect_info::<SocketAddr>();
    let handle = axum_server::Handle::new();
    match tls {
        Some(m) => {
            let cfg = axum_server::tls_rustls::RustlsConfig::from_config(super::tls::server_config(&m)?);
            let server = axum_server::from_tcp_rustls(listener, cfg).handle(handle.clone());
            tokio::spawn(async move {
                if let Err(e) = server.serve(app).await {
                    crate::mlog!("web: server stopped: {e}");
                }
            });
        }
        None => {
            let server = axum_server::from_tcp(listener).handle(handle.clone());
            tokio::spawn(async move {
                if let Err(e) = server.serve(app).await {
                    crate::mlog!("web: server stopped: {e}");
                }
            });
        }
    }
    Ok(ServerHandle { handle })
}

pub fn router(ctx: ServerCtx) -> Router {
    let tls = ctx.tls;
    Router::new()
        .route("/api/session", get(session))
        .route("/api/login", post(login))
        .route("/api/logout", post(logout))
        .route("/api/health", get(|| async { Json(json!({"ok": true})) }))
        .route("/ws", get(ws))
        .route("/config.js", get(config_js))
        .route("/cert.pem", get(cert_pem))
        .fallback(static_or_spa)
        .layer(axum::middleware::from_fn(move |req: Request, next: Next| security_headers(req, next, tls)))
        .with_state(ctx)
}

async fn security_headers(req: Request, next: Next, tls: bool) -> Response {
    let mut r = next.run(req).await;
    let h = r.headers_mut();
    h.insert("x-content-type-options", HeaderValue::from_static("nosniff"));
    h.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    h.insert("x-frame-options", HeaderValue::from_static("DENY"));
    h.insert(
        "content-security-policy",
        HeaderValue::from_static("default-src 'self'; img-src 'self' data:; style-src 'self' 'unsafe-inline'; connect-src 'self' ws: wss:; manifest-src 'self'; worker-src 'self'"),
    );
    if tls {
        h.insert("strict-transport-security", HeaderValue::from_static("max-age=31536000"));
    }
    r
}

fn json_status(status: StatusCode, v: serde_json::Value) -> Response {
    (status, Json(v)).into_response()
}

/// Loopback-without-password: every loopback request is in. Otherwise a valid session cookie.
fn authenticated(ctx: &ServerCtx, peer: &SocketAddr, headers: &HeaderMap) -> bool {
    if !ctx.auth.requires_login() {
        return peer.ip().is_loopback() || peer.ip().to_canonical().is_loopback();
    }
    auth::cookie_token(headers).map(|t| ctx.auth.check(&t)).unwrap_or(false)
}

/// CSRF guard for state-changing API calls: a custom header a cross-site form can't set.
fn has_csrf_header(headers: &HeaderMap) -> bool {
    headers.get("x-mantra").and_then(|v| v.to_str().ok()) == Some("1")
}

async fn session(State(ctx): State<ServerCtx>, ConnectInfo(peer): ConnectInfo<SocketAddr>, headers: HeaderMap) -> Response {
    Json(json!({
        "authenticated": authenticated(&ctx, &peer, &headers),
        "version": env!("CARGO_PKG_VERSION"),
        "mode": "local",
        "tls": ctx.tls,
        "push": ctx.push,
        "loopback": peer.ip().is_loopback(),
        "password": ctx.auth.requires_login(),
    }))
    .into_response()
}

#[derive(serde::Deserialize)]
struct LoginBody {
    password: String,
}

async fn login(State(ctx): State<ServerCtx>, ConnectInfo(peer): ConnectInfo<SocketAddr>, headers: HeaderMap, body: axum::body::Bytes) -> Response {
    if !has_csrf_header(&headers) {
        return json_status(StatusCode::FORBIDDEN, json!({"error": "missing X-Mantra header"}));
    }
    let Ok(b) = serde_json::from_slice::<LoginBody>(&body) else {
        return json_status(StatusCode::BAD_REQUEST, json!({"error": "expected {\"password\": …}"}));
    };
    let ua = headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    match ctx.auth.login(&b.password, peer.ip().to_canonical(), &ua) {
        Ok(tok) => {
            let a = ctx.auth.clone();
            let _ = tokio::task::spawn_blocking(move || a.persist()).await;
            let mut r = json_status(StatusCode::OK, json!({"ok": true}));
            if let Ok(v) = HeaderValue::from_str(&auth::set_cookie(&tok.0, ctx.tls)) {
                r.headers_mut().insert(header::SET_COOKIE, v);
            }
            r
        }
        Err(LoginError::NoPassword) => json_status(StatusCode::NOT_FOUND, json!({"error": "no password is configured (localhost needs no login)"})),
        Err(LoginError::Wrong) => json_status(StatusCode::UNAUTHORIZED, json!({"error": "wrong password"})),
        Err(LoginError::Locked(secs)) => {
            let mut r = json_status(StatusCode::TOO_MANY_REQUESTS, json!({"error": format!("too many attempts, try again in {secs} s"), "retry_after": secs}));
            if let Ok(v) = HeaderValue::from_str(&secs.to_string()) {
                r.headers_mut().insert(header::RETRY_AFTER, v);
            }
            r
        }
    }
}

async fn logout(State(ctx): State<ServerCtx>, headers: HeaderMap) -> Response {
    if !has_csrf_header(&headers) {
        return json_status(StatusCode::FORBIDDEN, json!({"error": "missing X-Mantra header"}));
    }
    if let Some(t) = auth::cookie_token(&headers) {
        ctx.auth.logout(&t);
        let a = ctx.auth.clone();
        let _ = tokio::task::spawn_blocking(move || a.persist()).await;
    }
    let mut r = json_status(StatusCode::OK, json!({"ok": true}));
    if let Ok(v) = HeaderValue::from_str(&auth::clear_cookie(ctx.tls)) {
        r.headers_mut().insert(header::SET_COOKIE, v);
    }
    r
}

/// Browsers always send `Origin` on a WebSocket handshake: it must be this very server (no other
/// site may drive the session through the user's cookie). Non-browser clients send none.
fn origin_ok(headers: &HeaderMap, tls: bool) -> bool {
    let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) else { return true };
    let Some(host) = headers.get(header::HOST).and_then(|v| v.to_str().ok()) else { return false };
    let scheme = if tls { "https" } else { "http" };
    origin.eq_ignore_ascii_case(&format!("{scheme}://{host}"))
}

async fn ws(State(ctx): State<ServerCtx>, ConnectInfo(peer): ConnectInfo<SocketAddr>, headers: HeaderMap, upgrade: WebSocketUpgrade) -> Response {
    if !origin_ok(&headers, ctx.tls) {
        return json_status(StatusCode::FORBIDDEN, json!({"error": "cross-origin WebSocket refused"}));
    }
    if !authenticated(&ctx, &peer, &headers) {
        return json_status(StatusCode::UNAUTHORIZED, json!({"error": "log in first"}));
    }
    let (reg, tx) = (ctx.reg.clone(), ctx.tx.clone());
    upgrade.max_frame_size(1 << 20).max_message_size(1 << 20).on_upgrade(move |socket| super::conn::run_local(socket, reg, tx))
}

async fn config_js(State(ctx): State<ServerCtx>) -> Response {
    let body = format!("window.__MANTRA__={};\n", json!({"mode": "local", "protocol": super::protocol::PROTOCOL, "version": env!("CARGO_PKG_VERSION"), "tls": ctx.tls, "push": ctx.push}));
    ([(header::CONTENT_TYPE, "text/javascript; charset=utf-8"), (header::CACHE_CONTROL, "no-cache")], body).into_response()
}

async fn cert_pem(State(ctx): State<ServerCtx>) -> Response {
    match ctx.ca_pem {
        Some(pem) => ([(header::CONTENT_TYPE, "application/x-x509-ca-cert"), (header::CONTENT_DISPOSITION, "inline; filename=\"mantra-ca.pem\"")], pem).into_response(),
        None => json_status(StatusCode::NOT_FOUND, json!({"error": "not serving a Mantra-made certificate"})),
    }
}

/// SPA routes (the client router owns them) — all get index.html.
fn is_spa_route(path: &str) -> bool {
    matches!(path, "/" | "/settings" | "/run" | "/pulse" | "/runs" | "/inbox" | "/login" | "/connect") || path.starts_with("/s/") || path.starts_with("/agent/")
}

fn serve_asset(a: &Asset, headers: &HeaderMap, path: &str) -> Response {
    if headers.get(header::IF_NONE_MATCH).and_then(|v| v.to_str().ok()).map(|v| v.split(',').any(|t| t.trim() == a.etag)).unwrap_or(false) {
        return Response::builder().status(StatusCode::NOT_MODIFIED).header(header::ETAG, &a.etag).body(Body::empty()).unwrap_or_default();
    }
    let mut b = Response::builder().status(StatusCode::OK).header(header::CONTENT_TYPE, a.ctype).header(header::CACHE_CONTROL, "no-cache").header(header::ETAG, &a.etag);
    if path == "sw.js" {
        b = b.header("service-worker-allowed", "/");
    }
    b.body(Body::from(a.bytes)).unwrap_or_default()
}

async fn static_or_spa(method: Method, headers: HeaderMap, uri: axum::http::Uri) -> Response {
    if method != Method::GET && method != Method::HEAD {
        return json_status(StatusCode::NOT_FOUND, json!({"error": "not found"}));
    }
    let path = uri.path();
    let rel = path.trim_start_matches('/');
    let rel = rel.strip_prefix("assets/").unwrap_or(rel);
    let all = assets();
    if !rel.is_empty() && !rel.contains("..") {
        if let Some(a) = all.get(rel) {
            return serve_asset(a, &headers, rel);
        }
    }
    if is_spa_route(path) {
        if let Some(a) = all.get("index.html") {
            return serve_asset(a, &headers, "index.html");
        }
    }
    json_status(StatusCode::NOT_FOUND, json!({"error": "not found"}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origins_must_match_the_host() {
        let mut h = HeaderMap::new();
        assert!(origin_ok(&h, false), "no Origin: a non-browser client");
        h.insert(header::HOST, "192.168.1.5:7777".parse().unwrap());
        h.insert(header::ORIGIN, "http://192.168.1.5:7777".parse().unwrap());
        assert!(origin_ok(&h, false));
        assert!(!origin_ok(&h, true), "scheme matters");
        h.insert(header::ORIGIN, "http://evil.example".parse().unwrap());
        assert!(!origin_ok(&h, false));
    }

    #[test]
    fn spa_routes_and_assets() {
        assert!(is_spa_route("/agent/3") && is_spa_route("/s/abc") && is_spa_route("/"));
        assert!(!is_spa_route("/api/x") && !is_spa_route("/nope"));
        assert!(assets().contains_key("index.html"), "always an index, placeholder at worst");
        assert_eq!(content_type("sw.js"), "text/javascript; charset=utf-8");
        assert_eq!(content_type("manifest.webmanifest"), "application/manifest+json");
    }
}
