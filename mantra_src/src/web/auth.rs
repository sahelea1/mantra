//! Who may use the local web UI (design §8). `Authenticator` is the seam a future hosted
//! deployment (mantra.codes accounts) plugs into; `PasswordAuth` is v1: one password, cookie
//! sessions persisted as SHA-256 hashes in `web/sessions.json`, and an in-memory login limiter.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const COOKIE: &str = "mantra_session";
const MAX_SESSIONS: usize = 50;
const SESSION_IDLE_SECS: u64 = 30 * 24 * 3600;
const WINDOW: Duration = Duration::from_secs(15 * 60);
const FREE_FAILURES: usize = 5;
const GLOBAL_PER_MIN: usize = 60;

#[derive(Debug, Clone, PartialEq)]
pub struct SessionToken(pub String);

#[derive(Debug, Clone, PartialEq)]
pub enum LoginError {
    /// No password configured: there is nothing to log in to (loopback mode).
    NoPassword,
    Wrong,
    /// Too many failures; try again after this many seconds.
    Locked(u64),
}

pub trait Authenticator: Send + Sync {
    fn requires_login(&self) -> bool;
    /// Rate-limited inside. `ua` is kept with the session for the user's own records.
    fn login(&self, password: &str, peer: IpAddr, ua: &str) -> Result<SessionToken, LoginError>;
    fn check(&self, token: &str) -> bool;
    fn logout(&self, token: &str);
    /// Write the session store (called off the async threads, after login/logout).
    fn persist(&self) {}
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct SessionRec {
    pub token_sha256_hex: String,
    pub created_unix: u64,
    pub last_seen_unix: u64,
    #[serde(default)]
    pub ua: String,
}

/// Failed logins per peer (sliding window) and overall (per minute).
#[derive(Default)]
pub struct Limiter {
    per_ip: HashMap<IpAddr, VecDeque<Instant>>,
    global: VecDeque<Instant>,
    global_until: Option<Instant>,
}

impl Limiter {
    /// Seconds until this peer may try again, if locked.
    pub fn locked(&mut self, ip: IpAddr, now: Instant) -> Option<u64> {
        if let Some(u) = self.global_until {
            if u > now {
                return Some((u - now).as_secs().max(1));
            }
            self.global_until = None;
        }
        let q = self.per_ip.get_mut(&ip)?;
        while q.front().map(|t| now.saturating_duration_since(*t) > WINDOW).unwrap_or(false) {
            q.pop_front();
        }
        if q.len() < FREE_FAILURES {
            return None;
        }
        let over = (q.len() - FREE_FAILURES) as u32;
        let lock = Duration::from_secs(30u64.saturating_mul(1u64 << over.min(20))).min(WINDOW);
        let last = *q.back()?;
        let until = last + lock;
        (until > now).then(|| (until - now).as_secs().max(1))
    }

    pub fn fail(&mut self, ip: IpAddr, now: Instant) {
        self.per_ip.entry(ip).or_default().push_back(now);
        self.global.push_back(now);
        while self.global.front().map(|t| now.saturating_duration_since(*t) > Duration::from_secs(60)).unwrap_or(false) {
            self.global.pop_front();
        }
        if self.global.len() >= GLOBAL_PER_MIN {
            self.global_until = Some(now + Duration::from_secs(60));
            self.global.clear();
        }
        // Bound memory against an address-spraying attacker.
        if self.per_ip.len() > 10_000 {
            self.per_ip.retain(|_, q| q.back().map(|t| now.saturating_duration_since(*t) < WINDOW).unwrap_or(false));
        }
    }

    pub fn succeed(&mut self, ip: IpAddr) {
        self.per_ip.remove(&ip);
    }
}

pub fn sha256_hex(s: &str) -> String {
    Sha256::digest(s.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

pub struct PasswordAuth {
    password: Option<String>,
    path: Option<PathBuf>,
    sessions: Mutex<Vec<SessionRec>>,
    limiter: Mutex<Limiter>,
}

impl PasswordAuth {
    /// `path` = `web/sessions.json` (None in tests: memory only).
    pub fn new(password: Option<String>, path: Option<PathBuf>) -> PasswordAuth {
        let mut sessions: Vec<SessionRec> = path.as_ref().and_then(|p| std::fs::read_to_string(p).ok()).and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        let now = crate::util::unix_secs();
        sessions.retain(|s| now.saturating_sub(s.last_seen_unix) < SESSION_IDLE_SECS);
        PasswordAuth { password: password.filter(|p| !p.is_empty()), path, sessions: Mutex::new(sessions), limiter: Mutex::new(Limiter::default()) }
    }

    fn login_at(&self, password: &str, peer: IpAddr, ua: &str, now: Instant) -> Result<SessionToken, LoginError> {
        let Some(want) = &self.password else { return Err(LoginError::NoPassword) };
        let mut lim = self.limiter.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(secs) = lim.locked(peer, now) {
            return Err(LoginError::Locked(secs));
        }
        if !super::crypto::ct_eq(password.as_bytes(), want.as_bytes()) {
            lim.fail(peer, now);
            crate::mlog!("web: login failed from {peer}");
            return Err(LoginError::Wrong);
        }
        lim.succeed(peer);
        drop(lim);
        // Always a fresh token: a session id is never chosen by the client (no fixation).
        let token = super::crypto::b64url_encode(&super::crypto::random_bytes::<32>());
        let now_s = crate::util::unix_secs();
        let mut s = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        s.push(SessionRec { token_sha256_hex: sha256_hex(&token), created_unix: now_s, last_seen_unix: now_s, ua: crate::util::trunc(ua, 120) });
        while s.len() > MAX_SESSIONS {
            // evict the least recently seen
            if let Some(i) = s.iter().enumerate().min_by_key(|(_, r)| r.last_seen_unix).map(|(i, _)| i) {
                s.remove(i);
            }
        }
        crate::mlog!("web: login from {peer}");
        Ok(SessionToken(token))
    }
}

impl Authenticator for PasswordAuth {
    fn requires_login(&self) -> bool {
        self.password.is_some()
    }

    fn login(&self, password: &str, peer: IpAddr, ua: &str) -> Result<SessionToken, LoginError> {
        self.login_at(password, peer, ua, Instant::now())
    }

    fn check(&self, token: &str) -> bool {
        if token.is_empty() {
            return false;
        }
        let h = sha256_hex(token);
        let now = crate::util::unix_secs();
        let mut s = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
        match s.iter_mut().find(|r| super::crypto::ct_eq(r.token_sha256_hex.as_bytes(), h.as_bytes())) {
            Some(r) if now.saturating_sub(r.last_seen_unix) < SESSION_IDLE_SECS => {
                r.last_seen_unix = now;
                true
            }
            _ => false,
        }
    }

    fn logout(&self, token: &str) {
        let h = sha256_hex(token);
        self.sessions.lock().unwrap_or_else(|e| e.into_inner()).retain(|r| r.token_sha256_hex != h);
    }

    fn persist(&self) {
        let Some(p) = &self.path else { return };
        let body = serde_json::to_string_pretty(&*self.sessions.lock().unwrap_or_else(|e| e.into_inner())).unwrap_or_else(|_| "[]".into());
        if let Err(e) = crate::config::atomic_write_restricted(p, &body) {
            crate::mlog!("web: cannot save sessions: {e}");
        }
    }
}

/// The session token from a request's `Cookie` header(s).
pub fn cookie_token(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get_all(axum::http::header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|kv| kv.trim().split_once('='))
        .find(|(k, _)| *k == COOKIE)
        .map(|(_, v)| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

pub fn set_cookie(token: &str, tls: bool) -> String {
    format!("{COOKIE}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age=2592000{}", if tls { "; Secure" } else { "" })
}

pub fn clear_cookie(tls: bool) -> String {
    format!("{COOKIE}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0{}", if tls { "; Secure" } else { "" })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(n: u8) -> IpAddr {
        IpAddr::from([192, 168, 1, n])
    }

    #[test]
    fn login_mints_a_token_that_checks_and_logs_out() {
        let a = PasswordAuth::new(Some("hunter2".into()), None);
        assert!(a.requires_login());
        assert_eq!(a.login("nope", ip(1), "ua"), Err(LoginError::Wrong));
        let t = a.login("hunter2", ip(1), "ua").unwrap();
        assert!(a.check(&t.0));
        assert!(!a.check("forged"));
        let t2 = a.login("hunter2", ip(1), "ua").unwrap();
        assert_ne!(t, t2, "every login is a new session");
        a.logout(&t.0);
        assert!(!a.check(&t.0));
        assert!(a.check(&t2.0));
    }

    #[test]
    fn no_password_means_nothing_to_log_in_to() {
        let a = PasswordAuth::new(None, None);
        assert!(!a.requires_login());
        assert_eq!(a.login("x", ip(1), ""), Err(LoginError::NoPassword));
    }

    #[test]
    fn five_failures_lock_the_peer_with_backoff() {
        let a = PasswordAuth::new(Some("pw".into()), None);
        let t0 = Instant::now();
        for i in 0..5 {
            assert_eq!(a.login_at("bad", ip(2), "", t0 + Duration::from_millis(i)), Err(LoginError::Wrong));
        }
        match a.login_at("pw", ip(2), "", t0 + Duration::from_secs(1)) {
            Err(LoginError::Locked(s)) => assert!((25..=30).contains(&s), "{s}"),
            other => panic!("{other:?}"),
        }
        assert!(a.login_at("pw", ip(3), "", t0 + Duration::from_secs(1)).is_ok(), "other peers are unaffected");
        assert!(a.login_at("pw", ip(2), "", t0 + Duration::from_secs(31)).is_ok(), "the lock expires");
        // doubling: 6 failures → 60 s
        let mut lim = Limiter::default();
        for _ in 0..6 {
            lim.fail(ip(4), t0);
        }
        assert_eq!(lim.locked(ip(4), t0), Some(60));
    }

    #[test]
    fn a_failure_storm_locks_everyone_for_a_minute() {
        let mut lim = Limiter::default();
        let t0 = Instant::now();
        for i in 0..60u32 {
            lim.fail(IpAddr::from([10, 0, (i / 250) as u8, (i % 250) as u8]), t0);
        }
        assert!(lim.locked(ip(9), t0).is_some());
        assert!(lim.locked(ip(9), t0 + Duration::from_secs(61)).is_none());
    }

    #[test]
    fn sessions_persist_hashed_and_expire_when_idle() {
        let dir = std::env::temp_dir().join(format!("mantra-auth-{}", std::process::id()));
        let path = dir.join("sessions.json");
        let a = PasswordAuth::new(Some("pw".into()), Some(path.clone()));
        let t = a.login("pw", ip(1), "phone").unwrap();
        a.persist();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains(&t.0), "only the hash is stored");
        let b = PasswordAuth::new(Some("pw".into()), Some(path.clone()));
        assert!(b.check(&t.0), "sessions survive a restart");
        // age it past the idle limit
        let mut recs: Vec<SessionRec> = serde_json::from_str(&raw).unwrap();
        recs[0].last_seen_unix = 0;
        std::fs::write(&path, serde_json::to_string(&recs).unwrap()).unwrap();
        let c = PasswordAuth::new(Some("pw".into()), Some(path));
        assert!(!c.check(&t.0));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn cookies_parse_from_any_position() {
        let mut h = axum::http::HeaderMap::new();
        h.insert(axum::http::header::COOKIE, "a=1; mantra_session=tok123; b=2".parse().unwrap());
        assert_eq!(cookie_token(&h).as_deref(), Some("tok123"));
        let mut h = axum::http::HeaderMap::new();
        h.insert(axum::http::header::COOKIE, "mantra_session=".parse().unwrap());
        assert_eq!(cookie_token(&h), None);
        assert!(set_cookie("t", true).ends_with("; Secure"));
        assert!(set_cookie("t", false).contains("HttpOnly; SameSite=Strict"));
    }
}
