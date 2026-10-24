// Mantra web UI — screenshots for a visual review, phone (390×844) and desktop (1280×800): the
// start-run card with its pattern picker; Settings (the phone's list of sections, the desktop's
// one page), Models & providers, the Add-provider sheet (the key, then the models it found), the
// Patterns list and editor, MCP servers; the model picker; a run of `mantra-default` whose goal
// asks for a swarm, with its "Shared notes" open, a worker and its Context panel; the run screen
// with Stop while it runs, and with Delete and the next run's card once it is stopped; and the
// new pages again in the light theme. A sibling of web-e2e.mjs, run against the same kind of server:
//
//   MANTRA_HOME=$(mktemp -d) MANTRA_WEB_PASSWORD=test mantra --demo --headless --web --web-listen 127.0.0.1:7788
//   MANTRA_E2E_OUT=/tmp/shots node docs/tools/web-visual.mjs [prefix]
//   … node docs/tools/web-visual.mjs [prefix] mcp [pattern]   # MCP + screenshot cards (see below)
//
// It asserts nothing beyond what it needs to reach each screen; the shots are for a human.
// Env vars as in web-e2e.mjs (MANTRA_E2E_BASE, MANTRA_E2E_PASS, MANTRA_E2E_OUT, MANTRA_BIN,
// MANTRA_E2E_PW_MODULES).

import path from 'node:path';
import fs from 'node:fs';
import os from 'node:os';
import { spawn } from 'node:child_process';
import { fileURLToPath, pathToFileURL } from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const BASE = process.env.MANTRA_E2E_BASE || 'http://127.0.0.1:7788';
const PASSWORD = process.env.MANTRA_E2E_PASS || 'test';
const OUT = process.env.MANTRA_E2E_OUT || path.join(HERE, '..', '..', 'tmp-web-visual');
const PREFIX = (process.argv[2] || 'web').trim();
const BIN = process.env.MANTRA_BIN || '';

async function loadPlaywright() {
    try {
        return await import('playwright');
    } catch (e) {
        const dir = process.env.MANTRA_E2E_PW_MODULES;
        if (!dir) throw new Error('playwright is not installed: `npm i playwright`, or set MANTRA_E2E_PW_MODULES');
        return await import(pathToFileURL(path.join(dir, 'playwright', 'index.mjs')).href);
    }
}

// MANTRA_BIN: start the demo server these shots are taken of (a fresh home), and stop it after.
async function startServer() {
    if (await fetch(BASE + '/').then(() => true, () => false)) throw new Error(BASE + ' is taken already — stop what is listening there, or set MANTRA_E2E_BASE to a free address');
    const home = fs.mkdtempSync(path.join(os.tmpdir(), 'mantra-visual-'));
    const out = fs.openSync(path.join(home, 'mantra.log'), 'w');
    const env = Object.assign({}, process.env, { MANTRA_HOME: home, MANTRA_WEB_PASSWORD: PASSWORD });
    const child = spawn(BIN, ['--demo', '--headless', '--web', '--web-listen', new URL(BASE).host], { env, stdio: ['ignore', out, out] });
    process.on('exit', () => child.kill());
    for (let i = 0; i < 150; i++) {
        if (child.exitCode !== null) throw new Error('mantra exited before the web server came up — ' + path.join(home, 'mantra.log'));
        try { await fetch(BASE + '/'); return child; } catch (_) { await new Promise((r) => setTimeout(r, 200)); }
    }
    child.kill();
    throw new Error('timed out waiting for ' + BASE);
}

async function main() {
    const { chromium, devices } = await loadPlaywright();
    const server = BIN ? await startServer() : null;
    fs.mkdirSync(OUT, { recursive: true });
    const b = await chromium.launch();
    const errs = [];
    const shots = [];
    const newPage = async (phone, scheme) => {
        const ctx = await b.newContext(phone
            ? Object.assign({}, devices['iPhone 13'], { viewport: { width: 390, height: 844 }, deviceScaleFactor: 1, colorScheme: scheme || 'dark', serviceWorkers: 'block' })
            : { viewport: { width: 1280, height: 800 }, deviceScaleFactor: 1, colorScheme: scheme || 'dark', serviceWorkers: 'block' });
        const p = await ctx.newPage();
        p.on('pageerror', (e) => errs.push('pageerror: ' + e.message));
        p.on('console', (m) => { if (/Service Worker registration blocked/.test(m.text())) return; if (m.type() === 'error') errs.push('console.error: ' + m.text()); });
        await p.goto(BASE + '/');
        await p.waitForSelector('input[type=password]');
        await p.fill('input[type=password]', PASSWORD);
        await p.press('input[type=password]', 'Enter');
        await p.waitForSelector('.agent-row', { timeout: 15000 });
        // the install tip is for people, not for screenshots
        await p.evaluate(() => { window.Mantra.store.S.ui.banners.ios = true; window.Mantra.store.S.ui.banners.install = true; window.Mantra.act.changed(); });
        return p;
    };
    const nav = async (p, route) => { await p.evaluate((r) => window.Mantra.act.nav(r), route); await p.waitForTimeout(500); };
    const shot = async (p, name, full) => { const f = path.join(OUT, PREFIX + '-' + name + '.png'); await p.screenshot({ path: f, fullPage: !!full }); shots.push(f); };
    const pages = () => [['phone', phone], ['desktop', desk]];
    const both = async (name, fn, list) => {
        for (const [tag, p] of list || pages()) {
            try { await fn(p, tag); await shot(p, name + '-' + tag); } catch (e) { console.log(`FAIL ${name} (${tag}): ${e.message.split('\n')[0]}`); errs.push(name + ': ' + e.message.split('\n')[0]); }
        }
    };
    const request = (p, cmd, args, t) => p.evaluate(([c, a, t]) => window.Mantra.act.request(c, a, t), [cmd, args || {}, t || 30000]);

    const desk = await newPage(false);
    const phone = await newPage(true);

    // `mcp [pattern]`: a run whose workers call the demo's MCP server and take a screenshot (start
    // the server with MANTRA_SCREENSHOT_CMD set) — the run screen, and a worker's screen with its
    // `scene_info` card open and the screenshot's image marker.
    if (process.argv[3] === 'mcp') {
        const pattern = process.argv[4] || 'mantra-default';
        await desk.evaluate((pt) => window.Mantra.act.cmd('start_run', { goal: 'build a tiny todo CLI', pattern: pt }), pattern);
        await desk.waitForFunction(() => { const r = window.Mantra.store.S.run; return r && r.stage && r.stage.kind === 'review'; }, null, { timeout: 120000 });
        await nav(desk, '/run');
        await desk.click('button:has-text("Approve plan")');
        await desk.waitForFunction(() => [...window.Mantra.store.S.agents.values()].some((a) => a.role_kind === 'worker'), null, { timeout: 90000 });
        const w = await desk.evaluate(() => [...window.Mantra.store.S.agents.values()].find((a) => a.role_kind === 'worker').id);
        await nav(desk, '/agent/' + w);
        await desk.waitForSelector('.tool-image', { timeout: 90000 });
        await desk.waitForTimeout(1500);
        await both('mcp-run', async (p) => { await nav(p, '/run'); await p.waitForTimeout(600); });
        await both('mcp-worker', async (p) => {
            await nav(p, '/agent/' + w);
            await p.waitForSelector('.tool-image', { timeout: 10000 });
            const scene = p.locator('.item.tool .item-head:has-text("scene_info")').first();
            if (await scene.count()) { await scene.click(); await p.waitForTimeout(300); }
            await p.locator('.tool-image').first().scrollIntoViewIfNeeded();
            await p.waitForTimeout(400);
        });
        await b.close();
        if (server) server.kill();
        console.log('SCREENSHOTS\n' + shots.join('\n'));
        console.log(errs.length ? 'ERRORS\n' + errs.join('\n') : 'no errors');
        process.exit(errs.length ? 1 : 0);
    }

    // 1. the model picker on the Solo agent
    await both('model-picker', async (p) => {
        await nav(p, '/agent/1');
        await p.evaluate(() => window.Mantra.act.sheet({ kind: 'model', agent: 1 }));
        await p.waitForSelector('.pick-list', { timeout: 5000 });
        await p.waitForTimeout(300);
    });
    for (const p of [phone, desk]) { await p.keyboard.press('Escape'); await p.evaluate(() => window.Mantra.act.closeSheet()); }

    // 2. the start-run card: a goal typed, a pattern picked, a width of its own
    await both('start-run', async (p) => {
        await nav(p, '/');
        await p.fill('textarea.goal', 'Add OAuth login with GitHub, with tests');
        await p.selectOption('.start-row select', 'mantra-thorough');
        await p.fill('.par-wrap input', '6');
        await p.locator('.start-row').scrollIntoViewIfNeeded();
        await p.waitForTimeout(300);
    });
    for (const p of [phone, desk]) await p.evaluate(() => { const u = window.Mantra.store.S.ui; u.runGoal = ''; u.runParallel = ''; u.runPattern = ''; window.Mantra.act.changed(); });

    // 3. Settings: the phone's list of sections, the desktop's one page; then its biggest pages
    await both('settings', async (p) => { await nav(p, '/settings'); await p.waitForTimeout(400); });
    await both('settings-solo', async (p) => { await nav(p, '/settings/solo'); });
    await both('models', async (p) => { await nav(p, '/settings/models'); await p.waitForSelector('.mt.models .mt-row', { timeout: 8000 }); });
    await both('providers', async (p) => {
        await nav(p, '/settings/models');
        await p.locator('.mt.providers').scrollIntoViewIfNeeded();
        await p.waitForTimeout(300);
    });
    // a model's cell being edited, and its "More" fields
    await both('model-edit', async (p) => {
        await nav(p, '/settings/models');
        const row = p.locator('.mt.models [data-alias="luna"]');
        await row.locator('button[aria-label^="More fields"]').click();
        await row.locator('button[aria-label^="Model id:"]').click();
        await row.scrollIntoViewIfNeeded();
        await p.waitForTimeout(300);
    });
    for (const p of [phone, desk]) { await p.keyboard.press('Escape'); await p.evaluate(() => { window.Mantra.store.S.ui.cfg.open = {}; window.Mantra.act.changed(); }); }

    // 4. the Add-provider sheet: a key pasted (the URL guessed from it), then the models found on
    //    the demo's mock API (as localhost: a provider of its own, all its models new)
    const mock = (await request(desk, 'models_get')).providers.find((x) => x.id === 'bija').base_url.replace('127.0.0.1', 'localhost');
    await both('add-provider', async (p) => {
        await nav(p, '/settings/models');
        await p.click('button:has-text("Add provider")');
        await p.fill('#ap-key', 'sk-demo-00000000000000000000000000000000');
        await p.waitForFunction(() => { const i = document.querySelector('#ap-url'); return i && i.value.startsWith('https://'); }, null, { timeout: 5000 });
        await p.waitForTimeout(300);
    });
    await both('add-provider-models', async (p) => {
        await p.fill('#ap-key', 'demo-key');
        await p.fill('#ap-url', mock);
        await p.click('.sheet button:has-text("Probe")');
        await p.waitForSelector('.sheet .check-row', { timeout: 60000 });
        await p.waitForTimeout(300);
    });
    for (const p of [phone, desk]) await p.evaluate(() => window.Mantra.act.closeSheet());
    await request(desk, 'provider_delete', { id: 'local' }).catch(() => { });

    // 5. patterns: the list, the editor (a role card, then the settings and flow)
    await both('patterns', async (p) => { await nav(p, '/settings/patterns'); await p.waitForSelector('.pat-row', { timeout: 8000 }); });
    await both('pattern-editor', async (p) => { await nav(p, '/patterns/mantra-default'); await p.waitForSelector('.role-card', { timeout: 8000 }); });
    await both('pattern-editor-swarm', async (p) => {
        await nav(p, '/patterns/mantra-default');
        await p.waitForSelector('.role-card', { timeout: 8000 });
        await p.fill('input[aria-label="Workers at once of worker-swarm"]', '24');
        await p.locator('.role-card:has-text("worker-swarm")').scrollIntoViewIfNeeded();
        await p.waitForTimeout(300);
    });
    for (const p of [phone, desk]) await p.evaluate(() => { window.Mantra.store.S.ui.cfg.editor = null; });
    await both('mcp', async (p) => { await nav(p, '/settings/mcp'); });

    // 6. a run whose goal asks for a swarm (every built-in has the worker-swarm class now):
    //    approve the plan, wait for the first shared note
    await desk.evaluate(() => window.Mantra.act.cmd('start_run', { goal: 'build a tiny todo CLI, with a test per module written by a swarm', pattern: 'mantra-default' }));
    await desk.waitForFunction(() => { const r = window.Mantra.store.S.run; return r && r.stage && r.stage.kind === 'review'; }, null, { timeout: 90000 });
    await both('run-active', async (p) => { await nav(p, '/run'); await p.waitForTimeout(300); });
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
    // 7. a bija worker's agent screen (on a phone the Context panel is a tab of its own)
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

    // 8. Stop: the confirm sheet, then the stopped run — Delete, and the next run's card under it
    await both('run-stop-confirm', async (p) => {
        await nav(p, '/run');
        const live = await p.evaluate(() => { const r = window.Mantra.store.S.run; return !!(r && r.active); });
        if (live) await p.click('.run-hero button:has-text("Stop")');
        await p.waitForTimeout(300);
    });
    await phone.evaluate(() => window.Mantra.act.closeSheet());
    await desk.click('.sheet button:has-text("Stop run")').catch(async () => { await request(desk, 'run_stop').catch(() => { }); });
    await desk.waitForFunction(() => { const r = window.Mantra.store.S.run; return r && !r.active; }, null, { timeout: 20000 });
    await both('run-stopped', async (p) => { await nav(p, '/run'); await p.waitForSelector('.run-stopped, .run-hero button:has-text("Delete")', { timeout: 8000 }); await p.waitForTimeout(300); });
    await both('runs-list', async (p) => { await nav(p, '/runs'); await p.waitForTimeout(800); });

    // 9. the new pages in the light theme
    const lphone = await newPage(true, 'light'), ldesk = await newPage(false, 'light');
    const light = [['phone', lphone], ['desktop', ldesk]];
    await both('light-settings', async (p) => { await nav(p, '/settings'); }, light);
    await both('light-models', async (p) => { await nav(p, '/settings/models'); await p.waitForSelector('.mt.models .mt-row', { timeout: 8000 }); }, light);
    await both('light-pattern-editor', async (p) => { await nav(p, '/patterns/mantra-lean'); await p.waitForSelector('.role-card', { timeout: 8000 }); }, light);
    await both('light-run-stopped', async (p) => { await nav(p, '/run'); }, light);

    await b.close();
    if (server) server.kill();
    console.log('SCREENSHOTS\n' + shots.join('\n'));
    console.log(errs.length ? 'ERRORS\n' + errs.join('\n') : 'no errors');
    process.exit(errs.length ? 1 : 0);
}

main();
