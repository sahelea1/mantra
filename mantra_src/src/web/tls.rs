//! HTTPS for the local web UI (design §9.1).
//!
//! `--web-tls` makes a small private CA (`web/tls/ca.pem`, kept for years) and a server
//! certificate signed by it for every name this machine answers to. Phones and browsers only offer
//! "trust this" for a *CA* certificate (iOS's Certificate Trust Settings, Android's "CA
//! certificate" install), so `/cert.pem` hands out the CA; the server certificate can then be
//! re-issued when the machine's addresses change without anyone re-installing anything.

use anyhow::{Context, Result};
use rcgen::{BasicConstraints, CertificateParams, DistinguishedName, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair, KeyUsagePurpose};
use serde::{Deserialize, Serialize};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const LEAF_DAYS: u64 = 825; // Apple's maximum for a TLS server certificate
const CA_DAYS: u64 = 3650;
const RENEW_DAYS: u64 = 30;

/// What the HTTPS listener serves.
#[derive(Clone)]
pub struct TlsMaterial {
    /// Server certificate chain (PEM; leaf first).
    pub chain_pem: String,
    pub key_pem: String,
    /// The CA certificate to install on devices (`/cert.pem`); `None` for your own certificate.
    pub ca_pem: Option<String>,
    pub sans: Vec<String>,
}

#[derive(Serialize, Deserialize, Default, Debug, PartialEq)]
struct Meta {
    sans: Vec<String>,
    not_after_unix: u64,
    #[serde(default)]
    ca_cn: String,
}

/// Days since 1970-01-01 → (year, month, day), proleptic Gregorian (Hinnant's `civil_from_days`).
fn civil(days: i64) -> (i32, u8, u8) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u8;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u8;
    let y = (yoe + era * 400 + if m <= 2 { 1 } else { 0 }) as i32;
    (y, m, d)
}

/// Validity from/to unix seconds (day precision). rcgen doesn't re-export `time`'s date type, so
/// the dates are set here rather than returned.
fn set_validity(p: &mut CertificateParams, from: u64, to: u64) {
    let (y, m, d) = civil((from / 86_400) as i64);
    p.not_before = rcgen::date_time_ymd(y, m, d);
    let (y, m, d) = civil((to / 86_400) as i64);
    p.not_after = rcgen::date_time_ymd(y, m, d);
}

/// The machine's host name, best effort.
pub fn hostname() -> Option<String> {
    let from_cmd = std::process::Command::new("hostname").output().ok().filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    from_cmd.filter(|h| !h.is_empty()).or_else(|| std::fs::read_to_string("/etc/hostname").ok().map(|h| h.trim().to_string()).filter(|h| !h.is_empty()))
}

/// The primary LAN IPv4: the source address the OS would pick for an outside destination. A UDP
/// `connect` sends no packet.
pub fn lan_ipv4() -> Option<IpAddr> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("8.8.8.8:80").ok()?;
    let ip = s.local_addr().ok()?.ip();
    (!ip.is_unspecified() && !ip.is_loopback()).then_some(ip)
}

/// Every name the server certificate should cover.
pub fn discover_sans(listen: Option<SocketAddr>, extra: &[String]) -> Vec<String> {
    let mut v: Vec<String> = vec!["localhost".into(), "127.0.0.1".into(), "::1".into()];
    if let Some(h) = hostname() {
        v.push(h.clone());
        if !h.contains('.') {
            v.push(format!("{h}.local"));
        }
    }
    if let Some(l) = listen {
        if !l.ip().is_unspecified() {
            v.push(l.ip().to_string());
        }
    }
    if let Some(ip) = lan_ipv4() {
        v.push(ip.to_string());
    }
    v.extend(extra.iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()));
    let mut seen = std::collections::HashSet::new();
    v.retain(|s| seen.insert(s.to_lowercase()));
    v
}

fn ca_params(cn: &str, now: u64) -> Result<CertificateParams> {
    let mut p = CertificateParams::new(Vec::<String>::new())?;
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, cn);
    dn.push(DnType::OrganizationName, "Mantra");
    p.distinguished_name = dn;
    p.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    p.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign, KeyUsagePurpose::DigitalSignature];
    set_validity(&mut p, now.saturating_sub(86_400), now + CA_DAYS * 86_400);
    Ok(p)
}

fn write_secret(path: &Path, body: &str) -> Result<()> {
    crate::config::atomic_write_restricted(path, body).with_context(|| format!("writing {}", path.display()))
}

/// Load or (re)generate the CA + server certificate under `dir` (`web/tls/`).
pub fn self_signed(dir: &Path, sans: &[String]) -> Result<TlsMaterial> {
    self_signed_at(dir, sans, crate::util::unix_secs())
}

fn self_signed_at(dir: &Path, sans: &[String], now: u64) -> Result<TlsMaterial> {
    std::fs::create_dir_all(dir)?;
    restrict_dir(dir);
    let meta: Meta = std::fs::read_to_string(dir.join("meta.json")).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
    let (ca_path, ca_key_path, cert_path, key_path) = (dir.join("ca.pem"), dir.join("ca-key.pem"), dir.join("cert.pem"), dir.join("key.pem"));
    // The CA survives as long as it is readable: re-installing it on every device is the one
    // thing this design exists to avoid.
    let ca = match (std::fs::read_to_string(&ca_path), std::fs::read_to_string(&ca_key_path)) {
        (Ok(pem), Ok(k)) if !meta.ca_cn.is_empty() => KeyPair::from_pem(&k).ok().map(|kp| (pem, kp, meta.ca_cn.clone())),
        _ => None,
    };
    let (ca_pem, ca_key, ca_cn, ca_fresh) = match ca {
        Some((pem, kp, cn)) => (pem, kp, cn, false),
        None => {
            let kp = KeyPair::generate()?;
            let cn = format!("Mantra local CA ({})", hostname().unwrap_or_else(|| "this machine".into()));
            let cert = ca_params(&cn, now)?.self_signed(&kp)?;
            write_secret(&ca_key_path, &kp.serialize_pem())?;
            write_secret(&ca_path, &cert.pem())?;
            (cert.pem(), kp, cn, true)
        }
    };
    let fresh_enough = meta.not_after_unix > now + RENEW_DAYS * 86_400;
    let same_names = meta.sans == sans;
    if !ca_fresh && fresh_enough && same_names {
        if let (Ok(cert), Ok(key)) = (std::fs::read_to_string(&cert_path), std::fs::read_to_string(&key_path)) {
            return Ok(TlsMaterial { chain_pem: format!("{cert}{ca_pem}"), key_pem: key, ca_pem: Some(ca_pem), sans: sans.to_vec() });
        }
    }
    let issuer = Issuer::new(ca_params(&ca_cn, now)?, &ca_key);
    let kp = KeyPair::generate()?;
    let mut p = CertificateParams::new(sans.to_vec())?;
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, format!("mantra on {}", hostname().unwrap_or_else(|| "localhost".into())));
    p.distinguished_name = dn;
    p.is_ca = IsCa::NoCa;
    p.key_usages = vec![KeyUsagePurpose::DigitalSignature, KeyUsagePurpose::KeyEncipherment];
    p.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let not_after = now + LEAF_DAYS * 86_400;
    set_validity(&mut p, now.saturating_sub(86_400), not_after);
    let cert = p.signed_by(&kp, &issuer)?;
    write_secret(&key_path, &kp.serialize_pem())?;
    write_secret(&cert_path, &cert.pem())?;
    let meta = Meta { sans: sans.to_vec(), not_after_unix: not_after, ca_cn };
    write_secret(&dir.join("meta.json"), &serde_json::to_string_pretty(&meta)?)?;
    crate::mlog!("web: tls certificate for [{}]", sans.join(", "));
    Ok(TlsMaterial { chain_pem: format!("{}{ca_pem}", cert.pem()), key_pem: kp.serialize_pem(), ca_pem: Some(ca_pem), sans: sans.to_vec() })
}

/// `--web-cert/--web-key`: your own PEM files.
pub fn load(cert: &Path, key: &Path) -> Result<TlsMaterial> {
    let chain_pem = std::fs::read_to_string(cert).map_err(|e| anyhow::anyhow!("--web-cert: cannot read {}: {e}", cert.display()))?;
    let key_pem = std::fs::read_to_string(key).map_err(|e| anyhow::anyhow!("--web-cert: cannot read {}: {e}", key.display()))?;
    Ok(TlsMaterial { chain_pem, key_pem, ca_pem: None, sans: vec![] })
}

/// rustls server config (HTTP/1.1 only; WebSockets need nothing more).
pub fn server_config(m: &TlsMaterial) -> Result<Arc<rustls::ServerConfig>> {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(m.chain_pem.as_bytes()).collect::<Result<_, _>>().map_err(|e| anyhow::anyhow!("certificate PEM: {e}"))?;
    if certs.is_empty() {
        anyhow::bail!("certificate PEM holds no certificate");
    }
    let key = PrivateKeyDer::from_pem_slice(m.key_pem.as_bytes()).map_err(|e| anyhow::anyhow!("key PEM: {e}"))?;
    let mut cfg = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| anyhow::anyhow!("certificate/key: {e}"))?;
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(cfg))
}

pub fn restrict_dir(dir: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    let _ = dir;
}

pub fn tls_dir(web_dir: &Path) -> PathBuf {
    web_dir.join("tls")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates() {
        assert_eq!(civil(0), (1970, 1, 1));
        assert_eq!(civil(19_723), (2024, 1, 1));
        assert_eq!(civil(20_512), (2026, 2, 28));
        assert_eq!(civil(11_016), (2000, 2, 29));
    }

    #[test]
    fn certificates_parse_back_and_are_reissued_only_when_needed() {
        let dir = std::env::temp_dir().join(format!("mantra-tls-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let sans = vec!["localhost".to_string(), "127.0.0.1".to_string(), "mybox.local".to_string()];
        let now = crate::util::unix_secs();
        let a = self_signed_at(&dir, &sans, now).unwrap();
        assert!(server_config(&a).is_ok(), "rustls accepts the chain and key");
        assert!(a.chain_pem.matches("BEGIN CERTIFICATE").count() == 2, "leaf + CA");
        let ca = a.ca_pem.clone().unwrap();
        // same names, fresh → reused as-is
        let b = self_signed_at(&dir, &sans, now).unwrap();
        assert_eq!(a.chain_pem, b.chain_pem);
        // a new address → new leaf, same CA (devices keep trusting it)
        let mut more = sans.clone();
        more.push("192.168.1.50".into());
        let c = self_signed_at(&dir, &more, now).unwrap();
        assert_ne!(c.chain_pem, a.chain_pem);
        assert_eq!(c.ca_pem.as_deref(), Some(ca.as_str()));
        // close to expiry → renewed
        let d = self_signed_at(&dir, &more, now + (LEAF_DAYS - 10) * 86_400).unwrap();
        assert_ne!(d.chain_pem, c.chain_pem);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join("key.pem")).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}
