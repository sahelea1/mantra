// Mantra web UI — the Agent screen: header, actions, windowed transcript, approvals, queue, composer.
//
// Transcript items are memoised per (agent, ord, v, expanded, verbose): a streaming delta only
// rebuilds the one item that changed, and dom.js skips identical vnodes. Loading older items
// moves a bounded window through history instead of mounting an ever-growing transcript.
'use strict';
(function (M) {
    const h = M.h, F = M.fmt, P = M.parts;
    const S = () => M.store.S;
    const icon = F.icon;
    const WINDOW = 200;
    const MAX_WINDOW = 400;
    const MESSAGE_CHARS = 64000;

    const memo = new Map();
    const scroll = {};   // agent id → {stick, el, anchor, seen, top, endOrd}
    const win = {};      // agent id → bounded window size
    const knownAgents = new Set();

    // ── item renderers ───────────────────────────────────────────────────────────────────────────
    function expandKey(aid, it) { return aid + ':' + it.ord; }
    function toggleExpand(k) { const e = S().ui.expanded; e[k] = !e[k]; M.act.changed(); }

    function cmdState(it) {
        if (!it.done) return { cls: 'run', label: 'running' };
        if (it.status === 'declined') return { cls: 'bad', label: 'declined' };
        if (it.exit !== undefined && it.exit !== null && it.exit !== 0) return { cls: 'bad', label: 'exit ' + it.exit };
        if (it.status === 'failed') return { cls: 'bad', label: 'failed' };
        return { cls: 'ok', label: '' };
    }

    // While text streams in, a `**` or backtick may be open; hide the dangling marker until its
    // partner arrives instead of flashing raw asterisks.
    function openMarkers(t) {
        const fences = (t.match(/^\s*```/gm) || []).length;
        if (fences % 2) return t;
        const lines = t.split('\n');
        let last = lines[lines.length - 1];
        if (((last.match(/\*\*/g) || []).length) % 2) last = last.replace(/\*\*(?!.*\*\*)/, '');
        if (((last.match(/`/g) || []).length) % 2) last = last.replace(/`(?!.*`)/, '');
        lines[lines.length - 1] = last;
        return lines.join('\n');
    }

    function secs(ms) { return ms < 1000 ? Math.max(1, Math.round(ms)) + 'ms' : ms < 10000 ? (ms / 1000).toFixed(1) + 's' : F.durShort(ms); }

    function tail(text, n) {
        const lines = String(text || '').replace(/\s+$/, '').split('\n');
        return lines.length > n ? { text: lines.slice(-n).join('\n'), cut: lines.length - n } : { text: lines.join('\n'), cut: 0 };
    }

    function messageText(it, full) {
        const value = String(it.text || '');
        return !full && value.length > MESSAGE_CHARS ? '… earlier text hidden; show full message to read it\n\n' + value.slice(-MESSAGE_CHARS) : value;
    }

    function fullMessage(a, it, full) {
        return String(it.text || '').length > MESSAGE_CHARS
            ? h('button', { type: 'button', class: 'link', onclick: () => toggleExpand(expandKey(a.id, it) + ':full') }, full ? 'Show recent text' : 'Show full message')
            : null;
    }

    function renderItem(a, it, exp, verbose, full) {
        const k = 'i' + it.ord;
        switch (it.kind) {
            case 'user':
                return h('div', { key: k, class: 'msg user' }, h('div', { class: 'bubble' }, fullMessage(a, it, full), F.inline(messageText(it, full))));
            case 'agent':
                return h('div', { key: k, class: 'msg agent md' + (it.done ? '' : ' streaming') },
                    fullMessage(a, it, full), F.markdown(it.done ? messageText(it, full) : openMarkers(messageText(it, full))));
            case 'reasoning': {
                const open = exp || verbose;
                const recent = String(it.text || '').slice(-512).trimEnd();
                const summary = recent.slice(recent.lastIndexOf('\n') + 1).replace(/^\*\*|\*\*$/g, '');
                return h('div', { key: k, class: 'msg reason' + (open ? ' open' : '') },
                    h('button', { type: 'button', class: 'reason-head', onclick: () => toggleExpand(expandKey(a.id, it)), 'aria-expanded': String(!!open) },
                        h('span', { class: 'reason-label' + (it.done ? '' : ' shimmer') }, it.done ? 'Thought' : 'Thinking'),
                        !open && summary ? h('span', { class: 'reason-sum' }, F.trunc(summary, 120)) : null,
                        icon(open ? 'up' : 'down', 'chev')),
                    open ? h('div', { class: 'reason-body md' }, fullMessage(a, it, full), F.markdown(messageText(it, full))) : null);
            }
            case 'plan':
                return h('div', { key: k, class: 'card item plan-item' },
                    h('div', { class: 'item-head' }, icon('plan'), h('span', null, 'Plan')),
                    h('div', { class: 'md' }, fullMessage(a, it, full), F.markdown(messageText(it, full))));
            case 'command': {
                const st = cmdState(it);
                const out = it.output || '';
                const open = exp || (verbose && out) || (!it.done && out);
                const t = open && !exp && !verbose ? tail(out, 8) : { text: out, cut: 0 };
                return h('div', { key: k, class: 'card item cmd ' + st.cls },
                    h('button', { type: 'button', class: 'item-head cmd-head', onclick: () => toggleExpand(expandKey(a.id, it)), 'aria-expanded': String(!!open) },
                        h('span', { class: 'cmd-ico' }, st.cls === 'run' ? h('span', { class: 'spin' }) : st.cls === 'ok' ? icon('check') : icon('close')),
                        h('code', { class: 'cmd-text' }, it.cmd || it.text || ''),
                        h('span', { class: 'cmd-meta' }, st.label ? h('span', { class: 'cmd-exit' }, st.label) : null, it.dur_ms ? secs(it.dur_ms) : null),
                        out ? icon(open ? 'up' : 'down', 'chev') : null),
                    open && out ? h('pre', { class: 'cmd-out' }, t.cut ? h('span', { class: 'dl note' }, '… ' + t.cut + ' earlier lines\n') : null, t.text) : null);
            }
            case 'files': {
                const ch = it.changes || [];
                const adds = ch.reduce((s, c) => s + (c.adds || 0), 0), dels = ch.reduce((s, c) => s + (c.dels || 0), 0);
                return h('div', { key: k, class: 'card item files' },
                    h('div', { class: 'item-head' }, icon('edit'), h('span', null, ch.length === 1 ? 'Edited a file' : 'Edited ' + ch.length + ' files'),
                        h('span', { class: 'files-sum' }, h('span', { class: 'add' }, '+' + adds), ' ', h('span', { class: 'del' }, '−' + dels))),
                    h('div', { class: 'file-list' }, ch.map((c) => h('button', { type: 'button', key: c.path, class: 'file-row', onclick: () => M.act.sheet({ kind: 'diff', agent: a.id, path: c.path }) },
                        h('span', { class: 'fkind k-' + (c.kind || 'update') }, (c.kind || 'update')[0].toUpperCase()),
                        h('span', { class: 'fpath' }, c.path),
                        h('span', { class: 'fnum' }, h('span', { class: 'add' }, '+' + (c.adds || 0)), ' ', h('span', { class: 'del' }, '−' + (c.dels || 0)))))));
            }
            case 'tool': {
                const tl = it.tool || {};
                const open = exp || verbose;
                const bad = tl.status === 'failed' || tl.status === 'error';
                return h('div', { key: k, class: 'card item tool' + (bad ? ' bad' : '') },
                    h('button', { type: 'button', class: 'item-head', onclick: () => toggleExpand(expandKey(a.id, it)), 'aria-expanded': String(!!open) },
                        !it.done ? h('span', { class: 'spin' }) : icon('tool'),
                        // Mantra's own tools without their prefix, as the terminal shows them (`akasha_note`)
                        h('span', { class: 'tool-name' }, (tl.name || it.text || 'tool').replace(/^mantra_/, '')),
                        tl.status && tl.status !== 'completed' ? h('span', { class: 'cmd-exit' }, tl.status) : null,
                        icon(open ? 'up' : 'down', 'chev')),
                    // the result carried an image (a screenshot, an MCP render): say so and where it is — nothing draws it;
                    // a long path loses its start, not the file name (rtl, the LRMs keep the leading / in place)
                    tl.image != null ? h('div', { class: 'tool-image' }, h('span', { class: 'tool-image-mark' }, icon('image'), 'image'), tl.image ? h('span', { class: 'tool-image-path', title: tl.image }, '\u200e' + tl.image + '\u200e') : null) : null,
                    open ? h('div', { class: 'tool-body' },
                        tl.args ? [h('div', { class: 'tool-label' }, 'arguments'), h('pre', { class: 'cmd-out' }, typeof tl.args === 'string' ? tl.args : JSON.stringify(tl.args, null, 2))] : null,
                        tl.result ? [h('div', { class: 'tool-label' }, 'result'), h('pre', { class: 'cmd-out' }, F.trunc(typeof tl.result === 'string' ? tl.result : JSON.stringify(tl.result, null, 2), 8000))] : null) : null);
            }
            case 'web':
                return h('div', { key: k, class: 'item-line web' }, icon('search'), h('span', null, 'Searched the web'), it.query ? h('q', null, it.query) : null);
            case 'notice':
                return h('div', { key: k, class: 'notice lvl-' + (it.level || 'info') }, h('span', null, it.text));
            case 'compaction':
                return h('div', { key: k, class: 'divider' }, h('span', null, 'context compacted' + (it.to ? ' · ' + F.tokens(it.from || 0) + ' → ' + F.tokens(it.to) + ' tok' : '')));
        }
        return h('div', { key: k, class: 'notice' }, it.text || '');
    }

    function itemView(a, it) {
        const ek = expandKey(a.id, it);
        const exp = !!S().ui.expanded[ek];
        const full = !!S().ui.expanded[ek + ':full'];
        const verbose = !!S().ui.verbose;
        const c = memo.get(ek);
        if (c && c.it === it && c.exp === exp && c.verbose === verbose && c.full === full) return c.node;
        const node = renderItem(a, it, exp, verbose, full);
        if (memo.size > 6000) memo.clear();
        memo.set(ek, { it, exp, verbose, full, node });
        return node;
    }

    // ── busy line ────────────────────────────────────────────────────────────────────────────────
    function busyLine(a) {
        const now = Date.now();
        if (a.waiting_answer)
            return h('div', { class: 'busy-line wait', key: 'busy' }, icon('question'), h('span', null, 'Waiting for an answer'));
        if (a.approval_pending || a.status === 'waiting')
            return h('div', { class: 'busy-line wait', key: 'busy' }, icon('alert'), h('span', null, 'Waiting for approval'));
        if (a.status === 'retrying') {
            return h('div', { class: 'busy-line wait', key: 'busy' }, icon('refresh'), h('span', null, P.retryingText(a, 80)), a.retry_note ? h('span', { class: 'dim' }, ' · ' + a.retry_note) : null);
        }
        if (a.swarm_waiting)
            return h('div', { class: 'busy-line wait', key: 'busy' }, icon('run'), h('span', null, 'Waiting for Akasha swarm'));
        if (a.status === 'starting' || a.awaiting_start) {
            return h('div', { class: 'busy-line', key: 'busy' }, h('span', { class: 'spin' }), h('span', { class: 'shimmer' }, 'Starting ' + F.backend(a.backend) + '…'));
        }
        if (!a.busy) return null;
        const label = a.compacting ? 'Compacting context…' : (a.activity || 'Thinking…');
        const quiet = a.quiet_ms && a.quiet_ms >= 20000;
        return h('div', { class: 'busy-line', key: 'busy' },
            h('span', { class: 'spin' }),
            h('span', { class: 'shimmer' }, F.trunc(label, 50)),
            h('span', { class: 'dim' }, [
                a.turn_started_at ? ' · ' + F.dur(now - a.turn_started_at) : '',
                a.tokens_total ? ' · ' + F.tokens(a.tokens_total) + ' tok' : '',
                F.cached(a.tokens_total, a.tokens_cached) ? ' · ' + F.cached(a.tokens_total, a.tokens_cached) : '',
                a.effort ? ' · ' + a.effort : '',
            ].join('')),
            quiet ? h('span', { class: 'quiet' }, ' · quiet ' + F.durShort(a.quiet_ms)) : null);
    }

    // ── transcript ───────────────────────────────────────────────────────────────────────────────
    function endIndex(items, st) {
        if (st.endOrd === undefined || st.endOrd === null) return items.length;
        // Older pages can be prepended and fresh events appended without moving the viewed page.
        let lo = 0, hi = items.length;
        while (lo < hi) {
            const mid = (lo + hi) >>> 1;
            if (items[mid].ord <= st.endOrd) lo = mid + 1;
            else hi = mid;
        }
        return lo;
    }

    function anchorAtTop(st) {
        const el = st.el;
        if (!el || !el.isConnected) return;
        const top = el.getBoundingClientRect().top;
        for (const child of el.firstElementChild.children) {
            if (child.getBoundingClientRect().bottom <= top) continue;
            const key = child.__v && child.__v.key;
            if (typeof key === 'string' && key.startsWith('i')) {
                st.anchor = { key, offset: child.getBoundingClientRect().top - top };
                return;
            }
        }
        st.anchor = { bottom: el.scrollHeight - el.scrollTop };
    }

    function attach(el, id) {
        const st = scroll[id] || (scroll[id] = { stick: true, seen: -1 });
        st.el = el;
        if (!st.stick && st.top !== undefined) st.restore = true;
        el.addEventListener('scroll', () => {
            if (st.el !== el || !el.isConnected || st.restore) return; // ignore detached and initial-layout events
            st.top = el.scrollTop;
            const near = el.scrollHeight - el.scrollTop - el.clientHeight < 48;
            if (near !== st.stick && st.endOrd == null) { st.stick = near; M.act.changed(); }
            if (near && st.endOrd == null) { const a = M.store.agent(id); if (a && a.items.length) st.seen = a.items[a.items.length - 1].ord; }
        }, { passive: true });
    }

    async function loadEarlier(a) {
        const st = scroll[a.id] || (scroll[a.id] = { stick: false, seen: -1 });
        const w = win[a.id] || WINDOW;
        const end = endIndex(a.items, st);
        const shownFrom = Math.max(0, end - w);
        if (shownFrom > 0) {
            anchorAtTop(st);
            st.stick = false;
            if (w < MAX_WINDOW) win[a.id] = Math.min(MAX_WINDOW, w + WINDOW);
            else st.endOrd = a.items[Math.min(end - 1, shownFrom + WINDOW - 1)].ord;
            M.act.changed(); return;
        }
        const before = a.items.length ? a.items[0].ord : a.items_total;
        S().ui.busy['earlier' + a.id] = true;
        // Not st.anchor yet: the busy-spinner render below would consume it before the items it's
        // meant to anchor even arrive, jumping the scroll to nowhere. Set it once they're in hand.
        M.act.changed();
        try {
            const m = await M.act.request('fetch_items', { agent: a.id, before, count: WINDOW });
            const n = M.store.prependItems(a.id, m && m.items);
            if (n) {
                anchorAtTop(st);
                st.stick = false;
                // Keep the first part of the previous page mounted as the anchor. The tail of
                // a long page can be revisited with the new-message button.
                if (w + n > MAX_WINDOW) st.endOrd = a.items[n + Math.min(end - 1, WINDOW - 1)].ord;
                win[a.id] = Math.min(MAX_WINDOW, w + n);
            }
        } catch (e) {
            M.act.flash('Could not load earlier messages: ' + e.message, 'error');
        } finally {
            delete S().ui.busy['earlier' + a.id];
            M.act.changed();
        }
    }

    // True while the fixed flash toast floats above the composer on a narrow layout — used to give
    // the transcript extra bottom padding so the toast doesn't hide the last line (F5).
    function flashAboveComposer() {
        const s = S();
        if (M.act.wide() || s.ui.route.name !== 'agent') return false;
        if ((s.ui.flashes || []).length) return true;
        const t = s.toast;
        return !!(t && t.shown + (t.ttl_ms || 4000) > Date.now());
    }

    function transcript(a) {
        const w = win[a.id] || WINDOW;
        const items = a.items;
        const st = scroll[a.id] || (scroll[a.id] = { stick: true, seen: -1 });
        const end = endIndex(items, st);
        const start = Math.max(0, end - w);
        const more = start > 0 || (items.length ? items[0].ord > (a.items_first || 0) : (a.items_total || 0) > 0);
        const lastOrd = items.length ? items[items.length - 1].ord : -1;
        const olderPage = end < items.length;
        const showNew = olderPage || (!st.stick && lastOrd > st.seen && st.seen >= 0);
        const kids = [];
        if (more) kids.push(h('div', { class: 'earlier', key: 'earlier' }, P.btn('Load earlier', () => loadEarlier(a), { sm: true, kind: 'ghost', busyKey: 'earlier' + a.id, icon: 'up' })));
        else if (items.length) kids.push(h('div', { class: 'tx-begin', key: 'begin' }, h('span', null, 'Started ' + F.ago(a.created_at))));
        if (!items.length && !a.busy) {
            kids.push(h('div', { class: 'tx-empty', key: 'txempty' },
                h('div', { class: 'glyph xl', style: { color: F.colorVar(a.color) } }, a.glyph || '●'),
                h('div', { class: 'empty-title' }, a.role_kind === 'solo' ? 'Ready when you are' : a.name + ' hasn\'t said anything yet'),
                h('div', { class: 'empty-body' }, a.role_kind === 'solo' ? 'Ask for anything in this project. Start a message with ! to run a shell command.' : a.in_run ? 'Messages go to the planner; this view shows ' + a.name + '’s transcript.' : 'Messages you send here go to ' + a.name + '.')));
        }
        for (let i = start; i < end; i++) kids.push(itemView(a, items[i]));
        if (end === items.length) kids.push(busyLine(a));
        kids.push(h('div', { class: 'tx-end', key: 'end' }));
        return h('div', { class: 'tx-wrap', key: 'txw' + a.id },
            h('div', { class: 'tx', key: 'tx' + a.id, ref: (el) => attach(el, a.id), role: 'log', 'aria-live': 'polite', 'aria-relevant': 'additions' }, h('div', { class: 'tx-inner' + (flashAboveComposer() ? ' flash-pad' : '') }, kids)),
            showNew ? h('button', { type: 'button', class: 'new-pill', key: 'newpill', onclick: () => { const s2 = scroll[a.id]; if (s2) { s2.endOrd = null; s2.stick = true; s2.anchor = undefined; M.act.changed(); } } }, icon('arrowdown'), olderPage ? 'latest' : 'new') : null);
    }

    // After every render: keep followed transcripts pinned; restore the anchor after "load earlier".
    function afterRender() {
        const agents = S().agents;
        const removed = [...knownAgents].filter((id) => !agents.has(id));
        if (removed.length) {
            const gone = new Set(removed.map(String));
            for (const id of removed) { delete scroll[id]; delete win[id]; }
            for (const key of memo.keys()) if (gone.has(key.slice(0, key.indexOf(':')))) memo.delete(key);
            for (const key of Object.keys(S().ui.expanded)) {
                const colon = key.indexOf(':');
                if (colon > 0 && gone.has(key.slice(0, colon))) delete S().ui.expanded[key];
            }
        }
        if (knownAgents.size !== agents.size || removed.length) {
            knownAgents.clear();
            for (const id of agents.keys()) knownAgents.add(id);
        }
        for (const id in scroll) {
            const st = scroll[id];
            if (!st.el) continue;
            if (!st.el.isConnected) {
                // The last connected scroll event already recorded top. A detached element can
                // report zero even though the person was reading an older page.
                st.el = null; // the prior agent's mounted transcript can now be collected
                continue;
            }
            if (st.anchor !== undefined) {
                const anchor = st.anchor;
                const child = anchor.key && Array.from(st.el.firstElementChild.children).find((x) => x.__v && x.__v.key === anchor.key);
                if (child) st.el.scrollTop = child.getBoundingClientRect().top - st.el.getBoundingClientRect().top + st.el.scrollTop - anchor.offset;
                else if (anchor.bottom !== undefined) st.el.scrollTop = st.el.scrollHeight - anchor.bottom;
                st.top = st.el.scrollTop;
                st.anchor = undefined; st.restore = false; continue;
            }
            if (st.restore) { st.el.scrollTop = st.top; st.restore = false; continue; }
            if (st.stick) {
                st.el.scrollTop = st.el.scrollHeight;
                const a = M.store.agent(id);
                if (a && a.items.length) st.seen = a.items[a.items.length - 1].ord;
            }
        }
    }
    function scrollToEnd(id) { const st = scroll[id]; if (st) { st.stick = true; st.endOrd = null; } }

    // ── queue + composer ─────────────────────────────────────────────────────────────────────────
    function queueBar(a) {
        const q = a.queued || [];
        if (!q.length) return null;
        return h('div', { class: 'queue', key: 'queue' },
            h('div', { class: 'queue-head' }, h('span', { class: 'q-ico' }, '⏳'), h('span', null, 'Queued ' + q.length + ' — sent when ' + a.name + ' finishes this turn')),
            h('div', { class: 'queue-list' }, q.map((t, i) => h('div', { class: 'qchip', key: 'q' + i, title: t }, h('span', null, F.trunc(t, 80)),
                i === q.length - 1 ? h('button', { type: 'button', class: 'qedit', onclick: () => editLast(a), 'aria-label': 'Edit' }, icon('edit')) : null))),
            h('div', { class: 'queue-actions' },
                P.btn('Send now', () => M.act.cmd('send', { agent: a.id, text: '', force: true }, { busyKey: 'flush' + a.id, ok: 'Sent now' }), { sm: true, kind: 'primary', busyKey: 'flush' + a.id, icon: 'send' }),
                P.btn('Discard', () => M.act.cmd('discard_queue', { agent: a.id }, { ok: 'Queue cleared' }), { sm: true, kind: 'ghost', icon: 'trash' })));
    }
    async function editLast(a) {
        try {
            const d = await M.act.request('pop_queued', { agent: a.id });
            const t = (d && d.text) || '';
            const cur = S().ui.drafts['c' + a.id] || '';
            S().ui.drafts['c' + a.id] = cur ? t + '\n' + cur : t;
            M.act.changed();
            focusComposer();
        } catch (e) { M.act.flash(e.message, 'error'); }
    }

    // Slash commands use the same actions and confirmations as the visible controls.
    function slashCommands(a) {
        const s = S();
        const run = s.run;
        const list = [
            { n: '/compact', d: 'compact ' + a.name + '\'s context', f: () => M.act.cmd('compact', { agent: a.id }, { ok: 'Compacting' }) },
            { n: '/stop-agent', d: 'end this agent process', f: () => stopAgent(a) },
            { n: '/quit', d: 'close Mantra on the host', f: () => M.screens.closeMantra() },
            { n: '/interrupt', d: 'stop the current turn', f: () => M.act.cmd('interrupt', { agent: a.id }, { ok: 'Interrupted' }) },
            { n: '/model', d: 'switch model', f: () => M.act.sheet({ kind: 'model', agent: a.id }) },
            { n: '/effort', d: 'change reasoning effort', f: () => M.act.sheet({ kind: 'effort', agent: a.id }) },
            { n: '/diff', d: 'files changed by ' + a.name, f: () => M.act.sheet({ kind: 'diff', agent: a.id }) },
            { n: '/respawn', d: 'restart ' + a.name, f: () => M.act.cmd('respawn', { agent: a.id }, { ok: 'Respawning' }) },
            { n: '/verbose', d: 'toggle full reasoning and output', f: () => M.store.setPref('verbose', !s.ui.verbose) },
            { n: '/new', d: 'new Solo session', f: () => M.act.cmd('new_solo', {}, { ok: 'New session' }).then((d) => d && d.agent !== undefined && M.act.nav('/agent/' + d.agent)) },
            { n: '/run', d: 'start a run: /run <goal>', arg: true, f: (goal) => goal ? M.act.cmd('start_run', { goal }, { ok: 'Run started' }).then(() => M.act.nav('/run')) : M.act.nav('/') },
            { n: '/runs', d: 'runs in this project', f: () => M.act.nav('/runs') },
            { n: '/settings', d: 'notifications, appearance, remote', f: () => M.act.nav('/settings') },
        ];
        if (run) {
            list.push({ n: '/plan', d: 'the run\'s plan', f: () => M.act.nav('/run') });
            list.push({ n: run.halted ? '/resume' : '/pause', d: run.halted ? 'resume the run' : 'pause the run', f: () => M.act.cmd('pause_resume', {}, {}) });
            if (run.landable) list.push({ n: '/land', d: 'merge the finished run', f: () => M.act.cmd('land', {}, { ok: 'Landing' }) });
        }
        return list;
    }

    function mentionTargets() {
        return M.store.agentList().map((a) => ({ id: a.id, name: (a.run_name || a.name || '').replace(/\s+/g, '-'), a }));
    }

    function suggestions(a, text) {
        if (text.startsWith('/') && !/\s/.test(text)) {
            const q = text.toLowerCase();
            return slashCommands(a).filter((c) => c.n.startsWith(q)).map((c) => ({ key: c.n, label: c.n, desc: c.d, apply: () => { if (c.arg) setDraft(a, c.n + ' '); else { setDraft(a, ''); c.f(); } } }));
        }
        const m = /^@([\w.-]*)$/.exec(text);
        if (m && S().run && !a.in_run) {
            const q = m[1].toLowerCase();
            return mentionTargets().filter((t) => t.name.toLowerCase().startsWith(q) && t.id !== a.id).slice(0, 8)
                .map((t) => ({ key: '@' + t.id, label: '@' + t.name, desc: P.status(t.a).text, glyph: t.a, apply: () => setDraft(a, '@' + t.name + ' ') }));
        }
        return [];
    }

    function setDraft(a, t) { S().ui.drafts['c' + a.id] = t; S().ui.sugSel = 0; M.act.changed(); focusComposer(); }
    function focusComposer() { requestAnimationFrame(() => { const el = document.querySelector('.composer textarea'); if (el) { el.focus(); el.setSelectionRange(el.value.length, el.value.length); grow(el); } }); }
    function grow(el) {
        el.style.height = 'auto';
        const lh = parseFloat(getComputedStyle(el).lineHeight) || 22;
        el.style.height = Math.min(el.scrollHeight, lh * 6 + 20) + 'px';
    }

    async function submit(a, force) {
        const key = 'c' + a.id;
        const raw = S().ui.drafts[key] || '';
        let text = raw.trim();
        // An empty draft is only ever a flush of what's already queued (Ctrl+Enter with nothing
        // typed and nothing queued has nothing to do — same rule the server enforces).
        if (!text && !(force && (a.queued || []).length)) return;
        if (text.startsWith('/') && !text.startsWith('//')) {
            const [name, ...rest] = text.split(/\s+/);
            const c = slashCommands(a).find((x) => x.n === name.toLowerCase());
            if (c) { S().ui.drafts[key] = ''; M.act.changed(); c.f(rest.join(' ').trim()); return; }
            M.act.flash('Unknown command ' + name + ' — type / to see what works here', 'warn');
            return;
        }
        let target = a.in_run && S().run ? M.store.agent(S().run.planner) || a : a;
        const m = /^@([\w.-]+)\s+([\s\S]+)$/.exec(text);
        if (m && S().run && !a.in_run) {
            const t = mentionTargets().find((x) => x.name.toLowerCase() === m[1].toLowerCase());
            if (t) { target = t.a; text = m[2].trim(); }
        }
        S().ui.drafts[key] = '';
        scrollToEnd(a.id);
        M.act.changed();
        const el = document.querySelector('.composer textarea');
        if (el) grow(el);
        try {
            await M.act.request('send', { agent: target.id, text, force: !!force });
            if (target.id !== a.id) M.act.flash('Sent to ' + target.name, 'ok');
            else if (a.busy && !force) M.act.flash('Queued — ' + a.name + ' gets it after this turn', 'info');
        } catch (e) {
            // Put the text back so nothing typed is lost.
            const newer = S().ui.drafts[key] || '';
            S().ui.drafts[key] = newer ? raw + '\n' + newer : raw;
            M.act.flash(e.message, 'error');
            M.act.changed();
        }
    }

    function composer(a) {
        if (a.status === 'stopped') return P.completedAgent(a)
            ? h('div', { class: 'composer-note completed', key: 'stopped-composer' }, 'This agent’s work is complete.')
            : h('div', { class: 'composer-note', key: 'stopped-composer' }, a.in_run ? 'This process has ended. Respawn it to continue.' : 'This process has ended. Start a new Solo session to continue.');
        const s = S();
        const key = 'c' + a.id;
        const text = s.ui.drafts[key] || '';
        const open = s.conn.state === 'open';
        const sug = s.ui.sugDismiss === text ? [] : suggestions(a, text);
        const sel = Math.min(s.ui.sugSel || 0, Math.max(0, sug.length - 1));
        const wide = M.act.wide();
        const placeholder = a.role_kind === 'solo'
            ? 'Message ' + a.name + (wide ? '…  (! runs a shell command)' : '…')
            : a.in_run ? 'Message the planner…' : 'Message ' + (a.run_name || a.name) + '…';
        return h('div', { class: 'composer' + (a.busy ? ' busy' : ''), key: 'composer' },
            sug.length ? h('div', { class: 'suggest', role: 'listbox', key: 'sug' }, sug.map((x, i) => h('button', {
                type: 'button', key: x.key, role: 'option', 'aria-selected': String(i === sel), class: 'sug' + (i === sel ? ' on' : ''),
                onmousedown: (e) => e.preventDefault(), onclick: () => x.apply(),
            }, x.glyph ? P.glyph(x.glyph) : null, h('b', null, x.label), h('span', null, x.desc)))) : null,
            h('div', { class: 'composer-box' },
                h('textarea', {
                    key: 'ta' + a.id, rows: 1, value: text, placeholder, 'aria-label': 'Message ' + a.name, enterkeyhint: wide ? 'send' : 'enter', autocomplete: 'off', autocapitalize: 'sentences', spellcheck: 'true',
                    ref: (el) => requestAnimationFrame(() => grow(el)),
                    oninput: (e) => { s.ui.drafts[key] = e.target.value; s.ui.sugDismiss = null; s.ui.sugSel = 0; grow(e.target); M.act.changed(); },
                    onkeydown: (e) => {
                        if (sug.length && (e.key === 'ArrowDown' || e.key === 'ArrowUp')) { e.preventDefault(); s.ui.sugSel = (sel + (e.key === 'ArrowDown' ? 1 : sug.length - 1)) % sug.length; M.act.changed(); return; }
                        if (sug.length && (e.key === 'Tab' || (e.key === 'Enter' && !e.shiftKey && wide))) { e.preventDefault(); sug[sel].apply(); return; }
                        if (e.key === 'Enter' && (e.ctrlKey || e.metaKey)) { e.preventDefault(); submit(a, true); return; }
                        if (e.key === 'Enter' && !e.shiftKey && wide && !e.isComposing) { e.preventDefault(); submit(a, false); }
                        if (e.key === 'Escape' && sug.length) { e.preventDefault(); e.stopPropagation(); s.ui.sugDismiss = text; M.act.changed(); }
                    },
                }),
                h('div', { class: 'composer-btns' },
                    a.busy && text.trim() && !sug.length ? h('button', { type: 'button', class: 'btn sm ghost force', title: 'Send now — interrupt-free steer into the current turn (Ctrl+Enter)', disabled: !open || null, onclick: () => submit(a, true) }, 'Send now') : null,
                    a.busy && !text.trim() ? h('button', { type: 'button', class: 'ibtn stop', title: 'Interrupt ' + a.name, 'aria-label': 'Interrupt', disabled: !open || null, onclick: () => M.act.cmd('interrupt', { agent: a.id }, { ok: 'Interrupted' }) }, icon('stop')) : null,
                    h('button', { type: 'button', class: 'send', title: a.busy ? 'Queue (sent after this turn)' : 'Send', 'aria-label': a.busy ? 'Queue message' : 'Send', disabled: !open || !text.trim() || null, onclick: () => submit(a, false) }, icon('send')))),
            !open ? h('div', { class: 'composer-note' }, 'Not connected — your message stays here until Mantra is back') : null);
    }

    // ── header, actions ──────────────────────────────────────────────────────────────────────────
    function stopAgent(a) {
        M.act.sheet({
            kind: 'confirm', danger: true, title: 'Stop ' + a.name + '?', confirm: 'Stop agent',
            body: 'Ends this agent process and its Solo subagents. Its transcript stays here. ' + (a.in_run ? 'Use Respawn to continue its task.' : 'Start a new Solo session to continue.'),
            run: () => M.act.cmd('agent_stop', { agent: a.id }, { ok: 'Agent stopped' }),
        });
    }

    function actionList(a) {
        const A = M.act;
        const files = (a.files || []).length;
        const planN = (a.plan || []).length;
        return [
            a.status !== 'stopped' ? { id: 'stop-agent', icon: 'stop', label: 'Stop agent', run: () => stopAgent(a) } : null,
            a.busy ? { id: 'interrupt', icon: 'stop', label: 'Interrupt', run: () => A.cmd('interrupt', { agent: a.id }, { ok: 'Interrupted' }) } : null,
            a.status !== 'stopped' ? { id: 'compact', icon: 'compact', label: a.compact_pending ? 'Compact (queued)' : 'Compact', run: () => A.cmd('compact', { agent: a.id }, { ok: 'Compacting' }), disabled: a.compacting } : null,
            a.in_run || a.status === 'crashed' ? { id: 'respawn', icon: 'respawn', label: 'Respawn', run: () => A.cmd('respawn', { agent: a.id }, { ok: 'Respawning ' + a.name }) } : null,
            { id: 'model', icon: 'model', label: 'Model', run: () => A.sheet({ kind: 'model', agent: a.id }) },
            a.efforts && a.efforts.length ? { id: 'effort', icon: 'spark', label: 'Effort', run: () => A.sheet({ kind: 'effort', agent: a.id }) } : null,
            { id: 'diff', icon: 'diff', label: files ? 'Diff · ' + files : 'Diff', run: () => A.sheet({ kind: 'diff', agent: a.id }), disabled: !files },
            planN ? { id: 'plan', icon: 'plan', label: 'Steps ' + a.plan_done + '/' + a.plan_total, run: () => A.sheet({ kind: 'steps', agent: a.id }) } : null,
        ].filter(Boolean);
    }

    function header(a) {
        const wide = M.act.wide();
        const acts = actionList(a);
        return h('div', { class: 'agent-head', key: 'ahead' },
            h('div', { class: 'agent-id' },
                wide ? null : P.iconBtn('back', () => M.act.back('/'), 'Back', { cls: 'back' }),
                P.glyph(a, 'xl'),
                h('div', { class: 'agent-names' },
                    h('h1', null, a.worker ? a.worker.task_id : a.name, a.worker && a.worker.title ? h('span', { class: 'agent-role' }, ' · ' + a.worker.title) : a.role && a.role !== a.name ? h('span', { class: 'agent-role' }, ' · ' + a.role) : null),
                    P.statusPill(a)),
                wide ? null : P.iconBtn('dots', () => M.act.sheet({ kind: 'actions', agent: a.id }), 'Actions')),
            h('div', { class: 'agent-meta' },
                P.modelChip(a, () => M.act.sheet({ kind: 'model', agent: a.id })),
                P.ctxGauge(a),
                (a.plan || []).length ? h('button', { type: 'button', class: 'chip', onclick: () => M.act.sheet({ kind: 'steps', agent: a.id }) }, F.icon('plan'), a.plan_done + '/' + a.plan_total) : null,
                (a.files || []).length ? h('button', { type: 'button', class: 'chip', onclick: () => M.act.sheet({ kind: 'diff', agent: a.id }) }, h('span', { class: 'add' }, '+' + a.files_adds), h('span', { class: 'del' }, '−' + a.files_dels)) : null),
            wide ? h('div', { class: 'agent-actions' }, acts.map((x) => P.btn(x.label, x.run, { sm: true, kind: 'ghost', icon: x.icon, key: x.id, disabled: x.disabled || S().conn.state !== 'open' }))) : null);
    }

    // A failed or crashed agent gets its fixes right above the composer.
    function troubleCard(a) {
        if (P.completedAgent(a) || (a.status !== 'failed' && a.status !== 'crashed' && a.status !== 'stopped')) return null;
        const canRespawn = a.in_run || a.status === 'crashed';
        return h('div', { class: 'agent-trouble', key: 'trouble', role: 'alert' },
            h('div', null, h('b', null, a.status === 'stopped' ? a.worker && a.worker.state === 'failed' && !a.stopped_by_user ? a.name + '’s task failed' : a.name + ' is stopped' : a.status === 'crashed' ? a.name + ' crashed' : a.name + '’s last turn failed')),
            a.status_detail ? h('div', { class: 'band-msg' }, F.trunc(a.status_detail, 300)) : null,
            h('div', { class: 'band-actions' },
                P.btn('Switch model', () => M.act.sheet({ kind: 'model', agent: a.id }), { sm: true, icon: 'model', key: 'm' }),
                canRespawn ? P.btn('Respawn', () => M.act.cmd('respawn', { agent: a.id }, { busyKey: 'respawn' + a.id, ok: 'Respawning ' + a.name }), { sm: true, icon: 'respawn', busyKey: 'respawn' + a.id, key: 'r' }) : null,
                !canRespawn && a.status === 'stopped' ? P.btn('New Solo session', () => M.act.cmd('new_solo', {}).then((d) => d && M.act.nav('/agent/' + d.agent)), { sm: true, icon: 'plus' }) : !canRespawn ? h('span', { class: 'dim sm' }, 'Send a message to try again.') : null));
    }

    function swarmCard(a) {
        const g = a.swarm;
        if (!g) return null;
        const go = (id) => M.act.nav('/agent/' + id);
        const isOwner = g.owner === a.id;
        const chars = (n) => n < 1000 ? String(n) : (n / 1024).toFixed(n < 10 * 1024 ? 1 : 0).replace(/\.0$/, '') + 'k';
        const notes = g.notes || [];
        return h('section', { class: 'swarm-card', key: 'swarm', 'aria-label': 'Akasha swarm ' + g.name },
            h('div', { class: 'swarm-head' },
                h('b', null, 'Akasha swarm · ', g.name),
                isOwner ? h('span', { class: 'dim sm' }, 'Owner') : h('button', { type: 'button', class: 'link sm', onclick: () => go(g.owner) }, 'Parent: ', g.owner_name)),
            h('div', { class: 'swarm-members' },
                (g.children || []).map((child) => h('button', { type: 'button', class: 'swarm-member', key: child.id, onclick: () => go(child.id) },
                    h('span', null, child.name), h('span', { class: 'dim sm' }, child.status))),
                isOwner && h('span', { class: 'dim sm swarm-stop-hint' }, 'Stopping the owner ends this swarm.')),
            h('details', { class: 'swarm-notes' },
                h('summary', null, 'Shared notes · ', F.plural(notes.length, 'note'), ' · ', chars(g.notes_used || 0), '/', chars(g.notes_cap || 0)),
                notes.length ? h('ol', { class: 'notes' }, notes.map((note, i) => h('li', { class: 'note' + (note.urgent ? ' urgent' : ''), key: i },
                    h('div', { class: 'note-meta' }, note.urgent ? h('span', { class: 'mini-chip warn' }, 'urgent') : null, h('span', null, note.author)),
                    h('div', { class: 'note-x' }, F.inline(note.text))))) : h('p', { class: 'sec-sub' }, 'No notes yet.')));
    }

    function trackRespawn(id) {
        const s = S(), a = M.store.agent(id), run = s.run;
        if (!a || !run || !a.in_run) return;
        const role = ['planner', 'manager', 'orchestrator', 'gate_agent', 'finale_agent'].find((key) => run[key] === a.id);
        const task = (run.workers || []).find((w) => w.agent === a.id);
        if (!role && !task) return;
        (s.ui.respawns || (s.ui.respawns = {}))[id] = { run: run.id, role, task: task && task.task_id };
        M.act.changed();
    }
    function cancelRespawn(id) {
        if (S().ui.respawns) delete S().ui.respawns[id];
        M.act.changed();
    }
    function respawning(a) {
        const s = S(), pending = s.ui.respawns && s.ui.respawns[a.id];
        if (!pending) return false;
        if (!s.run || s.run.id !== pending.run) { delete s.ui.respawns[a.id]; return false; }
        const w = pending.task && (s.run.workers || []).find((w) => w.task_id === pending.task);
        const id = pending.role ? s.run[pending.role] : w && w.agent;
        if (id !== undefined && id !== null && id !== a.id && M.store.agent(id)) {
            requestAnimationFrame(() => {
                if (S().ui.route.name === 'agent' && S().ui.route.id === a.id) M.act.nav('/agent/' + id);
                cancelRespawn(a.id);
            });
        }
        return true;
    }

    function agentScreen(id) {
        const s = S();
        const a = M.store.agent(id);
        if (!a) {
            if (!s.synced) return P.loading(P.waitText());
            return h('div', { class: 'screen-pad' }, P.empty('agent', 'This agent is gone', 'It may have finished and been archived, or Mantra restarted.', [P.btn('Back to the team', () => M.act.nav('/'), { kind: 'primary' })]));
        }
        delete s.ui.unread[a.id];
        const aps = s.approvals.filter((x) => x.agent === a.id);
        const q = s.run && s.run.question && s.run.question.from === a.id ? P.questionBand(s.run) : null;
        const retrying = respawning(a);
        const trouble = retrying ? h('div', { class: 'composer-note', key: 'respawning', role: 'status' }, 'Respawning ' + a.name + '… ' + (s.run && s.run.halted ? 'Resume the run to start the new attempt.' : 'Opening the new attempt when it is ready.')) : troubleCard(a);
        const recipient = a.in_run && s.run ? M.store.agent(s.run.planner) || a : a;
        return h('div', { class: 'agent-screen', key: 'agent' + a.id },
            header(a),
            swarmCard(a),
            transcript(a),
            h('div', { class: 'agent-foot', key: 'foot' },
                q || trouble || aps.length || (recipient.queued || []).length ? h('div', { class: 'foot-stack', key: 'stack' },
                    trouble,
                    q,
                    aps.length ? h('div', { class: 'approvals', key: 'aps' }, aps.map((ap) => P.approvalCard(ap))) : null,
                    queueBar(recipient)) : null,
                a.in_run && recipient.id !== a.id ? h('div', { class: 'composer-note', key: 'planner-recipient' }, 'Messages go to the planner. ', h('button', { class: 'btn sm ghost', type: 'button', onclick: () => M.act.nav('/agent/' + recipient.id) }, 'Open planner')) : null,
                retrying ? null : composer(recipient)));
    }

    M.agentView = { agentScreen, header, actionList, transcript, afterRender, scrollToEnd, focusComposer, grow, trackRespawn, cancelRespawn };
})(window.Mantra = window.Mantra || {});
