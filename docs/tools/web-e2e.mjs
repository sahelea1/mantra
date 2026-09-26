// Mantra web UI — end-to-end check against a REAL running `mantra --demo --headless --web …`
// backend (see `mantra_src/scripts/web-e2e.sh`, which builds mantra, starts/stops it, and runs
// this script). Implements the scenario in WEB-DESIGN.md §13: login, the Solo agent's
// message → command approval → approve flow, starting a run from the Team screen, plan review,
// approving the plan, phase-1 workers appearing, talking to a worker, its diff, the question
// band, the pulse/runs/inbox/settings screens — and saves the screenshots the README/docs use.
//
// Two passes, run by web-e2e.sh against two separate `mantra --demo` processes (the question
// band only appears when the backend was started with MANTRA_MOCK_ASK=1 MANTRA_MOCK_ASK_USER=1 —
// see mock.rs — so it needs its own run rather than a flag flipped mid-scenario):
//   node web-e2e.mjs main   (default) — solo approval, start a run, workers, worker chat + diff,
//                                        pulse/runs/settings; captures 4 of the 5 required shots.
//   node web-e2e.mjs ask               — same run shape, then waits for and answers the question
//                                        band; captures the 5th (the inbox shot, while pending).
//
// Env vars:
//   MANTRA_E2E_BASE        base URL of the running web server (default http://127.0.0.1:7788)
//   MANTRA_E2E_PASS        the --web-password given to mantra (default "test")
//   MANTRA_E2E_OUT         where to write docs/img/web-*.png (default ../img next to this file)
//   MANTRA_E2E_PW_MODULES  a node_modules dir to load playwright from when it isn't resolvable
//                          the normal way (see loadPlaywright() below)
//
// Exit code is the number of failed steps (0 = all green). Each failed step is caught and logged
// so the rest of the scenario still runs and reports — see step() below — rather than the whole
// script dying on the first assertion and hiding how much of the UI actually works.

import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const BASE = process.env.MANTRA_E2E_BASE || 'http://127.0.0.1:7788';
const PASSWORD = process.env.MANTRA_E2E_PASS || 'test';
const OUT = process.env.MANTRA_E2E_OUT || path.join(HERE, '..', 'img');
const MODE = (process.argv[2] || 'main').trim();

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

async function main() {
    const { chromium, devices } = await loadPlaywright();
    const b = await chromium.launch();
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
    const failShot = (tag, name) => path.join(OUT, '..', '..', 'tmp-e2e-fail-' + tag + '-' + name.replace(/\W+/g, '-') + '.png');

    async function step(tag, page, name, fn) {
        const t = Date.now();
        try {
            await fn();
            console.log(`PASS [${tag}] ${name} (${Date.now() - t} ms)`);
        } catch (e) {
            fails++;
            console.log(`FAIL [${tag}] ${name}: ${e.message.split('\n')[0]}`);
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
        // fresh backend with those set for this mode.
        await step('ask', driver, 'start run → plan review', async () => {
            await nav(driver, '/');
            await driver.fill('textarea.goal', 'Add a REST API with auth and a small web UI');
            await driver.click('button:has-text("Start run")');
            await driver.waitForFunction(() => { const r = window.Mantra.store.S.run; return r && r.stage && r.stage.kind === 'review'; }, null, { timeout: 60000 });
        });
        await step('ask', driver, 'approve plan → workers', async () => {
            await nav(driver, '/run');
            await driver.waitForSelector('button:has-text("Approve plan")', { timeout: 5000 });
            await driver.click('button:has-text("Approve plan")');
            await driver.waitForFunction(() => { const r = window.Mantra.store.S.run; return r && (r.workers || []).length > 0; }, null, { timeout: 60000 });
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
            await driver.fill('.band.question input, .band.question textarea', 'cursor pagination, page size 50');
            await driver.click('.band.question button[type=submit]');
            await driver.waitForFunction(() => !window.Mantra.store.S.run.question, null, { timeout: 15000 });
            await driver.waitForFunction(() => /The user decided/.test(JSON.stringify([...window.Mantra.store.S.agents.values()].map((a) => (a.items || []).map((i) => i.text)))), null, { timeout: 30000 });
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
            await driver.waitForFunction(() => { const tx = document.querySelector('.tx'); return tx && /approved: \$ cargo test/.test(tx.textContent); }, null, { timeout: 20000 });
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
            await driver.waitForFunction(() => { const r = window.Mantra.store.S.run; return r && r.stage && r.stage.kind === 'review'; }, null, { timeout: 60000 });
        });
        await step('main', driver, 'run screen → approve plan → workers appear', async () => {
            await nav(driver, '/run');
            await driver.waitForSelector('button:has-text("Approve plan")', { timeout: 5000 });
            await driver.click('button:has-text("Approve plan")');
            await driver.waitForFunction(() => { const r = window.Mantra.store.S.run; return r && (r.workers || []).length > 0; }, null, { timeout: 60000 });
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
        await step('main', driver, 'open a running worker + send a message', async () => {
            await driver.waitForFunction(() => { const s = window.Mantra.store.S; return [...s.agents.values()].some((a) => a.role_kind === 'worker' && a.busy); }, null, { timeout: 30000 });
            worker = await driver.evaluate(() => { const s = window.Mantra.store.S; const w = [...s.agents.values()].find((a) => a.role_kind === 'worker' && a.busy); return w && w.id; });
            if (!worker) throw new Error('no busy worker found in the roster');
            await nav(driver, '/agent/' + worker);
            await driver.waitForSelector('.composer textarea');
            await driver.fill('.composer textarea', 'please keep the handlers small');
            await driver.click('.composer button.send');
            await driver.waitForFunction(() => /please keep the handlers small/.test(document.body.textContent), null, { timeout: 10000 });
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
            await driver.waitForFunction(() => document.querySelectorAll('.pulse-row, .pulse li, .pl-row').length > 0 || /spawn|phase/i.test(document.body.textContent), null, { timeout: 8000 });
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
    }

    await b.close();
    console.log(shots.length ? 'SCREENSHOTS\n' + shots.join('\n') : 'no screenshots taken');
    console.log(errs.length ? 'PAGE ERRORS\n' + errs.join('\n') : 'no page errors');
    console.log(fails ? `${fails} FAILED [${MODE}]` : `ALL PASS [${MODE}]`);
    process.exit(fails ? 1 : 0);
}

main();
