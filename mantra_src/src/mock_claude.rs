//! `mantra mock-claude`: a fake `claude -p --input-format stream-json …` speaking the real WP10.1
//! wire protocol (`hub/claude.rs`'s `Translator`), so `--demo` and end-to-end tests can drive the
//! Claude Code backend with zero API calls — the same job `mock.rs` does for `codex app-server`.
//!
//! Unlike `mock.rs` (whose dynamic-tool calls travel over the same JSON-RPC connection it already
//! shares with Mantra), a real `claude` calls `mantra_*` tools through a *separate* MCP stdio server
//! (`mantra mcp-bridge`, WP10.4), so to exercise that path faithfully this mock does too: when
//! `--mcp-config` names one, it spawns it exactly as `claude` would and talks real MCP to it
//! (`initialize` → `tools/list` → `tools/call`), so a demo run exercises the *entire* WP10.4 chain
//! (mock-claude → mcp-bridge → Hub's Unix socket → `App`/`Run::on_tool_call` → back) rather than a
//! shortcut around it.
//!
//! Role behaviour mirrors `mock.rs`'s scripts (`planner`/`orchestrator`/`worker`/`gate`), scoped
//! down to the paths WP10.6's demo pattern (`mantra-default-claude`) actually exercises: submitting
//! the plan, spawning a phase, one file edit per worker, and a passing gate report. It shares the
//! plan JSON and task-info/name-casing helpers with `mock.rs` directly (`pub(crate)`) rather than
//! duplicating them.

use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};

/// A minimal MCP client for the one `mantra` server named in `--mcp-config`: spawns
/// `mantra mcp-bridge`, does the `initialize`/`notifications/initialized` handshake, and answers
/// `tools/call` one request at a time (mirrors how `claude` itself drives it — one call in flight).
struct Bridge {
    child: Child,
    stdin: ChildStdin,
    lines: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
    next_id: i64,
}

impl Bridge {
    async fn spawn(mcp_config_json: &str) -> Option<Bridge> {
        let cfg: Value = serde_json::from_str(mcp_config_json).ok()?;
        let server = cfg.pointer("/mcpServers/mantra")?;
        let command = server.get("command").and_then(|c| c.as_str())?.to_string();
        let args: Vec<String> = server.get("args").and_then(|a| a.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_default();
        let mut child = tokio::process::Command::new(&command).args(&args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).kill_on_drop(true).spawn().ok()?;
        let stdin = child.stdin.take()?;
        let stdout = child.stdout.take()?;
        let mut b = Bridge { child, stdin, lines: BufReader::new(stdout).lines(), next_id: 1 };
        b.request(json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "mantra-mock-claude", "version": env!("CARGO_PKG_VERSION")}}})).await?;
        b.notify(json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).await;
        b.next_id += 1;
        let list_id = b.next_id;
        b.request(json!({"jsonrpc": "2.0", "id": list_id, "method": "tools/list", "params": {}})).await;
        Some(b)
    }
    async fn notify(&mut self, v: Value) {
        let _ = self.stdin.write_all(v.to_string().as_bytes()).await;
        let _ = self.stdin.write_all(b"\n").await;
        let _ = self.stdin.flush().await;
    }
    async fn request(&mut self, v: Value) -> Option<Value> {
        self.notify(v).await;
        match self.lines.next_line().await {
            Ok(Some(l)) => serde_json::from_str(&l).ok(),
            _ => None,
        }
    }
    async fn call_tool(&mut self, name: &str, args: Value) -> (bool, String) {
        self.next_id += 1;
        let id = self.next_id;
        let req = json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": name, "arguments": args}});
        match self.request(req).await {
            Some(resp) => {
                if let Some(err) = resp.get("error") {
                    return (false, err.get("message").and_then(|m| m.as_str()).unwrap_or("mcp error").to_string());
                }
                let result = resp.get("result").cloned().unwrap_or_else(|| json!({}));
                let is_error = result.get("isError").and_then(|b| b.as_bool()).unwrap_or(false);
                let text = result.get("content").and_then(|c| c.as_array()).and_then(|a| a.first()).and_then(|f| f.get("text")).and_then(|t| t.as_str()).unwrap_or("").to_string();
                (!is_error, text)
            }
            None => (false, "mantra mock-claude: mcp-bridge request failed".into()),
        }
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

struct Mock {
    out: tokio::io::Stdout,
    bridge: Option<Bridge>,
    cwd: PathBuf,
    model: String,
    /// The `--append-system-prompt` text: role instructions + protocol, same content Codex's
    /// `developerInstructions` gets — `task_info` (shared with `mock.rs`) parses it for workers.
    dev: String,
    speed: f64,
    msg_n: u64,
    tokens: u64,
}

impl Mock {
    async fn emit(&mut self, v: Value) {
        let _ = self.out.write_all(v.to_string().as_bytes()).await;
        let _ = self.out.write_all(b"\n").await;
        let _ = self.out.flush().await;
    }
    async fn sleep(&self, ms: u64) {
        tokio::time::sleep(std::time::Duration::from_millis((ms as f64 / self.speed) as u64)).await;
    }
    fn uid(&mut self, p: &str) -> String {
        self.msg_n += 1;
        format!("{p}-{:06x}", self.msg_n)
    }
    async fn say(&mut self, text: &str) {
        let mid = self.uid("msg");
        self.emit(json!({"type": "assistant", "message": {"id": mid, "content": [{"type": "text", "text": text}]}})).await;
        self.tokens += text.len() as u64;
        self.sleep(80).await;
    }
    /// Emits the `tool_use`/`tool_result` pair for one `mcp__mantra__<name>` call, actually round
    /// tripping through the spawned `mantra mcp-bridge` (or, with no bridge configured, failing the
    /// call the same way a real `claude` process would if the MCP server never connected).
    async fn tool_use(&mut self, name: &str, args: Value) -> (bool, String) {
        let mid = self.uid("msg");
        let tool_id = self.uid("tu");
        let full_name = format!("mcp__mantra__{name}");
        self.emit(json!({"type": "assistant", "message": {"id": mid, "content": [{"type": "tool_use", "id": tool_id, "name": full_name, "input": args.clone()}]}})).await;
        self.sleep(60).await;
        let (ok, text) = match self.bridge.as_mut() {
            Some(b) => b.call_tool(name, args).await,
            None => (false, "mantra mock-claude: no --mcp-config given".into()),
        };
        self.emit(json!({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": tool_id, "is_error": !ok, "content": text}]}})).await;
        self.sleep(40).await;
        self.tokens += 500 + text.len() as u64;
        (ok, text)
    }
    async fn write_file(&mut self, rel: &str, content: &str) {
        let path = self.cwd.join(rel);
        if let Some(d) = path.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let _ = std::fs::write(&path, content);
        let mid = self.uid("msg");
        let tool_id = self.uid("tu");
        self.emit(json!({"type": "assistant", "message": {"id": mid, "content": [{"type": "tool_use", "id": tool_id, "name": "Write", "input": {"file_path": path.to_string_lossy(), "content": content}}]}})).await;
        self.sleep(120).await;
        self.emit(json!({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": tool_id, "is_error": false, "content": "File written"}]}})).await;
        self.sleep(30).await;
        self.tokens += 900 + content.len() as u64;
    }
    async fn bash(&mut self, cmd: &str) {
        let mid = self.uid("msg");
        let tool_id = self.uid("tu");
        let out = std::process::Command::new("sh").arg("-c").arg(cmd).current_dir(&self.cwd).output().map(|o| String::from_utf8_lossy(&o.stdout).to_string()).unwrap_or_default();
        self.emit(json!({"type": "assistant", "message": {"id": mid, "content": [{"type": "tool_use", "id": tool_id, "name": "Bash", "input": {"command": cmd}}]}})).await;
        self.sleep(80).await;
        self.emit(json!({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": tool_id, "is_error": false}], "tool_use_result": {"stdout": out, "stderr": ""}}})).await;
        self.sleep(30).await;
        self.tokens += 600;
    }
    async fn result(&mut self, num_turns: i64) {
        let mut model_usage = serde_json::Map::new();
        model_usage.insert(self.model.clone(), json!({"contextWindow": 200_000}));
        self.emit(json!({
            "type": "result", "is_error": false, "num_turns": num_turns,
            "usage": {"input_tokens": self.tokens, "output_tokens": self.tokens / 4, "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0},
            "modelUsage": Value::Object(model_usage),
            "result": "",
        }))
        .await;
    }
    async fn compact(&mut self) {
        self.emit(json!({"type": "system", "subtype": "status", "status": "compacting"})).await;
        self.sleep(200).await;
        self.emit(json!({"type": "system", "subtype": "status", "compact_result": "success"})).await;
        let pre = self.tokens.max(1000);
        self.tokens = 500;
        self.emit(json!({"type": "system", "subtype": "compact_boundary", "compact_metadata": {"trigger": "manual", "pre_tokens": pre, "post_tokens": self.tokens}})).await;
        self.emit(json!({"type": "result", "is_error": false, "num_turns": 0})).await;
    }
}

async fn planner_turn(m: &mut Mock, text: &str) {
    if text.contains("[mantra:plan]") {
        let (_, r) = m.tool_use("mantra_submit_plan", crate::mock::mock_plan(true)).await;
        if r.starts_with("REJECTED") {
            // mock_plan(true) deliberately gives two Foundations tasks the same scope, exercising
            // the same reject → fix → resubmit path `mock.rs`'s planner takes.
            let _ = m.tool_use("mantra_submit_plan", crate::mock::mock_plan(false)).await;
        }
        m.say("Plan submitted: 3 phases, 7 tasks — foundations, then parallel features, then integration.").await;
    } else if text.contains("[mantra:reprompt]") {
        let _ = m.tool_use("mantra_status", json!({})).await;
        m.say("Reviewed status — no plan change needed.").await;
    } else if text.contains("[mantra:finale]") {
        let _ = m.tool_use("mantra_wait", json!({})).await;
    } else if text.contains("[mantra:event]") {
        let _ = m.tool_use("mantra_gate_report", json!({"pass": true, "summary": "verified against the plan"})).await;
        m.say("Verification complete.").await;
    } else {
        m.say("Noted.").await;
    }
}

async fn orchestrator_turn(m: &mut Mock, text: &str) {
    if text.contains("[mantra:phase]") {
        let json_block = text.split("```json").nth(1).and_then(|s| s.split("```").next()).unwrap_or("{}");
        let phase: Value = serde_json::from_str(json_block).unwrap_or_else(|_| json!({}));
        let ids: Vec<String> = phase.get("tasks").and_then(|t| t.as_array()).map(|a| a.iter().filter_map(|t| t.get("id").and_then(|i| i.as_str()).map(str::to_string)).collect()).unwrap_or_default();
        for id in &ids {
            let _ = m.tool_use("mantra_spawn", json!({"task_id": id})).await;
        }
        let _ = m.tool_use("mantra_wait", json!({})).await;
    } else if text.contains("[mantra:handoff]") {
        m.say("Handoff: phase complete, interfaces in place, no open risks.").await;
    } else {
        let _ = m.tool_use("mantra_wait", json!({})).await;
    }
}

async fn worker_turn(m: &mut Mock) {
    let (id, title, scope) = crate::mock::task_info(&m.dev);
    let dir = scope.first().map(|s| s.trim_end_matches("/**").trim_end_matches("/*").to_string()).unwrap_or_else(|| "src".into());
    m.bash(&format!("rg -n \"struct|fn\" {dir} || true")).await;
    let file = format!("{dir}/{}.rs", id.replace('-', "_"));
    let camel = crate::mock::camel(&id);
    let content = format!("//! {title}\n\npub struct {camel};\n\nimpl {camel} {{\n    pub fn new() -> Self {{ {camel} }}\n}}\n");
    m.write_file(&file, &content).await;
    m.say(&format!("Implemented **{title}** in `{file}`.\n\nSTATUS: done\nSUMMARY: added {camel} per the task.")).await;
}

async fn gate_turn(m: &mut Mock) {
    let _ = m.tool_use("mantra_gate_report", json!({"pass": true, "summary": "coherent, checks green"})).await;
    m.say("GATE: pass — everything is coherent and the checks are green.").await;
}

async fn manager_turn(m: &mut Mock, text: &str) {
    if text.contains("[mantra:escalation]") {
        let _ = m.tool_use("mantra_resume_run", json!({"note": "Try once more; run the failing check yourself before reporting."})).await;
        m.say("Resumed the run with a hint for the stuck agent.").await;
        return;
    }
    let _ = m.tool_use("mantra_wait", json!({})).await;
    m.say("Nothing to change — waiting for the next event.").await;
}

pub async fn run() {
    let args: Vec<String> = std::env::args().skip(2).collect();
    let get = |flag: &str| args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned();
    let has = |flag: &str| args.iter().any(|a| a == flag);
    let session_id = get("--session-id").or_else(|| get("--resume")).unwrap_or_else(crate::util::uuid_v4);
    let is_resume = has("--resume");
    let model = get("--model").unwrap_or_else(|| "claude-sonnet-5".into());
    let dev = get("--append-system-prompt").unwrap_or_default();
    let role = dev.lines().find_map(|l| l.trim().strip_prefix("mantra-role:")).map(|s| s.trim().to_string()).unwrap_or_else(|| "solo".into());
    let speed = std::env::var("MANTRA_MOCK_SPEED").ok().and_then(|s| s.parse::<f64>().ok()).unwrap_or(1.0).max(0.05);
    let cwd = std::env::current_dir().unwrap_or_default();
    let bridge = match get("--mcp-config") {
        Some(cfg) => Bridge::spawn(&cfg).await,
        None => None,
    };
    let mut m = Mock { out: tokio::io::stdout(), bridge, cwd, model: model.clone(), dev, speed, msg_n: 0, tokens: 0 };
    let mcp_servers = if m.bridge.is_some() { json!([{"name": "mantra", "status": "connected"}]) } else { json!([]) };
    m.emit(json!({"type": "system", "subtype": "init", "session_id": session_id, "model": model, "tools": [], "mcp_servers": mcp_servers, "permissionMode": "bypassPermissions", "apiKeySource": "none", "capabilities": {}})).await;

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut turn_n: i64 = if is_resume { 1 } else { 0 };
    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else { continue };
        match v.get("type").and_then(|t| t.as_str()) {
            Some("control_request") => {
                // A real turn interruption needs concurrent stdin reading this single-threaded
                // sequential mock doesn't do (turns run to completion before the loop reads again);
                // acking late is still safe — `Translator` treats a late/unrequested ack as a no-op.
                if let Some(rid) = v.get("request_id").and_then(|r| r.as_str()) {
                    m.emit(json!({"type": "control_response", "response": {"subtype": "success", "request_id": rid}})).await;
                }
            }
            Some("user") => {
                let text = v.pointer("/message/content").and_then(|c| c.as_str()).unwrap_or("").to_string();
                if text.trim() == "/compact" {
                    m.compact().await;
                    continue;
                }
                turn_n += 1;
                match role.as_str() {
                    "planner" => planner_turn(&mut m, &text).await,
                    "orchestrator" => orchestrator_turn(&mut m, &text).await,
                    "worker" => worker_turn(&mut m).await,
                    "gate" => gate_turn(&mut m).await,
                    "manager" => manager_turn(&mut m, &text).await,
                    _ => m.say(&format!("(mock claude) received: {}", crate::util::trunc(&text, 80))).await,
                }
                m.result(turn_n).await;
            }
            _ => {}
        }
    }
}
