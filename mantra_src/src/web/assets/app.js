// Mantra web UI — app shell: boot, router, layouts (phone tab bar / desktop three columns), sheets,
// command palette, notifications (Web Push), the relay connect flow, banners and timers.
//
// Loaded last (see index.html). The other files define M.h/M.patch (ui/dom.js), M.fmt, M.store,
// M.parts, M.agentView and M.screens; this file defines M.act (what views call), M.push, M.relay.
'use strict';
(function (M) {
    const h = M.h, F = M.fmt, P = M.parts, AV = M.agentView, SC = M.screens;
    const { S } = M.store;
    const icon = F.icon;
    const T = window.MantraTransport, C = window.MantraCrypto;
    const CFG = Object.assign({ mode: 'local', protocol: 1 }, window.__MANTRA__ || {});
    const MODE = CFG.mode === 'relay' ? 'relay' : 'local';
    S.conn.mode = MODE;

    const mqWide = matchMedia('(min-width: 1024px)');
    const mqMid = matchMedia('(min-width: 720px)');
    const mqReduce = matchMedia('(prefers-reduced-motion: reduce)');
    const mqDark = matchMedia('(prefers-color-scheme: dark)');
    let client = null;
    let base = '';            // '/s/<sid>' in relay mode
    let navDepth = 0;

    // ── rendering ────────────────────────────────────────────────────────────────────────────────
    let queued = false;
    function changed() {
        if (queued) return;
        queued = true;
        requestAnimationFrame(render);
    }
    function render() {
        queued = false;
        const root = document.getElementById('app');
        M.patch(root, Root());
        AV.afterRender();
        updateTitle();
    }

    // ── actions (what views call) ────────────────────────────────────────────────────────────────
    function flash(text, level, action) {
        const f = { id: Math.random().toString(36).slice(2), text: String(text || ''), level: level || 'info', at: Date.now(), action };
        S.ui.flashes = (S.ui.flashes || []).filter((x) => x.text !== f.text).concat(f).slice(-3);
        setTimeout(() => { S.ui.flashes = (S.ui.flashes || []).filter((x) => x.id !== f.id); changed(); }, action ? 7000 : 3800);
        changed();
    }
    function request(cmd, args, timeout) {
        if (!client) return Promise.reject(new Error('not connected'));
        return client.request(cmd, args, timeout);
    }
    // Run a command with a spinner key and error flash; resolves with data, rejects on error.
    async function cmd(name, args, opts) {
        opts = opts || {};
        if (opts.busyKey) { S.ui.busy[opts.busyKey] = true; changed(); }
        try {
            const d = await request(name, args);
            if (opts.ok) flash(opts.ok, 'ok');
            return d;
        } catch (e) {
            flash(e.message || String(e), 'error');
            throw e;
        } finally {
            if (opts.busyKey) { delete S.ui.busy[opts.busyKey]; changed(); }
        }
    }
    function wide() { return mqMid.matches; }
    function motion() {
        return !(S.ui.motion === 'reduce' || (S.ui.motion === 'system' && mqReduce.matches));
    }
    async function copy(text) {
        try { await navigator.clipboard.writeText(text); flash('Copied', 'ok'); }
        catch (_) {
            const ta = document.createElement('textarea');
            ta.value = text; ta.setAttribute('readonly', ''); ta.style.position = 'fixed'; ta.style.opacity = '0';
            document.body.appendChild(ta); ta.select();
            try { document.execCommand('copy'); flash('Copied', 'ok'); } catch (_) { flash('Copy failed — select and copy by hand', 'warn'); }
            ta.remove();
        }
    }
    function sheet(sh) {
        S.ui.sheet = sh;
        if (sh && sh.kind === 'diff') loadDiff(sh);
        changed();
    }
    function closeSheet() { S.ui.sheet = null; changed(); }

    function applyPrefs() {
        const el = document.documentElement;
        const t = S.ui.theme;
        if (t === 'dark' || t === 'light') el.setAttribute('data-theme', t); else el.removeAttribute('data-theme');
        el.classList.toggle('reduce-motion', !motion());
        // The status-bar colour follows a manual theme; with "system" each meta keeps its media query.
        for (const m of document.querySelectorAll('meta[name="theme-color"]')) {
            const forDark = (m.getAttribute('media') || '').includes('dark');
            m.content = t === 'dark' ? '#0F1116' : t === 'light' ? '#F7F8FA' : forDark ? '#0F1116' : '#F7F8FA';
        }
        changed();
    }

    // ── routing ──────────────────────────────────────────────────────────────────────────────────
    const TABS = { team: 'team', agent: 'agent', run: 'run', inbox: 'inbox', more: 'more', pulse: 'more', runs: 'more', settings: 'more' };
    function parse(path) {
        let p = path;
        if (base && p.startsWith(base)) p = p.slice(base.length);
        p = p.replace(/\/+$/, '') || '/';
        let m;
        if (p === '/') return { name: 'team' };
        if ((m = /^\/agent\/(\d+)$/.exec(p))) return { name: 'agent', id: Number(m[1]) };
        if ((m = /^\/(run|pulse|inbox|runs|settings|more|login)$/.exec(p))) return { name: m[1] };
        return { name: 'team' };
    }
    function nav(path, opts) {
        opts = opts || {};
        const r = parse(base + path);
        const from = S.ui.route;
        const url = base + (path === '/' ? (base ? '' : '/') : path);
        if (opts.replace) history.replaceState({ d: navDepth }, '', url);
        else if (location.pathname !== url) { navDepth++; history.pushState({ d: navDepth }, '', url); }
        setRoute(r, from, opts.back);
    }
    function back(fallback) {
        if (navDepth > 0) history.back();
        else nav(fallback || '/', { replace: true, back: true });
    }
    function setRoute(r, from, isBack) {
        from = from || S.ui.route;
        const sameTab = TABS[r.name] === TABS[from.name];
        S.ui.anim = isBack ? 'pop' : sameTab && r.name !== from.name ? 'push' : r.name === 'agent' && from.name !== 'agent' ? 'push' : 'fade';
        S.ui.route = r;
        S.ui.sheet = null;
        S.ui.palette = null;
        if (r.name === 'agent') { M.store.setPref('focus', r.id); delete S.ui.unread[r.id]; }
        if (r.name === 'runs' && S.conn.state === 'open') SC.loadRuns();
        changed();
    }
    window.addEventListener('popstate', (ev) => {
        navDepth = ev.state && typeof ev.state.d === 'number' ? ev.state.d : 0;
        setRoute(parse(location.pathname), null, true);
    });

    function focusAgentId() {
        if (S.ui.route.name === 'agent' && M.store.agent(S.ui.route.id)) return S.ui.route.id;
        const f = S.ui.focus;
        if (f !== null && f !== undefined && M.store.agent(f)) return Number(f);
        const list = M.store.agentList();
        const solo = list.find((a) => a.role_kind === 'solo');
        if (solo) return solo.id;
        const lead = list.find((a) => a.role_kind === 'orchestrator') || list.find((a) => a.role_kind === 'planner') || list[0];
        return lead ? lead.id : null;
    }

    // ── shell ────────────────────────────────────────────────────────────────────────────────────
    function attention() {
        const run = S.run;
        return (S.approvals.length) + (run && run.halted ? 1 : 0) + (run && run.question ? 1 : 0);
    }
    function updateTitle() {
        const n = attention();
        const name = (S.app && S.app.project_name) || 'Mantra';
        const t = (n ? '(' + n + ') ' : '') + name + ' · Mantra';
        if (document.title !== t) document.title = t;
        if (navigator.setAppBadge) { try { n ? navigator.setAppBadge(n) : navigator.clearAppBadge(); } catch (_) { } }
    }

    function connDot() {
        const st = S.conn.state;
        const tone = st === 'open' ? 'ok' : st === 'reconnecting' || st === 'connecting' ? 'wait' : 'bad';
        const label = st === 'open' ? (MODE === 'relay' ? 'connected via relay (end-to-end encrypted)' : 'connected') : st;
        return h('span', { class: 'conn-dot ' + tone, title: label, role: 'status', 'aria-label': label }, MODE === 'relay' ? icon('globe') : null, h('i'));
    }

    function connBar() {
        const st = S.conn.state;
        if (st === 'open' || st === 'idle' || (!S.synced && st === 'connecting')) return null;
        const secs = S.conn.retryAt ? Math.max(0, Math.ceil((S.conn.retryAt - Date.now()) / 1000)) : 0;
        const text = st === 'offline' ? 'Offline — showing the last known state'
            : st === 'failed' ? (S.conn.error || 'Disconnected')
                : st === 'connecting' ? 'Connecting…'
                    : (S.conn.error ? S.conn.error + ' ' : 'Reconnecting… ') + (secs ? '(retry in ' + secs + 's)' : '');
        return h('div', { class: 'connbar ' + st, role: 'status', key: 'connbar' },
            h('span', { class: 'connbar-text' }, text),
            st === 'reconnecting' || st === 'offline' ? h('button', { type: 'button', class: 'link', onclick: () => client && client.retryNow() }, 'Retry now') : null,
            st === 'failed' && MODE === 'relay' ? h('button', { type: 'button', class: 'link', onclick: () => M.relay.toConnect() }, 'Connect again') : null);
    }

    function banners() {
        const out = [];
        const dis = S.ui.banners;
        const insecure = location.protocol === 'http:' && !/^(localhost|127\.0\.0\.1|\[::1\])$/.test(location.hostname);
        if (insecure && !dis.insecure) {
            out.push(banner('insecure', 'alert', ['Notifications and installing need HTTPS. Start Mantra with ', h('code', null, '--web-tls'), ' and install its certificate from ', h('a', { href: '/cert.pem' }, '/cert.pem'), ' on this device.']));
        }
        if (S.ui.install && !dis.install) {
            out.push(banner('install', 'download', 'Install Mantra as an app for a full-screen view and notifications.', P.btn('Install', async () => {
                const ev = S.ui.install; S.ui.install = null; changed();
                try { ev.prompt(); await ev.userChoice; } catch (_) { }
            }, { sm: true, kind: 'primary' })));
        } else if (isIOS() && !standalone() && !dis.ios && window.isSecureContext) {
            out.push(banner('ios', 'download', ['Tip: tap ', h('b', null, 'Share › Add to Home Screen'), ' to install Mantra and get notifications.']));
        }
        if (S.ui.swUpdate) {
            out.push(banner('update', 'refresh', 'A new version of the web UI is ready.', P.btn('Reload', () => { S.ui.swUpdate.postMessage({ type: 'skipWaiting' }); }, { sm: true, kind: 'primary' }), true));
        }
        if (MODE === 'relay' && M.relay.offerRemember && !S.ui.remembered && S.conn.state === 'open') {
            out.push(banner('remember', 'lock', 'Remember this session on this device? You won’t need the link or password again here.', P.btn('Remember', () => M.relay.remember(), { sm: true, kind: 'primary' }), false, () => { M.relay.offerRemember = false; changed(); }));
        }
        return out.length ? h('div', { class: 'banners', key: 'banners' }, out) : null;
    }
    function banner(id, ico, text, action, sticky, onClose) {
        return h('div', { class: 'banner', key: 'bn-' + id, role: 'note' }, icon(ico), h('div', { class: 'banner-text' }, text), action || null,
            sticky ? null : P.iconBtn('close', () => { if (onClose) onClose(); else { S.ui.banners[id] = true; M.store.save('banners', S.ui.banners); changed(); } }, 'Dismiss'));
    }

    function screenFor(r) {
        switch (r.name) {
            case 'agent': return AV.agentScreen(r.id);
            case 'run': return SC.runScreen();
            case 'pulse': return SC.pulseScreen();
            case 'inbox': return SC.inboxScreen();
            case 'runs': return SC.runsScreen();
            case 'settings': return SC.settingsScreen();
            case 'more': return SC.moreScreen();
            default: return SC.teamScreen();
        }
    }
    // Tips and warnings live where people look first, not on every screen.
    const BANNER_ROUTES = { team: 1, settings: 1, more: 1 };
    const TITLES = { run: 'Run', pulse: 'Pulse', inbox: 'Inbox', runs: 'Runs', settings: 'Settings', more: 'More' };

    function topbar(r) {
        const app = S.app || {};
        const sub = r.name === 'team' ? (S.run ? S.run.stage.label + ' · ' + F.durShort(S.run.elapsed_ms) : app.branch ? '⎇ ' + app.branch : '')
            : r.name === 'run' && S.run ? S.run.stage.label : r.name === 'pulse' && S.run ? S.run.stage.label : '';
        const backTo = { pulse: '/more', runs: '/more', settings: '/more' }[r.name];
        return h('header', { class: 'topbar', key: 'topbar' },
            backTo ? P.iconBtn('back', () => back(backTo), 'Back', { cls: 'back' }) : h('img', { class: 'topbar-logo', src: '/icons/icon.svg', alt: '', width: 28, height: 28 }),
            h('div', { class: 'topbar-titles' },
                h('h1', null, r.name === 'team' ? (app.project_name || 'Mantra') : TITLES[r.name] || 'Mantra'),
                sub ? h('div', { class: 'topbar-sub' }, sub) : null),
            r.name === 'runs' ? P.iconBtn('refresh', () => SC.loadRuns(), 'Refresh') : null,
            connDot());
    }

    function tabbar(r) {
        const run = S.run;
        const fid = focusAgentId();
        const fa = fid !== null ? M.store.agent(fid) : null;
        const tab = TABS[r.name];
        const runBadge = run && (run.halted || run.question || (run.want_review && run.stage.kind === 'review'));
        const t = (id, ico, label, path, badge, dot) => h('button', { type: 'button', key: id, class: 'tab' + (tab === id ? ' on' : ''), 'aria-current': tab === id ? 'page' : null, onclick: () => { if (tab === id && id !== 'agent') { const sc = document.querySelector('.content'); if (sc) sc.scrollTo({ top: 0, behavior: motion() ? 'smooth' : 'auto' }); } nav(path); } },
            h('span', { class: 'tab-ico' }, id === 'agent' && fa ? h('span', { class: 'tab-glyph', style: { color: F.colorVar(fa.color) } }, fa.glyph) : icon(ico),
                badge ? h('span', { class: 'badge' }, badge > 99 ? '99+' : String(badge)) : dot ? h('span', { class: 'badge dot ' + dot }) : null),
            h('span', { class: 'tab-label' }, label));
        return h('nav', { class: 'tabbar', key: 'tabbar', 'aria-label': 'Sections' },
            t('team', 'team', 'Team', '/'),
            t('agent', 'agent', fa ? F.trunc(fa.worker ? fa.worker.task_id : fa.name, 12) : 'Agent', fid !== null ? '/agent/' + fid : '/', fa ? S.ui.unread[fa.id] : 0),
            t('run', 'run', 'Run', '/run', 0, runBadge ? (run.halted ? 'halt' : run.question ? 'wait' : 'review') : null),
            t('inbox', 'inbox', 'Inbox', '/inbox', S.approvals.length),
            t('more', 'more', 'More', '/more'));
    }

    function phoneShell(r) {
        const fixed = r.name === 'agent';
        return h('div', { class: 'shell phone' + (S.ui.kb ? ' kb' : ''), key: 'phone' },
            connBar(),
            fixed ? null : topbar(r),
            h('main', { class: 'content' + (fixed ? ' fixed' : ''), key: 'content' },
                fixed || !BANNER_ROUTES[r.name] ? null : banners(),
                h('div', { class: 'screen anim-' + (motion() ? S.ui.anim || 'fade' : 'none') + (fixed ? ' fill' : ''), key: 'scr-' + r.name + (r.id !== undefined ? r.id : '') }, screenFor(r))),
            tabbar(r));
    }

    // Desktop / tablet: sidebar + main (+ right panel on wide screens for team/agent).
    function sidebar(r) {
        const app = S.app || {};
        const run = S.run;
        const item = (path, ico, label, name, badge) => h('button', { type: 'button', key: name, class: 'side-nav-item' + (r.name === name ? ' on' : ''), onclick: () => nav(path) },
            icon(ico), h('span', null, label), badge ? h('span', { class: 'unread' }, String(badge)) : null);
        return h('aside', { class: 'side', key: 'side', 'aria-label': 'Team' },
            h('div', { class: 'side-head' },
                h('img', { src: '/icons/icon.svg', alt: '', width: 30, height: 30 }),
                h('div', { class: 'side-titles' },
                    h('div', { class: 'side-project', title: app.project || '' }, app.project_name || 'Mantra'),
                    h('div', { class: 'side-branch' }, app.branch ? '⎇ ' + app.branch : MODE === 'relay' ? 'remote' : '')),
                connDot()),
            h('div', { class: 'side-scroll' },
                run ? h('div', { class: 'side-run', key: 'sr' }, P.runCard({ compact: true })) : null,
                run && run.halted ? h('button', { type: 'button', class: 'side-alert halt', key: 'sh', onclick: () => nav('/') }, '⛔ ', P.HALT_TITLE[run.halted.reason] || 'Halted') : null,
                run && run.question ? h('button', { type: 'button', class: 'side-alert question', key: 'sq', onclick: () => nav('/') }, '? ', (run.question.from_name || 'agent') + ' has a question') : null,
                S.synced ? h('div', { class: 'side-team', key: 'st' }, P.teamList({ compact: true })) : h('div', { class: 'side-loading' }, P.loading(P.waitText()))),
            h('nav', { class: 'side-nav', 'aria-label': 'Sections' },
                item('/', 'team', 'Overview', 'team'),
                item('/run', 'run', 'Run', 'run', run && (run.halted || run.question) ? '!' : 0),
                item('/inbox', 'inbox', 'Inbox', 'inbox', S.approvals.length),
                item('/pulse', 'pulse', 'Pulse', 'pulse'),
                item('/runs', 'runs', 'Runs', 'runs'),
                item('/settings', 'settings', 'Settings', 'settings')));
    }

    function mainHeader(r) {
        if (r.name === 'agent') return null;
        const app = S.app || {};
        const title = r.name === 'team' ? 'Overview' : TITLES[r.name];
        const sub = r.name === 'team' ? (app.project || '') : r.name === 'run' && S.run ? S.run.id : '';
        return h('header', { class: 'main-head', key: 'mh' },
            h('h1', null, title), sub ? h('span', { class: 'main-sub mono' }, sub) : null, h('span', { class: 'spacer' }),
            r.name === 'runs' ? P.btn('Refresh', () => SC.loadRuns(), { sm: true, kind: 'ghost', icon: 'refresh' }) : null,
            mqWide.matches && (r.name === 'team') ? P.iconBtn('panel', () => M.store.setPref('panelOpen', !S.ui.panelOpen), S.ui.panelOpen ? 'Hide side panel' : 'Show side panel') : null,
            h('button', { type: 'button', class: 'kbd-hint', onclick: () => openPalette() }, icon('search'), h('span', null, 'Search'), h('kbd', null, isMac() ? '⌘K' : 'Ctrl K')));
    }

    function rightPanel(r) {
        const fid = r.name === 'agent' ? r.id : focusAgentId();
        const a = fid !== null ? M.store.agent(fid) : null;
        const tab = S.ui.panel;
        const tabs = [['plan', 'Plan'], ['pulse', 'Pulse'], ['files', 'Files'], ['context', 'Context']];
        let body;
        if (tab === 'pulse') body = SC.pulseList(120, 'all') || P.empty('pulse', 'No pulse yet', S.run ? 'Run events appear here.' : 'Start a run to see its journal.');
        else if (tab === 'files') body = a ? filesPanel(a) : P.empty('file', 'No agent selected');
        else if (tab === 'context') body = a ? contextPanel(a) : P.empty('info', 'No agent selected');
        else body = planPanel(a);
        return h('aside', { class: 'panel', key: 'panel', 'aria-label': 'Details' },
            h('div', { class: 'panel-tabs', role: 'tablist' },
                tabs.map(([id, label]) => h('button', { type: 'button', key: id, role: 'tab', 'aria-selected': String(tab === id), class: 'ptab' + (tab === id ? ' on' : ''), onclick: () => M.store.setPref('panel', id) }, label)),
                h('span', { class: 'spacer' }),
                P.iconBtn('close', () => M.store.setPref('panelOpen', false), 'Hide panel')),
            h('div', { class: 'panel-body', key: 'pb-' + tab }, body));
    }
    function planPanel(a) {
        const kids = [];
        if (a && (a.plan || []).length) {
            kids.push(h('div', { class: 'panel-sec', key: 'steps' }, h('h3', { class: 'group-title' }, (a.worker ? a.worker.task_id : a.name) + ' · steps ' + a.plan_done + '/' + a.plan_total), stepsList(a.plan)));
        }
        const plan = S.plan;
        if (plan) {
            kids.push(h('div', { class: 'panel-sec', key: 'plan' }, h('h3', { class: 'group-title' }, 'Run plan · v' + plan.version),
                h('div', { class: 'panel-plan-title' }, plan.title),
                h('ol', { class: 'mini-phases' }, plan.phases.map((ph, i) => {
                    const st = S.run && S.run.stage;
                    const cur = st && st.kind === 'phase' ? st.phase : st && (st.kind === 'finale' || st.kind === 'done') ? 99 : -1;
                    return h('li', { key: i, class: i < cur ? 'done' : i === cur ? 'cur' : null }, h('b', null, (i < cur ? '✓ ' : '') + ph.name), h('span', { class: 'dim' }, ' · ' + F.plural(ph.tasks.length, 'task')));
                })),
                P.btn('Open plan', () => nav('/run'), { sm: true, kind: 'ghost', icon: 'plan' })));
        }
        if (!kids.length) return P.empty('plan', 'No plan', a ? (a.name + ' has no step list right now.') : 'Plans appear when an agent or a run makes one.');
        return kids;
    }
    function stepsList(steps) {
        return h('ol', { class: 'steps' }, steps.map((s, i) => h('li', { key: i, class: 'step ' + (s.status || '') },
            h('span', { class: 'step-ico' }, s.status === 'completed' ? '✓' : s.status === 'inProgress' || s.status === 'in_progress' ? '◐' : '○'), h('span', null, s.text))));
    }
    function filesPanel(a) {
        const files = a.files || [];
        if (!files.length) return P.empty('file', 'No changes yet', a.name + ' hasn’t edited any files.');
        return [h('div', { class: 'panel-sum', key: 'sum' }, F.plural(files.length, 'file'), ' · ', h('span', { class: 'add' }, '+' + a.files_adds), ' ', h('span', { class: 'del' }, '−' + a.files_dels),
            h('span', { class: 'spacer' }), P.btn('All diffs', () => sheet({ kind: 'diff', agent: a.id }), { sm: true, kind: 'ghost', icon: 'diff' })),
        h('div', { class: 'file-list', key: 'fl' }, files.map((f) => h('button', { type: 'button', key: f.path, class: 'file-row', onclick: () => sheet({ kind: 'diff', agent: a.id, path: f.path }) },
            h('span', { class: 'fkind k-' + (f.kind || 'update') }, (f.kind || 'update')[0].toUpperCase()), h('span', { class: 'fpath' }, f.path),
            h('span', { class: 'fnum' }, h('span', { class: 'add' }, '+' + f.adds), ' ', h('span', { class: 'del' }, '−' + f.dels)))))];
    }
    function contextPanel(a) {
        const w = a.worker;
        const rows = [
            ['Model', a.model_alias + (a.model && a.model !== a.model_alias ? ' (' + a.model + ')' : '')],
            ['Provider', a.provider_name || a.provider],
            ['Backend', a.backend === 'claude-code' ? 'Claude Code' : 'Codex'],
            ['Effort', a.effort],
            ['Context', a.ctx_percent !== undefined ? a.ctx_percent + '% of ' + F.tokens(a.ctx_window) + (a.ctx_assumed ? ' (assumed)' : '') : '—'],
            ['Compacts at', a.compact_percent ? a.compact_percent + '%' : null],
            ['Tokens', F.tokens(a.tokens_total)],
            ['Turns', String(a.turn_count || 0)],
            w ? ['Task', w.task_id + ' · attempt ' + w.attempt] : null,
            w ? ['Branch', w.branch] : null,
            w && w.tripwires && w.tripwires.length ? ['Tripwires', w.tripwires.join(', ')] : null,
            ['Started', F.ago(a.created_at)],
            ['Working dir', a.cwd],
            a.thread_id ? ['Thread', a.thread_id] : null,
        ].filter((x) => x && x[1]);
        return [
            h('div', { class: 'panel-sec', key: 'gauge' }, P.ctxGauge(a, true)),
            h('dl', { class: 'kv', key: 'kv' }, rows.map(([k, v]) => [h('dt', { key: 'k' + k }, k), h('dd', { key: 'v' + k, class: k === 'Working dir' || k === 'Thread' || k === 'Branch' ? 'mono' : null }, v)])),
            w && w.report_tail ? h('div', { class: 'panel-sec', key: 'rep' }, h('h3', { class: 'group-title' }, 'Latest report'), h('div', { class: 'md small' }, F.markdown(w.report_tail))) : null,
        ];
    }

    function deskShell(r) {
        const showPanel = mqWide.matches && S.ui.panelOpen && (r.name === 'agent' || r.name === 'team');
        return h('div', { class: 'shell desk' + (showPanel ? ' with-panel' : ''), key: 'desk' },
            sidebar(r),
            h('main', { class: 'main', key: 'main' },
                connBar(),
                mainHeader(r),
                BANNER_ROUTES[r.name] ? banners() : null,
                h('div', { class: 'main-body' + (r.name === 'agent' ? ' fill' : ''), key: 'mb' },
                    h('div', { class: 'screen anim-' + (motion() ? 'fade' : 'none') + (r.name === 'agent' ? ' fill' : ''), key: 'scr-' + r.name + (r.id !== undefined ? r.id : '') }, screenFor(r))),
                r.name === 'agent' && mqWide.matches && !S.ui.panelOpen ? h('button', { type: 'button', class: 'panel-reopen', title: 'Show side panel', onclick: () => M.store.setPref('panelOpen', true) }, icon('panel')) : null),
            showPanel ? rightPanel(r) : null);
    }

    function Root() {
        const r = S.ui.route;
        let body;
        if (r.name === 'login') body = SC.loginScreen();
        else if (r.name === 'connect') body = SC.connectScreen();
        else body = wide() ? deskShell(r) : phoneShell(r);
        return [body, sheetLayer(), paletteLayer(), flashLayer()];
    }

    // ── flashes + server toast ───────────────────────────────────────────────────────────────────
    function flashLayer() {
        const now = Date.now();
        const list = (S.ui.flashes || []).slice();
        const t = S.toast;
        if (t && t.shown + (t.ttl_ms || 4000) > now && !(S.ui.flashes || []).some((f) => f.text === t.text)) list.unshift({ id: 'srv' + t.at, text: t.text, level: t.level, at: t.at });
        return h('div', { class: 'flashes' + (S.ui.route.name === 'agent' && !wide() ? ' above-composer' : ''), key: 'flashes', 'aria-live': 'polite' },
            list.map((f) => h(f.action ? 'button' : 'div', { type: f.action ? 'button' : null, key: f.id, class: 'flash lvl-' + f.level, onclick: f.action ? () => { S.ui.flashes = S.ui.flashes.filter((x) => x.id !== f.id); f.action(); } : null },
                h('span', { class: 'flash-dot' }), h('span', null, f.text), f.action ? icon('chevron', 'chev') : null)));
    }

    // ── sheets ───────────────────────────────────────────────────────────────────────────────────
    async function loadDiff(sh) {
        sh.loading = true; sh.error = null;
        try {
            const d = await request('diff', sh.path ? { agent: sh.agent, path: sh.path } : { agent: sh.agent });
            sh.files = (d && d.files) || [];
            if (sh.path && !sh.sel) sh.sel = sh.path;
            // Asked for one file: also fetch the list so the person can move between files.
            if (sh.path) request('diff', { agent: sh.agent }).then((all) => { if (S.ui.sheet === sh && all && all.files) { const one = sh.files[0]; sh.files = all.files.map((f) => (one && f.path === one.path ? one : f)); changed(); } }, () => { });
        } catch (e) { sh.error = e.message; }
        sh.loading = false;
        changed();
    }
    function sheetLayer() {
        const sh = S.ui.sheet;
        if (!sh) return null;
        let title = '', body = null, cls = '';
        const a = sh.agent !== undefined ? M.store.agent(sh.agent) : null;
        switch (sh.kind) {
            case 'actions': {
                if (!a) break;
                title = a.worker ? a.worker.task_id : a.name;
                body = [
                    h('div', { class: 'sheet-meta', key: 'meta' }, P.modelChip(a), P.ctxGauge(a), P.statusPill(a)),
                    h('div', { class: 'action-grid', key: 'grid' }, AV.actionList(a).map((x) => h('button', { type: 'button', key: x.id, class: 'action-tile', disabled: x.disabled || S.conn.state !== 'open' || null, onclick: () => { if (!['model', 'effort', 'diff', 'plan'].includes(x.id)) closeSheet(); x.run(); } }, icon(x.icon), h('span', null, x.label)))),
                    P.toggle(S.ui.verbose, (on) => M.store.setPref('verbose', on), 'Verbose transcript', 'expand reasoning and command output', { key: 'verbose' }),
                    a.worker ? h('dl', { class: 'kv', key: 'kv' }, [['Task', a.worker.title], ['Attempt', String(a.worker.attempt)], ['Branch', a.worker.branch]].filter((x) => x[1]).map(([k, v]) => [h('dt', { key: 'k' + k }, k), h('dd', { key: 'v' + k }, v)])) : null,
                ];
                break;
            }
            case 'model': {
                if (!a) break;
                title = 'Model for ' + a.name;
                const models = (S.app && S.app.models) || [];
                body = h('div', { class: 'pick-list' }, models.length ? models.map((m) => h('button', {
                    type: 'button', key: m.alias, class: 'pick' + (m.alias === a.model_alias ? ' on' : '') + (m.problem ? ' problem' : ''),
                    onclick: () => { closeSheet(); if (m.alias !== a.model_alias) cmd('set_model', { agent: a.id, alias: m.alias }, { ok: a.name + ' → ' + m.alias }); },
                }, h('span', { class: 'pick-main' }, h('b', null, m.alias), h('span', { class: 'dim mono sm' }, ' ' + m.model)),
                    h('span', { class: 'pick-sub' }, [m.provider_name, m.backend === 'claude-code' ? 'Claude Code' : null, m.context_window ? F.tokens(m.context_window) + ' ctx' : null, m.note].filter(Boolean).join(' · ')),
                    m.problem ? h('span', { class: 'pick-problem' }, m.problem) : null,
                    m.alias === a.model_alias ? icon('check', 'pick-check') : null)) : P.empty('model', 'No models', 'The model registry is empty.'));
                break;
            }
            case 'effort': {
                if (!a) break;
                title = 'Effort for ' + a.name;
                const efforts = a.efforts || [];
                body = efforts.length ? h('div', { class: 'pick-list' }, efforts.map((e) => h('button', {
                    type: 'button', key: e, class: 'pick' + (e === a.effort ? ' on' : ''),
                    onclick: () => { closeSheet(); if (e !== a.effort) cmd('set_effort', { agent: a.id, effort: e }, { ok: 'Effort ' + e }); },
                }, h('span', { class: 'pick-main' }, P.effortBar(e, efforts)), e === a.effort ? icon('check', 'pick-check') : null))) : P.empty('model', 'No effort levels', 'This model has no effort levels.');
                break;
            }
            case 'steps': {
                if (!a) break;
                title = 'Steps · ' + a.plan_done + '/' + a.plan_total;
                body = (a.plan || []).length ? stepsList(a.plan) : P.empty('plan', 'No steps');
                break;
            }
            case 'diff': {
                cls = 'wide';
                title = 'Changes' + (a ? ' · ' + (a.worker ? a.worker.task_id : a.name) : '');
                if (sh.loading && !sh.files) body = P.loading('Loading diff…');
                else if (sh.error) body = P.errorBox('Could not load the diff: ' + sh.error, () => loadDiff(sh));
                else if (!sh.files || !sh.files.length) body = P.empty('diff', 'No changes', 'Nothing to show.');
                else {
                    const sel = sh.sel && sh.files.find((f) => f.path === sh.sel) ? sh.sel : sh.files[0].path;
                    const f = sh.files.find((x) => x.path === sel);
                    body = [
                        h('div', { class: 'diff-files', key: 'df', role: 'tablist' }, sh.files.map((x) => h('button', { type: 'button', key: x.path, role: 'tab', 'aria-selected': String(x.path === sel), class: 'diff-file' + (x.path === sel ? ' on' : ''), onclick: () => { sh.sel = x.path; changed(); } },
                            h('span', { class: 'fpath' }, x.path.split('/').pop()), h('span', { class: 'fnum' }, h('span', { class: 'add' }, '+' + (x.adds || 0)), ' ', h('span', { class: 'del' }, '−' + (x.dels || 0)))))),
                        h('div', { class: 'diff-path mono', key: 'dp' }, f.path),
                        f.diff ? h('pre', { class: 'diff', key: 'd-' + f.path }, F.diffLines(f.diff)) : h('div', { class: 'dim', key: 'nd' }, 'No diff text for this file (binary, or not loaded).'),
                    ];
                }
                break;
            }
            case 'confirm': {
                title = sh.title;
                body = [h('p', { class: 'sheet-text', key: 't' }, sh.body), h('div', { class: 'sheet-actions', key: 'a' },
                    P.btn('Cancel', closeSheet, { kind: 'ghost' }),
                    P.btn(sh.confirm || 'OK', () => { closeSheet(); Promise.resolve(sh.run()).catch(() => { }); }, { kind: sh.danger ? 'danger' : 'primary' }))];
                cls = 'small';
                break;
            }
        }
        if (!body) { S.ui.sheet = null; return null; }
        return h('div', { class: 'sheet-layer', key: 'sheet-' + sh.kind, onclick: (e) => { if (e.target === e.currentTarget) closeSheet(); } },
            h('div', { class: 'sheet ' + cls, role: 'dialog', 'aria-modal': 'true', 'aria-label': title, ref: (el) => requestAnimationFrame(() => { const f = el.querySelector('button.on, button, input'); if (f && wide()) f.focus({ preventScroll: true }); }) },
                h('div', { class: 'sheet-grab', 'aria-hidden': 'true' }),
                h('div', { class: 'sheet-head' }, h('h2', null, title), P.iconBtn('close', closeSheet, 'Close')),
                h('div', { class: 'sheet-body' }, body)));
    }

    // ── command palette (Ctrl/⌘+K) ───────────────────────────────────────────────────────────────
    function openPalette() { S.ui.palette = { q: '', sel: 0 }; changed(); requestAnimationFrame(() => { const i = document.querySelector('.palette input'); if (i) i.focus(); }); }
    function paletteItems(q) {
        const items = [];
        for (const a of M.store.agentList()) items.push({ key: 'a' + a.id, label: a.worker ? a.worker.task_id : a.name, sub: P.status(a).text, glyph: a, run: () => nav('/agent/' + a.id) });
        const go = [['/', 'Overview', 'team'], ['/run', 'Run', 'run'], ['/inbox', 'Inbox', 'inbox'], ['/pulse', 'Pulse', 'pulse'], ['/runs', 'Runs', 'runs'], ['/settings', 'Settings', 'settings']];
        for (const [p, l, i] of go) items.push({ key: 'go' + p, label: 'Go to ' + l, icon: i, run: () => nav(p) });
        const run = S.run;
        if (run && run.stage.kind === 'review') items.push({ key: 'approve', label: 'Approve plan', icon: 'check', run: () => cmd('plan_approve', {}, { ok: 'Plan approved' }) });
        if (run && run.stage.kind !== 'done') items.push({ key: 'pause', label: run.halted ? 'Resume run' : 'Pause run', icon: run.halted ? 'play' : 'pause', run: () => cmd('pause_resume', {}) });
        if (run && run.landable) items.push({ key: 'land', label: 'Land run', icon: 'land', run: () => sheet({ kind: 'confirm', title: 'Land this run?', body: 'Merges ' + run.branch + ' into your current branch.', confirm: 'Land', run: () => cmd('land', {}, { ok: 'Landing…' }) }) });
        items.push({ key: 'solo', label: 'New Solo session', icon: 'plus', run: () => cmd('new_solo', {}, { ok: 'New session' }).then((d) => d && nav('/agent/' + d.agent)) });
        items.push({ key: 'theme', label: 'Toggle dark / light', icon: 'sun', run: () => { const dark = S.ui.theme === 'dark' || (S.ui.theme === 'system' && mqDark.matches); M.store.setPref('theme', dark ? 'light' : 'dark'); applyPrefs(); } });
        items.push({ key: 'verbose', label: (S.ui.verbose ? 'Hide' : 'Show') + ' full reasoning & output', icon: 'eye', run: () => M.store.setPref('verbose', !S.ui.verbose) });
        if (mqWide.matches) items.push({ key: 'panel', label: (S.ui.panelOpen ? 'Hide' : 'Show') + ' side panel', icon: 'panel', run: () => M.store.setPref('panelOpen', !S.ui.panelOpen) });
        const ql = q.trim().toLowerCase();
        if (!ql) return items;
        return items.filter((x) => (x.label + ' ' + (x.sub || '')).toLowerCase().includes(ql));
    }
    function paletteLayer() {
        const pl = S.ui.palette;
        if (!pl) return null;
        const items = paletteItems(pl.q).slice(0, 40);
        const sel = Math.min(pl.sel, Math.max(0, items.length - 1));
        const pick = (x) => { S.ui.palette = null; changed(); x.run(); };
        return h('div', { class: 'sheet-layer palette-layer', key: 'palette', onclick: (e) => { if (e.target === e.currentTarget) { S.ui.palette = null; changed(); } } },
            h('div', { class: 'palette', role: 'dialog', 'aria-label': 'Command palette' },
                h('div', { class: 'palette-input' }, icon('search'), h('input', {
                    value: pl.q, placeholder: 'Jump to an agent or run a command…', 'aria-label': 'Search',
                    oninput: (e) => { pl.q = e.target.value; pl.sel = 0; changed(); },
                    onkeydown: (e) => {
                        if (e.key === 'ArrowDown') { e.preventDefault(); pl.sel = (sel + 1) % Math.max(1, items.length); changed(); }
                        else if (e.key === 'ArrowUp') { e.preventDefault(); pl.sel = (sel + items.length - 1) % Math.max(1, items.length); changed(); }
                        else if (e.key === 'Enter' && items[sel]) { e.preventDefault(); pick(items[sel]); }
                    },
                })),
                h('div', { class: 'palette-list', role: 'listbox' }, items.length ? items.map((x, i) => h('button', { type: 'button', key: x.key, role: 'option', 'aria-selected': String(i === sel), class: 'pal-item' + (i === sel ? ' on' : ''), onmousemove: () => { if (pl.sel !== i) { pl.sel = i; changed(); } }, onclick: () => pick(x) },
                    x.glyph ? P.glyph(x.glyph) : h('span', { class: 'pal-ico' }, icon(x.icon)), h('span', { class: 'pal-label' }, x.label), x.sub ? h('span', { class: 'pal-sub' }, x.sub) : null)) : h('div', { class: 'pal-empty' }, 'Nothing matches'))));
    }

    // ── keyboard ─────────────────────────────────────────────────────────────────────────────────
    function typing(el) { return el && (el.tagName === 'INPUT' || el.tagName === 'TEXTAREA' || el.tagName === 'SELECT' || el.isContentEditable); }
    document.addEventListener('keydown', (e) => {
        if ((e.ctrlKey || e.metaKey) && (e.key === 'k' || e.key === 'K')) { e.preventDefault(); if (S.ui.palette) { S.ui.palette = null; changed(); } else openPalette(); return; }
        if (e.key === 'Escape') {
            if (S.ui.palette) { S.ui.palette = null; changed(); e.preventDefault(); return; }
            if (S.ui.sheet) { closeSheet(); e.preventDefault(); return; }
        }
        if (typing(e.target) || e.ctrlKey || e.metaKey || e.altKey) return;
        if (e.key === '[' || e.key === ']') {
            const ids = M.store.agentList().map((a) => a.id);
            if (!ids.length) return;
            const cur = S.ui.route.name === 'agent' ? ids.indexOf(S.ui.route.id) : -1;
            const next = e.key === ']' ? (cur + 1) % ids.length : (cur <= 0 ? ids.length - 1 : cur - 1);
            nav('/agent/' + ids[next]);
            e.preventDefault();
        }
    });

    // ── phone viewport / software keyboard ───────────────────────────────────────────────────────
    function onViewport() {
        const vv = window.visualViewport;
        if (!vv) return;
        document.documentElement.style.setProperty('--app-h', vv.height + 'px');
        const kb = !wide() && window.innerHeight - vv.height > 120;
        if (kb !== S.ui.kb) { S.ui.kb = kb; changed(); }
        // iOS scrolls the layout viewport to reveal the focused field; keep the app pinned.
        if (vv.offsetTop > 0 && !wide()) window.scrollTo(0, 0);
    }
    if (window.visualViewport) {
        window.visualViewport.addEventListener('resize', onViewport);
        window.visualViewport.addEventListener('scroll', onViewport);
    }
    for (const mq of [mqWide, mqMid]) mq.addEventListener('change', changed);
    mqReduce.addEventListener('change', applyPrefs);
    mqDark.addEventListener('change', applyPrefs);

    // ── platform bits ────────────────────────────────────────────────────────────────────────────
    function isIOS() { return /iPad|iPhone|iPod/.test(navigator.userAgent) || (navigator.platform === 'MacIntel' && navigator.maxTouchPoints > 1); }
    function isMac() { return /Mac/.test(navigator.platform || navigator.userAgent); }
    function standalone() { return matchMedia('(display-mode: standalone)').matches || navigator.standalone === true; }
    function deviceName() {
        const ua = navigator.userAgent;
        const os = /iPhone/.test(ua) ? 'iPhone' : /iPad/.test(ua) || isIOS() ? 'iPad' : /Android/.test(ua) ? 'Android' : /Mac/.test(ua) ? 'Mac' : /Windows/.test(ua) ? 'Windows' : /Linux/.test(ua) ? 'Linux' : 'device';
        const br = /Edg\//.test(ua) ? 'Edge' : /Firefox\//.test(ua) ? 'Firefox' : /Chrome\//.test(ua) ? 'Chrome' : /Safari\//.test(ua) ? 'Safari' : 'browser';
        return os + ' · ' + br + (standalone() ? ' (app)' : '');
    }
    window.addEventListener('beforeinstallprompt', (e) => { e.preventDefault(); S.ui.install = e; changed(); });
    window.addEventListener('appinstalled', () => { S.ui.install = null; flash('Installed', 'ok'); });

    // ── service worker ───────────────────────────────────────────────────────────────────────────
    let swReg = null;
    function registerSW() {
        if (!('serviceWorker' in navigator)) return;
        // Service workers need a secure context; http://localhost counts, a LAN IP over http doesn't.
        if (!window.isSecureContext) return;
        navigator.serviceWorker.register('/sw.js', { scope: '/' }).then((reg) => {
            swReg = reg;
            const track = (w) => { if (!w) return; w.addEventListener('statechange', () => { if (w.state === 'installed' && navigator.serviceWorker.controller) { S.ui.swUpdate = w; changed(); } }); };
            track(reg.installing);
            reg.addEventListener('updatefound', () => track(reg.installing));
            if (reg.waiting && navigator.serviceWorker.controller) { S.ui.swUpdate = reg.waiting; changed(); }
            if (base) reg.active && reg.active.postMessage({ type: 'base', base });
            M.push.sync();
        }).catch(() => { });
        let reloading = false;
        navigator.serviceWorker.addEventListener('controllerchange', () => { if (S.ui.swUpdate && !reloading) { reloading = true; location.reload(); } });
        navigator.serviceWorker.addEventListener('message', (ev) => {
            const d = ev.data || {};
            if (d.navigate) navFromUrl(d.navigate);
            if (d.push) { const n = d.push; flash(n.title + (n.body ? ' — ' + F.trunc(n.body, 80) : ''), n.kind === 'halt' ? 'error' : 'warn', n.url ? () => navFromUrl(n.url) : null); }
        });
    }
    function navFromUrl(url) {
        try {
            let p = new URL(url, location.origin).pathname;
            if (base && p.startsWith(base)) p = p.slice(base.length) || '/';
            nav(p);
        } catch (_) { }
    }

    // ── Web Push ─────────────────────────────────────────────────────────────────────────────────
    const DEFAULT_PREFS = { halt: true, question: true, approval: true, review: true, done: true, turn: false };
    const pushKey = () => 'push.' + (MODE === 'relay' ? base : 'local');
    M.push = {
        busy: false,
        state() {
            const hl = S.hello || {};
            const saved = M.store.load(pushKey(), null);
            return {
                serverPush: hl.push !== undefined ? !!hl.push : !!(S.app && S.app.push_enabled),
                secure: window.isSecureContext,
                supported: 'serviceWorker' in navigator && 'PushManager' in window && 'Notification' in window,
                iosNeedsInstall: isIOS() && !standalone(),
                permission: 'Notification' in window ? Notification.permission : 'denied',
                subscribed: !!(saved && saved.endpoint && S.ui.pushLive !== false),
                endpoint: saved && saved.endpoint,
                prefs: Object.assign({}, DEFAULT_PREFS, saved && saved.prefs),
                device: saved && saved.device,
                busy: this.busy,
            };
        },
        async registration() {
            if (swReg) return swReg;
            return navigator.serviceWorker.ready;
        },
        // On every (re)connect: make sure the server still has this device's subscription, and
        // that the browser still has it (it can be dropped when permission is revoked).
        async sync() {
            const saved = M.store.load(pushKey(), null);
            if (!saved || !saved.endpoint || S.conn.state !== 'open' || !('PushManager' in window)) return;
            try {
                const reg = await this.registration();
                const sub = await reg.pushManager.getSubscription();
                if (!sub || sub.endpoint !== saved.endpoint || Notification.permission !== 'granted') { M.store.save(pushKey(), null); S.ui.pushLive = false; changed(); return; }
                S.ui.pushLive = true;
                await request('push_subscribe', { subscription: sub.toJSON(), device: saved.device || deviceName(), prefs: Object.assign({}, DEFAULT_PREFS, saved.prefs) });
            } catch (_) { }
        },
        async enable() {
            const hl = S.hello || {};
            if (!hl.vapid) { flash('This Mantra has no push key yet — restart it with push enabled', 'error'); return; }
            this.busy = true; changed();
            try {
                const perm = await Notification.requestPermission();
                if (perm !== 'granted') { flash(perm === 'denied' ? 'Notifications are blocked for this site' : 'Notifications not allowed', 'warn'); return; }
                const reg = await this.registration();
                let sub = await reg.pushManager.getSubscription();
                const key = C.unb64url(hl.vapid);
                if (sub && sub.options && sub.options.applicationServerKey) {
                    const cur = new Uint8Array(sub.options.applicationServerKey);
                    if (cur.length !== key.length || cur.some((b, i) => b !== key[i])) { await sub.unsubscribe(); sub = null; }
                }
                if (!sub) sub = await reg.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: key });
                const device = deviceName();
                await request('push_subscribe', { subscription: sub.toJSON(), device, prefs: DEFAULT_PREFS });
                M.store.save(pushKey(), { endpoint: sub.endpoint, device, prefs: DEFAULT_PREFS });
                S.ui.pushLive = true;
                flash('Notifications on for this device', 'ok');
            } catch (e) {
                flash('Could not turn on notifications: ' + (e.message || e), 'error');
            } finally { this.busy = false; changed(); }
        },
        async disable() {
            const st = this.state();
            this.busy = true; changed();
            try {
                if (st.endpoint) await request('push_unsubscribe', { endpoint: st.endpoint }).catch(() => { });
                const reg = await this.registration();
                const sub = await reg.pushManager.getSubscription();
                if (sub) await sub.unsubscribe();
            } catch (_) { }
            M.store.save(pushKey(), null);
            this.busy = false;
            flash('Notifications off for this device', 'info');
        },
        async setPref(k, on) {
            const saved = M.store.load(pushKey(), null);
            if (!saved) return;
            saved.prefs = Object.assign({}, DEFAULT_PREFS, saved.prefs, { [k]: on });
            M.store.save(pushKey(), saved);
            changed();
            try { await request('push_prefs', { endpoint: saved.endpoint, prefs: saved.prefs }); } catch (e) { flash(e.message, 'error'); }
        },
        async test() {
            const st = this.state();
            if (!st.endpoint) return;
            try { await cmd('push_test', { endpoint: st.endpoint }, { busyKey: 'push-test' }); flash('Test sent — it should arrive in a few seconds', 'ok'); } catch (_) { }
        },
    };

    // ── local mode: session + login ──────────────────────────────────────────────────────────────
    async function session() {
        const r = await fetch('/api/session', { credentials: 'same-origin', cache: 'no-store' });
        if (!r.ok) throw new Error('HTTP ' + r.status);
        return r.json();
    }
    async function login(pw) {
        try {
            const r = await fetch('/api/login', { method: 'POST', credentials: 'same-origin', headers: { 'Content-Type': 'application/json', 'X-Mantra': '1' }, body: JSON.stringify({ password: pw }) });
            if (r.status === 404) { startLocal(); return { ok: true }; }
            let j = {};
            try { j = await r.json(); } catch (_) { }
            if (r.ok) { S.ui.needsLogin = true; startLocal(); return { ok: true }; }
            if (r.status === 429) return { ok: false, error: j.error || ('Too many attempts — try again in ' + (j.retry_after || 60) + ' s') };
            if (r.status === 401) return { ok: false, error: 'Wrong password' };
            return { ok: false, error: j.error || ('Login failed (HTTP ' + r.status + ')') };
        } catch (e) {
            return { ok: false, error: 'Can’t reach Mantra — is it still running?' };
        }
    }
    async function logout() {
        try { await fetch('/api/logout', { method: 'POST', credentials: 'same-origin', headers: { 'X-Mantra': '1' } }); } catch (_) { }
        if (client) client.stop();
        client = null;
        S.synced = false;
        S.ui.route = { name: 'login' };
        history.replaceState({ d: 0 }, '', '/login');
        changed();
    }

    function wireClient(c) {
        client = c;
        c.emit = (type, m) => {
            switch (type) {
                case 'state':
                    S.conn.state = m.state; S.conn.error = m.error;
                    if (m.state === 'auth') {
                        S.ui.needsLogin = true;
                        S.ui.route = { name: 'login' };
                        history.replaceState({ d: 0 }, '', '/login');
                    }
                    if (m.state === 'open') { if (S.ui.route.name === 'runs') SC.loadRuns(); M.push.sync(); if (M.relay.onOpen) M.relay.onOpen(); }
                    if (MODE === 'relay' && M.relay.onState) M.relay.onState(m);
                    break;
                case 'retry': S.conn.retryAt = m.at; break;
                case 'hello': S.hello = m; break;
                case 'protocol': flash('This page speaks protocol 1 but Mantra speaks ' + m.protocol + ' — reload to update', 'warn'); break;
                case 'snapshot':
                    M.store.applySnapshot(m);
                    if (S.ui.route.name === 'agent' && S.ui.route.id !== undefined) delete S.ui.unread[S.ui.route.id];
                    break;
                case 'delta': {
                    const focus = S.ui.route.name === 'agent' ? S.ui.route.id : null;
                    if (!M.store.applyDelta(m, focus)) { c.synced = false; c.sendHello(); }
                    break;
                }
                case 'note': onNote(m); break;
                case 'fatal': if (M.relay.onFatal) M.relay.onFatal(m); break;
            }
            changed();
        };
    }

    const NOTE_ROUTE = { halt: '/run', question: '/run', review: '/run', done: '/run', failed: '/run' };
    function onNote(n) {
        M.store.addNote(n);
        const path = n.kind === 'approval' ? (n.agent !== undefined ? '/agent/' + n.agent : '/inbox') : n.kind === 'turn' && n.agent !== undefined ? '/agent/' + n.agent : NOTE_ROUTE[n.kind];
        const text = String(n.text || '').replace(/^Mantra(:| needs you:)\s*/, '');
        const onScreen = path && location.pathname === base + path;
        if (!onScreen && n.kind !== 'info') flash(text, n.kind === 'halt' || n.kind === 'failed' ? 'error' : n.kind === 'done' ? 'ok' : 'warn', path ? () => nav(path) : null);
        // Tab in the background and no push on this device: a plain local notification.
        if (document.visibilityState === 'hidden' && !M.push.state().subscribed && 'Notification' in window && Notification.permission === 'granted' && swReg && n.kind !== 'info') {
            try { swReg.showNotification('Mantra', { body: text, tag: n.kind, icon: '/icons/icon-192.png', badge: '/icons/badge-96.png', data: { url: base + (path || '/') } }); } catch (_) { }
        }
    }

    function startLocal() {
        if (client) client.stop();
        const proto = location.protocol === 'https:' ? 'wss:' : 'ws:';
        const t = new T.LocalTransport(proto + '//' + location.host + '/ws');
        const c = new T.Client({
            transport: t, mode: 'local', clientKind: standalone() ? 'pwa' : 'web', onEvent: () => { },
            checkAuth: async () => { const s = await session(); return !!s.authenticated; },
        });
        wireClient(c);
        if (S.ui.route.name === 'login') { const r = parse(location.pathname); S.ui.route = r.name === 'login' ? { name: 'team' } : r; history.replaceState({ d: 0 }, '', r.name === 'login' ? '/' : location.pathname); }
        c.start();
        changed();
    }

    async function bootLocal() {
        S.ui.route = parse(location.pathname);
        let sess = null;
        try { sess = await session(); } catch (_) { /* server down: start anyway, the client retries */ }
        if (sess) {
            S.ui.sessionInfo = sess;
            if (!sess.loopback) S.ui.needsLogin = true;
            if (!sess.authenticated) {
                S.ui.route = { name: 'login' };
                history.replaceState({ d: 0 }, '', '/login');
                changed();
                return;
            }
        }
        startLocal();
        if (location.protocol === 'https:') {
            fetch('/cert.pem', { method: 'HEAD', cache: 'no-store' }).then((r) => { S.ui.certAvailable = r.ok; changed(); }, () => { });
        }
    }

    // ── relay mode ───────────────────────────────────────────────────────────────────────────────
    const IDB = {
        open() {
            return new Promise((res, rej) => {
                if (!window.indexedDB) { rej(new Error('no IndexedDB')); return; }
                const r = indexedDB.open('mantra', 1);
                r.onupgradeneeded = () => r.result.createObjectStore('devices', { keyPath: 'sid' });
                r.onsuccess = () => res(r.result);
                r.onerror = () => rej(r.error);
            });
        },
        async tx(mode, fn) {
            const db = await this.open();
            return new Promise((res, rej) => {
                const t = db.transaction('devices', mode);
                const st = t.objectStore('devices');
                const out = fn(st);
                t.oncomplete = () => { res(out && out.result !== undefined ? out.result : undefined); db.close(); };
                t.onerror = () => { rej(t.error); db.close(); };
            });
        },
        all() { return this.tx('readonly', (s) => s.getAll()).catch(() => []); },
        get(sid) { return this.tx('readonly', (s) => s.get(sid)).catch(() => null); },
        put(v) { return this.tx('readwrite', (s) => s.put(v)); },
        del(sid) { return this.tx('readwrite', (s) => s.delete(sid)).catch(() => { }); },
    };

    function validRelay(u) { return typeof u === 'string' && /^wss:\/\/[^\s/]+/i.test(u) || /^ws:\/\/(localhost|127\.0\.0\.1|\[::1\])(:\d+)?/i.test(u || ''); }

    M.relay = {
        sid: null, raw: null, relay: null, pendingSid: null, offerRemember: false,
        defaultRelay() { return validRelay(CFG.relay) ? CFG.relay : 'wss://remote.mantra.codes'; },
        async boot() {
            const m = /^\/s\/([a-z2-7]{26})(\/.*)?$/i.exec(location.pathname);
            const hash = new URLSearchParams(location.hash.replace(/^#/, ''));
            // A link may name its relay (#r=wss://…) so the site and the relay can live on
            // different hosts; the page's own config is the default.
            const relay = validRelay(hash.get('r')) ? hash.get('r') : this.defaultRelay();
            S.ui.devices = await IDB.all();
            if (!m) { this.toConnect(); return; }
            const sid = m[1].toLowerCase();
            base = '/s/' + sid;
            S.ui.route = parse(location.pathname);
            const k = hash.get('k');
            if (k) {
                let raw;
                try { raw = C.unb64url(k); } catch (_) { raw = null; }
                // Drop the key from the address bar and history as soon as we have it.
                history.replaceState({ d: 0 }, '', location.pathname);
                if (raw && raw.length === 32) {
                    this.offerRemember = !S.ui.devices.some((d) => d.sid === sid);
                    await this.start(sid, raw, null, relay);
                    return;
                }
                this.pendingSid = sid;
                S.ui.connect = { code: '', pw: '', remember: false, error: 'That link is damaged — enter the password instead.', step: null };
                S.ui.route = { name: 'connect' };
                changed();
                return;
            }
            const ss = sessionGet(sid);
            if (ss) { await this.start(sid, ss.raw, null, ss.relay || relay); return; }
            const saved = S.ui.devices.find((d) => d.sid === sid);
            if (saved) { await this.start(sid, null, saved.key, saved.relay || relay, true); return; }
            this.pendingSid = sid;
            S.ui.route = { name: 'connect' };
            changed();
        },
        toConnect(err) {
            if (client) client.stop();
            client = null;
            S.synced = false;
            if (!this.pendingSid) { base = ''; history.replaceState({ d: 0 }, '', '/'); }
            S.ui.connect = Object.assign({ code: '', pw: '', remember: false }, S.ui.connect || {}, { error: err || null, step: null });
            S.ui.route = { name: 'connect' };
            changed();
        },
        async connectWith(code, pw, remember) {
            const Cn = S.ui.connect;
            const sid = C.normalizeCode(code);
            if (!sid) { Cn.error = 'That code doesn’t look right — it has 26 letters and digits (2–7).'; changed(); return; }
            if (!window.crypto || !window.crypto.subtle) { Cn.error = 'This page needs HTTPS for encryption.'; changed(); return; }
            Cn.step = 'Deriving the key from your password…'; Cn.error = null; changed();
            let raw;
            try { raw = await C.deriveKey(pw, sid); } catch (e) { Cn.step = null; Cn.error = e.message; changed(); return; }
            this.offerRemember = false;
            this.wantRemember = !!remember;
            base = '/s/' + sid;
            this.pendingSid = null;
            history.replaceState({ d: 0 }, '', base);
            await this.start(sid, raw, null, this.relay || this.defaultRelay());
        },
        async connectSaved(d) {
            base = '/s/' + d.sid;
            history.replaceState({ d: 0 }, '', base);
            await this.start(d.sid, null, d.key, d.relay || this.defaultRelay(), true);
        },
        async start(sid, raw, key, relay, remembered) {
            this.sid = sid; this.raw = raw; this.relay = relay;
            S.ui.remembered = !!remembered;
            const master = key || await C.importMasterKey(raw, false);
            if (raw) sessionPut(sid, raw, relay);
            S.ui.connect = Object.assign({}, S.ui.connect || { code: '', pw: '' }, { step: 'Connecting to the relay…', error: null });
            if (S.ui.route.name === 'connect') S.ui.route = parse(location.pathname);
            if (S.ui.route.name === 'connect' || S.ui.route.name === 'login') S.ui.route = { name: 'team' };
            if (client) client.stop();
            const t = new T.RelayTransport(relay, sid, master);
            const c = new T.Client({ transport: t, mode: 'relay', clientKind: standalone() ? 'pwa' : 'web', onEvent: () => { } });
            wireClient(c);
            c.start();
            if (swReg && swReg.active) swReg.active.postMessage({ type: 'base', base });
            changed();
        },
        onOpen() {
            if (this.wantRemember) { this.wantRemember = false; this.remember(); }
        },
        onState() { },
        onFatal(m) {
            sessionDel(this.sid);
            const saved = (S.ui.devices || []).find((d) => d.sid === this.sid);
            let err = m.error;
            if (m.code === 'badkey' && saved) err = 'This saved session no longer opens — the link was probably rotated. Forget it and use the new link or code.';
            this.pendingSid = m.code === 'badkey' ? this.sid : null;
            this.toConnect(err);
        },
        async remember() {
            if (!this.raw) { S.ui.remembered = true; changed(); return; }
            try {
                const key = await C.importMasterKey(this.raw, false);
                await IDB.put({ sid: this.sid, relay: this.relay, key, label: (S.app && S.app.project_name) || 'Mantra session', saved_at: Date.now() });
                S.ui.devices = await IDB.all();
                S.ui.remembered = true;
                this.offerRemember = false;
                flash('Remembered on this device', 'ok');
            } catch (e) { flash('Could not remember this device: ' + e.message, 'error'); }
            changed();
        },
        async forget(sid) {
            sid = sid || this.sid;
            await IDB.del(sid);
            S.ui.devices = await IDB.all();
            if (sid === this.sid) S.ui.remembered = false;
            flash('Forgotten on this device', 'info');
            changed();
        },
        disconnect() {
            sessionDel(this.sid);
            if (client) client.stop();
            client = null;
            location.assign('/');
        },
    };
    function sessionGet(sid) {
        try { const v = JSON.parse(sessionStorage.getItem('mantra.k.' + sid) || 'null'); return v ? { raw: C.unb64url(v.k), relay: v.r } : null; } catch (_) { return null; }
    }
    function sessionPut(sid, raw, relay) { try { sessionStorage.setItem('mantra.k.' + sid, JSON.stringify({ k: C.b64url(raw), r: relay })); } catch (_) { } }
    function sessionDel(sid) { try { sessionStorage.removeItem('mantra.k.' + sid); } catch (_) { } }

    // ── timers ───────────────────────────────────────────────────────────────────────────────────
    // Elapsed clocks tick every second while something is live; relative times every 30 s.
    setInterval(() => {
        if (document.visibilityState !== 'visible') return;
        const live = (S.run && S.run.active) || M.store.agentList().some((a) => a.busy) || S.conn.state === 'reconnecting' || (S.toast && S.toast.shown + (S.toast.ttl_ms || 4000) > Date.now() - 1000);
        if (live) changed();
    }, 1000);
    setInterval(() => { if (document.visibilityState === 'visible') changed(); }, 30000);

    // ── boot ─────────────────────────────────────────────────────────────────────────────────────
    M.act = { changed, cmd, request, nav, back, flash, sheet, closeSheet, copy, wide, motion, applyPrefs, login, logout };
    applyPrefs();
    if (MODE === 'relay') M.relay.boot().then(registerSW);
    else { bootLocal(); registerSW(); }
    changed();
})(window.Mantra = window.Mantra || {});
