// Mantra web UI — end-to-end check against a REAL running `mantra --demo --headless --web …`
// backend (see `mantra_src/scripts/web-e2e.sh`, which builds mantra, starts/stops it, and runs
// this script). Implements the scenario in WEB-DESIGN.md §13: login, the Solo agent's
// message → command approval → approve flow, starting a run from the Team screen, plan review,
// approving the plan, phase-1 workers appearing, messaging the planner from a worker view, its diff, the question
// band, the pulse/runs/inbox/settings screens — and saves the screenshots the README/docs use.
// Since 0.8 also the configuration the web shares with the TUI: settings, models and providers
// (a provider from a key against the demo's mock API), patterns, MCP, and stopping a run.
//
// Two passes, run by web-e2e.sh against two separate `mantra --demo` processes (the question
// band only appears when the backend was started with MANTRA_MOCK_ASK=1 MANTRA_MOCK_ASK_USER=1 —
// see mock.rs — so it needs its own run rather than a flag flipped mid-scenario):
//   node web-e2e.mjs main   (default) — solo approval, start a run, workers, planner chat + worker diff,
//                                        pulse/runs/settings; captures 4 of the 5 required shots;
//                                        then settings, models, providers, patterns, MCP, and
//                                        stop the run → start the next from the Run screen.
//   node web-e2e.mjs ask               — same run shape, then waits for and answers the question
//                                        band; captures the 5th (the inbox shot, while pending).
//
// Env vars:
//   MANTRA_E2E_BASE        base URL of the running web server (default http://127.0.0.1:7788)
//   MANTRA_E2E_PASS        the --web-password given to mantra (default "test")
//   MANTRA_E2E_OUT         where to write the web-*.png shots (default ../img next to this file,
//                          the tracked docs/img — point it elsewhere for a check that should not
//                          touch them); a failed step's screenshot goes here too
//   MANTRA_BIN             a mantra binary to start for this run (`--demo --headless --web` on
//                          MANTRA_E2E_BASE's address, a fresh MANTRA_HOME, the password above) and
//                          stop afterwards; unset, a server is expected to be running already
//   MANTRA_E2E_PW_MODULES  a node_modules dir to load playwright from when it isn't resolvable
//                          the normal way (see loadPlaywright() below)
//
// Exit code is the number of failed steps (0 = all green). Each failed step is caught and logged
// so the rest of the scenario still runs and reports — see step() below — rather than the whole
// script dying on the first assertion and hiding how much of the UI actually works.

import path from 'node:path';
import fs from 'node:fs';
import os from 'node:os';
import { spawn } from 'node:child_process';
import { fileURLToPath, pathToFileURL } from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const BASE = process.env.MANTRA_E2E_BASE || 'http://127.0.0.1:7788';
const PASSWORD = process.env.MANTRA_E2E_PASS || 'test';
const OUT = process.env.MANTRA_E2E_OUT || path.join(HERE, '..', 'img');
const MODE = (process.argv[2] || 'main').trim();
const BIN = process.env.MANTRA_BIN || '';

// MANTRA_BIN: start the server this run drives, the way web-e2e.sh does — a fresh home, the demo
// at MOCK_SPEED 4, the question-band mock env for the `ask` pass — and say where its log is.
async function startServer() {
    // Something already answering there would be driven instead of the binary asked for.
    if (await fetch(BASE + '/').then(() => true, () => false)) throw new Error(BASE + ' is taken already — stop what is listening there, or set MANTRA_E2E_BASE to a free address');
    const home = fs.mkdtempSync(path.join(os.tmpdir(), 'mantra-e2e-'));
    const log = path.join(home, 'mantra.log');
    const addr = new URL(BASE).host;
    const env = Object.assign({}, process.env, { MANTRA_HOME: home, MANTRA_WEB_PASSWORD: PASSWORD, MANTRA_MOCK_SPEED: '4' }, MODE === 'ask' ? { MANTRA_MOCK_ASK: '1', MANTRA_MOCK_ASK_USER: '1' } : {});
    const out = fs.openSync(log, 'w');
    const child = spawn(BIN, ['--demo', '--headless', '--web', '--web-listen', addr], { env, stdio: ['ignore', out, out] });
    // never left behind: not by a failed step, not by an exception
    process.on('exit', () => child.kill());
    for (let i = 0; i < 150; i++) {
        if (child.exitCode !== null) throw new Error('mantra exited before the web server came up — ' + log);
        try { await fetch(BASE + '/'); console.log('started ' + BIN + ' (MANTRA_HOME=' + home + ', log ' + log + ')'); return child; } catch (_) { await new Promise((r) => setTimeout(r, 200)); }
    }
    child.kill();
    throw new Error('timed out waiting for ' + BASE + ' — ' + log);
}

// `playwright` is not a repo dependency (this is a scratchpad-committed test tool, not a crate);
// resolve it the normal way first (a local `npm i playwright` anywhere from this file up to the
// repo root), and only fall back to this development sandbox's own checkout so the script still
// runs while iterating here. Anyone else running this should `npm i playwright` once, e.g. in
// `docs/tools/` or the repo root.
async function loadPlaywright() {
    try {
        return await import('playwright');
    } catch (e) {
        const fallbackDir = process.env.MANTRA_E2E_PW_MODULES || '/tmp/claude-0/-home-user-mantra/4cae3785-c325-5d67-9a4a-b733d367d7bb/scratchpad/pw/node_modules';
        const entry = path.join(fallbackDir, 'playwright', 'index.mjs');
        try {
            return await import(pathToFileURL(entry).href);
        } catch (e2) {
            console.error('playwright is not installed and no fallback checkout was found.');
            console.error('  run `npm i playwright` (e.g. in docs/tools/) or set MANTRA_E2E_PW_MODULES to a node_modules dir that has it.');
            console.error('  (' + e.message + ')');
            process.exit(1);
        }
    }
}

// Poll from the test process: page.waitForFunction injects eval under older
// Playwright versions, which the application's strict CSP correctly rejects.
async function waitForState(page, predicate, arg, options = {}) {
    const deadline = Date.now() + (options.timeout ?? 30000);
    while (Date.now() < deadline) {
        if (await page.evaluate(predicate, arg)) return;
        await new Promise((resolve) => setTimeout(resolve, 100));
    }
    throw new Error('Timed out waiting for browser state: ' + predicate.toString());
}

async function main() {
    const { chromium, devices } = await loadPlaywright();
    const server = BIN ? await startServer() : null;
    fs.mkdirSync(OUT, { recursive: true });
    const b = await chromium.launch(process.env.MANTRA_E2E_CHROMIUM ? { executablePath: process.env.MANTRA_E2E_CHROMIUM } : {});
    let fails = 0;
    const errs = [];
    const shots = [];

    const newPage = async (phone) => {
        // deviceScaleFactor pinned to 1 so the PNG's pixel size is exactly the viewport (390×844 /
        // 1440×900), not iPhone 13's native @3x — the other iPhone 13 traits (touch, mobile UA)
        // are kept so :hover/coarse-pointer CSS and the mobile layout still behave like a phone.
        const ctx = await b.newContext(phone
            ? Object.assign({}, devices['iPhone 13'], { viewport: { width: 390, height: 844 }, deviceScaleFactor: 1, colorScheme: 'dark', serviceWorkers: 'block' })
            : { viewport: { width: 1440, height: 900 }, deviceScaleFactor: 1, colorScheme: 'dark', serviceWorkers: 'block' });
        const p = await ctx.newPage();
        p.on('pageerror', (e) => errs.push('pageerror: ' + e.message));
        p.on('console', (m) => { if (/Service Worker registration blocked/.test(m.text())) return; if (m.type() === 'error' || m.type() === 'warning') errs.push('console.' + m.type() + ': ' + m.text()); });
        return { ctx, page: p };
    };
    const login = async (p) => {
        await p.goto(BASE + '/');
        await p.waitForSelector('input[type=password]');
        await p.fill('input[type=password]', PASSWORD);
        await p.press('input[type=password]', 'Enter');
        await p.waitForSelector('.agent-row', { timeout: 15000 });
    };
    const nav = async (p, route) => { await p.evaluate((r) => window.Mantra.act.nav(r), route); await p.waitForTimeout(400); };
    const shot = async (p, name) => { const f = path.join(OUT, name); await p.screenshot({ path: f }); shots.push(f); };
    const failShot = (tag, name) => path.join(OUT, 'tmp-e2e-fail-' + tag + '-' + name.replace(/\W+/g, '-') + '.png');
    // A command as the page sends it: `{ok, d}` with the reply's data, or `{ok: false, e}`.
    const req = (p, cmd, args, timeout) => p.evaluate(([c, a, t]) => window.Mantra.act.request(c, a, t).then((d) => ({ ok: true, d }), (e) => ({ ok: false, e: e.message })), [cmd, args || {}, timeout || 30000]);
    const must = async (p, cmd, args, timeout) => { const r = await req(p, cmd, args, timeout); if (!r.ok) throw new Error(cmd + ': ' + r.e); return r.d; };
    const refused = async (p, cmd, args, words) => { const r = await req(p, cmd, args); if (r.ok) throw new Error(cmd + ' was not refused'); if (!r.e.includes(words)) throw new Error(cmd + ': "' + r.e + '" does not say "' + words + '"'); return r.e; };
    const field = (st, key) => st.fields.find((f) => f.key === key);

    async function step(tag, page, name, fn) {
        const t = Date.now();
        try {
            await fn();
            console.log(`PASS [${tag}] ${name} (${Date.now() - t} ms)`);
        } catch (e) {
            fails++;
            console.log(`FAIL [${tag}] ${name}: ${e.message}`);
            await page.screenshot({ path: failShot(tag, name) }).catch(() => {});
        }
    }

    const { page: driver } = await newPage(false);
    const { page: phone } = await newPage(true);

    await step('main', driver, 'login (desktop)', () => login(driver));
    await step('main', phone, 'login (phone)', () => login(phone));

    if (MODE === 'ask') {
        // Dedicated pass: the question band only appears when this mantra process was started
        // with MANTRA_MOCK_ASK=1 MANTRA_MOCK_ASK_USER=1 (mock.rs) — the shell script starts a
        // fresh backend with those set for this mode. That script is the Codex mock's, and the
        // built-ins run on bija: the run goes on a copy of mantra-default whose roles are on the
        // demo's Codex models (`sol-codex`, …), saved through the web like any pattern.
        await step('ask', driver, 'a pattern on the Codex mock → start run → plan review', async () => {
            const models = (await must(driver, 'models_get')).models.map((m) => m.alias);
            const p = (await must(driver, 'pattern_get', { name: 'mantra-default' })).pattern;
            for (const r of Object.values(p.roles)) if (models.includes(r.model + '-codex')) r.model = r.model + '-codex';
            for (const r of Object.values(p.roles)) if (r.fallback && models.includes(r.fallback + '-codex')) r.fallback = r.fallback + '-codex';
            await must(driver, 'pattern_save', { name: 'e2e-on-codex', pattern: p });
            await nav(driver, '/');
            await driver.waitForSelector('.start-row option[value="e2e-on-codex"]', { state: 'attached', timeout: 15000 });
            await driver.selectOption('.start-row select', 'e2e-on-codex');
            await driver.fill('textarea.goal', 'Add a REST API with auth and a small web UI');
            const selected = await driver.inputValue('.start-row select');
            await driver.selectOption('.start-row select', 'auto');
            await driver.waitForSelector('.pat-desc:has-text("planner")');
            await shot(driver, 'web-auto-picker-desktop.png');
            await driver.selectOption('.start-row select', selected);
            await driver.click('button:has-text("Start run")');
            await waitForState(driver, () => { const r = window.Mantra.store.S.run; return r && r.stage && r.stage.kind === 'review'; }, null, { timeout: 60000 });
        });
        await step('ask', driver, 'approve plan → workers', async () => {
            await nav(driver, '/run');
            await driver.waitForSelector('button:has-text("Approve plan")', { timeout: 5000 });
            await driver.click('button:has-text("Approve plan")');
            await waitForState(driver, () => { const r = window.Mantra.store.S.run; return r && (r.workers || []).length > 0; }, null, { timeout: 60000 });
            await nav(driver, '/');
        });
        await step('ask', driver, 'question band appears (MANTRA_MOCK_ASK=1 MANTRA_MOCK_ASK_USER=1)', async () => {
            await driver.waitForSelector('.band.question', { timeout: 90000 });
        });
        await step('ask', phone, 'screenshot: inbox with a pending question', async () => {
            await nav(phone, '/inbox');
            await phone.waitForSelector('.band.question', { timeout: 10000 });
            await phone.waitForTimeout(400);
            await shot(phone, 'web-inbox-phone.png');
        });
        await step('ask', driver, 'answer the question band', async () => {
            // The mock's p2-ui leaves its scope at about this moment, and the mock orchestrator
            // answers one event a turn — an answer that reaches it inside another turn (as the
            // result of the mantra_wait it is in) is not relayed. Answer once p2-ui is done and
            // the orchestrator is between turns, so the answer starts a turn of its own.
            await waitForState(driver, () => {
                const s = window.Mantra.store.S;
                const orch = [...s.agents.values()].find((a) => a.role_kind === 'orchestrator' && a.in_run);
                return (s.run.workers || []).some((w) => w.task_id === 'p2-ui' && w.state === 'done') && orch && !orch.busy;
            }, null, { timeout: 30000 });
            await driver.fill('.band.question input, .band.question textarea', 'cursor pagination, page size 50');
            await driver.click('.band.question button[type=submit]');
            await waitForState(driver, () => !window.Mantra.store.S.run.question, null, { timeout: 15000 });
            await waitForState(driver, () => /The user decided/.test(JSON.stringify([...window.Mantra.store.S.agents.values()].map((a) => (a.items || []).map((i) => i.text)))), null, { timeout: 30000 });
        });
    } else {
        // "Approval" pass: a direct message to the Solo agent triggers a command-approval card
        // (distinct from the run's plan-approval below) — approve it and check the diff it made.
        await step('main', driver, 'solo: send → approval → approve → output', async () => {
            await nav(driver, '/agent/1');
            await driver.waitForSelector('.composer textarea');
            await driver.fill('.composer textarea', 'add a greeting helper');
            await driver.click('.composer button.send');
            await driver.waitForSelector('.approval', { timeout: 20000 });
            await driver.click('.approval .btn.primary');
            await waitForState(driver, () => { const tx = document.querySelector('.tx'); return tx && /approved: \$ cargo test/.test(tx.textContent); }, null, { timeout: 20000 });
            if (await driver.locator('.approval').count()) throw new Error('approval card still shown after approving');
        });
        await step('main', driver, 'solo: diff', async () => {
            await driver.locator('.file-row').first().click({ timeout: 5000 });
            await driver.waitForSelector('.sheet .diff, .diff', { timeout: 8000 });
            await driver.keyboard.press('Escape');
            await driver.waitForTimeout(300);
        });
        await step('main', driver, 'start run → plan review', async () => {
            await nav(driver, '/');
            await driver.fill('textarea.goal', 'Add a REST API with auth and a small web UI');
            await driver.click('button:has-text("Start run")');
            await waitForState(driver, () => { const r = window.Mantra.store.S.run; return r && r.stage && r.stage.kind === 'review'; }, null, { timeout: 60000 });
        });
        await step('main', driver, 'run screen → approve plan → workers appear', async () => {
            await nav(driver, '/run');
            await driver.waitForSelector('button:has-text("Approve plan")', { timeout: 5000 });
            await driver.click('button:has-text("Approve plan")');
            await waitForState(driver, () => { const r = window.Mantra.store.S.run; return r && (r.workers || []).length > 0; }, null, { timeout: 60000 });
            await nav(driver, '/');
            await driver.waitForTimeout(1500); // let the roster settle into its busy/phase look
        });
        await step('main', phone, 'screenshot: Team, phase 1 with workers', async () => {
            await nav(phone, '/');
            await phone.waitForSelector('.agent-row', { timeout: 8000 });
            await phone.waitForTimeout(600);
            await shot(phone, 'web-team-phone.png');
        });
        let worker = null;
        await step('main', driver, 'open a worker log + send only to the planner', async () => {
            await waitForState(driver, () => { const s = window.Mantra.store.S; return [...s.agents.values()].some((a) => a.role_kind === 'worker' && a.busy); }, null, { timeout: 30000 });
            worker = await driver.evaluate(() => { const s = window.Mantra.store.S; const w = [...s.agents.values()].find((a) => a.role_kind === 'worker' && a.busy); return w && w.id; });
            if (!worker) throw new Error('no busy worker found in the roster');
            await nav(driver, '/agent/' + worker);
            await driver.waitForSelector('.composer textarea');
            const label = await driver.getAttribute('.composer textarea', 'aria-label');
            if (!/planner/i.test(label || '')) throw new Error('worker view composer is not addressed to the planner: ' + label);
            const planner = await driver.evaluate(() => window.Mantra.store.S.run.planner);
            await driver.fill('.composer textarea', 'please keep the handlers small');
            await driver.click('.composer button.send');
            await nav(driver, '/agent/' + planner);
            await waitForState(driver, () => /please keep the handlers small/.test(document.querySelector('.tx')?.textContent || ''), null, { timeout: 10000 });
            await nav(driver, '/agent/' + worker);
            if (/please keep the handlers small/.test(await driver.locator('.tx').textContent())) throw new Error('the worker received the planner message');
        });
        await step('main', phone, 'screenshot: worker agent screen', async () => {
            await nav(phone, '/agent/' + worker);
            await phone.waitForTimeout(600);
            await shot(phone, 'web-agent-phone.png');
        });
        await step('main', driver, 'worker diff', async () => {
            await driver.waitForSelector('.file-row', { timeout: 30000, state: 'attached' });
            await driver.locator('.file-row').first().click();
            await driver.waitForSelector('.diff', { timeout: 8000 });
            await driver.keyboard.press('Escape');
        });
        await step('main', driver, 'screenshot: run screen (desktop, phase in progress)', async () => {
            await nav(driver, '/run');
            await driver.waitForTimeout(600);
            await shot(driver, 'web-run-desktop.png');
        });
        await step('main', driver, 'pulse', async () => {
            await nav(driver, '/pulse');
            await waitForState(driver, () => document.querySelectorAll('.pulse-row, .pulse li, .pl-row').length > 0 || /spawn|phase/i.test(document.body.textContent), null, { timeout: 8000 });
        });
        await step('main', driver, 'runs list shows the open run', async () => {
            await nav(driver, '/runs');
            await driver.waitForTimeout(800);
            const t = await driver.textContent('body');
            if (!/open|running|phase/i.test(t)) throw new Error('runs list does not show the open run');
        });
        await step('main', phone, 'screenshot: settings', async () => {
            await nav(phone, '/settings');
            await phone.waitForTimeout(500);
            await shot(phone, 'web-settings-phone.png');
        });

        // ── 0.8: the configuration the web shares with the TUI ──────────────────────────────────
        await step('main', driver, 'settings: get → set → set back (and a refusal says why)', async () => {
            const st = await must(driver, 'settings_get');
            if (field(st, 'web.password').value === 'hunter2' || !['set', 'not set'].includes(field(st, 'web.password').value)) throw new Error('the web password reads ' + field(st, 'web.password').value);
            const was = field(st, 'approval_mode').value;
            const to = was === 'never' ? 'untrusted' : 'never';
            // through the page: the Solo section, its Approvals select, the one saving indicator
            await nav(driver, '/settings/solo');
            await driver.selectOption('#fld-approval_mode', to);
            await waitForState(driver, (v) => { const c = window.Mantra.store.S.ui.cfg; return c && c.settings && c.settings.fields.find((f) => f.key === 'approval_mode').value === v; }, to, { timeout: 8000 });
            if (field(await must(driver, 'settings_get'), 'approval_mode').value !== to) throw new Error('approval_mode did not change');
            const bypass = await must(driver, 'settings_set', { key: 'approval_mode', value: 'BYPASS-SANDBOX-DANGER' });
            if (field(bypass.settings, 'approval_mode').value !== 'BYPASS-SANDBOX-DANGER') throw new Error('bypass permission not saved');
            const back = await must(driver, 'settings_set', { key: 'approval_mode', value: was });
            if (field(back.settings, 'approval_mode').value !== was) throw new Error('approval_mode did not go back');
            await refused(driver, 'settings_set', { key: 'fps', value: '1000' }, 'from 4 to 60');
            // a refusal typed into a field shows next to it
            await nav(driver, '/settings/appearance');
            await driver.fill('#fld-fps', '1000');
            await driver.press('#fld-fps', 'Enter');
            await driver.waitForSelector('.fld.has-err .fld-err:has-text("from 4 to 60")', { timeout: 5000 });
            await driver.press('#fld-fps', 'Escape');
        });

        await step('main', driver, 'models: get → set a cell in the table → revert; keys come back masked', async () => {
            const m = await must(driver, 'models_get');
            const luna = m.models.find((x) => x.alias === 'luna');
            if (!luna) throw new Error('no luna in models_get');
            await nav(driver, '/settings/models');
            const row = driver.locator('.mt.models [data-alias="luna"]');
            await row.locator('button[aria-label^="More fields"]').click();
            await row.locator('button[aria-label^="Note:"]').click();
            await row.locator('input.cell-input').fill('set from the web e2e');
            await row.locator('input.cell-input').press('Enter');
            await waitForState(driver, () => { const c = window.Mantra.store.S.ui.cfg; return c.models.models.find((x) => x.alias === 'luna').note === 'set from the web e2e'; }, null, { timeout: 8000 });
            const back = await must(driver, 'model_set', { alias: 'luna', field: 'note', value: luna.note });
            if (back.registry.models.find((x) => x.alias === 'luna').note !== luna.note) throw new Error('the note did not go back');
            await refused(driver, 'model_set', { alias: 'luna', field: 'context', value: 'lots' }, '200k');
            // a pasted key never comes back
            const set = await must(driver, 'provider_set', { id: 'bija', field: 'api_key', value: 'e2e-secret-key-1234' });
            const shown = JSON.stringify(set.registry);
            if (shown.includes('e2e-secret-key') || set.registry.providers.find((p) => p.id === 'bija').api_key !== '••••1234') throw new Error('the key is not masked: ' + shown.slice(0, 200));
            await must(driver, 'provider_set', { id: 'bija', field: 'api_key', value: '' });
        });

        await step('main', driver, 'provider from a key against the demo’s mock API: probe, import one model, delete it', async () => {
            const m = await must(driver, 'models_get');
            const url = m.providers.find((p) => p.id === 'bija').base_url;
            // The literal URL is the bija provider itself: the key goes into it, and every model
            // the mock lists is configured already — nothing to import there.
            const same = await must(driver, 'provider_from_key', { key: 'e2e-any-key-5678', base_url: url }, 60000);
            if (same.provider !== 'bija' || same.probe_error || !same.candidates.length || same.candidates.some((c) => c.state !== 'configured')) throw new Error('the bija URL: ' + JSON.stringify(same).slice(0, 300));
            if (JSON.stringify(same).includes('e2e-any-key')) throw new Error('the key came back');
            await must(driver, 'provider_set', { id: 'bija', field: 'api_key', value: '' });
            // The same server as localhost is a provider of its own ("local"), whose models are all
            // new: the whole flow, through the Add-provider sheet.
            const local = url.replace('127.0.0.1', 'localhost');
            await nav(driver, '/settings/models');
            await driver.click('button:has-text("Add API provider")');
            await driver.fill('#ap-key', 'e2e-unknown-prefix-key');
            await driver.waitForSelector('#ap-url', { timeout: 5000 });
            await driver.waitForSelector('.sheet .fld-hint:has-text("names no provider")', { timeout: 5000 });
            await driver.fill('#ap-url', local);
            await driver.click('.sheet button:has-text("Probe")');
            await driver.waitForSelector('.sheet .check-row', { timeout: 60000 });
            await driver.click('.sheet button:has-text("None")');
            // the list is redrawn on the next frame: wait for it, or check() finds the box still ticked
            await waitForState(driver, () => !document.querySelector('.sheet .check-row:not(.done) input:checked'), null, { timeout: 5000 });
            const pick = await driver.locator('.sheet .check-row:not(.done) .mono').first().textContent();
            await driver.locator('.sheet .check-row:not(.done) input').first().check();
            await driver.click('.sheet button:has-text("Add 1 model")');
            await waitForState(driver, (model) => { const c = window.Mantra.store.S.ui.cfg; return !window.Mantra.store.S.ui.sheet && c.models.models.some((x) => x.provider === 'local' && x.model === model); }, pick, { timeout: 10000 });
            const added = (await must(driver, 'models_get')).models.find((x) => x.provider === 'local' && x.model === pick);
            if (!added) throw new Error('the imported model is not in models_get');
            // delete it in the table (behind its confirm sheet), then the provider
            await driver.locator('.mt.models [data-alias="' + added.alias + '"] button[aria-label^="Delete "]').click();
            await driver.click('.sheet button:has-text("Delete")');
            await waitForState(driver, (a) => !window.Mantra.store.S.ui.cfg.models.models.some((x) => x.alias === a), added.alias, { timeout: 8000 });
            const gone = await must(driver, 'provider_delete', { id: 'local' });
            if (gone.registry.providers.some((p) => p.id === 'local')) throw new Error('the provider is still there');
        });

        await step('main', driver, 'patterns: get → save a copy → an invalid save lists its errors → delete', async () => {
            const got = await must(driver, 'pattern_get', { name: 'mantra-default' });
            if (got.tag !== 'default' || !got.pattern.roles['worker-swarm'] || got.pattern.roles['worker-swarm'].parallel !== 16) throw new Error('pattern_get: ' + JSON.stringify(got).slice(0, 200));
            await nav(driver, '/patterns/mantra-default');
            await driver.waitForSelector('#pe-desc', { timeout: 8000 });
            await driver.fill('#pe-desc', 'my e2e take on the default');
            await driver.click('button:has-text("Save as copy")');
            await driver.fill('.sheet input[aria-label="Name of the copy"]', 'e2e-pattern');
            await driver.click('.sheet button:has-text("Save copy")');
            await waitForState(driver, () => location.pathname.endsWith('/patterns/e2e-pattern'), null, { timeout: 8000 });
            await driver.waitForSelector('.pe-titles .tag.t-yours', { timeout: 8000 });
            const list = await must(driver, 'patterns_list');
            const mine = list.patterns.find((x) => x.name === 'e2e-pattern');
            if (!mine || mine.tag !== 'yours' || mine.description !== 'my e2e take on the default') throw new Error('patterns_list: ' + JSON.stringify(mine));
            await driver.check('input[aria-label="Allow worker-small to start an Akasha swarm"]');
            await driver.locator('.pe-acts button:has-text("Save")').first().click();
            await driver.waitForTimeout(400);
            const swarmPattern = await must(driver, 'pattern_get', { name: 'e2e-pattern' });
            if (!swarmPattern.pattern.roles['worker-small'].can_spawn_swarm) throw new Error('swarm role toggle did not persist');
            // invalid: refused with every reason, in the editor too
            const bad = JSON.parse(JSON.stringify((await must(driver, 'pattern_get', { name: 'e2e-pattern' })).pattern));
            bad.roles.planner.kind = 'boss';
            await refused(driver, 'pattern_save', { name: 'e2e-pattern', pattern: bad }, 'kind must be one of');
            await driver.fill('input[aria-label="Workers at once of worker-swarm"]', '999');
            await driver.locator('.pe-acts button:has-text("Save")').first().click();
            await driver.waitForSelector('.pe-errors:has-text("parallel must be from 1 to 128")', { timeout: 8000 });
            await driver.click('.pe-acts button:has-text("Delete")');
            await driver.click('.sheet button:has-text("Delete")');
            await waitForState(driver, () => location.pathname.endsWith('/settings/patterns'), null, { timeout: 8000 });
            if ((await must(driver, 'patterns_list')).patterns.some((x) => x.name === 'e2e-pattern')) throw new Error('e2e-pattern is still listed');
        });

        await step('main', driver, 'MCP: the demo server answers its test', async () => {
            const t = await must(driver, 'mcp_test', { name: 'demo' }, 60000);
            if (!/tool/.test(t.report)) throw new Error('mcp_test: ' + t.report);
            await refused(driver, 'mcp_test', { name: 'nope' }, 'no MCP server named nope');
            await nav(driver, '/settings/mcp');
            await driver.locator('[data-mcp="demo"] button:has-text("Test")').click();
            await driver.waitForSelector('[data-mcp="demo"] .mcp-test.ok', { timeout: 60000 });
        });

        await step('main', driver, 'model test: a probe agent answers', async () => {
            const t = await must(driver, 'model_test', { alias: 'luna' }, 150000);
            if (!/^ok/.test(t.status)) throw new Error('model_test: ' + JSON.stringify(t));
        });

        await step('main', phone, 'completed worker stays calm after its phase retires', async () => {
            await waitForState(phone, (id) => {
                const s = window.Mantra.store.S, a = s.agents.get(id);
                return s.run?.stage?.kind === 'done' && a?.status === 'stopped' && a.completed_work;
            }, worker, { timeout: 60000 });
            const retired = await phone.evaluate((id) => {
                const a = window.Mantra.store.agent(id);
                return { worker: a.worker, in_run: a.in_run, completed: a.completed_work };
            }, worker);
            if (retired.worker || retired.in_run || !retired.completed) throw new Error('retired worker lost precise completion: ' + JSON.stringify(retired));
            await nav(phone, '/agent/' + worker);
            await phone.waitForSelector('.agent-head .pill.tone-done:has-text("completed")');
            if (await phone.locator('.agent-trouble').count()) throw new Error('completed worker has a trouble card');
            await phone.waitForSelector('.composer-note.completed:has-text("work is complete")');
            await shot(phone, 'web-agent-completed-phone.png');
        });

        await step('main', driver, 'run: start one from the Run screen → Stop (confirm) → start the next → delete', async () => {
            // The demo's run is over by now (Land / Delete, and the start card under it); a run
            // that is over hands its screen to the next one.
            const startFromCard = async (goal, pattern, width) => {
                const before = await driver.evaluate(() => (window.Mantra.store.S.run || {}).id || null);
                await driver.waitForSelector('textarea.goal', { timeout: 10000 });
                await driver.fill('textarea.goal', goal);
                if (pattern) await driver.selectOption('.start-row select', pattern);
                if (width) await driver.fill('.par-wrap input', String(width));
                await driver.click('.start-row button:has-text("Start run")');
                await waitForState(driver, ([o, pt]) => { const r = window.Mantra.store.S.run; return r && r.id !== o && r.active && (!pt || r.pattern === pt); }, [before, pattern], { timeout: 20000 });
                return driver.evaluate(() => window.Mantra.store.S.run.id);
            };
            await nav(driver, '/run');
            await waitForState(driver, () => { const r = window.Mantra.store.S.run; return r && !r.active; }, null, { timeout: 60000 });
            await driver.waitForSelector('.run-hero button:has-text("Delete")', { timeout: 5000 });
            const first = await startFromCard('Add a changelog page', 'mantra-lean', 2);
            await nav(driver, '/run');
            await driver.click('.run-hero button:has-text("Stop")');
            await driver.click('.sheet button:has-text("Stop run")');
            await waitForState(driver, () => { const r = window.Mantra.store.S.run; return r && !r.active && r.stage.stopped; }, null, { timeout: 20000 });
            await driver.waitForSelector('.run-stopped', { timeout: 5000 });
            await refused(driver, 'run_stop', {}, 'no active run to stop');
            // the start card is right there again
            await startFromCard('Add a status page');
            await must(driver, 'run_stop');
            await driver.waitForSelector('.run-hero button:has-text("Delete")', { timeout: 10000 });
            await driver.click('.run-hero button:has-text("Delete")');
            await driver.click('.sheet button:has-text("Delete")');
            await waitForState(driver, () => !window.Mantra.store.S.run, null, { timeout: 20000 });
            const runs = await must(driver, 'runs_list');
            if (!runs.runs.some((x) => x.id === first && x.stage === 'stopped')) throw new Error('the stopped run is not in the runs list: ' + JSON.stringify(runs.runs.map((x) => [x.id, x.stage])));
        });
    }

    if (MODE === 'main') {
        await step('main', driver, 'drafts survive navigation, Escape, palette, and a delayed failed send', async () => {
            const ids = await driver.evaluate(() => {
                const store = window.Mantra.store;
                const solo = [...store.S.agents.values()].find(a => a.role_kind === 'solo');
                if (!solo) throw new Error('missing Solo');
                // A UI-only idle peer keeps this navigation test independent of run timing.
                // No command is sent to the fixture agent.
                store.S.agents.set(90001, { ...solo, id: 90001, name: 'Draft fixture', items: [] });
                return [solo.id, 90001];
            });
            const box = '.composer textarea';
            await nav(driver, '/agent/' + ids[0]);
            await driver.fill(box, 'draft for first agent');
            await driver.press(box, 'Escape');
            if (await driver.inputValue(box) !== 'draft for first agent') throw new Error('Escape erased draft');
            await driver.press(box, 'Control+k');
            await driver.waitForSelector('.palette input');
            await driver.press('.palette input', 'Escape');
            await waitForState(driver, () => document.activeElement === document.querySelector('.composer textarea'));
            await nav(driver, '/agent/' + ids[1]);
            await driver.fill(box, 'draft for second agent');
            await nav(driver, '/agent/' + ids[0]);
            if (await driver.inputValue(box) !== 'draft for first agent') throw new Error('agent switch lost draft');
            await driver.evaluate(() => {
                window.e2eRequest = window.Mantra.act.request;
                window.Mantra.act.request = (cmd, ...args) => cmd === 'send'
                    ? new Promise((resolve, reject) => { window.e2eRejectSend = reject; })
                    : window.e2eRequest(cmd, ...args);
            });
            try {
                await driver.click('.composer button.send');
                await waitForState(driver, () => !!window.e2eRejectSend);
                await driver.fill(box, 'newer draft');
                await driver.evaluate(() => window.e2eRejectSend(new Error('simulated disconnected send')));
                await waitForState(driver, () => document.querySelector('.composer textarea').value === 'draft for first agent\nnewer draft');
            } finally {
                await driver.evaluate(() => { window.Mantra.act.request = window.e2eRequest; delete window.e2eRequest; delete window.e2eRejectSend; });
            }
            await nav(driver, '/agent/' + ids[1]);
            if (await driver.inputValue(box) !== 'draft for second agent') throw new Error('failed send affected another agent');
            await nav(driver, '/agent/' + ids[0]);
            await driver.evaluate(() => window.Mantra.store.S.agents.delete(90001));
        });
        await step('main', driver, 'confirmation keeps keyboard focus and submits once', async () => {
            await driver.evaluate(() => {
                window.e2eConfirmed = 0;
                window.Mantra.act.sheet({ kind: 'confirm', title: 'Fixture confirmation', body: 'Local UI regression', confirm: 'Confirm fixture', run: () => { window.e2eConfirmed++; } });
            });
            await driver.waitForSelector('.sheet');
            await driver.keyboard.press('Control+k');
            if (await driver.locator('.palette').count()) throw new Error('palette replaced confirmation');
            await driver.evaluate(() => { const b = [...document.querySelectorAll('.sheet button')].find(x => x.textContent.includes('Confirm fixture')); b.click(); b.click(); });
            await waitForState(driver, () => window.e2eConfirmed === 1 && !window.Mantra.store.S.ui.sheet);
        });
        await step('main', driver, 'agent process: cancel stop → stop → transcript stays → new Solo', async () => {
            const fresh = await must(driver, 'new_solo');
            await nav(driver, '/agent/' + fresh.agent);
            await driver.waitForSelector('.agent-actions button:has-text("Stop agent")', { timeout: 10000 });
            await driver.click('.agent-actions button:has-text("Stop agent")');
            await driver.click('.sheet button:has-text("Cancel")');
            const stillLive = await driver.evaluate((id) => window.Mantra.store.agent(id).status !== 'stopped', fresh.agent);
            if (!stillLive) throw new Error('cancelling stopped the agent');
            await driver.click('.agent-actions button:has-text("Stop agent")');
            await driver.click('.sheet button:has-text("Stop agent")');
            await waitForState(driver, (id) => window.Mantra.store.agent(id)?.status === 'stopped', fresh.agent, { timeout: 10000 });
            await driver.waitForSelector('.agent-trouble button:has-text("New Solo session")');
            await refused(driver, 'send', { agent: fresh.agent, text: 'must not restart' }, 'process has ended');
            await driver.click('.agent-trouble button:has-text("New Solo session")');
            await waitForState(driver, (old) => location.pathname.startsWith('/agent/') && !location.pathname.endsWith('/' + old), fresh.agent);
        });
        // Only close a server this test itself launched, never a caller-owned remote instance.
        if (server) await step('main', driver, 'host shutdown: cancel keeps it alive → confirm exits cleanly', async () => {
            await nav(driver, '/settings/about');
            await driver.click('button:has-text("Close Mantra")');
            await driver.click('.sheet button:has-text("Cancel")');
            await must(driver, 'settings_get');
            await driver.click('button:has-text("Close Mantra")');
            await driver.click('.sheet button:has-text("Close Mantra")');
            await new Promise((resolve, reject) => {
                const timer = setTimeout(() => reject(new Error('Mantra did not exit after shutdown')), 10000);
                if (server.exitCode !== null) { clearTimeout(timer); return server.exitCode === 0 ? resolve() : reject(new Error('exit ' + server.exitCode)); }
                server.once('exit', (code) => { clearTimeout(timer); code === 0 ? resolve() : reject(new Error('exit ' + code)); });
            });
        });
    }

    await b.close();
    if (server) server.kill();
    console.log(shots.length ? 'SCREENSHOTS\n' + shots.join('\n') : 'no screenshots taken');
    console.log(errs.length ? 'PAGE ERRORS\n' + errs.join('\n') : 'no page errors');
    console.log(fails ? `${fails} FAILED [${MODE}]` : `ALL PASS [${MODE}]`);
    process.exit(fails ? 1 : 0);
}

main();
