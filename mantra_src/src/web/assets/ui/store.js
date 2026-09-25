// Mantra web UI — the store: server state (snapshot + deltas, spec §4/§5) and client UI state.
//
// Reducers mutate in place and bump `S.rev`; views read `S` directly and are re-rendered on the
// next animation frame after any change (see app.js `changed()`). Items per agent are an array
// sorted by `ord` (ascending); a delta almost always touches the tail, so lookups scan backwards.
'use strict';
(function (M) {
    const MAX_ITEMS = 2000;     // per agent, in memory
    const MAX_PULSE = 500;
    const MAX_NOTES = 200;

    function load(key, dflt) {
        try { const v = localStorage.getItem('mantra.' + key); return v === null ? dflt : JSON.parse(v); } catch (_) { return dflt; }
    }
    function save(key, val) {
        try { localStorage.setItem('mantra.' + key, JSON.stringify(val)); } catch (_) { /* private mode / quota */ }
    }

    const S = {
        rev: 0,
        seq: 0,
        synced: false,          // got at least one snapshot on this page load
        app: null,
        agents: new Map(),      // id → AgentView + {items: ItemView[]}
        order: [],              // agent ids in server order
        run: null,
        plan: null,
        approvals: [],
        toast: null,
        remote: null,
        pulse: [],
        notes: load('notes', []),
        hello: null,
        conn: { state: 'idle', error: null, mode: 'local' },
        ui: {
            route: { name: 'team' },
            focus: load('focus', null),
            theme: load('theme', 'system'),       // system | dark | light
            motion: load('motion', 'system'),     // system | reduce
            verbose: load('verbose', false),
            drafts: {},
            unread: {},
            sheet: null,
            palette: null,
            panel: load('panel', 'plan'),         // desktop right panel tab
            panelOpen: load('panelOpen', true),
            pulseFilter: 'all',
            banners: load('banners', {}),         // dismissed banner ids
            runGoal: '',
            runPattern: '',
            expanded: {},                          // item/card expand state: key → bool
            busy: {},                              // in-flight command keys → true (spinners, disabled)
            flash: null,                           // transient in-app message {text, level, at, action?}
            kb: false,                             // software keyboard open (phone)
            install: null,                         // deferred beforeinstallprompt event
            remember: false,
        },
    };

    function agentList() { return S.order.map((id) => S.agents.get(id)).filter(Boolean); }
    function agent(id) { return S.agents.get(Number(id)); }

    function sortItems(items) { items.sort((a, b) => a.ord - b.ord); return items; }

    function applySnapshot(m) {
        S.seq = m.seq || 0;
        S.app = m.app || null;
        const old = S.agents;
        S.agents = new Map();
        S.order = [];
        for (const a of m.agents || []) {
            const prev = old.get(a.id);
            const items = sortItems(Array.isArray(a.items) ? a.items.slice() : []);
            // Keep earlier pages the person already loaded if they are still contiguous.
            if (prev && prev.items.length && items.length && prev.items[0].ord < items[0].ord) {
                const older = prev.items.filter((it) => it.ord < items[0].ord && it.ord >= (a.items_first || 0));
                if (older.length && older[older.length - 1].ord === items[0].ord - 1) items.unshift(...older);
            }
            const v = Object.assign({}, a, { items });
            S.agents.set(a.id, v);
            S.order.push(a.id);
        }
        S.run = m.run || null;
        S.plan = m.plan || null;
        S.approvals = m.approvals || [];
        S.toast = m.toast ? Object.assign({ shown: Date.now() }, m.toast) : null;
        S.remote = m.remote || null;
        S.pulse = (m.pulse || []).slice(-MAX_PULSE);
        S.synced = true;
        S.rev++;
    }

    function upsertItem(ag, d) {
        const items = ag.items;
        let idx = -1;
        for (let i = items.length - 1; i >= 0; i--) {
            if (items[i].ord === d.ord) { idx = i; break; }
            if (items[i].ord < d.ord) break;
        }
        if (d.append !== undefined && !('kind' in d)) {
            if (idx < 0) return false; // an append for something we don't have → resync
            const it = Object.assign({}, items[idx]);
            if (it.kind === 'command') it.output = (it.output || '') + d.append;
            else it.text = (it.text || '') + d.append;
            it.v = d.v; it.done = d.done;
            items[idx] = it;
            return true;
        }
        if (idx >= 0) { items[idx] = d; return true; }
        // insert in order (normally at the end)
        let at = items.length;
        while (at > 0 && items[at - 1].ord > d.ord) at--;
        items.splice(at, 0, d);
        if (items.length > MAX_ITEMS) items.splice(0, items.length - MAX_ITEMS);
        return true;
    }

    // Returns false when the delta could not be applied cleanly (caller resyncs).
    function applyDelta(m, focusId) {
        let ok = true;
        S.seq = m.seq;
        if ('app' in m && m.app) S.app = m.app;
        if (m.agents) {
            for (const a of m.agents) {
                const prev = S.agents.get(a.id);
                const v = Object.assign({}, a, { items: prev ? prev.items : [] });
                S.agents.set(a.id, v);
                if (!prev) S.order.push(a.id);
            }
            // Keep server order: agent ids are allocated increasingly, so sort numerically.
            S.order.sort((x, y) => x - y);
        }
        if (m.agents_removed) {
            for (const id of m.agents_removed) { S.agents.delete(id); delete S.ui.unread[id]; }
            S.order = S.order.filter((id) => S.agents.has(id));
        }
        if (m.items) {
            for (const key of Object.keys(m.items)) {
                const ag = S.agents.get(Number(key));
                if (!ag) continue;
                const items = ag.items = ag.items.slice();
                let fresh = 0;
                for (const d of m.items[key]) {
                    const isNew = !items.length || d.ord > items[items.length - 1].ord;
                    if (!upsertItem(ag, d)) ok = false;
                    if (isNew && d.kind && d.kind !== 'user') fresh++;
                }
                if (fresh && Number(key) !== focusId) S.ui.unread[key] = (S.ui.unread[key] || 0) + fresh;
            }
        }
        if ('run' in m) S.run = m.run;
        if ('plan' in m) S.plan = m.plan;
        if (m.pulse && m.pulse.length) {
            const last = S.pulse.length ? S.pulse[S.pulse.length - 1].n : -1;
            for (const p of m.pulse) if (p.n > last) S.pulse.push(p);
            if (S.pulse.length > MAX_PULSE) S.pulse.splice(0, S.pulse.length - MAX_PULSE);
        }
        if (m.approvals) S.approvals = m.approvals;
        if ('toast' in m) S.toast = m.toast ? Object.assign({ shown: Date.now() }, m.toast) : null;
        if ('remote' in m) S.remote = m.remote;
        S.rev++;
        return ok;
    }

    // Older items from fetch_items (ascending, all < our first ord).
    function prependItems(id, items) {
        const ag = S.agents.get(Number(id));
        if (!ag || !items || !items.length) return 0;
        const first = ag.items.length ? ag.items[0].ord : Infinity;
        const older = items.filter((it) => it.ord < first);
        ag.items = older.concat(ag.items);
        S.rev++;
        return older.length;
    }

    function addNote(n) {
        S.notes.unshift({ kind: n.kind, text: n.text, agent: n.agent, at: n.at || Date.now() });
        if (S.notes.length > MAX_NOTES) S.notes.length = MAX_NOTES;
        save('notes', S.notes);
        S.rev++;
    }

    function setPref(key, val) { S.ui[key] = val; save(key, val); S.rev++; }

    M.store = { S, load, save, agent, agentList, applySnapshot, applyDelta, prependItems, addNote, setPref };
})(window.Mantra = window.Mantra || {});
