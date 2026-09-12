//! Claude Code backend (WP10): one long-lived `claude -p --input-format stream-json …` process
//! per agent, translated into the same Codex-shaped `HubEvent::Notif` methods `Agent::apply`
//! (`agent.rs`) already understands, so the engine and UI need no backend-specific branches (the
//! "one seam" rule in `v02plan.md` §0.1). The wire protocol this file implements is documented in
//! full in `v02plan.md` §10.1 (verified live against Claude Code 2.1.269 + LibertAI); a recorded
//! sample of the exact event shapes lives in `src/testdata/claude-stream.jsonl` and is exercised by
//! the test at the bottom of this file.
//!
//! WP10.4 adds the MCP bridge on top: when the Hub has a bridge socket (`hub::bridge_sock`,
//! non-`None` only while a `ClaudeCode` provider exists), every Claude agent gets
//! `--strict-mcp-config --mcp-config '…'` pointing `claude` at `mantra mcp-bridge --sock <path>
//! --agent <id>`, spawned by `claude` itself as an MCP stdio server. That subprocess connects back
//! to the same socket and identifies itself; `Hub`'s accept loop (`hub.rs`) routes the resulting
//! `Cmd::Bridge(UnixStream)` to *this* agent's own command channel (never a shared listener the
//! per-agent task can't reach — see `v02plan.md` §10.4), and this file owns the connection from
//! there: it answers `{"list":true}` with the role's tool array (already sitting in
//! `spec.dynamic_tools`, the very same JSON schemas Codex's `dynamicTools` gets) and turns
//! `{"call":…,"tool":…,"args":…}` into the same `HubEvent::Request{method:"item/tool/call"}` the
//! Codex path already produces, so `App::on_request`/`Run::on_tool_call` need no changes at all —
//! only `Cmd::Respond`/`RespondErr` whose id starts with `cc-call-` get routed back down to the
//! bridge instead of being Codex-only no-ops.

use super::{AgentId, Cmd, Exit, HubEvent, SpawnSpec};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::process::{ChildStderr, ChildStdout, Command};
use tokio::sync::mpsc;

/// Claude-only spawn configuration, resolved by `app::spawn_agent` from the model's `ProviderEntry`
/// (kept out of `hub` proper so the hub stays decoupled from `config::Registry` — see `SpawnSpec`).
#[derive(Debug, Clone)]
pub struct ClaudeSpawn {
    /// "subscription" (OAuth login, no `--bare`, no `ANTHROPIC_*`) | "api_key" (`--bare` +
    /// `ANTHROPIC_API_KEY` from `SpawnSpec.envs`, `ANTHROPIC_BASE_URL` when `base_url` is set).
    pub auth: String,
    /// Third-party gateway base URL for `auth == "api_key"`; empty = the real Anthropic API.
    /// Normalized to drop a trailing `/v1` before use (Claude wants the bare host, unlike Codex's
    /// custom providers which include it).
    pub base_url: String,
    /// Effective context window (WP2) passed as `--autocompact`.
    pub autocompact: u64,
    /// Role instructions, passed via `--append-system-prompt`.
    pub system_prompt: String,
    /// The Hub's MCP-bridge socket path (WP10.4), if it has one — `hub::Hub::bridge_sock()`.
    /// `None` means no `ClaudeCode` provider existed at startup; the agent then runs with no
    /// `mantra_*` dynamic tools at all rather than pointing `--mcp-config` at a socket nothing
    /// listens on.
    pub mcp_sock: Option<PathBuf>,
}

/// Runs one `claude` process for the lifetime of a single spawn attempt (a crash or a model/effort
/// change returns `Exit::Crashed` and `agent_task` restarts it with `--resume`, mirroring
/// `run_process`'s contract exactly — same `thread_id`-shaped slot doubles as the session id).
pub(super) async fn run_claude_process(
    id: AgentId,
    spec: &SpawnSpec,
    claude_cmd: &[String],
    ev: &mpsc::UnboundedSender<HubEvent>,
    cmds: &mut mpsc::UnboundedReceiver<Cmd>,
    session_id: &mut Option<String>,
    effort: &mut String,
    model: &mut String,
    is_restart: bool,
) -> Exit {
    let cs = match spec.claude.clone() {
        Some(c) => c,
        None => return Exit::Crashed("internal error: Claude backend spawned without ClaudeSpawn config".into()),
    };
    let Some((prog, base_args)) = claude_cmd.split_first() else {
        return Exit::Crashed("empty claude command".into());
    };
    let resume_id = session_id.clone();
    let sid = resume_id.clone().unwrap_or_else(crate::util::uuid_v4);
    let args = build_args(id, spec, &cs, model, effort, &sid, resume_id.is_some(), base_args);

    let mut command = Command::new(prog);
    command.args(&args).current_dir(&spec.cwd).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    apply_env(&mut command, spec, &cs);

    crate::mlog!("claude {id}: spawn {} {}", prog, args.join(" "));
    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => return Exit::Crashed(format!("failed to start `{}`: {e}", claude_cmd.join(" "))),
    };
    crate::mlog!("claude {id}: spawned pid={:?}", child.id());
    let (Some(stdin), Some(stdout), Some(stderr)) = (child.stdin.take(), child.stdout.take(), child.stderr.take()) else {
        let _ = child.kill().await;
        return Exit::Crashed("claude: missing stdio pipes".into());
    };
    let mut stdin = stdin;
    let (line_tx, mut line_rx) = mpsc::unbounded_channel::<LineIn>();
    tokio::spawn(read_lines(stdout, line_tx));
    let stderr_tail: Arc<Mutex<VecDeque<String>>> = Arc::new(Mutex::new(VecDeque::new()));
    tokio::spawn(read_stderr(stderr, stderr_tail.clone()));

    let mut tr = Translator::new(id, cs.autocompact, cs.auth != "api_key");
    // MCP bridge state (WP10.4): `None` until a `Cmd::Bridge` hands over a live connection (or after
    // it closes / this process is about to be replaced by a restart). `bridge_rx` is polled in the
    // select loop below only while it's `Some` (`recv_bridge`).
    let mut bridge_write: Option<OwnedWriteHalf> = None;
    let mut bridge_rx: Option<mpsc::UnboundedReceiver<BridgeIn>> = None;

    loop {
        tokio::select! {
            c = cmds.recv() => {
                match c {
                    None | Some(Cmd::Shutdown) => {
                        if tr.turn_open {
                            let _ = write_line(&mut stdin, &control_request("shutdown")).await;
                            tokio::time::sleep(Duration::from_millis(300)).await;
                        }
                        let _ = child.kill().await;
                        return Exit::Shutdown;
                    }
                    Some(Cmd::Turn { text }) | Some(Cmd::Steer { text }) => {
                        // Claude does its own queueing: a line written mid-turn is injected at the
                        // next tool boundary (verified, §10.1) — Turn and Steer are the same wire op.
                        if write_line(&mut stdin, &user_line(&text)).await.is_err() {
                            let _ = ev.send(HubEvent::CmdFailed { agent: id, what: "turn", error: "failed to write to claude's stdin".into(), text: Some(text) });
                        }
                    }
                    Some(Cmd::Interrupt) => {
                        // Mirror the Codex path (`hub.rs`'s `current_turn`-gated `turn/interrupt`):
                        // only fire when a turn is actually open, and remember the request id so the
                        // matching `control_response` — not just any later one — is what arms
                        // `interrupt_pending`. Otherwise a stray/idle-time interrupt (e.g. the
                        // unguarded 'x' keybinding) could bleed into an unrelated later turn's
                        // completion status. See `Translator::on_line`.
                        if tr.turn_open {
                            let reqid = format!("int-{}", crate::util::unix_secs());
                            tr.pending_interrupt_request = Some(reqid.clone());
                            let _ = write_line(&mut stdin, &control_request(&reqid)).await;
                        }
                    }
                    Some(Cmd::Compact) => { let _ = write_line(&mut stdin, &user_line("/compact")).await; }
                    Some(Cmd::Archive) => {} // the session file stays on disk; nothing to do until Shutdown
                    Some(Cmd::Shell { command }) => {
                        let _ = ev.send(HubEvent::CmdFailed { agent: id, what: "shell", error: "the Claude Code backend doesn't support raw shell commands".into(), text: Some(command) });
                    }
                    Some(Cmd::SetEffort(e)) => {
                        *effort = e;
                        if !tr.turn_open {
                            let _ = child.kill().await;
                            return Exit::Crashed("restarting to apply the new effort".into());
                        }
                        tr.restart_pending = true; // applied at the next idle moment, never mid-turn
                    }
                    Some(Cmd::SetModel(m)) => {
                        *model = m;
                        if !tr.turn_open {
                            let _ = child.kill().await;
                            return Exit::Crashed("restarting to apply the new model".into());
                        }
                        tr.restart_pending = true;
                    }
                    Some(Cmd::SetApproval(_)) => {
                        // Only "never" (--dangerously-skip-permissions) is modeled until Role.permission
                        // (WP3) lands in a later batch; every Claude agent runs unattended for now.
                    }
                    Some(Cmd::Restart) => {
                        let _ = child.kill().await;
                        return Exit::Crashed("restart requested".into());
                    }
                    Some(Cmd::Respond { id: rid, result }) => {
                        if let Some(call) = rid.as_str().and_then(|s| s.strip_prefix("cc-call-")) {
                            let success = result.get("success").and_then(|s| s.as_bool()).unwrap_or(true);
                            let text = result.pointer("/contentItems/0/text").and_then(|t| t.as_str()).unwrap_or("");
                            reply_bridge_call(&mut bridge_write, call, success, text, id).await;
                        }
                        // else: not a bridge call id — nothing else on the Claude backend consumes
                        // `Cmd::Respond` yet (there's no other kind of request it ever originates).
                    }
                    Some(Cmd::RespondErr { id: rid, message }) => {
                        if let Some(call) = rid.as_str().and_then(|s| s.strip_prefix("cc-call-")) {
                            reply_bridge_call(&mut bridge_write, call, false, &message, id).await;
                        }
                    }
                    Some(Cmd::Bridge(stream)) => {
                        let (rd, wr) = stream.into_split();
                        let (btx, brx) = mpsc::unbounded_channel();
                        tokio::spawn(read_bridge_lines(rd, btx));
                        bridge_write = Some(wr);
                        bridge_rx = Some(brx);
                        crate::mlog!("agent {id}: mcp bridge connected");
                    }
                }
            }
            b = recv_bridge(&mut bridge_rx), if bridge_rx.is_some() => {
                match b {
                    Some(BridgeIn::Line(v)) => {
                        if v.get("list").and_then(|x| x.as_bool()) == Some(true) {
                            if let Some(w) = bridge_write.as_mut() {
                                let _ = write_line(w, &mcp_tool_list(&spec.dynamic_tools)).await;
                            }
                        } else if let Some(call) = v.get("call").and_then(|c| c.as_str()) {
                            let tool = v.get("tool").and_then(|t| t.as_str()).unwrap_or("").to_string();
                            let args = v.get("args").cloned().unwrap_or_else(|| json!({}));
                            let _ = ev.send(HubEvent::Request { agent: id, id: json!(format!("cc-call-{call}")), method: "item/tool/call".into(), params: json!({"tool": tool, "arguments": args}) });
                        }
                    }
                    Some(BridgeIn::Closed) | None => {
                        crate::mlog!("agent {id}: mcp bridge connection closed");
                        bridge_write = None;
                        bridge_rx = None;
                    }
                }
            }
            l = line_rx.recv() => {
                match l {
                    Some(LineIn::Value(v)) => {
                        if !is_heartbeat(&v) {
                            crate::mlog!("claude {id}: {}", crate::util::trunc(&v.to_string(), 200));
                        }
                        for e in tr.on_line(&v, is_restart) {
                            if let HubEvent::Ready { thread_id, .. } = &e {
                                *session_id = Some(thread_id.clone());
                            }
                            let _ = ev.send(e);
                        }
                        if let Some(reason) = tr.fatal_auth.take() {
                            let _ = child.kill().await;
                            return Exit::Crashed(reason);
                        }
                        if tr.restart_pending && !tr.turn_open {
                            let _ = child.kill().await;
                            return Exit::Crashed("restarting to apply the new model/effort".into());
                        }
                    }
                    Some(LineIn::Closed) | None => {
                        let code = super::reap_exit_code(&mut child).await;
                        let code_s = code.map(|c| c.to_string()).unwrap_or_else(|| "unknown".into());
                        let tail = stderr_tail.lock().map(|t| t.iter().cloned().collect::<Vec<_>>().join(" / ")).unwrap_or_default();
                        return Exit::Crashed(if tail.trim().is_empty() { format!("claude exited (code {code_s})") } else { format!("claude exited (code {code_s}): {tail}") });
                    }
                }
            }
        }
    }
}

/// The CLI's own progress ticks (`system/thinking_tokens` every second while the model thinks,
/// `tool_progress` while a tool runs): folded into activity, never worth a log line each.
fn is_heartbeat(v: &Value) -> bool {
    match v.get("type").and_then(|x| x.as_str()) {
        Some("tool_progress") => true,
        Some("system") => v.get("subtype").and_then(|x| x.as_str()) == Some("thinking_tokens"),
        _ => false,
    }
}

fn control_request(request_id: &str) -> Value {
    json!({"type": "control_request", "request_id": request_id, "request": {"subtype": "interrupt"}})
}

fn user_line(text: &str) -> Value {
    json!({"type": "user", "message": {"role": "user", "content": text}})
}

/// Writes one NDJSON line and flushes — used for both `claude`'s stdin and a live bridge write half
/// (`OwnedWriteHalf`), so the MCP-bridge reply path (WP10.4) doesn't need its own copy.
async fn write_line<W: AsyncWriteExt + Unpin>(w: &mut W, v: &Value) -> std::io::Result<()> {
    w.write_all(v.to_string().as_bytes()).await?;
    w.write_all(b"\n").await?;
    w.flush().await
}

/// Answers one pending bridge call: `{"call": <n>, "ok": <bool>, "text": <string>}`, per §10.4's
/// wire shape between `mantra mcp-bridge` and the Hub. Silently dropped (logged) if the bridge
/// connection isn't there right now — the restart race §10.4 calls out explicitly: the bridge
/// subprocess sees its request time out and the agent gets re-prompted by the existing
/// `CmdFailed` path once `claude` itself reports the tool call failed.
async fn reply_bridge_call(bridge_write: &mut Option<OwnedWriteHalf>, call: &str, ok: bool, text: &str, agent: AgentId) {
    match bridge_write.as_mut() {
        Some(w) => {
            let _ = write_line(w, &json!({"call": call, "ok": ok, "text": text})).await;
        }
        None => crate::mlog!("agent {agent}: mcp bridge reply for call {call} dropped (no bridge connection)"),
    }
}

/// One line read from a live bridge connection: either `{"list":true}` (the bridge's `tools/list`
/// asking for this role's tool array) or `{"call":"<n>","tool":"<name>","args":{…}}` (a `tools/call`
/// to forward to the engine as `HubEvent::Request`) — see the module doc comment and §10.4.
enum BridgeIn {
    Line(Value),
    Closed,
}

async fn read_bridge_lines(rd: OwnedReadHalf, tx: mpsc::UnboundedSender<BridgeIn>) {
    let mut lines = BufReader::new(rd).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(l)) => {
                let l = l.trim();
                if l.is_empty() {
                    continue;
                }
                match serde_json::from_str::<Value>(l) {
                    Ok(v) => {
                        if tx.send(BridgeIn::Line(v)).is_err() {
                            return;
                        }
                    }
                    Err(e) => crate::mlog!("bad json from mcp-bridge: {e}: {}", crate::util::trunc(l, 200)),
                }
            }
            Ok(None) | Err(_) => break,
        }
    }
    let _ = tx.send(BridgeIn::Closed);
}

/// `bridge_rx.recv()` when there's a live connection, else pends forever so the `tokio::select!`
/// arm polling it (guarded by `if bridge_rx.is_some()`) simply never fires — the same "optional
/// branch" pattern `hub.rs` doesn't need because Codex has no equivalent side channel.
async fn recv_bridge(rx: &mut Option<mpsc::UnboundedReceiver<BridgeIn>>) -> Option<BridgeIn> {
    match rx {
        Some(r) => r.recv().await,
        None => std::future::pending().await,
    }
}

/// The role's tool array (already sitting in `SpawnSpec::dynamic_tools` — the same JSON schemas
/// Codex's `dynamicTools` gets) reshaped into an MCP `tools/list` result: `{name, description,
/// inputSchema}` per tool, dropping the Codex-only `type: "function"` wrapper (§10.4).
fn mcp_tool_list(dynamic_tools: &[Value]) -> Value {
    let tools: Vec<Value> = dynamic_tools
        .iter()
        .map(|t| {
            json!({
                "name": t.get("name").cloned().unwrap_or(Value::Null),
                "description": t.get("description").cloned().unwrap_or(Value::Null),
                "inputSchema": t.get("inputSchema").cloned().unwrap_or_else(|| json!({"type": "object"})),
            })
        })
        .collect();
    json!({"tools": tools})
}

/// `-p --input-format stream-json --output-format stream-json --verbose --dangerously-skip-permissions
/// --session-id|--resume <id> --model <m> [--effort e] [--autocompact n] [--append-system-prompt ...]
/// [--strict-mcp-config --mcp-config '…'] <sandbox args> [--bare]` (§10.1, §10.4).
fn build_args(id: AgentId, spec: &SpawnSpec, cs: &ClaudeSpawn, model: &str, effort: &str, session_id: &str, resume: bool, base_args: &[String]) -> Vec<String> {
    let mut a: Vec<String> = base_args.to_vec();
    a.extend(["-p".into(), "--input-format".into(), "stream-json".into(), "--output-format".into(), "stream-json".into(), "--verbose".into()]);
    // Only "never" is modeled until Role.permission (WP3) lands: every Claude agent is unattended.
    a.push("--dangerously-skip-permissions".into());
    a.push(if resume { "--resume".into() } else { "--session-id".into() });
    a.push(session_id.to_string());
    a.push("--model".into());
    a.push(model.to_string());
    if !effort.is_empty() {
        a.push("--effort".into());
        a.push(effort.to_string());
    }
    if cs.autocompact > 0 {
        a.push("--autocompact".into());
        a.push(cs.autocompact.to_string());
    }
    let mcp = cs.mcp_sock.as_ref().filter(|_| !spec.dynamic_tools.is_empty());
    if !cs.system_prompt.is_empty() || mcp.is_some() {
        let mut sp = cs.system_prompt.clone();
        if mcp.is_some() {
            // Tell the agent what its `mantra_*` tools are actually called once exposed as MCP
            // tools (§10.4) — the role protocol text (`engine/tools.rs`) only knows the bare names.
            sp.push_str("\n\nYour mantra_* tools are exposed as MCP tools named `mcp__mantra__<name>` (e.g. `mcp__mantra__mantra_status`). Call them by that full name.");
        }
        a.push("--append-system-prompt".into());
        a.push(sp);
    }
    if let Some(sock) = mcp {
        a.push("--strict-mcp-config".into());
        a.push("--mcp-config".into());
        a.push(mcp_config_json(id, sock).to_string());
    }
    a.extend(sandbox_args(&spec.sandbox, &spec.cwd));
    if cs.auth == "api_key" {
        a.push("--bare".into());
    }
    a
}

/// `{"mcpServers":{"mantra":{"command":"<this mantra binary>","args":["mcp-bridge","--sock",<sock>,
/// "--agent",<id>]}}}` — spawned by `claude` itself as a stdio MCP server (§10.1, §10.4). Falls back
/// to the literal `"mantra"` on the vanishingly unlikely chance `current_exe()` fails; `claude` would
/// then report that server as failed to start, which is a much clearer signal than silently
/// skipping `--mcp-config` and leaving the agent's tool calls unexplained.
fn mcp_config_json(id: AgentId, sock: &Path) -> Value {
    let exe = std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| "mantra".into());
    json!({"mcpServers": {"mantra": {"command": exe, "args": ["mcp-bridge", "--sock", sock.to_string_lossy(), "--agent", id.to_string()]}}})
}

/// Sandbox → Claude's own tool restrictions (git worktrees still isolate workers regardless).
fn sandbox_args(sandbox: &str, cwd: &Path) -> Vec<String> {
    match sandbox {
        "read-only" => vec!["--tools".into(), "Read,Glob,Grep,WebFetch".into(), "--disallowedTools".into(), "Edit,Write,Bash".into()],
        "danger-full-access" => vec!["--add-dir".into(), "/".into()],
        _ => vec!["--add-dir".into(), cwd.to_string_lossy().into_owned()],
    }
}

/// Strip a stored `.../v1` suffix: Codex-style provider base URLs include it, but
/// `ANTHROPIC_BASE_URL` wants the bare host (verified: `https://api.libertai.io`, not `.../v1`).
fn strip_v1(base_url: &str) -> String {
    let b = base_url.trim().trim_end_matches('/');
    b.strip_suffix("/v1").unwrap_or(b).to_string()
}

/// Child environment: drop every `CLAUDE*`/`ANTHROPIC_*` var inherited from Mantra's own process
/// (in particular `CLAUDE_CODE_SESSION_ID`, which overrides `--session-id` — observed), then set
/// only what this agent needs.
fn apply_env(command: &mut Command, spec: &SpawnSpec, cs: &ClaudeSpawn) {
    for (k, _) in std::env::vars() {
        if k.starts_with("CLAUDE") || k.starts_with("ANTHROPIC_") {
            command.env_remove(&k);
        }
    }
    command.env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1");
    if crate::util::effective_uid_is_root() {
        // `--dangerously-skip-permissions` is refused for uid 0 without this (verified).
        command.env("IS_SANDBOX", "1");
    }
    if cs.auth == "api_key" && !cs.base_url.is_empty() {
        command.env("ANTHROPIC_BASE_URL", strip_v1(&cs.base_url));
    }
    // `app::spawn_agent` resolves the key (via the provider's `env_key`) into here, so `hub::claude`
    // never has to read `config::ProviderEntry` itself — never on argv, never logged.
    for (k, v) in &spec.envs {
        command.env(k, v);
    }
}

enum LineIn {
    Value(Value),
    Closed,
}

async fn read_lines(stdout: ChildStdout, tx: mpsc::UnboundedSender<LineIn>) {
    let mut lines = BufReader::with_capacity(1 << 16, stdout).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                match serde_json::from_str::<Value>(line) {
                    Ok(v) => {
                        if tx.send(LineIn::Value(v)).is_err() {
                            return;
                        }
                    }
                    // Non-JSON lines (e.g. `[claude-code:unrecognized_model] …`) land on stderr in
                    // practice, but tolerate one on stdout too rather than crash the reader.
                    Err(e) => crate::mlog!("bad json from claude: {e}: {}", crate::util::trunc(line, 200)),
                }
            }
            Ok(None) | Err(_) => break,
        }
    }
    let _ = tx.send(LineIn::Closed);
}

async fn read_stderr(stderr: ChildStderr, tail: Arc<Mutex<VecDeque<String>>>) {
    const TAIL_LINES: usize = 5;
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(l)) = lines.next_line().await {
        crate::mlog!("[claude stderr] {}", crate::util::trunc(&l, 400));
        let clean = crate::util::strip_ansi(&l);
        if !clean.trim().is_empty() {
            if let Ok(mut t) = tail.lock() {
                t.push_back(clean);
                while t.len() > TAIL_LINES {
                    t.pop_front();
                }
            }
        }
    }
}

/// Folds one `claude` NDJSON line at a time into the Codex-shaped `HubEvent`s `Agent::apply`
/// already understands. Pure (no I/O), so it's fully unit-testable — see the fixture test below.
struct Translator {
    agent: AgentId,
    /// `system/init` fires again after `/compact`; only the first one is a real `Ready`.
    seen_init: bool,
    /// Whether we've told `Agent` a turn started (first `assistant`/`compact_boundary` since the
    /// last `result`). One `result` line always closes exactly the turn it ends (verified: even a
    /// multi-step tool-use exchange produces one `result` at the very end).
    turn_open: bool,
    /// Set when a `control_response{subtype:"success"}` for our own interrupt request comes back;
    /// consumed by the next `result` to report `status: "interrupted"` instead of "failed".
    interrupt_pending: bool,
    /// The `request_id` of the interrupt we're currently waiting on, set only while a turn is open
    /// (see the `Cmd::Interrupt` arm in `run_claude_process`). A `control_response` only arms
    /// `interrupt_pending` when its own `request_id` matches this — an unrelated or stale response
    /// can never be mistaken for the ack of an interrupt this turn actually requested.
    pending_interrupt_request: Option<String>,
    /// A model/effort change arrived mid-turn; restart (picking up the change via `--resume`) as
    /// soon as `turn_open` goes false, per §10.3 ("never mid-turn").
    restart_pending: bool,
    /// tool_use id -> what `item/started` used, so the matching `tool_result` lands on the same
    /// `Kind` (`Agent::apply_item` dispatches purely on the `type` string we send, not on any
    /// state carried on the item itself) — and, for a file edit, the path again: `apply_item`
    /// only turns a `fileChange` item's `changes` into a `Signal::FilesChanged` when the
    /// *completed* event's own `changes` array is non-empty, so the path must be resent, not just
    /// implied by what `item/started` already told the item.
    open_tools: HashMap<String, ToolOpen>,
    /// The context window Mantra configured for this agent (passed as `--autocompact`).
    autocompact: u64,
    /// Prefer the window the CLI reports (`result.modelUsage.*.contextWindow`) over `autocompact`
    /// for the gauge. True for a subscription login — the CLI knows what the plan really grants —
    /// and false for a custom `api_key` gateway, whose real window only the user's config knows.
    trust_reported_window: bool,
    /// The window the CLI last reported in a `result` line, if any.
    reported_window: Option<u64>,
    /// Running Σ of tokens across every API call of this session (Codex's `total.totalTokens`
    /// semantics: every request's input + output, cache reads included), fed from each
    /// `assistant` message's `usage`.
    total_tokens: u64,
    /// The last `assistant` message id whose usage was folded into `total_tokens` — the CLI emits
    /// one `assistant` line per content block of the same message, each repeating the usage.
    last_usage_msg: Option<String>,
    /// Context in use after the latest `assistant` message: (input incl. cache, output).
    last_ctx: (u64, u64),
    /// `assistant` messages with usage seen in the open turn — when zero, `result.usage` is the
    /// only signal we have.
    turn_msgs: u32,
    /// Set on `system/api_retry` with a 401/403 status: the CLI itself retries these ~10 times
    /// before giving up (verified) — Mantra kills the process on the first one instead.
    fatal_auth: Option<String>,
}

fn as_u64(v: &Value) -> Option<u64> {
    v.as_u64()
}

/// What kind of item a `tool_use` id started as, carrying just enough to build a matching
/// `item/completed` payload once the `tool_result` arrives.
enum ToolOpen {
    Command,
    File(String),
    DynamicMantra,
    Mcp,
}

impl Translator {
    fn new(agent: AgentId, autocompact: u64, trust_reported_window: bool) -> Self {
        Translator {
            agent,
            seen_init: false,
            turn_open: false,
            interrupt_pending: false,
            pending_interrupt_request: None,
            restart_pending: false,
            open_tools: HashMap::new(),
            autocompact,
            trust_reported_window,
            reported_window: None,
            total_tokens: 0,
            last_usage_msg: None,
            last_ctx: (0, 0),
            turn_msgs: 0,
            fatal_auth: None,
        }
    }

    /// The window the gauge and the compaction threshold are measured against.
    fn window(&self) -> Option<u64> {
        let configured = Some(self.autocompact).filter(|w| *w > 0);
        if self.trust_reported_window {
            self.reported_window.or(configured)
        } else {
            configured.or(self.reported_window)
        }
    }

    /// Fold an API `usage` object (an `assistant` message's, or `result`'s) into the running
    /// totals and emit the gauge update. Returns false when the object carried no counts.
    fn fold_usage(&mut self, usage: Option<&Value>, msg_id: Option<&str>, out: &mut Vec<HubEvent>) -> bool {
        let g = |k: &str| usage.and_then(|u| u.get(k)).and_then(as_u64).unwrap_or(0);
        let (input, output) = (g("input_tokens") + g("cache_read_input_tokens") + g("cache_creation_input_tokens"), g("output_tokens"));
        if input + output == 0 {
            return false;
        }
        let repeat = msg_id.is_some() && msg_id == self.last_usage_msg.as_deref();
        if !repeat {
            self.total_tokens += input + output;
            self.last_usage_msg = msg_id.map(|s| s.to_string());
        }
        self.last_ctx = (input, output);
        self.turn_msgs += 1;
        out.push(token_usage(self.agent, input, output, Some((self.total_tokens, self.window()))));
        true
    }

    fn on_line(&mut self, v: &Value, is_restart: bool) -> Vec<HubEvent> {
        let mut out = vec![];
        match v.get("type").and_then(|x| x.as_str()).unwrap_or("") {
            "system" => self.on_system(v, is_restart, &mut out),
            "assistant" => self.on_assistant(v, &mut out),
            "user" => self.on_user(v, &mut out),
            "control_response" => {
                if v.pointer("/response/subtype").and_then(|x| x.as_str()) == Some("success") {
                    let req_id = v.pointer("/response/request_id").and_then(|x| x.as_str());
                    // Only the ack of the interrupt *this* turn requested may arm
                    // `interrupt_pending` — an idle-time or otherwise unrelated response must not
                    // bleed into a later, unrelated turn's completion status.
                    if self.turn_open && req_id.is_some() && req_id == self.pending_interrupt_request.as_deref() {
                        self.interrupt_pending = true;
                        self.pending_interrupt_request = None;
                    }
                }
            }
            "result" => self.on_result(v, &mut out),
            // A tool is still running: no state change, but the agent is alive (watchdog/stall).
            "tool_progress" => out.push(activity(self.agent, None)),
            // control_request (from the CLI's own side, unused), commands_changed…
            _ => {}
        }
        out
    }

    fn ensure_turn_open(&mut self, out: &mut Vec<HubEvent>) {
        if !self.turn_open {
            self.turn_open = true;
            out.push(HubEvent::Notif { agent: self.agent, method: "turn/started".into(), params: json!({}) });
        }
    }

    fn on_system(&mut self, v: &Value, is_restart: bool, out: &mut Vec<HubEvent>) {
        match v.get("subtype").and_then(|x| x.as_str()).unwrap_or("") {
            "init" => {
                if !self.seen_init {
                    self.seen_init = true;
                    let thread_id = v.get("session_id").and_then(|x| x.as_str()).unwrap_or("").to_string();
                    let model = v.get("model").and_then(|x| x.as_str()).unwrap_or("").to_string();
                    out.push(HubEvent::Ready { agent: self.agent, thread_id, model, resumed: is_restart });
                }
                // else: the repeat after /compact — never a second Ready.
            }
            "compact_boundary" => {
                // Same from→to dance Codex's own `contextCompaction` item uses (agent.rs): a
                // tokenUsage update sets `ctx_used`, the item captures it as `from`, and the
                // tokenUsage update after `item/completed` fills in `to` once it's lower.
                self.ensure_turn_open(out);
                let meta = v.get("compact_metadata");
                let pre = meta.and_then(|m| m.get("pre_tokens")).and_then(as_u64).unwrap_or(0);
                let post = meta.and_then(|m| m.get("post_tokens")).and_then(as_u64).unwrap_or(0);
                let id = format!("cc-compact-{}", v.get("uuid").and_then(|x| x.as_str()).unwrap_or("0"));
                let (total, window) = (self.total_tokens, self.window());
                out.push(token_usage(self.agent, pre, 0, Some((total, window))));
                out.push(HubEvent::Notif { agent: self.agent, method: "item/started".into(), params: json!({"item": {"id": id, "type": "contextCompaction"}}) });
                out.push(HubEvent::Notif { agent: self.agent, method: "item/completed".into(), params: json!({"item": {"id": id, "type": "contextCompaction"}}) });
                out.push(token_usage(self.agent, post, 0, Some((total, window))));
                self.last_ctx = (post, 0);
            }
            "thinking_tokens" => {
                // Emitted about once a second while the model reasons, before any visible
                // content: the turn is live, and the agent is not idle.
                self.ensure_turn_open(out);
                out.push(activity(self.agent, Some("thinking")));
            }
            "api_retry" => {
                if let Some(status @ (401 | 403)) = v.get("error_status").and_then(|x| x.as_i64()) {
                    // Surface it as a failed turn first (Solo shows the error, a run halts with
                    // `HaltReason::Auth`), then the process is killed so the CLI's own ten retries
                    // don't hammer the provider with the same bad key.
                    let msg = format!("HTTP {status} from the provider — check this provider's API key (or its base_url)");
                    out.push(HubEvent::Notif { agent: self.agent, method: "turn/completed".into(), params: json!({"turn": {"status": "failed", "error": {"message": msg, "codexErrorInfo": "unauthorized"}}}) });
                    self.turn_open = false;
                    self.open_tools.clear();
                    self.fatal_auth = Some(format!("auth error {status} — bad API key for this provider?"));
                }
            }
            // status (compacting/success/failed), task_started, task_notification, task_summary,
            // post_turn_summary, commands_changed: no Agent-visible signal needed.
            _ => {}
        }
    }

    fn on_assistant(&mut self, v: &Value, out: &mut Vec<HubEvent>) {
        let Some(content) = v.pointer("/message/content").and_then(|c| c.as_array()) else { return };
        let msg_id = v.pointer("/message/id").and_then(|x| x.as_str()).unwrap_or("m");
        // Every API call's own usage: `input + cache_read + cache_creation` is exactly the
        // context the model just saw, so the gauge moves while the turn runs instead of
        // jumping once at `result` (whose `usage` is the *sum* over the turn's calls — never a
        // context size, and the reason a 200k model used to read "211k / 200k").
        if v.pointer("/message/usage").is_some() {
            self.ensure_turn_open(out);
            self.fold_usage(v.pointer("/message/usage"), Some(msg_id), out);
        }
        for (i, block) in content.iter().enumerate() {
            match block.get("type").and_then(|x| x.as_str()).unwrap_or("") {
                "text" => {
                    let text = block.get("text").and_then(|x| x.as_str()).unwrap_or("");
                    if text.is_empty() {
                        continue;
                    }
                    self.ensure_turn_open(out);
                    let id = format!("cc-{msg_id}-{i}");
                    out.push(HubEvent::Notif { agent: self.agent, method: "item/started".into(), params: json!({"item": {"id": id, "type": "agentMessage", "text": ""}}) });
                    out.push(HubEvent::Notif { agent: self.agent, method: "item/completed".into(), params: json!({"item": {"id": id, "type": "agentMessage", "text": text}}) });
                }
                "tool_use" => {
                    let Some(id) = block.get("id").and_then(|x| x.as_str()) else { continue };
                    self.ensure_turn_open(out);
                    let name = block.get("name").and_then(|x| x.as_str()).unwrap_or("");
                    let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
                    let (open, item) = started_item(id, name, &input);
                    self.open_tools.insert(id.to_string(), open);
                    out.push(HubEvent::Notif { agent: self.agent, method: "item/started".into(), params: json!({"item": item}) });
                }
                _ => {} // "thinking"/"redacted_thinking" etc: not required for v0.2 (no --include-partial-messages)
            }
        }
    }

    fn on_user(&mut self, v: &Value, out: &mut Vec<HubEvent>) {
        // Plain-string `content` is a synthetic message (a /compact summary, `<local-command-stdout>`,
        // "[Request interrupted by user for tool use]"): never a real tool_result, always ignored.
        let Some(blocks) = v.pointer("/message/content").and_then(|c| c.as_array()) else { return };
        for block in blocks {
            if block.get("type").and_then(|x| x.as_str()) != Some("tool_result") {
                continue;
            }
            let Some(tool_id) = block.get("tool_use_id").and_then(|x| x.as_str()) else { continue };
            let is_error = block.get("is_error").and_then(|x| x.as_bool()).unwrap_or(false);
            let status = if is_error { "failed" } else { "completed" };
            let text = tool_result_text(block, v.get("tool_use_result"));
            let open = self.open_tools.remove(tool_id).unwrap_or(ToolOpen::Mcp);
            let item = match open {
                ToolOpen::Command => json!({"id": tool_id, "type": "commandExecution", "status": status, "aggregatedOutput": text}),
                // Resend the path: `apply_item` only turns this into `Signal::FilesChanged` when
                // *this* event's `changes` array is non-empty, not from what `item/started` said.
                ToolOpen::File(path) => json!({"id": tool_id, "type": "fileChange", "status": status, "changes": [{"path": path, "kind": {"type": "modified"}, "diff": ""}]}),
                ToolOpen::DynamicMantra => json!({"id": tool_id, "type": "dynamicToolCall", "status": status, "contentItems": [{"type": "text", "text": text}]}),
                ToolOpen::Mcp => json!({"id": tool_id, "type": "mcpToolCall", "status": status, "result": text}),
            };
            out.push(HubEvent::Notif { agent: self.agent, method: "item/completed".into(), params: json!({"item": item}) });
        }
    }

    fn on_result(&mut self, v: &Value, out: &mut Vec<HubEvent>) {
        let interrupted = std::mem::take(&mut self.interrupt_pending);
        // A turn whose only assistant content is `thinking`/`redacted_thinking` (no visible text,
        // no tool_use — see `on_assistant`) never calls `ensure_turn_open` itself, so `turn_open`
        // would still be false here. Open it now unconditionally rather than bailing out: every
        // `result` line closes exactly the turn it ends, even an invisible one, so `turn_open` stays
        // in lockstep with each `result` instead of getting stuck false for the rest of the process
        // (which would silently swallow every later turn's `turn/started`/`turn/completed`).
        self.ensure_turn_open(out);
        let num_turns = v.get("num_turns").and_then(|x| x.as_i64()).unwrap_or(1);
        if let Some(w) = v.get("modelUsage").and_then(|m| m.as_object()).and_then(|o| o.values().next()).and_then(|e| e.get("contextWindow")).and_then(as_u64).filter(|w| *w > 0) {
            self.reported_window = Some(w);
        }
        if num_turns > 0 {
            // The num_turns:0 housekeeping result (a manual /compact, or an automatic one) carries
            // an all-zero `usage` block — its real token counts already went out via the
            // compact_boundary tokenUsage updates above, so pushing this one would zero the gauge.
            if self.turn_msgs > 0 {
                // The gauge already tracks the last call's context; re-emit it so a window first
                // learned from this very `result` reaches the agent too.
                let (input, output) = self.last_ctx;
                out.push(token_usage(self.agent, input, output, Some((self.total_tokens, self.window()))));
            } else {
                // No per-call usage came through (an older CLI, or a turn with no assistant
                // line): `result.usage` is the best available estimate.
                self.fold_usage(v.get("usage"), None, out);
            }
        }
        self.turn_msgs = 0;
        let is_error = v.get("is_error").and_then(|x| x.as_bool()).unwrap_or(false);
        let status = if interrupted { "interrupted" } else if is_error { "failed" } else { "completed" };
        let error = if status == "failed" {
            let msg = v
                .get("result")
                .and_then(|x| x.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .unwrap_or_else(|| v.get("subtype").and_then(|x| x.as_str()).unwrap_or("turn failed").to_string());
            let mut e = json!({"message": msg});
            if let Some(code) = classify_claude_error(v, &msg) {
                e["codexErrorInfo"] = json!(code);
            }
            e
        } else {
            Value::Null
        };
        out.push(HubEvent::Notif { agent: self.agent, method: "turn/completed".into(), params: json!({"turn": {"status": status, "error": error}}) });
        self.turn_open = false;
        self.pending_interrupt_request = None; // any request left unacked when the turn closed is stale
        self.open_tools.clear(); // any tool call left dangling when the turn ended is stale
    }
}

/// A liveness tick for `Agent`: bumps `last_event` (watchdog, stall tripwire) and optionally
/// relabels the activity — a Mantra-only method, see `Agent::apply`.
fn activity(agent: AgentId, label: Option<&str>) -> HubEvent {
    let params = match label {
        Some(l) => json!({"activity": l}),
        None => json!({}),
    };
    HubEvent::Notif { agent, method: "mantra/activity".into(), params }
}

/// Builds a `thread/tokenUsage/updated` params value. `last_out` almost always 0 by construction —
/// callers fold everything Codex's own `ctx_used = last.inputTokens + last.outputTokens` should
/// count (input + cache_read + cache_creation, per §10.3) into `last_in` up front, so the sum comes
/// out exactly right without agent.rs needing to know about Claude's separate cache-token fields.
fn token_usage(agent: AgentId, last_in: u64, last_out: u64, total_and_window: Option<(u64, Option<u64>)>) -> HubEvent {
    let mut tu = json!({"last": {"inputTokens": last_in, "outputTokens": last_out}});
    let total = total_and_window.map(|(t, _)| t).unwrap_or(last_in + last_out);
    tu["total"] = json!({"totalTokens": total});
    if let Some((_, Some(w))) = total_and_window {
        tu["modelContextWindow"] = json!(w);
    }
    HubEvent::Notif { agent, method: "thread/tokenUsage/updated".into(), params: json!({"tokenUsage": tu}) }
}

/// Classifies a `codexErrorInfo`-shaped key so `agent::classify`/`refine` (unchanged) map it to the
/// right `ErrKind`. `ErrKind::ProviderRejected` (WP6) doesn't exist yet in this batch, so 400/422
/// map to the closest existing bucket, `ErrKind::BadRequest` (`"badRequest"`) — a WP6 follow-up can
/// widen this once that variant lands, with no change needed here beyond the returned key.
fn classify_claude_error(v: &Value, msg: &str) -> Option<&'static str> {
    let m = msg.to_lowercase();
    if m.contains("prompt is too long") || (m.contains("context") && (m.contains("too long") || m.contains("exceed"))) {
        return Some("contextWindowExceeded");
    }
    if m.contains("max_budget") || m.contains("budget exceeded") {
        return Some("usageLimitExceeded");
    }
    match v.get("api_error_status").and_then(|x| x.as_i64()) {
        Some(401) | Some(403) => Some("unauthorized"),
        Some(400) | Some(422) => Some("badRequest"),
        Some(429) => Some("rateLimitExceeded"),
        Some(s) if (500..600).contains(&s) => Some("serverOverloaded"),
        _ => None,
    }
}

/// Classifies one `assistant` `tool_use` block into the synthetic item shape `Agent::apply_item`
/// expects, per §10.3: `Bash` → `commandExecution`, `Edit|Write|MultiEdit|NotebookEdit` →
/// `fileChange`, `mcp__mantra__*` → `dynamicToolCall` (name stripped to `mantra_*`; not reachable
/// without the WP10.4 bridge, but translated correctly once it exists), everything else →
/// `mcpToolCall` (server labelled `"claude"` for its built-in tools, since there's no real
/// originating MCP server name to report).
fn started_item(id: &str, name: &str, input: &Value) -> (ToolOpen, Value) {
    if name == "Bash" {
        let cmd = input.get("command").and_then(|x| x.as_str()).unwrap_or("").to_string();
        (ToolOpen::Command, json!({"id": id, "type": "commandExecution", "command": cmd, "status": "inProgress"}))
    } else if matches!(name, "Edit" | "Write" | "MultiEdit" | "NotebookEdit") {
        let path = input.get("file_path").or_else(|| input.get("notebook_path")).and_then(|x| x.as_str()).unwrap_or("").to_string();
        (ToolOpen::File(path.clone()), json!({"id": id, "type": "fileChange", "status": "inProgress", "changes": [{"path": path, "kind": {"type": "modified"}, "diff": ""}]}))
    } else if let Some(mantra_name) = name.strip_prefix("mcp__mantra__") {
        (ToolOpen::DynamicMantra, json!({"id": id, "type": "dynamicToolCall", "tool": mantra_name, "arguments": input, "status": "inProgress"}))
    } else {
        (ToolOpen::Mcp, json!({"id": id, "type": "mcpToolCall", "server": "claude", "tool": name, "arguments": input, "status": "inProgress"}))
    }
}

fn tool_result_text(block: &Value, tool_use_result: Option<&Value>) -> String {
    if let Some(stdout) = tool_use_result.and_then(|t| t.get("stdout")).and_then(|x| x.as_str()) {
        let stderr = tool_use_result.and_then(|t| t.get("stderr")).and_then(|x| x.as_str()).unwrap_or("");
        return if stderr.is_empty() { stdout.to_string() } else { format!("{stdout}\n{stderr}") };
    }
    match block.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(items)) => items.iter().find_map(|it| it.get("text").and_then(|x| x.as_str())).unwrap_or("").to_string(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{Agent, Kind, Signal};

    #[test]
    fn build_args_covers_resume_effort_autocompact_and_sandbox() {
        let spec = SpawnSpec {
            cwd: std::path::PathBuf::from("/work"),
            model: String::new(),
            provider: String::new(),
            effort: String::new(),
            approval: "never".into(),
            sandbox: "read-only".into(),
            developer_instructions: String::new(),
            dynamic_tools: vec![],
            config: serde_json::Map::new(),
            extra_args: vec![],
            resume_thread: None,
            backend: crate::config::ProviderKind::ClaudeCode,
            envs: vec![],
            claude: None,
        };
        let cs = ClaudeSpawn { auth: "api_key".into(), base_url: "https://api.libertai.io/v1".into(), autocompact: 200_000, system_prompt: "be nice".into(), mcp_sock: None };
        let args = build_args(1, &spec, &cs, "sonnet5", "high", "abc-123", true, &[]);
        assert!(args.windows(2).any(|w| w == ["--resume".to_string(), "abc-123".to_string()]));
        assert!(!args.contains(&"--session-id".to_string()));
        assert!(args.windows(2).any(|w| w == ["--model".to_string(), "sonnet5".to_string()]));
        assert!(args.windows(2).any(|w| w == ["--effort".to_string(), "high".to_string()]));
        assert!(args.windows(2).any(|w| w == ["--autocompact".to_string(), "200000".to_string()]));
        assert!(args.contains(&"--dangerously-skip-permissions".to_string()));
        assert!(args.contains(&"--bare".to_string()));
        assert!(args.windows(2).any(|w| w == ["--tools".to_string(), "Read,Glob,Grep,WebFetch".to_string()]));
        assert!(!args.contains(&"--mcp-config".to_string()), "no bridge socket configured (mcp_sock: None) — must never add --mcp-config to a socket nothing listens on");
    }

    #[test]
    fn build_args_wires_the_mcp_bridge_when_a_socket_and_tools_exist() {
        let spec = SpawnSpec {
            cwd: std::path::PathBuf::from("/work"),
            model: String::new(),
            provider: String::new(),
            effort: String::new(),
            approval: "never".into(),
            sandbox: "workspace-write".into(),
            developer_instructions: String::new(),
            dynamic_tools: vec![json!({"type": "function", "name": "mantra_status", "description": "status", "inputSchema": {"type": "object", "properties": {}}})],
            config: serde_json::Map::new(),
            extra_args: vec![],
            resume_thread: None,
            backend: crate::config::ProviderKind::ClaudeCode,
            envs: vec![],
            claude: None,
        };
        let cs = ClaudeSpawn { auth: "subscription".into(), base_url: String::new(), autocompact: 200_000, system_prompt: "be nice".into(), mcp_sock: Some(std::path::PathBuf::from("/tmp/mantra-9.sock")) };
        let args = build_args(7, &spec, &cs, "sonnet5", "high", "abc-123", false, &[]);
        assert!(args.contains(&"--strict-mcp-config".to_string()));
        let i = args.iter().position(|a| a == "--mcp-config").expect("--mcp-config present");
        let cfg: Value = serde_json::from_str(&args[i + 1]).expect("--mcp-config value must be valid JSON");
        assert_eq!(cfg.pointer("/mcpServers/mantra/args/1").and_then(|v| v.as_str()), Some("--sock"));
        assert_eq!(cfg.pointer("/mcpServers/mantra/args/2").and_then(|v| v.as_str()), Some("/tmp/mantra-9.sock"));
        assert_eq!(cfg.pointer("/mcpServers/mantra/args/4").and_then(|v| v.as_str()), Some("7"), "--agent must carry this spawn's own agent id");
        assert!(args.iter().any(|a| a.contains("mcp__mantra__")), "the system prompt must tell the agent its tools' MCP-prefixed names");
        // no tools at all (e.g. an architect-less role) → no point spawning a bridge for nothing
        let mut no_tools = spec.clone();
        no_tools.dynamic_tools = vec![];
        let args2 = build_args(7, &no_tools, &cs, "sonnet5", "high", "abc-123", false, &[]);
        assert!(!args2.contains(&"--mcp-config".to_string()), "an empty tool list must not wire up a bridge either");
    }

    /// Drives `Translator` output into an `Agent` and returns it, for gauge assertions.
    fn feed(tr: &mut Translator, agent: &mut Agent, lines: &[Value]) {
        for v in lines {
            for e in tr.on_line(v, false) {
                if let HubEvent::Notif { method, params, .. } = e {
                    agent.apply(&method, &params);
                }
            }
        }
    }

    /// The gauge follows each API call's own usage while the turn runs (context = input +
    /// cache reads + cache writes), never the turn-summed `result.usage` — which is how a 200k
    /// model used to read "211k / 200k" at the end of a long turn and 0 in between.
    #[test]
    fn context_gauge_tracks_per_call_usage_not_the_turn_sum() {
        let mut tr = Translator::new(1, 200_000, true);
        let mut agent = Agent::new(1, "t", "planner", std::path::PathBuf::from("/tmp/x"));
        let call1 = json!({"type": "assistant", "message": {"id": "m1", "usage": {"input_tokens": 1000, "cache_read_input_tokens": 50_000, "cache_creation_input_tokens": 4000, "output_tokens": 300}, "content": [{"type": "text", "text": "looking"}]}});
        // the same message again, as the CLI does per content block — must not double count
        let call1b = json!({"type": "assistant", "message": {"id": "m1", "usage": {"input_tokens": 1000, "cache_read_input_tokens": 50_000, "cache_creation_input_tokens": 4000, "output_tokens": 300}, "content": [{"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "ls"}}]}});
        feed(&mut tr, &mut agent, &[call1, call1b]);
        assert!(agent.turn_active, "a usage-bearing assistant line opens the turn");
        assert_eq!(agent.ctx_used, 55_300, "context = input + cache read + cache creation + output of the latest call");
        assert_eq!(agent.tokens_total, 55_300, "one message, counted once");
        assert_eq!(agent.ctx_window, Some(200_000), "the configured window until the CLI reports one");

        let call2 = json!({"type": "assistant", "message": {"id": "m2", "usage": {"input_tokens": 200, "cache_read_input_tokens": 55_000, "cache_creation_input_tokens": 900, "output_tokens": 100}, "content": [{"type": "text", "text": "done"}]}});
        // result.usage is the Σ over both calls (a subscription CLI also reports the real window)
        let result = json!({"type": "result", "is_error": false, "num_turns": 1, "usage": {"input_tokens": 1200, "cache_read_input_tokens": 105_000, "cache_creation_input_tokens": 4900, "output_tokens": 400}, "modelUsage": {"claude-sonnet-5": {"contextWindow": 1_000_000}}});
        feed(&mut tr, &mut agent, &[call2, result]);
        assert_eq!(agent.ctx_used, 56_200, "the last call's context, not the 111k turn sum");
        assert_eq!(agent.tokens_total, 55_300 + 56_200);
        assert_eq!(agent.ctx_window, Some(1_000_000), "subscription auth trusts the window the CLI reports");
        assert!(!agent.turn_active);
    }

    /// An `api_key` gateway's real window is whatever the user configured; the CLI's guess is only
    /// a fallback. And a turn with no per-call usage still gets an estimate from `result.usage`.
    #[test]
    fn api_key_auth_keeps_the_configured_window_and_result_usage_is_the_fallback() {
        let mut tr = Translator::new(1, 262_144, false);
        let mut agent = Agent::new(1, "t", "planner", std::path::PathBuf::from("/tmp/x"));
        let result = json!({"type": "result", "is_error": false, "num_turns": 1, "usage": {"input_tokens": 1200, "cache_read_input_tokens": 3000, "output_tokens": 400}, "modelUsage": {"qwen": {"contextWindow": 200_000}}});
        feed(&mut tr, &mut agent, &[result]);
        assert_eq!(agent.ctx_used, 4_600);
        assert_eq!(agent.ctx_window, Some(262_144));
    }

    /// `system/thinking_tokens` (once a second while the model reasons) and `tool_progress`
    /// (while a long command runs) prove the agent is alive: they open the turn, label the
    /// activity and move `last_event` — so the watchdog never mistakes a three-minute `uv pip
    /// install` for an idle agent.
    #[test]
    fn heartbeats_keep_the_agent_alive_and_open_the_turn() {
        let mut tr = Translator::new(1, 200_000, true);
        let mut agent = Agent::new(1, "t", "planner", std::path::PathBuf::from("/tmp/x"));
        agent.last_event = std::time::Instant::now() - std::time::Duration::from_secs(500);
        let think = json!({"type": "system", "subtype": "thinking_tokens", "estimated_tokens": 150, "session_id": "s"});
        feed(&mut tr, &mut agent, &[think.clone()]);
        assert!(agent.turn_active);
        assert_eq!(agent.activity, "thinking");
        assert!(agent.last_event.elapsed().as_secs() < 5, "a heartbeat is an event");
        assert!(is_heartbeat(&think) && is_heartbeat(&json!({"type": "tool_progress"})) && !is_heartbeat(&json!({"type": "assistant"})));
        let bash = json!({"type": "assistant", "message": {"id": "m1", "content": [{"type": "tool_use", "id": "t1", "name": "Bash", "input": {"command": "uv pip install pytest"}}]}});
        feed(&mut tr, &mut agent, &[bash]);
        agent.last_event = std::time::Instant::now() - std::time::Duration::from_secs(500);
        feed(&mut tr, &mut agent, &[json!({"type": "tool_progress", "tool_use_id": "t1", "elapsed_time_seconds": 90})]);
        assert!(agent.last_event.elapsed().as_secs() < 5);
        assert!(agent.activity.starts_with("$ uv pip"), "progress ticks keep the command label: {}", agent.activity);
    }

    #[test]
    fn mcp_tool_list_reshapes_codex_style_dynamic_tools_for_mcp() {
        let tools = vec![json!({"type": "function", "name": "mantra_status", "description": "status", "inputSchema": {"type": "object"}})];
        let v = mcp_tool_list(&tools);
        assert_eq!(v.pointer("/tools/0/name").and_then(|x| x.as_str()), Some("mantra_status"));
        assert_eq!(v.pointer("/tools/0/description").and_then(|x| x.as_str()), Some("status"));
        assert!(v.pointer("/tools/0/type").is_none(), "the Codex-only `type: function` wrapper must be dropped");
    }

    #[test]
    fn strip_v1_drops_only_a_trailing_v1() {
        assert_eq!(strip_v1("https://api.libertai.io/v1"), "https://api.libertai.io");
        assert_eq!(strip_v1("https://api.libertai.io/v1/"), "https://api.libertai.io");
        assert_eq!(strip_v1("https://api.libertai.io"), "https://api.libertai.io");
    }

    /// Feeds the exact recorded NDJSON lines from `src/testdata/claude-stream.jsonl` (a real,
    /// live-captured Claude Code 2.1.269 + LibertAI session — see `v02plan.md` §10.1) through the
    /// translator and a real `Agent`, and asserts the resulting item sequence and `Signal::TurnDone`
    /// match what §10.3 specifies.
    #[test]
    fn translates_the_recorded_claude_session_the_way_wp10_3_specifies() {
        let raw = include_str!("../testdata/claude-stream.jsonl");
        let mut tr = Translator::new(1, 200_000, false);
        let mut agent = Agent::new(1, "t", "solo", std::path::PathBuf::from("/tmp/ccfix"));
        let mut statuses = vec![];
        let mut ready_count = 0;

        for line in raw.lines().filter(|l| !l.trim().is_empty()) {
            let v: Value = serde_json::from_str(line).expect("fixture must be valid NDJSON");
            if v.get("type").and_then(|x| x.as_str()) == Some("control_response") {
                // Simulate what `run_claude_process`'s `Cmd::Interrupt` arm does: it only writes
                // the control_request (recording its id) while `turn_open` is true, which it is at
                // this point in the fixture (the bash turn opened by the previous line is still
                // open) — so the recorded ack below is expected to arm `interrupt_pending`.
                tr.pending_interrupt_request = Some("int-1".into());
            }
            for e in tr.on_line(&v, false) {
                match e {
                    HubEvent::Ready { .. } => ready_count += 1,
                    HubEvent::Notif { method, params, .. } => {
                        for s in agent.apply(&method, &params) {
                            if let Signal::TurnDone { status, .. } = s {
                                statuses.push(status);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        assert_eq!(ready_count, 1, "a repeated system/init (after /compact) must never emit a second Ready");
        assert!(agent.items.iter().any(|i| matches!(i.kind, Kind::Command { .. })), "the Bash tool_use/tool_result pair must become a Command item");
        assert!(agent.items.iter().any(|i| matches!(i.kind, Kind::Compaction { from, to } if from > 0 && to.is_some())), "compact_boundary must become a completed Compaction item with from/to filled in");
        assert!(agent.items.iter().any(|i| matches!(i.kind, Kind::Agent) && i.text.contains("hello world")), "plain assistant text must become an Agent item");
        assert_eq!(statuses.iter().filter(|s| s.as_str() == "completed").count(), 3, "3 ordinary turns in the fixture (plain text, bash, post-compact) must report TurnDone{{completed}}");
        assert_eq!(statuses.iter().filter(|s| s.as_str() == "interrupted").count(), 1, "the bash turn ended by control_request/interrupt must report TurnDone{{interrupted}}, not failed");
        assert!(!statuses.iter().any(|s| s == "failed"), "nothing in this fixture is a genuine failure");
        assert!(!agent.busy(), "the agent must be idle once every result in the fixture has been processed");
    }

    /// Regression test for the WP10 review finding: a `control_response{subtype:"success"}` that
    /// does not correlate to a `request_id` this translator actually armed (e.g. the ack of a stray
    /// interrupt sent while idle, via the unguarded 'x' keybinding) must never set
    /// `interrupt_pending`, and must therefore never bleed "interrupted" into an unrelated later
    /// turn that in fact completed normally.
    /// A 401/403 from the provider must show up as a failed turn (`unauthorized`, so a run halts
    /// with `HaltReason::Auth` and Solo prints the error) *and* mark the process for a kill before
    /// the CLI's own retries — never as a silent crash with a Debug-formatted status.
    #[test]
    fn provider_auth_error_fails_the_turn_and_kills_the_process() {
        let mut tr = Translator::new(1, 200_000, false);
        let assistant = json!({"type": "assistant", "message": {"id": "m1", "content": [{"type": "text", "text": "hi"}]}});
        let _ = tr.on_line(&assistant, false);
        let retry = json!({"type": "system", "subtype": "api_retry", "error_status": 401, "attempt": 1});
        let out = tr.on_line(&retry, false);
        let err = out.iter().find_map(|e| match e {
            HubEvent::Notif { method, params, .. } if method == "turn/completed" => Some(params["turn"].clone()),
            _ => None,
        });
        let err = err.expect("a failed turn must be reported");
        assert_eq!(err["status"], "failed");
        assert_eq!(err["error"]["codexErrorInfo"], "unauthorized");
        assert!(err["error"]["message"].as_str().unwrap().contains("HTTP 401"));
        assert!(!tr.turn_open);
        let reason = tr.fatal_auth.clone().expect("the process must be killed before the CLI retries");
        assert!(reason.contains("401") && !reason.contains("Some("), "{reason}");
        // a 5xx retry is not fatal
        let mut tr2 = Translator::new(1, 200_000, false);
        assert!(tr2.on_line(&json!({"type": "system", "subtype": "api_retry", "error_status": 503}), false).is_empty());
        assert!(tr2.fatal_auth.is_none());
    }

    #[test]
    fn uncorrelated_control_response_never_arms_a_later_unrelated_turn() {
        let mut tr = Translator::new(1, 200_000, false);

        // A stray ack arrives with no turn open and nothing pending (the old code armed
        // `interrupt_pending` unconditionally here).
        let stray = json!({"type": "control_response", "response": {"subtype": "success", "request_id": "int-stray"}});
        assert!(tr.on_line(&stray, false).is_empty());
        assert!(!tr.interrupt_pending, "an unrequested/idle-time ack must never arm interrupt_pending");

        // The next, entirely unrelated turn now runs to completion.
        let assistant = json!({"type": "assistant", "message": {"id": "m1", "content": [{"type": "text", "text": "hi"}]}});
        let started = tr.on_line(&assistant, false);
        assert!(tr.turn_open);
        drop(started);

        let result = json!({"type": "result", "is_error": false, "num_turns": 1, "usage": {"input_tokens": 1, "output_tokens": 1}});
        let out = tr.on_line(&result, false);
        let status = out.iter().find_map(|e| match e {
            HubEvent::Notif { method, params, .. } if method == "turn/completed" => params.pointer("/turn/status").and_then(|s| s.as_str()),
            _ => None,
        });
        assert_eq!(status, Some("completed"), "a genuinely completed turn must not be reported as interrupted because of a stray earlier ack");
    }

    /// A `control_response` that arrives for the *right* `request_id` but after the turn it was
    /// meant to interrupt has already closed (e.g. the turn ended on its own just before the ack
    /// came back) must not reach forward and mark a later turn interrupted either.
    #[test]
    fn control_response_after_its_turn_already_closed_does_not_leak_forward() {
        let mut tr = Translator::new(1, 200_000, false);

        let assistant = json!({"type": "assistant", "message": {"id": "m1", "content": [{"type": "text", "text": "hi"}]}});
        tr.on_line(&assistant, false);
        tr.pending_interrupt_request = Some("int-1".into()); // as if Cmd::Interrupt fired while this turn was open

        // The turn closes on its own before the ack arrives.
        let result1 = json!({"type": "result", "is_error": false, "num_turns": 1, "usage": {"input_tokens": 1, "output_tokens": 1}});
        tr.on_line(&result1, false);
        assert!(!tr.turn_open);

        // The late ack for the now-closed turn's interrupt request arrives.
        let late_ack = json!({"type": "control_response", "response": {"subtype": "success", "request_id": "int-1"}});
        tr.on_line(&late_ack, false);
        assert!(!tr.interrupt_pending, "an ack for an already-closed turn must not arm a future turn's status");

        // A second, unrelated turn completes normally.
        let assistant2 = json!({"type": "assistant", "message": {"id": "m2", "content": [{"type": "text", "text": "hi again"}]}});
        tr.on_line(&assistant2, false);
        let result2 = json!({"type": "result", "is_error": false, "num_turns": 1, "usage": {"input_tokens": 1, "output_tokens": 1}});
        let out = tr.on_line(&result2, false);
        let status = out.iter().find_map(|e| match e {
            HubEvent::Notif { method, params, .. } if method == "turn/completed" => params.pointer("/turn/status").and_then(|s| s.as_str()),
            _ => None,
        });
        assert_eq!(status, Some("completed"));
    }

    /// Regression test for the WP10 review finding: a turn whose only assistant content is a
    /// `thinking` block (no visible text, no `tool_use`) never calls `ensure_turn_open` from
    /// `on_assistant`. The old code's defensive bail-out in `on_result` (`if !self.turn_open {
    /// return; }`) silently dropped such a turn's `result` line entirely, leaving `turn_open` stuck
    /// `false` forever and swallowing every later turn's `turn/started`/`turn/completed` too.
    #[test]
    fn a_turn_with_only_thinking_content_still_closes_and_does_not_wedge_later_turns() {
        let mut tr = Translator::new(1, 200_000, false);

        let thinking_only = json!({"type": "assistant", "message": {"id": "m1", "content": [{"type": "thinking", "thinking": "hmm"}]}});
        let started = tr.on_line(&thinking_only, false);
        assert!(started.is_empty(), "a thinking-only block must not itself open a turn (per on_assistant)");
        assert!(!tr.turn_open);

        let result1 = json!({"type": "result", "is_error": false, "num_turns": 1, "usage": {"input_tokens": 1, "output_tokens": 1}});
        let out1 = tr.on_line(&result1, false);
        assert!(!tr.turn_open, "the invisible turn must still close on its own result");
        let methods: Vec<&str> = out1
            .iter()
            .filter_map(|e| if let HubEvent::Notif { method, .. } = e { Some(method.as_str()) } else { None })
            .collect();
        assert!(methods.contains(&"turn/started"), "the result must retroactively open the turn it closes");
        assert!(methods.contains(&"turn/completed"));

        // A normal, visible turn right after must still work — the old bug wedged `turn_open`
        // false forever, so `ensure_turn_open` (which only opens `if !turn_open`) would have kept
        // finding it "already open" and never fired `turn/started` again.
        let assistant2 = json!({"type": "assistant", "message": {"id": "m2", "content": [{"type": "text", "text": "hi"}]}});
        let out2 = tr.on_line(&assistant2, false);
        assert!(out2.iter().any(|e| matches!(e, HubEvent::Notif { method, .. } if method == "turn/started")), "turn/started must fire again for the next real turn");
        let result2 = json!({"type": "result", "is_error": false, "num_turns": 1, "usage": {"input_tokens": 1, "output_tokens": 1}});
        let out3 = tr.on_line(&result2, false);
        let status = out3.iter().find_map(|e| match e {
            HubEvent::Notif { method, params, .. } if method == "turn/completed" => params.pointer("/turn/status").and_then(|s| s.as_str()),
            _ => None,
        });
        assert_eq!(status, Some("completed"));
    }
}
