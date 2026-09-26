#!/usr/bin/env bash
# Web UI end-to-end check (WEB-DESIGN.md §13): builds mantra, runs it twice against a real
# `mantra --demo --headless --web` backend — once plain, once with the question-band mock env,
# since that only appears when the backend itself was started with MANTRA_MOCK_ASK=1
# MANTRA_MOCK_ASK_USER=1 (mock.rs) and can't be flipped on mid-run — and drives each with
# docs/tools/web-e2e.mjs (Playwright). Exits non-zero if either pass fails.
set -euo pipefail
cd "$(dirname "$0")/.."                          # → mantra_src/
REPO_ROOT="$(cd .. && pwd)"
JS="$REPO_ROOT/docs/tools/web-e2e.mjs"
[ -f "$JS" ] || { echo "web-e2e: missing $JS" >&2; exit 1; }

cargo build -q
bin=./target/debug/mantra
[ -x "$bin" ] || { echo "web-e2e: $bin missing after build" >&2; exit 1; }

ADDR=127.0.0.1:7788
BASE="http://$ADDR"
PASSWORD=test
overall=0

# Runs one `mantra --demo --headless --web …` process, waits for it to come up, drives it with
# the Playwright script in the given mode, then tears it down. Extra args ("$@" after $mode) are
# NAME=VALUE env assignments layered on top (used for the "ask" pass's mock env).
run_pass() {
    local mode="$1"; shift
    local home; home="$(mktemp -d)"
    local log; log="$(mktemp)"
    echo "== web-e2e: $mode pass (MANTRA_HOME=$home) =="

    env MANTRA_HOME="$home" MANTRA_MOCK_SPEED=4 "$@" \
        "$bin" --demo --headless --web "$ADDR" --web-password "$PASSWORD" >"$log" 2>&1 &
    local pid=$!

    # Headless mantra prints exactly one line to stderr on startup and nothing else (never spam
    # stdout/stderr in headless mode — WEB-DESIGN.md §1); poll the port instead of the log.
    local waited=0
    until curl -s -o /dev/null "$BASE/" 2>/dev/null; do
        if ! kill -0 "$pid" 2>/dev/null; then
            echo "web-e2e: mantra exited before the web server came up ($mode pass)"
            cat "$log"; rm -f "$log"; rm -rf "$home"
            return 1
        fi
        waited=$((waited + 1))
        if [ "$waited" -ge 150 ]; then
            echo "web-e2e: timed out waiting for $BASE/ ($mode pass)"
            cat "$log"; kill "$pid" 2>/dev/null || true; rm -f "$log"; rm -rf "$home"
            return 1
        fi
        sleep 0.2
    done

    local rc=0
    MANTRA_E2E_BASE="$BASE" MANTRA_E2E_PASS="$PASSWORD" node "$JS" "$mode" || rc=$?

    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
    echo "---- mantra log ($mode pass) ----"
    cat "$log"
    rm -f "$log"
    rm -rf "$home"
    rm -rf "${TMPDIR:-/tmp}/mantra-demo-$pid"      # demo_project() in main.rs keys it by this pid
    return $rc
}

run_pass main || overall=1
run_pass ask MANTRA_MOCK_ASK=1 MANTRA_MOCK_ASK_USER=1 || overall=1

if [ "$overall" -eq 0 ]; then
    echo "web-e2e: ALL PASSES GREEN"
else
    echo "web-e2e: AT LEAST ONE PASS FAILED"
fi
exit "$overall"
