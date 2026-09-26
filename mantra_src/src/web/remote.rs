//! `mantra --remote`: reach this session through a relay, end-to-end encrypted (design §10).
//!
//! Package A owns the identity half — sid, host token, password, key, link, code and QR,
//! persisted in `web/remote.json` so a link survives restarts until it is rotated. The relay
//! networking (dial, demux, per-client E2EE handshake, reconnect) is package C's: it replaces
//! `run` below and may add private items, but keeps the public signatures.

use super::snapshot::RemoteInfo;
use super::{crypto, qr, WebRegistryHandle};
use crate::app::AppEvent;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;

#[derive(Clone, Debug)]
pub struct RemoteConfig {
    /// `wss://remote.mantra.codes` (or any ws:// / wss:// relay).
    pub relay: String,
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

pub fn link_of(relay: &str, sid: &str, key: &[u8; 32]) -> String {
    format!("{}/s/{sid}#k={}", relay_origin(relay), crypto::b64url_encode(key))
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
        let link = s.key.map(|k| link_of(&self.inner.relay, &s.sid, &k));
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

/// The relay connection. C: implement — dial `{relay}/host/{sid}` with `Authorization: Bearer
/// {host_token}` (tokio-tungstenite, rustls + webpki roots), demux `[16-byte client id][payload]`
/// frames and `open`/`close` control messages, run the §10.3 hello + `Cipher` per client, register
/// each client with `inner.web.register(ConnKind::Relay)` and feed decrypted text through
/// `super::conn::dispatch`, ping every 25 s, reconnect with backoff (1 s → 60 s, jitter) and
/// start over whenever `inner.generation` changes. Keep `set_status` current.
async fn run(inner: Arc<Inner>) {
    let mut generation = inner.generation.subscribe();
    loop {
        inner.set_status(false, 0, Some("relay client not implemented yet".into()));
        if generation.changed().await.is_err() {
            return;
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
        let link = link_of("wss://remote.mantra.codes", sid, &[0u8; 32]);
        assert_eq!(link, "https://remote.mantra.codes/s/abcdefghijklmnopqrstuvwxyz#k=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn identity_persists_and_rotates() {
        let dir = std::env::temp_dir().join(format!("mantra-remote-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (ctl, _c) = tokio::sync::mpsc::unbounded_channel();
        let reg = WebRegistryHandle::new(ctl);
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let cfg = RemoteConfig { relay: "ws://127.0.0.1:9".into(), password: None, dir: dir.clone() };
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
}
