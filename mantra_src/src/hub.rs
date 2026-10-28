//! The hub owns every agent's backend process.
//!
//! One process per agent — `codex app-server`, `claude -p` (`hub/claude.rs`) or `bija`
//! (`hub/bija.rs`) — gives crash isolation: when one dies, only that agent restarts (resuming its
//! thread or session), everything else keeps running.

mod bija;
mod claude;
mod antigravity;

use crate::rpc::{self, Conn, Incoming};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::{Child, ChildStderr};
use tokio::sync::mpsc;

pub use crate::config::ProviderKind as Backend;
use crate::config::McpServer;
pub use bija::{sandbox_mode as bija_sandbox, BijaSpawn};
pub use claude::ClaudeSpawn;
pub use antigravity::AntigravitySpawn;

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
    /// What to call the thread in the backend (`thread/name/set`), where it supports names.
    pub thread_name: Option<String>,
    /// Which process type owns this agent (`hub::run_process` for Codex, `hub::claude` for
    /// Claude Code — WP10 — and `hub::bija` for Bija). `agent_task` dispatches on this; the
    /// approval, config and extra args above are Codex-only and ignored by the other backends.
    pub backend: Backend,
    /// Extra environment variables to set on the child (e.g. a pasted `ANTHROPIC_API_KEY`, or the
    /// key a Bija provider's `--api-key-env` names), resolved by `app::spawn_agent` from the
    /// model's provider. Every backend applies them.
    pub envs: Vec<(String, String)>,
    /// Claude-only spawn config; `None` for the other backends.
    pub claude: Option<ClaudeSpawn>,
    /// Bija-only spawn config; `None` for the other backends.
    pub bija: Option<BijaSpawn>,
    /// Native Antigravity CLI streaming configuration.
    pub antigravity: Option<AntigravitySpawn>,
    /// The MCP servers this agent gets, already resolved from settings (`Settings::mcp_for`) by
    /// `app::spawn_agent`'s callers; empty = none, which leaves every backend's launch as it was.
    /// Codex gets them as `-c mcp_servers.<name>…` (`codex_mcp_args`), Claude Code beside
    /// `mantra` in `--mcp-config`, bija as `mcp.json` (`--mcp-file`).
    pub mcp: Vec<(String, McpServer)>,
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
    /// Replace backend/provider or context configuration inside the same logical agent/channel.
    Reconfigure { spec: Box<SpawnSpec>, command: Vec<String>, resume_text: Option<String> },
    /// After a `SetModel`: whether the new model takes images. bija offers or withdraws its own
    /// vision tools (`set{vision}`) and launches with or without `--vision` from then on; Codex
    /// and Claude Code have no tools of their own to switch (Mantra's `mantra_*` vision tools
    /// refuse a model that does not see).
    SetVision(bool),
    SetApproval(String),
    /// Rename the agent's thread (the run's title changed). Codex and bija; a Claude Code session
    /// has no name to set.
    SetName(String),
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

/// When a Codex spawn requested at `now` may start, given when the previous one starts (`last`):
/// `SPAWN_STAGGER` after it, or now if that is past. Returns the wait and the start, which is what
/// the next spawn spaces itself from — measured from the previous *request* instead, a burst of k
/// spawns would all wait the same 300 ms and start together, the very thing the stagger prevents.
fn stagger(last: Option<Instant>, now: Instant) -> (Duration, Instant) {
    let start = last.map(|prev| prev + SPAWN_STAGGER).filter(|t| *t > now).unwrap_or(now);
    (start - now, start)
}

/// How long quitting waits for the bija agents to finish, all of them together — they stop in
/// parallel, so one budget covers a swarm. bija closes its transcript first (`bija/src/agent.rs`
/// `shutdown`), so even a kill here loses nothing of the session; what follows is one 1 s SIGTERM
/// grace for its background jobs and leftover command groups together (`bija/src/tools/shell.rs`),
/// and this is well over it so they end cleanly instead of being orphaned. What is still running
/// past this is killed when the runtime goes, as Codex and Claude Code processes always are;
/// `hub/bija.rs`'s longer `SHUTDOWN_GRACE` is the limit for one process stopping before a restart.
pub const QUIT_GRACE: Duration = Duration::from_secs(3);

pub struct Hub {
    ev: mpsc::UnboundedSender<HubEvent>,
    agents: HashMap<AgentId, mpsc::UnboundedSender<Cmd>>,
    tasks: HashMap<AgentId, tokio::task::AbortHandle>,
    next: AgentId,
    pub codex_cmd: Vec<String>,
    /// Command used to launch one `claude` process per Claude Code agent (WP10.3).
    pub claude_cmd: Vec<String>,
    /// Command used to launch one `bija` process per Bija agent, already resolved
    /// (`config::resolve_bija_command`).
    pub bija_cmd: Vec<String>,
    pub antigravity_cmd: Vec<String>,
    /// When the last Codex process starts — after its stagger delay (`SPAWN_STAGGER`).
    last_spawn: Option<Instant>,
    /// Path of the Unix socket `mantra mcp-bridge` subprocesses connect to (WP10.4); `None` when no
    /// `ClaudeCode` provider exists (`enable_bridge = false` in `Hub::new`), or when the socket
    /// couldn't be created (logged, never fatal — Claude agents just run without dynamic tools).
    bridge_sock: Option<PathBuf>,
    /// Why the bridge socket could not be made, when it was wanted and could not be: every Claude
    /// Code agent then runs without Mantra's tools, which the user must hear about from the app —
    /// not only from `mantra.log` (`Hub::bridge_problem`).
    bridge_problem: Option<String>,
    /// Cloned into every agent task (any may switch to bija), and dropped once its process is
    /// reaped; nothing is ever sent. `shutdown_all` lets go of the Hub's own and waits on `bija_gone`, which closes
    /// when the last clone is gone.
    bija_alive: Option<mpsc::Sender<()>>,
    bija_gone: Option<mpsc::Receiver<()>>,
}

impl Hub {
    /// `enable_bridge`: whether to open the MCP-bridge Unix socket at all — only worth doing when
    /// the registry has at least one `ClaudeCode`-kind provider (WP10.4); computed by the caller
    /// (`main.rs`) so `Hub` itself never has to know about `config::Registry`.
    pub fn new(codex_cmd: Vec<String>, claude_cmd: Vec<String>, bija_cmd: Vec<String>, antigravity_cmd: Vec<String>, ev: mpsc::UnboundedSender<HubEvent>, enable_bridge: bool) -> Hub {
        // No Claude Code provider is not a problem: nothing would have used the socket.
        let (bridge_sock, bridge_problem) = match enable_bridge.then(|| setup_bridge(ev.clone())) {
            Some(Ok(path)) => (Some(path), None),
            Some(Err(why)) => (None, Some(why)),
            None => (None, None),
        };
        let (alive, gone) = mpsc::channel(1);
        Hub { ev, agents: HashMap::new(), tasks: HashMap::new(), next: 1, codex_cmd, claude_cmd, bija_cmd, antigravity_cmd, last_spawn: None, bridge_sock, bridge_problem, bija_alive: Some(alive), bija_gone: Some(gone) }
    }

    /// Why Claude Code agents have no bridge to Mantra's tools, if the socket was wanted and could
    /// not be made (the directory, the bind, or no short path when the home's is too long).
    pub fn bridge_problem(&self) -> Option<String> {
        self.bridge_problem.clone()
    }

    /// The bridge socket path a Claude agent's `--mcp-config` should point `mantra mcp-bridge` at,
    /// if the Hub has one (WP10.4).
    pub fn bridge_sock(&self) -> Option<PathBuf> {
        self.bridge_sock.clone()
    }

    /// Start the dynamic-tool bridge lazily when a provider is added mid-session.
    pub fn ensure_bridge(&mut self) -> Option<PathBuf> {
        if tokio::runtime::Handle::try_current().is_err() {
            self.bridge_problem = Some("MCP bridge requires an active async runtime".into());
            return None;
        }
        if self.bridge_sock.is_none() {
            match setup_bridge(self.ev.clone()) {
                Ok(path) => { self.bridge_sock = Some(path); self.bridge_problem = None; }
                Err(error) => self.bridge_problem = Some(error),
            }
        }
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
            Backend::Bija => self.bija_cmd.clone(),
            Backend::Antigravity => self.antigravity_cmd.clone(),
        };
        // Only Codex shares state between its processes (`CODEX_HOME`); a swarm of eighty bija
        // agents starts at once instead of over half a minute.
        let delay = if spec.backend == Backend::Codex {
            let (delay, start) = stagger(self.last_spawn, Instant::now());
            self.last_spawn = Some(start);
            delay
        } else {
            Duration::ZERO
        };
        // Any logical agent may switch to bija later; keep it in the graceful quit budget.
        let alive = self.bija_alive.clone();
        let task = agent_task(id, spec, cmd, ev, rx, delay);
        let task = tokio::spawn(async move {
            // Held for as long as the task runs: see `Stopping`.
            let _alive = alive;
            task.await
        });
        self.tasks.insert(id, task.abort_handle());
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
        if let Some(task) = self.tasks.remove(&id) {
            // A backend can be stuck in startup/handshake before it reads the command channel.
            // Let the normal cleanup run first, then bound the stop even in that state. All
            // backend children use kill_on_drop, so aborting also ends the actual process.
            tokio::spawn(async move {
                tokio::time::sleep(QUIT_GRACE).await;
                task.abort();
            });
        }
    }

    /// Tells every agent to shut down. The returned `Stopping` is what the quit path awaits before
    /// the runtime goes: its teardown kills every child still running, and a bija agent needs a
    /// moment first to end its running command and background jobs and close its transcript.
    pub fn shutdown_all(&mut self) -> Stopping {
        for id in self.agents.keys().copied().collect::<Vec<_>>() {
            self.shutdown(id);
        }
        // "removed at exit" (§10.4) — best-effort; a leaked file under `run/` is harmless (pid-scoped
        // name, next process's bind removes any stale one anyway) but tidying up is cheap.
        if let Some(p) = self.bridge_sock.take() {
            let _ = std::fs::remove_file(p);
        }
        self.bija_alive = None;
        Stopping(self.bija_gone.take())
    }
}

/// Agents still finishing after `Hub::shutdown_all`. Every logical task is tracked because it
/// may have switched to bija; one shared grace budget bounds cleanup for all backends.
pub struct Stopping(Option<mpsc::Receiver<()>>);

impl Stopping {
    /// Waits until every bija agent's process has exited, or `QUIT_GRACE` has passed. True when
    /// they all made it.
    pub async fn wait(self) -> bool {
        self.wait_at_most(QUIT_GRACE).await
    }

    async fn wait_at_most(self, limit: Duration) -> bool {
        match self.0 {
            // `recv` answers `None` once the last sender is gone, i.e. the last bija task ended.
            Some(mut gone) => tokio::time::timeout(limit, gone.recv()).await.is_ok(),
            None => true,
        }
    }
}

/// A backend's private process group. Kept beside its Child across every await, so normal
/// cleanup runs first and cancellation still reaps npm wrappers and attached tool processes.
/// Bija's deliberately detached background jobs retain its existing graceful cleanup path.
pub(super) struct ProcessGroup(Option<u32>);

impl ProcessGroup {
    pub(super) fn new(pid: Option<u32>) -> Self {
        Self(pid.filter(|pid| *pid > 1 && *pid <= i32::MAX as u32))
    }
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.0 {
            // The spawn used process_group(0), so this is the child's group, never Mantra's.
            unsafe { libc::killpg(pid as i32, libc::SIGKILL); }
        }
    }
}

/// Opens the Hub's one MCP-bridge Unix socket at `$MANTRA_HOME/run/<pid>.sock` and spawns the
/// accept loop (WP10.4). Never fatal: when the directory or the socket itself can't be created the
/// reason is logged and returned — Claude agents then run without the `mantra_*` dynamic tools
/// (`hub::claude::build_args` only adds `--mcp-config` with a socket), and `App` says so.
fn setup_bridge(ev: mpsc::UnboundedSender<HubEvent>) -> Result<PathBuf, String> {
    let fail = |why: String| {
        crate::mlog!("mcp bridge: {why}");
        Err(why)
    };
    let dir = crate::config::home().join("run");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return fail(format!("couldn't create {}: {e}", dir.display()));
    }
    let Some(path) = bridge_sock_path(&dir, std::process::id()) else {
        return fail(format!("{} is too long for a socket path, and no short directory of our own in /tmp", dir.display()));
    };
    let _ = std::fs::remove_file(&path); // stale socket from an unclean exit; pid-scoped so this is rare
    let listener = match UnixListener::bind(&path) {
        Ok(l) => l,
        Err(e) => return fail(format!("couldn't open the socket {}: {e}", path.display())),
    };
    tokio::spawn(bridge_accept_loop(listener, ev));
    Ok(path)
}

/// The longest socket path that binds everywhere: `sun_path` is 108 bytes on Linux and 104 on
/// macOS, the closing NUL included.
const SUN_PATH_MAX: usize = 100;

/// `<run>/<pid>.sock` when it fits a socket address; else (a long `$MANTRA_HOME` or home
/// directory, where the bind would fail and every Claude agent would go without Mantra's tools)
/// `/tmp/mantra-<uid>/<pid>.sock`, in a directory only this user can enter. `None` when that
/// directory is not ours (another user's, a symlink) or cannot be made.
fn bridge_sock_path(run: &std::path::Path, pid: u32) -> Option<PathBuf> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let name = format!("{pid}.sock");
    let path = run.join(&name);
    if path.as_os_str().len() <= SUN_PATH_MAX {
        return Some(path);
    }
    let uid = std::fs::metadata(run).ok()?.uid();
    let dir = PathBuf::from(format!("/tmp/mantra-{uid}"));
    let _ = std::fs::create_dir(&dir);
    let meta = std::fs::symlink_metadata(&dir).ok()?;
    if !meta.is_dir() || meta.uid() != uid {
        return None;
    }
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).ok()?;
    Some(dir.join(name))
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
    mut spec: SpawnSpec,
    mut cmd: Vec<String>,
    ev: mpsc::UnboundedSender<HubEvent>,
    mut cmds: mpsc::UnboundedReceiver<Cmd>,
    initial_delay: Duration,
) {
    if initial_delay > Duration::ZERO {
        tokio::time::sleep(initial_delay).await;
    }
    // `thread_id` doubles as the Claude and bija backends' session id: all three are "the
    // identifier this agent resumes with after a restart", just under different names.
    let mut thread_id = spec.resume_thread.clone();
    let mut effort = spec.effort.clone();
    let mut model = spec.model.clone();
    let mut approval = spec.approval.clone();
    // bija only: `--vision` for the next launch, which a live model switch changes (`SetVision`).
    let mut vision = spec.bija.as_ref().is_some_and(|b| b.vision);
    let mut restarts: u32 = 0;
    let mut launch_failures: u32 = 0;
    let mut window = Instant::now();
    // `Cmd::Turn`/`Cmd::Steer` text held instead of being lost: by the process attempt itself
    // (`hold_text` while Claude's `restart_pending` or before bija's `ready`; Claude's dead-resume
    // detection) or by this loop's own crash-backoff wait below — either way delivered once a
    // process reaches `Ready` (every backend drains it right after sending `HubEvent::Ready`).
    let mut replay: Vec<String> = vec![];
    // The last `Cmd::Turn` text actually sent to a child, kept only so `prepare_relaunch` can requeue
    // it on a *fresh* relaunch (the old session is gone, so that turn never got a reply).
    let mut last_turn: Option<String> = None;
    // bija only: its last process ended while a turn was open (`bija::run_bija_process`).
    let mut turn_cut = false;

    'outer: loop {
        // Hub::shutdown removes the sole sender. Do not launch a process after a stop
        // requested during the Codex stagger delay or between two relaunch attempts.
        if cmds.is_closed() {
            let _ = ev.send(HubEvent::Exited { agent: id });
            return;
        }
        // Request-response tasks can outlive their child. Scope their event channel to this
        // process attempt so a late old request failure cannot overwrite the next backend's
        // healthy state after Ready has already cleared the app's switch fence.
        let (attempt_ev, mut attempt_events) = mpsc::unbounded_channel();
        let started = {
            let process = async {
                match spec.backend {
                    Backend::Codex => run_process(id, &spec, &cmd, &attempt_ev, &mut cmds, &mut thread_id, &mut effort, &mut model, &mut approval, &mut replay, &mut last_turn).await,
                    Backend::ClaudeCode => claude::run_claude_process(id, &spec, &cmd, &attempt_ev, &mut cmds, &mut thread_id, &mut effort, &mut model, &mut replay, &mut last_turn).await,
                    Backend::Bija => bija::run_bija_process(id, &spec, &cmd, &attempt_ev, &mut cmds, &mut thread_id, &mut effort, &mut model, &mut vision, &mut replay, &mut last_turn, &mut turn_cut).await,
                    Backend::Antigravity => antigravity::run_antigravity_process(id, &spec, &cmd, &attempt_ev, &mut cmds, &mut thread_id, &mut effort, &mut model, &mut replay, &mut last_turn).await,
                }
            };
            tokio::pin!(process);
            loop {
                tokio::select! {
                    exit = &mut process => break exit,
                    Some(event) = attempt_events.recv() => { let _ = ev.send(event); }
                }
            }
        };
        while let Ok(event) = attempt_events.try_recv() { let _ = ev.send(event); }
        drop(attempt_events);
        let launch_failure = matches!(started, Exit::LaunchFailed(_));
        match started {
            Exit::Shutdown => {
                let _ = ev.send(HubEvent::Exited { agent: id });
                return;
            }
            Exit::Reconfigure { spec: next, command, resume_text } => {
                spec = *next;
                cmd = command;
                thread_id = None;
                effort = spec.effort.clone();
                model = spec.model.clone();
                approval = spec.approval.clone();
                vision = spec.bija.as_ref().is_some_and(|b| b.vision);
                last_turn = None;
                turn_cut = false;
                restarts = 0;
                launch_failures = 0;
                prepare_switch_replay(&mut replay, resume_text);
                let _ = ev.send(HubEvent::Crashed { agent: id, reason: "restarting to apply model configuration".into(), restarting: true, attempt: 0 });
                continue 'outer;
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
                                Some(Cmd::SetVision(v)) => vision = v,
                                Some(Cmd::SetApproval(a)) => {
                                    if spec.backend == Backend::Codex { approval = a; }
                                    else { let _ = ev.send(unsupported_approval(id, backend_name(spec.backend))); }
                                }
                                Some(Cmd::Reconfigure { spec: next, command, resume_text }) => {
                                    spec = *next; cmd = command; thread_id = None;
                                    effort = spec.effort.clone(); model = spec.model.clone(); approval = spec.approval.clone();
                                    vision = spec.bija.as_ref().is_some_and(|b| b.vision); last_turn = None; turn_cut = false;
                                    restarts = 0; launch_failures = 0;
                                    prepare_switch_replay(&mut replay, resume_text);
                                    continue 'outer;
                                }
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
                        Some(Cmd::Reconfigure { spec: next, command, resume_text }) => {
                            spec = *next; cmd = command; thread_id = None;
                            effort = spec.effort.clone(); model = spec.model.clone(); approval = spec.approval.clone();
                            vision = spec.bija.as_ref().is_some_and(|b| b.vision); last_turn = None; turn_cut = false;
                            restarts = 0; launch_failures = 0;
                            prepare_switch_replay(&mut replay, resume_text);
                            continue 'outer;
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
                        // A model picked while it is down is the one the restart launches.
                        Some(Cmd::SetEffort(e)) => effort = e,
                        Some(Cmd::SetModel(m)) => model = m,
                        Some(Cmd::SetVision(v)) => vision = v,
                        Some(Cmd::SetApproval(a)) => {
                            if spec.backend == Backend::Codex { approval = a; }
                            else { let _ = ev.send(unsupported_approval(id, backend_name(spec.backend))); }
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
    Reconfigure { spec: Box<SpawnSpec>, command: Vec<String>, resume_text: Option<String> },
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

/// A new session receives pending input as one ordered turn. Starting several Codex turns
/// before the first turn/started would reject later messages and lose their intent.
fn prepare_switch_replay(replay: &mut Vec<String>, resume_text: Option<String>) {
    if let Some(text) = resume_text { replay.insert(0, text); }
    if replay.len() > 1 {
        *replay = vec![replay.join("\n\n")];
    }
}

fn backend_name(backend: Backend) -> &'static str {
    match backend {
        Backend::Codex => "Codex",
        Backend::ClaudeCode => "Claude Code",
        Backend::Bija => "Bija",
        Backend::Antigravity => "Antigravity",
    }
}

fn unsupported_approval(agent: AgentId, backend: &str) -> HubEvent {
    HubEvent::CmdFailed {
        agent,
        what: "approval",
        error: format!("{backend} does not support live approval mode changes"),
        text: None,
    }
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
    let _group = ProcessGroup::new(child.id());
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
        "approvalPolicy": crate::config::approval_policy(approval),
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
    if let Some(name) = &spec.thread_name {
        fire(&conn, ev, id, "thread/name/set", json!({"threadId": tid, "name": name}), "name", None);
    }
    // Deliver anything held across the relaunch (a crash-backoff wait, or a Claude restart_pending
    // hold) now that there's a process again to send it to.
    let mut awaiting_turn_start = false;
    for text in replay.drain(..) {
        let p = turn_params(&tid, &text, effort, model, approval);
        *last_turn = Some(text.clone());
        awaiting_turn_start = true;
        fire(&conn, ev, id, "turn/start", p, "turn", Some(text));
    }

    let mut current_turn: Option<String> = None;
    // Ctrl+C may arrive while turn/start is awaiting its turn/started notification. Remember it
    // and interrupt that exact turn once its id arrives, instead of silently dropping the stop.
    let mut interrupt_on_start = false;
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
                        // A previous start may have failed without a turn/completed event. An
                        // interrupt for that failed start must not stop this new turn.
                        interrupt_on_start = false;
                        awaiting_turn_start = true;
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
                            interrupt_on_start = false;
                            awaiting_turn_start = true;
                            fire(&conn, ev, id, "turn/start", p, "turn", Some(text));
                        }
                    }
                    Some(Cmd::Interrupt) => {
                        if let Some(t) = &current_turn {
                            fire(&conn, ev, id, "turn/interrupt", json!({"threadId": tid, "turnId": t}), "interrupt", None);
                        } else if awaiting_turn_start {
                            interrupt_on_start = true;
                        }
                    }
                    Some(Cmd::Compact) => fire(&conn, ev, id, "thread/compact/start", json!({"threadId": tid}), "compact", None),
                    Some(Cmd::Archive) => fire(&conn, ev, id, "thread/archive", json!({"threadId": tid}), "archive", None),
                    Some(Cmd::Shell { command }) => fire(&conn, ev, id, "thread/shellCommand", json!({"threadId": tid, "command": command}), "shell", None),
                    Some(Cmd::Respond { id: rid, result }) => conn.respond(rid, result),
                    Some(Cmd::RespondErr { id: rid, message }) => conn.respond_err(rid, -32000, &message),
                    Some(Cmd::SetEffort(e)) => *effort = e,
                    Some(Cmd::SetModel(m)) => *model = m,
                    Some(Cmd::SetVision(_)) => {}
                    Some(Cmd::SetName(n)) => fire(&conn, ev, id, "thread/name/set", json!({"threadId": tid, "name": n}), "name", None),
                    Some(Cmd::SetApproval(a)) => {
                        // Apply to the live thread now (not just the next turn/start we send).
                        fire(&conn, ev, id, "thread/settings/update", json!({"threadId": tid, "approvalPolicy": crate::config::approval_policy(&a)}), "settings", None);
                        *approval = a;
                    }
                    Some(Cmd::Reconfigure { spec, command, resume_text }) => {
                        let _ = child.kill().await;
                        return Exit::Reconfigure { spec, command, resume_text };
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
                            "turn/started" => {
                                current_turn = params.pointer("/turn/id").and_then(|t| t.as_str()).map(str::to_string);
                                awaiting_turn_start = false;
                                if interrupt_on_start {
                                    if let Some(t) = &current_turn {
                                        fire(&conn, ev, id, "turn/interrupt", json!({"threadId": tid, "turnId": t}), "interrupt", None);
                                        interrupt_on_start = false;
                                    }
                                }
                            }
                            "turn/completed" => { current_turn = None; awaiting_turn_start = false; interrupt_on_start = false; }
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

#[cfg(test)]
#[test]
fn codex_turn_permission_uses_wire_policy_not_ui_label() {
    let p = turn_params("thread", "hello", "", "model", crate::config::BYPASS_SANDBOX_DANGER);
    assert_eq!(p["approvalPolicy"], "never");
    assert_eq!(turn_params("thread", "hello", "", "model", "on-request")["approvalPolicy"], "on-request");
}

/// `turn/start` params. Models without a reasoning-effort setting get no `effort` at all.
fn turn_params(tid: &str, text: &str, effort: &str, model: &str, approval: &str) -> Value {
    let mut p = json!({
        "threadId": tid,
        "input": [{ "type": "text", "text": text, "text_elements": [] }],
        "model": model,
        "approvalPolicy": crate::config::approval_policy(approval),
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

// ── MCP servers and images, for every backend ─────────────────────────────────────────────────

/// `-c` overrides that give a Codex process the agent's MCP servers: `mcp_servers.<name>.command`,
/// `.args`, `.enabled=true` and, when there is one, `.env`, each value a TOML literal. Codex
/// launches and speaks to them itself; their calls come back as `mcpToolCall` items under the
/// server's own name. `-c` merges at dotted leaf paths, so a same-named server in
/// `~/.codex/config.toml` keeps its other keys (its timeouts) and takes these — `enabled` among
/// them, or one switched off there would stay off, silently, for an agent given it here. The env
/// values ride on argv, as with Claude Code's `--mcp-config`: a secret is better kept in the
/// environment Mantra itself starts with.
pub fn codex_mcp_args(servers: &[(String, McpServer)]) -> Vec<String> {
    let mut out = vec![];
    for (name, s) in servers {
        let base = format!("mcp_servers.{name}");
        out.push("-c".into());
        out.push(format!("{base}.command={}", toml_quote(&s.command)));
        out.push("-c".into());
        out.push(format!("{base}.args=[{}]", s.args.iter().map(|a| toml_quote(a)).collect::<Vec<_>>().join(",")));
        out.push("-c".into());
        out.push(format!("{base}.enabled=true"));
        if !s.env.is_empty() {
            let pairs: Vec<String> = s.env.iter().map(|(k, v)| format!("{}={}", toml_quote(k), toml_quote(v))).collect();
            out.push("-c".into());
            out.push(format!("{base}.env={{{}}}", pairs.join(",")));
        }
    }
    out
}

/// A TOML basic string, every control character escaped too: Codex parses a `-c` value as TOML
/// and takes one that does not parse as a plain string, so an argument with a newline in it would
/// quietly arrive with its quotes rather than fail.
fn toml_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `mcp__<server>__<tool>` → `(server, tool)`: how Claude Code and bija name an MCP server's
/// tools. A server name never holds `__` (`config::mcp_name_problem`), so the first `__` after the
/// prefix ends it; the tool may be called anything.
fn mcp_tool_name(name: &str) -> Option<(&str, &str)> {
    let (server, tool) = name.strip_prefix("mcp__")?.split_once("__")?;
    (!server.is_empty() && !tool.is_empty()).then_some((server, tool))
}

/// `data:<mime>;base64,<data>` → `(mime, data)`: an `inputImage` item's `imageUrl` as the engine
/// builds it (spec §2.3). Anything else — a remote URL — is not an image Mantra can hand on.
pub fn data_url_parts(url: &str) -> Option<(&str, &str)> {
    let (head, data) = url.strip_prefix("data:")?.split_once(',')?;
    let mime = head.strip_suffix(";base64")?;
    (mime.starts_with("image/") && !data.is_empty()).then_some((mime, data))
}

/// `(mime, base64)` of every `inputImage` item in an engine answer (`{"contentItems":[…],
/// "success":…}`); none for the answers that carry only text, which is nearly all of them.
fn answer_images(result: &Value) -> Vec<(String, String)> {
    let Some(items) = result.get("contentItems").and_then(|c| c.as_array()) else { return vec![] };
    items
        .iter()
        .filter(|i| i.get("type").and_then(|t| t.as_str()) == Some("inputImage"))
        .filter_map(|i| i.get("imageUrl").and_then(|u| u.as_str()).and_then(data_url_parts))
        .map(|(mime, data)| (mime.to_string(), data.to_string()))
        .collect()
}

// ── shared by the NDJSON backends (`hub/claude.rs`, `hub/bija.rs`) ──────────────────────────────

/// One line of an NDJSON stream, or its end.
enum LineIn {
    Line(Value),
    Closed,
}

/// Reads NDJSON into `tx` — one `Value` per line, then `Closed` — from a child's stdout or the MCP
/// bridge's socket. By bytes, not `lines()`: `lines()` fails on a line that is not UTF-8, which the
/// backend would take for the process exiting; here such a line costs only itself. Every line is
/// logged as `<label>: …` unless `quiet` calls it one of the stream's own ticks (a token delta, a
/// progress beat); one that is not JSON (a CLI's stray warning) is logged and skipped.
async fn read_ndjson<R: AsyncRead + Unpin>(r: R, tx: mpsc::UnboundedSender<LineIn>, label: String, quiet: fn(&Value) -> bool) {
    let mut reader = BufReader::with_capacity(1 << 16, r);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let line = String::from_utf8_lossy(&buf);
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(line) {
            Ok(v) => {
                if !quiet(&v) {
                    crate::mlog!("{label}: {}", crate::util::trunc(line, 200));
                }
                if tx.send(LineIn::Line(v)).is_err() {
                    return;
                }
            }
            Err(e) => crate::mlog!("{label}: not JSON ({e}): {}", crate::util::trunc(line, 200)),
        }
    }
    let _ = tx.send(LineIn::Closed);
}

/// The last few non-empty lines a child wrote to stderr, ANSI stripped: what its crash reason quotes.
type StderrTail = Arc<Mutex<VecDeque<String>>>;

const STDERR_TAIL_LINES: usize = 5;

/// Logs every stderr line of a child as `[<label> stderr] …` and keeps the last
/// `STDERR_TAIL_LINES` non-empty ones in `tail`, until the pipe closes with the process.
async fn read_stderr_tail(stderr: ChildStderr, tail: StderrTail, label: &'static str) {
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(l)) = lines.next_line().await {
        crate::mlog!("[{label} stderr] {}", crate::util::trunc(&l, 400));
        let clean = crate::util::strip_ansi(&l);
        if !clean.trim().is_empty() {
            if let Ok(mut t) = tail.lock() {
                t.push_back(clean);
                while t.len() > STDERR_TAIL_LINES {
                    t.pop_front();
                }
            }
        }
    }
}

/// The tail's lines joined with `sep`.
fn stderr_tail_text(tail: &StderrTail, sep: &str) -> String {
    tail.lock().map(|t| t.iter().cloned().collect::<Vec<_>>().join(sep)).unwrap_or_default()
}

/// Writes one NDJSON line and flushes: a child's stdin, or a live MCP-bridge connection.
async fn write_line<W: AsyncWriteExt + Unpin>(w: &mut W, v: &Value) -> std::io::Result<()> {
    let mut line = v.to_string();
    line.push('\n');
    w.write_all(line.as_bytes()).await?;
    w.flush().await
}

/// Gives a child that was asked to go `grace` to exit by itself, then kills it.
async fn wait_or_kill(child: &mut Child, grace: Duration) {
    if tokio::time::timeout(grace, child.wait()).await.is_err() {
        let _ = child.kill().await;
    }
}

/// Holds a `Cmd::Turn`/`Cmd::Steer`'s text in `replay` while `hold` — while the process cannot be
/// trusted with it: Claude's is about to be relaunched, bija's has not reached `ready` and may
/// still fail to — and passes every other command, or any text when not holding, through.
/// `agent_task` carries `replay` across attempts, and the next `Ready` delivers it.
fn hold_text(hold: bool, replay: &mut Vec<String>, c: Cmd) -> Option<Cmd> {
    match c {
        Cmd::Turn { text } | Cmd::Steer { text } if hold => {
            replay.push(text);
            None
        }
        c => Some(c),
    }
}

/// A turn's text that never reached the process, handed back to the user with the failure.
fn stdin_lost(agent: AgentId, backend: &str, text: String) -> HubEvent {
    HubEvent::CmdFailed { agent, what: "turn", error: format!("failed to write to {backend}'s stdin"), text: Some(text) }
}

/// A thread's running token counts, as a gauge update carries them: every request's input plus
/// output so far (Codex's `total.totalTokens`), the part of that served from the cache, and the
/// context window when the backend knows it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Totals {
    tokens: u64,
    cached: u64,
    window: Option<u64>,
}

/// A Codex-shaped `thread/tokenUsage/updated`: the last call's context and the running totals.
/// `Agent` reads `last.inputTokens + last.outputTokens` as the context in use, so a backend whose
/// API counts cache reads and writes apart folds them into `last_in` up front.
fn token_usage(agent: AgentId, last_in: u64, last_out: u64, totals: Totals) -> HubEvent {
    let mut tu = json!({"last": {"inputTokens": last_in, "outputTokens": last_out}, "total": {"totalTokens": totals.tokens, "cachedInputTokens": totals.cached}});
    if let Some(w) = totals.window {
        tu["modelContextWindow"] = json!(w);
    }
    HubEvent::Notif { agent, method: "thread/tokenUsage/updated".into(), params: json!({"tokenUsage": tu}) }
}

/// A compaction the way Codex reports its own `contextCompaction` item, which `Agent` shows as one
/// `⇣ context compacted · from → to` line: the gauge at the old size (the item takes it as its
/// `from`), the item started and completed, then the gauge at the new size (`Agent` fills in `to`
/// once it drops).
fn compaction(agent: AgentId, id: &str, from: u64, to: u64, totals: Totals) -> [HubEvent; 4] {
    let item = |method: &str| HubEvent::Notif { agent, method: method.into(), params: json!({"item": {"id": id, "type": "contextCompaction"}}) };
    [token_usage(agent, from, 0, totals), item("item/started"), item("item/completed"), token_usage(agent, to, 0, totals)]
}

/// A liveness tick for `Agent` (a Mantra-only method, see `Agent::apply`): bumps `last_event`, which
/// the watchdog and the stall tripwire read, and relabels the activity when there is a label.
fn activity(agent: AgentId, label: Option<&str>) -> HubEvent {
    let params = match label {
        Some(l) => json!({"activity": l}),
        None => json!({}),
    };
    HubEvent::Notif { agent, method: "mantra/activity".into(), params }
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

    /// A burst of Codex spawns requested in the same instant starts 0, 300, 600 ms apart — not one
    /// now and all the others together at 300 ms — and a spawn after a quiet spell starts at once.
    #[test]
    fn codex_spawns_in_a_burst_start_one_stagger_apart() {
        let t0 = Instant::now();
        let mut last = None;
        let mut waits = vec![];
        for _ in 0..4 {
            let (wait, start) = stagger(last, t0);
            waits.push(wait);
            last = Some(start);
        }
        assert_eq!(waits, (0..4).map(|k| SPAWN_STAGGER * k).collect::<Vec<_>>());
        // half a stagger after the burst's last start: the rest of the gap
        let later = last.unwrap() + SPAWN_STAGGER / 2;
        assert_eq!(stagger(last, later), (SPAWN_STAGGER / 2, last.unwrap() + SPAWN_STAGGER));
        // long after: no wait, and the next one spaces itself from now
        let quiet = last.unwrap() + Duration::from_secs(5);
        assert_eq!(stagger(last, quiet), (Duration::ZERO, quiet));
        assert_eq!(stagger(None, t0), (Duration::ZERO, t0));
    }

    /// A `$MANTRA_HOME` too long for a socket address (a deep project dir, a CI scratch dir) moves
    /// the bridge socket to `/tmp/mantra-<uid>/`, owner-only, instead of failing the bind and
    /// leaving every Claude agent without Mantra's tools; a short one keeps it under `run/`.
    #[test]
    fn the_bridge_socket_moves_to_a_short_dir_when_the_home_is_too_long() {
        use std::os::unix::fs::PermissionsExt;
        let short = PathBuf::from(format!("/tmp/mantra-hub-test-{}", std::process::id()));
        std::fs::create_dir_all(&short).unwrap();
        assert_eq!(bridge_sock_path(&short, 42), Some(short.join("42.sock")));
        let deep = short.join("a-rather-long-directory-name-".repeat(4)).join("run");
        std::fs::create_dir_all(&deep).unwrap();
        let p = bridge_sock_path(&deep, 42).unwrap();
        assert!(p.as_os_str().len() <= SUN_PATH_MAX && p.file_name().unwrap() == "42.sock" && !p.starts_with(&deep), "{}", p.display());
        let dir = p.parent().unwrap();
        assert!(dir.to_string_lossy().starts_with("/tmp/mantra-"));
        assert_eq!(std::fs::metadata(dir).unwrap().permissions().mode() & 0o777, 0o700);
        // and it binds
        let _l = std::os::unix::net::UnixListener::bind(&p).unwrap();
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_dir_all(&short);
    }

    /// A bridge socket that cannot be made is a reason the app can show, not only a log line; a Hub
    /// that never wanted one (no Claude Code provider) has no problem to report. The home sits
    /// under a regular file, which no one — root included — can make a directory in.
    #[tokio::test]
    async fn a_bridge_socket_that_cannot_be_made_is_a_reason_the_hub_gives() {
        crate::web::snapshot::tests::with_mantra_home(|| {
            let file = crate::config::home().join("not-a-dir");
            std::fs::write(&file, "").unwrap();
            let home = std::env::var("MANTRA_HOME").unwrap();
            std::env::set_var("MANTRA_HOME", file.join("home"));
            let (ev, _rx) = mpsc::unbounded_channel();
            let hub = Hub::new(vec![], vec![], vec![], vec![], ev.clone(), true);
            let quiet = Hub::new(vec![], vec![], vec![], vec![], ev, false);
            std::env::set_var("MANTRA_HOME", home);
            let why = hub.bridge_problem().expect("the socket could not be made");
            assert!(why.contains("couldn't create") && why.contains("not-a-dir"), "{why}");
            assert_eq!(hub.bridge_sock(), None);
            assert_eq!(quiet.bridge_problem(), None, "no bridge wanted, no problem");
        });
    }

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

    #[cfg(unix)]
    #[tokio::test]
    async fn active_agent_changes_backend_without_reusing_session_ids_or_losing_text() {
        let dir = std::env::temp_dir().join(format!("mantra-backend-switch-{}", crate::util::uuid_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        // Each stand-in speaks its own backend protocol. Launch/input logs prove we did not
        // resume an incompatible id or send a queued request to the process being replaced.
        let script = r#"import sys,json,os
kind=os.environ['FAKE_BACKEND']
with open(os.environ['TRACE'],'a') as f: f.write(kind+' argv '+json.dumps(sys.argv)+'\n')
def emit(v): print(json.dumps(v),flush=True)
if kind=='bija': emit({'ev':'ready','session':'fresh-bija','model':'b','resumed':False,'pending':False})
for line in sys.stdin:
 v=json.loads(line)
 with open(os.environ['TRACE'],'a') as f: f.write(kind+' input '+line)
 if kind=='codex':
  method=v.get('method')
  if method=='initialize': emit({'id':v['id'],'result':{}})
  elif method=='thread/start': emit({'id':v['id'],'result':{'thread':{'id':'fresh-codex'},'model':'c'}})
  elif method=='turn/start':
   emit({'id':v['id'],'result':{}})
   emit({'method':'turn/started','params':{'turn':{'id':'active'}}})
 elif kind=='bija':
  if v.get('op')=='shutdown': break
  if v.get('op')=='prompt': emit({'ev':'turn_started','turn':'b-turn'})
 else:
  emit({'type':'system','subtype':'init','session_id':'fresh-claude','model':'cc'})
  emit({'type':'assistant','message':{'id':'msg','content':[{'type':'text','text':'continued'}]}})
  emit({'type':'result','subtype':'success','num_turns':1,'duration_api_ms':5,'is_error':False})
"#;
        let command = vec!["python3".into(), "-u".into(), "-c".into(), script.into()];
        let make_spec = |backend: Backend, name: &str| SpawnSpec {
            cwd: dir.clone(), model: name.into(), provider: name.into(), effort: "low".into(),
            approval: "never".into(), sandbox: "read-only".into(), developer_instructions: "task handoff".into(),
            dynamic_tools: vec![], config: Default::default(), extra_args: vec![], resume_thread: None,
            thread_name: None, backend,
            envs: vec![("FAKE_BACKEND".into(), name.into()), ("TRACE".into(), dir.join("trace").to_string_lossy().into_owned())],
            claude: (backend == Backend::ClaudeCode).then(|| ClaudeSpawn { auth: "subscription".into(), base_url: String::new(), autocompact: 0, system_prompt: "task handoff".into(), mcp_sock: None, strict_mcp: false }),
            bija: (backend == Backend::Bija).then(|| BijaSpawn { base_url: "http://unused".into(), wire: "openai-chat".into(), api_key_env: None, context_window: 1000, compact_pct: 85, system_prompt: "task handoff".into(), home: dir.clone(), sandbox: "none".into(), writable_roots: vec![], network: false, vision: false }),
            antigravity: None,
            mcp: vec![],
        };
        let (ev, mut events) = mpsc::unbounded_channel();
        let (tx, cmds) = mpsc::unbounded_channel();
        tokio::spawn(agent_task(42, make_spec(Backend::Codex, "codex"), command.clone(), ev, cmds, Duration::ZERO));
        async fn until(events: &mut mpsc::UnboundedReceiver<HubEvent>, expected: &str) {
            loop {
                match tokio::time::timeout(Duration::from_secs(8), events.recv()).await.unwrap().unwrap() {
                    HubEvent::Ready { agent, thread_id, resumed, .. } => {
                        assert_eq!(agent, 42);
                        assert!(!resumed, "every backend switch must start fresh");
                        if thread_id == expected { return; }
                    }
                    HubEvent::Notif { agent, method, .. } if method == expected => { assert_eq!(agent, 42); return; }
                    HubEvent::Crashed { attempt: 0, .. } => {}
                    HubEvent::Crashed { reason, .. } | HubEvent::CmdFailed { error: reason, .. } => panic!("{reason}"),
                    _ => {}
                }
            }
        }
        until(&mut events, "fresh-codex").await;
        tx.send(Cmd::Turn { text: "active original task".into() }).unwrap();
        until(&mut events, "turn/started").await;
        tx.send(Cmd::Reconfigure { spec: Box::new(make_spec(Backend::Bija, "bija")), command: command.clone(), resume_text: Some("continue after switch".into()) }).unwrap();
        until(&mut events, "fresh-bija").await;
        until(&mut events, "turn/started").await;
        tx.send(Cmd::Reconfigure { spec: Box::new(make_spec(Backend::ClaudeCode, "claude")), command: command.clone(), resume_text: Some("pending new instruction".into()) }).unwrap();
        until(&mut events, "fresh-claude").await;
        until(&mut events, "turn/completed").await;
        tx.send(Cmd::Reconfigure { spec: Box::new(make_spec(Backend::Codex, "codex")), command, resume_text: None }).unwrap();
        until(&mut events, "fresh-codex").await;
        tx.send(Cmd::Shutdown).unwrap();
        let trace = std::fs::read_to_string(dir.join("trace")).unwrap();
        assert!(!trace.contains("thread/resume") && !trace.contains("--resume"), "{trace}");
        assert_eq!(trace.matches("continue after switch").count(), 1, "{trace}");
        assert_eq!(trace.matches("pending new instruction").count(), 1, "{trace}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn backend_process_group_cleanup_ends_a_wrappers_child() {
        let mut command = tokio::process::Command::new("sh");
        command.args(["-c", "sleep 60 & echo $!; wait"])
            .stdout(std::process::Stdio::piped()).kill_on_drop(true).process_group(0);
        let mut child = command.spawn().unwrap();
        let group = ProcessGroup::new(child.id());
        let mut lines = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
        let worker = tokio::time::timeout(Duration::from_secs(2), lines.next_line()).await.unwrap().unwrap().unwrap();
        let worker: u32 = worker.trim().parse().unwrap();
        drop(group);
        tokio::time::timeout(Duration::from_secs(2), child.wait()).await.unwrap().unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                // An orphaned zombie is already dead and merely awaiting the host's reaper.
                let dead = std::fs::read_to_string(format!("/proc/{worker}/stat")).map(|s| s.rsplit_once(") ").map(|(_, rest)| rest.starts_with('Z') || rest.starts_with('X')).unwrap_or(false)).unwrap_or(true);
                if dead { break; }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }).await.expect("the wrapper's worker process must not survive");
    }

    /// An unresponsive app-server never reaches the command loop. Remote stop must still
    /// terminate its process within the grace period and must never enter crash/restart logic.
    #[cfg(unix)]
    #[tokio::test]
    async fn stopping_during_codex_handshake_is_bounded() {
        let dir = std::env::temp_dir().join(format!("mantra-stop-handshake-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pid_file = dir.join("pid");
        let _ = std::fs::remove_file(&pid_file);
        let spec = SpawnSpec {
            cwd: dir.clone(), model: "m".into(), provider: "openai".into(), effort: String::new(),
            approval: "never".into(), sandbox: "read-only".into(), developer_instructions: String::new(),
            dynamic_tools: vec![], config: Default::default(), extra_args: vec![], resume_thread: None,
            thread_name: None, backend: Backend::Codex, envs: vec![], claude: None, bija: None, antigravity: None, mcp: vec![],
        };
        let cmd = vec!["sh".into(), "-c".into(), "echo $$ > pid; exec sleep 60".into()];
        let (ev, mut rx) = mpsc::unbounded_channel();
        let mut hub = Hub::new(cmd, vec![], vec![], vec![], ev, false);
        hub.spawn(1, spec);
        tokio::time::timeout(Duration::from_secs(5), async {
            while !pid_file.exists() { tokio::time::sleep(Duration::from_millis(20)).await; }
        }).await.expect("mock process starts");
        let pid = std::fs::read_to_string(&pid_file).unwrap().trim().to_string();
        let task = hub.tasks[&1].clone();
        hub.shutdown(1);
        assert!(!hub.has_agent(1));
        tokio::time::timeout(QUIT_GRACE + Duration::from_secs(2), async {
            while !task.is_finished() { tokio::time::sleep(Duration::from_millis(20)).await; }
        }).await.expect("startup is cancellable within the grace period");
        // kill_on_drop is reaped asynchronously by Tokio; allow the reaper a moment as well.
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if !std::process::Command::new("kill").args(["-0", &pid]).stderr(std::process::Stdio::null()).status().unwrap().success() { break; }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }).await.expect("the app-server process was killed");
        assert!(!matches!(rx.try_recv(), Ok(HubEvent::Crashed { .. })), "shutdown must not restart");
        std::fs::remove_dir_all(dir).unwrap();
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
            thread_name: None,
            backend: Backend::Codex,
            envs: vec![],
            claude: None,
            bija: None,
            antigravity: None,
            mcp: vec![],
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

    /// A thread is given its name (`<run title> · <role>`) as soon as it is ready, and a later
    /// `Cmd::SetName` renames it — both through the app-server's `thread/name/set`.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_codex_thread_is_named_when_ready_and_renamed_on_request() {
        let dir = std::env::temp_dir().join(format!("mantra-hub-name-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("names.log");
        // a minimal app-server: answers the handshake and thread/start, records thread/name/set
        let script = r#"while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/^{"id":\([0-9]*\),.*/\1/p')
  case "$line" in
    *'"method":"initialize"'*) printf '{"id":%s,"result":{}}\n' "$id";;
    *'"method":"thread/start"'*) printf '{"id":%s,"result":{"thread":{"id":"thr-1"},"model":"m"}}\n' "$id";;
    *'"method":"thread/name/set"'*) printf '%s\n' "$line" >> "$NAMES_LOG"; printf '{"id":%s,"result":{}}\n' "$id";;
  esac
done"#;
        let spec = SpawnSpec {
            cwd: dir.clone(),
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
            thread_name: Some("Notes sync · planner".into()),
            backend: Backend::Codex,
            envs: vec![("NAMES_LOG".into(), log.to_string_lossy().to_string())],
            claude: None,
            bija: None,
            antigravity: None,
            mcp: vec![],
        };
        let cmd: Vec<String> = ["sh", "-c", script].iter().map(|s| s.to_string()).collect();
        let (ev, mut rx) = mpsc::unbounded_channel();
        let (ctl, cmds) = mpsc::unbounded_channel();
        tokio::spawn(agent_task(9, spec, cmd, ev, cmds, Duration::ZERO));
        loop {
            match tokio::time::timeout(Duration::from_secs(20), rx.recv()).await.expect("the agent comes up").expect("alive") {
                HubEvent::Ready { thread_id, .. } => {
                    assert_eq!(thread_id, "thr-1");
                    break;
                }
                HubEvent::Crashed { reason, .. } => panic!("the fake app-server failed: {reason}"),
                _ => {}
            }
        }
        ctl.send(Cmd::SetName("Notes sync · qa".into())).unwrap();
        let mut lines = vec![];
        for _ in 0..100 {
            lines = std::fs::read_to_string(&log).unwrap_or_default().lines().map(str::to_string).collect();
            if lines.len() >= 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("\"threadId\":\"thr-1\"") && l.contains("Notes sync · planner")), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("\"threadId\":\"thr-1\"") && l.contains("Notes sync · qa")), "{lines:?}");
        drop(ctl);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A stray stop while idle must not interrupt the next unrelated turn. A stop while the
    /// turn is open still addresses the exact turn id.
    #[cfg(unix)]
    #[tokio::test]
    async fn idle_codex_interrupt_does_not_carry_into_the_next_turn() {
        let dir = std::env::temp_dir().join(format!("mantra-hub-idle-interrupt-{}", crate::util::uuid_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("interrupts.log");
        let script = r#"while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/^{"id":\([0-9]*\),.*/\1/p')
  case "$line" in
    *'"method":"initialize"'*) printf '{"id":%s,"result":{}}\n' "$id";;
    *'"method":"thread/start"'*) printf '{"id":%s,"result":{"thread":{"id":"thr"}}}\n' "$id";;
    *'"method":"turn/start"'*) printf '{"id":%s,"result":{}}\n' "$id"; printf '{"method":"turn/started","params":{"turn":{"id":"t1"}}}\n';;
    *'"method":"turn/interrupt"'*) printf '%s\n' "$line" >> "$INTERRUPTS_LOG"; printf '{"id":%s,"result":{}}\n' "$id";;
  esac
done"#;
        let spec = SpawnSpec {
            cwd: dir.clone(), model: "m".into(), provider: "openai".into(), effort: String::new(),
            approval: "never".into(), sandbox: "read-only".into(), developer_instructions: String::new(),
            dynamic_tools: vec![], config: Default::default(), extra_args: vec![], resume_thread: None,
            thread_name: None, backend: Backend::Codex,
            envs: vec![("INTERRUPTS_LOG".into(), log.to_string_lossy().into_owned())],
            claude: None, bija: None, antigravity: None, mcp: vec![],
        };
        let (ev, mut events) = mpsc::unbounded_channel();
        let (ctl, cmds) = mpsc::unbounded_channel();
        tokio::spawn(agent_task(91, spec, ["sh", "-c", script].map(str::to_string).to_vec(), ev, cmds, Duration::ZERO));
        assert!(matches!(tokio::time::timeout(Duration::from_secs(5), events.recv()).await.unwrap(), Some(HubEvent::Ready { .. })));
        ctl.send(Cmd::Interrupt).unwrap();
        ctl.send(Cmd::Turn { text: "hello".into() }).unwrap();
        loop {
            match tokio::time::timeout(Duration::from_secs(5), events.recv()).await.unwrap() {
                Some(HubEvent::Notif { method, .. }) if method == "turn/started" => break,
                Some(HubEvent::CmdFailed { error, .. }) => panic!("{error}"),
                Some(HubEvent::Crashed { reason, .. }) => panic!("{reason}"),
                _ => {}
            }
        }
        assert!(!log.exists(), "idle interrupt must not affect the new turn");
        ctl.send(Cmd::Interrupt).unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !log.exists() { tokio::time::sleep(Duration::from_millis(20)).await; }
        }).await.unwrap();
        assert!(std::fs::read_to_string(&log).unwrap().contains("\"turnId\":\"t1\""));
        ctl.send(Cmd::Shutdown).unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejected_codex_start_does_not_carry_an_interrupt_to_the_next_turn() {
        let dir = std::env::temp_dir().join(format!("mantra-hub-rejected-start-{}", crate::util::uuid_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("interrupts.log");
        let script = r#"turns=0
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/^{"id":\([0-9]*\),.*/\1/p')
  case "$line" in
    *'"method":"initialize"'*) printf '{"id":%s,"result":{}}\n' "$id";;
    *'"method":"thread/start"'*) printf '{"id":%s,"result":{"thread":{"id":"thr"}}}\n' "$id";;
    *'"method":"turn/start"'*)
      turns=$((turns + 1))
      if [ "$turns" -eq 1 ]; then printf '{"id":%s,"error":{"code":-1,"message":"rejected"}}\n' "$id"
      else printf '{"id":%s,"result":{}}\n' "$id"; printf '{"method":"turn/started","params":{"turn":{"id":"t2"}}}\n'; fi;;
    *'"method":"turn/interrupt"'*) printf '%s\n' "$line" >> "$INTERRUPTS_LOG"; printf '{"id":%s,"result":{}}\n' "$id";;
  esac
done"#;
        let spec = SpawnSpec {
            cwd: dir.clone(), model: "m".into(), provider: "openai".into(), effort: String::new(),
            approval: "never".into(), sandbox: "read-only".into(), developer_instructions: String::new(),
            dynamic_tools: vec![], config: Default::default(), extra_args: vec![], resume_thread: None,
            thread_name: None, backend: Backend::Codex,
            envs: vec![("INTERRUPTS_LOG".into(), log.to_string_lossy().into_owned())],
            claude: None, bija: None, antigravity: None, mcp: vec![],
        };
        let (ev, mut events) = mpsc::unbounded_channel();
        let (ctl, cmds) = mpsc::unbounded_channel();
        tokio::spawn(agent_task(92, spec, ["sh", "-c", script].map(str::to_string).to_vec(), ev, cmds, Duration::ZERO));
        assert!(matches!(tokio::time::timeout(Duration::from_secs(5), events.recv()).await.unwrap(), Some(HubEvent::Ready { .. })));
        ctl.send(Cmd::Turn { text: "rejected".into() }).unwrap();
        assert!(matches!(tokio::time::timeout(Duration::from_secs(5), events.recv()).await.unwrap(), Some(HubEvent::CmdFailed { what: "turn", .. })));
        ctl.send(Cmd::Interrupt).unwrap();
        ctl.send(Cmd::Turn { text: "accepted".into() }).unwrap();
        loop {
            match tokio::time::timeout(Duration::from_secs(5), events.recv()).await.unwrap() {
                Some(HubEvent::Notif { method, .. }) if method == "turn/started" => break,
                Some(HubEvent::CmdFailed { error, .. }) => panic!("{error}"),
                Some(HubEvent::Crashed { reason, .. }) => panic!("{reason}"),
                _ => {}
            }
        }
        // fire sends the interrupt asynchronously; let the fake receive anything dispatched by
        // turn/started before checking its log.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!log.exists(), "a rejected turn's interrupt must not stop the next turn");
        ctl.send(Cmd::Shutdown).unwrap();
        let _ = std::fs::remove_dir_all(dir);
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

    /// While held, turn and steer texts go into `replay` in the order they arrived and nothing
    /// else is touched; once the hold lifts, texts pass straight through again.
    #[test]
    fn texts_are_held_in_order_and_other_commands_pass_through() {
        let mut replay = vec![];
        assert!(hold_text(true, &mut replay, Cmd::Turn { text: "a".into() }).is_none());
        assert!(hold_text(true, &mut replay, Cmd::Steer { text: "b".into() }).is_none());
        assert!(matches!(hold_text(true, &mut replay, Cmd::Interrupt), Some(Cmd::Interrupt)), "only text is held");
        assert_eq!(replay, vec!["a".to_string(), "b".to_string()], "held texts come out in the order they arrived");
        assert!(matches!(hold_text(false, &mut replay, Cmd::Turn { text: "c".into() }), Some(Cmd::Turn { text }) if text == "c"), "no hold: straight through, untouched");
        assert_eq!(replay.len(), 2, "a passed-through text is never also held");
    }

    /// A line that is not UTF-8 (or not JSON) costs that line, never the rest of the stream.
    #[tokio::test]
    async fn a_bad_line_costs_only_itself() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        read_ndjson(&b"{\"n\":1}\n\xff\xfe garbage\n\nnot json\n{\"n\":2}\n"[..], tx, "test".into(), |_| true).await;
        let mut got = vec![];
        while let Some(l) = rx.recv().await {
            match l {
                LineIn::Line(v) => got.push(v["n"].as_u64()),
                LineIn::Closed => got.push(None),
            }
        }
        assert_eq!(got, vec![Some(1), Some(2), None]);
    }

    /// The compaction every backend reports: gauge at the old size, the item, gauge at the new
    /// size — the order `Agent` needs to fill in from → to.
    #[test]
    fn a_compaction_is_the_gauge_the_item_and_the_gauge_again() {
        let totals = Totals { tokens: 900, cached: 100, window: Some(200_000) };
        let events = compaction(3, "c-1", 90_000, 3_000, totals);
        let shapes: Vec<(String, Value)> = events.iter().map(|e| match e {
            HubEvent::Notif { agent: 3, method, params } => (method.clone(), params.clone()),
            other => panic!("{other:?}"),
        }).collect();
        assert_eq!(shapes[0].0, "thread/tokenUsage/updated");
        assert_eq!(shapes[0].1["tokenUsage"]["last"]["inputTokens"], 90_000);
        assert_eq!(shapes[0].1["tokenUsage"]["total"], json!({"totalTokens": 900, "cachedInputTokens": 100}));
        assert_eq!(shapes[0].1["tokenUsage"]["modelContextWindow"], 200_000);
        assert_eq!((shapes[1].0.as_str(), shapes[2].0.as_str()), ("item/started", "item/completed"));
        assert_eq!(shapes[1].1, json!({"item": {"id": "c-1", "type": "contextCompaction"}}));
        assert_eq!(shapes[3].1["tokenUsage"]["last"]["inputTokens"], 3_000);
        let no_window = token_usage(3, 1, 2, Totals { window: None, ..totals });
        assert!(matches!(no_window, HubEvent::Notif { params, .. } if params["tokenUsage"].get("modelContextWindow").is_none()), "no window is no window, never 0");
    }

    fn mcp_server(command: &str, args: &[&str], env: &[(&str, &str)]) -> McpServer {
        McpServer { command: command.into(), args: args.iter().map(|a| a.to_string()).collect(), env: env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(), enabled: true }
    }

    /// Each server is four `-c` lines at most — command, args as a TOML array, `enabled=true` (a
    /// same-named server switched off in ~/.codex/config.toml must not stay off), env as an inline
    /// table only when it has one — with every value a TOML string Codex parses back as written,
    /// spaces, quotes, backslashes and newlines included.
    #[test]
    fn codex_gets_each_mcp_server_as_toml_overrides() {
        let servers = vec![
            ("blender".to_string(), mcp_server("uvx", &["blender-mcp"], &[])),
            ("files".to_string(), mcp_server("/opt/my tools/fs", &["--root", "/tmp/a b", "say \"hi\"", "C:\\dir", "two\nlines"], &[("TOKEN", "s\"3"), ("A_B", "x")])),
        ];
        let args = codex_mcp_args(&servers);
        assert_eq!(&args[..6], &["-c", "mcp_servers.blender.command=\"uvx\"", "-c", "mcp_servers.blender.args=[\"blender-mcp\"]", "-c", "mcp_servers.blender.enabled=true"]);
        assert_eq!(args.iter().filter(|a| *a == "-c").count(), 7, "no env line for a server without env: {args:?}");
        let values: HashMap<&str, &str> = args.iter().filter(|a| *a != "-c").filter_map(|a| a.split_once('=')).collect();
        // what Codex does with `-c key=value`: the value is parsed as TOML
        let parse = |key: &str| -> toml::Value {
            let text = format!("v = {}", values.get(key).unwrap_or_else(|| panic!("{key} missing: {args:?}")));
            toml::from_str::<toml::Table>(&text).unwrap_or_else(|e| panic!("{key}: {e}"))["v"].clone()
        };
        assert_eq!(parse("mcp_servers.files.command").as_str(), Some("/opt/my tools/fs"));
        let fs_args: Vec<String> = parse("mcp_servers.files.args").as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect();
        assert_eq!(fs_args, vec!["--root", "/tmp/a b", "say \"hi\"", "C:\\dir", "two\nlines"]);
        assert_eq!(parse("mcp_servers.files.enabled").as_bool(), Some(true));
        let env = parse("mcp_servers.files.env");
        assert_eq!((env["TOKEN"].as_str(), env["A_B"].as_str()), (Some("s\"3"), Some("x")));
        assert!(codex_mcp_args(&[]).is_empty(), "no servers, no arguments");
        assert_eq!(toml_quote("bell\u{7}"), "\"bell\\u0007\"");
    }

    #[test]
    fn mcp_tool_names_split_into_server_and_tool() {
        assert_eq!(mcp_tool_name("mcp__blender__get_scene_info"), Some(("blender", "get_scene_info")));
        assert_eq!(mcp_tool_name("mcp__demo__render__preview"), Some(("demo", "render__preview")), "the tool keeps the rest");
        assert_eq!(mcp_tool_name("mcp__my-files__read"), Some(("my-files", "read")));
        for not in ["mcp__blender", "mcp____x", "mcp__x__", "Read", "mantra_status", "mcp_blender__x"] {
            assert_eq!(mcp_tool_name(not), None, "{not}");
        }
    }

    #[test]
    fn an_answers_images_are_its_data_url_input_images() {
        assert_eq!(data_url_parts("data:image/png;base64,iVBORw0K"), Some(("image/png", "iVBORw0K")));
        for not in ["https://example.com/a.png", "data:image/png,raw", "data:text/plain;base64,aGk=", "data:image/png;base64,"] {
            assert_eq!(data_url_parts(not), None, "{not}");
        }
        let answer = json!({"contentItems": [
            {"type": "inputText", "text": "saved /tmp/s.png"},
            {"type": "inputImage", "imageUrl": "data:image/jpeg;base64,/9j/4AAQ"},
            {"type": "inputImage", "imageUrl": "https://example.com/elsewhere.png"},
        ], "success": true});
        assert_eq!(answer_images(&answer), vec![("image/jpeg".to_string(), "/9j/4AAQ".to_string())]);
        assert!(answer_images(&json!({"contentItems": [{"type": "inputText", "text": "ok"}], "success": true})).is_empty());
        assert!(answer_images(&json!({"action": "decline"})).is_empty());
    }
}
