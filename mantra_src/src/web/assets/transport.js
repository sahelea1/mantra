// Mantra web UI — transports and the protocol client. Spec §5 (protocol), §10.5 (relay).
//
//   LocalTransport   same-origin WebSocket /ws, cookie auth, JSON text frames
//   RelayTransport   <relay>/c/<sid>, binary frames, E2EE per crypto.js
//   Client           hello/snapshot/delta sequencing, request→result promises, ping, reconnect
//
// The relay URL always comes from configuration (config.js or the link), never from
// location.host: the relay may live on a different host than the site serving these files.
'use strict';
(function (root) {
    const C = root.MantraCrypto;
    const PROTOCOL = 1;

    // Close codes the relay uses (see the relay README) → what we tell the person.
    // `retry`: whether reconnecting on our own can help.
    const RELAY_CLOSE = {
        4404: { error: "Mantra isn't connected to the relay. Start it with `mantra --remote`.", retry: true, slow: true, code: 'nohost' },
        4410: { error: 'Mantra left the relay (it quit or lost its connection). Waiting for it to come back…', retry: true, code: 'hostgone' },
        4429: { error: 'Session full — too many devices are connected to this Mantra right now.', retry: false, code: 'full' },
        4413: { error: 'A message was too large for the relay.', retry: true, code: 'big' },
        4400: { error: 'The relay rejected a frame.', retry: true, code: 'proto' },
        4000: { error: 'Mantra closed the connection.', retry: true, code: 'closed' },
    };

    class LocalTransport {
        constructor(url) { this.url = url; this.ws = null; this.opened = false; }
        connect() {
            this.opened = false;
            let ws;
            try { ws = new WebSocket(this.url); } catch (e) { setTimeout(() => this.onclose({ code: 0, reason: String(e), opened: false }), 0); return; }
            this.ws = ws;
            ws.onopen = () => { this.opened = true; this.onopen(); };
            ws.onmessage = (ev) => {
                if (typeof ev.data !== 'string') return;
                let msg;
                try { msg = JSON.parse(ev.data); } catch (_) { return; }
                this.onmessage(msg);
            };
            ws.onclose = (ev) => { if (this.ws === ws) { this.ws = null; this.onclose({ code: ev.code, reason: ev.reason, opened: this.opened }); } };
            ws.onerror = () => { };
        }
        send(obj) { if (this.ws && this.ws.readyState === 1) { this.ws.send(JSON.stringify(obj)); return true; } return false; }
        close() { const ws = this.ws; this.ws = null; if (ws) try { ws.close(1000); } catch (_) { } }
    }

    class RelayTransport {
        // master: a HKDF CryptoKey from MantraCrypto.importMasterKey
        constructor(relay, sid, master) { this.relay = relay.replace(/\/+$/, ''); this.sid = sid; this.master = master; this.ws = null; }
        connect() {
            this.opened = false; this.ck = null; this.tx = 0; this.rx = 0; this.gotData = false; this.frags = []; this.fragBytes = 0;
            this.failReason = null;
            this.rxChain = Promise.resolve(); this.txChain = Promise.resolve();
            let ws;
            try { ws = new WebSocket(this.relay + '/c/' + this.sid); } catch (e) { setTimeout(() => this.onclose({ code: 0, reason: String(e), opened: false }), 0); return; }
            ws.binaryType = 'arraybuffer';
            this.ws = ws;
            ws.onopen = () => {
                this.cr = C.random(16);
                try { ws.send(C.helloFrame({ v: 1, cr: C.b64url(this.cr) })); } catch (_) { }
                // The host has 10 s to answer; after that the relay or host is broken for us.
                this.helloTimer = setTimeout(() => { if (!this.ck) this.fail(ws, 'hello-timeout'); }, 10000);
            };
            ws.onmessage = (ev) => {
                if (typeof ev.data === 'string') { this.fail(ws, 'proto'); return; }
                const buf = new Uint8Array(ev.data);
                // Decryption is async; chain so frames are processed strictly in order.
                this.rxChain = this.rxChain.then(() => this.recv(ws, buf)).catch((e) => this.fail(ws, e && e.message === 'badkey' ? 'badkey' : 'proto'));
            };
            ws.onclose = (ev) => this.closed(ws, ev.code, ev.reason);
            ws.onerror = () => { };
        }
        // Report the end of `ws`, once. Frames that arrived before it may still be decrypting —
        // the host's badkey refusal is one of them — so let the chain settle before judging.
        closed(ws, code, reason) {
            clearTimeout(this.helloTimer);
            if (this.ws !== ws) return;
            this.rxChain.then(() => {
                if (this.ws !== ws) return;
                this.ws = null;
                const info = { code, reason, opened: this.opened };
                // A wrong password/code is known only from the host's plaintext {"err":"badkey"}
                // frame or a local decrypt failure (both land in failReason). A close code alone
                // never means it: 1005/1006 is also what a dropped mobile connection looks like.
                if (this.failReason) info.local = this.failReason;
                this.onclose(info);
            });
        }
        fail(ws, why) {
            this.failReason = this.failReason || why;
            try { ws.close(1000); } catch (_) { }
            // Our side is done: don't wait for the relay to finish the close handshake (the
            // reference relay only does once its own timers fire), or we'd sit in CLOSING for a minute.
            this.closed(ws, 1000, why);
        }
        async recv(ws, buf) {
            if (this.ws !== ws) return;
            const f = C.parseFrame(buf);
            // The host's one plaintext refusal before it closes us (§10.3): {"err":"badkey"} when
            // our first encrypted frame didn't decrypt (wrong password/code), {"err":"hello"} when
            // it couldn't read our hello. Unauthenticated, but a relay could just as well drop us.
            if (f.type === C.T_HELLO && f.json && typeof f.json.err === 'string' && !this.gotData) {
                throw new Error(f.json.err === 'badkey' ? 'badkey' : 'proto');
            }
            if (!this.ck) {
                if (f.type !== C.T_HELLO) throw new Error('proto');
                const j = f.json || {};
                if (j.v !== 1 || typeof j.hr !== 'string') throw new Error('proto');
                const hr = C.unb64url(j.hr);
                if (hr.length !== 16) throw new Error('proto');
                // Protocol and version come later, inside the encrypted server hello (§5).
                this.ck = await C.connKey(this.master, this.cr, hr);
                clearTimeout(this.helloTimer);
                this.opened = true;
                this.onopen();
                return;
            }
            if (f.type === C.T_HELLO) throw new Error('proto');
            if (f.counter !== this.rx) throw new Error('proto');
            this.rx += 1;
            let pt;
            try { pt = await C.open(this.ck, C.DIR_HOST, f.counter, f.body); } catch (_) { throw new Error(this.gotData ? 'proto' : 'badkey'); }
            this.gotData = true;
            // Same cap as crypto.rs MAX_MESSAGE: a peer can't make us buffer fragments forever.
            this.fragBytes += pt.length;
            if (this.fragBytes > C.MAX_MESSAGE) throw new Error('proto');
            if (f.type === C.T_CONT) {
                this.frags.push(pt);
                return;
            }
            const whole = this.frags.length ? C.concat(...this.frags, pt) : pt;
            this.frags = []; this.fragBytes = 0;
            let msg;
            try { msg = JSON.parse(C.utf8(whole)); } catch (_) { return; }
            if (this.ws === ws) this.onmessage(msg);
        }
        send(obj) {
            const ws = this.ws;
            if (!ws || ws.readyState !== 1 || !this.ck) return false;
            const text = JSON.stringify(obj);
            this.txChain = this.txChain.then(async () => {
                const frames = await C.sealMessage(this.ck, C.DIR_CLIENT, () => this.tx++, text);
                for (const fr of frames) if (ws.readyState === 1) ws.send(fr);
            }).catch(() => this.fail(ws, 'proto'));
            return true;
        }
        close() { const ws = this.ws; this.ws = null; if (ws) try { ws.close(1000); } catch (_) { } }
    }

    // ── Client ───────────────────────────────────────────────────────────────────────────────────
    class Client {
        // opts: { transport, mode: 'local'|'relay', clientKind, onEvent(type, payload), checkAuth?: async () => bool }
        constructor(opts) {
            this.t = opts.transport;
            this.mode = opts.mode;
            this.clientKind = opts.clientKind || 'web';
            this.emit = opts.onEvent;
            this.checkAuth = opts.checkAuth;
            this.req = 1;
            this.pending = new Map();
            this.seq = 0;
            this.synced = false;
            this.state = 'idle';
            this.attempt = 0;
            this.stopped = false;
            this.lastRx = 0;
            this.t.onopen = () => this.onOpen();
            this.t.onmessage = (m) => this.onMessage(m);
            this.t.onclose = (info) => this.onClose(info);
            this.onOnline = () => { if (this.state !== 'open' && !this.stopped) this.retryNow(); };
            this.onVisible = () => {
                if (document.visibilityState !== 'visible' || this.stopped) return;
                // Phones freeze background tabs; a socket that looks open may be long dead.
                if (this.state === 'open' && Date.now() - this.lastRx > 30000) { this.t.close(); this.onClose({ code: 0, opened: true }); }
                else if (this.state !== 'open') this.retryNow();
            };
            this.onOffline = () => { if (this.state !== 'open' && !this.stopped) this.setState('offline'); };
            root.addEventListener('online', this.onOnline);
            root.addEventListener('offline', this.onOffline);
            document.addEventListener('visibilitychange', this.onVisible);
        }
        start() { this.stopped = false; this.connect(); }
        stop() {
            this.stopped = true;
            clearTimeout(this.retryTimer); clearInterval(this.pingTimer);
            root.removeEventListener('online', this.onOnline);
            root.removeEventListener('offline', this.onOffline);
            document.removeEventListener('visibilitychange', this.onVisible);
            this.t.close();
            this.rejectAll('disconnected');
        }
        connect() {
            clearTimeout(this.retryTimer);
            this.setState(this.attempt === 0 && !this.everOpen ? 'connecting' : 'reconnecting');
            this.t.connect();
        }
        retryNow() { this.attempt = 0; clearTimeout(this.retryTimer); if (!this.t.ws) this.connect(); }
        setState(s, error) {
            this.state = s;
            this.emit('state', { state: s, error: error || null, attempt: this.attempt });
        }
        onOpen() {
            this.everOpen = true;
            this.attempt = 0;
            this.synced = false;
            this.lastRx = Date.now();
            this.sendHello();
            clearInterval(this.pingTimer);
            this.pingTimer = setInterval(() => {
                if (Date.now() - this.lastRx > 60000) { this.t.close(); this.onClose({ code: 0, opened: true, reason: 'timeout' }); return; }
                this.t.send({ t: 'ping' });
            }, 25000);
        }
        sendHello() {
            const h = { t: 'hello', protocol: PROTOCOL, client: this.clientKind, ua: navigator.userAgent.slice(0, 200) };
            if (this.seq) h.since = this.seq;
            this.t.send(h);
        }
        onMessage(m) {
            this.lastRx = Date.now();
            if (!m || typeof m.t !== 'string') return;
            switch (m.t) {
                case 'hello':
                    this.hello = m;
                    if (m.protocol !== PROTOCOL) this.emit('protocol', m);
                    this.emit('hello', m);
                    break;
                case 'snapshot':
                    this.seq = m.seq || 0;
                    this.synced = true;
                    if (this.state !== 'open') this.setState('open');
                    this.emit('snapshot', m);
                    break;
                case 'delta':
                    if (!this.synced) return;
                    if (m.seq !== this.seq + 1) {
                        // Missed something (or the server restarted its counter): ask for a full
                        // snapshot and ignore deltas until it arrives.
                        this.synced = false;
                        this.sendHello();
                        return;
                    }
                    this.seq = m.seq;
                    this.emit('delta', m);
                    break;
                case 'result': case 'items': {
                    const p = this.pending.get(m.req);
                    if (!p) return;
                    this.pending.delete(m.req);
                    clearTimeout(p.timer);
                    if (m.t === 'items') p.resolve(m);
                    else if (m.ok) p.resolve(m.data === undefined ? null : m.data);
                    else p.reject(new Error(m.error || 'failed'));
                    break;
                }
                case 'note': this.emit('note', m); break;
                case 'ping': this.t.send({ t: 'pong' }); break;
                case 'pong': break;
                case 'bye':
                    this.bye = m.reason || 'quit';
                    if (m.reason === 'unauthorized') { this.stopped = true; this.t.close(); this.setState('auth'); }
                    break;
            }
        }
        async onClose(info) {
            clearInterval(this.pingTimer);
            this.synced = false;
            this.rejectAll('connection lost');
            if (this.stopped) return;
            if (info.local === 'badkey') {
                this.stopped = true;
                this.setState('failed', 'Wrong password or code');
                this.emit('fatal', { code: 'badkey', error: 'Wrong password or code' });
                return;
            }
            if (this.bye === 'replaced') {
                this.bye = null;
                this.stopped = true;
                this.setState('failed', 'This session was opened elsewhere');
                this.emit('fatal', { code: 'replaced', error: 'This session was opened elsewhere' });
                return;
            }
            if (this.bye === 'rotated') {
                this.bye = null;
                this.stopped = true;
                this.setState('failed', 'This link was rotated on the host');
                this.emit('fatal', { code: 'rotated', error: 'This link was rotated on the host. Ask for the new link or code.' });
                return;
            }
            const rc = this.mode === 'relay' ? RELAY_CLOSE[info.code] : null;
            if (rc && !rc.retry) {
                this.stopped = true;
                this.setState('failed', rc.error);
                this.emit('fatal', { code: rc.code, error: rc.error });
                return;
            }
            // Local: a socket that never opened is either a dead server or a missing/expired
            // session cookie (the upgrade is refused with 401) — the session endpoint tells which.
            if (this.mode === 'local' && !info.opened && this.checkAuth) {
                try {
                    const ok = await this.checkAuth();
                    if (ok === false) { this.stopped = true; this.setState('auth'); return; }
                } catch (_) { /* server unreachable → plain reconnect */ }
            }
            if (this.bye === 'unauthorized') return;
            const slow = this.bye === 'slow';
            this.attempt += 1;
            // The server shed us for being too slow to keep up — hammering it right back at 1 s
            // just repeats the problem (and looked like a silent, unexplained reconnect loop
            // before this: no rc for local/unmatched codes, and 'slow' isn't 'quit'). Skip ahead
            // to the backoff attempt 3 would already be at.
            if (slow) this.attempt = Math.max(this.attempt, 3);
            // 1 s → 30 s with ±30 % jitter so a room full of phones doesn't reconnect in lockstep.
            const baseMs = Math.min(30000, 1000 * Math.pow(2, Math.min(this.attempt - 1, 5))) * (rc && rc.slow ? 2 : 1);
            const wait = Math.round(baseMs * (0.7 + Math.random() * 0.6));
            const offline = navigator.onLine === false;
            let err = rc ? rc.error : slow ? 'Mantra dropped this connection for falling behind. Reconnecting…' : (this.bye === 'quit' ? 'Mantra quit. Waiting for it to start again…' : null);
            this.bye = null;
            this.setState(offline ? 'offline' : 'reconnecting', err);
            this.retryAt = Date.now() + wait;
            this.emit('retry', { at: this.retryAt });
            this.retryTimer = setTimeout(() => this.connect(), wait);
        }
        rejectAll(why) {
            for (const [, p] of this.pending) { clearTimeout(p.timer); p.reject(new Error(why)); }
            this.pending.clear();
        }
        // Send a command. Resolves with `data` (or the `items` message for fetch_items).
        request(cmd, args, timeoutMs) {
            return new Promise((resolve, reject) => {
                if (this.state !== 'open') { reject(new Error('not connected')); return; }
                const req = this.req++;
                const msg = Object.assign({ t: 'cmd', req, cmd }, args || {});
                const timer = setTimeout(() => { this.pending.delete(req); reject(new Error('no answer from Mantra')); }, timeoutMs || 30000);
                this.pending.set(req, { resolve, reject, timer });
                if (!this.t.send(msg)) { clearTimeout(timer); this.pending.delete(req); reject(new Error('not connected')); }
            });
        }
    }

    root.MantraTransport = { PROTOCOL, RELAY_CLOSE, LocalTransport, RelayTransport, Client };
})(window);
