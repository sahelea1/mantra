//! Web Push (design §9.2): VAPID keys, the subscription store, the note → notification table, and
//! delivery — RFC 8291 `aes128gcm` content encryption + a VAPID ES256 JWT, sent as a hand-rolled
//! HTTPS POST over `tokio-rustls` (no `web-push` crate: see the design doc for why).

use serde::{Deserialize, Serialize};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use tokio_rustls::TlsConnector;

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
            // "Send a test" has no toggle in the Settings UI (the frontend never sends `test:
            // false`), but the field is honoured like any other kind rather than hardcoded true.
            "test" => self.test,
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

/// The default VAPID `sub` claim (design §9.2), used until a contact is configured.
pub const DEFAULT_CONTACT: &str = "https://mantra.codes";

/// The application server's VAPID identity (`web/push/vapid.json`).
#[derive(Clone)]
pub struct Vapid {
    /// Uncompressed P-256 public point, base64url — the browser's `applicationServerKey`.
    pub public_b64url: String,
    pub secret: p256::SecretKey,
    /// The JWT `sub` claim: a contact URI (`mailto:` or `https://`). Defaults to
    /// [`DEFAULT_CONTACT`]; set `settings.web.contact` via [`Vapid::with_contact`].
    pub contact: String,
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
        Vapid { public_b64url: super::crypto::b64url_encode(public.as_bytes()), secret, contact: DEFAULT_CONTACT.to_string() }
    }

    /// Set the VAPID JWT `sub` claim (`settings.web.contact`); blank strings keep the default.
    pub fn with_contact(mut self, contact: String) -> Vapid {
        let contact = contact.trim();
        if !contact.is_empty() {
            self.contact = contact.to_string();
        }
        self
    }
}

enum Job {
    Push(Push),
    /// A subscription was stored from a cheap syntactic check on the App task; resolve its host
    /// here (off that task) and drop it if it turns out to be private (SSRF guard, design §9.2).
    Validate(String),
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
            let connector = match tls_client_config() {
                Ok(cfg) => TlsConnector::from(cfg),
                Err(e) => {
                    crate::mlog!("web: push disabled — cannot set up TLS: {e}");
                    // Keep draining so senders never see a stuck channel; nothing can be sent.
                    while let Some(job) = rx.recv().await {
                        if let Job::Test(_, reply) = job {
                            let _ = reply.send(Err("push delivery could not start (TLS setup failed)".into()));
                        }
                    }
                    return;
                }
            };
            while let Some(job) = rx.recv().await {
                match job {
                    Job::Push(p) => deliver_push(&store, &vapid, &connector, &p).await,
                    Job::Validate(endpoint) => validate_new_subscription(&store, &endpoint).await,
                    Job::Test(endpoint, reply) => {
                        let r = deliver_test(&store, &vapid, &connector, &endpoint).await;
                        let _ = reply.send(r);
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

    /// Resolve `endpoint`'s host off the App task; drop the subscription if it is private.
    pub fn validate(&self, endpoint: &str) {
        let _ = self.tx.send(Job::Validate(endpoint.to_string()));
    }
}

// ───────────────────────────── delivery ─────────────────────────────

async fn deliver_push(store: &Arc<Mutex<Store>>, vapid: &Vapid, connector: &TlsConnector, p: &Push) {
    let subs = {
        let s = store.lock().unwrap_or_else(|e| e.into_inner());
        s.list()
    };
    if subs.is_empty() {
        return;
    }
    let payload = p.payload();
    for sub in &subs {
        if !sub.prefs.wants(&p.kind) {
            continue;
        }
        deliver_one(store, vapid, connector, sub, &payload, p).await;
    }
}

/// Send a real test notification and reply with the push service's verdict (design §9.2 point 6).
async fn deliver_test(store: &Arc<Mutex<Store>>, vapid: &Vapid, connector: &TlsConnector, endpoint: &str) -> Result<(), String> {
    let sub = {
        let s = store.lock().unwrap_or_else(|e| e.into_inner());
        s.list().into_iter().find(|s| s.endpoint == endpoint)
    };
    let sub = sub.ok_or("unknown subscription — turn notifications on again")?;
    let push = Push { kind: "test".into(), title: "Mantra".into(), body: "Test notification — if you see this, push is working.".into(), url: "/".into(), urgency_high: false };
    let body = prepare_body(push.payload().as_bytes(), &sub)?;
    let extra = [("TTL".to_string(), "3600".to_string()), ("Urgency".to_string(), "normal".to_string()), ("Topic".to_string(), "test".to_string())];
    match send_to(connector, vapid, endpoint, &extra, body).await {
        Ok(200) | Ok(201) => {
            reset_failures(store, endpoint);
            Ok(())
        }
        Ok(404) | Ok(410) => {
            remove_sub(store, endpoint, "the push service says it is gone");
            Err("the push service says this subscription no longer exists".into())
        }
        Ok(status) => {
            bump_failure(store, endpoint, &format!("status {status}"));
            Err(format!("the push service responded with status {status}"))
        }
        Err(e) => {
            bump_failure(store, endpoint, &e);
            Err(e)
        }
    }
}

/// Encrypt and send one payload to one subscription, updating the store by the result (design
/// §9.2: 200/201 ok, 404/410 remove, 429/5xx count a failure, 5 consecutive failures → remove).
async fn deliver_one(store: &Arc<Mutex<Store>>, vapid: &Vapid, connector: &TlsConnector, sub: &Subscription, payload: &str, p: &Push) {
    let body = match prepare_body(payload.as_bytes(), sub) {
        Ok(b) => b,
        Err(e) => {
            bump_failure(store, &sub.endpoint, &e);
            return;
        }
    };
    let ttl = if matches!(p.kind.as_str(), "halt" | "question" | "approval") { "86400" } else { "3600" };
    let urgency = if p.urgency_high { "high" } else { "normal" };
    let extra = [("TTL".to_string(), ttl.to_string()), ("Urgency".to_string(), urgency.to_string()), ("Topic".to_string(), p.kind.clone())];
    match send_to(connector, vapid, &sub.endpoint, &extra, body).await {
        Ok(200) | Ok(201) => reset_failures(store, &sub.endpoint),
        Ok(404) | Ok(410) => remove_sub(store, &sub.endpoint, "the push service says it is gone"),
        Ok(status) => bump_failure(store, &sub.endpoint, &format!("status {status}")),
        Err(e) => bump_failure(store, &sub.endpoint, &e),
    }
}

/// A subscription just stored from a cheap syntactic check (`push_subscribe`, App task): resolve
/// its host for real here, and drop it if that turns out to be private — see design §9.2 point 4.
async fn validate_new_subscription(store: &Arc<Mutex<Store>>, endpoint: &str) {
    let Some((host, port)) = parse_endpoint_authority(endpoint) else {
        remove_sub(store, endpoint, "not a well-formed https endpoint");
        return;
    };
    if let Err(e) = resolve_public(&host, port).await {
        remove_sub(store, endpoint, &format!("push endpoint is not public ({e})"));
    }
}

/// Decode subscription keys and encrypt one payload for it (fresh salt + ephemeral key per send).
fn prepare_body(payload: &[u8], sub: &Subscription) -> Result<Vec<u8>, String> {
    let ua_pub = super::crypto::b64url_decode(&sub.p256dh).filter(|k| k.len() == 65).ok_or("bad subscription key (p256dh)")?;
    let auth = super::crypto::b64url_decode(&sub.auth).filter(|a| a.len() == 16).ok_or("bad subscription key (auth)")?;
    let salt = super::crypto::random_bytes::<16>();
    let as_secret = p256::SecretKey::random(&mut rand::rngs::OsRng);
    encrypt_aes128gcm(payload, &ua_pub, &auth, salt, &as_secret)
}

fn bump_failure(store: &Arc<Mutex<Store>>, endpoint: &str, reason: &str) {
    let mut removed = false;
    {
        let mut s = store.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(sub) = s.get_mut(endpoint) {
            sub.failures += 1;
            removed = sub.failures >= 5;
        }
        if removed {
            s.remove(endpoint);
        }
        s.save();
    }
    if removed {
        crate::mlog!("web: push {}: {reason} — removed after 5 consecutive failures", log_id(endpoint));
    } else {
        crate::mlog!("web: push {}: {reason}", log_id(endpoint));
    }
}

fn remove_sub(store: &Arc<Mutex<Store>>, endpoint: &str, reason: &str) {
    let mut s = store.lock().unwrap_or_else(|e| e.into_inner());
    if s.remove(endpoint) {
        s.save();
        crate::mlog!("web: push subscription {} removed: {reason}", log_id(endpoint));
    }
}

fn reset_failures(store: &Arc<Mutex<Store>>, endpoint: &str) {
    let mut s = store.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(sub) = s.get_mut(endpoint) {
        if sub.failures != 0 {
            sub.failures = 0;
            s.save();
        }
    }
}

/// Host + last 6 characters only — the endpoint (and any key) must never be logged in full.
fn log_id(endpoint: &str) -> String {
    let host = parse_endpoint_authority(endpoint).map(|(h, _)| h).unwrap_or_else(|| "?".into());
    let tail: String = endpoint.chars().rev().take(6).collect::<Vec<_>>().into_iter().rev().collect();
    format!("{host} …{tail}")
}

// ───────────────────────────── RFC 8291: aes128gcm content encryption ─────────────────────────────

const KEY_INFO_PREFIX: &[u8] = b"WebPush: info\0";
const CEK_INFO: &[u8] = b"Content-Encoding: aes128gcm\0";
const NONCE_INFO: &[u8] = b"Content-Encoding: nonce\0";
const RECORD_SIZE: u32 = 4096;
/// The padding delimiter octet for a message that fits in a single record (RFC 8188 §2).
const PAD_DELIMITER: u8 = 0x02;

/// RFC 8291 `aes128gcm` content encryption. `salt` and `as_secret` (the application server's
/// ephemeral P-256 key for this one message) are parameters rather than generated inside, so the
/// RFC's Appendix A test vector can drive this function directly; the production call site
/// (`prepare_body`) passes fresh random values for both on every send.
pub fn encrypt_aes128gcm(plaintext: &[u8], ua_public: &[u8], auth: &[u8], salt: [u8; 16], as_secret: &p256::SecretKey) -> Result<Vec<u8>, String> {
    use aes_gcm::aead::{Aead, KeyInit, Payload};
    use aes_gcm::{Aes128Gcm, Nonce};
    use hkdf::Hkdf;
    use p256::elliptic_curve::sec1::ToEncodedPoint;
    use sha2::Sha256;

    if auth.len() != 16 {
        return Err("auth secret must be 16 bytes".into());
    }
    let ua_point = p256::PublicKey::from_sec1_bytes(ua_public).map_err(|_| "bad subscription key (p256dh)".to_string())?;
    let as_public = as_secret.public_key();
    let as_encoded = as_public.to_encoded_point(false);
    let as_pub = as_encoded.as_bytes();
    if as_pub.len() != 65 || ua_public.len() != 65 {
        return Err("EC public keys must be 65 bytes (uncompressed P-256)".into());
    }

    // ecdh_secret, then PRK_key/IKM (info = "WebPush: info\0" || ua_pub || as_pub, salt = auth).
    let shared = p256::ecdh::diffie_hellman(as_secret.to_nonzero_scalar(), ua_point.as_affine());
    let mut key_info = Vec::with_capacity(KEY_INFO_PREFIX.len() + 65 + 65);
    key_info.extend_from_slice(KEY_INFO_PREFIX);
    key_info.extend_from_slice(ua_public);
    key_info.extend_from_slice(as_pub);
    let mut ikm = [0u8; 32];
    Hkdf::<Sha256>::new(Some(auth), &shared.raw_secret_bytes()[..]).expand(&key_info, &mut ikm).map_err(|_| "hkdf expand failed".to_string())?;

    // CEK/NONCE from the random salt and the IKM above (RFC 8188 aes128gcm key derivation).
    let hk2 = Hkdf::<Sha256>::new(Some(&salt), &ikm);
    let mut cek = [0u8; 16];
    hk2.expand(CEK_INFO, &mut cek).map_err(|_| "hkdf expand failed".to_string())?;
    let mut nonce = [0u8; 12];
    hk2.expand(NONCE_INFO, &mut nonce).map_err(|_| "hkdf expand failed".to_string())?;

    let mut padded = Vec::with_capacity(plaintext.len() + 1);
    padded.extend_from_slice(plaintext);
    padded.push(PAD_DELIMITER);
    let cipher = Aes128Gcm::new((&cek).into());
    let ct = cipher.encrypt(&Nonce::from(nonce), Payload { msg: &padded, aad: b"" }).map_err(|_| "encryption failed".to_string())?;

    let mut body = Vec::with_capacity(16 + 4 + 1 + 65 + ct.len());
    body.extend_from_slice(&salt);
    body.extend_from_slice(&RECORD_SIZE.to_be_bytes());
    body.push(65u8);
    body.extend_from_slice(as_pub);
    body.extend_from_slice(&ct);
    Ok(body)
}

// ───────────────────────────── VAPID (ES256 JWT) ─────────────────────────────

fn b64url_json(v: &serde_json::Value) -> String {
    super::crypto::b64url_encode(v.to_string().as_bytes())
}

/// The VAPID `Authorization` header's JWT: header `{"typ":"JWT","alg":"ES256"}`, claims
/// `{aud, exp: now+12h, sub}`, signed with the VAPID key (raw `r||s`, design §9.2).
pub fn build_vapid_jwt(vapid: &Vapid, aud: &str, now_unix: u64) -> String {
    use p256::ecdsa::{signature::Signer, Signature, SigningKey};
    let header = b64url_json(&serde_json::json!({"typ": "JWT", "alg": "ES256"}));
    let claims = b64url_json(&serde_json::json!({"aud": aud, "exp": now_unix + 12 * 3600, "sub": vapid.contact}));
    let signing_input = format!("{header}.{claims}");
    let key = SigningKey::from(vapid.secret.clone());
    let sig: Signature = key.sign(signing_input.as_bytes());
    format!("{signing_input}.{}", super::crypto::b64url_encode(&sig.to_bytes()))
}

/// `vapid t=<jwt>, k=<pub>` for the `Authorization` header.
fn vapid_authorization(vapid: &Vapid, aud: &str) -> String {
    format!("vapid t={}, k={}", build_vapid_jwt(vapid, aud, crate::util::unix_secs()), vapid.public_b64url)
}

// ───────────────────────────── endpoint parsing + SSRF guard ─────────────────────────────

/// Parse an `https://host[:port]/path` endpoint's authority. `host` has brackets stripped for an
/// IPv6 literal. `None` if it isn't a well-formed https URL with a non-empty host.
fn parse_endpoint_authority(url: &str) -> Option<(String, u16)> {
    let rest = url.strip_prefix("https://")?;
    let end = rest.find('/').unwrap_or(rest.len());
    let authority = &rest[..end];
    if authority.is_empty() {
        return None;
    }
    if let Some(stripped) = authority.strip_prefix('[') {
        let (host, after) = stripped.split_once(']')?;
        if host.is_empty() {
            return None;
        }
        let port = match after.strip_prefix(':') {
            Some(p) => p.parse().ok()?,
            None if after.is_empty() => 443,
            None => return None,
        };
        return Some((host.to_string(), port));
    }
    match authority.rsplit_once(':') {
        Some((h, p)) if !h.is_empty() && !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => Some((h.to_string(), p.parse().ok()?)),
        _ => Some((authority.to_string(), 443)),
    }
}

/// The path (with query) a POST goes to; `/` if the endpoint has none.
fn endpoint_path(url: &str) -> &str {
    let rest = url.strip_prefix("https://").unwrap_or(url);
    match rest.find('/') {
        Some(i) => &rest[i..],
        None => "/",
    }
}

/// The JWT `aud` claim: `scheme://host[:port]` (`new URL(endpoint).origin`, port omitted when 443).
fn endpoint_origin(url: &str) -> Option<String> {
    let (host, port) = parse_endpoint_authority(url)?;
    let h = if host.contains(':') { format!("[{host}]") } else { host };
    Some(if port == 443 { format!("https://{h}") } else { format!("https://{h}:{port}") })
}

/// Cheap, synchronous check for `push_subscribe` (design §9.2 point 4): scheme + IP-literal
/// classification, no DNS. A hostname (not a bare IP) is accepted here — [`Sender::validate`]
/// does the full resolution off the App task and drops the subscription if it is private.
pub fn check_endpoint_syntax(url: &str) -> Result<(), String> {
    const ERR: &str = "push endpoint must be a public https URL";
    if !url.starts_with("https://") {
        return Err(ERR.into());
    }
    let Some((host, _port)) = parse_endpoint_authority(url) else {
        return Err(ERR.into());
    };
    if host.is_empty() || host.eq_ignore_ascii_case("localhost") {
        return Err(ERR.into());
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
        if !is_public_ip(ip) {
            return Err(ERR.into());
        }
    }
    Ok(())
}

/// Reject loopback, RFC1918, link-local, CGNAT, multicast and IPv6 unique-local/link-local/mapped
/// forms — anything that would let a subscription make Mantra call back into its own network.
fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4),
        IpAddr::V6(v6) => is_public_v6(v6),
    }
}

fn is_public_v4(v4: Ipv4Addr) -> bool {
    if v4.is_private() || v4.is_loopback() || v4.is_link_local() || v4.is_multicast() || v4.is_broadcast() || v4.is_unspecified() || v4.is_documentation() {
        return false;
    }
    let o = v4.octets();
    if o[0] == 100 && (64..=127).contains(&o[1]) {
        return false; // 100.64.0.0/10, carrier-grade NAT
    }
    true
}

fn is_public_v6(v6: Ipv6Addr) -> bool {
    if v6.is_loopback() || v6.is_unspecified() || v6.is_multicast() {
        return false;
    }
    if let Some(v4) = v6.to_ipv4_mapped() {
        return is_public_v4(v4);
    }
    let seg = v6.segments();
    if seg[0] & 0xfe00 == 0xfc00 {
        return false; // fc00::/7, unique local
    }
    if seg[0] & 0xffc0 == 0xfe80 {
        return false; // fe80::/10, link-local
    }
    true
}

/// DNS-resolve `host:port` and reject the result unless every address is public.
async fn resolve_public(host: &str, port: u16) -> Result<Vec<SocketAddr>, String> {
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host, port)).await.map_err(|e| format!("dns lookup failed: {e}"))?.collect();
    if addrs.is_empty() {
        return Err("dns lookup returned no addresses".into());
    }
    if let Some(bad) = addrs.iter().find(|a| !is_public_ip(a.ip())) {
        return Err(format!("resolves to a private address ({})", bad.ip()));
    }
    Ok(addrs)
}

// ───────────────────────────── hand-rolled HTTPS POST ─────────────────────────────

fn tls_client_config() -> Result<Arc<rustls::ClientConfig>, String> {
    let roots = rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let cfg = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("tls: {e}"))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Arc::new(cfg))
}

/// Encrypt, sign and POST one payload to one subscription's endpoint; `Ok` carries the HTTP status.
async fn send_to(connector: &TlsConnector, vapid: &Vapid, endpoint: &str, extra_headers: &[(String, String)], body: Vec<u8>) -> Result<u16, String> {
    let (host, port) = parse_endpoint_authority(endpoint).ok_or("not a well-formed https endpoint")?;
    let origin = endpoint_origin(endpoint).ok_or("not a well-formed https endpoint")?;
    let path = endpoint_path(endpoint);
    // SSRF guard, again, right before the send (design §9.2 point 4): a subscription that was
    // fine at subscribe time but now resolves privately (DNS rebinding, or it slipped past the
    // App task's syntactic check) is skipped and counted as a failure, never contacted.
    let addrs = resolve_public(&host, port).await?;
    let mut headers = vec![
        ("Content-Type".to_string(), "application/octet-stream".to_string()),
        ("Content-Encoding".to_string(), "aes128gcm".to_string()),
        ("Authorization".to_string(), vapid_authorization(vapid, &origin)),
    ];
    headers.extend_from_slice(extra_headers);
    let request = build_request(&host, path, &headers, &body);
    send_request(connector, &host, &addrs, &request).await
}

/// A plain HTTP/1.1 POST, `Connection: close`.
fn build_request(host: &str, path: &str, headers: &[(String, String)], body: &[u8]) -> Vec<u8> {
    let mut head = format!("POST {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nContent-Length: {}\r\n", body.len());
    for (k, v) in headers {
        head.push_str(k);
        head.push_str(": ");
        head.push_str(v);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    let mut out = head.into_bytes();
    out.extend_from_slice(body);
    out
}

/// The response's HTTP status code, from its first line.
fn parse_status_line(buf: &[u8]) -> Result<u16, String> {
    let end = buf.iter().position(|&b| b == b'\n').ok_or("empty response")?;
    let line = String::from_utf8_lossy(&buf[..end]);
    let line = line.trim_end_matches('\r');
    let mut parts = line.split_whitespace();
    let proto = parts.next().ok_or("empty response")?;
    if !proto.starts_with("HTTP/") {
        return Err(format!("not an HTTP response ({line:?})"));
    }
    let code = parts.next().ok_or("bad status line")?;
    code.parse::<u16>().map_err(|_| format!("bad status code ({code:?})"))
}

/// Connect (trying each resolved address), TLS handshake, write the request, read to EOF (the
/// server closes on `Connection: close`) and parse the status — all under one 10 s timeout.
async fn send_request(connector: &TlsConnector, host: &str, addrs: &[SocketAddr], request: &[u8]) -> Result<u16, String> {
    let work = async {
        let mut last_err = "no addresses".to_string();
        let mut tcp = None;
        for a in addrs {
            match TcpStream::connect(a).await {
                Ok(s) => {
                    tcp = Some(s);
                    break;
                }
                Err(e) => last_err = e.to_string(),
            }
        }
        let tcp = tcp.ok_or_else(|| format!("connect failed: {last_err}"))?;
        let _ = tcp.set_nodelay(true);
        let name = rustls::pki_types::ServerName::try_from(host.to_string()).map_err(|_| "bad hostname".to_string())?;
        let mut tls = connector.connect(name, tcp).await.map_err(|e| format!("tls handshake failed: {e}"))?;
        tls.write_all(request).await.map_err(|e| format!("write failed: {e}"))?;
        tls.flush().await.map_err(|e| format!("write failed: {e}"))?;
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let n = tls.read(&mut chunk).await.map_err(|e| format!("read failed: {e}"))?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.len() > 65_536 {
                break; // push service responses are tiny; this is just a guard
            }
        }
        parse_status_line(&buf)
    };
    match tokio::time::timeout(Duration::from_secs(10), work).await {
        Ok(r) => r,
        Err(_) => Err("timed out".into()),
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
    fn prefs_wants_honours_the_stored_test_toggle() {
        assert!(Prefs::default().wants("test"), "the default is on, same behaviour as before");
        assert!(!Prefs { test: false, ..Default::default() }.wants("test"), "a stored false is now honoured rather than ignored");
        assert!(!Prefs::default().wants("nonsense"));
    }

    #[test]
    fn vapid_keys_persist() {
        let dir = std::env::temp_dir().join(format!("mantra-vapid-{}", std::process::id()));
        let a = Vapid::load_or_create(&dir).unwrap();
        let b = Vapid::load_or_create(&dir).unwrap();
        assert_eq!(a.public_b64url, b.public_b64url);
        assert_eq!(crate::web::crypto::b64url_decode(&a.public_b64url).unwrap().len(), 65);
        assert_eq!(a.contact, DEFAULT_CONTACT, "unset contact falls back to the design's default");
        let c = a.with_contact("mailto:ops@example.com".into());
        assert_eq!(c.contact, "mailto:ops@example.com");
        let d = Vapid::from_secret(p256::SecretKey::random(&mut rand::rngs::OsRng)).with_contact("  ".into());
        assert_eq!(d.contact, DEFAULT_CONTACT, "a blank configured contact keeps the default");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// RFC 8291 Appendix A: with the RFC's fixed keys, salt and auth secret, our encryption must
    /// reproduce its exact ciphertext bytes.
    #[test]
    fn rfc8291_appendix_a_vector() {
        let b64 = super::super::crypto::b64url_decode;
        let plaintext = b64("V2hlbiBJIGdyb3cgdXAsIEkgd2FudCB0byBiZSBhIHdhdGVybWVsb24").unwrap();
        assert_eq!(plaintext, b"When I grow up, I want to be a watermelon");
        let as_secret = p256::SecretKey::from_slice(&b64("yfWPiYE-n46HLnH0KqZOF1fJJU3MYrct3AELtAQ-oRw").unwrap()).unwrap();
        let ua_public = b64("BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4").unwrap();
        let auth = b64("BTBZMqHH6r4Tts7J_aSIgg").unwrap();
        let salt: [u8; 16] = b64("DGv6ra1nlYgDCS1FRnbzlw").unwrap().try_into().unwrap();

        let body = encrypt_aes128gcm(&plaintext, &ua_public, &auth, salt, &as_secret).unwrap();

        let expected_header = b64("DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A8").unwrap();
        let expected_ct = b64("8pfeW0KbunFT06SuDKoJH9Ql87S1QUrdirN6GcG7sFz1y1sqLgVi1VhjVkHsUoEsbI_0LpXMuGvnzQ").unwrap();
        let mut expected = expected_header;
        expected.extend_from_slice(&expected_ct);
        assert_eq!(body, expected);
        assert_eq!(&body[..16], &salt[..]);
        assert_eq!(&body[16..20], &RECORD_SIZE.to_be_bytes());
        assert_eq!(body[20], 65);
    }

    #[test]
    fn vapid_jwt_has_the_right_shape_and_verifies() {
        use p256::ecdsa::{signature::Verifier, Signature, VerifyingKey};
        let secret = p256::SecretKey::random(&mut rand::rngs::OsRng);
        let vapid = Vapid::from_secret(secret).with_contact("mailto:ops@example.com".into());
        let jwt = build_vapid_jwt(&vapid, "https://push.example.net", 1_700_000_000);
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3, "{jwt}");
        let header: serde_json::Value = serde_json::from_slice(&super::super::crypto::b64url_decode(parts[0]).unwrap()).unwrap();
        assert_eq!(header, serde_json::json!({"typ": "JWT", "alg": "ES256"}));
        let claims: serde_json::Value = serde_json::from_slice(&super::super::crypto::b64url_decode(parts[1]).unwrap()).unwrap();
        assert_eq!(claims["aud"], "https://push.example.net");
        assert_eq!(claims["sub"], "mailto:ops@example.com");
        assert_eq!(claims["exp"], serde_json::json!(1_700_000_000u64 + 12 * 3600));
        let sig_bytes = super::super::crypto::b64url_decode(parts[2]).unwrap();
        assert_eq!(sig_bytes.len(), 64, "raw r||s, not DER");
        let vk = VerifyingKey::from(vapid.secret.public_key());
        let sig = Signature::from_slice(&sig_bytes).unwrap();
        assert!(vk.verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &sig).is_ok());
        let header_val = vapid_authorization(&vapid, "https://push.example.net");
        assert!(header_val.starts_with("vapid t="), "{header_val}");
        assert!(header_val.contains(&format!(", k={}", vapid.public_b64url)));
    }

    #[test]
    fn endpoint_parsing() {
        assert_eq!(parse_endpoint_authority("https://fcm.googleapis.com/fcm/send/xyz"), Some(("fcm.googleapis.com".to_string(), 443)));
        assert_eq!(parse_endpoint_authority("https://example.org:8443/x"), Some(("example.org".to_string(), 8443)));
        assert_eq!(parse_endpoint_authority("https://[::1]:9000/x"), Some(("::1".to_string(), 9000)));
        assert_eq!(parse_endpoint_authority("https://[::1]/x"), Some(("::1".to_string(), 443)));
        assert_eq!(parse_endpoint_authority("http://example.org/x"), None);
        assert_eq!(parse_endpoint_authority("https:///x"), None);
        assert_eq!(endpoint_path("https://example.org/fcm/send/xyz?x=1"), "/fcm/send/xyz?x=1");
        assert_eq!(endpoint_path("https://example.org"), "/");
        assert_eq!(endpoint_origin("https://example.org/x").as_deref(), Some("https://example.org"));
        assert_eq!(endpoint_origin("https://example.org:8443/x").as_deref(), Some("https://example.org:8443"));
        assert_eq!(endpoint_origin("https://[::1]:9000/x").as_deref(), Some("https://[::1]:9000"));
    }

    #[test]
    fn ssrf_classifier_rejects_private_and_accepts_public() {
        let private: &[&str] = &["127.0.0.1", "10.0.0.5", "172.16.4.4", "192.168.1.1", "169.254.1.1", "100.64.0.1", "100.127.255.255", "224.0.0.1", "255.255.255.255", "0.0.0.0", "::1", "fc00::1", "fd12:3456::1", "fe80::1", "::ffff:127.0.0.1", "::ffff:10.1.2.3"];
        for ip in private {
            assert!(!is_public_ip(ip.parse().unwrap()), "{ip} should be classified private");
        }
        let public: &[&str] = &["8.8.8.8", "1.1.1.1", "104.16.1.1", "2606:4700:4700::1111", "100.63.255.255", "100.128.0.1"];
        for ip in public {
            assert!(is_public_ip(ip.parse().unwrap()), "{ip} should be classified public");
        }
        assert!(check_endpoint_syntax("https://fcm.googleapis.com/fcm/send/xyz").is_ok());
        assert!(check_endpoint_syntax("http://fcm.googleapis.com/x").is_err(), "not https");
        assert!(check_endpoint_syntax("https://127.0.0.1/x").is_err(), "loopback literal");
        assert!(check_endpoint_syntax("https://192.168.1.1/x").is_err(), "private literal");
        assert!(check_endpoint_syntax("https://[::1]/x").is_err(), "ipv6 loopback literal");
        assert!(check_endpoint_syntax("https://localhost/x").is_err(), "localhost");
        // A hostname (not an IP literal) is only checked syntactically here; DNS happens async.
        assert!(check_endpoint_syntax("https://internal.example.corp/x").is_ok());
    }

    #[test]
    fn status_line_and_request_building() {
        assert_eq!(parse_status_line(b"HTTP/1.1 201 Created\r\nContent-Length: 0\r\n\r\n").unwrap(), 201);
        assert_eq!(parse_status_line(b"HTTP/1.1 410 Gone\r\n\r\n").unwrap(), 410);
        assert!(parse_status_line(b"garbage").is_err());
        assert!(parse_status_line(b"").is_err());

        let req = build_request("push.example.net", "/send/xyz", &[("TTL".to_string(), "3600".to_string())], b"body");
        let text = String::from_utf8_lossy(&req);
        assert!(text.starts_with("POST /send/xyz HTTP/1.1\r\n"), "{text}");
        assert!(text.contains("Host: push.example.net\r\n"));
        assert!(text.contains("Connection: close\r\n"));
        assert!(text.contains("Content-Length: 4\r\n"));
        assert!(text.contains("TTL: 3600\r\n"));
        assert!(text.ends_with("body"));
    }

    #[test]
    fn log_id_never_reveals_the_full_endpoint() {
        let id = log_id("https://fcm.googleapis.com/fcm/send/AbCdEfGhIjKlMnOpQrStUvWxYz");
        assert_eq!(id, "fcm.googleapis.com …UvWxYz");
        assert!(!id.contains("/fcm/send/"), "must not contain the path");
    }
}
