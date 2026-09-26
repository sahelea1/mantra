// Mantra web UI — shared view pieces: glyphs, status, chips, gauges, the stage rail, the halt /
// question / review bands, agent + worker rows, approval cards, empty states, sheets.
// Views are plain functions returning vnodes; they read the store directly (M.store.S) and act
// through M.act (defined in app.js) at event time.
'use strict';
(function (M) {
    const h = M.h, F = M.fmt;
    const S = () => M.store.S;
    const icon = F.icon;

    // ── small atoms ──────────────────────────────────────────────────────────────────────────────
    function glyph(a, cls) {
        if (!a) return h('span', { class: 'glyph ' + (cls || '') }, '·');
        return h('span', { class: 'glyph ' + (cls || '') + (a.busy ? ' breathing' : ''), style: { color: F.colorVar(a.color) }, 'aria-hidden': 'true' }, a.glyph || '●');
    }

    function btn(label, onclick, opts) {
        opts = opts || {};
        const busy = opts.busyKey && S().ui.busy[opts.busyKey];
        return h('button', {
            type: opts.type || 'button',
            class: 'btn' + (opts.kind ? ' ' + opts.kind : '') + (opts.sm ? ' sm' : '') + (opts.cls ? ' ' + opts.cls : '') + (busy ? ' is-busy' : ''),
            onclick: opts.disabled || busy ? null : onclick,
            disabled: opts.disabled || busy || null,
            title: opts.title || null,
            'aria-label': opts.aria || (typeof label === 'string' ? null : opts.title) || null,
            key: opts.key,
        }, opts.icon ? icon(opts.icon) : null, label ? h('span', null, label) : null, busy ? h('span', { class: 'spin' }) : null);
    }
    function iconBtn(name, onclick, title, opts) {
        opts = opts || {};
        return h('button', { type: 'button', class: 'ibtn' + (opts.cls ? ' ' + opts.cls : ''), onclick: opts.disabled ? null : onclick, disabled: opts.disabled || null, title, 'aria-label': title, key: opts.key },
            icon(name), opts.badge ? h('span', { class: 'badge' }, opts.badge > 99 ? '99+' : String(opts.badge)) : null);
    }

    function effortBar(effort, efforts) {
        if (!effort || !efforts || !efforts.length) return null;
        const idx = efforts.indexOf(effort);
        return h('span', { class: 'effort', style: { color: F.effortColor(effort) }, title: 'effort ' + effort },
            h('span', { class: 'effort-label' }, effort),
            h('span', { class: 'effort-bar', 'aria-hidden': 'true' }, efforts.map((e, i) => h('i', { class: i <= idx ? 'on' : null }))));
    }

    function ctxColor(p) { return p >= 85 ? 'var(--red)' : p >= 65 ? 'var(--amber)' : 'var(--teal)'; }
    function ctxGauge(a, wide) {
        if (a.ctx_percent === undefined || a.ctx_percent === null) {
            return a.tokens_total ? h('span', { class: 'ctx none' }, F.tokens(a.tokens_total) + ' tok') : null;
        }
        const p = a.ctx_percent;
        return h('span', { class: 'ctx' + (wide ? ' wide' : ''), title: 'context ' + p + '% of ' + F.tokens(a.ctx_window) + (a.ctx_assumed ? ' (window assumed)' : '') + ' · compacts at ' + a.compact_percent + '%' },
            h('span', { class: 'ctx-track' },
                h('span', { class: 'ctx-fill', style: { width: p + '%', background: ctxColor(p) } }),
                a.compact_percent ? h('span', { class: 'ctx-tick', style: { left: a.compact_percent + '%' } }) : null),
            h('span', { class: 'ctx-num', style: { color: p >= 65 ? ctxColor(p) : null } }, 'ctx ' + p + '%'),
            a.tokens_total ? h('span', { class: 'ctx-tok' }, ' · ' + F.tokens(a.tokens_total) + ' tok') : null);
    }

    function modelChip(a, onclick) {
        return h(onclick ? 'button' : 'span', { class: 'chip model', onclick: onclick || null, type: onclick ? 'button' : null, title: a.model ? a.model + (a.provider_name ? ' via ' + a.provider_name : '') : null },
            h('b', null, a.model_alias || a.model || '?'),
            a.effort && a.efforts && a.efforts.length ? [h('span', { class: 'sep' }, '·'), effortBar(a.effort, a.efforts)] : null);
    }

    // ── agent status vocabulary ──────────────────────────────────────────────────────────────────
    // → {text, tone} where tone ∈ busy | wait | bad | idle | dim | done | review
    function status(a, now) {
        now = now || Date.now();
        if (!a) return { text: '', tone: 'dim' };
        const w = a.worker;
        if (a.approval_pending) return { text: 'needs approval', tone: 'wait' };
        if (a.waiting_answer) return { text: 'waiting for an answer', tone: 'wait' };
        switch (a.status) {
            case 'starting': return { text: 'starting…', tone: 'dim' };
            case 'retrying': return { text: 'retrying' + (a.status_detail ? ' — ' + F.trunc(a.status_detail, 60) : ''), tone: 'wait' };
            case 'failed': return { text: 'failed' + (a.status_detail ? ': ' + F.trunc(a.status_detail, 60) : ''), tone: 'bad' };
            case 'crashed': return { text: 'crashed' + (a.status_detail ? ': ' + F.trunc(a.status_detail, 60) : ''), tone: 'bad' };
            case 'stopped': return { text: a.stopped_by_user ? 'stopped by you' : 'stopped', tone: 'wait' };
        }
        if (a.busy) {
            const label = a.compacting ? 'compacting' : (a.activity || (a.awaiting_start ? 'starting' : 'thinking')).replace(/…$/, '');
            const el = a.turn_started_at ? ' · ' + F.durShort(now - a.turn_started_at) : '';
            const quiet = a.quiet_ms && a.quiet_ms >= 20000 ? ' · quiet ' + F.durShort(a.quiet_ms) : '';
            return { text: F.trunc(label, 40).toLowerCase() + el + quiet, tone: quiet ? 'wait' : 'busy', quiet: !!quiet };
        }
        if (w && w.state === 'done') return { text: 'done', tone: 'done' };
        if (w && w.state === 'failed') return { text: 'failed', tone: 'bad' };
        if (a.expected) return { text: 'expected to work · idle ' + F.durShort(now - a.last_event_at), tone: 'wait' };
        return { text: 'idle ' + F.durShort(now - (a.last_event_at || now)), tone: 'idle' };
    }
    function statusPill(a, now) {
        const st = status(a, now);
        return h('span', { class: 'pill tone-' + st.tone }, h('i', { class: 'dot' }), st.text);
    }

    const WSTATE = {
        queued: { text: 'queued', tone: 'dim' }, preparing: { text: 'preparing', tone: 'dim' }, running: { text: 'running', tone: 'busy' },
        retrying: { text: 'retrying', tone: 'wait' }, done: { text: 'done', tone: 'done' }, failed: { text: 'failed', tone: 'bad' }, cancelled: { text: 'cancelled', tone: 'dim' },
    };
    function wstate(w) { return WSTATE[w.state] || { text: w.state, tone: 'dim' }; }

    // Group agents the way the Stage screen does: leadership, the phase's workers, gate/finale.
    const LEAD = { planner: 0, manager: 1, orchestrator: 2, architect: 3 };
    function groups() {
        const s = S();
        const list = M.store.agentList();
        const lead = [], workers = [], gate = [], other = [], earlier = [];
        for (const a of list) {
            // Agents of finished phases (an earlier orchestrator, last phase's workers and gate)
            // are no longer run members; keep them out of the live groups so the current team leads.
            if (s.run && !a.in_run && a.role_kind !== 'solo' && a.role_kind !== 'architect' && a.role_kind !== 'probe') earlier.push(a);
            else if (a.role_kind in LEAD) lead.push(a);
            else if (a.role_kind === 'worker') workers.push(a);
            else if (a.role_kind === 'gate' || a.role_kind === 'finale' || a.role_kind === 'probe') gate.push(a);
            else other.push(a);
        }
        lead.sort((x, y) => LEAD[x.role_kind] - LEAD[y.role_kind]);
        return { lead, workers, gate, other, earlier, run: s.run };
    }
    function roleLabel(a) {
        if (a.role_kind === 'worker' && a.worker) return a.worker.task_id;
        return a.name;
    }

    // ── rows ─────────────────────────────────────────────────────────────────────────────────────
    function agentRow(a, opts) {
        opts = opts || {};
        const s = S();
        const st = status(a, opts.now);
        const unread = s.ui.unread[a.id];
        const active = s.ui.route.name === 'agent' && Number(s.ui.route.id) === a.id;
        const w = a.worker;
        return h('button', { type: 'button', key: 'a' + a.id, class: 'row agent-row tone-' + st.tone + (active ? ' active' : '') + (opts.compact ? ' compact' : ''), onclick: () => M.act.nav('/agent/' + a.id) },
            glyph(a, 'lg'),
            h('span', { class: 'row-main' },
                h('span', { class: 'row-title' },
                    h('span', { class: 'name' }, w ? w.task_id : a.name),
                    w && w.attempt > 1 ? h('span', { class: 'mini-chip' }, '#' + w.attempt) : null,
                    w && w.tripwires && w.tripwires.length ? h('span', { class: 'mini-chip warn', title: 'edited outside its scope: ' + w.tripwires.join(', ') }, '⚠ ' + w.tripwires.length) : null,
                    a.approval_pending ? h('span', { class: 'mini-chip warn' }, '⚑') : null),
                h('span', { class: 'row-sub' },
                    h('i', { class: 'dot' }),
                    h('span', { class: 'st' }, st.text),
                    !opts.compact && w && w.title ? h('span', { class: 'row-extra' }, ' · ' + F.trunc(w.title, 60)) : null,
                    !opts.compact && a.tokens_total && a.busy ? h('span', { class: 'row-extra' }, ' · ' + F.tokens(a.tokens_total) + ' tok') : null)),
            unread ? h('span', { class: 'unread', title: unread + ' new' }, unread > 9 ? '9+' : String(unread)) : null,
            opts.compact ? null : h('span', { class: 'row-meta' }, a.model_alias),
            icon('chevron', 'chev'));
    }

    // A task without an agent yet (queued/preparing) — not clickable.
    function taskRow(w) {
        const st = wstate(w);
        return h('div', { key: 't' + w.task_id, class: 'row agent-row static tone-' + st.tone },
            h('span', { class: 'glyph lg', style: { color: 'var(--faint)' } }, '◇'),
            h('span', { class: 'row-main' },
                h('span', { class: 'row-title' }, h('span', { class: 'name' }, w.task_id), w.attempt > 1 ? h('span', { class: 'mini-chip' }, '#' + w.attempt) : null),
                h('span', { class: 'row-sub' }, h('i', { class: 'dot' }), h('span', { class: 'st' }, st.text), w.title ? h('span', { class: 'row-extra' }, ' · ' + F.trunc(w.title, 60)) : null)));
    }

    function section(title, kids, opts) {
        opts = opts || {};
        if (!kids || (Array.isArray(kids) && !kids.filter(Boolean).length)) return null;
        return h('section', { class: 'group' + (opts.cls ? ' ' + opts.cls : ''), key: opts.key || title },
            title ? h('h3', { class: 'group-title' }, title, opts.right || null) : null,
            h('div', { class: 'group-body' }, kids));
    }

    // Team list: leadership, phase workers (with not-yet-spawned tasks), gate/finale, others.
    function teamList(opts) {
        opts = opts || {};
        const now = Date.now();
        const g = groups();
        const run = g.run;
        const over = !!(run && run.stage && (run.stage.kind === 'done' || run.stage.kind === 'failed'));
        const out = [];
        // Without a run the others (Solo) lead; with one they follow the run's own groups.
        if (g.other.length && !run) out.push(section(null, g.other.map((a) => agentRow(a, { now, compact: opts.compact })), { key: 'other' }));
        if (g.lead.length) out.push(section('Leadership', g.lead.map((a) => agentRow(a, { now, compact: opts.compact })), { key: 'lead' }));
        if (run) {
            const byTask = new Map();
            for (const a of g.workers) if (a.worker) byTask.set(a.worker.task_id, a);
            const rows = [];
            const seen = new Set();
            for (const w of run.workers || []) {
                const a = w.agent !== undefined && w.agent !== null ? M.store.agent(w.agent) : byTask.get(w.task_id);
                if (a) { rows.push(agentRow(a, { now, compact: opts.compact })); seen.add(a.id); }
                else rows.push(taskRow(w));
            }
            for (const a of g.workers) if (!seen.has(a.id)) rows.push(agentRow(a, { now, compact: opts.compact }));
            const ph = run.stage && run.stage.phase !== undefined ? run.stage.phase : null;
            const phName = ph !== null && S().plan && S().plan.phases[ph] ? S().plan.phases[ph].name : null;
            // A finished run has no tasks left to come: skip the empty placeholders.
            if (rows.length || !over) out.push(section(ph !== null ? 'Phase ' + (ph + 1) + (phName ? ' · ' + phName : '') : 'Tasks', rows.length ? rows : h('div', { class: 'row-empty' }, 'No tasks yet'), { key: 'workers' }));
        } else if (g.workers.length) out.push(section('Workers', g.workers.map((a) => agentRow(a, { now, compact: opts.compact })), { key: 'workers' }));
        if (g.gate.length || (run && !over)) {
            out.push(section('Gate & finale', g.gate.length ? g.gate.map((a) => agentRow(a, { now, compact: opts.compact })) : h('div', { class: 'row-empty' }, h('span', { class: 'glyph', style: { color: 'var(--faint)' } }, '◎'), ' not yet'), { key: 'gate' }));
        }
        if (g.other.length && run) out.push(section('Also here', g.other.map((a) => agentRow(a, { now, compact: opts.compact })), { key: 'other' }));
        if (g.earlier.length) out.push(section('Earlier phases', g.earlier.map((a) => agentRow(a, { now, compact: opts.compact })), { key: 'past', cls: 'past' }));
        return out;
    }

    // ── stage rail ───────────────────────────────────────────────────────────────────────────────
    function stageSegments(run, plan) {
        const st = run.stage || {};
        const nPh = plan && plan.phases ? plan.phases.length : Math.max(1, (st.phase || 0) + 1);
        const segs = [{ id: 'plan', label: 'Plan', short: 'P' }, { id: 'review', label: 'Review', short: 'R' }];
        for (let i = 0; i < nPh; i++) segs.push({ id: 'p' + i, label: plan && plan.phases[i] ? plan.phases[i].name : 'Phase ' + (i + 1), short: String(i + 1) });
        segs.push({ id: 'finale', label: 'Finale', short: 'F' });
        let cur;
        switch (st.kind) {
            case 'setup': case 'planning': cur = 0; break;
            case 'review': cur = 1; break;
            case 'phase': cur = 2 + (st.phase || 0); break;
            case 'finale': cur = 2 + nPh; break;
            case 'done': cur = segs.length; break;
            case 'failed': cur = Math.min(segs.length - 1, 2 + (run.history ? run.history.length : 0)); break;
            default: cur = 0;
        }
        const halted = !!run.halted || st.kind === 'failed';
        return segs.map((s, i) => Object.assign(s, { state: i < cur ? 'done' : i === cur ? (halted ? (st.kind === 'failed' ? 'bad' : 'halt') : run.want_review && s.id === 'review' ? 'review' : 'cur') : 'todo' }));
    }
    function stageRail(run, plan, full) {
        const segs = stageSegments(run, plan);
        return h('div', { class: 'rail' + (full ? ' full' : ''), role: 'img', 'aria-label': 'stage: ' + (run.stage ? run.stage.label : '') },
            segs.map((s) => h('div', { key: s.id, class: 'seg ' + s.state, title: s.label },
                h('span', { class: 'seg-bar' }),
                full ? h('span', { class: 'seg-label' }, s.label) : h('span', { class: 'seg-short' }, s.short))));
    }

    function runSummary(run) {
        const bits = [];
        bits.push(run.stage ? run.stage.label : '');
        if (run.elapsed_ms) bits.push(F.durShort(run.elapsed_ms));
        if (run.total_tokens) bits.push(F.tokens(run.total_tokens) + ' tok');
        if (run.busy_count) bits.push(run.busy_count + ' busy');
        return bits.filter(Boolean).join(' · ');
    }

    function runCard(opts) {
        opts = opts || {};
        const s = S(), run = s.run;
        if (!run) return null;
        const st = run.stage || {};
        const kindTone = run.halted ? 'halt' : st.kind === 'done' ? 'done' : st.kind === 'failed' ? 'bad' : run.want_review ? 'review' : 'live';
        return h('button', { type: 'button', key: 'runcard', class: 'card run-card tone-' + kindTone + (opts.compact ? ' compact' : ''), onclick: () => M.act.nav('/run') },
            h('div', { class: 'run-card-top' },
                h('span', { class: 'run-state' }, h('i', { class: 'dot' }), run.halted ? 'halted' : st.kind === 'done' ? 'complete' : st.kind === 'failed' ? 'failed' : run.want_review ? 'plan review' : run.active ? 'running' : 'paused'),
                h('span', { class: 'run-id' }, run.pattern)),
            h('div', { class: 'run-brief' }, F.trunc(run.brief, opts.compact ? 90 : 160)),
            stageRail(run, s.plan, false),
            h('div', { class: 'run-meta' }, runSummary(run)));
    }

    // ── bands ────────────────────────────────────────────────────────────────────────────────────
    const HALT_TITLE = {
        user: 'Paused by you', auth: 'Sign-in problem', usage_limit: 'Usage limit reached', provider_rejected: 'The provider rejected a request',
        environment: 'Environment problem', gate_exhausted: 'The gate keeps failing', attempts_exhausted: 'A task ran out of attempts', agent_turn_failed: 'An agent turn failed',
    };
    function haltActions(hv) {
        const A = M.act;
        const who = hv.agent !== undefined && hv.agent !== null ? hv.agent : null;
        const whoName = hv.agent_name || 'the agent';
        const resume = btn('Resume', () => A.cmd('pause_resume', {}, { busyKey: 'resume' }), { kind: 'primary', icon: 'play', busyKey: 'resume', key: 'resume' });
        const respawn = (label) => who !== null ? btn(label, () => A.cmd('respawn', { agent: who }, { busyKey: 'respawn' + who, ok: 'Respawning ' + whoName }), { busyKey: 'respawn' + who, icon: 'respawn', key: 'respawn' }) : null;
        const model = who !== null ? btn('Switch model', () => A.sheet({ kind: 'model', agent: who }), { icon: 'model', key: 'model' }) : null;
        switch (hv.reason) {
            case 'user': case 'auth': case 'usage_limit': return [resume];
            case 'provider_rejected': return [model, respawn('Retry')];
            case 'environment': return [respawn('Retry'), who === null ? resume : null];
            case 'gate_exhausted': return [btn('Retry gate', () => A.cmd('pause_resume', {}, { busyKey: 'resume' }), { kind: 'primary', icon: 'refresh', busyKey: 'resume', key: 'resume' })];
            case 'attempts_exhausted': return [respawn('Retry ' + whoName)];
            case 'agent_turn_failed': return [respawn('Respawn ' + whoName), btn('Retry turn', () => A.cmd('pause_resume', {}, { busyKey: 'resume' }), { busyKey: 'resume', icon: 'refresh', key: 'retry' })];
        }
        return [resume];
    }
    function feedbackField(key, placeholder, label) {
        const s = S();
        const val = s.ui.drafts[key] || '';
        const submit = () => {
            const t = (S().ui.drafts[key] || '').trim();
            if (!t) return;
            M.act.cmd('run_input', { text: t }, { busyKey: key, ok: 'Sent' }).then(() => { S().ui.drafts[key] = ''; M.act.changed(); }, () => { });
        };
        return h('form', { class: 'inline-form', key: key, onsubmit: (e) => { e.preventDefault(); submit(); } },
            h('input', { class: 'input', value: val, placeholder, enterkeyhint: 'send', 'aria-label': placeholder, oninput: (e) => { S().ui.drafts[key] = e.target.value; M.act.changed(); } }),
            btn(label || 'Send', null, { type: 'submit', kind: 'primary', busyKey: key, disabled: !val.trim() }));
    }
    function haltBand(run) {
        const hv = run.halted;
        if (!hv) return null;
        const acts = haltActions(hv).filter(Boolean);
        const wantsFeedback = hv.reason === 'gate_exhausted' || hv.reason === 'attempts_exhausted';
        return h('div', { class: 'band halt', key: 'halt', role: 'alert' },
            h('div', { class: 'band-head' }, h('span', { class: 'band-ico' }, '⛔'), h('b', null, HALT_TITLE[hv.reason] || 'Halted'), h('span', { class: 'band-age' }, F.ago(hv.since_at))),
            hv.message ? h('div', { class: 'band-msg' }, F.trunc(hv.message, 400)) : null,
            hv.hint ? h('div', { class: 'band-hint' }, hv.hint) : null,
            h('div', { class: 'band-actions' }, acts),
            wantsFeedback ? feedbackField('halt-feedback', 'Feedback for the planner…', 'Send') : null);
    }
    function questionBand(run) {
        const q = run.question;
        if (!q) return null;
        return h('div', { class: 'band question', key: 'question', role: 'alert' },
            h('div', { class: 'band-head' }, h('span', { class: 'band-ico' }, '?'), h('b', null, (q.from_name || 'An agent') + ' has a question'), h('span', { class: 'band-age' }, F.ago(q.since_at))),
            h('div', { class: 'band-msg md' }, F.markdown(q.text)),
            feedbackField('answer', 'Your answer…', 'Answer'));
    }
    function reviewBand(run) {
        if (!run.want_review || (run.stage && run.stage.kind !== 'review')) return null;
        const title = S().plan && S().plan.title;
        return h('div', { class: 'band review', key: 'review' },
            h('div', { class: 'band-head' }, h('span', { class: 'band-ico' }, '☰'), h('b', null, 'Plan ready for review')),
            title ? h('div', { class: 'band-msg' }, title) : null,
            h('div', { class: 'band-actions' },
                btn('Review plan', () => M.act.nav('/run'), { kind: 'primary', icon: 'plan' }),
                btn('Approve', () => M.act.cmd('plan_approve', {}, { busyKey: 'approve-plan', ok: 'Plan approved' }), { busyKey: 'approve-plan', icon: 'check' })));
    }
    function bands() {
        const run = S().run;
        if (!run) return null;
        return [haltBand(run), questionBand(run), reviewBand(run)];
    }

    // ── approvals ────────────────────────────────────────────────────────────────────────────────
    function approvalCard(ap, opts) {
        opts = opts || {};
        const s = S();
        const k = 'ap:' + ap.key;
        const isQ = ap.kind === 'question' && ap.questions && ap.questions.length;
        const answers = s.ui.drafts[k] || {};
        const decide = (decision) => {
            let answer;
            if (isQ && decision === 'yes') {
                const parts = ap.questions.map((q) => (answers[q.id] || '').trim());
                if (parts.some((p) => !p)) { M.act.flash('Answer every question first', 'warn'); return; }
                answer = ap.questions.length === 1 ? parts[0] : ap.questions.map((q, i) => q.id + ': ' + parts[i]).join('\n');
            }
            M.act.cmd('approve', { key: ap.key, decision, answer }, { busyKey: k, ok: decision === 'no' ? 'Declined' : decision === 'cancel' ? 'Cancelled' : 'Approved' });
        };
        const kindIcon = ap.kind === 'command' ? 'terminal' : ap.kind === 'patch' ? 'diff' : ap.kind === 'question' ? 'question' : 'lock';
        const ag = M.store.agent(ap.agent);
        return h('div', { class: 'card approval', key: k, role: 'group', 'aria-label': 'approval' },
            h('div', { class: 'ap-head' },
                h('span', { class: 'ap-ico' }, icon(kindIcon)),
                h('span', { class: 'ap-title' },
                    opts.showAgent ? h('button', { type: 'button', class: 'link', onclick: () => M.act.nav('/agent/' + ap.agent) }, ag ? glyph(ag) : null, ' ', ap.agent_name) : null,
                    opts.showAgent ? h('span', { class: 'sep' }, ' · ') : null,
                    isQ ? 'asks' : 'needs approval'),
                h('span', { class: 'band-age' }, F.ago(ap.at))),
            ap.title ? h(ap.kind === 'command' ? 'pre' : 'div', { class: ap.kind === 'command' ? 'ap-cmd' : 'ap-text' }, ap.kind === 'command' ? '$ ' + ap.title : ap.title) : null,
            ap.detail ? h('div', { class: 'ap-detail' }, F.trunc(ap.detail, 600)) : null,
            isQ ? h('div', { class: 'ap-qs' }, ap.questions.map((q) => h('div', { class: 'ap-q', key: q.id },
                h('div', { class: 'ap-q-prompt' }, q.prompt),
                q.options && q.options.length ? h('div', { class: 'choices' }, q.options.map((o) => h('button', {
                    type: 'button', key: o, class: 'choice' + (answers[q.id] === o ? ' on' : ''),
                    onclick: () => { s.ui.drafts[k] = Object.assign({}, answers, { [q.id]: o }); M.act.changed(); },
                }, o))) : null,
                h('input', { class: 'input', placeholder: q.options && q.options.length ? 'Or type an answer…' : 'Your answer…', value: answers[q.id] || '', oninput: (e) => { s.ui.drafts[k] = Object.assign({}, S().ui.drafts[k] || {}, { [q.id]: e.target.value }); M.act.changed(); } })))) : null,
            h('div', { class: 'ap-actions' },
                isQ ? [
                    btn('Answer', () => decide('yes'), { kind: 'primary', busyKey: k, key: 'y' }),
                    btn('Decline', () => decide('no'), { key: 'n', busyKey: k }),
                ] : [
                    btn('Yes', () => decide('yes'), { kind: 'primary', busyKey: k, key: 'y', title: 'Allow once' }),
                    btn('Session', () => decide('session'), { busyKey: k, key: 's', title: 'Allow for the rest of this session' }),
                    btn('No', () => decide('no'), { busyKey: k, key: 'n', kind: 'danger-ghost', title: 'Decline; the agent continues' }),
                    btn('Cancel', () => decide('cancel'), { busyKey: k, key: 'c', kind: 'ghost', title: 'Decline and stop the turn' }),
                ]));
    }

    // ── empty / loading / error ──────────────────────────────────────────────────────────────────
    function empty(ico, title, body, actions) {
        return h('div', { class: 'empty', key: 'empty' },
            h('div', { class: 'empty-ico' }, typeof ico === 'string' && ico.length > 2 ? icon(ico) : ico),
            h('div', { class: 'empty-title' }, title),
            body ? h('div', { class: 'empty-body' }, body) : null,
            actions ? h('div', { class: 'empty-actions' }, actions) : null);
    }
    // What to say while there is nothing to show yet, by connection state.
    function waitText() {
        const st = S().conn.state;
        if (st === 'reconnecting' || st === 'offline') return 'Can’t reach Mantra — retrying…';
        return S().conn.mode === 'relay' ? 'Waiting for Mantra…' : 'Connecting to Mantra…';
    }
    function loading(text) {
        return h('div', { class: 'empty loading', key: 'loading' }, h('div', { class: 'mandala-spin', 'aria-hidden': 'true' }), h('div', { class: 'empty-body' }, text || 'Loading…'));
    }
    function errorBox(text, retry) {
        return h('div', { class: 'errbox', key: 'err', role: 'alert' }, icon('alert'), h('span', null, text), retry ? btn('Retry', retry, { sm: true }) : null);
    }

    function toggle(on, onchange, label, sub, opts) {
        opts = opts || {};
        return h('label', { class: 'toggle-row' + (opts.disabled ? ' disabled' : ''), key: opts.key || label },
            h('span', { class: 'toggle-text' }, h('span', null, label), sub ? h('small', null, sub) : null),
            h('input', { type: 'checkbox', class: 'switch', checked: !!on, disabled: opts.disabled || null, onchange: (e) => onchange(e.target.checked) }));
    }
    function segmented(value, options, onchange, aria) {
        return h('div', { class: 'segmented', role: 'radiogroup', 'aria-label': aria || null },
            options.map(([v, label]) => h('button', { type: 'button', key: v, role: 'radio', 'aria-checked': String(v === value), class: v === value ? 'on' : null, onclick: () => onchange(v) }, label)));
    }

    function copyBtn(text, label) {
        return btn(label || 'Copy', () => M.act.copy(text), { sm: true, icon: 'copy', kind: 'ghost' });
    }

    M.parts = {
        glyph, btn, iconBtn, effortBar, ctxGauge, ctxColor, modelChip, status, statusPill, wstate, groups, roleLabel,
        agentRow, taskRow, section, teamList, stageRail, stageSegments, runSummary, runCard,
        haltBand, questionBand, reviewBand, bands, feedbackField, approvalCard, empty, loading, waitText, errorBox, toggle, segmented, copyBtn, HALT_TITLE,
    };
})(window.Mantra = window.Mantra || {});
