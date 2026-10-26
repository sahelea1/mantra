// Mantra web UI — configuration: Settings as sections (a phone lists them, each a page of its
// own; a desktop shows them on one scrolling page), Models & providers with every cell edited in
// place and the Add-provider sheet, Patterns (the list and the editor), MCP servers.
//
// Nothing here knows a rule. A value goes to Mantra as typed (settings_set, model_set,
// pattern_save…), and the error string that comes back is shown next to the field it is about.
'use strict';
(function (M) {
    const h = M.h, F = M.fmt, P = M.parts, SC = M.screens;
    const S = () => M.store.S;
    const icon = F.icon;
    const changed = () => M.act.changed();

    // ── state ────────────────────────────────────────────────────────────────────────────────────
    // `settings` / `models` / `patterns` hold the last settings_get / models_get / patterns_list
    // reply; every change answers with the new one. `drafts` are typed field values not saved yet,
    // `edit` the one cell being edited, `errs` the error text per field or cell key.
    function C() {
        const ui = S().ui;
        return ui.cfg || (ui.cfg = { settings: null, models: null, patterns: null, editor: null, loading: {}, err: {}, drafts: {}, errs: {}, pending: {}, saving: {}, edit: null, open: {}, mcpTest: {}, mcpAdd: { name: '', line: '' }, addModel: { provider: '', model: '' } });
    }

    const LOADERS = { settings: 'settings_get', models: 'models_get', patterns: 'patterns_list' };
    async function load(what) {
        const c = C();
        if (c.loading[what] || S().conn.state !== 'open') return;
        c.loading[what] = true;
        changed();
        try { c[what] = await M.act.request(LOADERS[what], {}); c.err[what] = null; } catch (e) { c.err[what] = e.message; }
        c.loading[what] = false;
        changed();
    }

    // What a screen needs from Mantra, asked for on the way in and again after a reconnect.
    function onRoute(r) {
        if (r.name === 'settings') {
            const sec = r.section;
            if (!sec || SERVER_SECTION[sec] || sec === 'mcp') load('settings');
            if (!sec || sec === 'models') load('models');
            if (!sec || sec === 'patterns') load('patterns');
        } else if (r.name === 'pattern') openEditor(r.id);
    }

    // One indicator per form: "Saving…", then "Saved" for a moment, or nothing.
    function setStatus(form, st) {
        const c = C();
        c.saving[form] = st;
        if (st === 'saved') setTimeout(() => { if (C().saving[form] === 'saved') { C().saving[form] = null; changed(); } }, 2200);
        changed();
    }
    function savingBadge(form) {
        const st = C().saving[form];
        if (st === 'saving') return h('span', { class: 'saving', role: 'status' }, h('span', { class: 'spin' }), 'Saving…');
        if (st === 'saved') return h('span', { class: 'saving ok', role: 'status' }, icon('check'), 'Saved');
        return null;
    }
    // A failed load: the reason, and a retry.
    function loadBox(what, title) {
        const c = C();
        if (c.err[what]) return P.errorBox('Could not load ' + title + ': ' + c.err[what], () => load(what));
        return P.loading('Loading ' + title + '…');
    }
    function errLine(key) {
        const e = C().errs[key];
        return e ? h('div', { class: 'fld-err', role: 'alert', key: 'err-' + key }, e) : null;
    }

    // ── sections ─────────────────────────────────────────────────────────────────────────────────
    // `server` names the section of Settings::fields a section shows (settings_ops.rs).
    const SECTIONS = [
        { id: 'solo', title: 'Solo', icon: 'agent', server: 'Solo' },
        { id: 'runs', title: 'Runs', icon: 'run', server: 'Runs' },
        { id: 'features', title: 'Features', icon: 'spark', server: 'Features' },
        { id: 'mcp', title: 'MCP servers', icon: 'tool' },
        { id: 'models', title: 'Models & providers', icon: 'model' },
        { id: 'patterns', title: 'Patterns', icon: 'plan' },
        { id: 'web', title: 'Web & remote', icon: 'globe', server: 'Web & remote' },
        { id: 'appearance', title: 'Appearance', icon: 'sun', server: 'Appearance' },
        { id: 'notifications', title: 'Notifications', icon: 'bell' },
        { id: 'about', title: 'About', icon: 'info' },
    ];
    const SERVER_SECTION = {};
    for (const x of SECTIONS) if (x.server) SERVER_SECTION[x.id] = x.server;

    function value(key) {
        const st = C().settings;
        const f = st && st.fields.find((x) => x.key === key);
        return f ? f.value : '';
    }
    // The row under a section's name in the phone's list: what is set there now.
    function sectionSub(x) {
        const c = C(), s = S();
        switch (x.id) {
            case 'solo': return c.settings ? value('default_model') + ' · approvals ' + value('approval_mode') + ' · ' + value('sandbox') : 'model, approvals, sandbox';
            case 'runs': return c.settings ? 'new runs use ' + value('default_pattern') : 'the pattern for new runs, role models';
            case 'features': return c.settings ? 'Akasha ' + value('experimental.akasha') : 'experimental features';
            case 'mcp': return c.settings ? (c.settings.mcp.length ? F.plural(c.settings.mcp.length, 'server') + ': ' + c.settings.mcp.map((m) => m.name).join(', ') : 'none yet — tools for your agents') : 'tools for your agents';
            case 'models': return c.models ? F.plural(c.models.models.length, 'model') + ' · ' + F.plural(c.models.providers.length, 'provider') : 'aliases, providers, keys';
            case 'patterns': return c.patterns ? F.plural(c.patterns.patterns.length, 'pattern') + ' · new runs use ' + c.patterns.current : 'roles, models and flow of a run';
            case 'web': return s.remote && s.remote.enabled ? 'remote access on' : 'listen address, password, remote access';
            case 'appearance': return 'theme, motion, the terminal’s look';
            case 'notifications': return M.push.state().subscribed ? 'on for this device' : 'halts, questions and approvals on this device';
            case 'about': return 'Mantra ' + ((s.app && s.app.version) || '');
        }
        return '';
    }

    function sectionList() {
        return h('div', { class: 'card flush', key: 'sections' }, SECTIONS.map((x) => h('button', { type: 'button', class: 'row nav-row', key: x.id, onclick: () => M.act.nav('/settings/' + x.id) },
            h('span', { class: 'nav-ico' }, icon(x.icon)),
            h('span', { class: 'row-main' }, h('span', { class: 'row-title' }, x.title), h('span', { class: 'row-sub' }, h('span', { class: 'st' }, sectionSub(x)))),
            icon('chevron', 'chev'))));
    }

    // `alone`: the section is the page (its title is in the bar above, not on its first card).
    function sectionBody(x, alone) {
        const title = alone ? null : x.title;
        switch (x.id) {
            case 'solo': case 'features': return [fieldsCard(x, title)];
            case 'runs': return [fieldsCard(x, title), SC.rolesCard()];
            case 'mcp': return [mcpCard()];
            case 'models': return [modelsCard(), providersCard()];
            case 'patterns': return [patternsCard()];
            case 'web': return [fieldsCard(x, alone ? 'For the next start' : x.title), servingCard(), SC.remoteCard(), SC.certCard(), SC.sessionCard()];
            case 'appearance': return [SC.appearanceCard('This browser'), fieldsCard(x, 'Terminal', 'terminal')];
            case 'notifications': return [SC.notificationsCard()];
            case 'about': return [SC.aboutCard()];
        }
        return [];
    }

    function settingsScreen(r) {
        const sec = r.section && SECTIONS.find((x) => x.id === r.section);
        if (sec) return SC.page('settings-' + sec.id, [demoNote(), sectionBody(sec, true)], 'settings-page');
        if (!M.act.wide()) return SC.page('settings', [demoNote(), sectionList()], 'narrow');
        // Desktop: every section on one page, an index of them on top.
        const jump = (id) => { const el = document.getElementById('set-' + id); if (el) el.scrollIntoView({ behavior: M.act.motion() ? 'smooth' : 'auto', block: 'start' }); };
        return SC.page('settings', [
            h('nav', { class: 'set-index', key: 'index', 'aria-label': 'Sections' }, SECTIONS.map((x) => h('button', { type: 'button', class: 'chip', key: x.id, onclick: () => jump(x.id) }, x.title))),
            demoNote(),
            SECTIONS.map((x) => h('section', { class: 'set-sec', key: x.id, id: 'set-' + x.id, 'aria-label': x.title }, sectionBody(x, false))),
        ], 'settings-page');
    }

    // The topbar's title for a configuration route ('' = the shell's own).
    function title(r) {
        if (r.name === 'pattern') return r.id || 'Pattern';
        if (r.name !== 'settings') return '';
        const sec = r.section && SECTIONS.find((x) => x.id === r.section);
        return sec ? sec.title : 'Settings';
    }

    // ── fields (Settings::fields) ────────────────────────────────────────────────────────────────
    async function setSetting(f, val, form) {
        const c = C();
        val = String(val);
        if (c.pending[f.key] === val) return;
        c.pending[f.key] = val;
        setStatus(form, 'saving');
        try {
            const d = await M.act.request('settings_set', { key: f.key, value: val });
            c.settings = d.settings;
            delete c.drafts[f.key];
            delete c.errs[f.key];
            setStatus(form, 'saved');
            if (d.note) M.act.flash(f.label + ' saved — ' + d.note, 'info');
        } catch (e) {
            c.errs[f.key] = e.message;
            setStatus(form, null);
        } finally { delete c.pending[f.key]; }
    }

    function fieldRow(f, form) {
        const c = C();
        const id = 'fld-' + f.key.replace(/\W/g, '-');
        const draft = c.drafts[f.key];
        const commit = (v) => {
            if (v === f.value) { delete c.drafts[f.key]; delete c.errs[f.key]; changed(); return; }
            setSetting(f, v, form);
        };
        const keys = (e) => {
            if (e.key === 'Enter') { e.preventDefault(); commit(e.target.value); }
            else if (e.key === 'Escape') { e.preventDefault(); e.stopPropagation(); delete c.drafts[f.key]; delete c.errs[f.key]; changed(); }
        };
        let ctl;
        if (f.kind === 'bool') {
            ctl = h('input', { id, type: 'checkbox', class: 'switch', checked: f.value === 'on', onchange: (e) => setSetting(f, e.target.checked ? 'on' : 'off', form) });
        } else if (f.kind === 'enum') {
            const opts = (f.options || []).slice();
            if (f.value && !opts.includes(f.value)) opts.unshift(f.value);
            ctl = h('select', { id, class: 'select', value: f.value, onchange: (e) => setSetting(f, e.target.value, form) }, opts.map((o) => h('option', { value: o, key: o, selected: o === f.value }, o)));
        } else if (f.kind === 'secret') {
            const set = f.value === 'set';
            ctl = h('div', { class: 'fld-row' },
                h('input', {
                    id, class: 'input', type: 'password', autocomplete: 'new-password', value: draft || '', placeholder: set ? 'set — type a new one to replace it' : 'not set',
                    oninput: (e) => { c.drafts[f.key] = e.target.value; changed(); }, onkeydown: (e) => { if (e.key === 'Enter' && !e.target.value) { e.preventDefault(); return; } keys(e); },
                    onblur: (e) => { if (e.target.value) commit(e.target.value); },
                }),
                set ? P.btn('Clear', () => M.act.sheet({ kind: 'confirm', danger: true, title: 'Clear the ' + f.label.toLowerCase() + '?', body: f.hint, confirm: 'Clear', run: () => setSetting(f, '', form) }), { sm: true, kind: 'ghost', key: 'clear' }) : null);
        } else {
            ctl = h('input', {
                id, class: 'input' + (f.kind === 'number' ? ' num' : ''), type: f.kind === 'number' ? 'number' : 'text', min: f.min, max: f.max, inputmode: f.kind === 'number' ? 'numeric' : null,
                value: draft !== undefined ? draft : f.value, spellcheck: 'false', autocapitalize: 'off', autocomplete: 'off', placeholder: f.key === 'web.listen' ? '127.0.0.1:7777' : null,
                oninput: (e) => { c.drafts[f.key] = e.target.value; changed(); }, onkeydown: keys,
                onblur: (e) => { if (c.drafts[f.key] !== undefined) commit(e.target.value); },
            });
        }
        return h('div', { class: 'fld' + (f.kind === 'bool' ? ' fld-bool' : '') + (c.errs[f.key] ? ' has-err' : ''), key: f.key },
            h('div', { class: 'fld-text' }, h('label', { for: id }, f.label), h('small', null, f.hint), f.effect ? h('small', { class: 'fld-effect' }, '↻ ' + f.effect) : null),
            h('div', { class: 'fld-ctl' }, ctl),
            errLine(f.key));
    }

    function demoNote() {
        return S().app && S().app.demo ? h('div', { class: 'notice-card demo-note', key: 'demo' }, icon('info'), h('span', null, 'Demo: what you change here lasts for this session only — nothing is saved.')) : null;
    }
    function fieldsCard(x, cardTitle, ico) {
        const st = C().settings;
        const fields = st ? st.fields.filter((f) => f.section === x.server) : [];
        const body = !st ? loadBox('settings', 'the settings') : fields.map((f) => fieldRow(f, x.id));
        const badge = savingBadge(x.id);
        // a card without a title still shows its saving indicator, in a head of its own
        const lone = !cardTitle && badge ? h('div', { class: 'sec-head lone', key: 'lone' }, h('span', { class: 'spacer' }), badge) : null;
        return SC.card(cardTitle, [lone, h('div', { class: 'fields', key: 'f' }, body)], { key: 'fields-' + x.id, icon: cardTitle ? ico || x.icon : null, right: badge, cls: 'fields-card' });
    }

    // What the web layer serves right now (as opposed to the settings for the next start).
    function servingCard() {
        const st = C().settings;
        const w = st && st.web;
        if (!w) return null;
        const rows = [
            ['Serving', (w.urls || []).length ? w.urls.join('  ') : 'only through the remote relay'],
            ['HTTPS', w.tls ? (w.self_signed ? 'on, Mantra’s own certificate' : 'on') : 'off'],
            ['Password', w.password ? 'set' : 'none (localhost only)'],
            ['Push', w.push ? 'on' : 'off'],
        ];
        return SC.card('Running now', [
            h('p', { class: 'sec-sub', key: 's' }, 'The fields above apply at the next start; this is what this session serves.'),
            h('dl', { class: 'kv', key: 'kv' }, rows.map(([k, v]) => [h('dt', { key: 'k' + k }, k), h('dd', { key: 'v' + k, class: k === 'Serving' ? 'mono' : null }, v)])),
        ], { key: 'serving', icon: 'globe' });
    }

    // ── MCP servers ──────────────────────────────────────────────────────────────────────────────
    async function mcp(cmdName, args, form, errKey) {
        const c = C();
        setStatus(form, 'saving');
        try {
            const d = await M.act.request(cmdName, args);
            c.settings = d.settings;
            delete c.errs[errKey];
            setStatus(form, 'saved');
            return true;
        } catch (e) {
            c.errs[errKey] = e.message;
            setStatus(form, null);
            return false;
        }
    }
    async function mcpTest(name) {
        const c = C();
        c.mcpTest[name] = { busy: true };
        changed();
        try {
            const d = await M.act.request('mcp_test', { name }, 90000);
            c.mcpTest[name] = { ok: true, text: d.report + (d.enabled ? '' : '\n(disabled: no agent gets it until it is switched on)') };
        } catch (e) { c.mcpTest[name] = { ok: false, text: e.message }; }
        changed();
    }
    function mcpCard() {
        const c = C(), st = c.settings;
        if (!st) return SC.card('MCP servers', loadBox('settings', 'the MCP servers'), { key: 'mcp', icon: 'tool' });
        const rows = st.mcp.map((m) => {
            const t = c.mcpTest[m.name];
            return h('div', { class: 'mcp-row', key: m.name, 'data-mcp': m.name },
                h('div', { class: 'mcp-main' },
                    h('div', { class: 'mcp-top' }, h('b', null, m.name), m.solo ? h('span', { class: 'mini-chip', title: 'Solo agents get it (Settings › Solo)' }, 'Solo') : null,
                        (m.env || []).map((k) => h('span', { class: 'mini-chip', key: k, title: 'an environment variable it is given (its value stays on this machine)' }, '$' + k))),
                    h('code', { class: 'mono dim mcp-line' }, m.line),
                    m.problem ? h('div', { class: 'fld-err' }, m.problem) : null),
                h('div', { class: 'mcp-acts' },
                    h('input', { type: 'checkbox', class: 'switch', checked: !!m.enabled, title: m.enabled ? 'On — agents asking for it get it' : 'Off', 'aria-label': (m.enabled ? 'Disable ' : 'Enable ') + m.name, onchange: (e) => mcp('mcp_set', { name: m.name, enabled: e.target.checked }, 'mcp', 'mcp:' + m.name) }),
                    P.btn('Test', () => mcpTest(m.name), { sm: true, kind: 'ghost', icon: 'check', disabled: t && t.busy, key: 'test' }),
                    P.iconBtn('trash', () => M.act.sheet({ kind: 'confirm', danger: true, title: 'Remove ' + m.name + '?', body: 'Takes ' + m.name + ' out of settings.toml. Agents running now keep it until they end; new ones go without.', confirm: 'Remove', run: () => mcp('mcp_remove', { name: m.name }, 'mcp', 'mcp:' + m.name) }), 'Remove ' + m.name)),
                t ? h('pre', { class: 'mcp-test' + (t.busy ? '' : t.ok ? ' ok' : ' bad'), key: 'test' }, t.busy ? 'Starting ' + m.name + '…' : t.text) : null,
                errLine('mcp:' + m.name));
        });
        const A = c.mcpAdd;
        const add = async (e) => {
            e.preventDefault();
            if (!A.name.trim() || !A.line.trim()) return;
            if (await mcp('mcp_set', { name: A.name.trim(), command: A.line }, 'mcp', 'mcp:add')) { c.mcpAdd = { name: '', line: '' }; changed(); }
        };
        return SC.card('MCP servers', [
            h('p', { class: 'sec-sub', key: 'sub' }, 'Tools from other programs — Blender, a browser, a database — for your agents. Solo gets the ones Settings › Solo names; a pattern role the ones its MCP field names.'),
            rows.length ? h('div', { class: 'mcp-list', key: 'list' }, rows) : h('div', { class: 'row-empty', key: 'none' }, 'No MCP servers yet.'),
            h('form', { class: 'mcp-add', key: 'add', onsubmit: add },
                h('input', { class: 'input', value: A.name, placeholder: 'name, e.g. blender', 'aria-label': 'Server name', autocapitalize: 'off', spellcheck: 'false', oninput: (e) => { A.name = e.target.value; changed(); } }),
                h('input', { class: 'input mono', value: A.line, placeholder: 'command line, e.g. uvx blender-mcp', 'aria-label': 'Command line', autocapitalize: 'off', spellcheck: 'false', oninput: (e) => { A.line = e.target.value; changed(); } }),
                P.btn('Add', null, { type: 'submit', icon: 'plus', disabled: !A.name.trim() || !A.line.trim() })),
            errLine('mcp:add'),
        ], { key: 'mcp', icon: 'tool', right: savingBadge('mcp') });
    }

    // ── cells edited in place ────────────────────────────────────────────────────────────────────
    // A cell shows its value; a tap turns it into an input with the value in it. Enter (or leaving
    // the field) saves, Esc puts it back. One cell at a time.
    function cell(k, shown, raw, save, opts) {
        opts = opts || {};
        const c = C();
        if (c.edit && c.edit.k === k) {
            const done = (keep) => {
                const e = C().edit;
                if (!e || e.k !== k) return;
                C().edit = null;
                if (keep && e.value !== e.orig) save(e.value);
                changed();
            };
            return h('input', {
                key: k + ':edit', class: 'input cell-input' + (opts.mono ? ' mono' : ''), type: opts.secret ? 'password' : 'text', value: c.edit.value, 'aria-label': opts.label || null,
                placeholder: opts.placeholder || '', autocomplete: opts.secret ? 'new-password' : 'off', spellcheck: 'false', autocapitalize: 'off',
                ref: (el) => requestAnimationFrame(() => { el.focus(); if (!opts.secret) el.select(); }),
                oninput: (e) => { c.edit.value = e.target.value; },
                onkeydown: (e) => { if (e.key === 'Enter') { e.preventDefault(); done(true); } else if (e.key === 'Escape') { e.preventDefault(); e.stopPropagation(); done(false); } },
                onblur: () => done(true),
            });
        }
        const v = raw === undefined || raw === null ? '' : String(raw);
        return h('button', {
            type: 'button', key: k, class: 'cell-btn' + (opts.mono ? ' mono' : '') + (c.errs[k] ? ' bad' : ''), title: opts.title || 'Edit',
            'aria-label': (opts.label || 'value') + ': ' + (shown || 'empty') + ' — edit',
            onclick: () => { c.edit = { k, value: opts.secret ? '' : v, orig: opts.secret ? '' : v }; changed(); },
        }, shown ? h('span', { class: 'cell-v' }, shown) : h('span', { class: 'cell-v dim' }, opts.empty || '—'), icon('edit', 'cell-ico'));
    }

    // ── models & providers ───────────────────────────────────────────────────────────────────────
    // Every change answers with the whole registry: the table is redrawn from that.
    async function reg(cmdName, args, errKey, ok) {
        const c = C();
        setStatus('models', 'saving');
        try {
            const d = await M.act.request(cmdName, args);
            if (d && d.registry) c.models = d.registry;
            delete c.errs[errKey];
            setStatus('models', 'saved');
            if (ok) M.act.flash(typeof ok === 'function' ? ok(d) : ok, 'ok');
            return d;
        } catch (e) {
            c.errs[errKey] = e.message;
            setStatus('models', null);
            return null;
        }
    }
    const modelSet = (alias, field) => (v) => reg('model_set', { alias, field, value: String(v) }, 'm:' + alias + ':' + field);
    const providerSet = (id, field) => (v) => reg('provider_set', { id, field, value: String(v) }, 'p:' + id + ':' + field);

    // The cell errors of one row, under it.
    function rowErrs(prefix) {
        const errs = Object.entries(C().errs).filter(([k]) => k.startsWith(prefix));
        return errs.length ? h('div', { class: 'mt-errs', key: 'errs' }, errs.map(([k, e]) => h('div', { class: 'fld-err', key: k, role: 'alert' }, e))) : null;
    }

    async function modelTest(alias) {
        const c = C();
        c.testing = Object.assign({}, c.testing, { [alias]: true });
        changed();
        try {
            const d = await M.act.request('model_test', { alias }, 150000);
            M.act.flash(alias + ': ' + d.status, 'ok');
        } catch (e) {
            M.act.flash(alias + ': ' + e.message, 'error');
        }
        delete c.testing[alias];
        load('models');
    }

    function modelRow(m, R) {
        const c = C();
        const k = (f) => 'm:' + m.alias + ':' + f;
        const more = !!c.open['m:' + m.alias];
        // Codex's login is an account rather than a provider row until its first model is added.
        const provs = ['openai', ...R.providers.filter((p) => p.id !== 'openai').map((p) => p.id)];
        if (!provs.includes(m.provider)) provs.unshift(m.provider);
        const select = (field, val, opts, label) => h('select', { class: 'select cell-select', value: val, 'aria-label': label + ' of ' + m.alias, onchange: (e) => modelSet(m.alias, field)(e.target.value) },
            opts.map((o) => h('option', { value: o, key: o, selected: o === val }, o)));
        const testing = c.testing && c.testing[m.alias];
        const status = testing ? 'testing…' : m.test || '';
        return h('div', { class: 'mt-row' + (m.problem ? ' problem' : '') + (more ? ' open' : ''), key: m.alias, role: 'row', 'data-alias': m.alias },
            h('div', { class: 'mt-cell c-alias', 'data-label': 'Alias', role: 'cell' }, cell(k('alias'), m.alias, m.alias, modelSet(m.alias, 'alias'), { label: 'Alias', mono: true }),
                m.default ? h('span', { class: 'mini-chip', title: 'Solo’s default model (Settings › Solo)' }, 'Solo') : null),
            h('div', { class: 'mt-cell', 'data-label': 'Provider', role: 'cell' }, select('provider', m.provider, provs, 'Provider')),
            h('div', { class: 'mt-cell c-model', 'data-label': 'Model', role: 'cell' }, cell(k('model'), m.model, m.model, modelSet(m.alias, 'model'), { label: 'Model id', mono: true })),
            h('div', { class: 'mt-cell', 'data-label': 'Context', role: 'cell' }, cell(k('context'), m.context_window ? F.tokens(m.context_window) : '', m.context_window ? F.tokens(m.context_window) : '', modelSet(m.alias, 'context'), { label: 'Context window', empty: '200k?', title: 'Tokens: 200000, 200k or 1m — empty assumes 200k', placeholder: '200k' })),
            h('div', { class: 'mt-cell', 'data-label': 'Effort', role: 'cell' }, m.efforts.length ? select('effort', m.default_effort, m.efforts, 'Effort') : h('span', { class: 'dim cell-none', title: 'No effort levels: set some under More' }, 'none')),
            h('div', { class: 'mt-cell c-vision', 'data-label': 'Vision', role: 'cell' }, h('input', { type: 'checkbox', class: 'switch', checked: !!m.vision, title: 'Takes images (screenshots)', 'aria-label': 'Vision for ' + m.alias, onchange: (e) => modelSet(m.alias, 'vision')(e.target.checked ? 'on' : 'off') })),
            h('div', { class: 'mt-cell c-acts', role: 'cell' },
                P.btn('Test', () => modelTest(m.alias), { sm: true, kind: 'ghost', disabled: !!testing, key: 'test', title: 'Ask the model for one word' }),
                P.iconBtn(more ? 'up' : 'down', () => { c.open['m:' + m.alias] = !more; changed(); }, more ? 'Fewer fields' : 'More fields', { key: 'more' }),
                P.iconBtn('trash', () => M.act.sheet({ kind: 'confirm', danger: true, title: 'Delete ' + m.alias + '?', body: m.alias + ' (' + m.model + ') leaves models.toml. Patterns that name it need another model.', confirm: 'Delete', run: () => reg('model_delete', { alias: m.alias }, k('delete'), 'Deleted ' + m.alias) }), 'Delete ' + m.alias, { key: 'del' })),
            status || m.problem ? h('div', { class: 'mt-note', key: 'note' },
                status ? h('span', { class: 'mt-status' + (/^ok/.test(status) ? ' ok' : /^error/.test(status) ? ' bad' : '') }, status) : null,
                m.problem ? h('span', { class: 'mt-problem' }, m.problem) : null) : null,
            more ? h('div', { class: 'mt-more', key: 'more' },
                labelled('Compacts at', cell(k('compact'), m.compact_percent ? m.compact_percent + '%' : '', m.compact_percent || '', modelSet(m.alias, 'compact'), { label: 'Compacts at', empty: '85%', placeholder: '85' })),
                labelled('Efforts', cell(k('efforts'), m.efforts.join(', '), m.efforts.join(', '), modelSet(m.alias, 'efforts'), { label: 'Efforts', empty: 'none', placeholder: 'low, medium, high' })),
                labelled('Note', cell(k('note'), m.note, m.note, modelSet(m.alias, 'note'), { label: 'Note' })),
                labelled('Backend', h('span', { class: 'cell-static' }, F.backend(m.backend) + ' · ' + m.provider_name))) : null,
            rowErrs('m:' + m.alias + ':'));
    }
    function labelled(label, ctl) { return h('div', { class: 'mt-kv', key: label }, h('span', { class: 'mt-k' }, label), ctl); }

    function modelsCard() {
        const c = C(), R = c.models;
        if (!R) return SC.card('Models', loadBox('models', 'the models'), { key: 'models', icon: 'model' });
        const A = c.addModel;
        const provs = R.providers.map((p) => p.id);
        const provider = provs.includes(A.provider) ? A.provider : provs.find((p) => p !== 'openai') || provs[0] || '';
        const add = async (e) => {
            e.preventDefault();
            const d = await reg('model_add', { provider, model: A.model.trim() || undefined }, 'm-add', (x) => 'Added ' + x.alias);
            if (d) { c.addModel = { provider, model: '' }; changed(); }
        };
        return SC.card('Models', [
            h('p', { class: 'sec-sub', key: 'sub' }, 'Patterns and pickers name a model by its alias. Tap a cell to change it; it is saved as you leave it.'),
            h('div', { class: 'mt models', role: 'table', 'aria-label': 'Models', key: 'table' },
                h('div', { class: 'mt-head', role: 'row' }, ['Alias', 'Provider', 'Model', 'Context', 'Effort', 'Vision', ''].map((x, i) => h('span', { key: i, role: 'columnheader' }, x))),
                R.models.map((m) => modelRow(m, R))),
            h('form', { class: 'mt-add', key: 'add', onsubmit: add },
                h('select', { class: 'select', value: provider, 'aria-label': 'Provider of the new model', onchange: (e) => { A.provider = e.target.value; changed(); } }, provs.map((p) => h('option', { value: p, key: p, selected: p === provider }, p))),
                h('input', { class: 'input mono', value: A.model, placeholder: 'model id, as the provider names it', 'aria-label': 'Model id', autocapitalize: 'off', spellcheck: 'false', oninput: (e) => { A.model = e.target.value; changed(); } }),
                P.btn('Add model', null, { type: 'submit', icon: 'plus', disabled: !provider })),
            errLine('m-add'),
        ], { key: 'models', icon: 'model', right: savingBadge('models') });
    }

    function providerRow(p) {
        const c = C();
        const k = (f) => 'p:' + p.id + ':' + f;
        const more = !!c.open['p:' + p.id];
        const del = () => M.act.sheet({
            kind: 'confirm', danger: true,
            title: p.account ? 'Remove the Codex account?' : 'Delete ' + p.id + '?',
            body: p.account ? 'Removes its ' + F.plural(p.models, 'model') + ' from models.toml. You can add Codex models again by id.' : 'Deletes ' + p.id + (p.models ? ' and its ' + F.plural(p.models, 'model') : '') + ' from models.toml, its key with it.',
            confirm: p.account ? 'Remove' : 'Delete', run: () => reg('provider_delete', { id: p.id }, k('delete'), (d) => (p.account ? 'Codex account removed' : 'Deleted ' + p.id) + (d.models ? ' · ' + F.plural(d.models, 'model') : '')),
        });
        if (p.account) {
            return h('div', { class: 'mt-row account', key: 'acct', role: 'row', 'data-provider': 'openai' },
                h('div', { class: 'mt-cell c-alias', 'data-label': 'Id', role: 'cell' }, h('span', { class: 'cell-static mono' }, 'openai')),
                h('div', { class: 'mt-cell c-wide', 'data-label': 'Name', role: 'cell' }, h('span', { class: 'cell-static', title: 'Codex runs these models on its own ChatGPT login: there is nothing to set here' }, p.name + ' — the ChatGPT login of Codex, nothing to set')),
                h('div', { class: 'mt-cell', 'data-label': 'Kind', role: 'cell' }, h('span', { class: 'cell-static' }, 'codex')),
                h('div', { class: 'mt-cell', 'data-label': 'Models', role: 'cell' }, h('span', { class: 'cell-static' }, F.plural(p.models, 'model'))),
                h('div', { class: 'mt-cell c-acts', role: 'cell' }, P.iconBtn('trash', del, 'Remove the Codex account', { key: 'del' })),
                rowErrs('p:openai:'));
        }
        const R = c.models;
        const opts = R.options || {};
        const select = (field, val, list, label) => h('select', { class: 'select cell-select', value: val, 'aria-label': label + ' of ' + p.id, onchange: (e) => providerSet(p.id, field)(e.target.value) },
            list.map((o) => h('option', { value: o, key: o, selected: o === val }, o)));
        const key = p.api_key || (p.env_key ? '$' + p.env_key : '');
        const find = () => M.act.sheet({ kind: 'add-provider', step: 'key', key: '', url: p.base_url, urlTouched: true, existing: p.id, draft: null, error: null, result: null, sel: {}, filter: '' });
        return h('div', { class: 'mt-row' + (p.draft ? ' problem' : '') + (more ? ' open' : ''), key: p.id, role: 'row', 'data-provider': p.id },
            h('div', { class: 'mt-cell c-alias', 'data-label': 'Id', role: 'cell' }, cell(k('id'), p.id, p.id, providerSet(p.id, 'id'), { label: 'Id', mono: true })),
            h('div', { class: 'mt-cell', 'data-label': 'Name', role: 'cell' }, cell(k('name'), p.name, p.name, providerSet(p.id, 'name'), { label: 'Name' })),
            h('div', { class: 'mt-cell c-model', 'data-label': 'Base URL', role: 'cell' }, cell(k('base_url'), p.base_url, p.base_url, providerSet(p.id, 'base_url'), { label: 'Base URL', mono: true, placeholder: 'https://…/v1' })),
            h('div', { class: 'mt-cell', 'data-label': 'Key', role: 'cell' }, cell(k('api_key'), key, '', providerSet(p.id, 'api_key'), { label: 'API key', secret: true, mono: true, empty: p.kind === 'claude-code' && p.auth === 'subscription' ? 'login' : 'no key', placeholder: 'paste a new key', title: 'Paste a new key (it is never shown again)' })),
            h('div', { class: 'mt-cell', 'data-label': 'Kind', role: 'cell' }, select('kind', p.kind, opts.kinds || ['bija', 'codex', 'claude-code'], 'Kind')),
            h('div', { class: 'mt-cell', 'data-label': p.kind === 'claude-code' ? 'Auth' : 'Wire', role: 'cell' },
                p.kind === 'bija' ? select('wire', p.wire || 'openai', opts.wires || ['openai', 'anthropic'], 'Wire')
                    : p.kind === 'claude-code' ? select('auth', p.auth, opts.auths || ['subscription', 'api_key'], 'Auth') : h('span', { class: 'dim cell-none' }, '—')),
            h('div', { class: 'mt-cell c-acts', role: 'cell' },
                p.kind === 'bija' ? P.btn('Find models', find, { sm: true, kind: 'ghost', key: 'find', title: 'Ask ' + (p.host || p.id) + ' for its model list', disabled: !p.base_url }) : null,
                P.iconBtn(more ? 'up' : 'down', () => { c.open['p:' + p.id] = !more; changed(); }, more ? 'Fewer fields' : 'More fields', { key: 'more' }),
                P.iconBtn('trash', del, 'Delete ' + p.id, { key: 'del' })),
            p.draft ? h('div', { class: 'mt-note', key: 'note' }, h('span', { class: 'mt-problem' }, p.draft)) : null,
            more ? h('div', { class: 'mt-more', key: 'more' },
                labelled('Key from', cell(k('env_key'), p.env_key ? '$' + p.env_key : '', p.env_key, providerSet(p.id, 'env_key'), { label: 'Environment variable', empty: 'no variable', placeholder: 'MY_PROVIDER_API_KEY', mono: true })),
                labelled('Models', h('span', { class: 'cell-static' }, String(p.models))),
                p.api_key ? labelled('Stored key', P.btn('Clear it', () => M.act.sheet({ kind: 'confirm', danger: true, title: 'Clear the key of ' + p.id + '?', body: 'The key stored in models.toml goes; ' + (p.env_key ? '$' + p.env_key + ' is read instead.' : 'requests then go without one.'), confirm: 'Clear', run: () => providerSet(p.id, 'api_key')('') }), { sm: true, kind: 'danger-ghost', key: 'clear' })) : null,
                p.host ? labelled('Requests go to', h('span', { class: 'cell-static mono' }, p.host)) : null) : null,
            rowErrs('p:' + p.id + ':'));
    }

    function providersCard() {
        const c = C(), R = c.models;
        if (!R) return null;
        const B = R.backends || {};
        const addClaude = async (auth) => {
            const added = await reg('add_backend', { kind: 'claude-code' }, 'backend');
            if (!added) return;
            const p = C().models.providers.find((x) => x.kind === 'claude-code');
            if (!p) return;
            if (!await reg('provider_set', { id: p.id, field: 'auth', value: auth }, 'backend')) return;
            if (auth === 'api_key') {
                await reg('provider_set', { id: p.id, field: 'env_key', value: 'ANTHROPIC_API_KEY' }, 'backend');
                M.act.flash('Claude Code: paste an API key in its row, or set $ANTHROPIC_API_KEY', 'ok');
            } else {
                M.act.flash('Claude Code uses your subscription login', 'ok');
            }
        };
        const addCodex = () => {
            c.addModel.provider = 'openai';
            changed();
            requestAnimationFrame(() => document.querySelector('input[aria-label="Model id"]')?.focus());
        };
        const open = () => M.act.sheet({ kind: 'add-provider', step: 'key', choice: '', key: '', url: '', urlTouched: false, draft: null, error: null, result: null, sel: {}, filter: '' });
        return SC.card('Providers', [
            h('p', { class: 'sec-sub', key: 'sub' }, 'Where the models run. Keys are stored in ~/.mantra/models.toml, readable by you only, and never shown again.'),
            h('div', { class: 'mt providers', role: 'table', 'aria-label': 'Providers', key: 'table' },
                h('div', { class: 'mt-head', role: 'row' }, ['Id', 'Name', 'Base URL', 'Key', 'Kind', 'Wire / auth', ''].map((x, i) => h('span', { key: i, role: 'columnheader' }, x))),
                R.providers.map(providerRow)),
            h('div', { class: 'form-row start', key: 'acts' },
                P.btn('Add API provider', open, { kind: 'primary', icon: 'plus', key: 'add' }),
                !R.providers.some((p) => p.kind === 'bija') ? P.btn('Bija · inactive', open, { key: 'bija-status', disabled: true, title: 'Choose Add API provider to configure Bija' }) : null,
                P.btn('Add Codex model', addCodex, { icon: 'plus', key: 'codex', disabled: !(B.codex && B.codex.installed), title: 'Enter a Codex model id above; no model list is built into Mantra' }),
                P.btn('Claude Code subscription', () => addClaude('subscription'), { icon: 'plus', key: 'claude-code', disabled: !(B.claude && B.claude.installed), title: 'Uses your Claude Code login' }),
                P.btn('Claude Code API key', () => addClaude('api_key'), { icon: 'plus', key: 'claude-key', disabled: !(B.claude && B.claude.installed), title: 'Paste a key or set $ANTHROPIC_API_KEY' })),
            errLine('backend'),
        ], { key: 'providers', icon: 'globe' });
    }

    // ── the Add-provider sheet: choose an API → key → probe → checklist ────────────────────────
    // Choices fill only this form. No Bija provider or model is added until the user submits it.
    const API_CHOICES = [
        ['openai', 'OpenAI API', 'https://api.openai.com/v1'],
        ['anthropic', 'Anthropic API', 'https://api.anthropic.com/v1'],
        ['openrouter', 'OpenRouter', 'https://openrouter.ai/api/v1'],
        ['groq', 'Groq', 'https://api.groq.com/openai/v1'],
        ['xai', 'xAI', 'https://api.x.ai/v1'],
        ['gemini', 'Gemini API', 'https://generativelanguage.googleapis.com/v1beta/openai'],
        ['cerebras', 'Cerebras', 'https://api.cerebras.ai/v1'],
        ['fireworks', 'Fireworks', 'https://api.fireworks.ai/inference/v1'],
        ['local', 'Local / custom OpenAI-compatible', ''],
    ];
    let draftTimer = null;
    function askDraft(sh) {
        clearTimeout(draftTimer);
        draftTimer = setTimeout(async () => {
            const key = sh.key, url = sh.urlTouched ? sh.url : '';
            if (!key.trim() && !url.trim()) { sh.draft = null; changed(); return; }
            try {
                const d = await M.act.request('provider_from_key', { key, base_url: url.trim() || undefined, probe: false });
                if (S().ui.sheet !== sh || sh.key !== key || (sh.urlTouched ? sh.url : '') !== url) return;
                sh.draft = d.draft;
                if (!sh.urlTouched) sh.url = d.draft.base_url || '';
            } catch (_) { /* the probe says it again */ }
            changed();
        }, 300);
    }
    async function probe(sh) {
        sh.step = 'probing'; sh.error = null;
        changed();
        try {
            const d = await M.act.request('provider_from_key', { key: sh.key.trim(), base_url: sh.url.trim() || undefined }, 90000);
            if (S().ui.sheet !== sh) return;
            C().models = d.registry;
            sh.result = d;
            sh.sel = {};
            for (const x of d.candidates) if (x.selected && x.state !== 'configured') sh.sel[x.model] = true;
            sh.step = 'pick';
        } catch (e) {
            sh.step = 'key';
            sh.error = e.message;
        }
        changed();
    }
    async function importPicked(sh) {
        const r = sh.result;
        const picked = r.candidates.filter((x) => sh.sel[x.model]);
        sh.busy = true; changed();
        try {
            const d = await M.act.request('models_import', { provider: r.provider, candidates: picked });
            C().models = d.registry;
            M.act.flash((r.draft.name || r.provider) + ': ' + F.plural(d.added, 'model') + ' added' + (d.updated ? ', ' + d.updated + ' updated' : ''), 'ok');
            M.act.closeSheet();
        } catch (e) { sh.error = e.message; }
        sh.busy = false; changed();
    }
    function addProviderSheet(sh) {
        const busy = sh.step === 'probing';
        if (sh.step === 'pick') return { title: 'Models on ' + ((sh.result.draft && sh.result.draft.name) || sh.result.provider), body: pickBody(sh), cls: '' };
        const d = sh.draft;
        const hint = d && d.needs_url && !sh.url ? 'This key’s prefix names no provider — give the API’s base URL (it usually ends in /v1).'
            : d && d.guessed && !sh.urlTouched ? 'Guessed from the key — change it for a gateway or a proxy.' : '';
        const go = (e) => { if (e) e.preventDefault(); if (!busy && (sh.key.trim() || sh.url.trim())) probe(sh); };
        const body = h('form', { class: 'ap-form', key: 'f', onsubmit: go },
            sh.existing ? h('p', { class: 'sheet-text' }, 'Asks ', h('b', null, sh.existing), ' for its model list. Paste a key to replace its own, or leave it empty.')
                : h('p', { class: 'sheet-text' }, 'Choose an API, then enter its key. For a local server, enter its URL and leave the key empty. This adds a Bija backend only when you submit.'),
            !sh.existing ? [
                h('label', { class: 'select-label', for: 'ap-choice', key: 'cl' }, 'API provider'),
                h('select', { id: 'ap-choice', class: 'select', key: 'cs', value: sh.choice || '', onchange: (e) => {
                    sh.choice = e.target.value;
                    const choice = API_CHOICES.find((x) => x[0] === sh.choice);
                    sh.url = choice ? choice[2] : '';
                    sh.urlTouched = !!choice;
                    sh.error = null;
                    askDraft(sh); changed();
                } }, [h('option', { value: '', key: 'none' }, 'Select an API or paste a key'), ...API_CHOICES.map((x) => h('option', { value: x[0], key: x[0] }, x[1]))]),
            ] : null,
            h('label', { class: 'select-label', for: 'ap-key' }, 'API key'),
            h('input', { id: 'ap-key', class: 'input mono', type: 'password', autocomplete: 'off', value: sh.key, placeholder: 'the key, as the provider gave it to you', disabled: busy || null, oninput: (e) => { sh.key = e.target.value; sh.error = null; askDraft(sh); changed(); } }),
            sh.key.trim() || sh.url || sh.existing ? [
                h('label', { class: 'select-label', for: 'ap-url', key: 'l' }, 'Base URL'),
                h('input', { id: 'ap-url', key: 'u', class: 'input mono', value: sh.url, placeholder: 'https://api.example-provider.dev/v1', autocapitalize: 'off', spellcheck: 'false', disabled: busy || null, oninput: (e) => { sh.url = e.target.value; sh.urlTouched = true; sh.error = null; askDraft(sh); changed(); } }),
                hint ? h('small', { class: 'fld-hint', key: 'h' }, hint) : null,
                d && d.name && !sh.existing ? h('small', { class: 'fld-hint', key: 'n' }, d.existing
                    ? ['The key goes into ', h('b', { key: 'p' }, d.existing), ', which is at this URL already.']
                    : 'A new provider for ' + d.name + ' (' + (d.wire === 'anthropic' ? 'Messages API' : 'Chat Completions') + ') — name it in the table later.') : null,
            ] : null,
            sh.error ? h('div', { class: 'fld-err', role: 'alert' }, sh.error) : null,
            busy ? h('div', { class: 'auth-step' }, h('span', { class: 'spin' }), 'Asking ' + ((d && d.name) || sh.url || 'the provider') + ' for its models…') : null,
            h('div', { class: 'sheet-actions' },
                P.btn('Cancel', () => M.act.closeSheet(), { kind: 'ghost' }),
                P.btn('Probe', null, { type: 'submit', kind: 'primary', icon: 'search', disabled: busy || !(sh.key.trim() || sh.url.trim()) })));
        return { title: sh.existing ? 'Find models' : 'Add a provider', body, cls: '' };
    }
    function pickBody(sh) {
        const r = sh.result;
        const all = r.candidates;
        const q = sh.filter.trim().toLowerCase();
        const shown = q ? all.filter((x) => x.model.toLowerCase().includes(q)) : all;
        const n = Object.keys(sh.sel).filter((m) => sh.sel[m]).length;
        const fresh = all.filter((x) => x.state !== 'configured');
        if (r.probe_error) {
            return [
                h('div', { class: 'errbox', key: 'e' }, icon('alert'), h('span', null, r.probe_error)),
                h('p', { class: 'sheet-text', key: 't' }, h('b', null, r.provider), ' is added all the same: add its models by hand in the table, or go back, fix the key or the URL and probe again.'),
                h('div', { class: 'sheet-actions', key: 'a' }, P.btn('Back', () => { sh.step = 'key'; changed(); }, { kind: 'ghost' }), P.btn('Done', () => M.act.closeSheet(), { kind: 'primary' })),
            ];
        }
        return [
            h('p', { class: 'sheet-text', key: 't' }, F.plural(all.length, 'model') + ' found' + (fresh.length < all.length ? ', ' + (all.length - fresh.length) + ' added already' : '') + '. Pick the ones to add.'),
            all.length > 12 ? h('input', { class: 'input', key: 'q', value: sh.filter, placeholder: 'Filter…', 'aria-label': 'Filter models', oninput: (e) => { sh.filter = e.target.value; changed(); } }) : null,
            fresh.length > 1 ? h('div', { class: 'pick-tools', key: 'tools' },
                h('button', { type: 'button', class: 'link sm', onclick: () => { for (const x of shown) if (x.state !== 'configured') sh.sel[x.model] = true; changed(); } }, 'All' + (q ? ' shown' : '')),
                h('button', { type: 'button', class: 'link sm', onclick: () => { sh.sel = {}; changed(); } }, 'None')) : null,
            h('div', { class: 'checklist', key: 'list', role: 'group', 'aria-label': 'Models found' }, shown.map((x) => {
                const done = x.state === 'configured';
                return h('label', { class: 'check-row' + (done ? ' done' : ''), key: x.model },
                    h('input', { type: 'checkbox', checked: done || !!sh.sel[x.model], disabled: done || null, onchange: (e) => { sh.sel[x.model] = e.target.checked; changed(); } }),
                    h('span', { class: 'check-main' }, h('span', { class: 'mono' }, x.model),
                        h('small', null, [x.context ? F.tokens(x.context) + ' ctx' + (x.context_known ? '' : ' (assumed)') : null, x.efforts && x.efforts.length ? 'efforts' : null, x.vision ? 'vision' : null, done ? 'added already' : x.state === 'update' ? 'fills in what is missing' : null].filter(Boolean).join(' · '))));
            })),
            sh.error ? h('div', { class: 'fld-err', key: 'err', role: 'alert' }, sh.error) : null,
            h('div', { class: 'sheet-actions', key: 'a' },
                P.btn('Back', () => { sh.step = 'key'; changed(); }, { kind: 'ghost' }),
                P.btn(n ? 'Add ' + F.plural(n, 'model') : 'Add', () => importPicked(sh), { kind: 'primary', icon: 'plus', disabled: !n || sh.busy })),
        ];
    }

    // ── patterns ─────────────────────────────────────────────────────────────────────────────────
    const TAG_TEXT = { default: 'default', 'built-in': 'built-in', edited: 'edited', yours: 'yours' };
    function tagChip(tag) { return tag ? h('span', { class: 'tag t-' + tag.replace(/\W/g, '') }, TAG_TEXT[tag] || tag) : null; }
    function rolesLine(p) {
        return (p.roles || []).filter((r) => r.kind === 'worker' || r.kind === 'planner').map((r) => r.name + ' ' + r.model + (r.parallel ? ' ×' + r.parallel : '')).join(' · ');
    }
    function patternsCard() {
        const c = C(), L = c.patterns;
        if (!L) return SC.card('Patterns', loadBox('patterns', 'the patterns'), { key: 'patterns', icon: 'plan' });
        return SC.card('Patterns', [
            h('p', { class: 'sec-sub', key: 'sub' }, 'A pattern is a run’s team: its roles, their models, how phases are checked. Open one to change it, or to save a copy of your own.'),
            h('div', { class: 'role-list', key: 'list' }, L.patterns.map((p) => h('button', { type: 'button', class: 'row pat-row', key: p.name, onclick: () => M.act.nav('/patterns/' + encodeURIComponent(p.name)) },
                h('span', { class: 'nav-ico' }, icon('plan')),
                h('span', { class: 'row-main' },
                    h('span', { class: 'row-title' }, h('span', { class: 'name' }, p.name), tagChip(p.tag), p.name === L.current ? h('span', { class: 'pill sm tone-busy' }, h('i', { class: 'dot' }), 'new runs') : null),
                    p.error ? h('span', { class: 'row-sub bad' }, p.error) : h('span', { class: 'row-sub' }, h('span', { class: 'st' }, p.description)),
                    !p.error ? h('span', { class: 'row-sub pat-roles' }, h('span', { class: 'st' }, rolesLine(p))) : null),
                icon('chevron', 'chev')))),
        ], { key: 'patterns', icon: 'plan' });
    }

    // The editor works on a copy (`work`); Save sends it whole and Mantra validates it.
    async function openEditor(name) {
        const c = C();
        if (c.editor && c.editor.name === name && (c.editor.data || c.editor.loading)) return;
        // not connected yet: an empty editor, filled when the connection opens (`onRoute` again)
        c.editor = { name, loading: S().conn.state === 'open', data: null, work: null, dirty: false, errors: [], error: null };
        changed();
        if (!c.editor.loading) return;
        try {
            const d = await M.act.request('pattern_get', { name });
            if (C().editor && C().editor.name === name) Object.assign(C().editor, { data: d, work: JSON.parse(JSON.stringify(d.pattern)), loading: false });
        } catch (e) { if (C().editor) Object.assign(C().editor, { loading: false, error: e.message }); }
        changed();
    }
    function touch() { const E = C().editor; E.dirty = true; changed(); }
    async function savePattern(asName) {
        const E = C().editor;
        E.saving = true; E.errors = [];
        changed();
        try {
            const d = await M.act.request('pattern_save', { name: asName || E.name, pattern: E.work });
            M.act.flash('Saved ' + d.name, 'ok');
            C().patterns = null;
            load('patterns');
            if (d.name !== E.name) { C().editor = null; M.act.nav('/patterns/' + encodeURIComponent(d.name), { replace: true }); return true; }
            C().editor = null;
            openEditor(d.name);
            return true;
        } catch (e) {
            E.errors = String(e.message).split('\n').filter(Boolean);
            E.saving = false;
            changed();
            return false;
        }
    }
    function nums(v) { const t = String(v).trim(); const n = Number(t); return t !== '' && Number.isFinite(n) ? n : t; }

    function patternEditor(r) {
        const c = C();
        const E = c.editor;
        if (!E || E.name !== r.id) { setTimeout(() => openEditor(r.id), 0); return SC.page('pattern', [P.loading('Loading ' + r.id + '…')]); }
        if (E.error) return SC.page('pattern', [backLink(), P.errorBox('Could not open ' + r.id + ': ' + E.error, () => { C().editor = null; openEditor(r.id); })]);
        if (!E.data) return SC.page('pattern', [P.loading('Loading ' + r.id + '…')]);
        const D = E.data, W = E.work, O = D.options;
        const roleNames = Object.keys(W.roles || {});
        const KIND_ORDER = (O.kinds || []);
        roleNames.sort((a, b) => (KIND_ORDER.indexOf(W.roles[a].kind) - KIND_ORDER.indexOf(W.roles[b].kind)) || a.localeCompare(b));
        const canDelete = D.yours || D.edited;
        const del = () => M.act.sheet({
            kind: 'confirm', danger: true, title: (D.yours ? 'Delete ' : 'Undo your edits to ') + E.name + '?',
            body: D.yours ? 'Deletes ' + (D.path || 'its file') + '.' : 'Deletes ' + (D.path || 'your copy') + ' — the built-in ' + E.name + ' comes back.', confirm: D.yours ? 'Delete' : 'Undo edits',
            run: async () => {
                try {
                    await M.act.cmd('pattern_delete', { name: E.name }, { ok: D.yours ? 'Deleted ' + E.name : E.name + ' is the built-in again' });
                    C().patterns = null; load('patterns'); C().editor = null;
                    if (D.yours) M.act.nav('/settings/patterns', { replace: true }); else openEditor(E.name);
                } catch (_) { }
            },
        });
        const copy = () => M.act.sheet({ kind: 'pattern-copy', name: E.name + '-copy', error: null });
        const useIt = () => M.act.cmd('settings_set', { key: 'default_pattern', value: E.name }, { ok: 'New runs use ' + E.name }).then((d) => { D.default = true; D.current = true; C().settings = d.settings; load('patterns'); }, () => { });
        const head = SC.card(null, [
            h('div', { class: 'pe-head', key: 'h' },
                h('div', { class: 'pe-titles' }, h('h1', null, E.name, ' ', tagChip(D.tag)), D.path ? h('code', { class: 'mono dim sm' }, D.path) : h('span', { class: 'dim sm' }, 'built into Mantra — Save writes an edited copy to ~/.mantra/patterns')),
                h('div', { class: 'pe-acts' },
                    P.btn(E.saving ? 'Saving…' : 'Save', () => savePattern(), { kind: 'primary', icon: 'check', disabled: !E.dirty || E.saving, key: 'save' }),
                    P.btn('Save as copy', copy, { icon: 'copy', key: 'copy' }),
                    D.default ? null : P.btn('Use for new runs', useIt, { icon: 'play', key: 'use', kind: 'ghost' }),
                    canDelete ? P.btn(D.yours ? 'Delete' : 'Undo edits', del, { icon: 'trash', key: 'del', kind: 'danger-ghost' }) : null)),
            D.default ? h('p', { class: 'sec-sub', key: 'def' }, 'New runs use this pattern.') : null,
            h('label', { class: 'select-label', for: 'pe-desc', key: 'dl' }, 'Description'),
            h('input', { id: 'pe-desc', key: 'd', class: 'input', value: W.description || '', oninput: (e) => { W.description = e.target.value; touch(); } }),
            E.errors.length ? h('div', { class: 'errbox pe-errors', key: 'errs', role: 'alert' }, icon('alert'), h('ul', { class: 'plain' }, E.errors.map((x, i) => h('li', { key: i }, x)))) : null,
        ], { key: 'head', cls: 'pe-card' });
        return SC.page('pattern', [
            backLink(), head,
            h('h3', { class: 'group-title', key: 'rt' }, 'Roles'),
            roleNames.map((n) => roleCard(n, W.roles[n], O)),
            settingsCardP(W, O), flowCard(W, roleNames), groupsCard(W, roleNames),
            E.dirty ? h('div', { class: 'save-bar', key: 'bar', role: 'status' }, h('span', null, 'Unsaved changes'),
                P.btn('Revert', () => { E.work = JSON.parse(JSON.stringify(D.pattern)); E.dirty = false; E.errors = []; changed(); }, { sm: true, kind: 'ghost' }),
                P.btn(E.saving ? 'Saving…' : 'Save', () => savePattern(), { sm: true, kind: 'primary', disabled: E.saving })) : null,
        ], 'pattern-page');
    }
    function backLink() { return h('button', { type: 'button', class: 'link sm back-link', key: 'back', onclick: () => M.act.back('/settings/patterns') }, '‹ Patterns'); }

    // One labelled control of the editor, in a two-column grid on wide screens.
    function pf(label, ctl, hint, wide) {
        return h('div', { class: 'pf' + (wide ? ' wide' : ''), key: label }, h('span', { class: 'pf-l' }, label), ctl, hint ? h('small', { class: 'fld-hint' }, hint) : null);
    }
    function sel(val, options, onchange, label) {
        const opts = options.slice();
        if (val !== undefined && val !== null && !opts.some((o) => (Array.isArray(o) ? o[0] : o) === val)) opts.unshift(val);
        return h('select', { class: 'select', value: val, 'aria-label': label, onchange: (e) => onchange(e.target.value) },
            opts.map((o) => { const [v, t] = Array.isArray(o) ? o : [o, o]; return h('option', { value: v, key: v, selected: v === val }, t); }));
    }
    function txt(val, oninput, opts) {
        opts = opts || {};
        return h(opts.area ? 'textarea' : 'input', { class: 'input' + (opts.mono ? ' mono' : ''), rows: opts.area ? opts.rows || 6 : null, type: opts.num ? 'number' : null, min: opts.min, max: opts.max, value: val === undefined || val === null ? '' : String(val), placeholder: opts.placeholder || null, 'aria-label': opts.label || null, spellcheck: opts.area ? null : 'false', autocapitalize: 'off', oninput: (e) => oninput(e.target.value) });
    }
    function toggleCtl(on, onchange, label) { return h('input', { type: 'checkbox', class: 'switch', checked: !!on, 'aria-label': label, onchange: (e) => onchange(e.target.checked) }); }

    function roleCard(name, role, O) {
        const models = O.models || [];
        const m = models.find((x) => x.alias === role.model);
        const efforts = ['auto'].concat(m ? m.efforts : []);
        const set = (k) => (v) => { role[k] = v; touch(); };
        const worker = role.kind === 'worker';
        const glyph = h('span', { class: 'glyph lg', style: { color: F.colorVar(role.color) } }, role.glyph || '◇');
        return SC.card(null, [
            h('div', { class: 'role-head', key: 'h' }, glyph, h('div', { class: 'role-titles' }, h('b', null, name), h('span', { class: 'dim sm' }, role.kind + (worker ? role.parallel ? ' · up to ' + role.parallel + ' at once' : ' · shares the pool' : ''))),
                h('span', { class: 'dim sm role-desc' }, role.description || '')),
            h('div', { class: 'pe-grid', key: 'g' },
                pf('Model', sel(role.model, models.map((x) => x.alias), set('model'), 'Model of ' + name)),
                pf('Effort', sel(role.effort, efforts, set('effort'), 'Effort of ' + name), role.effort === 'auto' ? 'Mantra picks it per spawn from the role’s job' : null),
                pf('Sandbox', sel(role.sandbox, O.sandboxes || [], set('sandbox'), 'Sandbox of ' + name)),
                pf('Approvals', sel(role.permission, O.permissions || [], set('permission'), 'Approvals of ' + name)),
                worker ? pf('Workers at once', txt(role.parallel, (v) => { role.parallel = String(v).trim() === '' ? null : nums(v); touch(); }, { num: true, min: O.parallel[0], max: O.parallel[1], placeholder: 'shares the pool', label: 'Workers at once of ' + name }), 'A class of its own; empty shares the pattern’s pool') : null,
                pf('Fallback', sel(role.fallback || '', [['', 'none — the manager is asked']].concat(models.filter((x) => x.alias !== role.model).map((x) => x.alias)), (v) => { role.fallback = v || null; touch(); }, 'Fallback of ' + name), 'Takes over when the model gives no first token in time'),
                pf('MCP servers', txt((role.mcp || []).join(', '), (v) => { role.mcp = v.split(/[\s,]+/).filter(Boolean); touch(); }, { placeholder: O.mcp && O.mcp.length ? 'none · * for all · ' + O.mcp.join(', ') : 'none (Settings › MCP servers)', label: 'MCP servers of ' + name, mono: true })),
                pf('Description', txt(role.description, set('description'), { label: 'Description of ' + name })),
                pf('Instructions', txt(role.instructions, set('instructions'), { area: true, rows: 7, label: 'Instructions of ' + name }), null, true)),
        ], { key: 'role-' + name, cls: 'role-card' });
    }

    // Pattern settings: the ones people change first, the rest folded.
    const P_SETTINGS = [
        ['max_parallel', 'Workers at once', 'the shared pool; a role with its own number is apart', 'num'],
        ['worker_retries', 'Retries per task', '', 'num'],
        ['review_plan', 'Review the plan', 'show the plan for approval before building', 'bool'],
        ['max_tasks_per_phase', 'Tasks per phase', 'at most (a swarm phase may hold twice its width)', 'num'],
        ['isolation', 'Isolation', 'worktree per worker, or shared', 'isolation'],
    ];
    const P_MORE = [
        ['gate_max_rounds', 'Gate rounds', '', 'num'], ['orchestrator_context', 'Orchestrator context', 'fresh per phase, or compacted', 'orchestrator_context'],
        ['stall_minutes', 'Stall after (min)', '', 'num'], ['check_timeout_secs', 'Check timeout (s)', '', 'num'],
        ['watchdog_seconds', 'Watchdog (s)', '', 'num'], ['watchdog_escalate_seconds', 'Escalate after (s)', '', 'num'],
        ['review_minutes', 'Coherence review (min)', '0 is off', 'num'], ['manager_minutes', 'Manager digest (min)', '0 is off', 'num'],
        ['first_token_minutes', 'First token within (min)', '0 is off', 'num'], ['context_cap', 'Context cap (tokens)', '0 is the model’s full window', 'num'],
        ['network', 'Network for workers', '', 'bool'], ['writable_roots', 'Extra writable dirs', 'comma-separated', 'list'],
    ];
    function settingCtl(S_, [key, label, hint, kind], O) {
        const set = (v) => { S_[key] = v; touch(); };
        let ctl;
        if (kind === 'bool') ctl = toggleCtl(S_[key], set, label);
        else if (kind === 'list') ctl = txt((S_[key] || []).join(', '), (v) => set(v.split(',').map((x) => x.trim()).filter(Boolean)), { label, mono: true, placeholder: 'none' });
        else if (kind === 'num') ctl = txt(S_[key], (v) => set(nums(v)), { num: true, min: 0, label });
        else ctl = sel(S_[key], O[kind] || [], set, label);
        return pf(label, ctl, hint || null);
    }
    function settingsCardP(W, O) {
        const S_ = W.settings || (W.settings = {});
        return SC.card('Settings', [
            h('div', { class: 'pe-grid', key: 'g' }, P_SETTINGS.map((x) => settingCtl(S_, x, O))),
            h('details', { class: 'pe-more', key: 'm' }, h('summary', null, 'More settings'), h('div', { class: 'pe-grid' }, P_MORE.map((x) => settingCtl(S_, x, O)))),
        ], { key: 'p-settings', icon: 'settings' });
    }
    function flowCard(W, roles) {
        const fl = W.flow || (W.flow = {});
        const of = (kind) => roles.filter((n) => W.roles[n].kind === kind);
        const set = (k) => (v) => { fl[k] = v; touch(); };
        const fin = fl.finale || (fl.finale = []);
        return SC.card('Flow', [
            h('div', { class: 'pe-grid', key: 'g' },
                pf('Planner', sel(fl.planner, of('planner'), set('planner'), 'Planner')),
                pf('Manager', sel(fl.manager || '', [['', 'none']].concat(of('manager')), set('manager'), 'Manager'), 'watches the whole run; escalations go to it first'),
                pf('Orchestrator', sel(fl.orchestrator, of('orchestrator'), set('orchestrator'), 'Orchestrator')),
                pf('Phase gate', sel(fl.phase_gate, of('gate'), set('phase_gate'), 'Phase gate')),
                pf('Re-prompts go to', sel(fl.on_reprompt, roles, set('on_reprompt'), 'Re-prompts go to'))),
            h('div', { class: 'pf-l', key: 'fl' }, 'Finale — after the last phase, in order'),
            fin.length ? h('ol', { class: 'finale', key: 'f' }, fin.map((st, i) => h('li', { key: i },
                sel(st.role, roles, (v) => { st.role = v; touch(); }, 'Finale step ' + (i + 1) + ' role'),
                txt(st.task, (v) => { st.task = v; touch(); }, { label: 'Finale step ' + (i + 1) + ' task', placeholder: 'what this step does' }),
                P.iconBtn('trash', () => { fin.splice(i, 1); touch(); }, 'Remove step ' + (i + 1))))) : h('p', { class: 'sec-sub', key: 'none' }, 'No finale steps.'),
            h('div', { class: 'form-row start', key: 'add' }, P.btn('Add a finale step', () => { fin.push({ role: of('gate')[0] || roles[0], task: '', may_spawn: false }); touch(); }, { sm: true, icon: 'plus' })),
        ], { key: 'p-flow', icon: 'run' });
    }
    function groupsCard(W, roles) {
        const gs = W.groups || (W.groups = []);
        return SC.card('Akasha groups', [
            h('p', { class: 'sec-sub', key: 's' }, 'Roles that share one block of notes. Inert unless Settings › Features › Akasha is on.'),
            gs.map((g, i) => h('div', { class: 'group-edit', key: 'g' + i },
                h('div', { class: 'pe-grid' },
                    pf('Name', txt(g.name, (v) => { g.name = v; touch(); }, { label: 'Group name' })),
                    pf('Shared characters', txt(g.shared_chars, (v) => { g.shared_chars = nums(v); touch(); }, { num: true, label: 'Shared characters' })),
                    pf('Pushed mid-turn', sel(g.push, ['none', 'urgent', 'all'], (v) => { g.push = v; touch(); }, 'Pushed mid-turn')),
                    pf('Automatic entries', toggleCtl(g.auto, (v) => { g.auto = v; touch(); }, 'Automatic entries'))),
                h('div', { class: 'pf-l' }, 'Members'),
                h('div', { class: 'choices' }, roles.map((r) => {
                    const on = (g.roles || []).includes(r);
                    return h('button', { type: 'button', key: r, class: 'choice' + (on ? ' on' : ''), 'aria-pressed': String(on), onclick: () => { g.roles = on ? g.roles.filter((x) => x !== r) : (g.roles || []).concat(r); touch(); } }, r);
                })),
                h('div', { class: 'form-row start' }, P.btn('Remove group', () => { gs.splice(i, 1); touch(); }, { sm: true, kind: 'danger-ghost', icon: 'trash' })))),
            h('div', { class: 'form-row start', key: 'add' }, P.btn('Add a group', () => { gs.push({ name: 'group' + (gs.length + 1), roles: [], shared_chars: 16384, push: 'urgent', auto: true }); touch(); }, { sm: true, icon: 'plus' })),
        ], { key: 'p-groups', icon: 'edit' });
    }

    function patternCopySheet(sh) {
        const go = async (e) => {
            e.preventDefault();
            if (!sh.name.trim()) return;
            sh.error = null;
            if (await savePattern(sh.name.trim())) M.act.closeSheet();
            else { sh.error = (C().editor && C().editor.errors.join('\n')) || 'not saved'; changed(); }
        };
        return {
            title: 'Save a copy', cls: 'small', body: h('form', { class: 'ap-form', onsubmit: go },
                h('p', { class: 'sheet-text' }, 'The pattern as it is in the editor, under a name of your own.'),
                h('input', { class: 'input', value: sh.name, 'aria-label': 'Name of the copy', autocapitalize: 'off', spellcheck: 'false', oninput: (e) => { sh.name = e.target.value; changed(); } }),
                sh.error ? h('div', { class: 'fld-err', role: 'alert' }, sh.error) : null,
                h('div', { class: 'sheet-actions' }, P.btn('Cancel', () => M.act.closeSheet(), { kind: 'ghost' }), P.btn('Save copy', null, { type: 'submit', kind: 'primary', disabled: !sh.name.trim() }))),
        };
    }

    // The sheets this file owns (app.js asks for the kinds it does not know).
    function sheet(sh) {
        if (sh.kind === 'add-provider') return addProviderSheet(sh);
        if (sh.kind === 'pattern-copy') return patternCopySheet(sh);
        return null;
    }

    // Command palette entries: every section, every pattern.
    function paletteItems() {
        const out = SECTIONS.map((x) => ({ key: 'set-' + x.id, label: x.id === 'models' || x.id === 'patterns' ? x.title : 'Settings: ' + x.title, icon: x.icon, run: () => M.act.nav('/settings/' + x.id) }));
        for (const p of (S().app && S().app.pattern_list) || []) out.push({ key: 'pat-' + p.name, label: 'Edit pattern ' + p.name, sub: p.description, icon: 'plan', run: () => M.act.nav('/patterns/' + encodeURIComponent(p.name)) });
        return out;
    }

    M.config = { settingsScreen, patternEditor, onRoute, title, sheet, paletteItems, SECTIONS };
})(window.Mantra = window.Mantra || {});
