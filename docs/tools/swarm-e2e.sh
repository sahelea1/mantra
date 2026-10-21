#!/usr/bin/env bash
# End-to-end check of the swarm pattern and Akasha: a whole `mantra-swarm` run on the real `bija`
# binary against the in-process mock model server that `mantra --demo` starts
# (src/mock_openai.rs), with Akasha on (the demo switches it on in memory). No API calls, no key,
# no cost. A sibling of bija-e2e.sh, which checks the plain bija run.
#
#   docs/tools/swarm-e2e.sh                     # release build, then the run, then PASS/FAIL
#   SWARM_E2E_SPEED=2 docs/tools/swarm-e2e.sh   # slower mock (the default is 8× demo speed)
#   SWARM_E2E_KEEP=1 docs/tools/swarm-e2e.sh    # keep the temp dir (MANTRA_HOME, demo repo) to look at
#
# What it proves, and how:
#   - the swarm run reaches Done              `"stage": "Done"` in the run's state.json
#   - it ran the swarm pattern                 the run's own pattern.toml is mantra-swarm, with its
#                                              `builders` group
#   - a worker shared a note through bija      a bija transcript holds a mantra_akasha_note call
#   - the note reached the group's block       state.json's `akasha` blocks hold at least one note
#                                              by an agent (not only Mantra's own lines)
#   - the Conductor journaled it               journal.jsonl has an `akasha · … noted #…` line
#   - nothing crashed                          no panic, no snapshot timeout
set -euo pipefail
cd "$(dirname "$0")/../../mantra_src"

target="${CARGO_TARGET_DIR:-$PWD/target}"
echo "building the workspace (release: mantra + bija)…"
cargo build --release -q
bin="$target/release/mantra"
for b in mantra bija; do
  [ -x "$target/release/$b" ] || { echo "FAIL: $target/release/$b was not built"; exit 1; }
done
command -v python3 >/dev/null || { echo "FAIL: python3 is needed to read state.json"; exit 1; }

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

echo "running a swarm on bija (mantra-swarm, Akasha on)…"
script="wait:0.5;until:plan review@120;key:a;until:run complete@600;snap:done"
set +e
out="$("$bin" --demo --pattern mantra-swarm --snapshot "$script" run "build a tiny todo CLI" 2>&1)"
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
  if grep -q '^name = "mantra-swarm"' "$run_dir/pattern.toml" 2>/dev/null && grep -q '^\[\[groups\]\]' "$run_dir/pattern.toml"; then
    pass "the run's pattern is mantra-swarm, with its group"
  else
    fail "the run's pattern.toml is not mantra-swarm with [[groups]]"
  fi
  # `akasha` in state.json is group name → block {group, cap, next_id, entries: [{id, author, at,
  # text, urgent}]}; Mantra's own lines are by `mantra`, the agents' by their names.
  notes="$(python3 -I - "$state" <<'PY'
import json, sys
st = json.load(open(sys.argv[1]))
blocks = (st.get("akasha") or {}).values()
entries = [e for b in blocks for e in b.get("entries", [])]
print(len([e for e in entries if e.get("author") != "mantra"]), len(entries))
PY
)" || notes="0 0"
  read -r by_agents total <<<"$notes"
  if [ "${by_agents:-0}" -ge 1 ]; then
    pass "$by_agents note(s) by agents in the run's shared notes ($total in all)"
  else
    fail "no note by an agent in state.json's akasha blocks ($total in all)"
  fi
  if grep -q 'akasha · .* noted #' "$run_dir/journal.jsonl" 2>/dev/null; then
    pass "the journal records the notes"
  else
    fail "no 'akasha · … noted #' line in $run_dir/journal.jsonl"
  fi
fi

sessions="$MANTRA_HOME/bija/sessions"
if grep -lq '"mantra_akasha_note"' "$sessions"/*/transcript.jsonl 2>/dev/null; then pass "a worker shared a note through bija"; else fail "no bija transcript holds a mantra_akasha_note call"; fi
if grep -lq '"write_file"' "$sessions"/*/transcript.jsonl 2>/dev/null; then pass "workers wrote files through bija"; else fail "no bija transcript holds a write_file call"; fi

if [ "$fails" -ne 0 ]; then
  tail -60 <<<"$out"
  echo "FAIL ($fails)"
  exit 1
fi
echo "PASS"
