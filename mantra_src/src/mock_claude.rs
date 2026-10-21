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
//!
//! The agent's own servers (`SpawnSpec.mcp`: Blender's, say, or `mantra mock-mcp` in `--demo`)
//! ride in the same `--mcp-config` beside the bridge, so this mock starts every server listed
//! there the same way, reports each `connected` or `failed` in `init` like `claude` does, and a
//! worker given a server with a `scene_info` tool calls it once, as `mcp__<server>__scene_info` —
//! the name the translator turns back into `mcpToolCall{server, tool}`.

use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// What a server gets to answer `initialize` and every page of `tools/list` — the budget bija
/// and `mantra mcp test` give one too. A server that misses it is reported `failed` and the
/// others carry on. `tools/call` has no budget: the bridge's `mantra_wait` blocks until the run
/// has news, by design.
const START_BUDGET: Duration = Duration::from_secs(20);

/// One stdio MCP server from `--mcp-config`, driven the way `claude` drives one — through
/// Mantra's own client (`mcp.rs`): `initialize` → `notifications/initialized` → `tools/list`
/// (every page), then `tools/call` one request at a time (`claude` awaits each tool call before
/// issuing the next). `mantra` is the bridge (`mantra mcp-bridge`); any other name is one of the
/// agent's own servers.
struct Server {
    name: String,
    client: crate::mcp::Client,
    /// The names `tools/list` gave, unprefixed.
    tools: Vec<String>,
}

impl Server {
    async fn spawn(name: &str, entry: &Value) -> Result<Server, String> {
        let config: crate::config::McpServer = serde_json::from_value(entry.clone()).map_err(|e| format!("not a server entry: {e}"))?;
        let mut client = crate::mcp::Client::launch(&config)?;
        let start = async {
            client.initialize().await?;
            client.list_tools().await
        };
        let listed = match tokio::time::timeout(START_BUDGET, start).await {
            Ok(r) => r,
            Err(_) => Err(format!("no answer within {} s", START_BUDGET.as_secs())),
        };
        match listed {
            Ok(tools) => Ok(Server { name: name.to_string(), tools: tools.iter().filter_map(|t| t.get("name").and_then(Value::as_str).map(str::to_string)).collect(), client }),
            Err(e) => {
                let said = client.last_words().await;
                Err(if said.is_empty() { e } else { format!("{e}\n{said}") })
            }
        }
    }

    /// One `tools/call`: whether it succeeded, its text parts joined, and its images
    /// (`(mime, base64)`).
    async fn call_tool(&mut self, name: &str, args: Value) -> (bool, String, Vec<(String, String)>) {
        match self.client.call_tool(name, args).await {
            Ok(r) => (!r.is_error, r.text, r.images),
            Err(e) => (false, format!("mantra mock-claude: {e}"), vec![]),
        }
    }
}

/// Starts every server of an `--mcp-config` value at once, as `claude` does: the ones that came
/// up, and each name's status for the `init` event. One that fails is only that — a line on
/// stderr and `failed` — never the end of the others.
async fn spawn_servers(mcp_config_json: &str) -> (Vec<Server>, Vec<Value>) {
    let cfg: Value = serde_json::from_str(mcp_config_json).unwrap_or_else(|_| json!({}));
    let entries: Vec<(String, Value)> = cfg.get("mcpServers").and_then(Value::as_object).map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect()).unwrap_or_default();
    let started = futures_util::future::join_all(entries.iter().map(|(name, entry)| Server::spawn(name, entry))).await;
    let mut servers = Vec::new();
    let mut status = Vec::new();
    for ((name, _), r) in entries.iter().zip(started) {
        match r {
            Ok(s) => {
                status.push(json!({"name": name, "status": "connected"}));
                servers.push(s);
            }
            Err(e) => {
                eprintln!("mantra mock-claude: MCP server {name} failed to start: {e}");
                status.push(json!({"name": name, "status": "failed"}));
            }
        }
    }
    (servers, status)
}

/// Where the stream-json events go: stdout, or (in tests) a list to look at.
enum Out {
    Stdout(tokio::io::Stdout),
    #[cfg(test)]
    Captured(Vec<Value>),
}

struct Mock {
    out: Out,
    /// Every server of `--mcp-config` that started, the bridge (`mantra`) among them.
    servers: Vec<Server>,
    /// Whether this agent already looked at a server's `scene_info` (once is the demo's point).
    saw_scene: bool,
    /// Whether a worker offered `mantra_screenshot` takes one (once): only with
    /// `$MANTRA_SCREENSHOT_CMD` set, so a plain `--demo` never captures a real screen.
    screenshots: bool,
    saw_screen: bool,
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
        match &mut self.out {
            Out::Stdout(out) => {
                let _ = out.write_all(v.to_string().as_bytes()).await;
                let _ = out.write_all(b"\n").await;
                let _ = out.flush().await;
            }
            #[cfg(test)]
            Out::Captured(all) => all.push(v),
        }
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
        self.mcp_use("mantra", name, args).await
    }
    /// The same for any server's tool, under the name `claude` gives it: `mcp__<server>__<tool>`.
    async fn mcp_use(&mut self, server: &str, tool: &str, args: Value) -> (bool, String) {
        let mid = self.uid("msg");
        let tool_id = self.uid("tu");
        let full_name = format!("mcp__{server}__{tool}");
        self.emit(json!({"type": "assistant", "message": {"id": mid, "content": [{"type": "tool_use", "id": tool_id, "name": full_name, "input": args.clone()}]}})).await;
        self.sleep(60).await;
        let none_at_all = self.servers.is_empty();
        let (ok, text, images) = match self.servers.iter_mut().find(|s| s.name == server) {
            Some(s) => s.call_tool(tool, args).await,
            None if none_at_all => (false, "mantra mock-claude: no --mcp-config given".into(), vec![]),
            None => (false, format!("mantra mock-claude: MCP server {server} is not connected"), vec![]),
        };
        // An image comes back as `claude` shows a model one: an image block in the tool_result's
        // content, beside the text — the image first here, so mantra.log's 200-character line
        // of this event shows it (serde_json writes the keys in order, `content` first).
        let content = if images.is_empty() {
            json!(text)
        } else {
            let mut parts: Vec<Value> = images.iter().map(|(mime, data)| json!({"type": "image", "source": {"type": "base64", "media_type": mime, "data": data}})).collect();
            parts.push(json!({"type": "text", "text": text}));
            Value::Array(parts)
        };
        self.emit(json!({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": tool_id, "is_error": !ok, "content": content}]}})).await;
        self.sleep(40).await;
        self.tokens += 500 + text.len() as u64;
        (ok, text)
    }
    /// The first of the agent's own servers (never the bridge) whose tools include `tool`.
    fn server_offering(&self, tool: &str) -> Option<String> {
        self.servers.iter().find(|s| s.name != "mantra" && s.tools.iter().any(|t| t == tool)).map(|s| s.name.clone())
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
    // Given a server with a scene (`--demo`'s `mantra mock-mcp` as `demo`, or a 3D app's own),
    // the worker looks at it once before touching anything, as a real one would.
    let mut saw = String::new();
    if let Some(server) = m.server_offering("scene_info").filter(|_| !m.saw_scene) {
        m.saw_scene = true;
        let (ok, text) = m.mcp_use(&server, "scene_info", json!({})).await;
        let first = text.lines().next().unwrap_or("").trim().to_string();
        saw = if ok { format!("\n\nLooked at the scene through `{server}` first — {first}.") } else { format!("\n\nCould not read the scene through `{server}`: {}", crate::util::trunc(&first, 80)) };
    }
    // Offered Mantra's screenshot tool (a model that takes images) and allowed to: one look at
    // the screen, the image arriving through the bridge as MCP image content.
    let offered = m.servers.iter().any(|s| s.name == "mantra" && s.tools.iter().any(|t| t == "mantra_screenshot"));
    if m.screenshots && offered && !m.saw_screen {
        m.saw_screen = true;
        let (ok, text) = m.mcp_use("mantra", "mantra_screenshot", json!({})).await;
        let first = crate::util::trunc(text.lines().next().unwrap_or("").trim(), 120);
        saw.push_str(&if ok { format!("\n\nTook one look at the screen ({}).", crate::mock::shot_seen(&text)) } else { format!("\n\nCould not see the screen: {first}") });
    }
    m.bash(&format!("rg -n \"struct|fn\" {dir} || true")).await;
    let file = format!("{dir}/{}.rs", id.replace('-', "_"));
    let camel = crate::mock::camel(&id);
    let content = format!("//! {title}\n\npub struct {camel};\n\nimpl {camel} {{\n    pub fn new() -> Self {{ {camel} }}\n}}\n");
    m.write_file(&file, &content).await;
    m.say(&format!("Implemented **{title}** in `{file}`.{saw}\n\nSTATUS: done\nSUMMARY: added {camel} per the task.")).await;
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
    let (servers, mcp_servers) = match get("--mcp-config") {
        Some(cfg) => spawn_servers(&cfg).await,
        None => (vec![], vec![]),
    };
    let tools: Vec<String> = servers.iter().flat_map(|s| s.tools.iter().map(move |t| format!("mcp__{}__{t}", s.name))).collect();
    let screenshots = std::env::var_os("MANTRA_SCREENSHOT_CMD").is_some_and(|c| !c.is_empty());
    let mut m = Mock { out: Out::Stdout(tokio::io::stdout()), servers, saw_scene: false, screenshots, saw_screen: false, cwd, model: model.clone(), dev, speed, msg_n: 0, tokens: 0 };
    m.emit(json!({"type": "system", "subtype": "init", "session_id": session_id, "model": model, "tools": tools, "mcp_servers": mcp_servers, "permissionMode": "bypassPermissions", "apiKeySource": "none", "capabilities": {}})).await;

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

#[cfg(test)]
mod tests {
    use super::*;

    /// A stdio MCP server in a few lines of `sh`, offering one tool (`$TOOL`) that answers
    /// `$TEXT` — both from the config's `env`, so the env reaches the server too. It says
    /// something unasked before its `tools/list` answer and pings back before a call's, the
    /// chatter a real server may send. serde_json writes `"id":N` without spaces, so `sed` finds it.
    const STAND_IN: &str = r#"while IFS= read -r line; do
  id=$(printf '%s\n' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*) printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2024-11-05","capabilities":{"tools":{}},"serverInfo":{"name":"stand-in","version":"1"}}}\n' "$id" ;;
    *'"method":"tools/list"'*) printf '{"jsonrpc":"2.0","method":"notifications/message","params":{"level":"info","data":"hello"}}\n{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"%s","inputSchema":{"type":"object"}}]}}\n' "$id" "$TOOL" ;;
    *'"method":"tools/call"'*) printf '{"jsonrpc":"2.0","id":"srv-1","method":"ping"}\n{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"%s"}%s],"isError":false}}\n' "$id" "$TEXT" "$EXTRA" ;;
  esac
done
"#;

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mantra-mock-claude-{}", crate::util::uuid_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn mock(servers: Vec<Server>, cwd: PathBuf) -> Mock {
        let dev = "You build things.\nmantra-role: worker\n## Your task: p1-models — Domain models\nScope: src/models/**\n".to_string();
        Mock { out: Out::Captured(vec![]), servers, saw_scene: false, screenshots: false, saw_screen: false, cwd, model: "mock".into(), dev, speed: 1000.0, msg_n: 0, tokens: 0 }
    }

    fn captured(m: &Mock) -> &[Value] {
        match &m.out {
            Out::Captured(all) => all,
            Out::Stdout(_) => &[],
        }
    }

    /// The first content block of every event that has one.
    fn blocks(events: &[Value]) -> Vec<&Value> {
        events.iter().filter_map(|e| e.pointer("/message/content/0")).collect()
    }

    /// The `tool_use` names the mock emitted, in order.
    fn calls(events: &[Value]) -> Vec<String> {
        blocks(events).into_iter().filter(|c| c["type"] == "tool_use").filter_map(|c| c["name"].as_str().map(str::to_string)).collect()
    }

    /// Every server in `--mcp-config` starts, not only `mantra`: two stand-ins come up with their
    /// tools, one that cannot start is `failed` without holding the others up, and a worker
    /// calls the scene server's `scene_info` once — as `mcp__demo__scene_info`, through the server
    /// — and says what it saw.
    #[tokio::test]
    async fn every_configured_server_starts_and_the_worker_reads_the_scene_once() {
        let dir = scratch();
        let script = dir.join("stand-in.sh");
        std::fs::write(&script, STAND_IN).unwrap();
        let script = script.to_string_lossy().to_string();
        let cfg = json!({"mcpServers": {
            "demo": {"command": "sh", "args": [script], "env": {"TOOL": "scene_info", "TEXT": "Scene demo: 3 objects"}},
            "files": {"command": "sh", "args": [script], "env": {"TOOL": "read_text_file", "TEXT": "hello"}},
            "broken": {"command": dir.join("no-such-server").to_string_lossy(), "args": []},
        }});
        let (servers, status) = spawn_servers(&cfg.to_string()).await;
        let names: Vec<(&str, Vec<&str>)> = servers.iter().map(|s| (s.name.as_str(), s.tools.iter().map(String::as_str).collect())).collect();
        assert_eq!(names, vec![("demo", vec!["scene_info"]), ("files", vec!["read_text_file"])]);
        assert_eq!(status, vec![json!({"name": "broken", "status": "failed"}), json!({"name": "demo", "status": "connected"}), json!({"name": "files", "status": "connected"})]);

        let mut m = mock(servers, dir.clone());
        assert_eq!(m.server_offering("scene_info").as_deref(), Some("demo"));
        assert_eq!(m.server_offering("read_text_file").as_deref(), Some("files"));
        worker_turn(&mut m).await;
        worker_turn(&mut m).await; // a second turn (a steer, say) does not look again
        let events = captured(&m);
        let names = calls(events);
        assert_eq!(names.iter().filter(|n| n.starts_with("mcp__")).collect::<Vec<_>>(), vec!["mcp__demo__scene_info"], "{names:?}");
        assert_eq!(names[0], "mcp__demo__scene_info", "the scene comes first");
        let result = blocks(events).into_iter().find(|c| c["type"] == "tool_result").unwrap();
        assert_eq!((result["is_error"].as_bool(), result["content"].as_str()), (Some(false), Some("Scene demo: 3 objects")));
        let said: Vec<&str> = blocks(events).into_iter().filter(|c| c["type"] == "text").filter_map(|c| c["text"].as_str()).collect();
        assert!(said[0].contains("through `demo` first — Scene demo: 3 objects.") && said[0].contains("\n\nSTATUS: done\nSUMMARY: "), "{}", said[0]);
        assert!(!said[1].contains("scene"), "{}", said[1]);
        assert!(dir.join("src/models/p1_models.rs").is_file());
        drop(m);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A worker offered `mantra_screenshot` takes one look when screenshots are allowed (the
    /// script's `$MANTRA_SCREENSHOT_CMD`), and the image the bridge sends back reaches the stream
    /// as an image block of the tool_result — the shape `hub/claude.rs` reads; never otherwise.
    #[tokio::test]
    async fn a_worker_takes_one_screenshot_and_its_image_comes_back() {
        let dir = scratch();
        let script = dir.join("stand-in.sh");
        std::fs::write(&script, STAND_IN).unwrap();
        let script = script.to_string_lossy().to_string();
        let image = r#",{"type":"image","mimeType":"image/png","data":"iVBORw0KGgo="}"#;
        let cfg = json!({"mcpServers": {"mantra": {"command": "sh", "args": [script], "env": {"TOOL": "mantra_screenshot", "TEXT": "saved to /r/screenshots/p1-models-1.png", "EXTRA": image}}}});
        let (servers, _) = spawn_servers(&cfg.to_string()).await;
        let mut m = mock(servers, dir.clone());
        worker_turn(&mut m).await;
        assert!(!calls(captured(&m)).iter().any(|n| n.contains("screenshot")), "not allowed: no look");
        let (servers, _) = spawn_servers(&cfg.to_string()).await;
        let mut m = Mock { screenshots: true, ..mock(servers, dir.clone()) };
        worker_turn(&mut m).await;
        worker_turn(&mut m).await;
        let events = captured(&m);
        assert_eq!(calls(events).iter().filter(|n| n.contains("screenshot")).collect::<Vec<_>>(), vec!["mcp__mantra__mantra_screenshot"], "once");
        let result = blocks(events).into_iter().find(|c| c["type"] == "tool_result").unwrap();
        assert_eq!(result["content"], json!([{"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "iVBORw0KGgo="}}, {"type": "text", "text": "saved to /r/screenshots/p1-models-1.png"}]));
        let line = events.iter().find(|e| e.pointer("/message/content/0/type") == Some(&json!("tool_result"))).unwrap().to_string();
        assert!(crate::util::trunc(&line, 200).contains(r#""media_type":"image/png""#), "the log's line shows the image: {line}");
        let said: Vec<&str> = blocks(events).into_iter().filter(|c| c["type"] == "text").filter_map(|c| c["text"].as_str()).collect();
        assert!(said[0].contains("Took one look at the screen (p1-models-1.png)."), "{}", said[0]);
        drop(m);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Without servers nothing changes: no MCP call, and a `mantra_*` call fails the way it
    /// always did with no `--mcp-config`.
    #[tokio::test]
    async fn without_servers_the_worker_makes_no_mcp_call() {
        let dir = scratch();
        let mut m = mock(vec![], dir.clone());
        worker_turn(&mut m).await;
        assert!(calls(captured(&m)).iter().all(|n| !n.starts_with("mcp__")), "{:?}", calls(captured(&m)));
        let (ok, text) = m.tool_use("mantra_status", json!({})).await;
        assert!(!ok && text.contains("no --mcp-config given"), "{text}");
        let (servers, status) = spawn_servers("not json").await;
        assert!(servers.is_empty() && status.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
