// Standalone browser regression for transcript windows and remote delta state.
// Run: node docs/tools/web-state-e2e.mjs (uses installed Playwright + Chromium).
// MANTRA_E2E_CHROMIUM can point to an existing Chromium executable if its version differs.
import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

let playwright;
try { playwright = createRequire(import.meta.url)('playwright'); }
catch (_) {
    const modules = process.env.MANTRA_E2E_PW_MODULES;
    if (!modules || !fs.existsSync(path.join(modules, 'playwright', 'index.js'))) throw new Error('Install playwright or set MANTRA_E2E_PW_MODULES to its node_modules directory');
    playwright = createRequire(import.meta.url)(path.join(modules, 'playwright', 'index.js'));
}
const { chromium } = playwright;
const ui = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../../mantra_src/src/web/assets/ui');
const browser = await chromium.launch({ headless: true, ...(process.env.MANTRA_E2E_CHROMIUM ? { executablePath: process.env.MANTRA_E2E_CHROMIUM } : {}) });
try {
    const page = await browser.newPage({ viewport: { width: 700, height: 600 } });
    await page.setContent('<style>.tx { height: 260px; overflow: auto } .msg { min-height: 32px; padding: 4px }</style><div id="root"></div>');
    for (const name of ['dom.js', 'fmt.js', 'store.js']) await page.addScriptTag({ path: path.join(ui, name) });
    await page.evaluate(() => {
        const M = window.Mantra;
        M.parts = {
            btn: (label, click) => M.h('button', { onclick: click }, label),
            retryingText: (a) => a.status_detail || 'retrying',
        };
        M.act = {
            wide: () => true, motion: () => false,
            changed: () => requestAnimationFrame(() => window.testRender()),
            request: async () => ({ items: [] }),
            flash: (text) => { throw Error(text); },
        };
    });
    await page.addScriptTag({ path: path.join(ui, 'agent.js') });
    const initial = await page.evaluate(() => {
        const M = window.Mantra;
        const agent = (id, count) => ({ id, name: 'Worker ' + id, status: 'idle', busy: false, items_first: 0, items_total: count,
            items: Array.from({ length: count }, (_, ord) => ({ ord, id: 'i' + ord, kind: 'agent', text: 'Message ' + ord, done: true, v: 1 })) });
        M.store.applySnapshot({ seq: 1, agents: [agent(1, 1200), agent(2, 500)] });
        M.store.S.ui.route = { name: 'agent', id: 1 };
        window.testRender = () => {
            const a = M.store.agent(M.store.S.ui.route.id);
            M.patch(document.getElementById('root'), M.agentView.transcript(a));
            M.agentView.afterRender();
        };
        testRender();
        const a = M.store.agent(1);
        const items = a.items;
        M.store.applyDelta({ seq: 2, items: { 1: [{ ord: 1199, id: 'i1199', v: 2, done: true, append: ' streamed' }] } }, 1);
        testRender();
        return { sameArray: a.items === items, tail: a.items.at(-1).text, count: document.querySelectorAll('.tx-inner > .msg').length };
    });
    assert.equal(initial.sameArray, true, 'streaming must not copy the entire history');
    assert.equal(initial.tail, 'Message 1199 streamed');
    assert.equal(initial.count, 200);

    // Browsing back through six pages must keep a bounded DOM and the viewed position.
    for (let n = 0; n < 5; n++) {
        await page.getByRole('button', { name: 'Load earlier' }).click();
        await page.waitForTimeout(30);
        const count = await page.locator('.tx-inner > .msg').count();
        assert.ok(count <= 400 && count >= 200, `page ${n}: mounted ${count} items`);
    }
    const older = await page.evaluate(() => {
        const tx = document.querySelector('.tx');
        tx.scrollTop = 120;
        return { top: tx.scrollTop, first: document.querySelector('.tx-inner > .msg').textContent };
    });
    await page.waitForTimeout(50);
    await page.evaluate(() => { Mantra.store.S.ui.route.id = 2; testRender(); });
    await page.evaluate(() => { Mantra.store.S.ui.route.id = 1; testRender(); });
    const restored = await page.evaluate(() => ({ top: document.querySelector('.tx').scrollTop, first: document.querySelector('.tx-inner > .msg').textContent }));
    assert.equal(restored.first, older.first, 'switching agents preserves the history page');
    assert.ok(Math.abs(restored.top - older.top) < 5, `switching agents preserves scroll (${older.top} -> ${restored.top})`);
    await page.getByRole('button', { name: 'latest' }).click();
    await page.waitForTimeout(30);

    // A waiting turn can still be busy at the protocol layer; its UI must not animate as working.
    const waits = await page.evaluate(() => {
        const M = window.Mantra, a = M.store.agent(1);
        const states = [
            { status: 'waiting', busy: true },
            { status: 'busy', busy: true, swarm_waiting: true },
            { status: 'retrying', busy: true, status_detail: 'provider' },
            { status: 'busy', busy: true, quiet_ms: 30000 },
        ];
        return states.map((x) => {
            Object.assign(a, { approval_pending: false, waiting_answer: false, swarm_waiting: false }, x);
            M.patch(document.getElementById('root'), M.agentView.transcript(a));
            return { text: document.querySelector('.busy-line')?.textContent || '', className: document.querySelector('.busy-line')?.className || '' };
        });
    });
    assert.match(waits[0].text, /Waiting for approval/);
    assert.match(waits[1].text, /Waiting for Akasha swarm/);
    assert.match(waits[2].text, /provider/);
    assert.match(waits[3].text, /quiet 30s/);
    assert.equal(waits[3].className, 'busy-line', 'quiet telemetry does not mean blocked');

    // The final large response should have an explicit, reversible full-text affordance.
    const large = await page.evaluate(() => {
        const M = window.Mantra, a = M.store.agent(1);
        a.items[a.items.length - 1] = { ord: 1199, kind: 'agent', text: 'begin ' + 'x'.repeat(80000) + ' end', done: true, v: 3 };
        M.store.S.ui.route.id = 1;
        M.agentView.scrollToEnd(1);
        testRender();
        return { length: document.querySelector('.tx-inner').textContent.length, button: !!Array.from(document.querySelectorAll('.msg button')).find((b) => b.textContent === 'Show full message') };
    });
    assert.ok(large.button);
    assert.ok(large.length < 70000, 'large text is bounded until explicitly expanded');
    await page.getByRole('button', { name: 'Show full message' }).click();
    await page.waitForTimeout(30);
    assert.ok((await page.locator('.tx-inner').textContent()).length > 80000);

    // Removal discards item expansion and the old window; reusing an id starts from 200 items.
    const removed = await page.evaluate(() => {
        const M = window.Mantra, s = M.store.S;
        s.ui.expanded['1:1199'] = true;
        s.ui.expanded['2:5'] = true;
        s.ui.route.id = 2;
        testRender(); // agent 1's detached DOM reference is released here
        M.store.applyDelta({ seq: 3, agents_removed: [1] }, 2);
        testRender();
        return { old: s.ui.expanded['1:1199'], other: s.ui.expanded['2:5'] };
    });
    assert.equal(removed.old, undefined);
    assert.equal(removed.other, true);
    const reused = await page.evaluate(() => {
        const M = window.Mantra;
        M.store.applySnapshot({ seq: 4, agents: [{ id: 1, name: 'New Solo', status: 'idle', busy: false, items_first: 0, items_total: 500,
            items: Array.from({ length: 500 }, (_, ord) => ({ ord, kind: 'agent', text: 'New ' + ord, done: true, v: 1 })) }] });
        M.store.S.ui.route.id = 1;
        testRender();
        return document.querySelectorAll('.tx-inner > .msg').length;
    });
    assert.equal(reused, 200, 'a reused id starts with a fresh transcript window');

    // A completed run stops its processes. The worker page must not describe that as a failure.
    await page.evaluate(() => { window.testParts = window.Mantra.parts; });
    await page.addScriptTag({ path: path.join(ui, 'parts.js') });
    const finished = await page.evaluate(() => {
        const M = window.Mantra, oldParts = window.testParts;
        // agent.js captured the small test stub at load time; fill that object with the real
        // shared components used by the complete agent screen.
        Object.assign(oldParts, M.parts);
        M.parts = oldParts;
        const a = { id: 7, name: 'Worker 7', role_kind: 'worker', status: 'stopped', stopped_by_user: false,
            in_run: true, busy: false, worker: { task_id: 'p2-ui', state: 'done', attempt: 1 },
            items: [], items_first: 0, items_total: 0, created_at: Date.now(), model_alias: 'luna' };
        M.store.S.agents.set(7, a);
        M.store.S.run = { stage: { kind: 'done' } };
        const inspect = () => {
            M.patch(document.getElementById('root'), M.agentView.agentScreen(7));
            return { status: M.parts.status(a), trouble: !!document.querySelector('.agent-trouble'),
                composer: document.querySelector('.composer-note')?.textContent || '',
                composerClass: document.querySelector('.composer-note')?.className || '' };
        };
        const completed = inspect();
        a.stopped_by_user = true;
        const manuallyStopped = inspect();
        a.worker.state = 'failed'; a.stopped_by_user = false;
        const stoppedFailedWorker = inspect();
        a.status = 'failed';
        const failed = inspect();
        return { completed, manuallyStopped, stoppedFailedWorker, failed };
    });
    assert.deepEqual(finished.completed.status, { text: 'completed', tone: 'done' });
    assert.equal(finished.completed.trouble, false);
    assert.match(finished.completed.composer, /work is complete/);
    assert.match(finished.completed.composerClass, /completed/);
    assert.equal(finished.manuallyStopped.status.text, 'stopped by you');
    assert.equal(finished.manuallyStopped.trouble, true);
    assert.equal(finished.stoppedFailedWorker.status.tone, 'bad');
    assert.equal(finished.stoppedFailedWorker.trouble, true);
    assert.equal(finished.failed.status.tone, 'bad');
    assert.equal(finished.failed.trouble, true);
    // Run composers always address the planner, even while inspecting a worker's log.
    const routed = await page.evaluate(async () => {
        const M = window.Mantra, s = M.store.S;
        const base = { status: 'idle', busy: false, items: [], items_first: 0, items_total: 0, created_at: Date.now(), in_run: true, model_alias: 'luna' };
        s.agents.set(9, { ...base, id: 9, name: 'Planner', role_kind: 'planner' });
        s.agents.set(7, { ...base, id: 7, name: 'Worker 7', role_kind: 'worker', worker: { task_id: 'p2-ui', state: 'running', attempt: 1 } });
        s.order = [7, 9];
        s.run = { id: 'test-run', planner: 9, stage: { kind: 'phase' }, workers: [{ task_id: 'p2-ui', agent: 7, attempt: 1 }] };
        s.ui.route = { name: 'agent', id: 7 };
        s.conn.state = 'open';
        const calls = [];
        M.act.request = async (name, args) => { calls.push({ name, ...args }); return {}; };
        M.act.flash = () => {};
        M.act.nav = (url) => { s.ui.route = { name: 'agent', id: Number(url.split('/').at(-1)) }; };
        M.patch(document.getElementById('root'), M.agentView.agentScreen(7));
        const textarea = document.querySelector('.composer textarea');
        textarea.value = 'Please adjust the plan';
        textarea.dispatchEvent(new Event('input', { bubbles: true }));
        M.patch(document.getElementById('root'), M.agentView.agentScreen(7));
        document.querySelector('.composer .send').click();
        await Promise.resolve();
        return { calls, label: textarea.getAttribute('aria-label'), note: document.querySelector('.composer-note')?.textContent };
    });
    assert.equal(routed.calls[0].agent, 9, 'worker-page messages address the planner');
    assert.equal(routed.label, 'Message Planner');
    assert.match(routed.note, /Messages go to the planner/);

    // Respawn replaces an id. Hide its stale failure while pending and follow the new attempt.
    const pending = await page.evaluate(() => {
        const M = window.Mantra, s = M.store.S;
        Object.assign(s.agents.get(7), { status: 'failed', status_detail: 'old provider error' });
        M.agentView.trackRespawn(7);
        M.patch(document.getElementById('root'), M.agentView.agentScreen(7));
        return { trouble: !!document.querySelector('.agent-trouble'), text: document.querySelector('.agent-foot').textContent };
    });
    assert.equal(pending.trouble, false);
    assert.match(pending.text, /Respawning Worker 7/);
    await page.evaluate(() => {
        const M = window.Mantra, s = M.store.S;
        const old = s.agents.get(7);
        s.agents.set(8, { ...old, id: 8, name: 'Worker 8', status: 'busy', status_detail: undefined, worker: { ...old.worker, attempt: 2 } });
        old.in_run = false; old.status = 'stopped';
        s.run.workers = [{ task_id: 'p2-ui', agent: 8, attempt: 2 }];
        M.patch(document.getElementById('root'), M.agentView.agentScreen(7));
    });
    await page.waitForTimeout(30);
    assert.equal(await page.evaluate(() => Mantra.store.S.ui.route.id), 8, 'respawn opens the replacement attempt');
    const recovered = await page.evaluate(() => {
        const M = window.Mantra;
        M.patch(document.getElementById('root'), M.agentView.agentScreen(8));
        return { trouble: !!document.querySelector('.agent-trouble'), text: document.querySelector('.agent-foot').textContent };
    });
    assert.equal(recovered.trouble, false);
    assert.doesNotMatch(recovered.text, /old provider error|Respawning/);

    await page.addScriptTag({ path: path.join(ui, 'screens.js') });
    const auto = await page.evaluate(() => {
        const M = window.Mantra, s = M.store.S;
        s.app = { pattern_name: 'auto', pattern_list: [{ name: 'auto', builtin: true, description: 'Planner creates a pattern' }, { name: 'mantra-default', builtin: true }] };
        M.patch(document.getElementById('root'), M.screens.startRunCard());
        return { selected: document.querySelector('select').value, label: document.querySelector('option').textContent };
    });
    assert.equal(auto.selected, 'auto');
    assert.match(auto.label, /Auto.*planner creates the pattern/);
    console.log('web-state-e2e: transcript, completed worker, planner routing, respawn recovery and Auto picker passed');
} finally {
    await browser.close();
}
