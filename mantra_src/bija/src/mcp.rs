//! MCP servers (SPEC §10): the stdio client that lets the model use a configured server's tools —
//! Blender's MCP add-on, a filesystem server, anything that speaks the Model Context Protocol.
//!
//! Each server is a child process speaking JSON-RPC 2.0 over NDJSON on its stdin/stdout. A
//! connection (`Conn`) is two small tasks: a writer that owns stdin, and a reader that owns stdout,
//! answers the server's own requests (`ping`) and hands every response to whoever is waiting for
//! that id. So a call is cancel-safe the way the rest of the runtime is: dropping its future (an
//! interrupt, a timeout) forgets the id — the late answer is dropped when it comes — and tells the
//! server with `notifications/cancelled`. Both tasks only ever wait on their pipe, so an idle
//! server costs bija nothing (SPEC §8).
//!
//! Servers start with the session, all at once, each with `START_BUDGET` for `initialize` and the
//! whole `tools/list`; one that cannot start, crashes or never answers is a warning and the others
//! go on — a broken server must never cost the session. Their tools are offered as
//! `mcp__<server>__<tool>`, so a name says where it comes from and no server can shadow a
//! built-in. They run where the agent runs (its cwd), outside the sandbox (they are the user's own
//! programs, often talking to an app over a socket), without bija's API key, each in a process
//! group of its own, which shutdown ends with the background jobs.

use crate::types::{clip, human_size, image_from_base64, prefix, str_at, ToolDef, ToolOutcome};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot, watch};

/// The protocol revision bija asks for; it speaks whatever the server answers with (the subset it
/// uses — initialize, tools/list, tools/call, ping — is the same in every revision).
pub const PROTOCOL_VERSION: &str = "2025-06-18";
/// From launch to a listed toolbox, per server.
pub const START_BUDGET: Duration = Duration::from_secs(20);
/// One tool call. Long (a render, a build), because an interrupt can always end it sooner.
const CALL_TIMEOUT: Duration = Duration::from_secs(600);
/// A message is one line; an image result makes it a long one. Past this it is dropped.
const MAX_LINE: usize = 48 * 1024 * 1024;
/// `tools/list` pages followed at most (a server whose cursor never ends).
const MAX_PAGES: usize = 100;
/// The text of one result the model gets at most (its head; the rest is counted).
const TEXT_CAP: usize = 64 * 1024;
/// Both wires refuse longer tool names (and other characters than these).
const NAME_MAX: usize = 64;
/// Between SIGTERM and SIGKILL at shutdown, paid once for all servers (alongside the jobs').
const SHUTDOWN_GRACE: Duration = Duration::from_secs(1);
/// stderr lines are forwarded cut to this.
const STDERR_LINE: usize = 1000;

/// One server of `--mcp-file`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ServerConfig {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    /// Added to the server's environment (after bija's key variables are removed from it).
    pub env: BTreeMap<String, String>,
}

/// Parses an `--mcp-file`: `{"mcpServers": {"<name>": {"command", "args", "env"}}}`, the shape MCP
/// clients share. A server that cannot run here (no command, a URL-only transport) is a warning,
/// not an error: the rest of the file still counts. Servers come back in name order.
pub fn parse_file(text: &str) -> Result<(Vec<ServerConfig>, Vec<String>), String> {
    let v: Value = serde_json::from_str(text).map_err(|e| format!("not valid JSON: {e}"))?;
    let Some(servers) = v.get("mcpServers").and_then(Value::as_object) else {
        return Err("expected {\"mcpServers\": {\"<name>\": {\"command\": …}}}".into());
    };
    let mut warnings = Vec::new();
    let mut out = Vec::new();
    let mut names: Vec<&String> = servers.keys().collect();
    names.sort();
    for name in names {
        let entry = &servers[name.as_str()];
        let skip = |why: &str| format!("mcp {name}: {why} — skipped");
        if name.trim().is_empty() {
            warnings.push("mcp: a server without a name — skipped".into());
            continue;
        }
        if !entry.is_object() {
            warnings.push(skip("not an object"));
            continue;
        }
        let kind = str_at(entry, "type");
        if !kind.is_empty() && kind != "stdio" {
            warnings.push(skip(&format!("only stdio servers run here, not {kind}")));
            continue;
        }
        let command = str_at(entry, "command").trim();
        if command.is_empty() {
            warnings.push(skip("no command"));
            continue;
        }
        let args = match entry.get("args") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(items)) => items.iter().map(scalar_text).collect(),
            Some(_) => {
                warnings.push(skip("`args` must be a list"));
                continue;
            }
        };
        let env = match entry.get("env") {
            None | Some(Value::Null) => BTreeMap::new(),
            Some(Value::Object(m)) => m.iter().map(|(k, v)| (k.clone(), scalar_text(v))).collect(),
            Some(_) => {
                warnings.push(skip("`env` must be an object"));
                continue;
            }
        };
        out.push(ServerConfig { name: name.clone(), command: command.to_string(), args, env });
    }
    Ok((out, warnings))
}

/// A JSON value as an argument or variable: strings as they are, numbers and booleans written out.
fn scalar_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// The line the system prompt gets when servers are configured.
pub fn system_note(servers: &[ServerConfig]) -> Option<String> {
    if servers.is_empty() {
        return None;
    }
    let names: Vec<&str> = servers.iter().map(|s| s.name.as_str()).collect();
    Some(format!("Tools named mcp__<server>__<tool> come from the MCP servers {} (they run on this machine).", names.join(", ")))
}

/// `mcp__<server>__<tool>` in the characters and length both wires accept.
pub fn tool_name(server: &str, tool: &str) -> String {
    let clean = |s: &str| s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect::<String>();
    let full = format!("mcp__{}__{}", clean(server), clean(tool));
    prefix(&full, NAME_MAX).to_string()
}

// ---------------------------------------------------------------------------------------------
// JSON-RPC over a pair of pipes

/// Why a request got no result.
#[derive(Debug, Clone, PartialEq)]
enum RpcError {
    /// The server's stdout closed: it exited, or crashed.
    Closed,
    /// A JSON-RPC error response.
    Error { code: i64, message: String },
}

type Waiters = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, RpcError>>>>>;

/// What the writer task is told to do.
enum Line {
    Send(String),
    /// Close the server's stdin: the polite "please exit".
    Close,
}

/// One JSON-RPC connection. Cheap to share by reference; every method takes `&self`.
struct Conn {
    to_server: mpsc::UnboundedSender<Line>,
    waiters: Waiters,
    next_id: AtomicU64,
    /// Flips to true when the server's stdout ends.
    closed: watch::Receiver<bool>,
}

impl Conn {
    /// Starts the reader and writer tasks on the server's stdout and stdin.
    fn spawn(name: &str, reader: impl AsyncRead + Send + Unpin + 'static, mut writer: impl AsyncWrite + Send + Unpin + 'static) -> Conn {
        let (to_server, mut lines) = mpsc::unbounded_channel::<Line>();
        tokio::spawn(async move {
            while let Some(Line::Send(mut line)) = lines.recv().await {
                line.push('\n');
                if writer.write_all(line.as_bytes()).await.is_err() || writer.flush().await.is_err() {
                    break;
                }
            }
            // `Line::Close`, or every sender gone: dropping the writer closes the server's stdin.
        });
        let waiters: Waiters = Arc::default();
        let (closed_tx, closed) = watch::channel(false);
        tokio::spawn(read_loop(name.to_string(), BufReader::new(reader), waiters.clone(), to_server.clone(), closed_tx));
        Conn { to_server, waiters, next_id: AtomicU64::new(1), closed }
    }

    fn is_closed(&self) -> bool {
        *self.closed.borrow()
    }

    fn send(&self, msg: &Value) -> bool {
        self.to_server.send(Line::Send(msg.to_string())).is_ok()
    }

    fn notify(&self, method: &str, params: Value) {
        self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}));
    }

    /// One request and its answer. Cancel-safe: dropped, it forgets the id (a late answer is
    /// dropped by the reader) and, when `cancellable`, tells the server it may stop.
    async fn request(&self, method: &str, params: Value, cancellable: bool) -> Result<Value, RpcError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        if let Ok(mut w) = self.waiters.lock() {
            w.insert(id, tx);
        }
        let mut pending = Pending { conn: self, id, cancellable };
        if self.is_closed() || !self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})) {
            return Err(RpcError::Closed);
        }
        let answer = rx.await.unwrap_or(Err(RpcError::Closed));
        pending.cancellable = false;
        answer
    }

    fn close(&self) {
        let _ = self.to_server.send(Line::Close);
    }
}

/// A request in flight; dropping it before its answer (an interrupt, a timeout) cleans up.
struct Pending<'a> {
    conn: &'a Conn,
    id: u64,
    cancellable: bool,
}

impl Drop for Pending<'_> {
    fn drop(&mut self) {
        if let Ok(mut w) = self.conn.waiters.lock() {
            w.remove(&self.id);
        }
        if self.cancellable && !self.conn.is_closed() {
            self.conn.notify("notifications/cancelled", json!({"requestId": self.id, "reason": "the call was interrupted"}));
        }
    }
}

/// Reads the server's stdout to its end: responses to their waiters, the server's own requests
/// answered (only `ping` exists for a client that declares no capabilities), its log messages to
/// stderr, and lines that are not JSON (a banner some servers print) skipped.
async fn read_loop<R: AsyncBufRead + Unpin>(name: String, mut reader: R, waiters: Waiters, to_server: mpsc::UnboundedSender<Line>, closed: watch::Sender<bool>) {
    let mut buf = Vec::new();
    loop {
        match read_line_capped(&mut reader, &mut buf, MAX_LINE).await {
            Ok(Some(true)) => {}
            Ok(Some(false)) => {
                eprintln!("mcp {name}: a message over {} was dropped", human_size(MAX_LINE as u64));
                continue;
            }
            Ok(None) | Err(_) => break,
        }
        let text = String::from_utf8_lossy(&buf);
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(text) else {
            eprintln!("mcp {name}: not JSON-RPC, ignored: {}", clip(text, 200));
            continue;
        };
        let id = msg.get("id").filter(|id| !id.is_null());
        match (msg.get("method").and_then(Value::as_str), id) {
            (Some("ping"), Some(id)) => {
                let _ = to_server.send(Line::Send(json!({"jsonrpc": "2.0", "id": id, "result": {}}).to_string()));
            }
            (Some(method), Some(id)) => {
                let error = json!({"code": -32601, "message": format!("{method} is not supported by this client")});
                let _ = to_server.send(Line::Send(json!({"jsonrpc": "2.0", "id": id, "error": error}).to_string()));
            }
            (Some("notifications/message"), None) => {
                let data = msg.pointer("/params/data").map(scalar_text).unwrap_or_default();
                eprintln!("mcp {name}: {}", clip(&data, STDERR_LINE));
            }
            (Some(_), None) => {}
            (None, Some(id)) => {
                // Our ids are numbers; a server that echoes them as strings is understood too.
                let key = id.as_u64().or_else(|| id.as_str().and_then(|s| s.parse().ok()));
                let waiter = key.and_then(|k| waiters.lock().ok().and_then(|mut w| w.remove(&k)));
                if let Some(tx) = waiter {
                    let answer = match msg.get("error").filter(|e| !e.is_null()) {
                        Some(e) => Err(RpcError::Error { code: e.get("code").and_then(Value::as_i64).unwrap_or(0), message: str_at(e, "message").to_string() }),
                        None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                    };
                    let _ = tx.send(answer);
                }
            }
            (None, None) => {}
        }
    }
    let _ = closed.send(true);
    // Whoever still waits learns the server is gone now, not at their timeout.
    if let Ok(mut w) = waiters.lock() {
        for (_, tx) in w.drain() {
            let _ = tx.send(Err(RpcError::Closed));
        }
    }
}

/// One line into `buf` (without its newline), at most `max` bytes. `Some(false)` for a longer
/// line, which is consumed and dropped; `None` at the end of the stream.
async fn read_line_capped<R: AsyncBufRead + Unpin>(r: &mut R, buf: &mut Vec<u8>, max: usize) -> io::Result<Option<bool>> {
    buf.clear();
    let mut too_long = false;
    loop {
        let available = r.fill_buf().await?;
        if available.is_empty() {
            return Ok(if buf.is_empty() && !too_long { None } else { Some(!too_long) });
        }
        let (take, end) = match available.iter().position(|&b| b == b'\n') {
            Some(i) => (i, Some(i + 1)),
            None => (available.len(), None),
        };
        if !too_long {
            if buf.len() + take <= max {
                buf.extend_from_slice(&available[..take]);
            } else {
                too_long = true;
                buf.clear();
            }
        }
        r.consume(end.unwrap_or(take));
        if end.is_some() {
            return Ok(Some(!too_long));
        }
    }
}

/// The handshake: `initialize`, `notifications/initialized`, then every page of `tools/list`.
/// Returns the protocol version the server speaks and its tool objects.
async fn handshake(conn: &Conn) -> Result<(String, Vec<Value>), String> {
    let params = json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {},
        "clientInfo": {"name": "bija", "version": env!("CARGO_PKG_VERSION")},
    });
    let init = conn.request("initialize", params, false).await.map_err(|e| rpc_problem("initialize", e))?;
    let version = match str_at(&init, "protocolVersion") {
        "" => PROTOCOL_VERSION.to_string(),
        v => v.to_string(),
    };
    conn.notify("notifications/initialized", json!({}));
    let mut tools = Vec::new();
    let mut cursor: Option<String> = None;
    let mut seen = HashSet::new();
    for _ in 0..MAX_PAGES {
        let params = match &cursor {
            Some(c) => json!({"cursor": c}),
            None => json!({}),
        };
        let page = match conn.request("tools/list", params, false).await {
            Ok(page) => page,
            // A server without tools may not know the method at all.
            Err(RpcError::Error { code: -32601, .. }) if cursor.is_none() => break,
            Err(e) => return Err(rpc_problem("tools/list", e)),
        };
        tools.extend(page.get("tools").and_then(Value::as_array).into_iter().flatten().cloned());
        match page.get("nextCursor").and_then(Value::as_str).filter(|c| !c.is_empty()) {
            Some(c) if seen.insert(c.to_string()) => cursor = Some(c.to_string()),
            _ => break,
        }
    }
    Ok((version, tools))
}

fn rpc_problem(what: &str, e: RpcError) -> String {
    match e {
        RpcError::Closed => format!("the server closed the connection during {what}"),
        RpcError::Error { code, message } => format!("{what} failed: {message} (error {code})"),
    }
}

// ---------------------------------------------------------------------------------------------
// Servers and their tools

struct Server {
    name: String,
    conn: Conn,
    /// The server's process group (`None` for an in-test connection).
    pgid: Option<i32>,
    /// Set by the task that waits on the process: `Some(status)` once it exited (`Some(None)`
    /// when the status could not be read).
    exited: watch::Receiver<Option<Option<ExitStatus>>>,
}

impl Server {
    fn running(&self) -> bool {
        self.exited.borrow().is_none() && !self.conn.is_closed()
    }
}

struct Tool {
    def: ToolDef,
    server: usize,
    /// The tool's own name on its server.
    tool: String,
}

/// The live servers of a session and the tools they offer.
#[derive(Default)]
pub struct Mcp {
    servers: Vec<Server>,
    tools: Vec<Tool>,
}

impl Mcp {
    /// Starts every server at once and lists their tools. Never fails: what could not start comes
    /// back as warnings, and the session goes on with the rest.
    pub async fn start(configs: Vec<ServerConfig>, cwd: &Path, scrub_env: &[String]) -> (Mcp, Vec<String>) {
        Mcp::start_within(configs, cwd, scrub_env, START_BUDGET).await
    }

    async fn start_within(configs: Vec<ServerConfig>, cwd: &Path, scrub_env: &[String], budget: Duration) -> (Mcp, Vec<String>) {
        let mut set = tokio::task::JoinSet::new();
        for (i, cfg) in configs.into_iter().enumerate() {
            let (cwd, scrub) = (cwd.to_path_buf(), scrub_env.to_vec());
            set.spawn(async move { (i, start_one(cfg, cwd, scrub, budget).await) });
        }
        let mut started = Vec::new();
        let mut warnings = Vec::new();
        while let Some(joined) = set.join_next().await {
            match joined {
                Ok((i, Ok(s))) => started.push((i, s)),
                Ok((_, Err(w))) => warnings.push(w),
                Err(e) => warnings.push(format!("mcp: a server's start-up failed: {e}")),
            }
        }
        started.sort_by_key(|(i, _)| *i);
        let mut mcp = Mcp::default();
        for (_, (server, tools)) in started {
            warnings.extend(mcp.add(server, tools));
        }
        (mcp, warnings)
    }

    /// Takes a started server and offers its tools; returns warnings for the tools it could not.
    fn add(&mut self, server: Server, tools: Vec<Value>) -> Vec<String> {
        let mut warnings = Vec::new();
        let idx = self.servers.len();
        let mut offered = 0;
        for t in tools {
            let tool = str_at(&t, "name").trim().to_string();
            if tool.is_empty() {
                continue;
            }
            let name = tool_name(&server.name, &tool);
            if self.tools.iter().any(|x| x.def.name == name) {
                warnings.push(format!("mcp {}: tool {tool} skipped: {name} is already taken", server.name));
                continue;
            }
            let description = match str_at(&t, "description").trim() {
                "" => format!("{tool} (from the MCP server {})", server.name),
                d => d.to_string(),
            };
            self.tools.push(Tool { def: ToolDef { name, description, parameters: object_schema(t.get("inputSchema")) }, server: idx, tool });
            offered += 1;
        }
        eprintln!("bija: mcp {}: {offered} tools", server.name);
        self.servers.push(server);
        warnings
    }

    /// The tools offered to the model, server by server.
    pub fn defs(&self) -> Vec<ToolDef> {
        self.tools.iter().map(|t| t.def.clone()).collect()
    }

    pub fn offers(&self, name: &str) -> bool {
        self.tools.iter().any(|t| t.def.name == name)
    }

    /// One `tools/call`. Cancel-safe (see `Conn::request`). Every failure is a result the model
    /// can read, never an aborted turn.
    pub async fn call(&self, name: &str, args: &Value) -> ToolOutcome {
        let Some(t) = self.tools.iter().find(|t| t.def.name == name) else {
            return ToolOutcome::err(format!("{name}: no such MCP tool"));
        };
        let server = &self.servers[t.server];
        if !server.running() {
            return ToolOutcome::err(format!("mcp {}: the server is not running any more (it exited); its tools are back when bija restarts", server.name));
        }
        let arguments = if args.is_object() { args.clone() } else { json!({}) };
        let params = json!({"name": t.tool, "arguments": arguments});
        match tokio::time::timeout(CALL_TIMEOUT, server.conn.request("tools/call", params, true)).await {
            Ok(Ok(result)) => outcome_of(&result),
            Ok(Err(RpcError::Closed)) => ToolOutcome::err(format!("mcp {}: the server exited during the call", server.name)),
            Ok(Err(RpcError::Error { code, message })) => ToolOutcome::err(format!("mcp {}: {message} (error {code})", server.name)),
            Err(_) => ToolOutcome::err(format!("mcp {}: no answer from {} within {} s; the call was cancelled", server.name, t.tool, CALL_TIMEOUT.as_secs())),
        }
    }

    /// Ends every server: stdin closed and SIGTERM to its group at once, a grace for them to go,
    /// then SIGKILL. Runs beside the jobs' shutdown, so the grace is paid once.
    pub async fn shutdown(&mut self) {
        for s in &self.servers {
            s.conn.close();
            if let Some(pgid) = s.pgid {
                signal_group(pgid, libc::SIGTERM);
            }
        }
        let until = tokio::time::Instant::now() + SHUTDOWN_GRACE;
        for s in &self.servers {
            let mut exited = s.exited.clone();
            let _ = tokio::time::timeout_at(until, exited.wait_for(Option::is_some)).await;
        }
        for s in &self.servers {
            if let Some(pgid) = s.pgid {
                // Whatever the server started is in its group, even after the server itself went.
                signal_group(pgid, libc::SIGKILL);
            }
        }
        self.servers.clear();
        self.tools.clear();
    }
}

/// Servers never outlive a runtime dropped without `shutdown` (a panic, a test).
impl Drop for Mcp {
    fn drop(&mut self) {
        for pgid in self.servers.iter().filter_map(|s| s.pgid) {
            signal_group(pgid, libc::SIGKILL);
        }
    }
}

/// A tool's `inputSchema` as both wires accept it: an object schema (`type: "object"` added when
/// a server leaves it out; one provider refuses the whole request over a single bad tool).
fn object_schema(schema: Option<&Value>) -> Value {
    match schema {
        Some(Value::Object(m)) if m.get("type").map_or(true, |t| t == "object") => {
            let mut m = m.clone();
            m.insert("type".into(), json!("object"));
            Value::Object(m)
        }
        _ => json!({"type": "object", "properties": {}}),
    }
}

/// SIGTERM/SIGKILL to a process group; refuses 0 and 1 ("ours", "everything").
fn signal_group(pgid: i32, sig: i32) -> bool {
    // SAFETY: killpg(2) has no memory effects; a gone group is just an error return.
    pgid > 1 && unsafe { libc::killpg(pgid, sig) } == 0
}

/// Launches one server and lists its tools within `budget`; `Err` is the warning that says why
/// it is skipped.
async fn start_one(cfg: ServerConfig, cwd: PathBuf, scrub_env: Vec<String>, budget: Duration) -> Result<(Server, Vec<Value>), String> {
    let name = cfg.name.clone();
    let mut cmd = Command::new(&cfg.command);
    cmd.args(&cfg.args).current_dir(&cwd).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).process_group(0);
    for var in &scrub_env {
        cmd.env_remove(var);
    }
    // The server's own settings win, even over a variable bija keeps from commands.
    cmd.envs(&cfg.env);
    let mut child = cmd.spawn().map_err(|e| format!("mcp {name}: cannot start `{}`: {e} — skipped", cfg.command))?;
    let pgid = child.id().and_then(|p| i32::try_from(p).ok()).filter(|p| *p > 1);
    let (Some(stdin), Some(stdout), Some(stderr)) = (child.stdin.take(), child.stdout.take(), child.stderr.take()) else {
        if let Some(pgid) = pgid {
            signal_group(pgid, libc::SIGKILL);
        }
        return Err(format!("mcp {name}: no pipes to the server — skipped"));
    };
    let last_err: Arc<Mutex<String>> = Arc::default();
    let mut stderr_done = tokio::spawn(forward_stderr(name.clone(), stderr, last_err.clone()));
    let (exited_tx, exited) = watch::channel(None);
    tokio::spawn(async move {
        let status = child.wait().await.ok();
        let _ = exited_tx.send(Some(status));
    });
    let conn = Conn::spawn(&name, stdout, stdin);
    let server = Server { name: name.clone(), conn, pgid, exited };
    let result = tokio::time::timeout(budget, handshake(&server.conn)).await;
    let problem = match result {
        Ok(Ok((version, tools))) => {
            if version != PROTOCOL_VERSION {
                eprintln!("bija: mcp {name}: speaks protocol {version}");
            }
            return Ok((server, tools));
        }
        Ok(Err(e)) => e,
        Err(_) => format!("no answer to initialize within {} s (a first run that downloads packages can take longer: run the command once by hand)", budget.as_secs()),
    };
    // A server that died says why on stderr: wait a moment for its exit and its last words.
    let mut exited = server.exited.clone();
    let status = match tokio::time::timeout(Duration::from_millis(300), exited.wait_for(Option::is_some)).await {
        Ok(Ok(st)) => *st,
        _ => None,
    };
    if let Some(pgid) = pgid {
        signal_group(pgid, libc::SIGKILL);
    }
    let _ = tokio::time::timeout(Duration::from_millis(300), &mut stderr_done).await;
    let said = last_err.lock().map(|s| s.clone()).unwrap_or_default();
    let detail = match (status, said.is_empty()) {
        (Some(st), false) => format!("it exited ({}) saying: {said}", exit_text(st)),
        (Some(st), true) => format!("it exited ({})", exit_text(st)),
        (None, _) => problem,
    };
    Err(format!("mcp {name}: {detail} — skipped"))
}

fn exit_text(st: Option<ExitStatus>) -> String {
    use std::os::unix::process::ExitStatusExt;
    match st {
        Some(st) => match (st.code(), st.signal()) {
            (Some(c), _) => format!("exit code {c}"),
            (None, Some(s)) => format!("signal {s}"),
            _ => "exit status unknown".into(),
        },
        None => "exit status unknown".into(),
    }
}

/// The server's stderr to bija's, one line at a time, prefixed with its name; the last non-blank
/// line is kept for a start-up failure's warning.
async fn forward_stderr(name: String, stderr: impl AsyncRead + Unpin, last: Arc<Mutex<String>>) {
    let mut reader = BufReader::new(stderr);
    let mut buf = Vec::new();
    while let Ok(Some(_)) = read_line_capped(&mut reader, &mut buf, 64 * 1024).await {
        let line = String::from_utf8_lossy(&buf);
        let line = line.trim_end();
        if line.trim().is_empty() {
            continue;
        }
        let line = clip(line, STDERR_LINE);
        eprintln!("mcp {name}: {line}");
        if let Ok(mut l) = last.lock() {
            *l = line;
        }
    }
}

/// A `tools/call` result as the model gets it: text parts joined with newlines, image parts as
/// images, `isError` as a failed call. Parts it cannot show are named, so nothing vanishes silently.
fn outcome_of(result: &Value) -> ToolOutcome {
    let mut texts: Vec<String> = Vec::new();
    let mut images = Vec::new();
    for part in result.get("content").and_then(Value::as_array).into_iter().flatten() {
        match str_at(part, "type") {
            "text" => texts.push(str_at(part, "text").to_string()),
            "image" => match image_from_base64(str_at(part, "data"), str_at(part, "mimeType")) {
                Ok(img) => images.push(img),
                Err(why) => texts.push(format!("[an image was left out: {why}]")),
            },
            "resource" => {
                let r = part.get("resource").unwrap_or(&Value::Null);
                let (uri, mime) = (str_at(r, "uri"), str_at(r, "mimeType"));
                if let Some(text) = r.get("text").and_then(Value::as_str) {
                    texts.push(text.to_string());
                } else if mime.starts_with("image/") {
                    match image_from_base64(str_at(r, "blob"), mime) {
                        Ok(img) => images.push(img),
                        Err(why) => texts.push(format!("[an image ({uri}) was left out: {why}]")),
                    }
                } else {
                    texts.push(format!("[resource {uri} ({mime}) not shown]"));
                }
            }
            "resource_link" => texts.push(format!("[resource link: {} {}]", str_at(part, "name"), str_at(part, "uri"))),
            other => texts.push(format!("[{other} content left out]")),
        }
    }
    let mut text = texts.join("\n");
    if text.trim().is_empty() && images.is_empty() {
        text = match result.get("structuredContent").filter(|s| !s.is_null()) {
            Some(s) => s.to_string(),
            None => "(no output)".to_string(),
        };
    }
    if text.len() > TEXT_CAP {
        let head = prefix(&text, TEXT_CAP).to_string();
        text = format!("{head}\n[… {} more bytes of the result cut]", text.len() - head.len());
    }
    let is_error = result.get("isError").and_then(Value::as_bool).unwrap_or(false);
    ToolOutcome { ok: !is_error, text, images, ..Default::default() }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{base64_encode, ImageData};
    use tokio::io::{DuplexStream, ReadHalf, WriteHalf};

    /// An in-test server on the other end of a duplex pipe: `answer(request)` returns the
    /// response's `result` (or `Err` for an error object), or `None` to stay silent.
    struct Fake {
        rx: tokio::io::Lines<BufReader<ReadHalf<DuplexStream>>>,
        tx: WriteHalf<DuplexStream>,
    }

    impl Fake {
        async fn recv(&mut self) -> Option<Value> {
            let line = self.rx.next_line().await.ok()??;
            serde_json::from_str(&line).ok()
        }
        async fn send(&mut self, v: Value) {
            self.tx.write_all(format!("{v}\n").as_bytes()).await.unwrap();
        }
        /// Serves until the client goes, answering with `answer`; records every message.
        async fn serve(mut self, seen: Arc<Mutex<Vec<Value>>>, answer: impl Fn(&Value) -> Option<Result<Value, Value>>) {
            while let Some(msg) = self.recv().await {
                seen.lock().unwrap().push(msg.clone());
                let Some(id) = msg.get("id").cloned() else { continue };
                if msg.get("method").is_none() {
                    continue; // our answer to a request of the fake's
                }
                match answer(&msg) {
                    Some(Ok(result)) => self.send(json!({"jsonrpc": "2.0", "id": id, "result": result})).await,
                    Some(Err(error)) => self.send(json!({"jsonrpc": "2.0", "id": id, "error": error})).await,
                    None => {}
                }
            }
        }
    }

    fn pipe() -> (Conn, Fake) {
        let (ours, theirs) = tokio::io::duplex(1 << 20);
        let (r, w) = tokio::io::split(ours);
        let (tr, tw) = tokio::io::split(theirs);
        (Conn::spawn("fake", r, w), Fake { rx: BufReader::new(tr).lines(), tx: tw })
    }

    fn server(conn: Conn) -> Server {
        let (tx, exited) = watch::channel(None);
        // The sender is leaked: an in-test server has no process to exit.
        std::mem::forget(tx);
        Server { name: "fake".into(), conn, pgid: None, exited }
    }

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";

    /// What the fake answers: two pages of tools, then calls by name.
    fn toolbox(msg: &Value) -> Option<Result<Value, Value>> {
        match str_at(msg, "method") {
            "initialize" => Some(Ok(json!({"protocolVersion": "2024-11-05", "capabilities": {"tools": {}}, "serverInfo": {"name": "fake"}}))),
            "tools/list" if msg.pointer("/params/cursor").is_none() => Some(Ok(json!({"tools": [
                {"name": "scene_info", "description": "Describe the scene.", "inputSchema": {"type": "object", "properties": {}}},
                {"name": "render", "inputSchema": {"properties": {"width": {"type": "integer"}}}},
            ], "nextCursor": "page2"}))),
            "tools/list" => Some(Ok(json!({"tools": [{"name": "fail"}, {"name": "weird.name/x"}]}))),
            "tools/call" => Some(match msg.pointer("/params/name").and_then(Value::as_str) {
                Some("scene_info") => Ok(json!({"content": [{"type": "text", "text": "Cube"}, {"type": "text", "text": "Light"}]})),
                Some("render") => Ok(json!({"content": [{"type": "text", "text": format!("width {}", msg.pointer("/params/arguments/width").cloned().unwrap_or(Value::Null))}, {"type": "image", "data": base64_encode(PNG), "mimeType": "image/png"}, {"type": "image", "data": "%%%", "mimeType": "image/png"}]})),
                Some("fail") => Ok(json!({"content": [{"type": "text", "text": "Blender is not running"}], "isError": true})),
                _ => Err(json!({"code": -32602, "message": "unknown tool"})),
            }),
            _ => Some(Err(json!({"code": -32601, "message": "no such method"}))),
        }
    }

    #[test]
    fn mcp_files_parse_and_explain_what_they_skip() {
        let (servers, warnings) = parse_file(
            r#"{"mcpServers": {
                "blender": {"command": "uvx", "args": ["blender-mcp"], "env": {"BLENDER_PORT": 9876}},
                "a-web": {"type": "http", "url": "http://localhost:1/mcp"},
                "empty": {"args": []},
                "plain": {"type": "stdio", "command": "srv"}
            }}"#,
        )
        .unwrap();
        assert_eq!(servers.len(), 2);
        assert_eq!(servers[0], ServerConfig { name: "blender".into(), command: "uvx".into(), args: vec!["blender-mcp".into()], env: [("BLENDER_PORT".to_string(), "9876".to_string())].into() });
        assert_eq!((servers[1].name.as_str(), servers[1].args.len()), ("plain", 0));
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings[0].starts_with("mcp a-web: only stdio") && warnings[1].starts_with("mcp empty: no command"), "{warnings:?}");
        assert!(parse_file("[]").unwrap_err().contains("mcpServers"));
        assert!(parse_file("{oops").unwrap_err().contains("JSON"));
        assert_eq!(parse_file(r#"{"mcpServers": {}}"#).unwrap(), (vec![], vec![]));
        assert!(system_note(&servers).unwrap().contains("blender, plain"));
        assert_eq!(system_note(&[]), None);
    }

    #[test]
    fn tool_names_carry_the_server_and_fit_both_wires() {
        assert_eq!(tool_name("blender", "get_scene_info"), "mcp__blender__get_scene_info");
        assert_eq!(tool_name("my server", "a.b/c"), "mcp__my_server__a_b_c");
        assert_eq!(tool_name("s", &"x".repeat(100)).len(), NAME_MAX);
        assert_eq!(object_schema(None), json!({"type": "object", "properties": {}}));
        assert_eq!(object_schema(Some(&json!({"properties": {"a": {}}}))), json!({"type": "object", "properties": {"a": {}}}));
        assert_eq!(object_schema(Some(&json!({"type": "string"}))), json!({"type": "object", "properties": {}}));
    }

    /// The whole protocol against a fake: the handshake with a paginated tool list, calls with
    /// text, images and `isError`, a JSON-RPC error, and the server's own `ping` answered.
    #[tokio::test]
    async fn the_client_lists_and_calls_tools() {
        let (conn, fake) = pipe();
        let seen: Arc<Mutex<Vec<Value>>> = Arc::default();
        tokio::spawn(fake.serve(seen.clone(), toolbox));
        let (version, tools) = handshake(&conn).await.unwrap();
        assert_eq!((version.as_str(), tools.len()), ("2024-11-05", 4), "the server's version is the one spoken");
        let mut mcp = Mcp::default();
        let warnings = mcp.add(server(conn), tools);
        assert!(warnings.is_empty(), "{warnings:?}");
        let names: Vec<String> = mcp.defs().into_iter().map(|d| d.name).collect();
        assert_eq!(names, ["mcp__fake__scene_info", "mcp__fake__render", "mcp__fake__fail", "mcp__fake__weird_name_x"]);
        let render = &mcp.defs()[1];
        assert_eq!(render.parameters["type"], "object");
        assert_eq!(render.description, "render (from the MCP server fake)");

        let out = mcp.call("mcp__fake__scene_info", &json!({})).await;
        assert!(out.ok && out.text == "Cube\nLight" && out.images.is_empty(), "{out:?}");
        let out = mcp.call("mcp__fake__render", &json!({"width": 64})).await;
        assert!(out.ok, "{out:?}");
        assert_eq!(out.text, "width 64\n[an image was left out: its data is not base64]");
        assert_eq!(out.images, vec![ImageData { mime: "image/png".into(), bytes: PNG.to_vec() }]);
        let out = mcp.call("mcp__fake__fail", &json!({})).await;
        assert!(!out.ok && out.text == "Blender is not running", "isError is a failed call: {out:?}");
        let out = mcp.call("mcp__fake__weird_name_x", &json!({})).await;
        assert!(!out.ok && out.text == "mcp fake: unknown tool (error -32602)", "{out:?}");

        let log = seen.lock().unwrap().clone();
        let methods: Vec<&str> = log.iter().map(|m| str_at(m, "method")).collect();
        assert_eq!(&methods[..4], ["initialize", "notifications/initialized", "tools/list", "tools/list"]);
        assert_eq!(log[0]["params"]["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(log[3]["params"]["cursor"], "page2");
        let call = log.iter().find(|m| m.pointer("/params/name") == Some(&json!("render"))).unwrap();
        assert_eq!(call["params"]["arguments"], json!({"width": 64}));
        assert!(log.iter().all(|m| m["jsonrpc"] == "2.0"));
    }

    #[tokio::test]
    async fn the_servers_ping_is_answered_and_noise_is_skipped() {
        let (conn, mut fake) = pipe();
        fake.tx.write_all(b"\nrunning 1 test\n").await.unwrap();
        fake.send(json!({"jsonrpc": "2.0", "id": "srv-1", "method": "ping"})).await;
        fake.send(json!({"jsonrpc": "2.0", "id": 7, "method": "sampling/createMessage", "params": {}})).await;
        assert_eq!(fake.recv().await.unwrap(), json!({"jsonrpc": "2.0", "id": "srv-1", "result": {}}));
        assert_eq!(fake.recv().await.unwrap()["error"]["code"], -32601);
        // Requests still work after the noise; a string id in the answer is understood.
        let ask = tokio::spawn(async move { conn.request("tools/list", json!({}), false).await });
        let req = fake.recv().await.unwrap();
        let id = req["id"].as_u64().unwrap().to_string();
        fake.send(json!({"jsonrpc": "2.0", "id": id, "result": {"tools": []}})).await;
        assert_eq!(ask.await.unwrap(), Ok(json!({"tools": []})));
    }

    /// An interrupted call is forgotten and the server told; its late answer goes nowhere, and the
    /// next call still gets its own answer.
    #[tokio::test]
    async fn a_dropped_call_is_cancelled() {
        let (conn, mut fake) = pipe();
        let conn = Arc::new(conn);
        let c = conn.clone();
        let call = tokio::spawn(async move { c.request("tools/call", json!({"name": "slow"}), true).await });
        let req = fake.recv().await.unwrap();
        call.abort();
        let _ = call.await;
        let cancel = fake.recv().await.unwrap();
        assert_eq!((cancel["method"].as_str(), &cancel["params"]["requestId"]), (Some("notifications/cancelled"), &req["id"]));
        fake.send(json!({"jsonrpc": "2.0", "id": req["id"], "result": {"late": true}})).await;
        let c = conn.clone();
        let next = tokio::spawn(async move { c.request("tools/list", json!({}), false).await });
        let req2 = fake.recv().await.unwrap();
        fake.send(json!({"jsonrpc": "2.0", "id": req2["id"], "result": {"fresh": true}})).await;
        assert_eq!(next.await.unwrap(), Ok(json!({"fresh": true})));
        assert!(conn.waiters.lock().unwrap().is_empty());
    }

    /// The server going away answers everyone waiting at once, and later calls say why.
    #[tokio::test]
    async fn a_server_that_exits_fails_its_calls_cleanly() {
        let (conn, mut fake) = pipe();
        let mut mcp = Mcp::default();
        mcp.add(server(conn), vec![json!({"name": "t"})]);
        let pending = tokio::spawn(async move {
            let out = mcp.call("mcp__fake__t", &json!({})).await;
            (out, mcp.call("mcp__fake__t", &json!({})).await)
        });
        fake.recv().await.unwrap();
        drop(fake);
        let (first, second) = tokio::time::timeout(Duration::from_secs(5), pending).await.unwrap().unwrap();
        assert!(!first.ok && first.text.contains("exited during the call"), "{first:?}");
        assert!(!second.ok && second.text.contains("not running any more"), "{second:?}");
    }

    /// A process that never answers `initialize` costs its budget and a warning, nothing more;
    /// one whose command does not exist is skipped at once; the rest still start.
    #[tokio::test]
    async fn servers_that_cannot_start_are_skipped_with_a_warning() {
        let silent = ServerConfig { name: "silent".into(), command: "sleep".into(), args: vec!["30".into()], env: BTreeMap::new() };
        let missing = ServerConfig { name: "gone".into(), command: "/nonexistent/bija-mcp".into(), ..ServerConfig::default() };
        let dies = ServerConfig { name: "dies".into(), command: "sh".into(), args: vec!["-c".into(), "echo 'cannot find blender' >&2; exit 3".into()], env: BTreeMap::new() };
        let started = std::time::Instant::now();
        let (mcp, warnings) = Mcp::start_within(vec![silent, missing, dies], Path::new("/"), &[], Duration::from_millis(600)).await;
        assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
        assert!(mcp.servers.is_empty() && mcp.defs().is_empty());
        let all = warnings.join("\n");
        assert_eq!(warnings.len(), 3, "{all}");
        assert!(all.contains("mcp silent: no answer to initialize within"), "{all}");
        assert!(all.contains("mcp gone: cannot start `/nonexistent/bija-mcp`"), "{all}");
        assert!(all.contains("mcp dies: it exited (exit code 3) saying: cannot find blender — skipped"), "{all}");
    }

    #[tokio::test]
    async fn long_lines_are_dropped_whole() {
        let data = format!("{}\nshort\nlast", "x".repeat(100));
        let mut r = BufReader::with_capacity(16, data.as_bytes());
        let mut buf = Vec::new();
        assert_eq!(read_line_capped(&mut r, &mut buf, 50).await.unwrap(), Some(false));
        assert_eq!(read_line_capped(&mut r, &mut buf, 50).await.unwrap(), Some(true));
        assert_eq!(buf, b"short");
        assert_eq!(read_line_capped(&mut r, &mut buf, 50).await.unwrap(), Some(true));
        assert_eq!(buf, b"last", "an unterminated last line still counts");
        assert_eq!(read_line_capped(&mut r, &mut buf, 50).await.unwrap(), None);
    }

    #[test]
    fn results_name_what_they_cannot_show() {
        let out = outcome_of(&json!({"content": [
            {"type": "resource", "resource": {"uri": "file:///a.txt", "mimeType": "text/plain", "text": "hello"}},
            {"type": "resource", "resource": {"uri": "file:///b.bin", "mimeType": "application/zip", "blob": "AAAA"}},
            {"type": "resource_link", "uri": "file:///c", "name": "c"},
            {"type": "audio", "data": "AAAA", "mimeType": "audio/wav"},
        ]}));
        assert_eq!(out.text, "hello\n[resource file:///b.bin (application/zip) not shown]\n[resource link: c file:///c]\n[audio content left out]");
        assert_eq!(outcome_of(&json!({"content": [], "structuredContent": {"n": 1}})).text, "{\"n\":1}");
        assert_eq!(outcome_of(&json!({})).text, "(no output)");
        let big = outcome_of(&json!({"content": [{"type": "text", "text": "y".repeat(TEXT_CAP + 10)}]}));
        assert!(big.text.len() < TEXT_CAP + 100 && big.text.ends_with("[… 10 more bytes of the result cut]"), "{}", &big.text[TEXT_CAP..]);
        assert!(image_from_base64(&base64_encode(b"GIF89a"), "image/png").is_ok_and(|i| i.mime == "image/gif"), "the bytes decide the type");
        assert!(image_from_base64(&base64_encode(b"<svg/>"), "image/svg+xml").unwrap_err().contains("image/svg+xml is not"));
    }
}
