//! The hub owns every agent's Codex process.
//!
//! One `codex app-server` process per agent gives crash isolation: when one dies, only that
//! agent restarts (with `thread/resume`), everything else keeps running.

mod claude;

use crate::rpc::{self, Conn, Incoming};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::Child;
use tokio::sync::mpsc;

pub use crate::config::ProviderKind as Backend;
pub use claude::ClaudeSpawn;

pub type AgentId = u32;

#[derive(Debug, Clone)]
pub struct SpawnSpec {
    pub cwd: PathBuf,
    pub model: String,
    pub provider: String,
    pub effort: String,
    pub approval: String,
    pub sandbox: String,
    pub developer_instructions: String,
    pub dynamic_tools: Vec<Value>,
    pub config: serde_json::Map<String, Value>,
    pub extra_args: Vec<String>,
    pub resume_thread: Option<String>,
    /// Which process type owns this agent (`hub::run_process` for Codex, `hub::claude` for
    /// Claude Code — WP10). `agent_task` dispatches on this; everything above is Codex-only and
    /// ignored for the Claude backend.
    pub backend: Backend,
    /// Extra environment variables to set on the child (e.g. a pasted `ANTHROPIC_API_KEY` resolved
    /// by `app::spawn_agent` from the model's provider). Codex's own process doesn't consume this
    /// yet (WP5 territory); the Claude backend applies it in full.
    pub envs: Vec<(String, String)>,
    /// Claude-only spawn config; `None` for Codex agents.
    pub claude: Option<ClaudeSpawn>,
}

#[derive(Debug)]
pub enum Cmd {
    Turn { text: String },
    Steer { text: String },
    Interrupt,
    Compact,
    Respond { id: Value, result: Value },
    RespondErr { id: Value, message: String },
    Archive,
    Shell { command: String },
    SetEffort(String),
    SetModel(String),
    SetApproval(String),
    Restart,
    /// A `mantra mcp-bridge` subprocess (spawned by `claude` per `--mcp-config`, WP10.4) connected
    /// to the Hub's Unix socket and identified itself as belonging to this agent; handed over by
    /// `Hub`'s accept loop via `HubEvent::BridgeConn` → `App::on_hub`. Only meaningful for the
    /// Claude backend (`hub::claude::run_claude_process`); Codex agents never receive one.
    Bridge(UnixStream),
    Shutdown,
}

#[derive(Debug)]
pub enum HubEvent {
    Ready { agent: AgentId, thread_id: String, model: String, resumed: bool },
    Notif { agent: AgentId, method: String, params: Value },
    Request { agent: AgentId, id: Value, method: String, params: Value },
    CmdFailed { agent: AgentId, what: &'static str, error: String, text: Option<String> },
    /// `attempt: 0` (paired with `restarting: true`) is a relaunch notice, not a crash count: it's
    /// how `Exit::Restarting` (a self-inflicted relaunch, never a failure) reaches `App` without a
    /// new event variant every consumer would need to learn.
    Crashed { agent: AgentId, reason: String, restarting: bool, attempt: u32 },
    Exited { agent: AgentId },
    /// A bridge connection arrived for `agent` (WP10.4); `App::on_hub` just forwards the stream to
    /// that agent's own command channel as `Cmd::Bridge` — routing by agent id is `Hub::send`'s job
    /// already, so the accept loop (which has no access to `Hub.agents`) doesn't need to know it.
    BridgeConn { agent: AgentId, stream: UnixStream },
}

/// Minimum gap Mantra tries to keep between two Codex spawns: starting several `codex app-server`
/// processes in the same instant against a fresh `CODEX_HOME` has been observed to crash the first
/// one or two (F2) — they recover via the normal restart path, but staggering avoids the noise.
const SPAWN_STAGGER: Duration = Duration::from_millis(300);

pub struct Hub {
    ev: mpsc::UnboundedSender<HubEvent>,
    agents: HashMap<AgentId, mpsc::UnboundedSender<Cmd>>,
    next: AgentId,
    pub codex_cmd: Vec<String>,
    /// Command used to launch one `claude` process per Claude Code agent (WP10.3).
    pub claude_cmd: Vec<String>,
    last_spawn: Option<Instant>,
    /// Path of the Unix socket `mantra mcp-bridge` subprocesses connect to (WP10.4); `None` when no
    /// `ClaudeCode` provider exists (`enable_bridge = false` in `Hub::new`), or when the socket
    /// couldn't be created (logged, never fatal — Claude agents just run without dynamic tools).
    bridge_sock: Option<PathBuf>,
}

impl Hub {
    /// `enable_bridge`: whether to open the MCP-bridge Unix socket at all — only worth doing when
    /// the registry has at least one `ClaudeCode`-kind provider (WP10.4); computed by the caller
    /// (`main.rs`) so `Hub` itself never has to know about `config::Registry`.
    pub fn new(codex_cmd: Vec<String>, claude_cmd: Vec<String>, ev: mpsc::UnboundedSender<HubEvent>, enable_bridge: bool) -> Hub {
        let bridge_sock = if enable_bridge { setup_bridge(ev.clone()) } else { None };
        Hub { ev, agents: HashMap::new(), next: 1, codex_cmd, claude_cmd, last_spawn: None, bridge_sock }
    }

    /// The bridge socket path a Claude agent's `--mcp-config` should point `mantra mcp-bridge` at,
    /// if the Hub has one (WP10.4).
    pub fn bridge_sock(&self) -> Option<PathBuf> {
        self.bridge_sock.clone()
    }

    pub fn alloc_id(&mut self) -> AgentId {
        let id = self.next;
        self.next += 1;
        id
    }

    pub fn spawn(&mut self, id: AgentId, spec: SpawnSpec) {
        let (tx, rx) = mpsc::unbounded_channel();
        self.agents.insert(id, tx);
        let ev = self.ev.clone();
        let cmd = match spec.backend {
            Backend::ClaudeCode => self.claude_cmd.clone(),
            Backend::Codex => self.codex_cmd.clone(),
        };
        let now = Instant::now();
        let delay = match self.last_spawn {
            Some(prev) => SPAWN_STAGGER.saturating_sub(now.duration_since(prev)),
            None => Duration::ZERO,
        };
        self.last_spawn = Some(now);
        tokio::spawn(agent_task(id, spec, cmd, ev, rx, delay));
    }

    /// Whether an agent process is registered under this id (the web API validates ids with it).
    pub fn has_agent(&self, id: AgentId) -> bool {
        self.agents.contains_key(&id)
    }

    pub fn send(&self, id: AgentId, c: Cmd) {
        if let Some(tx) = self.agents.get(&id) {
            let _ = tx.send(c);
        }
    }

    pub fn shutdown(&mut self, id: AgentId) {
        if let Some(tx) = self.agents.remove(&id) {
            let _ = tx.send(Cmd::Shutdown);
        }
    }

    pub fn shutdown_all(&mut self) {
        for (_, tx) in self.agents.drain() {
            let _ = tx.send(Cmd::Shutdown);
        }
        // "removed at exit" (§10.4) — best-effort; a leaked file under `run/` is harmless (pid-scoped
        // name, next process's bind removes any stale one anyway) but tidying up is cheap.
        if let Some(p) = self.bridge_sock.take() {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// Opens the Hub's one MCP-bridge Unix socket at `$MANTRA_HOME/run/<pid>.sock` and spawns the
/// accept loop (WP10.4). Returns `None` (logged, never fatal) if the directory or the socket
/// itself can't be created — Claude agents then just run without the `mantra_*` dynamic tools
/// (`hub::claude::build_args` only adds `--mcp-config` when this returned `Some`).
fn setup_bridge(ev: mpsc::UnboundedSender<HubEvent>) -> Option<PathBuf> {
    let dir = crate::config::home().join("run");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        crate::mlog!("mcp bridge: couldn't create {}: {e}", dir.display());
        return None;
    }
    let path = dir.join(format!("{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&path); // stale socket from an unclean exit; pid-scoped so this is rare
    let listener = match UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) => {
            crate::mlog!("mcp bridge: bind {}: {e}", path.display());
            return None;
        }
    };
    tokio::spawn(bridge_accept_loop(listener, ev));
    Some(path)
}

async fn bridge_accept_loop(listener: UnixListener, ev: mpsc::UnboundedSender<HubEvent>) {
    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let ev = ev.clone();
                tokio::spawn(async move {
                    match read_bridge_hello(stream).await {
                        Some((agent, stream)) => {
                            let _ = ev.send(HubEvent::BridgeConn { agent, stream });
                        }
                        None => crate::mlog!("mcp bridge: connection dropped (no valid hello)"),
                    }
                });
            }
            Err(e) => {
                crate::mlog!("mcp bridge: accept failed, bridge disabled: {e}");
                return;
            }
        }
    }
}

/// Reads the bridge subprocess's one-line handshake (`{"agent":<id>,"hello":true}`) byte by byte —
/// never through a `BufReader`, which could silently swallow bytes the subprocess sends right after
/// (it doesn't, but losing them would be a very quiet bug) — then hands the raw stream back so the
/// owning agent's task can read the rest of the connection as its own NDJSON lines.
async fn read_bridge_hello(mut stream: UnixStream) -> Option<(AgentId, UnixStream)> {
    use tokio::io::AsyncReadExt;
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match stream.read_exact(&mut byte).await {
            Ok(_) => {
                if byte[0] == b'\n' {
                    break;
                }
                line.push(byte[0]);
                if line.len() > 4096 {
                    return None;
                }
            }
            Err(_) => return None,
        }
    }
    let text = String::from_utf8(line).ok()?;
    let v: Value = serde_json::from_str(text.trim()).ok()?;
    if v.get("hello").and_then(|h| h.as_bool()) != Some(true) {
        return None;
    }
    let agent = v.get("agent").and_then(|a| a.as_u64())? as AgentId;
    Some((agent, stream))
}

const MAX_RESTARTS: u32 = 5;
/// Attempts (first launch included) a process that never reaches `Ready` gets before the agent
/// parks: a spawn refused, an app-server that exits before the handshake, a rejected thread start
/// all fail the same way every time, so retrying five times over minutes only delays the halt.
const MAX_LAUNCH_ATTEMPTS: u32 = 3;

/// The backoff before the next automatic restart, or `None` when the budget is spent and the
/// agent should park until the user asks for a restart. `attempt` counts this failure (1-based):
/// consecutive launch failures for a launch failure, restarts within the 10-minute window
/// otherwise.
fn restart_backoff(attempt: u32, launch_failure: bool) -> Option<Duration> {
    let budget = if launch_failure { MAX_LAUNCH_ATTEMPTS - 1 } else { MAX_RESTARTS };
    (attempt <= budget).then(|| Duration::from_millis(500 * (1u64 << attempt.min(6))))
}

async fn agent_task(
    id: AgentId,
    spec: SpawnSpec,
    cmd: Vec<String>,
    ev: mpsc::UnboundedSender<HubEvent>,
    mut cmds: mpsc::UnboundedReceiver<Cmd>,
    initial_delay: Duration,
) {
    if initial_delay > Duration::ZERO {
        tokio::time::sleep(initial_delay).await;
    }
    // `thread_id` doubles as the Claude backend's session id: both are "the identifier this agent
    // resumes with after a restart", just under different Codex/Claude names.
    let mut thread_id = spec.resume_thread.clone();
    let mut effort = spec.effort.clone();
    let mut model = spec.model.clone();
    let mut approval = spec.approval.clone();
    let mut restarts: u32 = 0;
    let mut launch_failures: u32 = 0;
    let mut window = Instant::now();
    // `Cmd::Turn`/`Cmd::Steer` text held instead of being lost: by the dying process itself (Claude's
    // `restart_pending`, or a dead-resume detection) or by this loop's own crash-backoff wait below —
    // either way delivered once the next process reaches `Ready` (see `run_process`/`run_claude_process`).
    let mut replay: Vec<String> = vec![];
    // The last `Cmd::Turn` text actually sent to a child, kept only so `prepare_relaunch` can requeue
    // it on a *fresh* relaunch (the old session is gone, so that turn never got a reply).
    let mut last_turn: Option<String> = None;

    'outer: loop {
        let started = match spec.backend {
            Backend::Codex => run_process(id, &spec, &cmd, &ev, &mut cmds, &mut thread_id, &mut effort, &mut model, &mut approval, &mut replay, &mut last_turn).await,
            Backend::ClaudeCode => claude::run_claude_process(id, &spec, &cmd, &ev, &mut cmds, &mut thread_id, &mut effort, &mut model, &mut replay, &mut last_turn).await,
        };
        let launch_failure = matches!(started, Exit::LaunchFailed(_));
        match started {
            Exit::Shutdown => {
                let _ = ev.send(HubEvent::Exited { agent: id });
                return;
            }
            Exit::Restarting { reason, fresh } => {
                // Never a failure: no counter, no backoff, no window reset — just go again.
                crate::mlog!("agent {id}: {reason} (relaunching)");
                prepare_relaunch(fresh, &mut thread_id, &last_turn, &mut replay);
                let _ = ev.send(HubEvent::Crashed { agent: id, reason, restarting: true, attempt: 0 });
                continue 'outer;
            }
            Exit::Crashed(reason) | Exit::LaunchFailed(reason) => {
                if window.elapsed() > Duration::from_secs(600) {
                    window = Instant::now();
                    restarts = 0;
                }
                // Each budget counts its own failures: a crash after `Ready` proves the launch
                // path works (the launch budget is per unbroken streak of failures to come up),
                // and a failure to come up does not eat into the crash budget.
                let attempt = if launch_failure {
                    launch_failures += 1;
                    launch_failures
                } else {
                    launch_failures = 0;
                    restarts += 1;
                    restarts
                };
                let backoff = restart_backoff(attempt, launch_failure);
                let restarting = backoff.is_some();
                crate::mlog!("agent {id} {}: {reason} (attempt {attempt}, restarting={restarting})", if launch_failure { "failed to launch" } else { "crashed" });
                let _ = ev.send(HubEvent::Crashed { agent: id, reason, restarting, attempt });
                if let Some(backoff) = backoff {
                    // Wait out the backoff, but stay responsive to shutdown.
                    let deadline = tokio::time::sleep(backoff);
                    tokio::pin!(deadline);
                    loop {
                        tokio::select! {
                            _ = &mut deadline => continue 'outer,
                            c = cmds.recv() => match c {
                                None | Some(Cmd::Shutdown) => { let _ = ev.send(HubEvent::Exited { agent: id }); return; }
                                Some(Cmd::SetEffort(e)) => effort = e,
                                Some(Cmd::SetModel(m)) => model = m,
                                Some(Cmd::SetApproval(a)) => approval = a,
                                Some(Cmd::Restart) => continue 'outer,
                                // Held, not failed: the retry is still coming, so this text still has
                                // a process to reach — see `replay`'s drain once `Ready` fires again.
                                Some(Cmd::Turn { text }) | Some(Cmd::Steer { text }) => replay.push(text),
                                _ => {}
                            }
                        }
                    }
                }
                // Out of restarts: park until the user asks for a restart.
                loop {
                    match cmds.recv().await {
                        None | Some(Cmd::Shutdown) => {
                            let _ = ev.send(HubEvent::Exited { agent: id });
                            return;
                        }
                        Some(Cmd::Restart) => {
                            restarts = 0;
                            launch_failures = 0;
                            window = Instant::now();
                            continue 'outer;
                        }
                        Some(Cmd::Turn { text }) | Some(Cmd::Steer { text }) => {
                            let _ = ev.send(HubEvent::CmdFailed { agent: id, what: "turn", error: "agent is down (press r to restart)".into(), text: Some(text) });
                        }
                        _ => {}
                    }
                }
            }
        }
    }
}

enum Exit {
    Shutdown,
    /// The process died unexpectedly (not a self-inflicted relaunch — see `Restarting`) after it
    /// had reached `Ready`.
    Crashed(String),
    /// Mantra itself asked for this relaunch — an effort/model change, a plain `Cmd::Restart`, or
    /// (Claude only) a `--resume`d session found unusable — never a failure: `agent_task` goes
    /// straight back to `'outer` with none of `Exit::Crashed`'s backoff or counters. `fresh` means
    /// the old session/thread id must not be resumed (the replacement process starts a new one).
    Restarting { reason: String, fresh: bool },
    /// The process never reached `Ready`: spawn refused, handshake failed, thread start rejected.
    /// Deterministic in practice, so `agent_task` gives it the short `MAX_LAUNCH_ATTEMPTS` budget.
    LaunchFailed(String),
}

/// What a relaunch (`Exit::Restarting`) does to state carried across it, before the next launch
/// attempt: a *fresh* one (the old session is gone — the dead-resume detection in
/// `hub/claude.rs`) drops `thread_id` so the next launch starts a brand new session, and requeues
/// the last turn's text since that turn never got a reply; anything else (an effort/model change, a
/// plain `Cmd::Restart`) resumes the same session and leaves both alone.
fn prepare_relaunch(fresh: bool, thread_id: &mut Option<String>, last_turn: &Option<String>, replay: &mut Vec<String>) {
    if fresh {
        *thread_id = None;
        if let Some(t) = last_turn {
            replay.push(t.clone());
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_process(
    id: AgentId,
    spec: &SpawnSpec,
    codex_cmd: &[String],
    ev: &mpsc::UnboundedSender<HubEvent>,
    cmds: &mut mpsc::UnboundedReceiver<Cmd>,
    thread_id: &mut Option<String>,
    effort: &mut String,
    model: &mut String,
    approval: &mut String,
    replay: &mut Vec<String>,
    last_turn: &mut Option<String>,
) -> Exit {
    // Mirrors claude.rs: whether this launch actually resumes a thread, independent of the
    // crash counter (a self-inflicted relaunch never bumps `restarts`).
    let is_restart = thread_id.is_some();
    let (conn, mut inc, mut child) = match rpc::spawn(codex_cmd, &spec.extra_args, &spec.cwd, &spec.envs) {
        Ok(x) => x,
        Err(e) => return Exit::LaunchFailed(e.to_string()),
    };
    if let Err(e) = rpc::handshake(&conn).await {
        // Codex says why it died on stderr (`/proc/self/exe` missing, a config it rejects, …);
        // that line is what the user needs, "codex process exited (-1)" is not.
        let detail = last_significant_stderr_line(&rpc::stderr_after_failure(&conn, &mut inc, Duration::from_millis(1500)).await);
        let _ = child.kill().await;
        return Exit::LaunchFailed(if detail.is_empty() { format!("handshake failed: {e}") } else { format!("handshake failed: {e} — {detail}") });
    }

    // Start or resume the thread.
    let mut params = json!({
        "model": model.clone(),
        "cwd": spec.cwd.to_string_lossy(),
        "approvalPolicy": approval.clone(),
        "sandbox": spec.sandbox,
        "serviceName": "mantra",
    });
    if spec.provider != "openai" && !spec.provider.is_empty() {
        params["modelProvider"] = json!(spec.provider);
    }
    if !spec.developer_instructions.is_empty() {
        params["developerInstructions"] = json!(spec.developer_instructions);
    }
    if !spec.config.is_empty() {
        params["config"] = Value::Object(spec.config.clone());
    }
    let result = if let Some(tid) = thread_id.clone() {
        params["threadId"] = json!(tid);
        match conn.request("thread/resume", params.clone()).await {
            Ok(v) => Ok(v),
            Err(e) => {
                crate::mlog!("agent {id}: resume failed ({e}); starting fresh thread");
                if let Some(o) = params.as_object_mut() {
                    o.remove("threadId");
                }
                if !spec.dynamic_tools.is_empty() {
                    params["dynamicTools"] = Value::Array(spec.dynamic_tools.clone());
                }
                conn.request("thread/start", params).await
            }
        }
    } else {
        if !spec.dynamic_tools.is_empty() {
            params["dynamicTools"] = Value::Array(spec.dynamic_tools.clone());
        }
        conn.request("thread/start", params).await
    };
    let v = match result {
        Ok(v) => v,
        Err(e) => {
            let tail = conn.stderr_tail();
            let _ = child.kill().await;
            return Exit::LaunchFailed(if tail.trim().is_empty() {
                format!("thread start failed: {e}")
            } else {
                format!("thread start failed: {e} — {}", tail.replace('\n', " / ").trim())
            });
        }
    };
    let tid = v.pointer("/thread/id").and_then(|t| t.as_str()).unwrap_or_default().to_string();
    if let Some(m) = v.get("model").and_then(|m| m.as_str()) {
        *model = m.to_string();
    }
    *thread_id = Some(tid.clone());
    let _ = ev.send(HubEvent::Ready { agent: id, thread_id: tid.clone(), model: model.clone(), resumed: is_restart });
    // Deliver anything held across the relaunch (a crash-backoff wait, or a Claude restart_pending
    // hold) now that there's a process again to send it to.
    for text in replay.drain(..) {
        let p = turn_params(&tid, &text, effort, model, approval);
        *last_turn = Some(text.clone());
        fire(&conn, ev, id, "turn/start", p, "turn", Some(text));
    }

    let mut current_turn: Option<String> = None;
    loop {
        tokio::select! {
            c = cmds.recv() => {
                match c {
                    None | Some(Cmd::Shutdown) => {
                        if let Some(t) = &current_turn {
                            let _ = conn.request_timeout("turn/interrupt", json!({"threadId": tid, "turnId": t}), Duration::from_secs(2)).await;
                        }
                        let _ = child.kill().await;
                        return Exit::Shutdown;
                    }
                    Some(Cmd::Turn { text }) => {
                        let p = turn_params(&tid, &text, effort, model, approval);
                        *last_turn = Some(text.clone());
                        fire(&conn, ev, id, "turn/start", p, "turn", Some(text));
                    }
                    Some(Cmd::Steer { text }) => {
                        if let Some(t) = &current_turn {
                            let p = json!({
                                "threadId": tid,
                                "input": [{ "type": "text", "text": text, "text_elements": [] }],
                                "expectedTurnId": t,
                            });
                            fire(&conn, ev, id, "turn/steer", p, "steer", Some(text));
                        } else {
                            // The turn ended just before this steer: start a new one, with the *current* policy.
                            let p = turn_params(&tid, &text, effort, model, approval);
                            fire(&conn, ev, id, "turn/start", p, "turn", Some(text));
                        }
                    }
                    Some(Cmd::Interrupt) => {
                        if let Some(t) = &current_turn {
                            fire(&conn, ev, id, "turn/interrupt", json!({"threadId": tid, "turnId": t}), "interrupt", None);
                        }
                    }
                    Some(Cmd::Compact) => fire(&conn, ev, id, "thread/compact/start", json!({"threadId": tid}), "compact", None),
                    Some(Cmd::Archive) => fire(&conn, ev, id, "thread/archive", json!({"threadId": tid}), "archive", None),
                    Some(Cmd::Shell { command }) => fire(&conn, ev, id, "thread/shellCommand", json!({"threadId": tid, "command": command}), "shell", None),
                    Some(Cmd::Respond { id: rid, result }) => conn.respond(rid, result),
                    Some(Cmd::RespondErr { id: rid, message }) => conn.respond_err(rid, -32000, &message),
                    Some(Cmd::SetEffort(e)) => *effort = e,
                    Some(Cmd::SetModel(m)) => *model = m,
                    Some(Cmd::SetApproval(a)) => {
                        // Apply to the live thread now (not just the next turn/start we send).
                        fire(&conn, ev, id, "thread/settings/update", json!({"threadId": tid, "approvalPolicy": a}), "settings", None);
                        *approval = a;
                    }
                    Some(Cmd::Restart) => {
                        let _ = child.kill().await;
                        return Exit::Restarting { reason: "restart requested".into(), fresh: false };
                    }
                    Some(Cmd::Bridge(_)) => {} // WP10.4 is Claude-only; a stray one here is a bug elsewhere, not fatal
                }
            }
            i = inc.recv() => {
                match i {
                    Some(Incoming::Notification { method, params }) => {
                        match method.as_str() {
                            "turn/started" => current_turn = params.pointer("/turn/id").and_then(|t| t.as_str()).map(|s| s.to_string()),
                            "turn/completed" => current_turn = None,
                            _ => {}
                        }
                        let _ = ev.send(HubEvent::Notif { agent: id, method, params });
                    }
                    Some(Incoming::Request { id: rid, method, params }) => {
                        let _ = ev.send(HubEvent::Request { agent: id, id: rid, method, params });
                    }
                    Some(Incoming::Closed { stderr_tail }) => {
                        let code = reap_exit_code(&mut child).await;
                        let code_s = code.map(|c| c.to_string()).unwrap_or_else(|| "unknown".into());
                        let last = last_significant_stderr_line(&stderr_tail);
                        return Exit::Crashed(if last.is_empty() {
                            format!("codex exited (code {code_s})")
                        } else {
                            format!("codex exited (code {code_s}): {last}")
                        });
                    }
                    None => {
                        let _ = child.kill().await;
                        return Exit::Crashed("codex connection lost".into());
                    }
                }
            }
        }
    }
}

/// Get the real exit code: check if it already exited, else give it up to 2s to finish on its own
/// (stdout closing usually means it's already exiting), else kill it. `None` means it could not be
/// determined (killed, or the platform doesn't report a code).
async fn reap_exit_code(child: &mut Child) -> Option<i32> {
    if let Ok(Some(status)) = child.try_wait() {
        return status.code();
    }
    match tokio::time::timeout(Duration::from_secs(2), child.wait()).await {
        Ok(Ok(status)) => status.code(),
        _ => {
            let _ = child.kill().await;
            None
        }
    }
}

/// The last non-empty stderr line that isn't just sandbox noise (a line containing `WARNING` or
/// `bubblewrap`) — falling back to the actual last line when every line is noise.
fn last_significant_stderr_line(tail: &str) -> String {
    let lines: Vec<&str> = tail.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    lines
        .iter()
        .rev()
        .find(|l| !(l.contains("WARNING") || l.contains("bubblewrap")))
        .or_else(|| lines.last())
        .map(|l| l.to_string())
        .unwrap_or_default()
}

/// `turn/start` params. Models without a reasoning-effort setting get no `effort` at all.
fn turn_params(tid: &str, text: &str, effort: &str, model: &str, approval: &str) -> Value {
    let mut p = json!({
        "threadId": tid,
        "input": [{ "type": "text", "text": text, "text_elements": [] }],
        "model": model,
        "approvalPolicy": approval,
    });
    if !effort.is_empty() {
        p["effort"] = json!(effort);
    }
    p
}

fn fire(conn: &Conn, ev: &mpsc::UnboundedSender<HubEvent>, id: AgentId, method: &'static str, params: Value, what: &'static str, text: Option<String>) {
    let conn = conn.clone();
    let ev = ev.clone();
    tokio::spawn(async move {
        if let Err(e) = conn.request_timeout(method, params, Duration::from_secs(60)).await {
            crate::mlog!("agent {id}: {method} failed: {e}");
            let _ = ev.send(HubEvent::CmdFailed { agent: id, what, error: e.message, text });
        }
    });
}

#[cfg(test)]
impl Hub {
    /// Registers a fake per-agent channel so tests can observe the `Cmd`s `Hub::send` forwards,
    /// without spawning a real Codex process.
    pub fn test_register(&mut self, id: AgentId) -> mpsc::UnboundedReceiver<Cmd> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.agents.insert(id, tx);
        rx
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_failures_stop_after_a_short_budget_while_crashes_keep_the_long_one() {
        // A process that never comes up: 3 attempts in all (2 restarts), quick backoff between them.
        assert_eq!(restart_backoff(1, true), Some(Duration::from_millis(1000)));
        assert_eq!(restart_backoff(2, true), Some(Duration::from_millis(2000)));
        assert_eq!(restart_backoff(3, true), None);
        assert_eq!(restart_backoff(4, true), None);
        // A crash mid-work keeps the classic MAX_RESTARTS budget, exponential and capped.
        for a in 1..=MAX_RESTARTS {
            assert!(restart_backoff(a, false).is_some(), "restart {a} of {MAX_RESTARTS} must still retry");
        }
        assert_eq!(restart_backoff(MAX_RESTARTS + 1, false), None);
        assert_eq!(restart_backoff(3, false), Some(Duration::from_millis(4000)));
        assert!(restart_backoff(1, false) < restart_backoff(2, false));
    }

    /// `Exit::Restarting` never reaches `restart_backoff` at all (a separate `match` arm in
    /// `agent_task` — see the code above), so the only counters-affecting logic worth a unit test
    /// is what a relaunch does to the carried-over session state: a fresh one drops it and requeues
    /// the last turn, anything else leaves it alone.
    #[test]
    fn a_fresh_relaunch_drops_the_session_and_replays_the_last_turn_a_plain_one_touches_neither() {
        let mut thread_id = Some("t1".to_string());
        let mut replay = vec![];
        prepare_relaunch(false, &mut thread_id, &Some("hello".into()), &mut replay);
        assert_eq!(thread_id, Some("t1".to_string()), "a plain relaunch (effort/model/Cmd::Restart) keeps resuming the same session");
        assert!(replay.is_empty());

        prepare_relaunch(true, &mut thread_id, &Some("hello".into()), &mut replay);
        assert_eq!(thread_id, None, "fresh drops the old session id so the next launch starts a new one");
        assert_eq!(replay, vec!["hello".to_string()], "the turn that never got a reply is requeued");

        let mut thread_id2 = Some("t2".into());
        let mut replay2 = vec![];
        prepare_relaunch(true, &mut thread_id2, &None, &mut replay2);
        assert_eq!(thread_id2, None);
        assert!(replay2.is_empty(), "no last turn yet: nothing to replay");
    }

    /// The wiring behind `restart_backoff`: a process that exits before the handshake (Codex
    /// 0.157.1 without `/proc/self/exe`) is a launch failure, reported with its stderr line,
    /// retried twice, then parked until the user asks for a restart.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_process_that_dies_before_the_handshake_parks_after_three_attempts() {
        let spec = SpawnSpec {
            cwd: std::env::temp_dir(),
            model: "m".into(),
            provider: "openai".into(),
            effort: String::new(),
            approval: "never".into(),
            sandbox: "read-only".into(),
            developer_instructions: String::new(),
            dynamic_tools: vec![],
            config: Default::default(),
            extra_args: vec![],
            resume_thread: None,
            backend: Backend::Codex,
            envs: vec![],
            claude: None,
        };
        let cmd: Vec<String> = ["sh", "-c", "echo 'ERROR codex_app_server: cannot read /proc/self/exe (simulated)' >&2; exit 3"].iter().map(|s| s.to_string()).collect();
        let (ev, mut rx) = mpsc::unbounded_channel();
        let (ctl, cmds) = mpsc::unbounded_channel();
        tokio::spawn(agent_task(7, spec, cmd, ev, cmds, Duration::ZERO));
        async fn next(rx: &mut mpsc::UnboundedReceiver<HubEvent>) -> HubEvent {
            tokio::time::timeout(Duration::from_secs(30), rx.recv()).await.expect("the agent task answers in time").expect("the agent task is alive")
        }
        let mut crashed = vec![];
        loop {
            if let HubEvent::Crashed { attempt, restarting, reason, .. } = next(&mut rx).await {
                crashed.push((attempt, restarting, reason));
                if !restarting {
                    break;
                }
            }
        }
        assert_eq!(crashed.iter().map(|c| c.0).collect::<Vec<_>>(), vec![1, 2, 3], "{crashed:?}");
        assert!(crashed[0].1 && crashed[1].1 && !crashed[2].1, "{crashed:?}");
        for (_, _, reason) in &crashed {
            assert!(reason.starts_with("handshake failed") && reason.contains("/proc/self/exe"), "{reason}");
        }
        // parked: a turn is refused until a restart is asked for
        ctl.send(Cmd::Turn { text: "hi".into() }).unwrap();
        match next(&mut rx).await {
            HubEvent::CmdFailed { what: "turn", error, .. } => assert!(error.contains("restart"), "{error}"),
            other => panic!("expected the turn to be refused, got {other:?}"),
        }
        drop(ctl);
        assert!(matches!(next(&mut rx).await, HubEvent::Exited { agent: 7 }));
    }

    #[test]
    fn picks_the_last_non_noise_stderr_line() {
        assert_eq!(last_significant_stderr_line("boot ok\nWARNING: bubblewrap sandbox degraded\n"), "boot ok");
        assert_eq!(last_significant_stderr_line("panic: thread start failed"), "panic: thread start failed");
        // when every line is noise, fall back to the last one rather than showing nothing
        assert_eq!(last_significant_stderr_line("WARNING: a\nWARNING: bubblewrap: b\n"), "WARNING: bubblewrap: b");
        assert_eq!(last_significant_stderr_line(""), "");
        assert_eq!(last_significant_stderr_line("  \n  \n"), "");
    }
}
