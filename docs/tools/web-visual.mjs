// Mantra web UI — screenshots of the 0.7.0 surfaces for a visual review, phone (390×844) and
// desktop (1280×800): the model picker (Codex, Claude Code and Bija rows), a `mantra-swarm` run
// with its "Shared notes" open, and a bija worker's agent screen (the Backend row). A sibling of
// web-e2e.mjs, run against the same kind of server:
//
//   MANTRA_HOME=$(mktemp -d) MANTRA_WEB_PASSWORD=test mantra --demo --headless --web --web-listen 127.0.0.1:7788
//   MANTRA_E2E_OUT=/tmp/shots node docs/tools/web-visual.mjs [prefix]
//
// It asserts nothing beyond what it needs to reach each screen; the shots are for a human.
// Env vars as in web-e2e.mjs (MANTRA_E2E_BASE, MANTRA_E2E_PASS, MANTRA_E2E_OUT, MANTRA_E2E_PW_MODULES).

import path from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const BASE = process.env.MANTRA_E2E_BASE || 'http://127.0.0.1:7788';
const PASSWORD = process.env.MANTRA_E2E_PASS || 'test';
const OUT = process.env.MANTRA_E2E_OUT || path.join(HERE, '..', '..', 'tmp-web-visual');
const PREFIX = (process.argv[2] || 'web').trim();

async function loadPlaywright() {
    try {
        return await import('playwright');
    } catch (e) {
        const dir = process.env.MANTRA_E2E_PW_MODULES;
        if (!dir) throw new Error('playwright is not installed: `npm i playwright`, or set MANTRA_E2E_PW_MODULES');
        return await import(pathToFileURL(path.join(dir, 'playwright', 'index.mjs')).href);
    }
}

async function main() {
    const { chromium, devices } = await loadPlaywright();
    const b = await chromium.launch();
    const errs = [];
    const shots = [];
    const newPage = async (phone) => {
        const ctx = await b.newContext(phone
            ? Object.assign({}, devices['iPhone 13'], { viewport: { width: 390, height: 844 }, deviceScaleFactor: 1, colorScheme: 'dark', serviceWorkers: 'block' })
            : { viewport: { width: 1280, height: 800 }, deviceScaleFactor: 1, colorScheme: 'dark', serviceWorkers: 'block' });
        const p = await ctx.newPage();
        p.on('pageerror', (e) => errs.push('pageerror: ' + e.message));
        p.on('console', (m) => { if (/Service Worker registration blocked/.test(m.text())) return; if (m.type() === 'error') errs.push('console.error: ' + m.text()); });
        await p.goto(BASE + '/');
        await p.waitForSelector('input[type=password]');
        await p.fill('input[type=password]', PASSWORD);
        await p.press('input[type=password]', 'Enter');
        await p.waitForSelector('.agent-row', { timeout: 15000 });
        return p;
    };
    const nav = async (p, route) => { await p.evaluate((r) => window.Mantra.act.nav(r), route); await p.waitForTimeout(500); };
    const shot = async (p, name, full) => { const f = path.join(OUT, PREFIX + '-' + name + '.png'); await p.screenshot({ path: f, fullPage: !!full }); shots.push(f); };
    const both = async (name, fn) => {
        for (const [tag, p] of [['phone', phone], ['desktop', desk]]) {
            try { await fn(p, tag); await shot(p, name + '-' + tag); } catch (e) { console.log(`FAIL ${name} (${tag}): ${e.message.split('\n')[0]}`); errs.push(name + ': ' + e.message.split('\n')[0]); }
        }
    };

    const desk = await newPage(false);
    const phone = await newPage(true);

    // 1. the model picker on the Solo agent
    await both('model-picker', async (p) => {
        await nav(p, '/agent/1');
        await p.evaluate(() => window.Mantra.act.sheet({ kind: 'model', agent: 1 }));
        await p.waitForSelector('.pick-list', { timeout: 5000 });
        await p.waitForTimeout(300);
    });
    for (const p of [phone, desk]) { await p.keyboard.press('Escape'); await p.evaluate(() => window.Mantra.act.closeSheet()); }

    // 2. a mantra-swarm run: approve the plan, wait for the first shared note
    await desk.evaluate(() => window.Mantra.act.cmd('start_run', { goal: 'build a tiny todo CLI', pattern: 'mantra-swarm' }));
    await desk.waitForFunction(() => { const r = window.Mantra.store.S.run; return r && r.stage && r.stage.kind === 'review'; }, null, { timeout: 90000 });
    await nav(desk, '/run');
    await desk.click('button:has-text("Approve plan")');
    await desk.waitForFunction(() => { const r = window.Mantra.store.S.run; return r && (r.akasha || []).some((b) => (b.entries || []).length > 0); }, null, { timeout: 90000 });
    await desk.waitForTimeout(1500);
    await both('run-notes', async (p) => {
        await nav(p, '/run');
        await p.waitForSelector('.notes-card', { timeout: 5000 });
        if ((await p.getAttribute('.notes-toggle', 'aria-expanded')) !== 'true') await p.click('.notes-toggle');
        await p.waitForTimeout(300);
        await p.locator('.notes-card').scrollIntoViewIfNeeded();
        await p.waitForTimeout(300);
    });
    // 3. a bija worker's agent screen (on a phone the Context panel is a tab of its own)
    const worker = await desk.evaluate(() => { const w = [...window.Mantra.store.S.agents.values()].find((a) => a.role_kind === 'worker'); return w && w.id; });
    await both('worker', async (p) => { await nav(p, '/agent/' + worker); await p.waitForTimeout(800); });
    await both('worker-context', async (p, tag) => {
        await nav(p, '/agent/' + worker);
        const tab = p.locator('button:has-text("Context"), [role=tab]:has-text("Context")').first();
        if (await tab.count()) await tab.click();
        await p.waitForTimeout(400);
        const backend = p.locator('dt:has-text("Backend")').first();
        if (await backend.count()) await backend.scrollIntoViewIfNeeded();
        await p.waitForTimeout(300);
    });

    await b.close();
    console.log('SCREENSHOTS\n' + shots.join('\n'));
    console.log(errs.length ? 'ERRORS\n' + errs.join('\n') : 'no errors');
    process.exit(errs.length ? 1 : 0);
}

main();
