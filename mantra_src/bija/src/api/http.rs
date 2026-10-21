//! Minimal HTTP/1.1 client (SPEC §7.5): http and https over one tokio thread, `CONNECT` tunnels
//! through `HTTPS_PROXY`/`HTTP_PROXY`, roots from webpki plus `SSL_CERT_FILE`, `Content-Length`,
//! chunked and close-delimited bodies, and an SSE splitter on top.
//!
//! Hand-rolled because the whole job is "POST a JSON body, read one response, maybe as a stream":
//! a general client (hyper, reqwest) would cost more RSS, binary size and build time than the rest
//! of bija together, and 80 agents on a laptop is a design goal.
//!
//! Ownership is what makes it cancel-safe. A connection lives in exactly one place at a time: the
//! [`Pool`]'s single idle slot, an exchange in flight, or the [`Response`] being read. Dropping a
//! future or a `Response` mid-way drops the connection with it and leaves the slot empty, so the
//! next request simply dials again. A connection goes back into the slot only after its response
//! was read to the end and the server did not ask to close it. A reused connection that turns out
//! to be dead (the server timed it out while it sat idle) is replaced by one fresh dial, silently:
//! that is the keep-alive race every client has, not an API failure worth a retry.

use crate::types::{clip, ApiError, ErrorKind};
use rustls::pki_types::ServerName;
use std::fmt;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::Instant;
use tokio_rustls::TlsConnector;

/// No byte for this long on an open stream = a broken stream (SPEC §7.2), which the caller retries.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(180);
/// Response heads are a few hundred bytes; anything this large is not an HTTP server talking.
const MAX_HEAD_BYTES: usize = 64 * 1024;
/// Spare capacity reserved before each socket read.
const READ_BYTES: usize = 16 * 1024;
/// A chunk-size line with extensions, or one trailer line.
const MAX_CHUNK_LINE: usize = 4096;

fn transient(message: impl Into<String>) -> ApiError {
    ApiError::new(ErrorKind::Transient, message)
}

// ------------------------------------------------------------------------------------------ URLs

/// An `http(s)://host[:port]/path?query` URL, the only kind bija ever talks to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Url {
    pub tls: bool,
    /// Lowercased; an IPv6 literal without its brackets.
    pub host: String,
    pub port: u16,
    /// Path and query, always starting with `/`; the fragment is dropped.
    pub path: String,
    /// Percent-decoded `user:password` from the authority (proxy URLs carry credentials this way).
    pub userinfo: Option<(String, String)>,
}

impl Url {
    pub fn parse(s: &str) -> Result<Url, String> {
        let s = s.trim();
        let (scheme, rest) = s.split_once("://").ok_or_else(|| format!("`{s}` is not an http:// or https:// URL"))?;
        let tls = match scheme.to_ascii_lowercase().as_str() {
            "http" => false,
            "https" => true,
            other => return Err(format!("unsupported URL scheme `{other}://`")),
        };
        let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let (authority, tail) = rest.split_at(end);
        let tail = tail.split('#').next().unwrap_or("");
        if tail.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(format!("the URL path `{tail}` contains spaces or control characters"));
        }
        let path = match tail {
            "" => "/".to_string(),
            t if t.starts_with('?') => format!("/{t}"),
            t => t.to_string(),
        };
        let (userinfo, hostport) = match authority.rsplit_once('@') {
            Some((user, hostport)) => (Some(split_userinfo(user)), hostport),
            None => (None, authority),
        };
        let (host, port) = split_host_port(hostport, if tls { 443 } else { 80 })?;
        if host.is_empty() {
            return Err(format!("`{s}` has no host"));
        }
        if host.chars().any(|c| c.is_whitespace() || c.is_control() || matches!(c, '/' | '\\' | '@')) {
            return Err(format!("`{host}` is not a valid host name"));
        }
        Ok(Url { tls, host, port, path, userinfo })
    }

    /// A proxy variable's value: `host:port` without a scheme means a plain-HTTP proxy, as every
    /// other tool reads it. TLS to the proxy itself (`https://` proxies) and SOCKS are not spoken.
    /// An error never repeats the credentials: it ends up in `turn_done` and Mantra's log.
    pub fn parse_proxy(s: &str) -> Result<Url, String> {
        let s = s.trim();
        let full = if s.contains("://") { s.to_string() } else { format!("http://{s}") };
        let shown = redacted(&full);
        let url = match Url::parse(&full) {
            Ok(url) => url,
            Err(e) if shown == full => return Err(e),
            // Said about the redacted spelling; when only the credentials were wrong, that parses.
            Err(_) => return Err(Url::parse(&shown).err().unwrap_or_else(|| format!("`{shown}` is not a valid proxy URL (percent-encode special characters in the credentials)"))),
        };
        if url.tls {
            return Err(format!("`{shown}` is a TLS proxy, which bija does not speak; use an http:// proxy URL"));
        }
        Ok(url)
    }

    /// `host[:port]` for the `Host` header: the port only when it is not the scheme's default.
    pub fn authority(&self) -> String {
        let default = if self.tls { 443 } else { 80 };
        if self.port == default {
            bracketed(&self.host)
        } else {
            self.host_port()
        }
    }

    /// `host:port`, always with the port: the target of a `CONNECT`.
    pub fn host_port(&self) -> String {
        format!("{}:{}", bracketed(&self.host), self.port)
    }
}

/// A URL as it may be shown: the credentials (everything between `://` and the last `@`) blanked.
fn redacted(url: &str) -> String {
    match url.split_once("://").and_then(|(scheme, rest)| rest.rsplit_once('@').map(|(_, host)| (scheme, host))) {
        Some((scheme, host)) => format!("{scheme}://***@{host}"),
        None => url.to_string(),
    }
}

fn bracketed(host: &str) -> String {
    if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_string()
    }
}

fn split_userinfo(raw: &str) -> (String, String) {
    let (user, pass) = raw.split_once(':').unwrap_or((raw, ""));
    (percent_decode(user), percent_decode(pass))
}

fn split_host_port(hostport: &str, default_port: u16) -> Result<(String, u16), String> {
    let (host, port) = if let Some(inner) = hostport.strip_prefix('[') {
        let (host, after) = inner.split_once(']').ok_or_else(|| format!("`{hostport}` has an unclosed `[`"))?;
        match after {
            "" => (host, None),
            a => (host, Some(a.strip_prefix(':').ok_or_else(|| format!("unexpected `{a}` after `[{host}]`"))?)),
        }
    } else {
        match hostport.rsplit_once(':') {
            Some((h, _)) if h.contains(':') => return Err(format!("the IPv6 address in `{hostport}` needs [brackets]")),
            Some((h, p)) => (h, Some(p)),
            None => (hostport, None),
        }
    };
    let port = match port {
        None | Some("") => default_port,
        Some(p) => p.parse::<u16>().ok().filter(|&p| p != 0).ok_or_else(|| format!("`{p}` is not a valid port"))?,
    };
    Ok((host.to_ascii_lowercase(), port))
}

/// `%40` → `@`; a malformed escape is kept as written rather than rejected.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        match (bytes[i], bytes.get(i + 1).copied().and_then(hex), bytes.get(i + 2).copied().and_then(hex)) {
            (b'%', Some(h), Some(l)) => {
                out.push((h * 16 + l) as u8);
                i += 3;
            }
            (b, _, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Standard base64 with padding, for `Proxy-Authorization: Basic`.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for group in bytes.chunks(3) {
        let b = |i: usize| group.get(i).copied().unwrap_or(0) as u32;
        let n = (b(0) << 16) | (b(1) << 8) | b(2);
        let sextet = |shift: u32| ALPHABET[((n >> shift) & 63) as usize] as char;
        out.push(sextet(18));
        out.push(sextet(12));
        out.push(if group.len() > 1 { sextet(6) } else { '=' });
        out.push(if group.len() > 2 { sextet(0) } else { '=' });
    }
    out
}

// ------------------------------------------------------------------------------------ proxy env

/// The proxy variables, read once when a [`Pool`] is made. Scheme-specific like curl and Go:
/// `HTTPS_PROXY` (then `https_proxy`) for https targets, `HTTP_PROXY` (then `http_proxy`) for
/// plain ones; both tunnel with `CONNECT`.
#[derive(Debug, Clone, Default)]
pub struct ProxyEnv {
    https: Option<Result<Url, String>>,
    http: Option<Result<Url, String>>,
    no_proxy: Vec<String>,
}

impl ProxyEnv {
    pub fn from_env() -> ProxyEnv {
        ProxyEnv::from_lookup(|name| std::env::var(name).ok())
    }

    /// The same, from any lookup: tests must not mutate the process environment.
    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> ProxyEnv {
        let first = |names: &[&str]| {
            names.iter().find_map(|&name| get(name).filter(|v| !v.trim().is_empty()).map(|v| Url::parse_proxy(&v).map_err(|e| format!("{name}: {e}"))))
        };
        let no_proxy = ["NO_PROXY", "no_proxy"]
            .iter()
            .filter_map(|&name| get(name))
            .flat_map(|v| v.split(',').map(|e| e.trim().to_ascii_lowercase()).collect::<Vec<_>>())
            .filter(|e| !e.is_empty())
            .collect();
        ProxyEnv { https: first(&["HTTPS_PROXY", "https_proxy"]), http: first(&["HTTP_PROXY", "http_proxy"]), no_proxy }
    }

    /// The proxy to tunnel through for `target`; `None` dials directly. A proxy variable that does
    /// not parse is an error rather than a silent direct connection the user did not ask for.
    pub fn proxy_for(&self, target: &Url) -> Result<Option<&Url>, String> {
        if bypasses_proxy(&target.host, &self.no_proxy) {
            return Ok(None);
        }
        match if target.tls { &self.https } else { &self.http } {
            None => Ok(None),
            Some(Ok(proxy)) => Ok(Some(proxy)),
            Some(Err(e)) => Err(e.clone()),
        }
    }
}

/// Loopback and `localhost` are never proxied (a local model server behind a corporate proxy is
/// the common case); otherwise a `NO_PROXY` entry matches exactly, `.example.com`/`*.example.com`
/// match the domain and its subdomains, `*` matches everything, and `10.0.0.0/8` matches IP hosts.
fn bypasses_proxy(host: &str, no_proxy: &[String]) -> bool {
    let ip = host.parse::<IpAddr>().ok();
    if host == "localhost" || host.ends_with(".localhost") || ip.is_some_and(|ip| ip.is_loopback()) {
        return true;
    }
    no_proxy.iter().any(|entry| no_proxy_matches(entry, host, ip))
}

fn no_proxy_matches(entry: &str, host: &str, ip: Option<IpAddr>) -> bool {
    if entry == "*" {
        return true;
    }
    if let Some((net, bits)) = entry.split_once('/') {
        return ip.is_some_and(|ip| in_cidr(ip, net, bits));
    }
    let entry = strip_entry_port(entry);
    if let Some(domain) = entry.strip_prefix("*.").or_else(|| entry.strip_prefix('.')) {
        return host == domain || (host.len() > domain.len() && host.ends_with(domain) && host.as_bytes()[host.len() - domain.len() - 1] == b'.');
    }
    host == entry || (ip.is_some() && entry.parse::<IpAddr>().ok() == ip)
}

/// `host:8080` → `host`, `[::1]:80` → `::1`; a bare IPv6 address keeps all its colons.
fn strip_entry_port(entry: &str) -> &str {
    if let Some(inner) = entry.strip_prefix('[') {
        return inner.split(']').next().unwrap_or(inner);
    }
    match entry.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') && port.bytes().all(|b| b.is_ascii_digit()) => host,
        _ => entry,
    }
}

fn in_cidr(ip: IpAddr, net: &str, bits: &str) -> bool {
    let (Ok(net), Ok(bits)) = (net.trim_matches(|c| c == '[' || c == ']').parse::<IpAddr>(), bits.trim().parse::<u32>()) else {
        return false;
    };
    let same_prefix = |a: u128, b: u128, width: u32| bits <= width && (bits == 0 || (a >> (width - bits)) == (b >> (width - bits)));
    match (ip, net) {
        (IpAddr::V4(a), IpAddr::V4(n)) => same_prefix(u32::from(a) as u128, u32::from(n) as u128, 32),
        (IpAddr::V6(a), IpAddr::V6(n)) => same_prefix(u128::from(a), u128::from(n), 128),
        _ => false,
    }
}

// ------------------------------------------------------------------------------------------ TLS

/// webpki roots plus the PEM bundle in `SSL_CERT_FILE` (a TLS-inspecting proxy's CA), exactly as
/// mantra's `web/remote.rs::client_tls`; ALPN pins HTTP/1.1 so no server tries to speak h2.
fn tls_config() -> Result<Arc<rustls::ClientConfig>, String> {
    let mut roots = rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    if let Some(path) = std::env::var_os("SSL_CERT_FILE").filter(|p| !p.is_empty()) {
        use rustls::pki_types::pem::PemObject;
        use rustls::pki_types::CertificateDer;
        let shown = path.to_string_lossy();
        let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(&path).and_then(|it| it.collect()).map_err(|e| format!("cannot read SSL_CERT_FILE {shown}: {e}"))?;
        if roots.add_parsable_certificates(certs).0 == 0 {
            return Err(format!("SSL_CERT_FILE {shown} holds no usable certificate"));
        }
    }
    let mut cfg = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|e| format!("tls: {e}"))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(cfg))
}

fn tls_failure(host: &str, e: std::io::Error) -> ApiError {
    let text = e.to_string();
    let hint = if text.contains("certificate") { " — behind a TLS-inspecting proxy, point SSL_CERT_FILE at its CA bundle" } else { "" };
    transient(format!("TLS handshake with {host} failed: {text}{hint}"))
}

// ---------------------------------------------------------------------------------- connections

trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}

/// Where a connection leads; a pooled connection is reused only for the same origin.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Origin {
    tls: bool,
    host: String,
    port: u16,
}

impl Origin {
    fn of(url: &Url) -> Origin {
        Origin { tls: url.tls, host: url.host.clone(), port: url.port }
    }
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", bracketed(&self.host), self.port)
    }
}

/// The time limits of one exchange: the whole of it, and the longest silence while reading.
#[derive(Debug, Clone, Copy)]
struct Limits {
    deadline: Instant,
    total: Duration,
    idle: Duration,
}

/// Why a read produced no bytes.
enum Broken {
    Io(std::io::Error),
    TimedOut { stalled: bool },
}

impl Broken {
    fn into_error(self, origin: &Origin, lim: &Limits) -> ApiError {
        match self {
            Broken::Io(e) => transient(format!("the connection to {origin} broke: {e}")),
            Broken::TimedOut { stalled: true } => transient(format!("the stream from {origin} stalled: no data for {} s", lim.idle.as_secs())),
            Broken::TimedOut { stalled: false } => transient(format!("the request to {origin} timed out after {} s", lim.total.as_secs())),
        }
    }
}

/// One open connection plus the bytes read from it that nobody has consumed yet.
struct Conn {
    io: Box<dyn Io>,
    buf: Vec<u8>,
    origin: Origin,
}

impl Conn {
    /// Reads whatever arrives next into `buf`; `Ok(0)` is EOF. `idle` bounds the wait for this read
    /// (the stall detector), `lim.deadline` the whole exchange.
    async fn fill(&mut self, lim: &Limits, idle: Option<Duration>) -> Result<usize, Broken> {
        let now = Instant::now();
        let (limit, stalled) = match idle {
            Some(d) if now + d < lim.deadline => (now + d, true),
            _ => (lim.deadline, false),
        };
        self.buf.reserve(READ_BYTES);
        match tokio::time::timeout_at(limit, self.io.read_buf(&mut self.buf)).await {
            Ok(Ok(n)) => Ok(n),
            Ok(Err(e)) => Err(Broken::Io(e)),
            Err(_) => Err(Broken::TimedOut { stalled }),
        }
    }
}

/// How a request/response-head exchange failed. `Stale` = the connection was dead before the
/// server answered a single byte, so on a reused connection one fresh dial may simply resend.
enum Exchange {
    Stale(ApiError),
    Failed(ApiError),
}

/// A parsed status line and header block (names lowercased).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Head {
    status: u16,
    reason: String,
    http10: bool,
    headers: Vec<(String, String)>,
}

impl Head {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }
}

/// End of the header block: `(length of the head text, bytes to consume)`; tolerates bare `\n\n`.
fn head_end(buf: &[u8]) -> Option<(usize, usize)> {
    let crlf = buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| (i, i + 4));
    let lf = buf.windows(2).position(|w| w == b"\n\n").map(|i| (i, i + 2));
    match (crlf, lf) {
        (Some(a), Some(b)) => Some(if a.0 <= b.0 { a } else { b }),
        (a, b) => a.or(b),
    }
}

fn parse_head(raw: &str) -> Result<Head, String> {
    let mut lines = raw.lines();
    let status_line = lines.next().unwrap_or("");
    let mut parts = status_line.splitn(3, ' ');
    let version = parts.next().unwrap_or("");
    if !version.starts_with("HTTP/1.") {
        return Err(format!("expected an HTTP/1.x status line, got `{}`", clip(status_line, 80)));
    }
    let status = parts.next().and_then(|c| c.parse::<u16>().ok()).filter(|c| (100..1000).contains(c)).ok_or_else(|| format!("bad status line `{}`", clip(status_line, 80)))?;
    let reason = parts.next().unwrap_or("").trim().to_string();
    let mut headers: Vec<(String, String)> = Vec::new();
    for line in lines {
        if line.starts_with([' ', '\t']) {
            // obs-fold: a continuation of the previous header's value
            if let Some((_, value)) = headers.last_mut() {
                value.push(' ');
                value.push_str(line.trim());
            }
        } else if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
    }
    Ok(Head { status, reason, http10: version == "HTTP/1.0", headers })
}

/// Reads one response head. EOF or a socket error before any byte arrived is `Stale`.
async fn read_head(conn: &mut Conn, lim: &Limits, idle: Option<Duration>) -> Result<Head, Exchange> {
    loop {
        if let Some((len, consumed)) = head_end(&conn.buf) {
            let raw = String::from_utf8_lossy(&conn.buf[..len]).into_owned();
            conn.buf.drain(..consumed);
            return parse_head(&raw).map_err(|e| Exchange::Failed(ApiError::new(ErrorKind::BadRequest, format!("{} did not answer in HTTP/1.1 ({e}) — is the URL's scheme right?", conn.origin))));
        }
        if conn.buf.len() > MAX_HEAD_BYTES {
            return Err(Exchange::Failed(ApiError::new(ErrorKind::BadRequest, format!("{} sent a response head over 64 KiB", conn.origin))));
        }
        let nothing_yet = conn.buf.is_empty();
        match conn.fill(lim, idle).await {
            Ok(0) if nothing_yet => return Err(Exchange::Stale(transient(format!("{} closed the connection without answering", conn.origin)))),
            Ok(0) => return Err(Exchange::Failed(transient(format!("{} closed the connection in the middle of the response head", conn.origin)))),
            Ok(_) => {}
            Err(Broken::Io(e)) if nothing_yet => return Err(Exchange::Stale(transient(format!("the connection to {} broke: {e}", conn.origin)))),
            Err(broken) => return Err(Exchange::Failed(broken.into_error(&conn.origin, lim))),
        }
    }
}

/// Sends a request and reads its final head (skipping `1xx` interim answers).
async fn exchange(conn: &mut Conn, head: &[u8], body: &[u8], lim: &Limits, head_idle: Option<Duration>) -> Result<Head, Exchange> {
    let write = async {
        conn.io.write_all(head).await?;
        if !body.is_empty() {
            conn.io.write_all(body).await?;
        }
        conn.io.flush().await
    };
    match tokio::time::timeout_at(lim.deadline, write).await {
        Err(_) => return Err(Exchange::Failed(Broken::TimedOut { stalled: false }.into_error(&conn.origin, lim))),
        Ok(Err(e)) => return Err(Exchange::Stale(transient(format!("sending the request to {} failed: {e}", conn.origin)))),
        Ok(Ok(())) => {}
    }
    let mut interim = false;
    loop {
        let head = match read_head(conn, lim, head_idle).await {
            Err(Exchange::Stale(e)) if interim => return Err(Exchange::Failed(e)),
            other => other?,
        };
        if (100..200).contains(&head.status) && head.status != 101 {
            interim = true;
            continue;
        }
        return Ok(head);
    }
}

/// TCP (to the proxy, if any), the `CONNECT` tunnel, then TLS for https targets.
async fn establish(url: &Url, proxy: Option<&Url>, tls: Option<Arc<rustls::ClientConfig>>, lim: &Limits) -> Result<Conn, ApiError> {
    let (host, port) = proxy.map_or((url.host.as_str(), url.port), |p| (p.host.as_str(), p.port));
    let tcp = TcpStream::connect((host, port)).await.map_err(|e| transient(format!("cannot connect to {}:{port}: {e}", bracketed(host))))?;
    // Requests go out as one head write and one body write; Nagle would hold the second back.
    let _ = tcp.set_nodelay(true);
    let mut conn = Conn { io: Box::new(tcp), buf: Vec::new(), origin: Origin::of(url) };
    if let Some(proxy) = proxy {
        open_tunnel(&mut conn, proxy, url, lim).await?;
    }
    let Some(cfg) = tls else {
        return Ok(conn);
    };
    let name = ServerName::try_from(url.host.clone()).map_err(|_| ApiError::new(ErrorKind::BadRequest, format!("`{}` is not a valid TLS server name", url.host)))?;
    let Conn { io, origin, .. } = conn;
    let stream = TlsConnector::from(cfg).connect(name, io).await.map_err(|e| tls_failure(&url.host, e))?;
    Ok(Conn { io: Box::new(stream), buf: Vec::new(), origin })
}

async fn open_tunnel(conn: &mut Conn, proxy: &Url, target: &Url, lim: &Limits) -> Result<(), ApiError> {
    let to = target.host_port();
    let via = proxy.host_port();
    let mut head = format!("CONNECT {to} HTTP/1.1\r\nHost: {to}\r\n");
    if let Some((user, pass)) = &proxy.userinfo {
        head.push_str(&format!("Proxy-Authorization: Basic {}\r\n", base64(format!("{user}:{pass}").as_bytes())));
    }
    head.push_str("\r\n");
    let failed = |e: String| transient(format!("the proxy at {via} could not open a tunnel to {to}: {e}"));
    let sent = async {
        conn.io.write_all(head.as_bytes()).await?;
        conn.io.flush().await
    };
    sent.await.map_err(|e| failed(e.to_string()))?;
    let reply = match read_head(conn, lim, None).await {
        Ok(reply) => reply,
        Err(Exchange::Stale(e) | Exchange::Failed(e)) => return Err(failed(e.message)),
    };
    if !(200..300).contains(&reply.status) {
        let mut message = format!("the proxy at {via} refused to tunnel to {to}: {} {}", reply.status, reply.reason);
        let kind = if reply.status == 407 {
            message.push_str(" — check the user and password in the proxy URL");
            ErrorKind::Auth
        } else {
            super::kind_for_status(reply.status, &message)
        };
        return Err(ApiError { kind, status: Some(reply.status), message, retry_after_ms: None });
    }
    if !conn.buf.is_empty() {
        return Err(failed("it sent data before the tunnel was up".into()));
    }
    Ok(())
}

// -------------------------------------------------------------------------------- request/pool

/// One request. Header values are checked for line breaks (never echoed: one of them is a key).
pub struct HttpRequest<'a> {
    pub method: &'a str,
    pub url: &'a Url,
    pub headers: &'a [(&'a str, String)],
    pub body: &'a [u8],
    /// The answer is a stream: then the wait for its head is bounded by the idle timeout too (a
    /// non-streaming completion legitimately sends nothing until it is done).
    pub streaming: bool,
}

fn encode_head(req: &HttpRequest<'_>) -> Result<Vec<u8>, ApiError> {
    let mut head = format!("{} {} HTTP/1.1\r\nHost: {}\r\n", req.method, req.url.path, req.url.authority());
    for (name, value) in req.headers {
        let name_ok = !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        if !name_ok || value.bytes().any(|b| b == b'\r' || b == b'\n' || b == 0) {
            return Err(ApiError::new(ErrorKind::BadRequest, format!("the `{name}` header has an invalid name or value")));
        }
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    if !req.body.is_empty() || req.method == "POST" {
        head.push_str(&format!("Content-Length: {}\r\n", req.body.len()));
    }
    head.push_str("Accept-Encoding: identity\r\nConnection: keep-alive\r\n\r\n");
    Ok(head.into_bytes())
}

/// The connection factory and its one idle keep-alive slot (SPEC §7.5). One per `api::Client`.
pub struct Pool {
    pub connect_timeout: Duration,
    /// Per exchange: dial, send, and reading the whole response.
    pub total_timeout: Duration,
    /// Longest silence tolerated while reading a response.
    pub idle_timeout: Duration,
    proxy: ProxyEnv,
    tls: Option<Arc<rustls::ClientConfig>>,
    idle: Option<Conn>,
}

impl Pool {
    pub fn new(connect_timeout: Duration, total_timeout: Duration) -> Pool {
        Pool { connect_timeout, total_timeout, idle_timeout: IDLE_TIMEOUT, proxy: ProxyEnv::from_env(), tls: None, idle: None }
    }

    #[cfg(test)]
    pub fn set_proxy(&mut self, proxy: ProxyEnv) {
        self.proxy = proxy;
    }

    #[cfg(test)]
    pub fn has_idle_connection(&self) -> bool {
        self.idle.is_some()
    }

    /// Sends `req` and returns the response once its head is in; the body is read from the
    /// [`Response`]. Errors are already classified (`Transient` for anything a retry may fix).
    pub async fn send<'p>(&'p mut self, req: &HttpRequest<'_>) -> Result<Response<'p>, ApiError> {
        let lim = Limits { deadline: Instant::now() + self.total_timeout, total: self.total_timeout, idle: self.idle_timeout };
        let head = encode_head(req)?;
        let origin = Origin::of(req.url);
        let head_idle = req.streaming.then_some(self.idle_timeout);
        if let Some(mut conn) = self.idle.take().filter(|c| c.origin == origin) {
            match exchange(&mut conn, &head, req.body, &lim, head_idle).await {
                Ok(answer) => return Ok(self.respond(conn, answer, req.method, lim)),
                // The server let the idle connection go; one fresh dial resends (module docs).
                Err(Exchange::Stale(_)) => {}
                Err(Exchange::Failed(e)) => return Err(e),
            }
        }
        let mut conn = self.connect(req.url, &lim).await?;
        match exchange(&mut conn, &head, req.body, &lim, head_idle).await {
            Ok(answer) => Ok(self.respond(conn, answer, req.method, lim)),
            Err(Exchange::Stale(e) | Exchange::Failed(e)) => Err(e),
        }
    }

    async fn connect(&mut self, url: &Url, lim: &Limits) -> Result<Conn, ApiError> {
        let proxy = self.proxy.proxy_for(url).map_err(ApiError::internal)?.cloned();
        let tls = if url.tls { Some(self.tls()?) } else { None };
        let limit = lim.deadline.min(Instant::now() + self.connect_timeout);
        match tokio::time::timeout_at(limit, establish(url, proxy.as_ref(), tls, lim)).await {
            Ok(conn) => conn,
            Err(_) if limit < lim.deadline => Err(transient(format!("connecting to {} timed out after {} s", Origin::of(url), self.connect_timeout.as_secs()))),
            Err(_) => Err(Broken::TimedOut { stalled: false }.into_error(&Origin::of(url), lim)),
        }
    }

    fn tls(&mut self) -> Result<Arc<rustls::ClientConfig>, ApiError> {
        if let Some(cfg) = &self.tls {
            return Ok(cfg.clone());
        }
        let cfg = tls_config().map_err(ApiError::internal)?;
        self.tls = Some(cfg.clone());
        Ok(cfg)
    }

    fn respond(&mut self, conn: Conn, head: Head, method: &str, lim: Limits) -> Response<'_> {
        let chunked = head.header("transfer-encoding").is_some_and(|v| v.to_ascii_lowercase().contains("chunked"));
        let length = head.header("content-length").and_then(|v| v.trim().parse::<u64>().ok());
        let body = if method == "HEAD" || head.status == 204 || head.status == 304 {
            Body::Length(0)
        } else if chunked {
            Body::Chunked(ChunkedDecoder::default())
        } else if let Some(n) = length {
            Body::Length(n)
        } else {
            Body::UntilClose
        };
        let connection = head.header("connection").map(|v| v.to_ascii_lowercase()).unwrap_or_default();
        // HTTP/1.0 closes unless it says otherwise; a close-delimited body ends with the connection.
        let keep_alive = if head.http10 { connection.contains("keep-alive") } else { !connection.contains("close") };
        let reusable = keep_alive && !matches!(body, Body::UntilClose);
        Response { status: head.status, reason: head.reason, headers: head.headers, body, conn: Some(conn), pool: self, lim, reusable }
    }
}

enum Body {
    Length(u64),
    Chunked(ChunkedDecoder),
    UntilClose,
    Done,
}

/// A response whose head has arrived; the body is pulled with [`Response::chunk`]. Reading it to
/// the end hands the connection back to the pool; dropping it early closes the connection.
pub struct Response<'p> {
    pub status: u16,
    pub reason: String,
    headers: Vec<(String, String)>,
    body: Body,
    conn: Option<Conn>,
    pool: &'p mut Pool,
    lim: Limits,
    reusable: bool,
}

impl Response<'_> {
    /// A header's value; `name` is matched case-insensitively.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }

    /// The next piece of the body as it arrives, `None` at its end.
    pub async fn chunk(&mut self) -> Result<Option<Vec<u8>>, ApiError> {
        loop {
            if matches!(self.body, Body::Done | Body::Length(0)) {
                self.body = Body::Done;
                self.release();
                return Ok(None);
            }
            let Some(conn) = self.conn.as_mut() else {
                return Ok(None);
            };
            if !conn.buf.is_empty() {
                match &mut self.body {
                    Body::Length(left) => {
                        let n = (*left).min(conn.buf.len() as u64) as usize;
                        *left -= n as u64;
                        let rest = conn.buf.split_off(n);
                        return Ok(Some(std::mem::replace(&mut conn.buf, rest)));
                    }
                    Body::Chunked(decoder) => {
                        let mut out = Vec::new();
                        let used = decoder.feed(&conn.buf, &mut out).map_err(|e| transient(format!("{} sent a malformed chunked body: {e}", conn.origin)))?;
                        conn.buf.drain(..used);
                        if decoder.is_done() {
                            self.body = Body::Done;
                        }
                        if !out.is_empty() {
                            return Ok(Some(out));
                        }
                        continue;
                    }
                    Body::UntilClose => return Ok(Some(std::mem::take(&mut conn.buf))),
                    Body::Done => continue,
                }
            }
            let read = conn.fill(&self.lim, Some(self.lim.idle)).await;
            match read {
                Ok(0) => match self.body {
                    Body::UntilClose => {
                        self.reusable = false;
                        self.body = Body::Done;
                    }
                    Body::Length(left) => return Err(transient(format!("{} closed the connection with {left} bytes of the response missing", conn.origin))),
                    _ => return Err(transient(format!("{} closed the connection in the middle of the response", conn.origin))),
                },
                Ok(_) => {}
                Err(broken) => return Err(broken.into_error(&conn.origin, &self.lim)),
            }
        }
    }

    /// The whole body; more than `cap` bytes is an error (and closes the connection).
    pub async fn bytes(mut self, cap: usize) -> Result<Vec<u8>, ApiError> {
        let mut all: Vec<u8> = Vec::new();
        while let Some(piece) = self.chunk().await? {
            if all.len() + piece.len() > cap {
                return Err(ApiError::new(ErrorKind::BadRequest, format!("the response body is larger than {} KiB", cap / 1024)));
            }
            if all.is_empty() {
                all = piece;
            } else {
                all.extend_from_slice(&piece);
            }
        }
        Ok(all)
    }

    /// Reads what is left of the body (after an SSE stream's terminal event) for up to `grace`, so
    /// the connection can go back to the pool; a server that keeps it open longer loses it.
    pub async fn finish_within(mut self, grace: Duration) {
        let drain = async {
            while let Ok(Some(_)) = self.chunk().await {}
        };
        let _ = tokio::time::timeout(grace, drain).await;
    }

    fn release(&mut self) {
        if let Some(conn) = self.conn.take() {
            // Bytes past the end of the body would be a pipelined answer nobody asked for.
            if self.reusable && conn.buf.is_empty() {
                self.pool.idle = Some(conn);
            }
        }
    }
}

// ------------------------------------------------------------------------------------- chunked

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum ChunkState {
    #[default]
    Size,
    Data(u64),
    DataEnd,
    Trailers,
    Done,
}

/// `Transfer-Encoding: chunked`, decoded incrementally: input may be split anywhere, including
/// inside a size line or its CRLF. Extensions (`;name=value`) and trailers are skipped; bare `\n`
/// line ends are tolerated.
#[derive(Debug, Default)]
pub struct ChunkedDecoder {
    state: ChunkState,
    line: Vec<u8>,
}

impl ChunkedDecoder {
    pub fn is_done(&self) -> bool {
        self.state == ChunkState::Done
    }

    /// Appends the body bytes found in `input` to `out` and returns how many input bytes it used:
    /// all of them, unless the body ended inside `input` (the rest is not part of this body).
    pub fn feed(&mut self, input: &[u8], out: &mut Vec<u8>) -> Result<usize, String> {
        let mut i = 0;
        while i < input.len() {
            match self.state {
                ChunkState::Done => break,
                ChunkState::Data(left) => {
                    let n = left.min((input.len() - i) as u64) as usize;
                    out.extend_from_slice(&input[i..i + n]);
                    i += n;
                    self.state = if left == n as u64 { ChunkState::DataEnd } else { ChunkState::Data(left - n as u64) };
                }
                _ => {
                    let rest = &input[i..];
                    let newline = rest.iter().position(|&b| b == b'\n');
                    let piece = &rest[..newline.unwrap_or(rest.len())];
                    if self.line.len() + piece.len() > MAX_CHUNK_LINE {
                        return Err("a chunk header line is too long".into());
                    }
                    self.line.extend_from_slice(piece);
                    match newline {
                        None => i = input.len(),
                        Some(p) => {
                            i += p + 1;
                            let line = std::mem::take(&mut self.line);
                            self.on_line(line.strip_suffix(b"\r").unwrap_or(&line))?;
                        }
                    }
                }
            }
        }
        Ok(i)
    }

    fn on_line(&mut self, line: &[u8]) -> Result<(), String> {
        match self.state {
            ChunkState::Size => {
                let text = String::from_utf8_lossy(line);
                let size = text.split(';').next().unwrap_or("").trim();
                let n = u64::from_str_radix(size, 16).map_err(|_| format!("bad chunk size `{}`", clip(size, 20)))?;
                self.state = if n == 0 { ChunkState::Trailers } else { ChunkState::Data(n) };
            }
            ChunkState::DataEnd if !line.is_empty() => return Err("chunk data is not followed by CRLF".into()),
            ChunkState::DataEnd => self.state = ChunkState::Size,
            ChunkState::Trailers if line.is_empty() => self.state = ChunkState::Done,
            ChunkState::Trailers | ChunkState::Data(_) | ChunkState::Done => {}
        }
        Ok(())
    }
}

// ----------------------------------------------------------------------------------------- SSE

/// One server-sent event: its `event:` name (empty when none) and its `data:` lines joined by `\n`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    pub event: String,
    pub data: String,
}

/// Turns body chunks into [`SseEvent`]s. Lines are decoded only once complete, so a UTF-8
/// character split across two network reads is never mangled. `[DONE]` is ordinary data here;
/// what ends a stream is the wire layer's business.
#[derive(Debug, Default)]
pub struct SseSplitter {
    line: Vec<u8>,
    event: String,
    data: String,
    has_data: bool,
}

impl SseSplitter {
    pub fn feed(&mut self, bytes: &[u8], out: &mut Vec<SseEvent>) {
        let mut rest = bytes;
        while let Some(p) = rest.iter().position(|&b| b == b'\n') {
            if self.line.is_empty() {
                self.on_line(&rest[..p], out);
            } else {
                self.line.extend_from_slice(&rest[..p]);
                let line = std::mem::take(&mut self.line);
                self.on_line(&line, out);
            }
            rest = &rest[p + 1..];
        }
        self.line.extend_from_slice(rest);
    }

    /// At the end of the body: a last line without its newline, and an event without its blank
    /// line, still count (some servers just close after the final event).
    pub fn finish(&mut self, out: &mut Vec<SseEvent>) {
        if !self.line.is_empty() {
            let line = std::mem::take(&mut self.line);
            self.on_line(&line, out);
        }
        self.on_line(b"", out);
    }

    fn on_line(&mut self, line: &[u8], out: &mut Vec<SseEvent>) {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() {
            if self.has_data {
                out.push(SseEvent { event: std::mem::take(&mut self.event), data: std::mem::take(&mut self.data) });
            }
            self.event.clear();
            self.data.clear();
            self.has_data = false;
            return;
        }
        if line[0] == b':' {
            return;
        }
        let text = String::from_utf8_lossy(line);
        let (field, value) = match text.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (text.as_ref(), ""),
        };
        match field {
            "data" => {
                if self.has_data {
                    self.data.push('\n');
                }
                self.data.push_str(value);
                self.has_data = true;
            }
            "event" => self.event = value.to_string(),
            _ => {}
        }
    }
}

// --------------------------------------------------------------------------------- test server

/// A scripted HTTP/1.1 server on 127.0.0.1 for the api tests: every request it reads gets the next
/// canned reply, and it counts the connections it accepted (keep-alive reuse is observable).
#[cfg(test)]
pub(crate) mod test_server {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    /// What the server does with the connection after writing a reply.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum After {
        KeepOpen,
        Close,
        /// Hold the connection open without another byte (a stalled stream).
        Stall,
    }

    pub struct Reply {
        pub bytes: Vec<u8>,
        pub after: After,
    }

    pub struct Server {
        pub port: u16,
        pub connections: Arc<AtomicUsize>,
        /// Every request's head and body, in arrival order.
        pub requests: Arc<Mutex<Vec<String>>>,
    }

    impl Server {
        pub fn url(&self, path: &str) -> String {
            format!("http://127.0.0.1:{}{path}", self.port)
        }
        pub fn connection_count(&self) -> usize {
            self.connections.load(Ordering::SeqCst)
        }
        pub fn request_count(&self) -> usize {
            self.requests.lock().map(|r| r.len()).unwrap_or(0)
        }
    }

    pub async fn start(replies: Vec<Reply>) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind a loopback port");
        let port = listener.local_addr().expect("local addr").port();
        let script = Arc::new(Mutex::new(VecDeque::from(replies)));
        let connections = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (count, log) = (connections.clone(), requests.clone());
        tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                count.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(serve(sock, script.clone(), log.clone()));
            }
        });
        Server { port, connections, requests }
    }

    async fn serve(mut sock: TcpStream, script: Arc<Mutex<VecDeque<Reply>>>, log: Arc<Mutex<Vec<String>>>) {
        let mut buf: Vec<u8> = Vec::new();
        loop {
            let Some(request) = read_request(&mut sock, &mut buf).await else { return };
            log.lock().expect("log lock").push(request);
            let Some(reply) = script.lock().expect("script lock").pop_front() else { return };
            if sock.write_all(&reply.bytes).await.is_err() {
                return;
            }
            match reply.after {
                After::KeepOpen => {}
                After::Close => return,
                After::Stall => std::future::pending::<()>().await,
            }
        }
    }

    async fn read_request(sock: &mut TcpStream, buf: &mut Vec<u8>) -> Option<String> {
        loop {
            if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buf[..end]).to_string();
                let length = head.lines().find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").and_then(|v| v.trim().parse::<usize>().ok())).unwrap_or(0);
                while buf.len() < end + 4 + length {
                    let mut more = [0u8; 8192];
                    let n = sock.read(&mut more).await.ok().filter(|&n| n > 0)?;
                    buf.extend_from_slice(&more[..n]);
                }
                let body = String::from_utf8_lossy(&buf[end + 4..end + 4 + length]).to_string();
                buf.drain(..end + 4 + length);
                return Some(format!("{head}\r\n\r\n{body}"));
            }
            let mut more = [0u8; 8192];
            let n = sock.read(&mut more).await.ok().filter(|&n| n > 0)?;
            buf.extend_from_slice(&more[..n]);
        }
    }

    /// A complete response with a `Content-Length` body.
    pub fn response(status: u16, extra_headers: &str, body: &str) -> Vec<u8> {
        format!("HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{extra_headers}\r\n{body}", body.len()).into_bytes()
    }

    /// A chunked `text/event-stream` head plus one chunk per event (`data: …\n\n` each), and the
    /// terminating chunk when `complete`.
    pub fn sse(events: &[&str], complete: bool) -> Vec<u8> {
        let mut out = b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
        for event in events {
            let piece = format!("{event}\n\n");
            out.extend_from_slice(format!("{:x}\r\n{piece}\r\n", piece.len()).as_bytes());
        }
        if complete {
            out.extend_from_slice(b"0\r\n\r\n");
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::test_server::{self, After, Reply};
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> ProxyEnv {
        let pairs: Vec<(String, String)> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        ProxyEnv::from_lookup(move |name| pairs.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone()))
    }

    fn url(s: &str) -> Url {
        Url::parse(s).expect("test URL parses")
    }

    #[test]
    fn urls_parse_into_scheme_host_port_and_path() {
        let u = url("https://API.Example.com/v1");
        assert_eq!((u.tls, u.host.as_str(), u.port, u.path.as_str()), (true, "api.example.com", 443, "/v1"));
        assert_eq!(u.authority(), "api.example.com");
        assert_eq!(u.host_port(), "api.example.com:443");
        let u = url("http://127.0.0.1:11434/v1/chat/completions?x=1#frag");
        assert_eq!((u.tls, u.port, u.path.as_str()), (false, 11434, "/v1/chat/completions?x=1"));
        assert_eq!(u.authority(), "127.0.0.1:11434");
        let u = url("http://[::1]:8080");
        assert_eq!((u.host.as_str(), u.port, u.path.as_str()), ("::1", 8080, "/"));
        assert_eq!(u.authority(), "[::1]:8080");
        assert_eq!(url("http://h?q=1").path, "/?q=1");
    }

    #[test]
    fn bad_urls_are_errors_not_panics() {
        for bad in ["api.example.com/v1", "ftp://h/", "https://", "http://h:99999/", "http://h:0/", "http://::1/", "http://[::1/", "http://h/a b", "http://h:x/"] {
            assert!(Url::parse(bad).is_err(), "{bad} should not parse");
        }
    }

    #[test]
    fn proxy_urls_default_to_http_and_decode_credentials() {
        let p = Url::parse_proxy("proxy.corp:3128").expect("bare host:port");
        assert_eq!((p.tls, p.host.as_str(), p.port), (false, "proxy.corp", 3128));
        let p = Url::parse_proxy("http://us%40er:p%3Ass@10.0.0.1:8080").expect("with credentials");
        assert_eq!(p.userinfo, Some(("us@er".to_string(), "p:ss".to_string())));
        assert!(Url::parse_proxy("https://proxy:443").is_err());
        assert!(Url::parse_proxy("socks5://proxy:1080").is_err());
    }

    #[test]
    fn a_proxy_error_never_repeats_the_credentials() {
        let tls = Url::parse_proxy("https://alice:s3cret@proxy.corp:3128").unwrap_err();
        assert!(tls.contains("`https://***@proxy.corp:3128` is a TLS proxy"), "{tls}");
        // A `/` in the password cuts the authority short: the parse fails on a piece of it.
        let bad = Url::parse_proxy("http://alice:pa/ss@proxy.corp:3128").unwrap_err();
        assert!(bad.contains("***@proxy.corp:3128") && bad.contains("percent-encode"), "{bad}");
        let no_host = Url::parse_proxy("http://alice:s3cret@").unwrap_err();
        for err in [&tls, &bad, &no_host] {
            assert!(!err.contains("alice") && !err.contains("s3cret") && !err.contains("pa/ss"), "{err}");
        }
        let env = ProxyEnv::from_lookup(|k| (k == "HTTPS_PROXY").then(|| "https://alice:s3cret@proxy.corp:3128".to_string()));
        let shown = format!("{:?}", env.https);
        assert!(shown.contains("HTTPS_PROXY") && !shown.contains("s3cret"), "{shown}");
        // Without credentials the message is as it always was.
        assert!(Url::parse_proxy("https://proxy:443").unwrap_err().contains("`https://proxy:443`"));
    }

    #[test]
    fn base64_matches_the_rfc_vectors() {
        let cases = [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v"), ("foob", "Zm9vYg=="), ("fooba", "Zm9vYmE="), ("foobar", "Zm9vYmFy")];
        for (plain, encoded) in cases {
            assert_eq!(base64(plain.as_bytes()), encoded);
        }
    }

    #[test]
    fn proxy_variables_are_scheme_specific_upper_case_first() {
        let e = env(&[("HTTPS_PROXY", "http://upper:1"), ("https_proxy", "http://lower:2"), ("http_proxy", "plain:3")]);
        assert_eq!(e.proxy_for(&url("https://api.example.com")).expect("ok").map(|p| p.host.as_str()), Some("upper"));
        assert_eq!(e.proxy_for(&url("http://api.example.com")).expect("ok").map(|p| p.port), Some(3));
        let e = env(&[("HTTPS_PROXY", "  "), ("https_proxy", "http://lower:2")]);
        assert_eq!(e.proxy_for(&url("https://a.b")).expect("ok").map(|p| p.host.as_str()), Some("lower"), "an empty value is skipped");
        assert!(env(&[]).proxy_for(&url("https://a.b")).expect("ok").is_none());
        let broken = env(&[("HTTPS_PROXY", "socks5://x:1")]);
        assert!(broken.proxy_for(&url("https://a.b")).expect_err("unusable proxy").contains("HTTPS_PROXY"));
    }

    #[test]
    fn no_proxy_and_loopback_bypass_the_proxy() {
        let e = env(&[("HTTPS_PROXY", "http://proxy:1"), ("HTTP_PROXY", "http://proxy:1"), ("NO_PROXY", "internal.host, .corp.example,*.svc.local,10.0.0.0/8,fd00::/8,gw:8443"), ("no_proxy", "other.host")]);
        let direct = |u: &str| e.proxy_for(&url(u)).expect("ok").is_none();
        for u in ["https://internal.host", "https://a.corp.example", "https://corp.example", "https://x.y.svc.local", "https://10.1.2.3", "https://[fd00::1]", "https://gw", "https://other.host"] {
            assert!(direct(u), "{u} should bypass the proxy");
        }
        for u in ["http://localhost:8000", "http://127.0.0.1:11434", "http://127.9.9.9", "http://[::1]:80", "http://app.localhost"] {
            assert!(direct(u), "{u} is loopback and never proxied");
        }
        for u in ["https://notinternal.host", "https://internal.host.evil", "https://xcorp.example", "https://11.1.2.3", "https://api.example.com"] {
            assert!(!direct(u), "{u} must go through the proxy");
        }
        assert!(env(&[("HTTPS_PROXY", "http://p:1"), ("NO_PROXY", "*")]).proxy_for(&url("https://a.b")).expect("ok").is_none());
    }

    #[test]
    fn chunked_bodies_decode_with_extensions_trailers_and_bare_newlines() {
        let wire = b"4;name=value\r\nWiki\r\n5\r\npedia\r\nE\r\n in\r\n\r\nchunks.\r\n0\r\nX-Trailer: 1\r\n\r\n";
        let mut d = ChunkedDecoder::default();
        let mut out = Vec::new();
        assert_eq!(d.feed(wire, &mut out), Ok(wire.len()));
        assert!(d.is_done());
        assert_eq!(out, b"Wikipedia in\r\n\r\nchunks.");
        let mut d = ChunkedDecoder::default();
        let mut out = Vec::new();
        d.feed(b"3\nabc\n0\n\n", &mut out).expect("bare LF framing");
        assert!(d.is_done());
        assert_eq!(out, b"abc");
    }

    #[test]
    fn chunked_decoding_survives_any_split_point() {
        let wire = b"a\r\n0123456789\r\n3 ; ext\r\nxyz\r\n0\r\n\r\n";
        for split in 0..=wire.len() {
            let mut d = ChunkedDecoder::default();
            let mut out = Vec::new();
            let (a, b) = wire.split_at(split);
            assert_eq!(d.feed(a, &mut out), Ok(a.len()), "split {split}");
            assert_eq!(d.feed(b, &mut out), Ok(b.len()), "split {split}");
            assert!(d.is_done(), "split {split}");
            assert_eq!(out, b"0123456789xyz", "split {split}");
        }
        let mut d = ChunkedDecoder::default();
        let mut out = Vec::new();
        for byte in wire.iter() {
            d.feed(std::slice::from_ref(byte), &mut out).expect("byte by byte");
        }
        assert!(d.is_done());
        assert_eq!(out, b"0123456789xyz");
    }

    #[test]
    fn chunked_decoding_rejects_garbage_and_stops_at_the_end() {
        let mut out = Vec::new();
        assert!(ChunkedDecoder::default().feed(b"zz\r\n", &mut out).is_err());
        assert!(ChunkedDecoder::default().feed(b"\r\n", &mut out).is_err(), "an empty size line");
        assert!(ChunkedDecoder::default().feed(b"2\r\nabX\r\n", &mut out).is_err(), "data longer than its size");
        assert!(ChunkedDecoder::default().feed(&[b'1'; MAX_CHUNK_LINE + 1], &mut out).is_err());
        let mut d = ChunkedDecoder::default();
        assert_eq!(d.feed(b"0\r\n\r\nHTTP/1.1", &mut out), Ok(5), "bytes after the body are left alone");
    }

    fn split_all(chunks: &[&[u8]]) -> Vec<SseEvent> {
        let mut s = SseSplitter::default();
        let mut out = Vec::new();
        for c in chunks {
            s.feed(c, &mut out);
        }
        s.finish(&mut out);
        out
    }

    fn ev(event: &str, data: &str) -> SseEvent {
        SseEvent { event: event.into(), data: data.into() }
    }

    #[test]
    fn sse_events_split_on_blank_lines() {
        let stream = b"data: {\"a\":1}\n\n: keep-alive comment\n\nevent: message_start\ndata: {\"b\":2}\r\n\r\ndata: [DONE]\n\n";
        assert_eq!(split_all(&[stream]), vec![ev("", "{\"a\":1}"), ev("message_start", "{\"b\":2}"), ev("", "[DONE]")]);
    }

    #[test]
    fn sse_joins_multi_line_data_and_ignores_other_fields() {
        let stream = b"id: 7\nretry: 100\ndata: line one\ndata:line two\ndata\n\nevent: only-a-name\n\n";
        assert_eq!(split_all(&[stream]), vec![ev("", "line one\nline two\n")], "an event without data is not dispatched");
    }

    #[test]
    fn sse_survives_any_split_point_and_utf8_across_reads() {
        let stream = "event: e\r\ndata: grüße 🌱\r\n\r\ndata: x\n\n".as_bytes();
        let want = vec![ev("e", "grüße 🌱"), ev("", "x")];
        for split in 0..=stream.len() {
            let (a, b) = stream.split_at(split);
            assert_eq!(split_all(&[a, b]), want, "split {split}");
        }
    }

    #[test]
    fn sse_flushes_an_unterminated_last_event_at_the_end() {
        assert_eq!(split_all(&[b"data: {\"x\":1}\n\ndata: [DONE]"]), vec![ev("", "{\"x\":1}"), ev("", "[DONE]")]);
        assert_eq!(split_all(&[b"data: tail\n"]), vec![ev("", "tail")]);
        assert!(split_all(&[b": just a comment"]).is_empty());
    }

    #[test]
    fn heads_parse_case_insensitively_with_folded_lines() {
        let h = parse_head("HTTP/1.1 429 Too Many Requests\r\nRetry-After: 3\r\nX-Long: a\r\n  b\r\nnocolon").expect("head");
        assert_eq!((h.status, h.reason.as_str(), h.http10), (429, "Too Many Requests", false));
        assert_eq!(h.header("retry-after"), Some("3"));
        assert_eq!(h.header("x-long"), Some("a b"));
        assert!(parse_head("SSH-2.0-OpenSSH").is_err());
        assert!(parse_head("HTTP/1.1 abc").is_err());
        assert_eq!(head_end(b"HTTP/1.1 200 OK\r\n\r\nbody"), Some((15, 19)));
        assert_eq!(head_end(b"HTTP/1.1 200 OK\n\nbody"), Some((15, 17)));
    }

    fn pool() -> Pool {
        let mut p = Pool::new(Duration::from_secs(5), Duration::from_secs(10));
        p.set_proxy(ProxyEnv::default());
        p
    }

    async fn get(pool: &mut Pool, u: &str) -> Result<(u16, Vec<u8>), ApiError> {
        let target = url(u);
        let resp = pool.send(&HttpRequest { method: "GET", url: &target, headers: &[], body: b"", streaming: false }).await?;
        let status = resp.status;
        Ok((status, resp.bytes(1 << 20).await?))
    }

    #[tokio::test]
    async fn bodies_of_every_framing_are_read_and_the_connection_is_reused() {
        let server = test_server::start(vec![
            Reply { bytes: test_server::response(200, "", "{\"a\":1}"), after: After::KeepOpen },
            Reply { bytes: test_server::sse(&["data: x"], true), after: After::KeepOpen },
            Reply { bytes: b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\nuntil close".to_vec(), after: After::Close },
        ])
        .await;
        let mut p = pool();
        assert_eq!(get(&mut p, &server.url("/a")).await.expect("first"), (200, b"{\"a\":1}".to_vec()));
        assert!(p.has_idle_connection());
        assert_eq!(get(&mut p, &server.url("/b")).await.expect("second").1, b"data: x\n\n");
        assert_eq!(get(&mut p, &server.url("/c")).await.expect("third").1, b"until close");
        assert!(!p.has_idle_connection(), "a close-delimited body never goes back to the pool");
        assert_eq!(server.connection_count(), 1);
        let first = server.requests.lock().expect("log")[0].clone();
        assert!(first.starts_with("GET /a HTTP/1.1\r\nHost: 127.0.0.1:"), "{first}");
        assert!(first.contains("Accept-Encoding: identity") && first.contains("Connection: keep-alive"), "{first}");
    }

    #[tokio::test]
    async fn a_dead_idle_connection_is_replaced_by_one_fresh_dial() {
        let server = test_server::start(vec![
            Reply { bytes: test_server::response(200, "", "one"), after: After::Close },
            Reply { bytes: test_server::response(200, "", "two"), after: After::KeepOpen },
        ])
        .await;
        let mut p = pool();
        assert_eq!(get(&mut p, &server.url("/")).await.expect("first").1, b"one");
        assert!(p.has_idle_connection(), "the server did not say close, so the connection is kept");
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(get(&mut p, &server.url("/")).await.expect("second").1, b"two");
        assert_eq!(server.connection_count(), 2);
    }

    #[tokio::test]
    async fn a_stalled_stream_times_out_and_a_dropped_response_leaves_the_pool_empty() {
        let server = test_server::start(vec![Reply { bytes: b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nabc".to_vec(), after: After::Stall }]).await;
        let mut p = pool();
        p.idle_timeout = Duration::from_millis(100);
        let target = url(&server.url("/"));
        let mut resp = p.send(&HttpRequest { method: "GET", url: &target, headers: &[], body: b"", streaming: true }).await.expect("head arrives");
        assert_eq!(resp.chunk().await.expect("first bytes"), Some(b"abc".to_vec()));
        let err = resp.chunk().await.expect_err("then nothing");
        assert_eq!(err.kind, ErrorKind::Transient);
        assert!(err.message.contains("stalled"), "{}", err.message);
        drop(resp);
        assert!(!p.has_idle_connection());
    }

    #[tokio::test]
    async fn requests_tunnel_through_the_proxy_with_basic_auth() {
        // The "proxy" answers the CONNECT and then plays the origin on the same socket.
        let server = test_server::start(vec![
            Reply { bytes: b"HTTP/1.1 200 Connection established\r\n\r\n".to_vec(), after: After::KeepOpen },
            Reply { bytes: test_server::response(200, "", "through"), after: After::KeepOpen },
        ])
        .await;
        let mut p = pool();
        p.set_proxy(env(&[("HTTP_PROXY", &format!("http://user:s%3Fcret@127.0.0.1:{}", server.port))]));
        assert_eq!(get(&mut p, "http://api.example.invalid:8080/x").await.expect("tunnelled").1, b"through");
        let log = server.requests.lock().expect("log").clone();
        assert!(log[0].starts_with("CONNECT api.example.invalid:8080 HTTP/1.1"), "{}", log[0]);
        assert!(log[0].contains(&format!("Proxy-Authorization: Basic {}", base64(b"user:s?cret"))), "{}", log[0]);
        assert!(log[1].starts_with("GET /x HTTP/1.1\r\nHost: api.example.invalid:8080"), "{}", log[1]);
    }

    #[tokio::test]
    async fn a_proxy_refusal_is_classified_by_its_status() {
        let server = test_server::start(vec![Reply { bytes: b"HTTP/1.1 407 Proxy Authentication Required\r\nContent-Length: 0\r\n\r\n".to_vec(), after: After::Close }]).await;
        let mut p = pool();
        p.set_proxy(env(&[("HTTPS_PROXY", &format!("127.0.0.1:{}", server.port))]));
        let err = get(&mut p, "https://api.example.invalid/").await.expect_err("refused");
        assert_eq!((err.kind, err.status), (ErrorKind::Auth, Some(407)));
    }

    #[tokio::test]
    async fn header_values_with_line_breaks_are_refused_before_sending() {
        let mut p = pool();
        let target = url("http://127.0.0.1:9/");
        let headers = [("Authorization", "Bearer a\r\nX-Evil: 1".to_string())];
        let err = p.send(&HttpRequest { method: "POST", url: &target, headers: &headers, body: b"{}", streaming: false }).await.err().expect("refused");
        assert_eq!(err.kind, ErrorKind::BadRequest);
        assert!(!err.message.contains("Evil"), "the value is never echoed: {}", err.message);
    }

    #[tokio::test]
    async fn a_refused_connection_is_transient() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        drop(listener);
        let err = get(&mut pool(), &format!("http://127.0.0.1:{port}/")).await.expect_err("nothing listens");
        assert_eq!(err.kind, ErrorKind::Transient);
        assert!(err.message.contains("cannot connect"), "{}", err.message);
    }
}
