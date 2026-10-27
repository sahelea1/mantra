// Browser-free view contract regressions. Run: node docs/tools/web-view-regression.mjs
// web-state-e2e.mjs covers browser rendering/scrolling; these also run on hosts without Chromium.
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import vm from 'node:vm';
import { fileURLToPath } from 'node:url';

const ui = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../../mantra_src/src/web/assets/ui');
const frames = [], requests = [];
const scope = { console, Date, Map, Set, requestAnimationFrame: (f) => frames.push(f),
    document: { querySelector: () => null }, localStorage: { getItem: () => null },
    setTimeout, clearTimeout };
scope.window = scope;
vm.createContext(scope);
for (const name of ['dom.js', 'fmt.js', 'store.js', 'parts.js', 'agent.js', 'screens.js']) {
    vm.runInContext(fs.readFileSync(path.join(ui, name), 'utf8'), scope, { filename: name });
}
const M = scope.Mantra, s = M.store.S;
M.act = {
    wide: () => true, motion: () => false, changed: () => {}, flash: () => {},
    nav: (url) => { s.ui.route = { name: 'agent', id: Number(url.split('/').at(-1)) }; },
    request: async (name, args) => { requests.push({ name, ...args }); return {}; },
    cmd: async (name, args) => { requests.push({ name, ...args }); return {}; },
};
function* nodes(v) {
    if (!v || typeof v !== 'object') return;
    if (Array.isArray(v)) { for (const x of v) yield* nodes(x); return; }
    yield v;
    for (const x of v.children || []) yield* nodes(x);
}
const find = (view, predicate) => [...nodes(view)].find(predicate);
const text = (v) => Array.isArray(v) ? v.map(text).join('') : typeof v === 'object' && v ? (v.children || []).map(text).join('') : String(v ?? '');
const hasClass = (v, cls) => (v.props.class || '').split(/\s+/).includes(cls);
const base = { status: 'idle', busy: false, items: [], items_first: 0, items_total: 0, created_at: Date.now(), in_run: true, model_alias: 'luna' };
s.agents.set(9, { ...base, id: 9, name: 'Planner', role_kind: 'planner' });
s.agents.set(7, { ...base, id: 7, name: 'Worker 7', role_kind: 'worker', worker: { task_id: 'p2-ui', state: 'running', attempt: 1 } });
s.order = [7, 9];
s.run = { id: 'test-run', planner: 9, stage: { kind: 'phase' }, workers: [{ task_id: 'p2-ui', agent: 7, attempt: 1 }] };
s.ui.route = { name: 'agent', id: 7 };
s.conn.state = 'open';
s.ui.drafts.c9 = 'Please adjust the plan';
let view = M.agentView.agentScreen(7);
assert.equal(find(view, (v) => v.tag === 'textarea').props['aria-label'], 'Message Planner');
assert.match(text(view), /Messages go to the planner/);
find(view, (v) => hasClass(v, 'send')).props.onclick();
await Promise.resolve();
assert.equal(requests.at(-1).agent, 9);
assert.equal(requests.at(-1).text, 'Please adjust the plan');

s.ui.drafts.c9 = '@worker';
view = M.agentView.agentScreen(7);
assert.equal(find(view, (v) => hasClass(v, 'suggest')), undefined, 'run composers do not suggest worker recipients');

Object.assign(s.agents.get(7), { status: 'failed', status_detail: 'old provider error' });
M.agentView.trackRespawn(7);
view = M.agentView.agentScreen(7);
assert.equal(find(view, (v) => hasClass(v, 'agent-trouble')), undefined);
assert.match(text(view), /Respawning Worker 7/);
const old = s.agents.get(7);
s.agents.set(8, { ...old, id: 8, name: 'Worker 8', status: 'busy', status_detail: undefined, worker: { ...old.worker, attempt: 2 } });
old.in_run = false; old.status = 'stopped';
s.run.workers = [{ task_id: 'p2-ui', agent: 8, attempt: 2 }];
M.agentView.agentScreen(7);
while (frames.length) frames.shift()();
assert.equal(s.ui.route.id, 8, 'a respawn opens the replacement attempt');
view = M.agentView.agentScreen(8);
assert.equal(find(view, (v) => hasClass(v, 'agent-trouble')), undefined);
assert.doesNotMatch(text(view), /old provider error|Respawning/);

s.app = { pattern_name: 'auto', pattern_list: [{ name: 'auto', builtin: true, description: 'Planner creates a pattern' }, { name: 'mantra-default', builtin: true }] };
view = M.screens.startRunCard();
assert.equal(find(view, (v) => v.tag === 'select').props.value, 'auto');
assert.match(text(find(view, (v) => v.tag === 'option')), /Auto.*planner creates the pattern/);

s.run.question = { text: 'Approve workflow change', since_at: Date.now(), from_name: 'Planner' };
s.run.pattern_review = true;
view = M.parts.questionBand(s.run);
find(view, (v) => v.tag === 'button' && text(v) === 'Approve workflow').props.onclick();
await Promise.resolve();
assert.equal(requests.at(-1).name, 'run_input');
assert.equal(requests.at(-1).text, 'approve');
console.log('web-view-regression: planner routing, recipient suggestions, respawn recovery, Auto picker and workflow approval passed');
