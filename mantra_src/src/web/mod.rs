//! `mantra --web` / `--remote`: the web UI server, its protocol, and the relay client
//! (design: WEB-DESIGN.md §2–§10).
//!
//! Ownership: the event loop owns `Web` (publisher state, server and relay handles); the App gets
//! a cheap `Link` (reply channel, remote + push handles) for commands and the TUI overlays.
//! Server and relay tasks only hold channels — every read or write of App state happens on the
//! App task, in `App::web_command` or `Web::publish`, so App stays single-owner.

pub mod auth;
pub mod commands;
pub mod conn;
// Parts of these are only used by the relay client and push delivery (packages B/C).
#[allow(dead_code)]
pub mod crypto;
pub mod protocol;
#[allow(dead_code)]
pub mod push;
pub mod qr;
#[allow(dead_code)]
pub mod remote;
pub mod server;
pub mod snapshot;
pub mod tls;

use crate::app::{App, AppEvent};
use anyhow::{Context, Result};
use protocol::{HelloMsg, NoteMsg, ServerMsg, PROTOCOL};
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

pub const DEFAULT_LISTEN: &str = "127.0.0.1:7777";
pub const DEFAULT_RELAY: &str = "wss://remote.mantra.codes";
/// The website remote links point at when the default relay is used.
pub const DEFAULT_SITE: &str = "https://remote.mantra.codes";
/// Outbound messages queued for one connection before it is dropped as too slow.
const MAX_QUEUED: usize = 512;

pub type ConnId = u64;

/// One command from a client, for the App task.
#[derive(Debug)]
pub struct Inbound {
    pub conn: ConnId,
    pub req: u64,
    pub cmd: protocol::Command,
}

/// What a connection's writer sends. Text is serialized once and shared by every connection.
#[derive(Clone, Debug)]
pub enum Outbound {
    Text(Arc<str>),
    /// Send `{"t":"bye","reason":…}` and close.
    Close(&'static str),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConnKind {
    Local,
    Relay,
}

/// Messages from connections to the event loop that are not App commands.
#[derive(Debug)]
pub enum Control {
    /// The client said hello: send it hello + a full snapshot, then include it in broadcasts.
    Hello { conn: ConnId },
    /// Something outside the App changed (the relay's state, a derived key): publish and redraw.
    Refresh,
}

struct ConnEntry {
    tx: mpsc::UnboundedSender<Outbound>,
    kind: ConnKind,
    /// Receives broadcasts (set once its snapshot went out, so deltas always follow it).
    joined: bool,
    queued: Arc<AtomicUsize>,
}

struct Registry {
    conns: Mutex<HashMap<ConnId, ConnEntry>>,
    next: AtomicU64,
    ctl: mpsc::UnboundedSender<Control>,
}

/// The connection registry shared by the local server, the relay client and the App: register a
/// connection, get its outbound queue; send to one; broadcast to all joined.
#[derive(Clone)]
pub struct WebRegistryHandle {
    inner: Arc<Registry>,
}

/// A registered connection's end of its outbound queue.
pub struct ConnHandle {
    pub id: ConnId,
    rx: mpsc::UnboundedReceiver<Outbound>,
    queued: Arc<AtomicUsize>,
}

impl ConnHandle {
    pub async fn recv(&mut self) -> Option<Outbound> {
        let m = self.rx.recv().await;
        if m.is_some() {
            let _ = self.queued.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| Some(n.saturating_sub(1)));
        }
        m
    }
}

impl WebRegistryHandle {
    fn new(ctl: mpsc::UnboundedSender<Control>) -> WebRegistryHandle {
        WebRegistryHandle { inner: Arc::new(Registry { conns: Mutex::new(HashMap::new()), next: AtomicU64::new(1), ctl }) }
    }

    fn conns(&self) -> std::sync::MutexGuard<'_, HashMap<ConnId, ConnEntry>> {
        self.inner.conns.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn register(&self, kind: ConnKind) -> ConnHandle {
        let id = self.inner.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::unbounded_channel();
        let queued = Arc::new(AtomicUsize::new(0));
        self.conns().insert(id, ConnEntry { tx, kind, joined: false, queued: queued.clone() });
        ConnHandle { id, rx, queued }
    }

    pub fn unregister(&self, id: ConnId) {
        self.conns().remove(&id);
    }

    /// Forward a client's hello to the event loop.
    pub fn hello(&self, id: ConnId) {
        let _ = self.inner.ctl.send(Control::Hello { conn: id });
    }

    /// Ask the event loop to publish and redraw soon (for state the App doesn't own).
    pub fn refresh(&self) {
        let _ = self.inner.ctl.send(Control::Refresh);
    }

    pub fn kind(&self, id: ConnId) -> Option<ConnKind> {
        self.conns().get(&id).map(|c| c.kind)
    }

    /// Connections receiving broadcasts.
    pub fn joined(&self) -> usize {
        self.conns().values().filter(|c| c.joined).count()
    }

    /// Connections of a kind (joined or not).
    pub fn count(&self, kind: ConnKind) -> usize {
        self.conns().values().filter(|c| c.kind == kind).count()
    }

    fn push(conns: &mut HashMap<ConnId, ConnEntry>, id: ConnId, m: Outbound) {
        let Some(c) = conns.get(&id) else { return };
        if c.queued.load(Ordering::Relaxed) >= MAX_QUEUED {
            // A client that can't keep up gets dropped (it reconnects and gets a fresh snapshot)
            // rather than growing our memory without bound.
            let _ = c.tx.send(Outbound::Close("slow"));
            crate::mlog!("web: connection {id} too slow, dropped");
            conns.remove(&id);
            return;
        }
        c.queued.fetch_add(1, Ordering::Relaxed);
        if c.tx.send(m).is_err() {
            conns.remove(&id);
        }
    }

    pub fn send_text(&self, id: ConnId, text: Arc<str>) {
        Self::push(&mut self.conns(), id, Outbound::Text(text));
    }

    pub fn send(&self, id: ConnId, msg: &ServerMsg) {
        self.send_text(id, msg.to_json().into());
    }

    /// Close one connection (the relay client uses it when a client misbehaves).
    #[allow(dead_code)]
    pub fn close(&self, id: ConnId, reason: &'static str) {
        let mut c = self.conns();
        if let Some(e) = c.remove(&id) {
            let _ = e.tx.send(Outbound::Close(reason));
        }
    }

    fn join(&self, id: ConnId) {
        if let Some(c) = self.conns().get_mut(&id) {
            c.joined = true;
        }
    }

    /// To every joined connection: `local` to local ones, `relay` to relay ones.
    fn broadcast(&self, local: &Arc<str>, relay: &Arc<str>) {
        let mut conns = self.conns();
        let ids: Vec<(ConnId, ConnKind)> = conns.iter().filter(|(_, c)| c.joined).map(|(id, c)| (*id, c.kind)).collect();
        for (id, kind) in ids {
            let text = if kind == ConnKind::Relay { relay.clone() } else { local.clone() };
            Self::push(&mut conns, id, Outbound::Text(text));
        }
    }

    fn close_all(&self, reason: &'static str) {
        for (_, c) in self.conns().drain() {
            let _ = c.tx.send(Outbound::Close(reason));
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum TlsMode {
    Off,
    SelfSigned,
    Files(PathBuf, PathBuf),
}

/// The web-related command-line flags (parsed in main.rs).
#[derive(Clone, Debug, Default)]
pub struct CliWeb {
    /// `--web` given (with or without a value).
    pub web: bool,
    /// `--web ADDR:PORT` / `--web-listen ADDR:PORT`.
    pub listen: Option<String>,
    pub password: Option<String>,
    pub tls: bool,
    pub cert: Option<PathBuf>,
    pub key: Option<PathBuf>,
    /// `--remote [URL]`: `Some(None)` = default relay.
    pub remote: Option<Option<String>>,
    /// `--remote-site URL`: the website links open (when it is not on the relay's host).
    pub remote_site: Option<String>,
    pub headless: bool,
}

/// Everything `Web::start` needs, validated.
#[derive(Clone, Debug)]
pub struct WebConfig {
    /// The local listener; `None` with `--remote` alone.
    pub listen: Option<SocketAddr>,
    pub password: Option<String>,
    pub tls: TlsMode,
    /// Relay URL when `--remote`.
    pub relay: Option<String>,
    /// The website remote links open (`https://…`), when `--remote`.
    pub remote_site: Option<String>,
    pub headless: bool,
    pub push: bool,
    /// VAPID `sub` claim (push delivery, package B).
    pub contact: String,
    pub sans: Vec<String>,
    /// `$MANTRA_HOME/web`.
    pub dir: PathBuf,
}

/// `host:port`, `[v6]:port`, `localhost:port` or a bare port (→ 127.0.0.1).
pub fn parse_listen(s: &str) -> Option<SocketAddr> {
    let s = s.trim();
    if let Ok(p) = s.parse::<u16>() {
        return Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), p));
    }
    if let Ok(a) = s.parse::<SocketAddr>() {
        return Some(a);
    }
    let (host, port) = s.rsplit_once(':')?;
    let port = port.parse::<u16>().ok()?;
    if host.eq_ignore_ascii_case("localhost") {
        return Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port));
    }
    None
}

impl WebConfig {
    /// Flags > environment > settings. `Ok(None)` when neither `--web` nor `--remote` was given.
    pub fn resolve(cli: &CliWeb, s: &crate::config::WebSettings, env_password: Option<String>) -> Result<Option<WebConfig>> {
        if cli.headless && !cli.web && cli.remote.is_none() {
            anyhow::bail!("--headless needs --web or --remote (nothing would be reachable)");
        }
        if cli.cert.is_some() != cli.key.is_some() {
            anyhow::bail!("--web-cert and --web-key go together");
        }
        // Checked before the early return below: otherwise `--remote-site` alone (or with `--web`)
        // would be dropped without a word. The settings value is fine unused (it waits for --remote).
        if cli.remote_site.is_some() && cli.remote.is_none() {
            anyhow::bail!("--remote-site needs --remote");
        }
        if !cli.web && cli.remote.is_none() {
            return Ok(None);
        }
        let password = cli.password.clone().or(env_password).or_else(|| Some(s.password.clone())).filter(|p| !p.is_empty());
        let listen = if cli.web {
            let raw = cli.listen.clone().or_else(|| Some(s.listen.clone()).filter(|l| !l.trim().is_empty())).unwrap_or_else(|| DEFAULT_LISTEN.to_string());
            let addr = parse_listen(&raw).ok_or_else(|| anyhow::anyhow!("--web: '{raw}' is not host:port or port"))?;
            if !addr.ip().is_loopback() && password.is_none() {
                anyhow::bail!("--web: listening on {addr} needs a password (--web-password, MANTRA_WEB_PASSWORD or [web] password); without one Mantra only listens on localhost");
            }
            // Loopback without a password trusts whoever reaches the port — tolerable when the
            // person at this terminal started it for themselves, not for an unattended service
            // that is typically reached through a port-forward or tunnel (which also arrives
            // from 127.0.0.1). `--headless --remote` alone has no listener and gets a generated
            // remote password, so it is unaffected.
            if cli.headless && password.is_none() {
                anyhow::bail!("--headless needs a password (--web-password / MANTRA_WEB_PASSWORD): nobody is at this terminal to vouch for localhost");
            }
            Some(addr)
        } else {
            None
        };
        let tls = match (&cli.cert, &cli.key) {
            (Some(c), Some(k)) => TlsMode::Files(c.clone(), k.clone()),
            _ if !s.cert.is_empty() && !s.key.is_empty() => TlsMode::Files(PathBuf::from(&s.cert), PathBuf::from(&s.key)),
            _ if cli.tls || s.tls => TlsMode::SelfSigned,
            _ => TlsMode::Off,
        };
        if let TlsMode::Files(c, k) = &tls {
            for p in [c, k] {
                std::fs::metadata(p).map_err(|e| anyhow::anyhow!("--web-cert: cannot read {}: {e}", p.display()))?;
            }
        }
        let relay = cli.remote.as_ref().map(|r| r.clone().or_else(|| Some(s.relay.clone()).filter(|x| !x.trim().is_empty())).unwrap_or_else(|| DEFAULT_RELAY.to_string()));
        if let Some(r) = &relay {
            if !(r.starts_with("wss://") || r.starts_with("ws://")) {
                anyhow::bail!("--remote: '{r}' is not a ws:// or wss:// URL");
            }
        }
        // The site serves the page, the relay forwards bytes: they need not share a host.
        let remote_site = match &relay {
            None => None,
            Some(r) => {
                let site = cli.remote_site.clone().or_else(|| Some(s.remote_site.clone())).map(|x| x.trim().trim_end_matches('/').to_string()).filter(|x| !x.is_empty());
                let site = site.unwrap_or_else(|| if r.trim_end_matches('/') == DEFAULT_RELAY { DEFAULT_SITE.to_string() } else { remote::relay_origin(r) });
                if !(site.starts_with("https://") || site.starts_with("http://")) {
                    anyhow::bail!("--remote-site: '{site}' is not an http:// or https:// URL");
                }
                Some(site)
            }
        };
        Ok(Some(WebConfig {
            listen,
            password,
            tls,
            relay,
            remote_site,
            headless: cli.headless,
            push: s.push,
            contact: if s.contact.trim().is_empty() { "https://mantra.codes".into() } else { s.contact.trim().to_string() },
            sans: s.sans.clone(),
            dir: crate::config::home().join("web"),
        }))
    }

    /// Legal but risky setups, said once at startup (log + TUI toast).
    pub fn warnings(&self) -> Vec<String> {
        let mut w = vec![];
        if self.listen.is_some_and(|a| a.ip().is_loopback()) && self.password.is_none() {
            w.push(format!("web UI on {} without a password — anything that can reach this port (port-forwards, tunnels) is trusted", self.listen.map(|a| a.ip().to_string()).unwrap_or_default()));
        }
        if let Some(r) = &self.relay {
            if remote::relay_without_tls(r) {
                w.push("relay without TLS: browsers on https pages cannot reach it".into());
            }
        }
        w
    }
}

/// What the TUI's `/web` overlay shows.
#[derive(Clone, Debug, Default)]
pub struct LinkInfo {
    pub listen: Option<SocketAddr>,
    /// URLs a browser can open (the LAN address first when listening on all interfaces).
    pub urls: Vec<String>,
    pub tls: bool,
    /// Serving our own CA-signed certificate (`/cert.pem` installs the CA).
    pub self_signed: bool,
    pub password: bool,
    pub push: bool,
}

#[derive(Clone)]
pub struct PushLink {
    pub store: Arc<Mutex<push::Store>>,
    pub sender: push::Sender,
}

/// The App's handle on the web layer.
#[derive(Clone)]
pub struct Link {
    pub reg: WebRegistryHandle,
    pub remote: Option<Arc<remote::Remote>>,
    pub push: Option<PushLink>,
    pub info: LinkInfo,
}

impl Link {
    pub fn reply(&self, conn: ConnId, msg: &ServerMsg) {
        self.reg.send(conn, msg);
    }
}

/// The running web layer, owned by the event loop.
pub struct Web {
    pub cfg: WebConfig,
    reg: WebRegistryHandle,
    ctl_rx: mpsc::UnboundedReceiver<Control>,
    publisher: snapshot::Publisher,
    server: Option<server::ServerHandle>,
    pub remote: Option<Arc<remote::Remote>>,
    push: Option<PushLink>,
    vapid_public: Option<String>,
    info: LinkInfo,
}

impl Web {
    pub async fn start(cfg: WebConfig, tx: mpsc::UnboundedSender<AppEvent>) -> Result<Web> {
        std::fs::create_dir_all(&cfg.dir).with_context(|| format!("creating {}", cfg.dir.display()))?;
        tls::restrict_dir(&cfg.dir);
        let (ctl_tx, ctl_rx) = mpsc::unbounded_channel();
        let reg = WebRegistryHandle::new(ctl_tx);
        let mut push_link = None;
        let mut vapid_public = None;
        if cfg.push {
            let dir = cfg.dir.join("push");
            let _ = std::fs::create_dir_all(&dir);
            tls::restrict_dir(&dir);
            match push::Vapid::load_or_create(&dir).map(|v| v.with_contact(cfg.contact.clone())) {
                Ok(v) => {
                    let store = Arc::new(Mutex::new(push::Store::load(&dir)));
                    vapid_public = Some(v.public_b64url.clone());
                    push_link = Some(PushLink { sender: push::Sender::start(store.clone(), v), store });
                }
                Err(e) => crate::mlog!("web: push disabled — cannot create VAPID keys: {e}"),
            }
        }
        for w in cfg.warnings() {
            crate::mlog!("web: {w}");
        }
        let mut info = LinkInfo { listen: cfg.listen, password: cfg.password.is_some(), push: push_link.is_some(), ..Default::default() };
        let mut server = None;
        if let Some(addr) = cfg.listen {
            let material = match &cfg.tls {
                TlsMode::Off => None,
                TlsMode::SelfSigned => {
                    let sans = tls::discover_sans(Some(addr), &cfg.sans);
                    Some(tls::self_signed(&tls::tls_dir(&cfg.dir), &sans)?)
                }
                TlsMode::Files(c, k) => Some(tls::load(c, k)?),
            };
            info.tls = material.is_some();
            info.self_signed = material.as_ref().map(|m| m.ca_pem.is_some()).unwrap_or(false);
            let auth: Arc<dyn auth::Authenticator> = Arc::new(auth::PasswordAuth::new(cfg.password.clone(), Some(cfg.dir.join("sessions.json"))));
            let ctx = server::ServerCtx { auth, reg: reg.clone(), tx: tx.clone(), tls: info.tls, push: push_link.is_some(), ca_pem: material.as_ref().and_then(|m| m.ca_pem.clone()) };
            let h = server::start(addr, material, ctx)?;
            info.urls = listen_urls(addr, info.tls);
            crate::mlog!("web: listening on {}", info.urls.join(" "));
            server = Some(h);
        }
        let remote = cfg.relay.as_ref().map(|relay| Arc::new(remote::Remote::start(remote::RemoteConfig { relay: relay.clone(), site: cfg.remote_site.clone().unwrap_or_else(|| remote::relay_origin(relay)), password: cfg.password.clone(), dir: cfg.dir.clone(), headless: cfg.headless }, reg.clone(), tx.clone())));
        let env = snapshot::Env { tls: info.tls, listen: info.urls.first().cloned(), headless: cfg.headless, push: push_link.is_some() };
        Ok(Web { cfg, reg, ctl_rx, publisher: snapshot::Publisher::new(env), server, remote, push: push_link, vapid_public, info })
    }

    pub fn link(&self) -> Link {
        Link { reg: self.reg.clone(), remote: self.remote.clone(), push: self.push.clone(), info: self.info.clone() }
    }

    /// Next control message (hello) from a connection; pending forever if the channel is gone.
    pub async fn next_control(&mut self) -> Option<Control> {
        self.ctl_rx.recv().await
    }

    /// Diff the App against the last publish and broadcast the delta (if any). Skipped while no
    /// client is connected — a hello always publishes first, so nobody ever sees a stale base.
    pub fn publish(&mut self, app: &App) {
        if self.reg.joined() == 0 {
            return;
        }
        self.publish_now(app);
    }

    fn publish_now(&mut self, app: &App) {
        let remote = self.remote.as_ref().map(|r| r.info());
        let Some(d) = self.publisher.publish(app, remote) else { return };
        let local: Arc<str> = ServerMsg::Delta(Box::new(d.clone())).to_json().into();
        let relay: Arc<str> = if d.remote.is_some() {
            let mut r = d;
            r.remote = None;
            ServerMsg::Delta(Box::new(r)).to_json().into()
        } else {
            local.clone()
        };
        self.reg.broadcast(&local, &relay);
    }

    pub fn on_control(&mut self, c: Control, app: &App) {
        match c {
            Control::Hello { conn } => {
                // Flush pending changes to everyone first, so the snapshot and the next delta
                // this connection sees are consecutive.
                self.publish_now(app);
                let Some(kind) = self.reg.kind(conn) else { return };
                let hello = ServerMsg::Hello(HelloMsg {
                    protocol: PROTOCOL,
                    version: env!("CARGO_PKG_VERSION").into(),
                    mode: if kind == ConnKind::Relay { "relay" } else { "local" }.into(),
                    conn: conn.to_string(),
                    vapid: self.vapid_public.clone(),
                    tls: kind == ConnKind::Relay || self.info.tls,
                    push: self.push.is_some(),
                });
                self.reg.send(conn, &hello);
                if self.publisher.full(true).is_none() {
                    // Nothing published yet (a hello in the first milliseconds): prime now.
                    self.publisher.publish(app, self.remote.as_ref().map(|r| r.info()));
                }
                if let Some(snap) = self.publisher.full(kind == ConnKind::Local) {
                    self.reg.send(conn, &ServerMsg::Snapshot(Box::new(snap)));
                }
                self.reg.join(conn);
            }
            Control::Refresh => self.publish(app),
        }
    }

    /// App notes → `note` messages for every client, and Web Push for subscribed devices.
    pub fn notes(&self, notes: &[String], app: &App) {
        for n in notes {
            let push = push::notification_for(n, app);
            let kind = push.as_ref().map(|p| p.kind.clone()).unwrap_or_else(|| "info".into());
            let agent = push.as_ref().and_then(|p| p.url.strip_prefix("/agent/")).and_then(|id| id.parse().ok());
            let msg: Arc<str> = ServerMsg::Note(NoteMsg { kind, text: n.clone(), agent, at: snapshot::now_ms() }).to_json().into();
            self.reg.broadcast(&msg, &msg);
            if let (Some(pl), Some(p)) = (&self.push, push) {
                pl.sender.enqueue(p);
            }
        }
    }

    pub fn shutdown(&mut self) {
        self.reg.close_all("quit");
        if let Some(s) = self.server.take() {
            s.shutdown();
        }
        if let Some(r) = &self.remote {
            r.shutdown();
        }
    }
}

/// Browser URLs for a listen address: all interfaces → the LAN address and localhost.
pub fn listen_urls(addr: SocketAddr, tls: bool) -> Vec<String> {
    let scheme = if tls { "https" } else { "http" };
    let port = addr.port();
    let fmt = |ip: IpAddr| match ip {
        IpAddr::V6(v6) => format!("{scheme}://[{v6}]:{port}"),
        IpAddr::V4(v4) => format!("{scheme}://{v4}:{port}"),
    };
    if addr.ip().is_unspecified() {
        let mut v = vec![];
        if let Some(lan) = tls::lan_ipv4() {
            v.push(fmt(lan));
        }
        v.push(format!("{scheme}://localhost:{port}"));
        v
    } else if addr.ip().is_loopback() {
        vec![format!("{scheme}://{}:{port}", if addr.is_ipv4() { "127.0.0.1".to_string() } else { "[::1]".into() })]
    } else {
        vec![fmt(addr.ip())]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::WebSettings;

    fn cli(web: bool) -> CliWeb {
        CliWeb { web, ..Default::default() }
    }

    #[test]
    fn listen_addresses_parse() {
        assert_eq!(parse_listen("8080"), Some("127.0.0.1:8080".parse().unwrap()));
        assert_eq!(parse_listen("0.0.0.0:7777"), Some("0.0.0.0:7777".parse().unwrap()));
        assert_eq!(parse_listen("[::1]:9"), Some("[::1]:9".parse().unwrap()));
        assert_eq!(parse_listen("localhost:81"), Some("127.0.0.1:81".parse().unwrap()));
        assert_eq!(parse_listen("run"), None);
        assert_eq!(parse_listen("example.com:80"), None);
    }

    #[test]
    fn config_rules() {
        let s = WebSettings::default();
        assert!(WebConfig::resolve(&cli(false), &s, None).unwrap().is_none(), "nothing asked, nothing started");
        let c = WebConfig::resolve(&cli(true), &s, None).unwrap().unwrap();
        assert_eq!(c.listen, Some(DEFAULT_LISTEN.parse().unwrap()));
        assert!(c.password.is_none(), "loopback without a password is fine");
        let mut all = cli(true);
        all.listen = Some("0.0.0.0:7777".into());
        let e = WebConfig::resolve(&all, &s, None).unwrap_err().to_string();
        assert!(e.contains("needs a password"), "{e}");
        assert!(WebConfig::resolve(&all, &s, Some("pw".into())).unwrap().is_some(), "env password satisfies it");
        let mut bad = cli(true);
        bad.listen = Some("nope".into());
        assert_eq!(WebConfig::resolve(&bad, &s, None).unwrap_err().to_string(), "--web: 'nope' is not host:port or port");
        let mut half = cli(true);
        half.cert = Some("a.pem".into());
        assert_eq!(WebConfig::resolve(&half, &s, None).unwrap_err().to_string(), "--web-cert and --web-key go together");
        let headless = CliWeb { headless: true, ..Default::default() };
        assert_eq!(WebConfig::resolve(&headless, &s, None).unwrap_err().to_string(), "--headless needs --web or --remote (nothing would be reachable)");
        // headless with a local listener: loopback alone is no reason to skip the password
        let hw = CliWeb { headless: true, ..cli(true) };
        assert_eq!(WebConfig::resolve(&hw, &s, None).unwrap_err().to_string(), "--headless needs a password (--web-password / MANTRA_WEB_PASSWORD): nobody is at this terminal to vouch for localhost");
        assert!(WebConfig::resolve(&hw, &s, Some("pw".into())).unwrap().is_some());
        let hr = CliWeb { headless: true, remote: Some(Some("ws://127.0.0.1:8787".into())), ..Default::default() };
        assert!(WebConfig::resolve(&hr, &s, None).unwrap().is_some(), "remote alone has no listener; its password is generated");
        // --remote-site without --remote is a mistake, not a no-op
        for c in [CliWeb { remote_site: Some("https://example.com".into()), ..cli(true) }, CliWeb { remote_site: Some("https://example.com".into()), ..Default::default() }] {
            assert_eq!(WebConfig::resolve(&c, &s, None).unwrap_err().to_string(), "--remote-site needs --remote");
        }
        let site_setting = WebSettings { remote_site: "https://example.com".into(), ..Default::default() };
        assert!(WebConfig::resolve(&cli(true), &site_setting, None).is_ok(), "a remote_site setting waits quietly for --remote");
        let remote = CliWeb { remote: Some(None), ..Default::default() };
        let c = WebConfig::resolve(&remote, &s, None).unwrap().unwrap();
        assert_eq!((c.listen, c.relay.as_deref()), (None, Some(DEFAULT_RELAY)));
        assert_eq!(c.remote_site.as_deref(), Some(DEFAULT_SITE));
        // a self-hosted relay serves its own site by default; a separate site can be named
        let own = CliWeb { remote: Some(Some("wss://relay.example.org:8787".into())), ..Default::default() };
        assert_eq!(WebConfig::resolve(&own, &s, None).unwrap().unwrap().remote_site.as_deref(), Some("https://relay.example.org:8787"));
        let split = CliWeb { remote_site: Some("https://remote.mantra.codes/".into()), ..own.clone() };
        assert_eq!(WebConfig::resolve(&split, &s, None).unwrap().unwrap().remote_site.as_deref(), Some("https://remote.mantra.codes"));
        let bad_site = CliWeb { remote_site: Some("remote.mantra.codes".into()), ..own };
        assert!(WebConfig::resolve(&bad_site, &s, None).unwrap_err().to_string().contains("--remote-site"));
        // precedence: flag > env > settings
        let st = WebSettings { password: "fromsettings".into(), ..Default::default() };
        let mut f = cli(true);
        assert_eq!(WebConfig::resolve(&f, &st, Some("fromenv".into())).unwrap().unwrap().password.as_deref(), Some("fromenv"));
        f.password = Some("fromflag".into());
        assert_eq!(WebConfig::resolve(&f, &st, Some("fromenv".into())).unwrap().unwrap().password.as_deref(), Some("fromflag"));
        assert_eq!(WebConfig::resolve(&cli(true), &st, None).unwrap().unwrap().password.as_deref(), Some("fromsettings"));
    }

    #[test]
    fn risky_setups_are_announced() {
        let s = WebSettings::default();
        let open = WebConfig::resolve(&cli(true), &s, None).unwrap().unwrap();
        assert_eq!(open.warnings(), vec!["web UI on 127.0.0.1 without a password — anything that can reach this port (port-forwards, tunnels) is trusted".to_string()]);
        assert!(WebConfig::resolve(&cli(true), &s, Some("pw".into())).unwrap().unwrap().warnings().is_empty());
        let plain = |r: &str| WebConfig::resolve(&CliWeb { remote: Some(Some(r.into())), ..Default::default() }, &s, None).unwrap().unwrap().warnings();
        assert_eq!(plain("ws://relay.lan:8787"), vec!["relay without TLS: browsers on https pages cannot reach it".to_string()]);
        assert!(plain("ws://127.0.0.1:8787").is_empty() && plain("ws://localhost:8787/").is_empty() && plain("wss://relay.lan").is_empty());
    }

    #[test]
    fn slow_connections_are_dropped_and_broadcasts_reach_joined_ones() {
        let (ctl, _rx) = mpsc::unbounded_channel();
        let reg = WebRegistryHandle::new(ctl);
        let mut a = reg.register(ConnKind::Local);
        let _b = reg.register(ConnKind::Relay);
        reg.join(a.id);
        let t: Arc<str> = "x".into();
        reg.broadcast(&t, &t);
        assert!(matches!(a.rx.try_recv(), Ok(Outbound::Text(_))));
        a.queued.store(0, Ordering::Relaxed);
        for _ in 0..MAX_QUEUED + 1 {
            reg.send_text(a.id, t.clone());
        }
        assert!(reg.kind(a.id).is_none(), "dropped once its queue overflowed");
        assert_eq!(reg.count(ConnKind::Relay), 1);
    }
}
