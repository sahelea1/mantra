#!/usr/bin/env bash
# End-to-end check of the swarm worker class (and Akasha): a whole `mantra run --parallel 8` in
# `--demo`, on the default pattern, with a goal that asks for a swarm — so the demo planner
# (src/mock.rs, src/mock_openai.rs) cuts a phase of ten `worker-swarm` tasks, a test per module —
# and Akasha on (the demo switches it on in memory). No API calls, no key, no cost. A sibling of
# bija-e2e.sh, which checks the plain run on the real bija binary.
#
#   docs/tools/swarm-e2e.sh                     # release build, then the run, then PASS/FAIL
#   SWARM_E2E_SPEED=2 docs/tools/swarm-e2e.sh   # slower mock (the default is 8× demo speed)
#   SWARM_E2E_KEEP=1 docs/tools/swarm-e2e.sh    # keep the temp dir (MANTRA_HOME, demo repo) to look at
#
# What it proves, and how:
#   - the run reaches Done                    `"stage": "Done"` in the run's state.json
#   - --parallel 8 set every class            the run's own pattern.toml is mantra-default with
#                                             max_parallel = 8 and worker-swarm's parallel = 8, and
#                                             its `builders` group
#   - the planner used the class              plan.json has a phase of ≥ 8 worker-swarm tasks
#   - more than 4 ran at once                 the journal's `spawned … (worker-swarm · N/8 at once)`
#                                             lines reach N > 4, every one against the run's cap of 8
#   - a worker shared a note                  state.json's `akasha` blocks hold a note by an agent,
#                                             and the journal has an `akasha · … noted #` line
#   - nothing crashed                         no panic, no snapshot timeout
set -euo pipefail
cd "$(dirname "$0")/../../mantra_src"

target="${CARGO_TARGET_DIR:-$PWD/target}"
echo "building the workspace (release: mantra + bija)…"
cargo build --release -q
bin="$target/release/mantra"
for b in mantra bija; do
  [ -x "$target/release/$b" ] || { echo "FAIL: $target/release/$b was not built"; exit 1; }
done
command -v python3 >/dev/null || { echo "FAIL: python3 is needed to read the run's files"; exit 1; }

# One temp dir holds everything: MANTRA_HOME, and — through TMPDIR, which the demo repo is
# created under — the demo project, so the script can find it afterwards.
tmp="$(mktemp -d)"
if [ -z "${SWARM_E2E_KEEP:-}" ]; then
  trap 'rm -rf "$tmp"' EXIT
else
  echo "keeping $tmp"
fi
export MANTRA_HOME="$tmp/home" TMPDIR="$tmp" MANTRA_MOCK_SPEED="${SWARM_E2E_SPEED:-8}" LANG="${LANG:-en_US.UTF-8}"
mkdir -p "$MANTRA_HOME"

echo "running a swarm (mantra-default, --parallel 8, Akasha on)…"
script="wait:0.5;until:plan review@120;key:a;until:run complete@600;snap:done"
set +e
out="$("$bin" --demo --snapshot "$script" run "build a tiny todo CLI, its tests written by a swarm" --parallel 8 2>&1)"
code=$?
set -e

fails=0
pass() { echo "PASS: $*"; }
fail() { echo "FAIL: $*"; fails=$((fails + 1)); }

[ "$code" -eq 0 ] && pass "mantra exited 0" || fail "mantra exited $code"
# Here-strings, not `echo | grep -q`: under pipefail an early match would SIGPIPE the echo and
# read as no match.
if grep -qi 'panicked' <<<"$out"; then fail "a panic: $(grep -i -m1 'panicked' <<<"$out")"; else pass "no panic"; fi
if grep -q '!! timeout' <<<"$out"; then fail "the snapshot timed out: $(grep -m1 '!! timeout' <<<"$out")"; else pass "no snapshot timeout"; fi

state="$(ls "$MANTRA_HOME"/runs/*/*/state.json 2>/dev/null | head -1 || true)"
if [ -z "$state" ]; then
  fail "no run state under $MANTRA_HOME/runs"
else
  run_dir="$(dirname "$state")"
  if grep -q '"stage": "Done"' "$state"; then
    pass "the run reached Done ($state)"
  else
    fail "the run did not reach Done: $(grep -m1 '"stage"' "$state" || echo 'no stage')"
  fi
  # Every check below reads one of the run's own files; each prints `PASS: …` or `FAIL: …` lines.
  checks="$(python3 -I - "$run_dir" <<'PY'
import json, re, sys, tomllib
run = sys.argv[1]
def say(ok, what):
    print(("PASS: " if ok else "FAIL: ") + what)

try:
    p = tomllib.load(open(f"{run}/pattern.toml", "rb"))
    swarm = p.get("roles", {}).get("worker-swarm", {})
    groups = [g.get("name") for g in p.get("groups", [])]
    say(p.get("name") == "mantra-default" and p.get("settings", {}).get("max_parallel") == 8 and swarm.get("parallel") == 8 and "builders" in groups,
        f"the run's pattern is mantra-default with every class at 8 (max_parallel {p.get('settings', {}).get('max_parallel')}, worker-swarm {swarm.get('parallel')}, groups {groups})")
except Exception as e:
    say(False, f"the run's pattern.toml does not read: {e}")

try:
    plan = json.load(open(f"{run}/plan.json"))
    widest = max((sum(1 for t in ph.get("tasks", []) if t.get("role") == "worker-swarm") for ph in plan.get("phases", [])), default=0)
    say(widest >= 8, f"the plan has a phase of {widest} worker-swarm tasks (≥ 8)")
except Exception as e:
    say(False, f"plan.json does not read: {e}")

spawned = []
for line in open(f"{run}/journal.jsonl"):
    m = re.search(r"spawned \S+ \(worker-swarm · (\d+)/(\d+) at once\)", json.loads(line).get("text", ""))
    if m:
        spawned.append((int(m.group(1)), int(m.group(2))))
peak = max((n for n, _ in spawned), default=0)
caps = sorted({cap for _, cap in spawned})
say(peak > 4, f"{len(spawned)} swarm workers spawned, at most {peak} at once (> 4)")
say(caps == [8], f"every swarm spawn counted against the run's cap of 8 (caps seen: {caps})")

st = json.load(open(f"{run}/state.json"))
entries = [e for b in (st.get("akasha") or {}).values() for e in b.get("entries", [])]
by_agents = [e for e in entries if e.get("author") != "mantra"]
say(len(by_agents) >= 1, f"{len(by_agents)} note(s) by agents in the run's shared notes ({len(entries)} in all)")
noted = any(re.search(r"akasha · .* noted #", json.loads(l).get("text", "")) for l in open(f"{run}/journal.jsonl"))
say(noted, "the journal records the notes")
PY
)" || checks="FAIL: the run's files could not be checked"
  echo "$checks"
  fails=$((fails + $(grep -c '^FAIL' <<<"$checks" || true)))
fi

if [ "$fails" -ne 0 ]; then
  tail -60 <<<"$out"
  echo "FAIL ($fails)"
  exit 1
fi
echo "PASS"
