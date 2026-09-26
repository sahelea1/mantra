// Mantra web UI — service worker: offline shell, Web Push, notification clicks. Spec §9.2, §11.1.
//
// __MANTRA_SW_VERSION__ below: web/server.rs substitutes it per build (like /config.js); a static bundle (the relay site) `sed`s the same token from its Dockerfile at image build time.
//
// Caching is network-first for everything (falling back to the cache when offline) rather than
// cache-first for assets: the assets are embedded in the Mantra binary and change with it while
// this file may not, so a cache-first worker would keep serving the old UI after an upgrade.
'use strict';
const VERSION = 'mantra-shell-__MANTRA_SW_VERSION__';
const SHELL = [
    '/', '/manifest.webmanifest', '/assets/app.css',
    '/assets/crypto.js', '/assets/transport.js', '/assets/ui/dom.js', '/assets/ui/fmt.js', '/assets/ui/store.js',
    '/assets/ui/parts.js', '/assets/ui/agent.js', '/assets/ui/screens.js', '/assets/app.js',
    '/icons/icon.svg', '/icons/icon-192.png', '/icons/badge-96.png',
];
const META = 'mantra-meta';
let base = '';

self.addEventListener('install', (ev) => {
    ev.waitUntil(caches.open(VERSION).then((c) => Promise.all(SHELL.map((u) => c.add(new Request(u, { cache: 'no-cache' })).catch(() => { })))));
});

self.addEventListener('activate', (ev) => {
    ev.waitUntil((async () => {
        for (const k of await caches.keys()) if (k !== VERSION && k !== META) await caches.delete(k);
        await self.clients.claim();
    })());
});

function never(url) {
    // Live data and auth never come from a cache.
    return url.pathname.startsWith('/api/') || url.pathname === '/ws' || url.pathname.startsWith('/c/') || url.pathname.startsWith('/host/') || url.pathname === '/cert.pem';
}

async function networkFirst(req, fallbackPath) {
    const cache = await caches.open(VERSION);
    try {
        const res = await Promise.race([fetch(req), new Promise((_, rej) => setTimeout(() => rej(new Error('timeout')), 6000))]);
        if (res && res.ok && res.type === 'basic') cache.put(fallbackPath || req, res.clone()).catch(() => { });
        return res;
    } catch (e) {
        const hit = await cache.match(fallbackPath || req, { ignoreSearch: true });
        if (hit) return hit;
        throw e;
    }
}

self.addEventListener('fetch', (ev) => {
    const req = ev.request;
    if (req.method !== 'GET') return;
    const url = new URL(req.url);
    if (url.origin !== self.location.origin || never(url)) return;
    // Every SPA route (/, /agent/3, /s/<sid>/run …) is the same index.html.
    if (req.mode === 'navigate') { ev.respondWith(networkFirst(req, '/')); return; }
    ev.respondWith(networkFirst(req));
});

// ── push ─────────────────────────────────────────────────────────────────────────────────────────
async function getBase() {
    if (base) return base;
    try {
        const r = await (await caches.open(META)).match('/__mantra_base');
        if (r) base = await r.text();
    } catch (_) { }
    return base;
}

self.addEventListener('message', (ev) => {
    const d = ev.data || {};
    if (d.type === 'skipWaiting') self.skipWaiting();
    if (d.type === 'base' && typeof d.base === 'string' && /^(\/s\/[a-z2-7]{26})?$/.test(d.base)) {
        base = d.base;
        ev.waitUntil(caches.open(META).then((c) => c.put('/__mantra_base', new Response(base))));
    }
});

self.addEventListener('push', (ev) => {
    let n = {};
    try { n = ev.data ? ev.data.json() : {}; } catch (_) { n = { title: 'Mantra', body: ev.data ? ev.data.text() : '' }; }
    const title = n.title || 'Mantra';
    ev.waitUntil((async () => {
        const url = (await getBase()) + (n.url || '/');
        const wins = await self.clients.matchAll({ type: 'window', includeUncontrolled: true });
        // Someone is looking at Mantra right now: show it in the page instead of the OS tray.
        const seen = wins.find((w) => w.visibilityState === 'visible' && w.focused);
        if (seen) { seen.postMessage({ push: Object.assign({}, n, { url }) }); return; }
        await self.registration.showNotification(title, {
            body: n.body || '',
            tag: n.tag || n.kind || 'mantra',
            renotify: n.kind === 'halt' || n.kind === 'question' || n.kind === 'approval',
            icon: '/icons/icon-192.png',
            badge: '/icons/badge-96.png',
            timestamp: n.at || Date.now(),
            data: { url },
        });
    })());
});

self.addEventListener('notificationclick', (ev) => {
    ev.notification.close();
    const url = (ev.notification.data && ev.notification.data.url) || '/';
    ev.waitUntil((async () => {
        const wins = await self.clients.matchAll({ type: 'window', includeUncontrolled: true });
        const w = wins.find((c) => new URL(c.url).origin === self.location.origin);
        if (w) {
            await w.focus();
            w.postMessage({ navigate: url });
            return;
        }
        await self.clients.openWindow(url);
    })());
});
