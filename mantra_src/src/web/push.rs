//! Web Push (design §9.2): VAPID keys, the subscription store, the note → notification table and
//! the delivery queue.
//!
//! Package A ships the store, the keys and `notification_for` for real; `Sender`'s delivery
//! (RFC 8291 aes128gcm + VAPID JWT + HTTPS POST) is package B's — the public signatures below are
//! final, only the bodies marked `B:` change.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};

/// One notification, ready to encrypt and send.
#[derive(Debug, Clone, PartialEq)]
pub struct Push {
    /// "halt" | "question" | "approval" | "review" | "done" | "failed" | "turn" | "info" | "test"
    pub kind: String,
    pub title: String,
    pub body: String,
    /// Where a tap takes the user (an SPA route).
    pub url: String,
    pub urgency_high: bool,
}

impl Push {
    /// The JSON the service worker receives: `{kind, title, body, url, tag, at}`.
    pub fn payload(&self) -> String {
        serde_json::json!({"kind": self.kind, "title": self.title, "body": self.body, "url": self.url, "tag": self.kind, "at": super::snapshot::now_ms()}).to_string()
    }
}

/// Which kinds a device wants. Defaults: everything but routine Solo turns.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(default)]
pub struct Prefs {
    pub halt: bool,
    pub question: bool,
    pub approval: bool,
    pub review: bool,
    pub done: bool,
    pub turn: bool,
    pub test: bool,
}

impl Default for Prefs {
    fn default() -> Self {
        Prefs { halt: true, question: true, approval: true, review: true, done: true, turn: false, test: true }
    }
}

impl Prefs {
    /// Whether this device wants a notification of `kind` ("failed"/"info" ride on `done`).
    pub fn wants(&self, kind: &str) -> bool {
        match kind {
            "halt" => self.halt,
            "question" => self.question,
            "approval" => self.approval,
            "review" => self.review,
            "done" | "failed" | "info" => self.done,
            "turn" => self.turn,
            "test" => true,
            _ => false,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct Subscription {
    pub endpoint: String,
    /// The browser's P-256 public key (65 bytes uncompressed, base64url).
    pub p256dh: String,
    /// 16-byte auth secret (base64url).
    pub auth: String,
    #[serde(default)]
    pub device: String,
    #[serde(default)]
    pub created_unix: u64,
    #[serde(default)]
    pub prefs: Prefs,
    /// Consecutive delivery failures (5 → the subscription is dropped).
    #[serde(default)]
    pub failures: u32,
}

/// `web/push/subscriptions.json`.
pub struct Store {
    path: Option<PathBuf>,
    subs: Vec<Subscription>,
}

impl Store {
    pub fn load(dir: &Path) -> Store {
        let path = dir.join("subscriptions.json");
        let subs = std::fs::read_to_string(&path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        Store { path: Some(path), subs }
    }

    /// A store that never touches the disk (tests).
    pub fn memory() -> Store {
        Store { path: None, subs: vec![] }
    }

    /// Insert, or replace the subscription with the same endpoint.
    pub fn upsert(&mut self, sub: Subscription) {
        self.subs.retain(|s| s.endpoint != sub.endpoint);
        self.subs.push(sub);
    }

    pub fn remove(&mut self, endpoint: &str) -> bool {
        let n = self.subs.len();
        self.subs.retain(|s| s.endpoint != endpoint);
        self.subs.len() != n
    }

    pub fn set_prefs(&mut self, endpoint: &str, prefs: Prefs) -> bool {
        match self.subs.iter_mut().find(|s| s.endpoint == endpoint) {
            Some(s) => {
                s.prefs = prefs;
                true
            }
            None => false,
        }
    }

    pub fn get_mut(&mut self, endpoint: &str) -> Option<&mut Subscription> {
        self.subs.iter_mut().find(|s| s.endpoint == endpoint)
    }

    pub fn list(&self) -> Vec<Subscription> {
        self.subs.clone()
    }

    pub fn save(&self) {
        let Some(p) = &self.path else { return };
        let body = serde_json::to_string_pretty(&self.subs).unwrap_or_else(|_| "[]".into());
        if let Err(e) = crate::config::atomic_write_restricted(p, &body) {
            crate::mlog!("web: cannot save push subscriptions: {e}");
        }
    }
}

/// The application server's VAPID identity (`web/push/vapid.json`).
#[derive(Clone)]
pub struct Vapid {
    /// Uncompressed P-256 public point, base64url — the browser's `applicationServerKey`.
    pub public_b64url: String,
    pub secret: p256::SecretKey,
}

#[derive(Serialize, Deserialize)]
struct VapidFile {
    private_key_b64url: String,
    public_key_b64url: String,
}

impl Vapid {
    pub fn load_or_create(dir: &Path) -> anyhow::Result<Vapid> {
        let path = dir.join("vapid.json");
        if let Some(f) = std::fs::read_to_string(&path).ok().and_then(|s| serde_json::from_str::<VapidFile>(&s).ok()) {
            if let Some(secret) = super::crypto::b64url_decode(&f.private_key_b64url).and_then(|b| p256::SecretKey::from_slice(&b).ok()) {
                return Ok(Vapid::from_secret(secret));
            }
            crate::mlog!("web: vapid.json unreadable — generating new push keys (devices must re-subscribe)");
        }
        let secret = p256::SecretKey::random(&mut rand::rngs::OsRng);
        let v = Vapid::from_secret(secret);
        let f = VapidFile { private_key_b64url: super::crypto::b64url_encode(&v.secret.to_bytes()), public_key_b64url: v.public_b64url.clone() };
        crate::config::atomic_write_restricted(&path, &serde_json::to_string_pretty(&f)?)?;
        Ok(v)
    }

    pub fn from_secret(secret: p256::SecretKey) -> Vapid {
        use p256::elliptic_curve::sec1::ToEncodedPoint;
        let public = secret.public_key().to_encoded_point(false);
        Vapid { public_b64url: super::crypto::b64url_encode(public.as_bytes()), secret }
    }
}

enum Job {
    Push(Push),
    Test(String, oneshot::Sender<Result<(), String>>),
}

/// The delivery queue: `enqueue` never blocks the App task; a tokio task does the network work.
#[derive(Clone)]
pub struct Sender {
    tx: mpsc::UnboundedSender<Job>,
}

impl Sender {
    pub fn start(store: Arc<Mutex<Store>>, vapid: Vapid) -> Sender {
        let (tx, mut rx) = mpsc::unbounded_channel::<Job>();
        tokio::spawn(async move {
            // B: implement delivery (RFC 8291 + VAPID over tokio-rustls), per-subscription prefs,
            // 404/410 → remove, failures counting, 10 s timeout.
            let _ = (&store, &vapid);
            while let Some(job) = rx.recv().await {
                match job {
                    Job::Push(p) => crate::mlog!("web: push {} queued (delivery not implemented yet)", p.kind),
                    Job::Test(_, reply) => {
                        let _ = reply.send(Err("push not implemented yet".into()));
                    }
                }
            }
        });
        Sender { tx }
    }

    pub fn enqueue(&self, push: Push) {
        let _ = self.tx.send(Job::Push(push));
    }

    /// Send a test notification to one subscription now; resolves with the push service's verdict.
    pub fn test(&self, endpoint: &str) -> oneshot::Receiver<Result<(), String>> {
        let (tx, rx) = oneshot::channel();
        let _ = self.tx.send(Job::Test(endpoint.to_string(), tx));
        rx
    }
}

/// Map an App note (`app.notes`, the OSC 9 texts) to a notification — design §9.2's table.
pub fn notification_for(note: &str, app: &crate::app::App) -> Option<Push> {
    let note = note.trim();
    if note.is_empty() {
        return None;
    }
    let p = |kind: &str, title: String, body: String, url: String, high: bool| Some(Push { kind: kind.into(), title, body, url, urgency_high: high });
    if let Some(rest) = note.strip_prefix("Mantra needs you:") {
        return p("halt", "Mantra needs you".into(), rest.trim().into(), "/run".into(), true);
    }
    let Some(rest) = note.strip_prefix("Mantra: ") else {
        return p("info", "Mantra".into(), note.into(), "/".into(), false);
    };
    if let Some(q) = rest.strip_prefix("the ") {
        if let Some((who, question)) = q.split_once(" has a question") {
            let question = question.trim_start_matches([' ', '—', '-', ':']).trim();
            let mut who_c = who.to_string();
            if let Some(f) = who_c.get_mut(0..1) {
                f.make_ascii_uppercase();
            }
            return p("question", format!("{who_c} has a question"), question.into(), "/run".into(), true);
        }
    }
    if let Some(name) = rest.strip_suffix(" needs approval") {
        let agent = app.agents.values().find(|a| a.name == name).map(|a| a.id);
        let body = agent.and_then(|id| app.approvals.iter().find(|x| x.agent == id)).map(|x| format!("{} {}", x.title, crate::util::trunc(x.detail.lines().next().unwrap_or(""), 100))).unwrap_or_default();
        let url = agent.map(|id| format!("/agent/{id}")).unwrap_or_else(|| "/inbox".into());
        return p("approval", format!("{name} needs approval"), body.trim().into(), url, true);
    }
    if rest.starts_with("plan ready for review") {
        let title = app.run.as_ref().and_then(|r| r.plan.as_ref()).map(|pl| pl.title.clone()).unwrap_or_default();
        return p("review", "Plan ready for review".into(), title, "/run".into(), false);
    }
    if let Some(more) = rest.strip_prefix("run complete") {
        return p("done", "Run complete".into(), more.trim_start_matches([' ', '—', '-', ':', '·']).trim().into(), "/run".into(), false);
    }
    if let Some(status) = rest.strip_prefix("Solo turn ") {
        let ok = status.trim() == "completed";
        let solo = app.solo;
        let body = solo.and_then(|s| app.agents.get(&s)).and_then(|a| a.final_message.as_ref()).map(|m| crate::util::trunc(m.trim(), 120)).unwrap_or_default();
        let url = solo.map(|s| format!("/agent/{s}")).unwrap_or_else(|| "/".into());
        return p("turn", if ok { "Solo finished".into() } else { "Solo turn failed".into() }, body, url, false);
    }
    let low = rest.to_lowercase();
    let kind = if low.contains("fail") || low.contains("stopped") || low.contains("error") { "failed" } else { "info" };
    p(kind, "Mantra".into(), rest.into(), "/".into(), false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::snapshot::tests::{add_agent, test_app};

    #[test]
    fn notes_map_to_notifications() {
        let mut app = test_app();
        let n = notification_for("Mantra needs you: the gate failed twice", &app).unwrap();
        assert_eq!((n.kind.as_str(), n.title.as_str(), n.body.as_str(), n.url.as_str(), n.urgency_high), ("halt", "Mantra needs you", "the gate failed twice", "/run", true));
        let n = notification_for("Mantra: the planner has a question — which database?", &app).unwrap();
        assert_eq!((n.kind.as_str(), n.title.as_str(), n.body.as_str()), ("question", "Planner has a question", "which database?"));
        add_agent(&mut app, 7, "p1-api");
        app.approvals.push(crate::app::Approval { agent: 7, id: serde_json::json!(1), method: "execCommandApproval".into(), title: "Run this command?".into(), detail: "$ cargo test".into(), params: serde_json::json!({}), at: std::time::Instant::now() });
        let n = notification_for("Mantra: p1-api needs approval", &app).unwrap();
        assert_eq!((n.kind.as_str(), n.url.as_str()), ("approval", "/agent/7"));
        assert!(n.body.contains("cargo test"), "{}", n.body);
        assert_eq!(notification_for("Mantra: plan ready for review", &app).unwrap().kind, "review");
        let n = notification_for("Mantra: run complete — branch mantra/x", &app).unwrap();
        assert_eq!((n.kind.as_str(), n.body.as_str()), ("done", "branch mantra/x"));
        let n = notification_for("Mantra: Solo turn failed", &app).unwrap();
        assert_eq!((n.kind.as_str(), n.title.as_str()), ("turn", "Solo turn failed"));
        assert_eq!(notification_for("Mantra: run failed: boom", &app).unwrap().kind, "failed");
        assert_eq!(notification_for("Mantra: something else", &app).unwrap().kind, "info");
        assert!(notification_for("  ", &app).is_none());
    }

    #[test]
    fn the_store_upserts_by_endpoint() {
        let mut s = Store::memory();
        let sub = |e: &str| Subscription { endpoint: e.into(), p256dh: "k".into(), auth: "a".into(), device: String::new(), created_unix: 0, prefs: Prefs::default(), failures: 0 };
        s.upsert(sub("a"));
        s.upsert(sub("b"));
        s.upsert(sub("a"));
        assert_eq!(s.list().len(), 2);
        assert!(s.set_prefs("a", Prefs { turn: true, ..Default::default() }));
        assert!(s.list().iter().find(|x| x.endpoint == "a").unwrap().prefs.turn);
        assert!(s.remove("a") && !s.remove("a"));
        assert!(!Prefs::default().wants("turn") && Prefs::default().wants("halt"));
    }

    #[test]
    fn vapid_keys_persist() {
        let dir = std::env::temp_dir().join(format!("mantra-vapid-{}", std::process::id()));
        let a = Vapid::load_or_create(&dir).unwrap();
        let b = Vapid::load_or_create(&dir).unwrap();
        assert_eq!(a.public_b64url, b.public_b64url);
        assert_eq!(crate::web::crypto::b64url_decode(&a.public_b64url).unwrap().len(), 65);
        let _ = std::fs::remove_dir_all(dir);
    }
}
