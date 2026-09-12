#!/bin/sh
# Mantra support bundle — everything needed to debug a problem, in one text file.
#
#   curl -fsSL https://raw.githubusercontent.com/sahelea1/mantra/master/support-bundle.sh | sh > mantra-bundle.txt
#
# Collects: versions, `mantra doctor`, login state of Codex and Claude Code (subscription
# or key — never the credentials themselves), OS/terminal facts, the sandbox knobs, your
# settings/models/patterns, Codex's config, the newest runs (state, journal, plan, phase
# outputs, merge logs) and the tail of mantra.log. Anything that looks like a key is
# redacted before it is printed: `api_key = "…"` values, *_API_KEY=… variables, Bearer
# tokens and sk-… strings. Nothing is uploaded — read the file before you share it.
#
#   MANTRA_HOME   honoured (default ~/.mantra)     BUNDLE_RUNS   runs to include (default 3)
#   MANTRA_BIN    the mantra binary, if not on PATH  BUNDLE_LOG    log lines to include (default 1500)

H="${MANTRA_HOME:-$HOME/.mantra}"
CH="${CODEX_HOME:-$HOME/.codex}"
CLH="${CLAUDE_CONFIG_DIR:-$HOME/.claude}"
RUNS="${BUNDLE_RUNS:-3}"
LOGN="${BUNDLE_LOG:-1500}"

# the mantra binary: $MANTRA_BIN, then PATH, then a local cargo build
MB="${MANTRA_BIN:-mantra}"
if ! command -v "$MB" >/dev/null 2>&1; then
  for c in ./mantra_src/target/release/mantra ./mantra_src/target/debug/mantra ./target/release/mantra "$HOME/.cargo/bin/mantra"; do
    [ -x "$c" ] && { MB="$c"; break; }
  done
fi

redact() {
  sed -E \
    -e 's/(api_key[[:space:]]*=[[:space:]]*")[^"]*(")/\1<redacted>\2/g' \
    -e 's/([A-Za-z0-9_]*API_KEY[A-Za-z0-9_]*[=:][[:space:]]*)[^[:space:]"]+/\1<redacted>/g' \
    -e 's/(Bearer[[:space:]]+)[A-Za-z0-9._-]+/\1<redacted>/g' \
    -e 's/sk-[A-Za-z0-9_-]{8,}/sk-<redacted>/g'
}
section() { printf '\n\n═══════════ %s ═══════════\n' "$1"; }
show() { if [ -f "$1" ]; then cat "$1"; else echo "(no $1)"; fi; }
have() { if [ -e "$1" ]; then echo "present"; else echo "absent"; fi; }

{
  echo "mantra support bundle · $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "MANTRA_HOME=$H  CODEX_HOME=$CH  CLAUDE_CONFIG_DIR=$CLH  cwd=$(pwd)"

  section "versions"
  printf 'mantra: '; command -v "$MB" >/dev/null 2>&1 && "$MB" --version 2>&1 || echo "NOT FOUND (not on PATH; set MANTRA_BIN=/path/to/mantra)"
  printf '        '; command -v "$MB" 2>&1
  printf 'codex:  '; codex --version 2>&1 | head -1
  printf '        '; command -v codex 2>&1
  printf 'claude: '; claude --version 2>&1 | head -1
  printf '        '; command -v claude 2>&1
  printf 'node:   '; node --version 2>&1
  printf 'git:    '; git --version 2>&1
  printf 'rust:   '; cargo --version 2>&1 | head -1

  section "mantra doctor"
  "$MB" doctor 2>&1

  section "login (subscription / key) — no credentials are printed"
  printf 'codex login status:  '; codex login status 2>&1 | head -3
  echo "codex auth.json:     $(have "$CH/auth.json")"
  echo "claude auth status:"; claude auth status 2>&1 | head -12
  echo "claude credentials:  $(have "$CLH/.credentials.json")  (~/.claude.json: $(have "$HOME/.claude.json"))"
  echo "keys in env:         $(env | grep -E '^[A-Za-z0-9_]*API_KEY=' | cut -d= -f1 | tr '\n' ' ')"

  section "system"
  uname -a
  [ -f /etc/os-release ] && head -4 /etc/os-release
  [ "$(uname -s)" = Darwin ] && sw_vers 2>/dev/null
  echo "uid=$(id -u)  SHELL=${SHELL:-}  LANG=${LANG:-}  LC_ALL=${LC_ALL:-}"
  echo "TERM=${TERM:-}  COLORTERM=${COLORTERM:-}  TERM_PROGRAM=${TERM_PROGRAM:-}  TMUX=${TMUX:+yes}  SSH=${SSH_CONNECTION:+yes}"
  echo "rows cols: $( (stty size </dev/tty) 2>/dev/null || echo '?')"
  command -v tmux >/dev/null 2>&1 && { tmux -V 2>&1; tmux show-options -s escape-time 2>/dev/null; }
  echo "locale -a: $(locale -a 2>/dev/null | grep -ci utf) UTF-8 locales"

  section "sandbox (Linux user namespaces)"
  for k in unprivileged_userns_clone apparmor_restrict_unprivileged_userns; do
    [ -f "/proc/sys/kernel/$k" ] && echo "$k = $(cat /proc/sys/kernel/$k)"
  done
  if command -v unshare >/dev/null 2>&1; then
    if unshare -U true 2>&1; then echo "unshare -U: ok"; else echo "unshare -U: FAILED (Codex's bwrap sandbox cannot start here)"; fi
  fi
  command -v bwrap >/dev/null 2>&1 && bwrap --version 2>&1
  [ -n "${IS_SANDBOX:-}" ] && echo "IS_SANDBOX=$IS_SANDBOX"

  section "$H/settings.toml"; show "$H/settings.toml"
  section "$H/models.toml (keys redacted)"; show "$H/models.toml"
  section "$H/patterns"
  ls -la "$H/patterns" 2>/dev/null || echo "(none)"
  for f in "$H"/patterns/*.toml; do [ -f "$f" ] && { echo "--- $f"; cat "$f"; }; done
  section "$H tree"
  ls -la "$H" "$H/run" "$H/worktrees" 2>&1

  section "$CH/config.toml (Codex)"; show "$CH/config.toml"

  section "runs"
  "$MB" runs 2>&1
  # newest N run directories by mtime
  for d in $(ls -dt "$H"/runs/*/*/ 2>/dev/null | head -n "$RUNS"); do
    echo; echo "──────── $d"; ls -la "$d" 2>&1
    echo "--- brief.md"; show "$d/brief.md"
    echo "--- state.json"; show "$d/state.json"
    echo "--- pattern.toml"; show "$d/pattern.toml"
    echo "--- plan.md (head)"; [ -f "$d/plan.md" ] && head -n 150 "$d/plan.md"
    echo "--- journal.jsonl"; show "$d/journal.jsonl"
    for p in "$d"/phase-*/*.md "$d"/phase-*-merge.log; do
      [ -f "$p" ] && { echo "--- $p (head)"; head -n 100 "$p"; }
    done
  done

  section "$H/logs/mantra.log (last $LOGN lines)"
  [ -f "$H/logs/mantra.log" ] && { ls -la "$H/logs/mantra.log"; tail -n "$LOGN" "$H/logs/mantra.log"; } || echo "(no log)"

  section "codex log (last 200 lines)"
  for f in "$CH"/log/codex-tui.log "$CH"/log/codex.log; do
    [ -f "$f" ] && { echo "--- $f"; tail -n 200 "$f"; }
  done

  section "end"
} 2>&1 | redact
