// Mantra web UI — screens: Team, Run, Pulse, Inbox, Runs, Settings, More, Login, Connect.
'use strict';
(function (M) {
    const h = M.h, F = M.fmt, P = M.parts;
    const S = () => M.store.S;
    const icon = F.icon;

    function page(key, kids, cls) { return h('div', { class: 'page' + (cls ? ' ' + cls : ''), key }, h('div', { class: 'page-inner' }, kids)); }
    function card(title, kids, opts) {
        opts = opts || {};
        return h('section', { class: 'card sec' + (opts.cls ? ' ' + opts.cls : ''), key: opts.key || title },
            title ? h('div', { class: 'sec-head' }, opts.icon ? icon(opts.icon) : null, h('h2', null, title), opts.right || null) : null,
            kids);
    }

    // ── Team ─────────────────────────────────────────────────────────────────────────────────────
    function startRunCard() {
        const s = S();
        const app = s.app || {};
        const pats = app.patterns || [];
        const pat = s.ui.runPattern || app.pattern_name || pats[0] || '';
        const goal = s.ui.runGoal || '';
        const start = () => {
            const g = (S().ui.runGoal || '').trim();
            if (!g) return;
            M.act.cmd('start_run', { goal: g, pattern: pat || undefined }, { busyKey: 'start-run', ok: 'Run started' })
                .then(() => { S().ui.runGoal = ''; M.act.nav('/run'); }, () => { });
        };
        return card('Start a run', [
            h('p', { class: 'sec-sub' }, 'A planner breaks the goal into phases, workers build them in parallel, a gate checks every phase.'),
            h('textarea', { class: 'input goal', rows: 3, value: goal, placeholder: 'What should the team build? e.g. “Add OAuth login with GitHub, with tests”', 'aria-label': 'Run goal', oninput: (e) => { s.ui.runGoal = e.target.value; M.act.changed(); }, onkeydown: (e) => { if (e.key === 'Enter' && (e.ctrlKey || e.metaKey)) { e.preventDefault(); start(); } } }),
            h('div', { class: 'form-row' },
                pats.length ? h('label', { class: 'select-wrap' }, h('span', { class: 'select-label' }, 'Pattern'),
                    h('select', { class: 'select', value: pat, onchange: (e) => { s.ui.runPattern = e.target.value; M.act.changed(); } }, pats.map((p) => h('option', { value: p, key: p, selected: p === pat }, p)))) : h('span'),
                P.btn('Start run', start, { kind: 'primary', icon: 'play', busyKey: 'start-run', disabled: !goal.trim() || s.conn.state !== 'open' })),
        ], { icon: 'run', key: 'startrun' });
    }

    function unfinishedCard() {
        const s = S();
        const ids = (s.app && s.app.unfinished_runs) || [];
        if (!ids.length) return null;
        return card('Unfinished runs', [
            h('div', { class: 'list' }, ids.map((id) => h('div', { class: 'list-row', key: id },
                h('code', { class: 'mono' }, id),
                P.btn('Resume', () => M.act.cmd('run_resume', { id }, { busyKey: 'resume-' + id, ok: 'Run resumed' }).then(() => M.act.nav('/run'), () => { }), { sm: true, busyKey: 'resume-' + id, icon: 'play' })))),
            h('button', { type: 'button', class: 'link sm', onclick: () => M.act.nav('/runs') }, 'All runs →'),
        ], { icon: 'runs', key: 'unfinished' });
    }

    function teamScreen() {
        const s = S();
        if (!s.synced) return page('team', [P.loading(s.conn.state === 'open' ? 'Syncing…' : 'Connecting to Mantra…')]);
        const app = s.app || {};
        const kids = [];
        if (app.sandbox_warning) kids.push(h('div', { class: 'notice-card', key: 'sandbox' }, icon('alert'), h('span', null, app.sandbox_warning)));
        kids.push(P.bands());
        if (s.run) kids.push(P.runCard());
        const agents = M.store.agentList();
        if (!agents.length) kids.push(P.empty('team', 'No agents yet', s.run ? 'The run is starting its first agent…' : 'Start a Solo session or a run.', [P.btn('New Solo session', () => M.act.cmd('new_solo', {}, { ok: 'New session' }).then((d) => d && M.act.nav('/agent/' + d.agent)), { kind: 'primary' })]));
        else kids.push(h('div', { class: 'team', key: 'team' }, P.teamList()));
        if (!s.run) { kids.push(startRunCard()); kids.push(unfinishedCard()); }
        return page('team', kids);
    }

    // ── Run ──────────────────────────────────────────────────────────────────────────────────────
    function workerFor(taskId) {
        const run = S().run;
        return run && (run.workers || []).find((w) => w.task_id === taskId);
    }

    function planView(plan) {
        const s = S();
        const md = s.ui.planMd;
        const head = h('div', { class: 'plan-head' },
            h('div', null, h('h2', null, plan.title || 'Plan'), plan.summary ? h('p', { class: 'sec-sub' }, plan.summary) : null),
            P.segmented(md ? 'md' : 'st', [['st', 'Tasks'], ['md', 'Markdown']], (v) => { s.ui.planMd = v === 'md'; M.act.changed(); }, 'Plan view'));
        if (md) return card(null, [head, h('div', { class: 'md plan-md' }, F.markdown(plan.markdown || ''))], { key: 'plan', cls: 'plan' });
        const run = s.run;
        const curPh = run && run.stage && run.stage.kind === 'phase' ? run.stage.phase : run && run.stage && (run.stage.kind === 'finale' || run.stage.kind === 'done') ? 1e9 : -1;
        return card(null, [
            head,
            h('ol', { class: 'phases' }, (plan.phases || []).map((ph, i) => h('li', { key: ph.id || i, class: 'phase' + (i < curPh ? ' done' : i === curPh ? ' cur' : '') },
                h('div', { class: 'phase-head' }, h('span', { class: 'phase-n' }, i < curPh ? '✓' : String(i + 1)), h('b', null, ph.name), ph.goal ? h('span', { class: 'phase-goal' }, ph.goal) : null),
                h('div', { class: 'tasks' }, (ph.tasks || []).map((t) => {
                    const w = workerFor(t.id);
                    const st = w ? P.wstate(w) : null;
                    const ag = w && w.agent !== undefined && w.agent !== null ? M.store.agent(w.agent) : null;
                    return h(ag ? 'button' : 'div', { type: ag ? 'button' : null, key: t.id, class: 'task' + (ag ? ' link-row' : ''), onclick: ag ? () => M.act.nav('/agent/' + ag.id) : null },
                        h('span', { class: 'task-id mono' }, t.id),
                        h('span', { class: 'task-title' }, t.title),
                        t.role ? h('span', { class: 'mini-chip' }, t.role) : null,
                        st ? h('span', { class: 'pill sm tone-' + st.tone }, h('i', { class: 'dot' }), st.text) : null);
                })),
                ph.gate && (ph.gate.checks || []).length ? h('div', { class: 'gate-line' }, h('span', { class: 'glyph', style: { color: 'var(--green)' } }, '◎'), ' gate: ', (ph.gate.checks || []).map((c, j) => h('code', { key: j }, c))) : null))),
            plan.final_checks && plan.final_checks.length ? h('div', { class: 'gate-line' }, 'final checks: ', plan.final_checks.map((c, j) => h('code', { key: j }, c))) : null,
        ], { key: 'plan', cls: 'plan' });
    }

    function runScreen() {
        const s = S();
        if (!s.synced) return page('run', [P.loading()]);
        const run = s.run;
        if (!run) {
            return page('run', [P.empty('run', 'No run open', 'Runs plan a goal into phases and build it with a team of agents.', [
                P.btn('Start a run', () => M.act.nav('/'), { kind: 'primary', icon: 'play' }),
                P.btn('Past runs', () => M.act.nav('/runs'), { icon: 'runs' }),
            ])]);
        }
        const st = run.stage || {};
        const plan = s.plan;
        const kids = [];
        kids.push(h('div', { class: 'run-hero', key: 'hero' },
            h('div', { class: 'run-hero-top' },
                h('span', { class: 'mono dim' }, run.id), h('span', { class: 'dim' }, ' · ' + run.pattern),
                h('span', { class: 'spacer' }),
                run.landable ? P.btn('Land', () => M.act.sheet({ kind: 'confirm', title: 'Land this run?', body: 'Merges ' + run.branch + ' into your current branch.', confirm: 'Land', run: () => M.act.cmd('land', {}, { ok: 'Landing…' }) }), { kind: 'primary', icon: 'land', sm: true }) : null,
                st.kind !== 'done' && st.kind !== 'failed' ? P.btn(run.halted ? 'Resume' : 'Pause', () => M.act.cmd('pause_resume', {}, { busyKey: 'resume' }), { sm: true, icon: run.halted ? 'play' : 'pause', busyKey: 'resume', kind: run.halted ? 'primary' : null }) : null),
            h('h1', { class: 'run-title' }, run.brief),
            P.stageRail(run, plan, true),
            h('div', { class: 'run-stats' },
                stat('stage', st.label), stat('elapsed', F.durShort(run.elapsed_ms)), stat('tokens', F.tokens(run.total_tokens)), stat('busy', String(run.busy_count || 0)),
                run.branch ? stat('branch', run.branch) : null),
            st.error ? h('div', { class: 'errbox' }, icon('alert'), h('span', null, st.error)) : null));
        kids.push(P.bands());
        if (st.kind === 'review' && plan) {
            kids.push(card('Review the plan', [
                h('p', { class: 'sec-sub' }, 'Approve to start phase 1, or tell the planner what to change.'),
                h('div', { class: 'band-actions' }, P.btn('Approve plan', () => M.act.cmd('plan_approve', {}, { busyKey: 'approve-plan', ok: 'Plan approved' }), { kind: 'primary', icon: 'check', busyKey: 'approve-plan' })),
                P.feedbackField('plan-feedback', 'Feedback for the planner…', 'Send feedback'),
            ], { key: 'review', cls: 'review-card', icon: 'plan' }));
        }
        if (plan) kids.push(planView(plan));
        else if (st.kind === 'setup' || st.kind === 'planning') kids.push(card(null, [P.loading('The planner is exploring the project and writing a plan…')], { key: 'noplan' }));
        if ((run.checks || []).length) {
            kids.push(card('Gate checks', h('div', { class: 'list' }, run.checks.map((c, i) => {
                const k = 'chk' + i;
                const open = s.ui.expanded[k];
                return h('div', { class: 'check' + (c.ok ? ' ok' : ' bad'), key: k },
                    h('button', { type: 'button', class: 'check-head', onclick: () => { s.ui.expanded[k] = !open; M.act.changed(); } },
                        h('span', { class: 'check-ico' }, c.ok ? icon('check') : icon('close')), h('code', null, c.cmd), c.output_tail ? icon(open ? 'up' : 'down', 'chev') : null),
                    open && c.output_tail ? h('pre', { class: 'cmd-out' }, c.output_tail) : null);
            })), { key: 'checks', icon: 'check' }));
        }
        if ((run.conflicts || []).length) kids.push(card('Merge conflicts', h('ul', { class: 'plain' }, run.conflicts.map((c) => h('li', { key: c }, h('code', null, c)))), { key: 'conf', icon: 'alert', cls: 'warn' }));
        if ((run.alerts || []).length) kids.push(card('Alerts', h('ul', { class: 'plain' }, run.alerts.map((c, i) => h('li', { key: i }, c))), { key: 'alerts', icon: 'alert', cls: 'warn' }));
        if ((run.history || []).length) {
            kids.push(card('Phase history', h('div', { class: 'list' }, run.history.map((r, i) => h('div', { class: 'hist', key: i },
                h('div', { class: 'hist-top' }, h('b', null, r.phase), h('span', { class: 'dim' }, r.tasks_done + '/' + r.tasks_total + ' tasks' + (r.rounds ? ' · ' + F.plural(r.rounds, 'gate round') : ''))),
                r.summary ? h('div', { class: 'hist-sum' }, F.trunc(r.summary, 400)) : null))), { key: 'hist', icon: 'runs' }));
        }
        return page('run', kids);
    }
    function stat(label, value) { return value ? h('div', { class: 'stat', key: label }, h('span', { class: 'stat-v' }, value), h('span', { class: 'stat-l' }, label)) : null; }

    // ── Pulse ────────────────────────────────────────────────────────────────────────────────────
    const ALERT = { red: 1, amber: 1, rose: 1 };
    function pulseKind(p) {
        if (/\bby you\b|plan feedback|plan approved|run resumed|^you\b/i.test(p.text) || p.glyph === '✎') return 'you';
        if (ALERT[p.color]) return 'alerts';
        return 'agents';
    }
    function pulseList(limit, filter) {
        const s = S();
        let list = s.pulse;
        if (filter && filter !== 'all') list = list.filter((p) => pulseKind(p) === filter);
        list = list.slice(-limit).reverse();
        if (!list.length) return null;
        return h('ol', { class: 'pulse', key: 'pl' }, list.map((p) => h('li', { key: 'p' + p.n, class: 'pulse-row' + (ALERT[p.color] ? ' alert' : '') },
            h('time', { class: 'pulse-t', title: new Date(p.at).toLocaleString() }, p.t || F.clock(p.at)),
            h('span', { class: 'pulse-g', style: { color: F.colorVar(p.color) } }, p.glyph),
            h('span', { class: 'pulse-x' }, p.text))));
    }
    function pulseScreen() {
        const s = S();
        if (!s.synced) return page('pulse', [P.loading()]);
        const f = s.ui.pulseFilter;
        const list = pulseList(500, f);
        return page('pulse', [
            h('div', { class: 'filters', key: 'f' }, P.segmented(f, [['all', 'All'], ['alerts', 'Alerts'], ['you', 'You'], ['agents', 'Agents']], (v) => { s.ui.pulseFilter = v; M.act.changed(); }, 'Filter')),
            list ? card(null, list, { key: 'pl', cls: 'flush' }) : P.empty('pulse', s.run ? 'Nothing here yet' : 'No run, no pulse', s.run ? (f === 'all' ? 'The run journal appears here as it happens.' : 'No ' + f + ' lines so far.') : 'The pulse is the live journal of a run: spawns, merges, gates, halts.'),
        ]);
    }

    // ── Inbox ────────────────────────────────────────────────────────────────────────────────────
    const NOTE_ICON = { halt: '⛔', question: '?', approval: '⚑', review: '☰', done: '✦', failed: '✗', turn: '◆', info: 'ℹ' };
    function inboxScreen() {
        const s = S();
        if (!s.synced) return page('inbox', [P.loading()]);
        const kids = [];
        const run = s.run;
        if (run) kids.push(P.bands());
        if (s.approvals.length) kids.push(h('div', { class: 'approvals', key: 'aps' }, h('h3', { class: 'group-title' }, 'Needs your approval'), s.approvals.map((ap) => P.approvalCard(ap, { showAgent: true }))));
        if (run && (run.alerts || []).length) kids.push(card('Run alerts', h('ul', { class: 'plain' }, run.alerts.map((a, i) => h('li', { key: i }, a))), { key: 'alerts', icon: 'alert', cls: 'warn' }));
        const nothing = !s.approvals.length && !(run && (run.halted || run.question || run.want_review));
        if (nothing) kids.push(P.empty('check', 'All clear', 'Approvals, questions and halts show up here — and on your phone, if you turn on notifications.', [P.btn('Notification settings', () => M.act.nav('/settings'), { icon: 'bell' })]));
        if (s.notes.length) {
            kids.push(card('Recent', [
                h('ol', { class: 'notes' }, s.notes.slice(0, 60).map((n, i) => h('li', { key: i + ':' + n.at, class: 'note-row k-' + n.kind },
                    h('span', { class: 'note-ico' }, NOTE_ICON[n.kind] || '·'),
                    h('span', { class: 'note-x' }, n.text.replace(/^Mantra(:| needs you:)\s*/, '')),
                    h('time', { class: 'note-t' }, F.ago(n.at))))),
            ], { key: 'notes', icon: 'bell', right: P.btn('Clear', () => { s.notes.length = 0; M.store.save('notes', []); M.act.changed(); }, { sm: true, kind: 'ghost' }) }));
        }
        return page('inbox', kids);
    }

    // ── Runs ─────────────────────────────────────────────────────────────────────────────────────
    async function loadRuns() {
        const s = S();
        s.ui.runs = Object.assign({}, s.ui.runs || {}, { loading: true, error: null });
        M.act.changed();
        try {
            const d = await M.act.request('runs_list', {});
            const list = Array.isArray(d) ? d : (d && (d.runs || d.list)) || [];
            s.ui.runs = { list, others: (d && d.others) || 0, loading: false, at: Date.now() };
        } catch (e) {
            s.ui.runs = { list: (s.ui.runs && s.ui.runs.list) || null, loading: false, error: e.message };
        }
        M.act.changed();
    }
    function runsScreen() {
        const s = S();
        const r = s.ui.runs;
        if (!r || (r.loading && !r.list)) return page('runs', [P.loading('Loading runs…')]);
        const kids = [];
        if (r.error) kids.push(P.errorBox('Could not load runs: ' + r.error, loadRuns));
        const list = r.list || [];
        if (!list.length && !r.error) kids.push(P.empty('runs', 'No runs in this project yet', 'Start one from the Team screen.', [P.btn('Start a run', () => M.act.nav('/'), { kind: 'primary', icon: 'play' })]));
        if (list.length) {
            kids.push(h('div', { class: 'card flush', key: 'rl' }, list.map((x) => h('div', { class: 'run-row', key: x.id },
                h('div', { class: 'run-row-main' },
                    h('div', { class: 'run-row-top' },
                        h('code', { class: 'mono' }, x.id),
                        x.open ? h('span', { class: 'pill sm tone-busy' }, h('i', { class: 'dot' }), 'open') : x.unfinished ? h('span', { class: 'pill sm tone-wait' }, h('i', { class: 'dot' }), 'unfinished') : null,
                        h('span', { class: 'dim' }, x.stage), h('span', { class: 'spacer' }), h('span', { class: 'dim sm' }, x.ago || (x.updated_at ? F.ago(x.updated_at * 1000) : ''))),
                    h('div', { class: 'run-row-brief' }, x.brief || h('span', { class: 'dim' }, '(no brief)')),
                    x.error ? h('div', { class: 'run-row-err' }, x.error) : null),
                h('div', { class: 'run-row-acts' },
                    !x.open && x.unfinished ? P.btn('Resume', () => M.act.cmd('run_resume', { id: x.id }, { busyKey: 'resume-' + x.id, ok: 'Run resumed' }).then(() => M.act.nav('/run'), () => { }), { sm: true, icon: 'play', busyKey: 'resume-' + x.id, disabled: !!s.run }) : null,
                    !x.open ? P.iconBtn('trash', () => M.act.sheet({ kind: 'confirm', danger: true, title: 'Delete run ' + x.id + '?', body: 'Removes its state and journal from ~/.mantra. Branches and worktrees in your repo are not touched.', confirm: 'Delete', run: () => M.act.cmd('run_delete', { id: x.id }, { ok: 'Deleted' }).then(loadRuns) }), 'Delete run') : null)))));
        }
        if (r.others) kids.push(h('p', { class: 'foot-note', key: 'others' }, F.plural(r.others, 'run') + ' in other projects — use ', h('code', null, 'mantra runs'), ' in a terminal.'));
        return page('runs', kids);
    }

    // ── Settings ─────────────────────────────────────────────────────────────────────────────────
    const PREF_LABELS = [
        ['halt', 'Halts', 'the run stopped and needs you'], ['question', 'Questions', 'an agent asks something'], ['approval', 'Approvals', 'a command or edit needs your OK'],
        ['review', 'Plan ready', 'a plan is waiting for review'], ['done', 'Run complete', 'finished or failed runs'], ['turn', 'Solo turns', 'every finished Solo reply'],
    ];
    function notificationsCard() {
        const s = S();
        const p = M.push.state();
        const kids = [];
        if (!p.serverPush) kids.push(h('p', { class: 'sec-sub' }, 'Web Push is turned off on this Mantra (', h('code', null, '[web] push = false'), ').'));
        else if (!p.secure) kids.push(h('p', { class: 'sec-sub' }, 'Notifications need HTTPS. Start Mantra with ', h('code', null, '--web-tls'), ' (and trust its certificate) or use ', h('code', null, '--remote'), '.'));
        else if (p.iosNeedsInstall) kids.push(h('p', { class: 'sec-sub' }, 'On iPhone and iPad, notifications work once Mantra is on your Home Screen: tap ', h('b', null, 'Share › Add to Home Screen'), ', open it from there, then come back here.'));
        else if (!p.supported) kids.push(h('p', { class: 'sec-sub' }, 'This browser doesn’t support Web Push.'));
        else {
            kids.push(P.toggle(p.subscribed, (on) => on ? M.push.enable() : M.push.disable(), 'Notify this device', p.permission === 'denied' ? 'Blocked in the browser’s site settings — allow notifications there first' : p.subscribed ? 'On — ' + (p.device || 'this device') : 'Get halts, questions and approvals even when Mantra isn’t open', { disabled: p.permission === 'denied' || p.busy, key: 'push-on' }));
            if (p.subscribed) {
                kids.push(h('div', { class: 'prefs', key: 'prefs' }, PREF_LABELS.map(([k, label, sub]) => P.toggle(p.prefs[k] !== false, (on) => M.push.setPref(k, on), label, sub, { key: 'pref-' + k }))));
                kids.push(h('div', { class: 'form-row', key: 'test' }, P.btn('Send a test', () => M.push.test(), { icon: 'bell', busyKey: 'push-test', sm: true })));
            }
        }
        return card('Notifications', kids, { key: 'notif', icon: 'bell' });
    }

    function appearanceCard() {
        const s = S();
        return card('Appearance', [
            h('div', { class: 'set-row', key: 'theme' }, h('span', null, 'Theme'), P.segmented(s.ui.theme, [['system', 'System'], ['dark', 'Dark'], ['light', 'Light']], (v) => { M.store.setPref('theme', v); M.act.applyPrefs(); }, 'Theme')),
            h('div', { class: 'set-row', key: 'motion' }, h('span', null, 'Motion'), P.segmented(s.ui.motion, [['system', 'System'], ['reduce', 'Reduced']], (v) => { M.store.setPref('motion', v); M.act.applyPrefs(); }, 'Motion')),
            P.toggle(s.ui.verbose, (on) => M.store.setPref('verbose', on), 'Verbose transcript', 'expand reasoning and command output', { key: 'verbose' }),
        ], { key: 'appearance', icon: 'sun' });
    }

    function drawQr(canvas, rows) {
        if (!canvas || !rows || !rows.length) return;
        const n = rows.length, quiet = 4, total = n + quiet * 2;
        const px = Math.max(2, Math.floor(232 / total));
        const size = px * total;
        const dpr = Math.min(3, window.devicePixelRatio || 1);
        canvas.width = size * dpr; canvas.height = size * dpr;
        canvas.style.width = size + 'px'; canvas.style.height = size + 'px';
        const ctx = canvas.getContext('2d');
        ctx.scale(dpr, dpr);
        ctx.fillStyle = '#fff'; ctx.fillRect(0, 0, size, size);
        ctx.fillStyle = '#0F1116';
        for (let y = 0; y < n; y++) for (let x = 0; x < rows[y].length; x++) if (rows[y][x] === '1') ctx.fillRect((x + quiet) * px, (y + quiet) * px, px, px);
    }

    function remoteCard() {
        const s = S();
        const r = s.remote;
        if (s.conn.mode === 'relay') return null;
        if (!r || !r.enabled) {
            return card('Remote access', [
                h('p', { class: 'sec-sub' }, 'Reach this session from anywhere — end-to-end encrypted through a relay that only ever sees ciphertext. Start Mantra with ', h('code', null, 'mantra --remote'), '.'),
            ], { key: 'remote', icon: 'globe' });
        }
        const reveal = s.ui.revealPw;
        const qrKey = (r.qr || []).join('').length + ':' + (r.link || '');
        return card('Remote access', [
            h('div', { class: 'remote-status', key: 'st' },
                h('span', { class: 'pill tone-' + (r.connected ? 'busy' : 'wait') }, h('i', { class: 'dot' }), r.connected ? 'connected to ' + r.relay.replace(/^wss?:\/\//, '') : 'reconnecting to ' + r.relay.replace(/^wss?:\/\//, '')),
                h('span', { class: 'dim sm' }, F.plural(r.clients || 0, 'device') + ' connected')),
            r.last_error && !r.connected ? h('div', { class: 'errbox', key: 'err' }, icon('alert'), h('span', null, r.last_error)) : null,
            h('div', { class: 'remote-grid', key: 'grid' },
                r.qr && r.qr.length ? h('div', { class: 'qr', key: 'qr' }, h('canvas', { key: qrKey, raw: true, ref: (el) => drawQr(el, r.qr), 'aria-label': 'QR code for the session link', role: 'img' })) : null,
                h('div', { class: 'remote-fields' },
                    field('Link', r.link, { copy: true, mono: true }),
                    field('Code', r.code, { copy: true, mono: true }),
                    r.password ? field('Password', reveal ? r.password : '••••••••', { copy: r.password, mono: true, extra: P.iconBtn(reveal ? 'eyeoff' : 'eye', () => { s.ui.revealPw = !reveal; M.act.changed(); }, reveal ? 'Hide' : 'Show') }) : null)),
            h('p', { class: 'sec-sub', key: 'how' }, 'Scan the QR code or open the link on your phone. Or go to ', h('b', null, (r.relay || '').replace(/^ws/, 'http')), ' and type the code and password.'),
            h('div', { class: 'form-row', key: 'rot' }, P.btn('Rotate link & code', () => M.act.sheet({ kind: 'confirm', danger: true, title: 'Rotate the remote link?', body: 'Every device using the current link, code or remembered session is disconnected and has to use the new one.', confirm: 'Rotate', run: () => M.act.cmd('remote_rotate', {}, { ok: 'New link ready' }) }), { icon: 'refresh', sm: true })),
        ], { key: 'remote', icon: 'globe' });
    }
    function field(label, value, opts) {
        opts = opts || {};
        return h('div', { class: 'field', key: label },
            h('span', { class: 'field-l' }, label),
            h('span', { class: 'field-v' + (opts.mono ? ' mono' : '') }, value || '—'),
            opts.extra || null,
            opts.copy && value ? P.iconBtn('copy', () => M.act.copy(typeof opts.copy === 'string' ? opts.copy : value), 'Copy ' + label.toLowerCase()) : null);
    }

    const CERT_STEPS = [
        ['iPhone / iPad', ['Open ', ['a', '/cert.pem'], ' in Safari → “Profile Downloaded”.', 'Settings › Profile Downloaded › Install.', 'Settings › General › About › Certificate Trust Settings › turn on full trust for “mantra on …”.']],
        ['Android', ['Download ', ['a', '/cert.pem'], '.', 'Settings › Security › Encryption & credentials › Install a certificate › CA certificate.', 'Chrome trusts it after a restart. Some other browsers ignore user certificates for service workers — use Chrome.']],
        ['macOS', ['Download ', ['a', '/cert.pem'], ' and open it in Keychain Access (System keychain).', 'Double-click “mantra on …” › Trust › When using this certificate: Always Trust.']],
        ['Windows', ['Download ', ['a', '/cert.pem'], ', open it › Install Certificate › Local Machine.', 'Place it in “Trusted Root Certification Authorities”.']],
        ['Linux (Chrome)', ['chrome://settings/certificates › Authorities › Import › ', ['a', '/cert.pem'], ' › trust for websites.']],
        ['Firefox', ['about:preferences#privacy › Certificates › View Certificates › Authorities › Import ', ['a', '/cert.pem'], '.']],
    ];
    function certCard() {
        const s = S();
        if (s.conn.mode !== 'local' || !s.ui.certAvailable) return null;
        return card('Certificate', [
            h('p', { class: 'sec-sub' }, 'Mantra serves HTTPS with its own self-signed certificate. Trust it once per device so the browser stops warning and notifications work.'),
            h('div', { class: 'form-row' }, h('a', { class: 'btn sm', href: '/cert.pem', download: 'mantra-cert.pem' }, icon('download'), h('span', null, 'Download certificate'))),
            h('div', { class: 'accordion' }, CERT_STEPS.map(([os, steps]) => h('details', { key: os },
                h('summary', null, os),
                h('ol', null, (Array.isArray(steps[0]) || typeof steps[0] === 'string' ? groupSteps(steps) : []).map((st, i) => h('li', { key: i }, st)))))),
        ], { key: 'cert', icon: 'cert' });
    }
    // Steps are strings, except an ['a', href] pair inside the first sentence(s) for a link.
    function groupSteps(steps) {
        const out = [];
        let cur = [];
        for (const x of steps) {
            if (Array.isArray(x)) cur.push(h('a', { href: x[1], download: 'mantra-cert.pem' }, x[1]));
            else if (cur.length && !/[.”]$/.test(typeof cur[cur.length - 1] === 'string' ? cur[cur.length - 1] : '')) cur.push(x);
            else { if (cur.length) out.push(cur); cur = [x]; }
            if (typeof x === 'string' && /[.”]$/.test(x)) { out.push(cur); cur = []; }
        }
        if (cur.length) out.push(cur);
        return out;
    }

    function aboutCard() {
        const s = S();
        const app = s.app || {};
        const hl = s.hello || {};
        const rows = [
            ['Version', (app.version || hl.version || '?') + ' · protocol ' + (app.protocol || hl.protocol || 1)],
            ['Connection', s.conn.mode === 'relay' ? 'remote (end-to-end encrypted)' : 'local' + (app.tls || hl.tls ? ' · HTTPS' : '')],
            ['Project', app.project],
            ['Branch', app.branch],
            app.listen ? ['Listening', app.listen] : null,
            app.headless ? ['Terminal', 'headless (no TUI)'] : null,
            ['Log', '~/.mantra/logs/mantra.log'],
        ].filter((x) => x && x[1]);
        return card('About', h('dl', { class: 'kv' }, rows.map(([k, v]) => [h('dt', { key: 'k' + k }, k), h('dd', { key: 'v' + k, class: k === 'Project' || k === 'Log' ? 'mono' : null }, v)])), { key: 'about', icon: 'info' });
    }

    function sessionCard() {
        const s = S();
        if (s.conn.mode === 'relay') {
            return card('This device', [
                h('p', { class: 'sec-sub' }, s.ui.remembered ? 'This session is remembered on this device — it reconnects without the password.' : 'The key for this session lives only in this tab.'),
                h('div', { class: 'form-row' },
                    s.ui.remembered ? P.btn('Forget on this device', () => M.relay.forget(), { icon: 'trash', sm: true }) : P.btn('Remember on this device', () => M.relay.remember(), { icon: 'lock', sm: true }),
                    P.btn('Disconnect', () => M.relay.disconnect(), { icon: 'logout', sm: true, kind: 'ghost' })),
            ], { key: 'session', icon: 'lock' });
        }
        if (!s.ui.needsLogin) return null;
        return card('Session', [h('div', { class: 'form-row' }, P.btn('Log out', () => M.act.logout(), { icon: 'logout', sm: true }))], { key: 'session', icon: 'lock' });
    }

    function settingsScreen() {
        return page('settings', [notificationsCard(), appearanceCard(), remoteCard(), certCard(), sessionCard(), aboutCard()], 'narrow');
    }

    // ── More (phone) ─────────────────────────────────────────────────────────────────────────────
    function moreScreen() {
        const s = S();
        const nav = (path, ico, label, sub, badge) => h('button', { type: 'button', class: 'row nav-row', key: path, onclick: () => M.act.nav(path) },
            h('span', { class: 'nav-ico' }, icon(ico)), h('span', { class: 'row-main' }, h('span', { class: 'row-title' }, label), sub ? h('span', { class: 'row-sub' }, sub) : null),
            badge ? h('span', { class: 'unread' }, String(badge)) : null, icon('chevron', 'chev'));
        const run = s.run;
        return page('more', [
            h('div', { class: 'card flush', key: 'nav' },
                nav('/pulse', 'pulse', 'Pulse', run ? (s.pulse.length ? F.trunc(s.pulse[s.pulse.length - 1].text, 60) : 'the run journal') : 'the run journal'),
                nav('/runs', 'runs', 'Runs', 'past and unfinished runs in this project'),
                nav('/settings', 'settings', 'Settings', 'notifications, appearance, remote access')),
            h('div', { class: 'card sec', key: 'theme' }, h('div', { class: 'set-row' }, h('span', null, 'Theme'), P.segmented(s.ui.theme, [['system', 'Auto'], ['dark', 'Dark'], ['light', 'Light']], (v) => { M.store.setPref('theme', v); M.act.applyPrefs(); }, 'Theme'))),
            h('p', { class: 'foot-note', key: 'ver' }, 'Mantra ' + ((s.app && s.app.version) || '') + ' · ' + (s.conn.mode === 'relay' ? 'remote' : 'local')),
        ]);
    }

    // ── Login (local) ────────────────────────────────────────────────────────────────────────────
    function brand() {
        return h('div', { class: 'brand-mark', key: 'brand' }, h('img', { src: '/icons/icon.svg', alt: '', width: 72, height: 72 }));
    }
    function loginScreen() {
        const s = S();
        const L = s.ui.login || (s.ui.login = { pw: '', error: null, busy: false });
        const submit = async (e) => {
            e.preventDefault();
            if (!L.pw || L.busy) return;
            L.busy = true; L.error = null; M.act.changed();
            const r = await M.act.login(L.pw);
            L.busy = false;
            if (!r.ok) { L.error = r.error; L.pw = ''; }
            M.act.changed();
        };
        return h('div', { class: 'auth', key: 'login' },
            h('form', { class: 'auth-card', onsubmit: submit },
                brand(),
                h('h1', null, 'Mantra on ', h('span', { class: 'accent' }, location.hostname || 'this machine')),
                h('p', { class: 'sec-sub' }, 'Enter the web password (', h('code', null, '--web-password'), ' or ', h('code', null, 'MANTRA_WEB_PASSWORD'), ').'),
                h('input', { class: 'input big', type: 'password', autocomplete: 'current-password', placeholder: 'Password', 'aria-label': 'Password', value: L.pw, autofocus: true, oninput: (e) => { L.pw = e.target.value; if (L.error) L.error = null; M.act.changed(); } }),
                L.error ? h('div', { class: 'auth-err', role: 'alert' }, L.error) : null,
                h('button', { type: 'submit', class: 'btn primary big' + (L.busy ? ' is-busy' : ''), disabled: !L.pw || L.busy || null }, h('span', null, L.busy ? 'Signing in…' : 'Sign in'), L.busy ? h('span', { class: 'spin' }) : null)));
    }

    // ── Connect (relay) ──────────────────────────────────────────────────────────────────────────
    function fmtCodeInput(v) {
        const raw = v.toLowerCase().replace(/[^a-z2-7]/g, '').slice(0, 26);
        return M.relay ? MantraCrypto.formatCode(raw) : raw;
    }
    function connectScreen() {
        const s = S();
        const C = s.ui.connect || (s.ui.connect = { code: '', pw: '', remember: false, error: null, step: null });
        const R = M.relay;
        const needPwOnly = !!R.pendingSid;
        const submit = async (e) => {
            e.preventDefault();
            if (C.step) return;
            await R.connectWith(needPwOnly ? R.pendingSid : C.code, C.pw, C.remember);
        };
        const sid = needPwOnly ? R.pendingSid : MantraCrypto.normalizeCode(C.code);
        const busy = !!C.step;
        const saved = s.ui.devices || [];
        return h('div', { class: 'auth', key: 'connect' },
            h('div', { class: 'auth-col' },
                h('form', { class: 'auth-card', onsubmit: submit },
                    brand(),
                    h('h1', null, needPwOnly ? 'Unlock this session' : 'Connect to Mantra'),
                    h('p', { class: 'sec-sub' }, needPwOnly
                        ? ['Session ', h('code', null, MantraCrypto.formatCode(R.pendingSid)), '. Enter its password.']
                        : ['Type the code and password shown by ', h('code', null, 'mantra --remote'), ' (or its ', h('code', null, '/remote'), ' screen).']),
                    needPwOnly ? null : h('input', {
                        class: 'input big mono', placeholder: 'xxxx-xxxx-xxxx-xxxx-xxxx-xxxxxx', 'aria-label': 'Session code', autocomplete: 'off', autocapitalize: 'none', spellcheck: 'false', inputmode: 'text', value: C.code, disabled: busy || null,
                        oninput: (e) => { C.code = fmtCodeInput(e.target.value); C.error = null; M.act.changed(); },
                    }),
                    h('input', { class: 'input big', type: 'password', placeholder: 'Password', 'aria-label': 'Password', autocomplete: 'current-password', value: C.pw, disabled: busy || null, oninput: (e) => { C.pw = e.target.value; C.error = null; M.act.changed(); } }),
                    P.toggle(C.remember, (on) => { C.remember = on; M.act.changed(); }, 'Remember this device', 'reconnect later without the password (the key is stored non-extractable in this browser)', { key: 'rem', disabled: busy }),
                    C.error ? h('div', { class: 'auth-err', role: 'alert' }, C.error) : null,
                    busy ? h('div', { class: 'auth-step' }, h('span', { class: 'spin' }), C.step) : null,
                    h('button', { type: 'submit', class: 'btn primary big', disabled: busy || !C.pw || !sid || null }, h('span', null, busy ? 'Connecting…' : 'Connect')),
                    h('p', { class: 'fine' }, icon('lock'), 'End-to-end encrypted: the relay only forwards ciphertext. Your password never leaves this device.')),
                saved.length && !needPwOnly ? h('div', { class: 'auth-card saved', key: 'saved' },
                    h('h2', null, 'On this device'),
                    saved.map((d) => h('div', { class: 'list-row', key: d.sid },
                        h('button', { type: 'button', class: 'saved-btn', onclick: () => R.connectSaved(d), disabled: busy || null },
                            h('b', null, d.label || 'Mantra session'), h('span', { class: 'mono dim sm' }, MantraCrypto.formatCode(d.sid)), h('span', { class: 'dim sm' }, 'saved ' + F.ago(d.saved_at))),
                        P.iconBtn('trash', () => R.forget(d.sid), 'Forget')))) : null));
    }

    M.screens = { teamScreen, runScreen, pulseScreen, pulseList, inboxScreen, runsScreen, loadRuns, settingsScreen, moreScreen, loginScreen, connectScreen, page, card };
})(window.Mantra = window.Mantra || {});
