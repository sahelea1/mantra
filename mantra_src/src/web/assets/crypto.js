// Mantra web UI — crypto helpers for the remote (relay) transport. Spec §10.
//
// Pure functions over Uint8Array plus thin WebCrypto wrappers; no DOM, no state, so the same file
// can be loaded by a test harness. Everything here must match web/crypto.rs byte for byte:
//   key      = PBKDF2-HMAC-SHA256(password, "mantra-remote:" + sid, 310000, 32 bytes)
//   conn key = HKDF-SHA256(ikm = key, salt = client_random || host_random, info = "mantra-remote-v1", 32)
//   nonce    = dir(1) || 00 00 00 || counter(u64 BE), AAD = the dir byte
//   frame    = type(1) || ...   0x01 hello JSON, 0x02 data (final), 0x03 data (continued)
//   hello    = client {"v":1,"cr":…} → host {"v":1,"hr":…} (nothing else before the key is proven;
//              protocol/version arrive in the encrypted server hello). A host that refuses the
//              handshake sends one plaintext 0x01 {"err":"badkey"|"hello"} and closes.
//   message  = at most MAX_MESSAGE bytes of plaintext once fragments are joined (crypto.rs too)
'use strict';
(function (root) {
    const enc = new TextEncoder();
    const dec = new TextDecoder('utf-8', { fatal: true });

    const PBKDF2_ITERS = 310000;
    const HKDF_INFO = enc.encode('mantra-remote-v1');
    const DIR_HOST = 0x01;    // host → client
    const DIR_CLIENT = 0x02;  // client → host
    const T_HELLO = 0x01, T_DATA = 0x02, T_CONT = 0x03;
    // The relay caps frames at 1 MiB; 900 kB of plaintext leaves room for the header and tag.
    const FRAG = 900000;
    // crypto.rs MAX_MESSAGE: a reassembled message larger than this is refused.
    const MAX_MESSAGE = 16 * 1024 * 1024;

    // ── encodings ────────────────────────────────────────────────────────────────────────────────
    function b64url(bytes) {
        let s = '';
        for (let i = 0; i < bytes.length; i += 0x8000) s += String.fromCharCode.apply(null, bytes.subarray(i, i + 0x8000));
        return btoa(s).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
    }
    function unb64url(str) {
        const s = String(str).replace(/-/g, '+').replace(/_/g, '/');
        const pad = s.length % 4 ? '='.repeat(4 - (s.length % 4)) : '';
        const bin = atob(s + pad); // throws on garbage — callers catch
        const out = new Uint8Array(bin.length);
        for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
        return out;
    }

    const B32 = 'abcdefghijklmnopqrstuvwxyz234567';
    function base32(bytes) {
        let out = '', bits = 0, val = 0;
        for (const b of bytes) {
            val = (val << 8) | b; bits += 8;
            while (bits >= 5) { out += B32[(val >>> (bits - 5)) & 31]; bits -= 5; }
        }
        if (bits > 0) out += B32[(val << (5 - bits)) & 31];
        return out;
    }
    function unbase32(str) {
        const out = [];
        let bits = 0, val = 0;
        for (const ch of String(str).toLowerCase()) {
            const i = B32.indexOf(ch);
            if (i < 0) throw new Error('not base32');
            val = ((val << 5) | i) & 0xffff; bits += 5;
            if (bits >= 8) { out.push((val >>> (bits - 8)) & 0xff); bits -= 8; }
        }
        return new Uint8Array(out);
    }

    // A sid is 16 random bytes → 26 base32 chars. People type it grouped 4-4-4-4-4-6, with or
    // without dashes, in any case — normalise all of that away. Returns null when it can't be one.
    function normalizeCode(code) {
        const s = String(code || '').toLowerCase().replace(/[\s\-_.]/g, '');
        if (!/^[a-z2-7]{26}$/.test(s)) return null;
        return s;
    }
    function formatCode(sid) {
        const s = String(sid || '');
        const parts = [];
        for (let i = 0; i < 20 && i < s.length; i += 4) parts.push(s.slice(i, i + 4));
        if (s.length > 20) parts.push(s.slice(20));
        return parts.join('-');
    }

    function concat(...arrs) {
        let n = 0;
        for (const a of arrs) n += a.length;
        const out = new Uint8Array(n);
        let o = 0;
        for (const a of arrs) { out.set(a, o); o += a.length; }
        return out;
    }
    function random(n) { const b = new Uint8Array(n); crypto.getRandomValues(b); return b; }

    // Counters are u64 on the wire; JS numbers are exact to 2^53, which at one frame per
    // millisecond is ~285 000 years — no BigInt needed.
    function u64be(n) {
        const b = new Uint8Array(8);
        let hi = Math.floor(n / 4294967296), lo = n >>> 0;
        new DataView(b.buffer).setUint32(0, hi >>> 0);
        new DataView(b.buffer).setUint32(4, lo);
        return b;
    }
    function readU64be(b, off) {
        const dv = new DataView(b.buffer, b.byteOffset + off, 8);
        return dv.getUint32(0) * 4294967296 + dv.getUint32(4);
    }
    function nonce(dir, counter) {
        const n = new Uint8Array(12);
        n[0] = dir;
        n.set(u64be(counter), 4);
        return n;
    }

    // ── WebCrypto ────────────────────────────────────────────────────────────────────────────────
    function subtle() {
        if (!root.crypto || !root.crypto.subtle) throw new Error('This browser has no WebCrypto here (it needs HTTPS)');
        return root.crypto.subtle;
    }

    // ~0.3 s on a phone. Returns the raw 32 bytes (the link carries them as #k=…).
    async function deriveKey(password, sid) {
        const base = await subtle().importKey('raw', enc.encode(password), 'PBKDF2', false, ['deriveBits']);
        const bits = await subtle().deriveBits({ name: 'PBKDF2', hash: 'SHA-256', salt: enc.encode('mantra-remote:' + sid), iterations: PBKDF2_ITERS }, base, 256);
        return new Uint8Array(bits);
    }

    // The long-lived key as a non-extractable HKDF key: this is what "remember this device"
    // stores in IndexedDB, so script on the page can use it but never read the bytes back out.
    function importMasterKey(raw, extractable) {
        return subtle().importKey('raw', raw, 'HKDF', !!extractable, ['deriveBits']);
    }

    async function connKey(master, cr, hr) {
        const bits = await subtle().deriveBits({ name: 'HKDF', hash: 'SHA-256', salt: concat(cr, hr), info: HKDF_INFO }, master, 256);
        return subtle().importKey('raw', bits, { name: 'AES-GCM' }, false, ['encrypt', 'decrypt']);
    }

    async function seal(ck, dir, counter, plain) {
        const ct = await subtle().encrypt({ name: 'AES-GCM', iv: nonce(dir, counter), additionalData: new Uint8Array([dir]), tagLength: 128 }, ck, plain);
        return new Uint8Array(ct);
    }
    async function open(ck, dir, counter, ct) {
        const pt = await subtle().decrypt({ name: 'AES-GCM', iv: nonce(dir, counter), additionalData: new Uint8Array([dir]), tagLength: 128 }, ck, ct);
        return new Uint8Array(pt);
    }

    // ── frames ───────────────────────────────────────────────────────────────────────────────────
    function helloFrame(obj) { return concat(new Uint8Array([T_HELLO]), enc.encode(JSON.stringify(obj))); }

    // Parse a relay payload into {type, json?, counter?, body?}. Throws on anything malformed so
    // the transport closes instead of guessing.
    function parseFrame(buf) {
        const b = buf instanceof Uint8Array ? buf : new Uint8Array(buf);
        if (b.length < 1) throw new Error('empty frame');
        const type = b[0];
        if (type === T_HELLO) return { type, json: JSON.parse(dec.decode(b.subarray(1))) };
        if (type === T_DATA || type === T_CONT) {
            if (b.length < 1 + 8 + 16) throw new Error('short data frame');
            return { type, counter: readU64be(b, 1), body: b.subarray(9) };
        }
        throw new Error('unknown frame type ' + type);
    }

    // Split one JSON message into encrypted frames (0x03… then a final 0x02). `next()` hands out
    // the direction's counter so the caller owns sequencing.
    async function sealMessage(ck, dir, next, text) {
        const bytes = enc.encode(text);
        const frames = [];
        let off = 0;
        do {
            const part = bytes.subarray(off, Math.min(off + FRAG, bytes.length));
            off += part.length;
            const last = off >= bytes.length;
            const c = next();
            const ct = await seal(ck, dir, c, part);
            frames.push(concat(new Uint8Array([last ? T_DATA : T_CONT]), u64be(c), ct));
        } while (off < bytes.length);
        return frames;
    }

    root.MantraCrypto = {
        PBKDF2_ITERS, DIR_HOST, DIR_CLIENT, T_HELLO, T_DATA, T_CONT, FRAG, MAX_MESSAGE,
        b64url, unb64url, base32, unbase32, normalizeCode, formatCode, concat, random, u64be, readU64be, nonce,
        deriveKey, importMasterKey, connKey, seal, open, helloFrame, parseFrame, sealMessage,
        utf8: (b) => dec.decode(b), bytes: (s) => enc.encode(s),
    };
})(typeof self !== 'undefined' ? self : globalThis);
