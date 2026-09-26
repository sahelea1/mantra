//! `mantra --remote`: reach this session through a relay, end-to-end encrypted (design §10).
//!
//! Package A owns the identity half — sid, host token, password, key, link, code and QR,
//! persisted in `web/remote.json` so a link survives restarts until it is rotated — and the relay
//! connection: dial out over IPv4, demux the relay's clients, run the per-client E2EE handshake
//! and hand each client to the same conn registry local WebSockets use, reconnect with backoff.
//! Nothing here runs unless `--remote` was given (`Web::start` only builds a `Remote` then).

use super::snapshot::RemoteInfo;
use super::{crypto, qr, ConnId, ConnKind, Outbound, WebRegistryHandle};
use crate::app::AppEvent;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;

#[derive(Clone, Debug)]
pub struct RemoteConfig {
    /// `wss://remote.mantra.codes` (or any ws:// / wss:// relay).
    pub relay: String,
    /// The website links open (`https://remote.mantra.codes`); it may be on another host than the relay.
    pub site: String,
    /// The configured web password; `None` → a generated one (kept in remote.json).
    pub password: Option<String>,
    /// `$MANTRA_HOME/web`.
    pub dir: PathBuf,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct Identity {
    sid: String,
    host_token: String,
    created_unix: u64,
    /// Only a *generated* password is stored; a configured one stays where the user put it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    password: Option<String>,
}

impl Identity {
    fn fresh(generated: Option<String>) -> Identity {
        Identity { sid: crypto::b32_encode(&crypto::random_bytes::<16>()), host_token: crypto::b64url_encode(&crypto::random_bytes::<32>()), created_unix: crate::util::unix_secs(), password: generated }
    }
}

/// Live state, shared between the handle (TUI, commands, publisher) and the relay task.
pub struct State {
    pub sid: String,
    pub host_token: String,
    pub password: String,
    pub generated: bool,
    /// The PBKDF2 key; `None` while it is being derived (startup, rotate).
    pub key: Option<[u8; 32]>,
    pub connected: bool,
    pub clients: u32,
    pub last_error: Option<String>,
    qr: Option<(String, Vec<String>)>,
}

pub struct Inner {
    pub relay: String,
    pub site: String,
    pub web: WebRegistryHandle,
    pub tx: UnboundedSender<AppEvent>,
    dir: PathBuf,
    fixed_password: Option<String>,
    pub state: Mutex<State>,
    /// Bumped whenever the identity changes (rotate) or the key becomes ready: the relay task
    /// (re)connects with the current identity.
    pub generation: tokio::sync::watch::Sender<u64>,
}

impl Inner {
    pub fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// (sid, host_token, key) once the key is ready.
    pub fn identity(&self) -> Option<(String, String, [u8; 32])> {
        let s = self.lock();
        Some((s.sid.clone(), s.host_token.clone(), s.key?))
    }

    /// Update what `/remote` and the web UI show; publishes when anything changed.
    pub fn set_status(&self, connected: bool, clients: u32, last_error: Option<String>) {
        let mut s = self.lock();
        let changed = s.connected != connected || s.clients != clients || s.last_error != last_error;
        s.connected = connected;
        s.clients = clients;
        s.last_error = last_error;
        drop(s);
        if changed {
            self.web.refresh();
        }
    }

    fn persist(&self) {
        let s = self.lock();
        let id = Identity { sid: s.sid.clone(), host_token: s.host_token.clone(), created_unix: crate::util::unix_secs(), password: s.generated.then(|| s.password.clone()) };
        drop(s);
        let body = serde_json::to_string_pretty(&id).unwrap_or_default();
        if let Err(e) = crate::config::atomic_write_restricted(&self.dir.join("remote.json"), &body) {
            crate::mlog!("remote: cannot save remote.json: {e}");
        }
    }

    /// Derive the key for the current sid off the async threads; publish it only if the sid is
    /// still the same (a rotate may have happened meanwhile).
    fn derive_key(self: &Arc<Self>) {
        let me = self.clone();
        let (sid, pw) = {
            let s = self.lock();
            (s.sid.clone(), s.password.clone())
        };
        tokio::task::spawn_blocking(move || {
            let key = crypto::derive_key(&pw, &sid);
            let mut s = me.lock();
            if s.sid == sid {
                s.key = Some(key);
                drop(s);
                me.generation.send_modify(|g| *g = g.wrapping_add(1));
                me.web.refresh();
            }
        });
    }
}

/// `https://` origin of the relay's website from its `wss://` URL.
pub fn relay_origin(relay: &str) -> String {
    let r = relay.trim_end_matches('/');
    if let Some(h) = r.strip_prefix("wss://") {
        format!("https://{h}")
    } else if let Some(h) = r.strip_prefix("ws://") {
        format!("http://{h}")
    } else {
        r.to_string()
    }
}

/// The sid as typed by people: `xxxx-xxxx-xxxx-xxxx-xxxx-xxxxxx`.
pub fn code_of(sid: &str) -> String {
    let c: Vec<char> = sid.chars().collect();
    let mut parts = vec![];
    let mut i = 0;
    for n in [4, 4, 4, 4, 4] {
        if i >= c.len() {
            break;
        }
        parts.push(c[i..(i + n).min(c.len())].iter().collect::<String>());
        i += n;
    }
    if i < c.len() {
        parts.push(c[i..].iter().collect());
    }
    parts.join("-")
}

/// The link a browser opens: the site's page for this sid, the key in the fragment (never sent
/// to any server). When the site is not on the relay's host the fragment also names the relay
/// (`&r=`), so moving the relay elsewhere needs no change to the site.
pub fn link_of(site: &str, relay: &str, sid: &str, key: &[u8; 32]) -> String {
    let site = site.trim_end_matches('/');
    let relay = relay.trim_end_matches('/');
    let r = if relay_origin(relay) == site { String::new() } else { format!("&r={}", fragment_value(relay)) };
    format!("{site}/s/{sid}#k={}{r}", crypto::b64url_encode(key))
}

/// Percent-encode a value for the link's fragment. The page reads it with `URLSearchParams`,
/// which splits on `&` and `=` and turns `+` into a space — so everything but unreserved
/// characters and the `:`/`/` every relay URL has is encoded (a relay behind a proxy may well
/// carry `?a=b&c` or `#`).
fn fragment_value(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    for b in v.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b':' | b'/') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// A plain `ws://` relay on another machine: the host can use it, but a browser on an https page
/// may not (mixed content), so shared links would never connect. Loopback is the local-testing case.
pub fn relay_without_tls(relay: &str) -> bool {
    let Some(rest) = relay.trim().strip_prefix("ws://") else { return false };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    let host = match authority.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(""),
        None => authority.rsplit_once(':').map(|(h, _)| h).unwrap_or(authority),
    };
    let loopback = host.eq_ignore_ascii_case("localhost") || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback());
    !loopback
}

pub struct Remote {
    inner: Arc<Inner>,
    task: Mutex<Option<JoinHandle<()>>>,
}

impl Remote {
    pub fn start(cfg: RemoteConfig, web: WebRegistryHandle, tx: UnboundedSender<AppEvent>) -> Remote {
        let path = cfg.dir.join("remote.json");
        let saved: Option<Identity> = std::fs::read_to_string(&path).ok().and_then(|s| serde_json::from_str(&s).ok()).filter(|i: &Identity| crypto::b32_decode(&i.sid).map(|b| b.len() == 16).unwrap_or(false) && !i.host_token.is_empty());
        let id = saved.unwrap_or_else(|| Identity::fresh(None));
        let (password, generated) = match (&cfg.password, &id.password) {
            (Some(p), _) => (p.clone(), false),
            (None, Some(p)) if !p.is_empty() => (p.clone(), true),
            (None, _) => {
                crate::mlog!("remote: generated a password (see /remote)");
                (crypto::gen_password(), true)
            }
        };
        let (gen_tx, _) = tokio::sync::watch::channel(0u64);
        let inner = Arc::new(Inner {
            relay: cfg.relay.clone(),
            site: cfg.site.clone(),
            web,
            tx,
            dir: cfg.dir.clone(),
            fixed_password: cfg.password.clone(),
            state: Mutex::new(State { sid: id.sid, host_token: id.host_token, password, generated, key: None, connected: false, clients: 0, last_error: None, qr: None }),
            generation: gen_tx,
        });
        inner.persist();
        inner.derive_key();
        let task = tokio::spawn(run(inner.clone()));
        crate::mlog!("remote: relay {}", cfg.relay);
        Remote { inner, task: Mutex::new(Some(task)) }
    }

    pub fn inner(&self) -> &Arc<Inner> {
        &self.inner
    }

    /// Everything the `/remote` overlay and the web UI show. `link`/`qr` are absent until the key
    /// has been derived (a fraction of a second after start or rotate).
    pub fn info(&self) -> RemoteInfo {
        let mut s = self.inner.lock();
        let link = s.key.map(|k| link_of(&self.inner.site, &self.inner.relay, &s.sid, &k));
        let qr = match (&link, &s.qr) {
            (Some(l), Some((ql, rows))) if l == ql => Some(rows.clone()),
            (Some(l), _) => {
                let rows = qr::matrix(l).map(|m| qr::rows01(&m));
                if let Some(r) = &rows {
                    s.qr = Some((l.clone(), r.clone()));
                }
                rows
            }
            (None, _) => None,
        };
        RemoteInfo {
            enabled: true,
            relay: self.inner.relay.clone(),
            connected: s.connected,
            sid: Some(s.sid.clone()),
            code: Some(code_of(&s.sid)),
            link,
            password: Some(s.password.clone()),
            qr,
            clients: s.clients,
            last_error: s.last_error.clone(),
        }
    }

    /// New sid + host token (+ a new password when it was generated); the old link, code and QR
    /// stop working, connected relay clients are dropped by the reconnect.
    pub fn rotate(&self) {
        {
            let mut s = self.inner.lock();
            let fresh = Identity::fresh(None);
            s.sid = fresh.sid;
            s.host_token = fresh.host_token;
            if self.inner.fixed_password.is_none() {
                s.password = crypto::gen_password();
            }
            s.key = None;
            s.qr = None;
            s.connected = false;
            s.clients = 0;
        }
        self.inner.persist();
        self.inner.derive_key();
        self.inner.generation.send_modify(|g| *g = g.wrapping_add(1));
        self.inner.web.refresh();
        crate::mlog!("remote: identity rotated");
    }

    pub fn shutdown(&self) {
        if let Some(t) = self.task.lock().unwrap_or_else(|e| e.into_inner()).take() {
            t.abort();
        }
    }

    /// Wait (up to `max`) until the key is derived, so the link can be printed.
    pub async fn ready(&self, max: std::time::Duration) -> bool {
        let end = tokio::time::Instant::now() + max;
        while tokio::time::Instant::now() < end {
            if self.inner.lock().key.is_some() {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        false
    }
}

// ───────────────────────────── the relay connection (§10.2–10.4) ─────────────────────────────

/// First retry delay after a failed or dropped relay connection; doubles up to `BACKOFF_MAX`.
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(60);
/// A connection that stayed up this long counts as healthy: the next failure retries quickly again.
const HEALTHY_AFTER: Duration = Duration::from_secs(10);
/// TCP + TLS + upgrade must finish within this.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// How often idle clients are checked (hello deadline, E2EE keep-alive).
const SWEEP_EVERY: Duration = Duration::from_secs(5);
/// Failed handshakes per client IP (§10.3): this many in `STRIKE_WINDOW` → ignore that IP for `BLOCK_FOR`.
const STRIKE_LIMIT: usize = 10;
const STRIKE_WINDOW: Duration = Duration::from_secs(600);
const BLOCK_FOR: Duration = Duration::from_secs(600);
/// The relay's answer to a new sid from an IPv4 address that already hosts one (one hosting
/// session per address, `--max-hosts-per-ip`). Only matched when an older relay sends no
/// `X-Mantra-Relay-Error` header.
const RELAY_IP_TAKEN: &str = "this address already hosts a session";
/// Clients tracked per relay connection — the relay's own per-session cap (16), so a leaked sid
/// can't make the host allocate a conn and a task per `open` without limit.
const MAX_CLIENTS: usize = 16;
/// Of those, at most this many may still be short of the protocol hello (not yet proven to know
/// the key); the rest of the room stays for devices that did.
const MAX_UNHELLOED: usize = 8;
/// After a relay reconnect the relay replays `open` for clients still attached to it; a client
/// not replayed within this long is gone.
const REPLAY_WAIT: Duration = Duration::from_secs(5);
/// While the relay connection is down clients are kept for a reconnect at most this long: the
/// relay's idle timeout, after which it has closed them (4410) for certain.
const MAX_ADRIFT: Duration = Duration::from_secs(90);
/// A refused client gets its plaintext refusal first and the close only after this: the relay
/// may deliver a close ahead of frames still queued for that client, which would swallow the
/// refusal. The browser hangs up by itself as soon as it reads it.
const REFUSE_GRACE: Duration = Duration::from_secs(1);
/// Refused clients awaiting that close; beyond this many they are closed at once.
const MAX_REFUSED: usize = 64;

type Cid = [u8; 16];
type RelayWs = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// The relay task: connect with the current identity, serve its clients until the connection
/// drops, back off, repeat. A rotate (or the key becoming ready) reconnects immediately.
async fn run(inner: Arc<Inner>) {
    let mut generation = inner.generation.subscribe();
    let mut backoff = BACKOFF_MIN;
    let mut strikes = Strikes::default();
    // Outlives single relay connections: the relay replays `open` for browsers still attached
    // after a host reconnect, and those browsers keep their E2EE session (they never redo the
    // hello), so their cipher state must survive here too.
    let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut clients = Clients::new(inner.web.clone(), inner.tx.clone(), [0u8; 32], out_tx);
    loop {
        generation.borrow_and_update();
        let Some((sid, token, key)) = inner.identity() else {
            // The key is still being derived; `derive_key` bumps the generation once it is ready.
            if generation.changed().await.is_err() {
                return;
            }
            continue;
        };
        // A rotated identity has a new key: nobody from the old one can follow.
        clients.rekey(key);
        // `session` itself watches `generation` (via the same `&mut` receiver) so a rotate mid-session
        // can tell attached clients before their connection drops — see the `rotated` arm there.
        let ended = session(&inner, &sid, &token, &mut clients, &mut out_rx, &mut strikes, &mut generation).await;
        clients.detach(Instant::now());
        // Bounded even while the relay stays unreachable: clients nobody can replay any more go.
        clients.prune(Instant::now());
        if ended.healthy || ended.rotated {
            backoff = BACKOFF_MIN;
        }
        if ended.rotated {
            // The clients already got their notice inside `session`; dial the new identity now,
            // same as the old immediate-reconnect path.
            crate::mlog!("remote: {}", ended.error);
            inner.set_status(false, 0, Some(ended.error));
            continue;
        }
        let wait = jittered(backoff);
        crate::mlog!("remote: {} — retrying in {:.1}s", ended.error, wait.as_secs_f32());
        inner.set_status(false, 0, Some(ended.error));
        backoff = backoff.saturating_mul(2).min(BACKOFF_MAX);
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            c = generation.changed() => {
                if c.is_err() {
                    return;
                }
                backoff = BACKOFF_MIN;
            }
        }
    }
}

/// ±25 % so many hosts behind one relay restart don't all come back in the same instant.
fn jittered(d: Duration) -> Duration {
    d.mul_f64(0.75 + rand::random::<f64>() * 0.5)
}

struct Ended {
    /// It was connected long enough that the next retry starts from the short delay again.
    healthy: bool,
    /// A rotate ended this session (clients were already told, §10.3/§10.4): reconnect at once,
    /// same as `healthy`, but skip the normal retry wait too (see `run`).
    rotated: bool,
    /// Human-readable, shown in `/remote` and the web UI's settings.
    error: String,
}

impl Ended {
    fn fail(error: String) -> Ended {
        Ended { healthy: false, rotated: false, error }
    }
}

/// One relay connection, start to end.
#[allow(clippy::too_many_arguments)]
async fn session(
    inner: &Arc<Inner>,
    sid: &str,
    token: &str,
    clients: &mut Clients,
    out_rx: &mut tokio::sync::mpsc::UnboundedReceiver<(Cid, Outbound)>,
    strikes: &mut Strikes,
    generation: &mut tokio::sync::watch::Receiver<u64>,
) -> Ended {
    let ws = match dial(&inner.relay, sid, token).await {
        Ok(ws) => ws,
        Err(error) => return Ended::fail(error),
    };
    crate::mlog!("remote: connected to the relay");
    let since = Instant::now();
    clients.attach(since);
    inner.set_status(true, clients.ready(), None);
    let (mut sink, mut stream) = ws.split();
    let mut sweep = tokio::time::interval(SWEEP_EVERY);
    sweep.tick().await;
    let mut last_ping = Instant::now();
    let mut relay_missed = 0u8;
    let mut rotated = false;
    let error = 'conn: loop {
        let frames: Vec<Frame> = tokio::select! {
            c = generation.changed() => {
                if c.is_err() {
                    break 'conn "shutting down".into();
                }
                // Nothing else bumps the generation while a session is up: this is a rotate.
                // Tell every client that finished its hello — sealed with the key it still knows
                // — before `run`'s `rekey` forgets them and dials the new identity (§10.3/§10.4).
                rotated = true;
                clients.rotate_notice()
            }
            m = stream.next() => match m {
                Some(Ok(Message::Binary(b))) => clients.binary(&b, strikes, Instant::now()),
                Some(Ok(Message::Text(t))) => clients.control(t.as_str(), strikes, Instant::now()),
                Some(Ok(Message::Pong(_))) => {
                    relay_missed = 0;
                    vec![]
                }
                Some(Ok(Message::Ping(_))) | Some(Ok(Message::Frame(_))) => vec![],
                Some(Ok(Message::Close(c))) => {
                    let why = c.map(|c| sanitize(c.reason.as_str())).filter(|r| !r.is_empty());
                    break 'conn match why {
                        // The relay hands the sid to the newest host with the same token: another
                        // Mantra with this identity (a copied MANTRA_HOME) just connected.
                        Some(r) if r.contains("replaced") || r.contains("superseded") => format!("another Mantra with this identity took over the relay connection ({r})"),
                        Some(r) => format!("the relay closed the connection ({r})"),
                        None => "the relay closed the connection".into(),
                    };
                }
                Some(Err(e)) => break 'conn format!("lost the relay connection: {e}"),
                None => break 'conn "lost the relay connection".into(),
            },
            o = out_rx.recv() => match o {
                Some((cid, out)) => clients.outbound(cid, out),
                None => vec![],
            },
            _ = sweep.tick() => {
                let now = Instant::now();
                if now.duration_since(last_ping) >= super::conn::PING_EVERY {
                    last_ping = now;
                    // The relay drops idle hosts after 90 s; a ping every 25 s keeps us visible,
                    // and two unanswered ones mean the path is dead even if TCP hasn't noticed.
                    if relay_missed >= 2 {
                        break 'conn "the relay stopped answering".into();
                    }
                    relay_missed += 1;
                    if sink.send(Message::Ping(Default::default())).await.is_err() {
                        break 'conn "lost the relay connection".into();
                    }
                    clients.sweep(now, true)
                } else {
                    clients.sweep(now, false)
                }
            }
        };
        for f in frames {
            let m = match f {
                Frame::Bin(b) => Message::Binary(b.into()),
                Frame::Text(t) => Message::Text(t.into()),
            };
            if let Err(e) = sink.send(m).await {
                break 'conn format!("lost the relay connection: {e}");
            }
        }
        if rotated {
            break 'conn "identity rotated".into();
        }
        inner.set_status(true, clients.ready(), None);
    };
    let _ = sink.close().await;
    // `clients` stay (the caller detaches them): a quick reconnect gets them replayed by the relay.
    Ended { healthy: since.elapsed() >= HEALTHY_AFTER, rotated, error }
}

/// Dial `{relay}/host/{sid}` over IPv4 (the relay refuses hosts on IPv6) with the host token.
async fn dial(relay: &str, sid: &str, token: &str) -> Result<RelayWs, String> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let target = RelayTarget::parse(relay)?;
    let url = format!("{}/host/{sid}", relay.trim_end_matches('/'));
    let mut req = url.as_str().into_client_request().map_err(|e| format!("bad relay URL {relay}: {e}"))?;
    let auth = format!("Bearer {token}").parse().map_err(|_| "bad host token".to_string())?;
    req.headers_mut().insert("Authorization", auth);
    let connect = async {
        let tcp = connect_ipv4(&target.host, target.port).await?;
        let connector = if target.tls { tokio_tungstenite::Connector::Rustls(client_tls()?) } else { tokio_tungstenite::Connector::Plain };
        tokio_tungstenite::client_async_tls_with_config(req, tcp, None, Some(connector)).await.map(|(ws, _)| ws).map_err(describe)
    };
    match tokio::time::timeout(CONNECT_TIMEOUT, connect).await {
        Ok(r) => r,
        Err(_) => Err(format!("the relay {} did not answer within {} s", target.host, CONNECT_TIMEOUT.as_secs())),
    }
}

#[derive(Debug, PartialEq)]
struct RelayTarget {
    host: String,
    port: u16,
    tls: bool,
}

impl RelayTarget {
    fn parse(relay: &str) -> Result<RelayTarget, String> {
        let (tls, rest) = if let Some(r) = relay.strip_prefix("wss://") {
            (true, r)
        } else if let Some(r) = relay.strip_prefix("ws://") {
            (false, r)
        } else {
            return Err(format!("the relay URL must start with wss:// or ws:// (got {relay})"));
        };
        let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
        let authority = authority.rsplit('@').next().unwrap_or(authority);
        if authority.starts_with('[') {
            return Err("the relay must be reachable over IPv4 (an IPv6 address was given)".into());
        }
        let (host, port) = match authority.rsplit_once(':') {
            Some((h, p)) => (h, p.parse::<u16>().map_err(|_| format!("bad port in relay URL {relay}"))?),
            None => (authority, if tls { 443 } else { 80 }),
        };
        if host.is_empty() {
            return Err(format!("no host in relay URL {relay}"));
        }
        Ok(RelayTarget { host: host.to_string(), port, tls })
    }
}

/// Hosts must reach the relay over IPv4 (it answers IPv6 hosts with 403), so only A records are
/// tried, in order.
async fn connect_ipv4(host: &str, port: u16) -> Result<tokio::net::TcpStream, String> {
    let addrs: Vec<std::net::SocketAddr> = tokio::net::lookup_host((host, port)).await.map_err(|e| format!("cannot resolve the relay {host}: {e}"))?.filter(|a| a.is_ipv4()).collect();
    if addrs.is_empty() {
        return Err(format!("the relay {host} has no IPv4 address (hosts must connect over IPv4)"));
    }
    let mut last = String::new();
    for a in addrs {
        match tokio::net::TcpStream::connect(a).await {
            Ok(s) => {
                let _ = s.set_nodelay(true);
                return Ok(s);
            }
            Err(e) => last = e.to_string(),
        }
    }
    Err(format!("cannot reach the relay {host}: {last}"))
}

fn client_tls() -> Result<Arc<rustls::ClientConfig>, String> {
    let roots = rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let cfg = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("tls: {e}"))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Arc::new(cfg))
}

/// A failed upgrade, in words a person can act on. The response body is the relay's (untrusted).
fn describe(e: tokio_tungstenite::tungstenite::Error) -> String {
    use tokio_tungstenite::tungstenite::Error;
    match e {
        Error::Http(resp) => {
            let body = resp.body().as_deref().map(|b| String::from_utf8_lossy(&b[..b.len().min(300)]).to_string()).unwrap_or_default();
            let code = resp.headers().get("x-mantra-relay-error").and_then(|v| v.to_str().ok());
            refusal(resp.status().as_u16(), code, &body)
        }
        other => format!("cannot reach the relay: {other}"),
    }
}

/// The relay's refusal of the host upgrade. `code` is its `X-Mantra-Relay-Error` header (the
/// relay README's table) and wins; without it (an older relay) the status and body decide.
fn refusal(status: u16, code: Option<&str>, body: &str) -> String {
    const IP_TAKEN: &str = "another Mantra session is already hosted from this network address — the relay allows one per IPv4 address; this one connects once that session ends";
    const BAD_TOKEN: &str = "the relay has this session bound to a different host token (another Mantra with the same session?) — it frees up 10 minutes after that host leaves, or rotate the link in /remote";
    const IPV6: &str = "the relay only accepts hosts over IPv4, and this connection reached it over IPv6 (through a proxy or VPN?)";
    const RATE: &str = "the relay is rate-limiting this address (too many connections a minute)";
    const BAD_SID: &str = "the relay does not know this session address — check the --remote URL (a path in front of /host?)";
    const FULL: &str = "the relay is full right now (too many sessions)";
    match code.map(str::trim) {
        Some("ip-taken") => return IP_TAKEN.into(),
        Some("bad-token") => return BAD_TOKEN.into(),
        Some("ipv6") => return IPV6.into(),
        Some("rate-limited") => return RATE.into(),
        Some("bad-sid") => return BAD_SID.into(),
        Some("full") => return FULL.into(),
        // Unknown (newer) code: fall back to what the status says.
        _ => {}
    }
    let body = sanitize(body);
    match status {
        409 if body.contains(RELAY_IP_TAKEN) => IP_TAKEN.into(),
        409 => BAD_TOKEN.into(),
        403 => format!("the relay refused this host{}", if body.is_empty() { String::new() } else { format!(": {body}") }),
        429 => RATE.into(),
        503 => FULL.into(),
        s => format!("the relay refused the connection (HTTP {s}{})", if body.is_empty() { String::new() } else { format!(": {body}") }),
    }
}

/// Untrusted text from the relay, safe to show: one line, printable, short.
fn sanitize(s: &str) -> String {
    let t: String = s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    crate::util::trunc(t.trim(), 160).to_string()
}

fn hex(cid: &Cid) -> String {
    cid.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<Cid> {
    if s.len() != 32 || !s.is_ascii() {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(out)
}

/// A frame for the relay.
#[derive(Debug, PartialEq)]
enum Frame {
    Bin(Vec<u8>),
    Text(String),
}

fn bin(cid: &Cid, payload: &[u8]) -> Frame {
    let mut f = Vec::with_capacity(16 + payload.len());
    f.extend_from_slice(cid);
    f.extend_from_slice(payload);
    Frame::Bin(f)
}

fn close_frame(cid: &Cid) -> Frame {
    Frame::Text(serde_json::json!({"t": "close", "c": hex(cid)}).to_string())
}

/// Failed handshakes per client IP (§10.3): 10 in 10 minutes → that IP's `open`s are ignored for
/// 10 minutes. Survives relay reconnects; bounded so a flood of addresses can't grow it forever.
/// Clients the relay reports without an address share one bucket rather than going unthrottled.
#[derive(Default)]
struct Strikes {
    hits: HashMap<String, VecDeque<Instant>>,
    blocked: HashMap<String, Instant>,
}

impl Strikes {
    fn is_blocked(&mut self, ip: &str, now: Instant) -> bool {
        let ip = bucket(ip);
        match self.blocked.get(ip) {
            Some(until) if *until > now => true,
            Some(_) => {
                self.blocked.remove(ip);
                false
            }
            None => false,
        }
    }

    fn strike(&mut self, ip: &str, now: Instant) {
        let ip = bucket(ip);
        if self.hits.len() >= 4096 && !self.hits.contains_key(ip) {
            self.hits.retain(|_, q| q.back().is_some_and(|t| now.duration_since(*t) < STRIKE_WINDOW));
            self.blocked.retain(|_, until| *until > now);
            if self.hits.len() >= 4096 {
                return;
            }
        }
        let q = self.hits.entry(ip.to_string()).or_default();
        q.push_back(now);
        while q.front().is_some_and(|t| now.duration_since(*t) >= STRIKE_WINDOW) {
            q.pop_front();
        }
        if q.len() >= STRIKE_LIMIT {
            self.hits.remove(ip);
            self.blocked.insert(ip.to_string(), now + BLOCK_FOR);
            crate::mlog!("remote: ignoring {ip} for 10 minutes after {STRIKE_LIMIT} failed handshakes");
        }
    }
}

/// The strikes key for a relay-reported IP; a missing one is not a free pass.
fn bucket(ip: &str) -> &str {
    if ip.is_empty() {
        "unknown"
    } else {
        ip
    }
}

/// One browser behind the relay.
struct Client {
    conn: ConnId,
    ip: String,
    /// `None` until the §10.3 hello exchange; afterwards every frame is sealed/opened with it.
    cipher: Option<crypto::Cipher>,
    /// The protocol hello (`{"t":"hello"}`, inside the E2EE channel) arrived.
    hello_seen: bool,
    opened: Instant,
    last_ping: Instant,
    missed: u8,
    /// Set while the relay connection it came through is gone: forgotten at this instant unless
    /// the relay replays its `open` on the next connection first.
    adrift: Option<Instant>,
    /// Moves this client's outbound queue into the relay task's channel.
    fwd: JoinHandle<()>,
}

/// Every client behind the relay, across reconnects. Pure bookkeeping — frames in, frames out —
/// so it is testable without a network; `session` owns the socket.
struct Clients {
    web: WebRegistryHandle,
    tx: UnboundedSender<AppEvent>,
    key: [u8; 32],
    out: UnboundedSender<(Cid, Outbound)>,
    map: HashMap<Cid, Client>,
    /// Refused (already forgotten) clients and when to close them if they haven't left.
    refused: Vec<(Cid, Instant)>,
}

impl Clients {
    fn new(web: WebRegistryHandle, tx: UnboundedSender<AppEvent>, key: [u8; 32], out: UnboundedSender<(Cid, Outbound)>) -> Clients {
        Clients { web, tx, key, out, map: HashMap::new(), refused: vec![] }
    }

    /// Clients past the hello (what `/remote` shows as connected).
    fn ready(&self) -> u32 {
        self.map.values().filter(|c| c.hello_seen && c.adrift.is_none()).count().min(u32::MAX as usize) as u32
    }

    /// Every client that finished its hello, told the link was rotated — sealed with the key it
    /// still knows, so it reads before `rekey` drops it and the relay connection closes (§10.3,
    /// §10.4). Clients still mid-handshake get nothing (they have no cipher to seal with yet);
    /// they simply vanish, same as any other drop.
    fn rotate_notice(&mut self) -> Vec<Frame> {
        let msg = super::protocol::ServerMsg::Bye { reason: "rotated".into() }.to_json();
        let mut out = vec![];
        for (cid, c) in self.map.iter_mut() {
            if let Some(cipher) = c.cipher.as_mut().filter(|_| c.hello_seen) {
                out.extend(cipher.seal(msg.as_bytes()).iter().map(|f| bin(cid, f)));
            }
        }
        out
    }

    /// A different key (rotate): every client belongs to the old identity.
    fn rekey(&mut self, key: [u8; 32]) {
        if self.key != key {
            let all: Vec<Cid> = self.map.keys().copied().collect();
            for cid in all {
                self.forget(&cid);
            }
            self.key = key;
        }
    }

    /// The relay connection is gone: keep everyone for a reconnect, but not forever.
    fn detach(&mut self, now: Instant) {
        for c in self.map.values_mut() {
            c.adrift.get_or_insert(now + MAX_ADRIFT);
        }
    }

    /// A new relay connection: whoever the relay still has is replayed within `REPLAY_WAIT`.
    fn attach(&mut self, now: Instant) {
        self.prune(now);
        for c in self.map.values_mut() {
            if let Some(t) = c.adrift.as_mut() {
                *t = (*t).min(now + REPLAY_WAIT);
            }
        }
    }

    /// Forget adrift clients whose time is up (no relay to tell).
    fn prune(&mut self, now: Instant) {
        let gone: Vec<Cid> = self.map.iter().filter(|(_, c)| c.adrift.is_some_and(|t| t <= now)).map(|(k, _)| *k).collect();
        for cid in gone {
            self.forget(&cid);
        }
    }

    /// A text control frame from the relay: `open` / `close`.
    fn control(&mut self, text: &str, strikes: &mut Strikes, now: Instant) -> Vec<Frame> {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(text) else { return vec![] };
        let Some(cid) = v.get("c").and_then(|c| c.as_str()).and_then(unhex) else { return vec![] };
        match v.get("t").and_then(|t| t.as_str()) {
            Some("open") => {
                let ip = v.get("ip").and_then(|i| i.as_str()).map(sanitize).unwrap_or_default();
                if strikes.is_blocked(&ip, now) {
                    return vec![close_frame(&cid)];
                }
                if let Some(i) = self.refused.iter().position(|(c, _)| *c == cid) {
                    self.refused.swap_remove(i);
                    return vec![close_frame(&cid)];
                }
                // The relay replays `open` for clients still attached after a host reconnect: the
                // browser keeps its E2EE session (it never repeats the hello), so do we.
                if let Some(c) = self.map.get_mut(&cid).filter(|c| c.adrift.is_some()) {
                    c.adrift = None;
                    c.ip = ip;
                    return vec![];
                }
                // Any other repeat of a known id: start over.
                self.forget(&cid);
                let unhelloed = self.map.values().filter(|c| !c.hello_seen).count();
                if self.map.len() >= MAX_CLIENTS || unhelloed >= MAX_UNHELLOED {
                    return vec![close_frame(&cid)];
                }
                let mut h = self.web.register(ConnKind::Relay);
                let conn = h.id;
                let out = self.out.clone();
                let fwd = tokio::spawn(async move {
                    while let Some(o) = h.recv().await {
                        let last = matches!(o, Outbound::Close(_));
                        if out.send((cid, o)).is_err() || last {
                            break;
                        }
                    }
                });
                self.map.insert(cid, Client { conn, ip, cipher: None, hello_seen: false, opened: now, last_ping: now, missed: 0, adrift: None, fwd });
                vec![]
            }
            Some("close") => {
                self.refused.retain(|(c, _)| *c != cid);
                self.forget(&cid);
                vec![]
            }
            _ => vec![],
        }
    }

    /// A binary frame from the relay: `[16-byte client id][payload]`.
    fn binary(&mut self, data: &[u8], strikes: &mut Strikes, now: Instant) -> Vec<Frame> {
        if data.len() < 17 {
            return vec![];
        }
        let mut cid = [0u8; 16];
        cid.copy_from_slice(&data[..16]);
        let payload = &data[16..];
        let Some(c) = self.map.get_mut(&cid) else { return vec![] }; // gone or refused
        let Some(cipher) = c.cipher.as_mut() else {
            // The client speaks first, with a plaintext hello carrying its random `cr`.
            let cr = (payload[0] == crypto::T_HELLO)
                .then(|| serde_json::from_slice::<serde_json::Value>(&payload[1..]).ok())
                .flatten()
                .filter(|v| v.get("v").and_then(|x| x.as_u64()) == Some(1))
                .and_then(|v| v.get("cr").and_then(|x| x.as_str()).and_then(crypto::b64url_decode))
                .and_then(|b| <[u8; 16]>::try_from(b.as_slice()).ok());
            let Some(cr) = cr else {
                strikes.strike(&c.ip.clone(), now);
                return self.refuse(&cid, "hello", now);
            };
            let hr = crypto::random_bytes::<16>();
            c.cipher = Some(crypto::Cipher::new(&crypto::conn_key(&self.key, &cr, &hr), crypto::DIR_HOST));
            // Nothing else before the client proves the key: protocol and version travel in the
            // encrypted `hello` (§5), so a guessed sid learns nothing about this Mantra.
            let reply = serde_json::json!({"v": 1, "hr": crypto::b64url_encode(&hr)});
            return vec![bin(&cid, &crypto::hello_frame(&reply.to_string()))];
        };
        let text = match cipher.open(payload) {
            Ok(None) => return vec![],
            Ok(Some(pt)) => String::from_utf8(pt).ok(),
            Err(_) => None,
        };
        let Some(text) = text else {
            // Before the protocol hello a failed decrypt is a wrong key (password or code):
            // that is what the strike limit throttles. Later it is a broken client.
            if !c.hello_seen {
                strikes.strike(&c.ip.clone(), now);
                return self.refuse(&cid, "badkey", now);
            }
            return self.drop_client(&cid);
        };
        match super::conn::dispatch(&self.web, &self.tx, c.conn, &text, &mut c.hello_seen) {
            super::conn::Dispatched::Reply(r) => cipher.seal(r.as_bytes()).iter().map(|f| bin(&cid, f)).collect(),
            super::conn::Dispatched::Pong => {
                c.missed = 0;
                vec![]
            }
            super::conn::Dispatched::Nothing => vec![],
        }
    }

    /// Something the App/registry sends to one client.
    fn outbound(&mut self, cid: Cid, o: Outbound) -> Vec<Frame> {
        let Some(c) = self.map.get_mut(&cid) else { return vec![] };
        match o {
            Outbound::Text(t) => match c.cipher.as_mut() {
                Some(cipher) => cipher.seal(t.as_bytes()).iter().map(|f| bin(&cid, f)).collect(),
                None => vec![],
            },
            Outbound::Close(reason) => {
                let mut v: Vec<Frame> = match c.cipher.as_mut() {
                    Some(cipher) => cipher.seal(super::protocol::ServerMsg::Bye { reason: reason.into() }.to_json().as_bytes()).iter().map(|f| bin(&cid, f)).collect(),
                    None => vec![],
                };
                v.extend(self.drop_client(&cid));
                v
            }
        }
    }

    /// Hello deadline (10 s) and, when `ping`, the E2EE keep-alive: a `{"t":"ping"}` every 25 s,
    /// two unanswered ones drop the client.
    fn sweep(&mut self, now: Instant, ping: bool) -> Vec<Frame> {
        let late: Vec<Cid> = self
            .map
            .iter()
            .filter(|(_, c)| (!c.hello_seen && now.duration_since(c.opened) >= super::conn::HELLO_TIMEOUT) || c.adrift.is_some_and(|t| t <= now))
            .map(|(k, _)| *k)
            .collect();
        let mut out = vec![];
        for cid in late {
            out.extend(self.drop_client(&cid));
        }
        self.refused.retain(|(cid, at)| {
            if *at <= now {
                out.push(close_frame(cid));
            }
            *at > now
        });
        if ping {
            let ping_json = serde_json::json!({"t": "ping"}).to_string();
            let mut dead = vec![];
            for (cid, c) in self.map.iter_mut() {
                let Some(cipher) = c.cipher.as_mut().filter(|_| c.hello_seen && c.adrift.is_none()) else { continue };
                if now.duration_since(c.last_ping) < super::conn::PING_EVERY.saturating_sub(SWEEP_EVERY) {
                    continue;
                }
                if c.missed >= 2 {
                    dead.push(*cid);
                    continue;
                }
                c.missed += 1;
                c.last_ping = now;
                out.extend(cipher.seal(ping_json.as_bytes()).iter().map(|f| bin(cid, f)));
            }
            for cid in dead {
                out.extend(self.drop_client(&cid));
            }
        }
        out
    }

    /// A failed handshake, said once in plaintext (`0x01 {"err":"badkey"|"hello"}`, §10.3) before
    /// the close, so the browser can tell a wrong password from a dropped connection — the relay's
    /// close code alone can't (it is 4000 for both, or none at all). The client is forgotten now
    /// (later frames from it are ignored); the close follows after `REFUSE_GRACE` via `sweep`.
    fn refuse(&mut self, cid: &Cid, why: &str, now: Instant) -> Vec<Frame> {
        let mut out = vec![bin(cid, &crypto::hello_frame(&serde_json::json!({"err": why}).to_string()))];
        self.forget(cid);
        if self.refused.len() >= MAX_REFUSED {
            out.push(close_frame(cid));
        } else {
            self.refused.push((*cid, now + REFUSE_GRACE));
        }
        out
    }

    /// Forget a client and tell the relay to close it.
    fn drop_client(&mut self, cid: &Cid) -> Vec<Frame> {
        if self.forget(cid) {
            vec![close_frame(cid)]
        } else {
            vec![]
        }
    }

    fn forget(&mut self, cid: &Cid) -> bool {
        match self.map.remove(cid) {
            Some(c) => {
                c.fwd.abort();
                self.web.unregister(c.conn);
                true
            }
            None => false,
        }
    }
}

impl Drop for Clients {
    fn drop(&mut self) {
        for (_, c) in self.map.drain() {
            c.fwd.abort();
            self.web.unregister(c.conn);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_links_and_origins() {
        let sid = "abcdefghijklmnopqrstuvwxyz";
        assert_eq!(code_of(sid), "abcd-efgh-ijkl-mnop-qrst-uvwxyz");
        assert_eq!(relay_origin("wss://remote.mantra.codes"), "https://remote.mantra.codes");
        assert_eq!(relay_origin("ws://127.0.0.1:8787/"), "http://127.0.0.1:8787");
        let link = link_of("https://remote.mantra.codes", "wss://remote.mantra.codes", sid, &[0u8; 32]);
        assert_eq!(link, "https://remote.mantra.codes/s/abcdefghijklmnopqrstuvwxyz#k=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA");
        // relay moved to its own host: the link names it, the site stays put
        let split = link_of("https://remote.mantra.codes/", "wss://relay.example.org:8787", sid, &[0u8; 32]);
        assert_eq!(split, "https://remote.mantra.codes/s/abcdefghijklmnopqrstuvwxyz#k=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA&r=wss://relay.example.org:8787");
        // a relay URL with its own query/fragment characters survives URLSearchParams on the page
        let odd = link_of("https://remote.mantra.codes", "wss://gw.example.org/mantra?tenant=a&x=b+c#y", sid, &[0u8; 32]);
        assert!(odd.ends_with("&r=wss://gw.example.org/mantra%3Ftenant%3Da%26x%3Db%2Bc%23y"), "{odd}");
        let r = odd.split_once("&r=").unwrap().1;
        assert!(!r.contains(['&', '=', '#', '+', '?']), "{r}");
    }

    #[test]
    fn plain_ws_relays_off_this_machine_are_flagged() {
        assert!(relay_without_tls("ws://relay.lan:8787"));
        assert!(relay_without_tls("ws://192.168.1.4:8787/x"));
        for ok in ["ws://127.0.0.1:8787", "ws://localhost", "ws://[::1]:8787/", "ws://127.0.0.2:1", "wss://relay.lan:8787", "wss://remote.mantra.codes"] {
            assert!(!relay_without_tls(ok), "{ok}");
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn identity_persists_and_rotates() {
        let dir = std::env::temp_dir().join(format!("mantra-remote-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (ctl, _c) = tokio::sync::mpsc::unbounded_channel();
        let reg = WebRegistryHandle::new(ctl);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let cfg = RemoteConfig { relay: "ws://127.0.0.1:9".into(), site: "http://127.0.0.1:9".into(), password: None, dir: dir.clone() };
        let r = Remote::start(cfg.clone(), reg.clone(), tx.clone());
        assert!(r.ready(std::time::Duration::from_secs(20)).await);
        let a = r.info();
        assert_eq!(a.sid.as_ref().unwrap().len(), 26);
        assert_eq!(a.password.as_ref().unwrap().split('-').count(), 4, "a generated password");
        assert!(a.link.as_ref().unwrap().starts_with("http://127.0.0.1:9/s/"));
        assert!(a.qr.as_ref().map(|q| q.len() >= 21).unwrap_or(false));
        r.shutdown();
        // a restart keeps sid and the generated password, so the link stays valid
        let r2 = Remote::start(cfg.clone(), reg.clone(), tx.clone());
        assert!(r2.ready(std::time::Duration::from_secs(20)).await);
        let b = r2.info();
        assert_eq!((a.sid.clone(), a.password.clone(), a.link.clone()), (b.sid.clone(), b.password.clone(), b.link.clone()));
        r2.rotate();
        let c = r2.info();
        assert_ne!(c.sid, b.sid);
        assert_ne!(c.password, b.password, "a generated password rotates too");
        assert!(c.link.is_none(), "no link until the new key is derived");
        assert!(r2.ready(std::time::Duration::from_secs(20)).await);
        assert!(r2.info().link.unwrap().contains(c.sid.as_ref().unwrap()));
        r2.shutdown();
        // a configured password is never written to remote.json
        let r3 = Remote::start(RemoteConfig { password: Some("mine".into()), ..cfg }, reg, tx);
        assert_eq!(r3.info().password.as_deref(), Some("mine"));
        assert!(!std::fs::read_to_string(dir.join("remote.json")).unwrap().contains("mine"));
        r3.shutdown();
        let _ = std::fs::remove_dir_all(dir);
    }


    #[test]
    fn relay_urls_parse_ipv4_only() {
        assert_eq!(RelayTarget::parse("wss://remote.mantra.codes"), Ok(RelayTarget { host: "remote.mantra.codes".into(), port: 443, tls: true }));
        assert_eq!(RelayTarget::parse("ws://127.0.0.1:8787/"), Ok(RelayTarget { host: "127.0.0.1".into(), port: 8787, tls: false }));
        assert_eq!(RelayTarget::parse("wss://relay.example.org:9443/mantra"), Ok(RelayTarget { host: "relay.example.org".into(), port: 9443, tls: true }));
        assert!(RelayTarget::parse("wss://[::1]:8787").unwrap_err().contains("IPv4"));
        assert!(RelayTarget::parse("https://remote.mantra.codes").is_err());
        assert!(RelayTarget::parse("ws://host:notaport").is_err());
    }

    #[test]
    fn relay_refusals_read_like_sentences() {
        // the relay's X-Mantra-Relay-Error header decides, whatever the body says
        let one_per_ip = refusal(409, Some("ip-taken"), "some other wording entirely");
        assert!(one_per_ip.contains("one per IPv4 address") && !one_per_ip.contains("409"), "{one_per_ip}");
        assert!(refusal(409, Some("bad-token"), "this address already hosts a session").contains("different host token"), "header beats body");
        assert!(refusal(403, Some("ipv6"), "").contains("over IPv6"));
        assert!(refusal(429, Some("rate-limited"), "").contains("rate-limiting"));
        assert!(refusal(404, Some("bad-sid"), "not found").contains("--remote URL"));
        assert!(refusal(503, Some(" full "), "").contains("full"));
        // an older relay without the header: status and body (substring) still work
        let old = refusal(409, None, "this address already hosts a session\n");
        assert_eq!(old, one_per_ip);
        assert!(refusal(409, None, "").contains("different host token"));
        assert_eq!(refusal(403, None, "hosts must connect over IPv4"), "the relay refused this host: hosts must connect over IPv4");
        // an unknown (newer) code falls back to the status
        assert!(refusal(503, Some("something-new"), "").contains("full"));
        // the body is untrusted: control characters never reach the UI, length is capped
        let odd = refusal(418, None, &format!("\x1b[31mred\x07{}", "x".repeat(1000)));
        assert!(odd.starts_with("the relay refused the connection (HTTP 418: ") && !odd.chars().any(|c| c.is_control()) && odd.len() < 300, "{odd}");
    }

    #[test]
    fn strikes_block_an_ip_for_ten_minutes() {
        let mut st = Strikes::default();
        let t0 = Instant::now();
        for i in 0..STRIKE_LIMIT - 1 {
            st.strike("203.0.113.9", t0 + Duration::from_secs(i as u64));
        }
        assert!(!st.is_blocked("203.0.113.9", t0 + Duration::from_secs(20)));
        st.strike("203.0.113.9", t0 + Duration::from_secs(30));
        assert!(st.is_blocked("203.0.113.9", t0 + Duration::from_secs(31)));
        assert!(!st.is_blocked("198.51.100.1", t0 + Duration::from_secs(31)), "other addresses are unaffected");
        assert!(!st.is_blocked("203.0.113.9", t0 + Duration::from_secs(31) + BLOCK_FOR));
        // strikes outside the window don't add up
        let mut st = Strikes::default();
        for i in 0..STRIKE_LIMIT * 2 {
            st.strike("a", t0 + STRIKE_WINDOW.mul_f64(i as f64 * 0.2));
        }
        assert!(!st.is_blocked("a", t0 + STRIKE_WINDOW * 4));
        // no address from the relay is one shared bucket, not a free pass
        let mut st = Strikes::default();
        for i in 0..STRIKE_LIMIT {
            st.strike("", t0 + Duration::from_secs(i as u64));
        }
        assert!(st.is_blocked("", t0 + Duration::from_secs(20)));
        assert!(st.is_blocked("unknown", t0 + Duration::from_secs(20)));
        assert!(!st.is_blocked("203.0.113.9", t0 + Duration::from_secs(20)));
    }

    /// The browser side of §10.3, as `crypto.js` does it.
    struct FakeBrowser {
        cid: Cid,
        cr: [u8; 16],
        cipher: Option<crypto::Cipher>,
    }

    impl FakeBrowser {
        fn new(n: u8) -> FakeBrowser {
            FakeBrowser { cid: [n; 16], cr: crypto::random_bytes::<16>(), cipher: None }
        }
        fn open(&self) -> String {
            serde_json::json!({"t": "open", "c": hex(&self.cid), "ip": "203.0.113.7"}).to_string()
        }
        fn hello(&self) -> Vec<u8> {
            let mut f = self.cid.to_vec();
            f.extend(crypto::hello_frame(&serde_json::json!({"v": 1, "cr": crypto::b64url_encode(&self.cr)}).to_string()));
            f
        }
        /// Take the host's hello reply and derive the connection key from `key`.
        fn accept(&mut self, frame: &Frame, key: &[u8; 32]) {
            let Frame::Bin(b) = frame else { panic!("expected binary, got {frame:?}") };
            assert_eq!(&b[..16], &self.cid);
            assert_eq!(b[16], crypto::T_HELLO);
            let v: serde_json::Value = serde_json::from_slice(&b[17..]).unwrap();
            assert_eq!(v.as_object().map(|o| o.len()), Some(2), "only v and hr before the key is proven: {v}");
            assert_eq!(v["v"], 1);
            let hr: [u8; 16] = crypto::b64url_decode(v["hr"].as_str().unwrap()).unwrap().try_into().unwrap();
            self.cipher = Some(crypto::Cipher::new(&crypto::conn_key(key, &self.cr, &hr), crypto::DIR_CLIENT));
        }
        fn send(&mut self, json: &str) -> Vec<u8> {
            let f = self.cipher.as_mut().unwrap().seal(json.as_bytes());
            assert_eq!(f.len(), 1);
            let mut out = self.cid.to_vec();
            out.extend(&f[0]);
            out
        }
        fn read(&mut self, frame: &Frame) -> String {
            let Frame::Bin(b) = frame else { panic!("expected binary, got {frame:?}") };
            String::from_utf8(self.cipher.as_mut().unwrap().open(&b[16..]).unwrap().unwrap()).unwrap()
        }
    }

    /// The plaintext refusal a browser gets (its close follows on a later sweep): `[cid][0x01]{"err":why}`.
    fn refused(cid: &Cid, why: &str) -> Vec<Frame> {
        let mut f = cid.to_vec();
        f.extend(crypto::hello_frame(&serde_json::json!({"err": why}).to_string()));
        vec![Frame::Bin(f)]
    }

    /// Receivers a test keeps alive so the Clients' senders stay connected.
    type Keep = (tokio::sync::mpsc::UnboundedReceiver<AppEvent>, tokio::sync::mpsc::UnboundedReceiver<(Cid, Outbound)>);

    fn clients(key: [u8; 32]) -> (Clients, WebRegistryHandle, tokio::sync::mpsc::UnboundedReceiver<super::super::Control>, Keep) {
        let (ctl, ctl_rx) = tokio::sync::mpsc::unbounded_channel();
        let reg = WebRegistryHandle::new(ctl);
        let (tx, app) = tokio::sync::mpsc::unbounded_channel();
        let (out_tx, out) = tokio::sync::mpsc::unbounded_channel();
        (Clients::new(reg.clone(), tx, key, out_tx), reg, ctl_rx, (app, out))
    }

    #[tokio::test]
    async fn a_relay_reconnect_keeps_attached_browsers_encrypted_session() {
        let key = [4u8; 32];
        let (mut cl, reg, mut ctl_rx, _keep) = clients(key);
        let mut st = Strikes::default();
        let t0 = Instant::now();
        let mut b = FakeBrowser::new(1);
        let mut gone = FakeBrowser::new(2);
        for x in [&mut b, &mut gone] {
            cl.control(&x.open(), &mut st, t0);
            let r = cl.binary(&x.hello(), &mut st, t0);
            x.accept(&r[0], &key);
            assert!(cl.binary(&x.send(r#"{"t":"hello","protocol":1}"#), &mut st, t0).is_empty());
        }
        while ctl_rx.try_recv().is_ok() {}
        assert_eq!(cl.ready(), 2);
        // the relay connection drops; a moment later the host is back and the relay replays
        // `open` for the browser still attached to it (not for the one that left meanwhile)
        cl.detach(t0 + Duration::from_secs(1));
        assert_eq!(cl.ready(), 0, "nobody is reachable while the relay connection is down");
        let t1 = t0 + Duration::from_secs(3);
        cl.attach(t1);
        assert!(cl.control(&b.open(), &mut st, t1).is_empty());
        assert_eq!(cl.ready(), 1);
        // the browser carries on with its next counter — no new hello, no strike, same conn
        let pong = cl.binary(&b.send(r#"{"t":"ping"}"#), &mut st, t1);
        assert_eq!(b.read(&pong[0]), r#"{"t":"pong"}"#);
        assert!(!st.is_blocked("203.0.113.7", t1));
        assert_eq!(reg.count(ConnKind::Relay), 2);
        // the one never replayed is dropped once REPLAY_WAIT is over
        assert!(cl.sweep(t1 + Duration::from_secs(1), false).is_empty());
        assert_eq!(cl.sweep(t1 + REPLAY_WAIT, false), vec![close_frame(&gone.cid)]);
        assert_eq!(reg.count(ConnKind::Relay), 1);
        assert_eq!(cl.ready(), 1);
        // a relay that stays away long enough takes everyone with it; so does a new key
        cl.detach(t1 + Duration::from_secs(10));
        cl.prune(t1 + Duration::from_secs(10) + MAX_ADRIFT);
        assert_eq!(reg.count(ConnKind::Relay), 0);
        let mut c = FakeBrowser::new(3);
        cl.control(&c.open(), &mut st, t1);
        let r = cl.binary(&c.hello(), &mut st, t1);
        c.accept(&r[0], &key);
        cl.rekey([5u8; 32]);
        assert_eq!(reg.count(ConnKind::Relay), 0, "a rotated identity forgets the old one's clients");
    }

    #[tokio::test]
    async fn a_wrong_key_or_hello_gets_a_plaintext_refusal_before_the_close() {
        let (mut cl, _reg, _ctl, _keep) = clients([1u8; 32]);
        let mut st = Strikes::default();
        let now = Instant::now();
        let mut b = FakeBrowser::new(1);
        cl.control(&b.open(), &mut st, now);
        let r = cl.binary(&b.hello(), &mut st, now);
        b.accept(&r[0], &[2u8; 32]);
        assert_eq!(cl.binary(&b.send(r#"{"t":"hello","protocol":1}"#), &mut st, now), refused(&b.cid, "badkey"));
        assert!(cl.binary(&b.send(r#"{"t":"ping"}"#), &mut st, now).is_empty(), "a refused client is already forgotten");
        // the close follows once the grace is over, unless the browser hung up first
        assert!(cl.sweep(now, false).is_empty());
        assert_eq!(cl.sweep(now + REFUSE_GRACE, false), vec![close_frame(&b.cid)]);
        assert!(cl.sweep(now + REFUSE_GRACE * 2, false).is_empty(), "closed once");
        let c = FakeBrowser::new(2);
        cl.control(&c.open(), &mut st, now);
        let mut junk = c.cid.to_vec();
        junk.extend(crypto::hello_frame(r#"{"v":2}"#));
        assert_eq!(cl.binary(&junk, &mut st, now), refused(&c.cid, "hello"));
        cl.control(&serde_json::json!({"t": "close", "c": hex(&c.cid)}).to_string(), &mut st, now);
        assert!(cl.sweep(now + REFUSE_GRACE, false).is_empty(), "the browser left by itself: nothing to close");
        // after the protocol hello a broken frame is no key problem: a plain close, no refusal
        let mut d = FakeBrowser::new(3);
        cl.control(&d.open(), &mut st, now);
        let r = cl.binary(&d.hello(), &mut st, now);
        d.accept(&r[0], &[1u8; 32]);
        cl.binary(&d.send(r#"{"t":"hello","protocol":1}"#), &mut st, now);
        let mut bad = d.cid.to_vec();
        bad.extend([crypto::T_DATA, 0, 0, 0, 0, 0, 0, 0, 9]);
        bad.extend([0u8; 16]);
        assert_eq!(cl.binary(&bad, &mut st, now), vec![close_frame(&d.cid)]);
    }

    #[tokio::test]
    async fn relay_clients_are_capped() {
        let key = [6u8; 32];
        let (mut cl, reg, _ctl, _keep) = clients(key);
        let mut st = Strikes::default();
        let now = Instant::now();
        // un-helloed opens (no key needed to send those) stop at MAX_UNHELLOED
        for i in 0..MAX_UNHELLOED as u8 {
            assert!(cl.control(&FakeBrowser::new(100 + i).open(), &mut st, now).is_empty());
        }
        let extra = FakeBrowser::new(200);
        assert_eq!(cl.control(&extra.open(), &mut st, now), vec![close_frame(&extra.cid)]);
        assert_eq!(reg.count(ConnKind::Relay), MAX_UNHELLOED);
        let idle: Vec<Cid> = (0..MAX_UNHELLOED as u8).map(|i| [100 + i; 16]).collect();
        for cid in &idle {
            cl.control(&serde_json::json!({"t": "close", "c": hex(cid)}).to_string(), &mut st, now);
        }
        // devices that proved the key fill the rest, up to MAX_CLIENTS in all
        for i in 0..MAX_CLIENTS as u8 {
            let mut b = FakeBrowser::new(1 + i);
            assert!(cl.control(&b.open(), &mut st, now).is_empty(), "client {i}");
            let r = cl.binary(&b.hello(), &mut st, now);
            b.accept(&r[0], &key);
            cl.binary(&b.send(r#"{"t":"hello","protocol":1}"#), &mut st, now);
        }
        assert_eq!(cl.ready() as usize, MAX_CLIENTS);
        let late = FakeBrowser::new(201);
        assert_eq!(cl.control(&late.open(), &mut st, now), vec![close_frame(&late.cid)]);
        assert_eq!(reg.count(ConnKind::Relay), MAX_CLIENTS);
    }

    #[tokio::test]
    async fn relay_clients_handshake_then_speak_the_protocol() {
        let (ctl, mut ctl_rx) = tokio::sync::mpsc::unbounded_channel();
        let reg = WebRegistryHandle::new(ctl);
        let (tx, mut app_rx) = tokio::sync::mpsc::unbounded_channel();
        let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel();
        let key = [9u8; 32];
        let mut cl = Clients::new(reg.clone(), tx, key, out_tx);
        let mut st = Strikes::default();
        let now = Instant::now();
        let mut b = FakeBrowser::new(1);
        assert!(cl.control(&b.open(), &mut st, now).is_empty());
        assert_eq!(reg.count(ConnKind::Relay), 1, "every relay client is an ordinary registered conn");
        let reply = cl.binary(&b.hello(), &mut st, now);
        assert_eq!(reply.len(), 1);
        b.accept(&reply[0], &key);
        assert_eq!(cl.ready(), 0, "not ready before the protocol hello");
        assert!(cl.binary(&b.send(r#"{"t":"hello","protocol":1,"client":"pwa"}"#), &mut st, now).is_empty());
        let conn = match ctl_rx.try_recv() {
            Ok(super::super::Control::Hello { conn }) => conn,
            other => panic!("expected a hello for the event loop, got {other:?}"),
        };
        assert_eq!(cl.ready(), 1);
        // ping → pong, sealed for this client
        let pong = cl.binary(&b.send(r#"{"t":"ping"}"#), &mut st, now);
        assert_eq!(b.read(&pong[0]), r#"{"t":"pong"}"#);
        // commands go to the App like a local conn's
        assert!(cl.binary(&b.send(r#"{"t":"cmd","req":7,"cmd":"land"}"#), &mut st, now).is_empty());
        assert!(matches!(app_rx.try_recv(), Ok(AppEvent::Web(super::super::Inbound { req: 7, conn: c, .. })) if c == conn));
        // what the App sends to the conn comes out encrypted, in order
        reg.send_text(conn, Arc::from(r#"{"t":"snapshot","seq":1}"#));
        let (cid, o) = out_rx.recv().await.unwrap();
        assert_eq!(cid, b.cid);
        let frames = cl.outbound(cid, o);
        assert_eq!(b.read(&frames[0]), r#"{"t":"snapshot","seq":1}"#);
        // a replayed frame (counter reuse) is fatal for that client only
        let good = b.send(r#"{"t":"ping"}"#);
        cl.binary(&good, &mut st, now);
        assert_eq!(cl.binary(&good, &mut st, now), vec![close_frame(&b.cid)]);
        assert_eq!(reg.count(ConnKind::Relay), 0);
        assert!(!st.is_blocked("203.0.113.7", now), "a broken client after its hello is no strike");
    }

    #[tokio::test]
    async fn wrong_keys_are_refused_and_throttled_per_ip() {
        let (ctl, _ctl_rx) = tokio::sync::mpsc::unbounded_channel();
        let reg = WebRegistryHandle::new(ctl);
        let (tx, _app) = tokio::sync::mpsc::unbounded_channel();
        let (out_tx, _out) = tokio::sync::mpsc::unbounded_channel();
        let mut cl = Clients::new(reg.clone(), tx, [1u8; 32], out_tx);
        let mut st = Strikes::default();
        let now = Instant::now();
        for i in 0..STRIKE_LIMIT as u8 {
            let mut b = FakeBrowser::new(i + 1);
            cl.control(&b.open(), &mut st, now);
            let r = cl.binary(&b.hello(), &mut st, now);
            b.accept(&r[0], &[2u8; 32]); // the browser derived its key from the wrong password
            assert_eq!(cl.binary(&b.send(r#"{"t":"hello","protocol":1}"#), &mut st, now), refused(&b.cid, "badkey"));
        }
        assert_eq!(reg.count(ConnKind::Relay), 0);
        let b = FakeBrowser::new(99);
        assert_eq!(cl.control(&b.open(), &mut st, now), vec![close_frame(&b.cid)], "10 wrong keys → that IP is ignored");
        assert_eq!(reg.count(ConnKind::Relay), 0);
        // anything but a hello first is refused too
        let other = serde_json::json!({"t": "open", "c": hex(&[77; 16]), "ip": "198.51.100.2"}).to_string();
        cl.control(&other, &mut st, now);
        let mut junk = vec![77u8; 16];
        junk.extend([crypto::T_DATA, 0, 0]);
        assert_eq!(cl.binary(&junk, &mut st, now), refused(&[77; 16], "hello"));
        // every refused client is closed once its grace is over
        assert_eq!(cl.sweep(now + REFUSE_GRACE, false).len(), STRIKE_LIMIT + 1);
        assert_eq!(cl.control(&other, &mut st, now), vec![], "a new open after the close starts over");
        cl.control(&serde_json::json!({"t": "close", "c": hex(&[77; 16])}).to_string(), &mut st, now);
        // a client that never says hello is closed after 10 s
        let mut b = FakeBrowser::new(50);
        b.cid = [50; 16];
        let late = serde_json::json!({"t": "open", "c": hex(&b.cid), "ip": ""}).to_string();
        cl.control(&late, &mut st, now);
        assert!(cl.sweep(now + Duration::from_secs(5), false).is_empty());
        assert_eq!(cl.sweep(now + Duration::from_secs(11), false), vec![close_frame(&b.cid)]);
    }

    /// A relay that refuses every host upgrade with `status`, extra `headers` and `body`.
    async fn refusing_relay(status: &'static str, headers: &'static str, body: &'static str) -> std::net::SocketAddr {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut s, _)) = l.accept().await {
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf).await;
                let _ = s.write_all(format!("HTTP/1.1 {status}\r\n{headers}Content-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await;
            }
        });
        addr
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn one_session_per_ipv4_refusal_is_shown_and_retried() {
        // the relay's header, with a body in words this build has never seen
        refusal_reaches_last_error("header", "X-Mantra-Relay-Error: ip-taken\r\n", "one IPv4, one host", "one per IPv4 address").await;
        // an older relay: no header, the known body
        refusal_reaches_last_error("body", "", "this address already hosts a session", "one per IPv4 address").await;
        refusal_reaches_last_error("token", "X-Mantra-Relay-Error: bad-token\r\n", "this address already hosts a session", "different host token").await;
    }

    async fn refusal_reaches_last_error(tag: &str, headers: &'static str, body: &'static str, want: &str) {
        let dir = std::env::temp_dir().join(format!("mantra-remote-409-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let addr = refusing_relay("409 Conflict", headers, body).await;
        let (ctl, _c) = tokio::sync::mpsc::unbounded_channel();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let r = Remote::start(RemoteConfig { relay: format!("ws://{addr}"), site: format!("http://{addr}"), password: None, dir: dir.clone() }, WebRegistryHandle::new(ctl), tx);
        let mut seen = None;
        for _ in 0..200 {
            if let Some(e) = r.info().last_error.filter(|e| !e.is_empty()) {
                seen = Some(e);
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let e = seen.expect("the refusal shows up in RemoteInfo.last_error");
        assert!(e.contains(want), "{tag}: {e}");
        assert!(!r.info().connected);
        r.shutdown();
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The blocker case: the host's relay connection drops and comes back while the browser stays
    /// attached to the relay — the relay replays `open`, the browser just carries on encrypted.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_host_reconnect_keeps_the_browser_session_through_run() {
        let dir = std::env::temp_dir().join(format!("mantra-remote-replay-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let (ctl, _ctl_rx) = tokio::sync::mpsc::unbounded_channel();
        let reg = WebRegistryHandle::new(ctl);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let r = Remote::start(RemoteConfig { relay: format!("ws://{addr}"), site: format!("http://{addr}"), password: Some("pw".into()), dir: dir.clone() }, reg.clone(), tx);
        assert!(r.ready(Duration::from_secs(20)).await);
        let key = r.inner().identity().unwrap().2;
        async fn next_bin(ws: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>) -> Vec<u8> {
            loop {
                if let Message::Binary(x) = ws.next().await.unwrap().unwrap() {
                    return x.to_vec();
                }
            }
        }
        let mut ws = tokio_tungstenite::accept_async(l.accept().await.unwrap().0).await.unwrap();
        let mut b = FakeBrowser::new(8);
        ws.send(Message::Text(b.open().into())).await.unwrap();
        ws.send(Message::Binary(b.hello().into())).await.unwrap();
        b.accept(&Frame::Bin(next_bin(&mut ws).await), &key);
        ws.send(Message::Binary(b.send(r#"{"t":"hello","protocol":1}"#).into())).await.unwrap();
        ws.send(Message::Binary(b.send(r#"{"t":"ping"}"#).into())).await.unwrap();
        loop {
            let f = next_bin(&mut ws).await;
            if b.read(&Frame::Bin(f)).contains("pong") {
                break;
            }
        }
        // the relay drops the host; the host dials again and gets the still-attached browser replayed
        drop(ws);
        let mut ws = tokio::time::timeout(Duration::from_secs(10), async { tokio_tungstenite::accept_async(l.accept().await.unwrap().0).await.unwrap() }).await.expect("the host reconnects");
        ws.send(Message::Text(b.open().into())).await.unwrap();
        ws.send(Message::Binary(b.send(r#"{"t":"ping"}"#).into())).await.unwrap();
        let got = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let f = next_bin(&mut ws).await;
                let t = b.read(&Frame::Bin(f));
                if t.contains("pong") {
                    break t;
                }
            }
        })
        .await
        .expect("the same E2EE session answers after the reconnect");
        assert_eq!(got, r#"{"t":"pong"}"#);
        assert_eq!(reg.count(ConnKind::Relay), 1, "still the one registered conn");
        r.shutdown();
        let _ = std::fs::remove_dir_all(dir);
    }

    /// §10.3/§10.4: a rotate tells every attached, hello-completed client before dropping it and
    /// dialing the new identity — not just a silent close.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_rotate_notifies_attached_clients_before_dropping_them() {
        let dir = std::env::temp_dir().join(format!("mantra-remote-rotate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let (ctl, _ctl_rx) = tokio::sync::mpsc::unbounded_channel();
        let reg = WebRegistryHandle::new(ctl);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let r = Remote::start(RemoteConfig { relay: format!("ws://{addr}"), site: format!("http://{addr}"), password: Some("pw".into()), dir: dir.clone() }, reg.clone(), tx);
        assert!(r.ready(Duration::from_secs(20)).await);
        let old_key = r.inner().identity().unwrap().2;
        async fn next_bin(ws: &mut tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>) -> Vec<u8> {
            loop {
                if let Message::Binary(x) = ws.next().await.unwrap().unwrap() {
                    return x.to_vec();
                }
            }
        }
        let mut ws = tokio_tungstenite::accept_async(l.accept().await.unwrap().0).await.unwrap();
        let mut b = FakeBrowser::new(9);
        ws.send(Message::Text(b.open().into())).await.unwrap();
        ws.send(Message::Binary(b.hello().into())).await.unwrap();
        b.accept(&Frame::Bin(next_bin(&mut ws).await), &old_key);
        ws.send(Message::Binary(b.send(r#"{"t":"hello","protocol":1}"#).into())).await.unwrap();
        // A `ping` dispatches (and answers) only after the earlier `hello` frame does, since the
        // host reads them off one ordered stream — so a `pong` back proves `hello_seen` is set.
        ws.send(Message::Binary(b.send(r#"{"t":"ping"}"#).into())).await.unwrap();
        loop {
            let f = next_bin(&mut ws).await;
            if b.read(&Frame::Bin(f)).contains("pong") {
                break;
            }
        }
        r.rotate();
        let notice = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let f = next_bin(&mut ws).await;
                let t = b.read(&Frame::Bin(f));
                if t.starts_with(r#"{"t":"bye""#) {
                    break t;
                }
            }
        })
        .await
        .expect("the client gets a bye before the connection drops");
        assert_eq!(notice, r#"{"t":"bye","reason":"rotated"}"#);
        // ...and the host reconnects at once, with a new identity nobody from the old one can follow.
        let mut ws2 = tokio::time::timeout(Duration::from_secs(10), async { tokio_tungstenite::accept_async(l.accept().await.unwrap().0).await.unwrap() }).await.expect("the host reconnects right away");
        let new_key = r.inner().identity().unwrap().2;
        assert_ne!(old_key, new_key, "rotate must produce a new key");
        let _ = ws2.close(None).await;
        r.shutdown();
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A fake relay speaking §10.2: accepts the host, then plays one browser through it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_browser_reaches_the_app_through_a_fake_relay() {
        let dir = std::env::temp_dir().join(format!("mantra-remote-e2e-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let (auth_tx, auth_rx) = tokio::sync::oneshot::channel::<String>();
        let (ctl, mut ctl_rx) = tokio::sync::mpsc::unbounded_channel();
        let reg = WebRegistryHandle::new(ctl);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let r = Remote::start(RemoteConfig { relay: format!("ws://{addr}"), site: format!("http://{addr}"), password: Some("pw".into()), dir: dir.clone() }, reg.clone(), tx);
        assert!(r.ready(Duration::from_secs(20)).await);
        let key = r.inner().identity().unwrap().2;
        let (s, _) = l.accept().await.unwrap();
        let mut auth_tx = Some(auth_tx);
        #[allow(clippy::result_large_err)]
        let cb = |req: &tokio_tungstenite::tungstenite::handshake::server::Request, resp| {
            let a = req.headers().get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
            let _ = auth_tx.take().map(|t| t.send(format!("{} {a}", req.uri().path())));
            Ok(resp)
        };
        let mut ws = tokio_tungstenite::accept_hdr_async(s, cb).await.unwrap();
        let (sid, token, _) = r.inner().identity().unwrap();
        assert_eq!(auth_rx.await.unwrap(), format!("/host/{sid} Bearer {token}"));
        let mut b = FakeBrowser::new(3);
        ws.send(Message::Text(b.open().into())).await.unwrap();
        ws.send(Message::Binary(b.hello().into())).await.unwrap();
        let reply = loop {
            match ws.next().await.unwrap().unwrap() {
                Message::Binary(x) => break Frame::Bin(x.to_vec()),
                _ => continue,
            }
        };
        b.accept(&reply, &key);
        ws.send(Message::Binary(b.send(r#"{"t":"hello","protocol":1}"#).into())).await.unwrap();
        let conn = loop {
            match ctl_rx.recv().await.unwrap() {
                super::super::Control::Hello { conn } => break conn,
                _ => continue,
            }
        };
        // a big message crosses as fragments and arrives whole
        let big = format!(r#"{{"t":"snapshot","pad":"{}"}}"#, "x".repeat(2_000_000));
        reg.send_text(conn, Arc::from(big.as_str()));
        let mut got = String::new();
        let mut frags = 0;
        while got.is_empty() {
            if let Message::Binary(x) = ws.next().await.unwrap().unwrap() {
                assert_eq!(&x[..16], &b.cid);
                frags += 1;
                if let Some(pt) = b.cipher.as_mut().unwrap().open(&x[16..]).unwrap() {
                    got = String::from_utf8(pt).unwrap();
                }
            }
        }
        assert_eq!(got, big);
        assert!(frags >= 3, "{frags} frames for 2 MB");
        for _ in 0..100 {
            if r.info().connected && r.info().clients == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(r.info().connected && r.info().clients == 1, "{:?}", r.info().last_error);
        // the relay says the browser left → its conn is gone
        ws.send(Message::Text(serde_json::json!({"t": "close", "c": hex(&b.cid)}).to_string().into())).await.unwrap();
        for _ in 0..100 {
            if reg.count(ConnKind::Relay) == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(reg.count(ConnKind::Relay), 0);
        r.shutdown();
        let _ = std::fs::remove_dir_all(dir);
    }
}
