//! Crypto for the remote relay (design §10): key derivation, the per-connection AES-256-GCM frame
//! format, and the small encodings the link / code / QR use. Everything here must match the
//! browser's WebCrypto side (`assets/crypto.js`) bit for bit — the tests pin vectors computed with
//! WebCrypto (node 22) so a drift on either side shows up here.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use hkdf::Hkdf;
use rand::RngCore;
use sha2::Sha256;

/// PBKDF2 rounds for the link key. High on purpose: the relay operator sees the sid, so the
/// password is all that stands between a guessed sid and the session.
pub const PBKDF2_ROUNDS: u32 = 310_000;
/// HKDF `info` for the per-connection key.
pub const HKDF_INFO: &[u8] = b"mantra-remote-v1";
/// Frame types (first byte of every relay payload).
pub const T_HELLO: u8 = 0x01;
pub const T_DATA: u8 = 0x02;
pub const T_CONT: u8 = 0x03;
/// Direction bytes (nonce prefix and AAD).
pub const DIR_HOST: u8 = 0x01;
pub const DIR_CLIENT: u8 = 0x02;
/// Largest plaintext per frame; bigger messages are split into `T_CONT` fragments. Leaves room
/// under the relay's 1 MiB frame cap for the client-id prefix, header and tag.
pub const MAX_FRAGMENT: usize = 900_000;
/// A reassembled message larger than this is refused (a peer streaming fragments forever).
pub const MAX_MESSAGE: usize = 16 * 1024 * 1024;

/// The 32-byte session key: PBKDF2-HMAC-SHA256(password, "mantra-remote:" + sid, 310 000).
pub fn derive_key(password: &str, sid: &str) -> [u8; 32] {
    derive_key_rounds(password, sid, PBKDF2_ROUNDS)
}

fn derive_key_rounds(password: &str, sid: &str, rounds: u32) -> [u8; 32] {
    let mut out = [0u8; 32];
    let salt = format!("mantra-remote:{sid}");
    pbkdf2::pbkdf2_hmac::<Sha256>(password.as_bytes(), salt.as_bytes(), rounds, &mut out);
    out
}

/// Per-connection key: HKDF-SHA256(ikm = key, salt = client_random || host_random, info).
pub fn conn_key(key: &[u8; 32], cr: &[u8; 16], hr: &[u8; 16]) -> [u8; 32] {
    let mut salt = [0u8; 32];
    salt[..16].copy_from_slice(cr);
    salt[16..].copy_from_slice(hr);
    let hk = Hkdf::<Sha256>::new(Some(&salt), key);
    let mut okm = [0u8; 32];
    // 32 bytes is far below HKDF-SHA256's 8160-byte limit, so expand cannot fail.
    let _ = hk.expand(HKDF_INFO, &mut okm);
    okm
}

/// One direction pair of an E2EE connection. `dir` is the direction this side *sends* in
/// (`DIR_HOST` on the host); it receives in the other one. Counters start at 0 on both sides and
/// must advance by exactly one per frame — a replayed, dropped or reordered frame is fatal.
pub struct Cipher {
    aead: Aes256Gcm,
    dir: u8,
    send_ctr: u64,
    recv_ctr: u64,
    partial: Vec<u8>,
}

impl Cipher {
    pub fn new(conn_key: &[u8; 32], dir: u8) -> Cipher {
        Cipher { aead: Aes256Gcm::new(conn_key.into()), dir, send_ctr: 0, recv_ctr: 0, partial: vec![] }
    }

    fn nonce(dir: u8, ctr: u64) -> [u8; 12] {
        let mut n = [0u8; 12];
        n[0] = dir;
        n[4..].copy_from_slice(&ctr.to_be_bytes());
        n
    }

    /// Encrypt one message into one or more frames (`[type][u64 BE counter][ciphertext||tag]`),
    /// in the order they must be sent.
    pub fn seal(&mut self, plaintext: &[u8]) -> Vec<Vec<u8>> {
        let chunks: Vec<&[u8]> = if plaintext.is_empty() { vec![&[][..]] } else { plaintext.chunks(MAX_FRAGMENT).collect() };
        let last = chunks.len() - 1;
        let mut out = Vec::with_capacity(chunks.len());
        for (i, chunk) in chunks.into_iter().enumerate() {
            let ctr = self.send_ctr;
            self.send_ctr = self.send_ctr.saturating_add(1);
            let nonce = Self::nonce(self.dir, ctr);
            let aad = [self.dir];
            // AES-GCM encryption only fails for plaintexts beyond 64 GiB.
            let ct = self.aead.encrypt(Nonce::from_slice(&nonce), Payload { msg: chunk, aad: &aad }).unwrap_or_default();
            let mut f = Vec::with_capacity(9 + ct.len());
            f.push(if i == last { T_DATA } else { T_CONT });
            f.extend_from_slice(&ctr.to_be_bytes());
            f.extend_from_slice(&ct);
            out.push(f);
        }
        out
    }

    /// Decrypt one frame. `Ok(None)` while a fragmented message is still incomplete; any error
    /// means the connection must be closed (wrong key, tampering, replay).
    pub fn open(&mut self, frame: &[u8]) -> Result<Option<Vec<u8>>, String> {
        if frame.len() < 9 + 16 {
            return Err("frame too short".into());
        }
        let t = frame[0];
        if t != T_DATA && t != T_CONT {
            return Err(format!("unexpected frame type {t:#04x}"));
        }
        let mut c = [0u8; 8];
        c.copy_from_slice(&frame[1..9]);
        let ctr = u64::from_be_bytes(c);
        if ctr != self.recv_ctr {
            return Err(format!("frame counter {ctr}, expected {}", self.recv_ctr));
        }
        let rdir = if self.dir == DIR_HOST { DIR_CLIENT } else { DIR_HOST };
        let nonce = Self::nonce(rdir, ctr);
        let aad = [rdir];
        let pt = self.aead.decrypt(Nonce::from_slice(&nonce), Payload { msg: &frame[9..], aad: &aad }).map_err(|_| "decryption failed (wrong key or tampered frame)".to_string())?;
        self.recv_ctr = self.recv_ctr.saturating_add(1);
        if self.partial.len().saturating_add(pt.len()) > MAX_MESSAGE {
            return Err("message too large".into());
        }
        self.partial.extend_from_slice(&pt);
        if t == T_CONT {
            return Ok(None);
        }
        Ok(Some(std::mem::take(&mut self.partial)))
    }
}

/// A plaintext hello frame: `[0x01][json]`.
pub fn hello_frame(json: &str) -> Vec<u8> {
    let mut f = Vec::with_capacity(1 + json.len());
    f.push(T_HELLO);
    f.extend_from_slice(json.as_bytes());
    f
}

// ───────────────────────────── encodings ─────────────────────────────

const B32: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";

/// RFC 4648 base32, lowercase, no padding (16 bytes → 26 chars).
pub fn b32_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity((data.len() * 8).div_ceil(5));
    let (mut buf, mut bits) = (0u32, 0u32);
    for &b in data {
        buf = (buf << 8) | b as u32;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(B32[((buf >> bits) & 31) as usize] as char);
        }
        buf &= (1 << bits) - 1;
    }
    if bits > 0 {
        out.push(B32[((buf << (5 - bits)) & 31) as usize] as char);
    }
    out
}

/// Inverse of `b32_encode`; any case, no padding. `None` on a character outside the alphabet.
pub fn b32_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 5 / 8);
    let (mut buf, mut bits) = (0u32, 0u32);
    for c in s.bytes() {
        let v = match c.to_ascii_lowercase() {
            c @ b'a'..=b'z' => c - b'a',
            c @ b'2'..=b'7' => c - b'2' + 26,
            _ => return None,
        };
        buf = (buf << 5) | v as u32;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
        buf &= (1 << bits) - 1;
    }
    Some(out)
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// base64url without padding (RFC 4648 §5) — what URLs, JWTs and Web Push keys use.
pub fn b64url_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        out.push(B64[(n >> 18 & 63) as usize] as char);
        out.push(B64[(n >> 12 & 63) as usize] as char);
        if c.len() > 1 {
            out.push(B64[(n >> 6 & 63) as usize] as char);
        }
        if c.len() > 2 {
            out.push(B64[(n & 63) as usize] as char);
        }
    }
    out
}

/// Decode base64url (padding optional; standard `+/` accepted too, browsers hand out both).
pub fn b64url_decode(s: &str) -> Option<Vec<u8>> {
    let s = s.trim_end_matches('=');
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut buf, mut bits) = (0u32, 0u32);
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' | b'+' => 62,
            b'_' | b'/' => 63,
            _ => return None,
        };
        buf = (buf << 6) | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
        buf &= (1 << bits) - 1;
    }
    Some(out)
}

pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    rand::rngs::OsRng.fill_bytes(&mut b);
    b
}

/// 256 short, unambiguous words: four of them are 32 bits of entropy, stretched by PBKDF2.
const WORDS: [&str; 256] = [
    "acorn", "actor", "agent", "alarm", "album", "alpha", "amber", "angle", "apple", "apron", "arrow", "atlas",
    "attic", "autumn", "badge", "bagel", "baker", "bamboo", "banjo", "barley", "basil", "beach", "beacon", "berry",
    "bison", "blade", "blanket", "bloom", "bottle", "branch", "brave", "breeze", "brick", "bridge", "brook", "brush",
    "bubble", "bucket", "butter", "cabin", "cactus", "camel", "candle", "canoe", "canyon", "carpet", "castle", "cedar",
    "cello", "chalk", "cherry", "chess", "cider", "circle", "citrus", "clay", "cliff", "cloud", "clover", "coast",
    "cobalt", "cocoa", "comet", "copper", "coral", "cotton", "crane", "crater", "crayon", "cricket", "crown",
    "crystal", "cube", "daisy", "dance", "delta", "denim", "desert", "dolphin", "dragon", "drum", "dune", "eagle",
    "echo", "elbow", "ember", "engine", "falcon", "feather", "fern", "ferry", "fiddle", "field", "fig", "flame",
    "flint", "flute", "forest", "fossil", "fox", "galaxy", "garden", "garlic", "gecko", "ginger", "glacier", "globe",
    "gold", "grape", "gravel", "guitar", "hammer", "harbor", "harp", "hazel", "helmet", "heron", "hill", "honey",
    "horizon", "husky", "igloo", "indigo", "iris", "island", "ivory", "jacket", "jade", "jaguar", "jasmine", "jelly",
    "jewel", "jungle", "kayak", "kettle", "kiwi", "koala", "ladder", "lagoon", "lake", "lantern", "lava", "lemon",
    "lily", "lime", "linen", "lion", "llama", "lotus", "lunar", "magnet", "mango", "maple", "marble", "meadow",
    "melon", "mint", "mirror", "mocha", "moose", "mosaic", "moss", "mountain", "nectar", "needle", "nest", "noodle",
    "nova", "oak", "oasis", "ocean", "olive", "onion", "opal", "orbit", "orchid", "otter", "owl", "paddle", "panda",
    "paper", "parrot", "pasta", "peach", "pearl", "pebble", "pepper", "piano", "pilot", "pine", "planet", "plum",
    "polar", "pond", "poppy", "prism", "puffin", "pumpkin", "quartz", "quill", "rabbit", "radio", "raven", "reef",
    "ribbon", "river", "robin", "rocket", "rose", "ruby", "saddle", "saffron", "salmon", "sand", "satin", "scarf",
    "shadow", "shell", "silk", "silver", "sky", "slate", "snow", "solar", "sparrow", "spice", "spruce", "star",
    "stone", "storm", "sugar", "summit", "sun", "swan", "teal", "thunder", "tiger", "timber", "toast", "topaz",
    "torch", "tulip", "tundra", "turtle", "valley", "velvet", "violet", "violin", "walnut", "willow", "window",
    "winter", "wolf", "yarrow", "zebra", "zephyr",
];

/// A generated remote password, e.g. `amber-kite-river-nine` (four words, `-`-joined).
pub fn gen_password() -> String {
    let b = random_bytes::<4>();
    b.iter().map(|i| WORDS[*i as usize]).collect::<Vec<_>>().join("-")
}

/// Constant-time byte comparison (content never short-circuits; only the length may).
pub fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    use subtle::ConstantTimeEq;
    a.len() == b.len() && bool::from(a.ct_eq(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    fn unhex(s: &str) -> Vec<u8> {
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    #[test]
    fn pbkdf2_known_answers() {
        // PBKDF2-HMAC-SHA256 "password"/"salt" (the RFC 6070 inputs, SHA-256 variant).
        let mut out = [0u8; 32];
        pbkdf2::pbkdf2_hmac::<Sha256>(b"password", b"salt", 1, &mut out);
        assert_eq!(hex(&out), "120fb6cffcf8b32c43e7225256c4f837a86548c92ccc35480805987cb70be17b");
        pbkdf2::pbkdf2_hmac::<Sha256>(b"password", b"salt", 4096, &mut out);
        assert_eq!(hex(&out), "c5e478d59288c841aa530db6845c4c8d962893a001ce4e11a4963873aa98134a");
        // The real derivation, pinned against WebCrypto's PBKDF2 (node 22).
        assert_eq!(hex(&derive_key("amber-kite-river-nine", "abcdefghijklmnopqrstuvwxyz")), "3c793c2b885f4ecc1d19656b5355fd766a404b666bf8c99d43d159f642bf486f");
        assert_eq!(derive_key_rounds("pw", "sid", 2), derive_key_rounds("pw", "sid", 2));
        assert_ne!(derive_key_rounds("pw", "sid", 2), derive_key_rounds("pw", "sie", 2));
    }

    #[test]
    fn hkdf_rfc5869_case_1() {
        let ikm = [0x0bu8; 22];
        let salt = unhex("000102030405060708090a0b0c");
        let info = unhex("f0f1f2f3f4f5f6f7f8f9");
        let mut okm = [0u8; 42];
        Hkdf::<Sha256>::new(Some(&salt), &ikm).expand(&info, &mut okm).unwrap();
        assert_eq!(hex(&okm), "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865");
    }

    #[test]
    fn conn_key_and_first_frame_match_webcrypto() {
        let key: [u8; 32] = core::array::from_fn(|i| i as u8);
        let ck = conn_key(&key, &[0xc1; 16], &[0x4a; 16]);
        assert_eq!(hex(&ck), "848e9338461cf84b6b46928380127f8a90047f33411ba2d307171c4102133b72");
        let mut host = Cipher::new(&ck, DIR_HOST);
        let frames = host.seal(br#"{"t":"pong"}"#);
        assert_eq!(frames.len(), 1);
        assert_eq!(hex(&frames[0]), "020000000000000000092d30d30a61e92b19a63373b9aa1b882eb37cd2c35d18be75dcffe8");
    }

    #[test]
    fn frames_round_trip_and_tampering_is_fatal() {
        let ck = [7u8; 32];
        let mut host = Cipher::new(&ck, DIR_HOST);
        let mut client = Cipher::new(&ck, DIR_CLIENT);
        for msg in [&b"hello"[..], b"", b"second"] {
            let f = host.seal(msg);
            assert_eq!(client.open(&f[0]).unwrap().as_deref(), Some(msg));
        }
        let f = client.seal(b"up");
        assert_eq!(host.open(&f[0]).unwrap().as_deref(), Some(&b"up"[..]));
        // tamper
        let mut f = host.seal(b"x");
        let n = f[0].len();
        f[0][n - 1] ^= 1;
        assert!(client.open(&f[0]).is_err());
        // own direction can't be reflected back
        let mut a = Cipher::new(&ck, DIR_HOST);
        let mut b = Cipher::new(&ck, DIR_HOST);
        assert!(b.open(&a.seal(b"echo")[0]).is_err());
    }

    #[test]
    fn counters_must_advance_by_one() {
        let ck = [9u8; 32];
        let mut host = Cipher::new(&ck, DIR_HOST);
        let mut client = Cipher::new(&ck, DIR_CLIENT);
        let f0 = host.seal(b"a");
        let f1 = host.seal(b"b");
        assert!(client.open(&f1[0]).unwrap_err().contains("counter"), "skipping a frame is refused");
        let mut client = Cipher::new(&ck, DIR_CLIENT);
        client.open(&f0[0]).unwrap();
        assert!(client.open(&f0[0]).is_err(), "a replay is refused");
    }

    #[test]
    fn large_messages_are_fragmented_and_reassembled() {
        let ck = [3u8; 32];
        let mut host = Cipher::new(&ck, DIR_HOST);
        let mut client = Cipher::new(&ck, DIR_CLIENT);
        let big: Vec<u8> = (0..(MAX_FRAGMENT * 2 + 10)).map(|i| (i % 251) as u8).collect();
        let frames = host.seal(&big);
        assert_eq!(frames.len(), 3);
        assert_eq!(frames.iter().map(|f| f[0]).collect::<Vec<_>>(), vec![T_CONT, T_CONT, T_DATA]);
        assert!(frames.iter().all(|f| f.len() < 1 << 20));
        assert_eq!(client.open(&frames[0]).unwrap(), None);
        assert_eq!(client.open(&frames[1]).unwrap(), None);
        assert_eq!(client.open(&frames[2]).unwrap(), Some(big));
    }

    #[test]
    fn encodings_round_trip() {
        for n in 0..40usize {
            let data: Vec<u8> = (0..n).map(|i| (i * 37 + 11) as u8).collect();
            assert_eq!(b32_decode(&b32_encode(&data)).unwrap(), data);
            assert_eq!(b64url_decode(&b64url_encode(&data)).unwrap(), data);
        }
        assert_eq!(b32_encode(&[0u8; 16]).len(), 26);
        assert_eq!(b32_encode(b"foobar"), "mzxw6ytboi"); // RFC 4648 test vector, lowercased
        assert_eq!(b32_decode("MZXW6YTBOI").unwrap(), b"foobar");
        assert!(b32_decode("mzxw1").is_none());
        assert_eq!(b64url_encode(b"foob"), "Zm9vYg");
        assert_eq!(b64url_decode("Zm9vYg==").unwrap(), b"foob");
        assert_eq!(b64url_encode(&[0xfb, 0xff]), "-_8");
        assert_eq!(b64url_encode(&[0u8; 32]).len(), 43);
    }

    #[test]
    fn generated_passwords_are_four_known_words() {
        assert_eq!(WORDS.iter().collect::<std::collections::HashSet<_>>().len(), 256);
        let p = gen_password();
        let parts: Vec<&str> = p.split('-').collect();
        assert_eq!(parts.len(), 4, "{p}");
        assert!(parts.iter().all(|w| WORDS.contains(w)));
        assert!(ct_eq(b"abc", b"abc") && !ct_eq(b"abc", b"abd") && !ct_eq(b"abc", b"ab"));
    }
}
