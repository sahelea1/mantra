//! `--demo` for the Bija backend: an in-process mock OpenAI Chat Completions server (SPEC §9.5),
//! scripted per `mantra-role:` like `mock.rs`, so a `mantra --demo` run on bija models drives the
//! real `bija` binary end to end with zero API calls.
//!
//! `mock.rs` *is* the agent (it fakes a whole Codex app-server); this file is only the model
//! behind one. The real `bija` runs the loop, executes `write_file` itself, forwards `mantra_*`
//! calls to Mantra and keeps the transcript, so a demo run exercises the same code a real
//! provider would. That makes the server stateless by construction: every request carries the
//! whole conversation, and the script reads its position from it — the last user message is the
//! prompt of the current turn, the assistant messages after it are the steps already taken, and
//! the trailing tool message is what the last step got back (a `REJECTED` plan, say).
//!
//! Two rules keep it from ever wedging a session: a request that offers no tools (bija's
//! compaction call) is always answered with text, and a tool the request does not offer is never
//! called. The wire is the real one — SSE `data:` chunks with `choices[0].delta`, tool calls
//! streamed by `index` with their arguments in pieces, a usage chunk, `data: [DONE]` — plus the
//! non-streaming JSON answer for `--no-stream`, so bija's parser sees what a provider sends.
//! `GET /v1/models` lists the demo's models the way a provider's catalogue does, so discovery
//! (`D` in `/models`) works on the demo's `bija` provider too.
//!
//! Knobs: `MANTRA_MOCK_SPEED` scales every delay like `mock.rs`; `MANTRA_MOCK_FLAKY=<n>` answers
//! every n-th request with a 503 (`retry-after: 1`), to watch bija's retries in the demo.

use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// A whole demo transcript (a gate prompt carries the phase's diff) can outgrow axum's 2 MB
/// default; a real provider takes far more than this.
const BODY_LIMIT: usize = 64 << 20;

/// Starts the server on 127.0.0.1 (random port) and returns the port. Lives until the process ends.
pub async fn start() -> std::io::Result<u16> {
    start_with(Mock::from_env()).await
}

async fn start_with(mock: Mock) -> std::io::Result<u16> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let app = Router::new().route("/v1/chat/completions", post(completions)).route("/v1/models", get(models)).layer(DefaultBodyLimit::max(BODY_LIMIT)).with_state(Arc::new(mock));
    tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            crate::mlog!("mock-openai: server stopped: {e}");
        }
    });
    Ok(port)
}

/// The server's whole state: its pacing, and counters that make ids unique and drive
/// `MANTRA_MOCK_FLAKY`. Nothing about any conversation is kept here.
struct Mock {
    speed: f64,
    /// Every n-th request fails with a 503; 0 = never (1 would fail everything forever).
    flaky: u64,
    requests: AtomicU64,
    ids: AtomicU64,
    /// Whether a worker offered `mantra_screenshot` calls it: only with `$MANTRA_SCREENSHOT_CMD`
    /// set. The demo's models take images, so without this a `--demo` on a desktop with `grim`
    /// or `scrot` would capture the real screen; with it, the e2e script's stand-in is used.
    screenshots: bool,
}

impl Mock {
    fn new(speed: f64, flaky: u64) -> Mock {
        // Seeded from the clock so a resumed bija session (a restarted Mantra, a fresh server)
        // never sees a call id it already holds in its transcript.
        let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64 & 0xffff_ffff).unwrap_or(0);
        Mock { speed: speed.max(0.05), flaky: if flaky < 2 { 0 } else { flaky }, requests: AtomicU64::new(0), ids: AtomicU64::new(seed << 16), screenshots: false }
    }

    fn from_env() -> Mock {
        let speed = std::env::var("MANTRA_MOCK_SPEED").ok().and_then(|s| s.parse::<f64>().ok()).unwrap_or(1.0);
        let flaky = std::env::var("MANTRA_MOCK_FLAKY").ok().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
        Mock { screenshots: std::env::var_os("MANTRA_SCREENSHOT_CMD").is_some_and(|c| !c.is_empty()), ..Mock::new(speed, flaky) }
    }

    fn uid(&self, prefix: &str) -> String {
        format!("{prefix}{:012x}", self.ids.fetch_add(1, Ordering::Relaxed))
    }

    /// Counts this request and says whether it is one `MANTRA_MOCK_FLAKY` fails.
    fn fail_this_one(&self) -> bool {
        let n = self.requests.fetch_add(1, Ordering::Relaxed) + 1;
        self.flaky > 0 && n % self.flaky == 0
    }
}

async fn completions(State(mock): State<Arc<Mock>>, body: Bytes) -> Response {
    let req: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return api_error(StatusCode::BAD_REQUEST, "invalid_request_error", &format!("the request body is not valid JSON: {e}")),
    };
    let turn = match Turn::read(&req) {
        Ok(t) => Turn { screenshots: mock.screenshots, ..t },
        Err(e) => return api_error(StatusCode::BAD_REQUEST, "invalid_request_error", &e),
    };
    if mock.fail_this_one() {
        let mut r = api_error(StatusCode::SERVICE_UNAVAILABLE, "server_error", "The server is overloaded, please try again later.");
        r.headers_mut().insert(header::RETRY_AFTER, header::HeaderValue::from_static("1"));
        return r;
    }
    let mut reply = script(&turn);
    for call in &mut reply.calls {
        call.id = mock.uid("call_");
    }
    let head = Head { id: mock.uid("chatcmpl-"), model: req.get("model").and_then(Value::as_str).unwrap_or("mock").to_string(), created: crate::util::unix_secs() };
    let usage = usage(body.len(), &reply, turn.has_history);
    if req.get("stream").and_then(Value::as_bool).unwrap_or(false) {
        stream_response(mock.speed, &head, &reply, &usage)
    } else {
        pause(reply.latency_ms, mock.speed).await;
        Json(completion_json(&head, &reply, &usage)).into_response()
    }
}

/// The models `--demo` puts on its `bija` provider (`config::bija_default_model_entries`), in the
/// listing shape OpenAI-compatible providers answer `GET /models` with, windows included. Read from
/// the registry's own defaults rather than kept here, so the two can never disagree.
async fn models() -> Json<Value> {
    let created = crate::util::unix_secs();
    let data: Vec<Value> = crate::config::bija_default_model_entries("bija")
        .into_iter()
        .map(|m| json!({"id": m.model, "object": "model", "created": created, "owned_by": "mantra-demo", "context_window": m.context_window}))
        .collect();
    Json(json!({"object": "list", "data": data}))
}

/// The error body OpenAI-compatible providers send, so bija classifies it like a real one.
fn api_error(status: StatusCode, kind: &str, message: &str) -> Response {
    (status, Json(json!({"error": {"message": message, "type": kind, "code": Value::Null}}))).into_response()
}

async fn pause(ms: u64, speed: f64) {
    if ms > 0 {
        tokio::time::sleep(Duration::from_millis((ms as f64 / speed) as u64)).await;
    }
}

// ── reading the conversation ────────────────────────────────────────────────────────────────

/// Where the conversation stands, read from the request alone.
#[derive(Debug, Default)]
struct Turn {
    /// The `mantra-role:` marker of the system prompt; `solo` when there is none.
    role: String,
    system: String,
    /// The last user message: the current turn's prompt (a steer starts a new count, which the
    /// worker's `[from …` answer is made for).
    prompt: String,
    /// Assistant messages after the prompt: how many steps of the script are already done.
    step: usize,
    /// The tool result that ends the conversation, if it ends in one.
    last_result: String,
    /// Names of the tools this request offers.
    offered: Vec<String>,
    /// Whether an earlier assistant message exists — its prefix would be served from a cache.
    has_history: bool,
    /// Every tool result of this turn so far, with the name of the call it answers: what the
    /// closing text can say the agent saw.
    results: Vec<(String, String)>,
    /// Whether the scripts may call `mantra_screenshot` (`Mock::screenshots`).
    screenshots: bool,
}

impl Turn {
    fn read(req: &Value) -> Result<Turn, String> {
        let all = req.get("messages").and_then(Value::as_array).ok_or("`messages` must be an array of messages")?;
        // A result's images ride in a `user` message of their own after the results; that is
        // still the same step of the same turn, so it is no prompt and ends nothing.
        let messages: Vec<&Value> = all.iter().filter(|m| !carries_images(m)).collect();
        let role_of = |m: &Value| m.get("role").and_then(Value::as_str).unwrap_or("").to_string();
        let system: Vec<String> = messages.iter().filter(|m| matches!(role_of(m).as_str(), "system" | "developer")).map(|m| text_of(m.get("content"))).collect();
        let system = system.join("\n");
        let role = system.lines().find_map(|l| l.trim().strip_prefix("mantra-role:")).map(|r| r.trim().to_string()).filter(|r| !r.is_empty()).unwrap_or_else(|| "solo".into());
        let last_user = messages.iter().rposition(|m| role_of(m) == "user");
        let tail = &messages[last_user.map(|i| i + 1).unwrap_or(0)..];
        let last_result = match messages.last() {
            Some(m) if role_of(m) == "tool" => text_of(m.get("content")),
            _ => String::new(),
        };
        let offered = req.get("tools").and_then(Value::as_array).map(|tools| tools.iter().filter_map(|t| t.pointer("/function/name").or_else(|| t.get("name")).and_then(Value::as_str).map(str::to_string)).collect()).unwrap_or_default();
        let mut names = std::collections::HashMap::new();
        let mut results = Vec::new();
        for m in tail {
            for call in m.get("tool_calls").and_then(Value::as_array).into_iter().flatten() {
                if let (Some(id), Some(name)) = (call.get("id").and_then(Value::as_str), call.pointer("/function/name").and_then(Value::as_str)) {
                    names.insert(id.to_string(), name.to_string());
                }
            }
            if role_of(m) == "tool" {
                let name = m.get("tool_call_id").and_then(Value::as_str).and_then(|id| names.get(id)).cloned().unwrap_or_default();
                results.push((name, text_of(m.get("content"))));
            }
        }
        Ok(Turn {
            role,
            system,
            prompt: last_user.map(|i| text_of(messages[i].get("content"))).unwrap_or_default(),
            step: tail.iter().filter(|m| role_of(m) == "assistant").count(),
            last_result,
            offered,
            has_history: messages.iter().any(|m| role_of(m) == "assistant"),
            results,
            screenshots: false,
        })
    }

    fn offers(&self, tool: &str) -> bool {
        self.offered.iter().any(|t| t == tool)
    }

    /// The first line of what the last call to `tool` in this turn answered.
    fn result_of(&self, tool: &str) -> Option<String> {
        self.results.iter().rev().find(|(name, _)| name == tool).map(|(_, text)| text.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_string())
    }
}

/// bija's chat wire puts a tool result's images in a `user` message after the results (a `tool`
/// message is text only): `image from <tool>: <path>` and the picture, nothing else.
fn carries_images(m: &Value) -> bool {
    if m.get("role").and_then(Value::as_str) != Some("user") {
        return false;
    }
    let Some(parts) = m.get("content").and_then(Value::as_array) else { return false };
    let image = |p: &Value| p.get("type").and_then(Value::as_str) == Some("image_url");
    let label = |p: &Value| p.get("type").and_then(Value::as_str) == Some("text") && p.get("text").and_then(Value::as_str).is_some_and(|t| t.starts_with("image from "));
    parts.iter().any(image) && parts.iter().all(|p| image(p) || label(p))
}

/// A message's text: a plain string, or the text parts of a content array.
fn text_of(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts.iter().filter_map(|p| p.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}

// ── the scripts ─────────────────────────────────────────────────────────────────────────────

/// One assistant message the model is about to send.
#[derive(Debug, Default, Clone)]
struct Reply {
    reasoning: String,
    text: String,
    calls: Vec<Call>,
    /// Time to first token, before `MANTRA_MOCK_SPEED`.
    latency_ms: u64,
}

#[derive(Debug, Clone)]
struct Call {
    /// Assigned by the server just before sending (unique across the process).
    id: String,
    name: String,
    args: Value,
}

impl Reply {
    fn say(text: impl Into<String>) -> Reply {
        Reply { text: text.into(), latency_ms: 250, ..Reply::default() }
    }
    fn call(name: &str, args: Value) -> Reply {
        Reply::calls(vec![(name.to_string(), args)])
    }
    fn calls(calls: Vec<(String, Value)>) -> Reply {
        Reply { calls: calls.into_iter().map(|(name, args)| Call { id: String::new(), name, args }).collect(), latency_ms: 300, ..Reply::default() }
    }
    fn thinking(mut self, reasoning: impl Into<String>) -> Reply {
        self.reasoning = reasoning.into();
        self
    }
    fn after(mut self, ms: u64) -> Reply {
        self.latency_ms = ms;
        self
    }
}

/// The next step of a fixed sequence, then the closing text once every step has been taken.
fn sequence(turn: &Turn, steps: Vec<Reply>, closing: Reply) -> Reply {
    steps.into_iter().nth(turn.step).unwrap_or(closing)
}

fn script(turn: &Turn) -> Reply {
    if turn.prompt.contains("Reply with exactly: OK") {
        return Reply::say("OK");
    }
    if turn.offered.is_empty() {
        return summary(turn);
    }
    let reply = match turn.role.as_str() {
        "planner" => planner(turn),
        "orchestrator" => orchestrator(turn),
        "worker" => worker(turn),
        "gate" => gate(turn),
        "manager" => manager(turn),
        _ => solo(turn),
    };
    offered_only(reply, &turn.offered)
}

/// A script that wants a tool the request does not offer says so instead: a call bija cannot
/// route would only come back as an error, round after round.
fn offered_only(reply: Reply, offered: &[String]) -> Reply {
    match reply.calls.iter().find(|c| !offered.contains(&c.name)) {
        Some(c) => Reply::say(format!("I would call `{}` next, but this session does not offer it (it has: {}). Stopping here.", c.name, offered.join(", "))),
        None => reply,
    }
}

/// The answer to a request without tools — bija's compaction call asking for a hand-off summary.
fn summary(turn: &Turn) -> Reply {
    let task = crate::util::trunc(turn.prompt.lines().find(|l| !l.trim().is_empty()).unwrap_or("the task"), 120);
    Reply::say(format!("Hand-off summary (demo model).\n- Goal: {task}\n- Done so far: the steps in the transcript above, all of which succeeded.\n- Next: carry on from the last step; nothing is blocked."))
}

fn planner(turn: &Turn) -> Reply {
    let p = &turn.prompt;
    if p.contains("[mantra:plan]") || p.contains("[mantra:revise]") || p.contains("[mantra] You haven't") || (p.contains("[mantra:resume]") && p.contains("mantra_submit_plan")) {
        // `mock_plan(true)` gives two parallel tasks one scope on purpose: the engine rejects it
        // and the resubmission exercises the same fix-and-retry path a real planner takes. Two
        // resubmissions at most, then text — never a loop against the engine's nudges.
        // a goal that asks for a swarm gets a phase of worker-swarm tasks too
        let swarm = crate::mock::wants_swarm(p);
        if turn.step == 0 {
            return Reply::call("mantra_submit_plan", crate::mock::mock_plan(p.contains("[mantra:plan]"), swarm)).thinking("**Designing phases**\n\nFoundations must land before features; the three features are independent, so they run as one parallel phase with disjoint scopes. Integration and docs last.").after(900);
        }
        if turn.last_result.starts_with("REJECTED") && turn.step < 3 {
            return Reply::call("mantra_submit_plan", crate::mock::mock_plan(false, swarm)).thinking("**Fixing the plan**\n\nTwo parallel tasks claimed the same scope. Giving the config loader its own directory.");
        }
        return Reply::say(crate::mock::mock_plan_said(swarm));
    }
    if p.contains("[mantra:finale]") || p.contains("[mantra:event]") {
        let report = Reply::call("mantra_gate_report", json!({"pass": true, "summary": "Every plan item is implemented; the final checks pass."})).thinking("**Final verification**\n\nComparing the plan against the run's diff and the reports so far.");
        return sequence(turn, vec![report], Reply::say("Verification complete: everything in the plan is implemented and checked."));
    }
    if p.contains("[mantra:reprompt]") {
        return sequence(turn, vec![Reply::call("mantra_status", json!({}))], Reply::say("Handled the re-prompt: no plan change needed; the running workers already cover it."));
    }
    if p.contains("[mantra:question]") {
        let answer = Reply::call("mantra_brief_orchestrator", json!({"message": "Decision: keep the API surface as planned; opaque tokens, stored hashed."}));
        return sequence(turn, vec![answer], Reply::say("Answered the orchestrator. No plan change."));
    }
    if p.contains("[mantra:escalation]") {
        let resume = Reply::call("mantra_resume_run", json!({"note": "Run the checks exactly as listed, and run the failing one yourself before reporting."}));
        return sequence(turn, vec![resume], Reply::say("Resumed the run with a note for the stuck agent."));
    }
    Reply::say("Noted.")
}

fn orchestrator(turn: &Turn) -> Reply {
    let p = &turn.prompt;
    if p.contains("[mantra:phase]") {
        let ids = phase_task_ids(p);
        let mut steps = Vec::new();
        if !ids.is_empty() {
            // All spawns in one assistant message: parallel tool calls, streamed by index — the
            // shape bija's stream parser must keep apart.
            let spawns = ids.iter().map(|id| ("mantra_spawn".to_string(), json!({"task_id": id}))).collect();
            steps.push(Reply::calls(spawns).thinking(format!("**Starting the phase**\n\n{} independent tasks — spawning them all in parallel.", ids.len())));
        }
        steps.push(Reply::call("mantra_wait", json!({})));
        return sequence(turn, steps, Reply::say(format!("Spawned {} — waiting for their reports.", if ids.is_empty() { "nothing".to_string() } else { ids.join(", ") })));
    }
    if p.contains("[mantra:handoff]") {
        return Reply::say("Handoff: phase complete. Interfaces are in place; keep naming consistent with `models::*`. No open risks.");
    }
    sequence(turn, vec![Reply::call("mantra_wait", json!({}))], Reply::say("Nothing to correct — waiting for the next event."))
}

/// The task ids in the ```json block of a `[mantra:phase]` brief (the same read `mock.rs` does).
fn phase_task_ids(prompt: &str) -> Vec<String> {
    let block = prompt.split("```json").nth(1).and_then(|s| s.split("```").next()).unwrap_or("{}");
    let phase: Value = serde_json::from_str(block).unwrap_or_else(|_| json!({}));
    phase.get("tasks").and_then(Value::as_array).map(|tasks| tasks.iter().filter_map(|t| t.get("id").and_then(Value::as_str).map(str::to_string)).collect()).unwrap_or_default()
}

fn worker(turn: &Turn) -> Reply {
    let (id, title, scope) = crate::mock::task_info(&turn.system);
    let dir = scope.first().map(|s| s.trim_end_matches("/**").trim_end_matches("/*").to_string()).unwrap_or_else(|| "src".into());
    let camel = crate::mock::camel(&id);
    if turn.prompt.contains("[from") {
        return Reply::say(format!("Adjusted as asked.\n\nSTATUS: done\nSUMMARY: {title} — applied the correction; changes stay within {dir}/."));
    }
    // A group member's urgent note arrives as a steer, a turn of its own: noted, nothing to redo.
    if turn.prompt.contains("[akasha ·") {
        return Reply::say(format!("Noted — nothing in {dir}/ depends on it.\n\nSTATUS: done\nSUMMARY: {title} — unchanged by the note."));
    }
    let file = format!("{dir}/{}.rs", id.replace('-', "_"));
    let content = format!("//! {title}\n\npub struct {camel};\n\nimpl {camel} {{\n    pub fn new() -> Self {{ {camel} }}\n    pub fn run(&self) -> Result<(), String> {{ Ok(()) }}\n}}\n");
    // Each task takes its own time, so a phase's workers report one by one, as they would.
    let h: u64 = id.bytes().map(u64::from).sum();
    let write = Reply::call("write_file", json!({"path": file, "content": content})).thinking(format!("**{title}**\n\nA small, self-contained type in `{dir}/` matches the task.")).after(400 + (h % 5) * 300);
    let report = Reply::say(format!("Implemented **{title}** in `{file}`.{}\n\nSTATUS: done\nSUMMARY: added {camel} with a constructor and `run`; the change stays within {dir}/.", what_i_saw(turn)));
    // Akasha (`--demo` switches it on): the models worker, in a group (the tool is
    // offered only then), shares the one fact the others will need first — unless the notes it
    // was briefed with already hold it. One author, so the group's notes hold it once.
    let mut steps = vec![];
    if dir == "src/models" && turn.offered.iter().any(|t| t == "mantra_akasha_note") && !turn.prompt.contains(crate::mock::AKASHA_NOTE) {
        steps.push(Reply::call("mantra_akasha_note", json!({"text": crate::mock::AKASHA_NOTE})).thinking("**Worth sharing**\n\nThe others will want to know where the models are."));
    }
    // MCP (`--demo` gives every role the `demo` server, `mantra mock-mcp`): a look at the scene
    // first, through bija's MCP client — the real one, end to end. Then, with vision on, one look
    // at the screen through Mantra's own tool; whatever either answers, the work goes on.
    if turn.offers(DEMO_SCENE) {
        steps.push(Reply::call(DEMO_SCENE, json!({})).thinking("**Looking first**\n\nThis session has a scene server; what is in the scene before anything is written."));
    }
    if turn.screenshots && turn.offers("mantra_screenshot") {
        steps.push(Reply::call("mantra_screenshot", json!({})).thinking("**A look at the screen**\n\nOne capture of the host's screen, to see the state it is in."));
    }
    steps.push(write);
    // …and a quick look that the file holds what it should: a real command through bija's `bash`.
    if turn.offered.iter().any(|t| t == "bash") {
        steps.push(Reply::call("bash", json!({"command": format!("grep -c 'pub fn' {file}")})).thinking("**Checking the file**\n\nTwo functions expected: `new` and `run`."));
    }
    sequence(turn, steps, report)
}

/// The demo MCP server's scene tool, under the name bija offers it.
const DEMO_SCENE: &str = "mcp__demo__scene_info";

/// A worker's word on what it looked at this turn, for its report (empty when it looked at
/// nothing): the request is all the server has, so it is read back from the turn's results.
fn what_i_saw(turn: &Turn) -> String {
    let mut saw = Vec::new();
    if let Some(scene) = turn.result_of(DEMO_SCENE) {
        saw.push(format!("the scene through `demo` ({})", crate::util::trunc(&scene, 120)));
    }
    if let Some(shot) = turn.result_of("mantra_screenshot") {
        saw.push(format!("the screen ({})", crate::mock::shot_seen(&shot)));
    }
    if saw.is_empty() {
        String::new()
    } else {
        format!("\n\nLooked at {} first.", saw.join(" and "))
    }
}

fn gate(turn: &Turn) -> Reply {
    let report = Reply::call("mantra_gate_report", json!({"pass": true, "summary": "Coherent, checks green."})).thinking("**Reviewing the merged result**\n\nThe parallel changes fit together: names, interfaces, no duplicated logic.").after(600);
    sequence(turn, vec![report], Reply::say("GATE: pass — everything is coherent and the checks are green."))
}

fn manager(turn: &Turn) -> Reply {
    if turn.prompt.contains("[mantra:escalation]") {
        let resume = Reply::call("mantra_resume_run", json!({"note": "Try once more, and run the failing check yourself before reporting."}));
        return sequence(turn, vec![resume], Reply::say("Resumed the run with a hint for the stuck agent."));
    }
    sequence(turn, vec![Reply::call("mantra_wait", json!({}))], Reply::say("Nothing to change — waiting for the next event."))
}

fn solo(turn: &Turn) -> Reply {
    let ask = crate::util::trunc(turn.prompt.lines().next().unwrap_or(""), 60);
    let look = Reply::call("list_dir", json!({})).thinking(format!("**Understanding the request**\n\nThe user wants: \"{ask}\". A look at the project first."));
    let answer = Reply::say(format!("This is the demo model behind bija: the tools are real (I just listed the project), the answers are scripted. You asked: \"{ask}\".\n\nPoint this alias at a real provider in `/models` and the same harness does the actual work."));
    sequence(turn, vec![look], answer)
}

// ── the wire ────────────────────────────────────────────────────────────────────────────────

/// What every chunk of one completion repeats.
struct Head {
    id: String,
    model: String,
    created: u64,
}

/// Plausible usage: the request's size in tokens (the tool definitions included), an output of
/// what the reply says, and the prefix a provider would serve from its cache once there is one.
/// It stays in the low thousands for the demo's conversations, far from any compaction threshold.
fn usage(request_bytes: usize, reply: &Reply, has_history: bool) -> Value {
    let prompt = 300 + request_bytes as u64 / 4;
    let args: usize = reply.calls.iter().map(|c| c.args.to_string().len()).sum();
    let completion = 12 + (reply.text.len() + reply.reasoning.len() + args) as u64 / 4;
    let cached = if has_history { prompt * 3 / 4 } else { 0 };
    json!({"prompt_tokens": prompt, "completion_tokens": completion, "total_tokens": prompt + completion, "prompt_tokens_details": {"cached_tokens": cached}})
}

fn finish_reason(reply: &Reply) -> &'static str {
    if reply.calls.is_empty() {
        "stop"
    } else {
        "tool_calls"
    }
}

fn chunk(head: &Head, delta: Value, finish: Option<&str>) -> Value {
    json!({"id": head.id, "object": "chat.completion.chunk", "created": head.created, "model": head.model,
           "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]})
}

/// Every `data:` payload of a streamed completion, in order, each with the pause before it
/// (milliseconds at speed 1). Pure, so the shape is testable without the pacing.
fn stream_chunks(head: &Head, reply: &Reply, usage: &Value) -> Vec<(Value, u64)> {
    let mut out = vec![(chunk(head, json!({"role": "assistant", "content": ""}), None), 0)];
    for piece in words(&reply.reasoning) {
        out.push((chunk(head, json!({"reasoning_content": piece}), None), 40));
    }
    for piece in words(&reply.text) {
        out.push((chunk(head, json!({"content": piece}), None), 30));
    }
    for (index, call) in reply.calls.iter().enumerate() {
        let start = json!({"tool_calls": [{"index": index, "id": call.id, "type": "function", "function": {"name": call.name, "arguments": ""}}]});
        out.push((chunk(head, start, None), 20));
        let args = call.args.to_string();
        for piece in pieces(&args, 64) {
            out.push((chunk(head, json!({"tool_calls": [{"index": index, "function": {"arguments": piece}}]}), None), 4));
        }
    }
    out.push((chunk(head, json!({}), Some(finish_reason(reply))), 0));
    out.push((json!({"id": head.id, "object": "chat.completion.chunk", "created": head.created, "model": head.model, "choices": [], "usage": usage}), 0));
    out
}

/// The `stream: false` answer: the same reply as one `chat.completion`.
fn completion_json(head: &Head, reply: &Reply, usage: &Value) -> Value {
    let mut message = json!({"role": "assistant", "content": if reply.text.is_empty() && !reply.calls.is_empty() { Value::Null } else { Value::String(reply.text.clone()) }});
    if !reply.reasoning.is_empty() {
        message["reasoning_content"] = Value::String(reply.reasoning.clone());
    }
    if !reply.calls.is_empty() {
        message["tool_calls"] = reply.calls.iter().map(|c| json!({"id": c.id, "type": "function", "function": {"name": c.name, "arguments": c.args.to_string()}})).collect();
    }
    json!({"id": head.id, "object": "chat.completion", "created": head.created, "model": head.model,
           "choices": [{"index": 0, "message": message, "finish_reason": finish_reason(reply)}], "usage": usage})
}

/// Streams the chunks from a spawned producer that sleeps between them; a client that hangs up
/// drops the receiver, which ends the producer at its next send.
fn stream_response(speed: f64, head: &Head, reply: &Reply, usage: &Value) -> Response {
    let chunks = stream_chunks(head, reply, usage);
    let latency = reply.latency_ms;
    let (tx, rx) = tokio::sync::mpsc::channel::<Bytes>(16);
    tokio::spawn(async move {
        pause(latency, speed).await;
        for (payload, wait) in chunks {
            pause(wait, speed).await;
            if tx.send(Bytes::from(format!("data: {payload}\n\n"))).await.is_err() {
                return;
            }
        }
        let _ = tx.send(Bytes::from_static(b"data: [DONE]\n\n")).await;
    });
    let body = futures_util::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|b| (Ok::<Bytes, std::convert::Infallible>(b), rx)) });
    (StatusCode::OK, [(header::CONTENT_TYPE, "text/event-stream"), (header::CACHE_CONTROL, "no-cache")], Body::from_stream(body)).into_response()
}

/// Text in two-word pieces, the way `mock.rs` streams it.
fn words(text: &str) -> Vec<String> {
    let parts: Vec<&str> = text.split_inclusive(' ').collect();
    parts.chunks(2).map(|c| c.concat()).collect()
}

/// `s` in pieces of at most `max` characters, cut on character boundaries (the demo plan has
/// non-ASCII in it, and a provider never splits inside a character).
fn pieces(s: &str, max: usize) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    for (n, (i, _)) in s.char_indices().enumerate() {
        if n > 0 && n % max.max(1) == 0 {
            out.push(&s[start..i]);
            start = i;
        }
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    struct Http {
        status: u16,
        headers: String,
        body: String,
    }

    /// One HTTP/1.1 POST by hand — the same bytes bija's own client puts on the wire.
    async fn post(port: u16, body: &str) -> Http {
        exchange(port, &format!("POST /v1/chat/completions HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nAccept: text/event-stream\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}", body.len())).await
    }

    /// A GET, the way discovery's `curl` asks.
    async fn get(port: u16, path: &str) -> Http {
        exchange(port, &format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAccept: application/json\r\nConnection: close\r\n\r\n")).await
    }

    /// Sends one raw request and reads the whole response.
    async fn exchange(port: u16, req: &str) -> Http {
        let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        s.write_all(req.as_bytes()).await.unwrap();
        let mut raw = Vec::new();
        tokio::time::timeout(Duration::from_secs(10), s.read_to_end(&mut raw)).await.expect("the response never ended").unwrap();
        let raw = String::from_utf8(raw).unwrap();
        let (head, rest) = raw.split_once("\r\n\r\n").unwrap();
        let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
        let headers = head.to_ascii_lowercase();
        let body = if headers.contains("transfer-encoding: chunked") { dechunk(rest) } else { rest.to_string() };
        Http { status, headers, body }
    }

    fn dechunk(mut s: &str) -> String {
        let mut out = String::new();
        loop {
            let (size, rest) = s.split_once("\r\n").unwrap();
            let n = usize::from_str_radix(size.trim(), 16).unwrap();
            if n == 0 {
                return out;
            }
            out.push_str(&rest[..n]);
            s = &rest[n + 2..];
        }
    }

    /// What a client assembles from the stream.
    #[derive(Default, Debug)]
    struct Parsed {
        text: String,
        reasoning: String,
        /// (id, name, arguments) by index.
        calls: Vec<(String, String, String)>,
        finish: String,
        usage: Value,
        done: bool,
    }

    fn parse_sse(body: &str) -> Parsed {
        let mut p = Parsed::default();
        for event in body.split("\n\n").filter(|e| !e.trim().is_empty()) {
            let data = event.strip_prefix("data: ").unwrap();
            if data == "[DONE]" {
                p.done = true;
                continue;
            }
            assert!(!p.done, "nothing may follow [DONE]");
            let v: Value = serde_json::from_str(data).unwrap();
            assert_eq!(v["object"], "chat.completion.chunk");
            if let Some(u) = v.get("usage") {
                p.usage = u.clone();
            }
            let Some(choice) = v["choices"].get(0) else { continue };
            let d = &choice["delta"];
            p.text.push_str(d["content"].as_str().unwrap_or(""));
            p.reasoning.push_str(d["reasoning_content"].as_str().unwrap_or(""));
            for tc in d["tool_calls"].as_array().into_iter().flatten() {
                let i = tc["index"].as_u64().unwrap() as usize;
                if let Some(id) = tc["id"].as_str() {
                    assert_eq!(i, p.calls.len(), "calls start in index order");
                    p.calls.push((id.into(), tc["function"]["name"].as_str().unwrap().into(), String::new()));
                }
                p.calls[i].2.push_str(tc["function"]["arguments"].as_str().unwrap_or(""));
            }
            if let Some(f) = choice["finish_reason"].as_str() {
                p.finish = f.into();
            }
        }
        p
    }

    async fn server() -> u16 {
        start_with(Mock::new(1000.0, 0)).await.unwrap()
    }

    fn tools(names: &[&str]) -> Value {
        names.iter().map(|n| json!({"type": "function", "function": {"name": n, "description": "", "parameters": {"type": "object"}}})).collect()
    }

    fn request(system: &str, messages: Vec<Value>, offered: &[&str], stream: bool) -> String {
        let mut all = vec![json!({"role": "system", "content": format!("You are a coding agent running in bija, a minimal harness.\n{system}")})];
        all.extend(messages);
        let mut req = json!({"model": "demo-model", "messages": all, "stream": stream, "stream_options": {"include_usage": true}});
        if !offered.is_empty() {
            req["tools"] = tools(offered);
        }
        req.to_string()
    }

    fn user(text: &str) -> Value {
        json!({"role": "user", "content": text})
    }

    /// The assistant message a call came back in, and its result — what bija sends next time.
    fn answered(call: &(String, String, String), result: &str) -> Vec<Value> {
        vec![
            json!({"role": "assistant", "content": null, "tool_calls": [{"id": call.0, "type": "function", "function": {"name": call.1, "arguments": call.2}}]}),
            json!({"role": "tool", "tool_call_id": call.0, "content": result}),
        ]
    }

    const BUILTINS: &[&str] = &["read_file", "write_file", "edit_file", "delete", "list_dir", "glob", "grep", "bash", "job"];
    const PLANNER: &str = "Plan carefully.\nmantra-role: planner\n## Mantra protocol";

    #[tokio::test]
    async fn the_planner_submits_fixes_a_rejection_then_summarises() {
        let port = server().await;
        let offered: Vec<&str> = BUILTINS.iter().copied().chain(["mantra_submit_plan", "mantra_status"]).collect();
        let prompt = user("[mantra:plan]\nUSER REQUEST:\nbuild a todo API");

        let r = post(port, &request(PLANNER, vec![prompt.clone()], &offered, true)).await;
        assert_eq!(r.status, 200);
        assert!(r.headers.contains("content-type: text/event-stream"), "{}", r.headers);
        let first = parse_sse(&r.body);
        assert!(first.done && first.finish == "tool_calls" && first.text.is_empty(), "{first:?}");
        assert!(first.reasoning.contains("Designing phases"));
        assert_eq!(first.calls.len(), 1);
        assert_eq!(first.calls[0].1, "mantra_submit_plan");
        assert!(first.calls[0].0.starts_with("call_"));
        // The arguments arrive in many pieces (the plan has a `↔` in it) and must reassemble exactly.
        let plan: Value = serde_json::from_str(&first.calls[0].2).unwrap();
        assert_eq!(plan, crate::mock::mock_plan(true, false));
        assert!(first.usage["prompt_tokens"].as_u64().unwrap() > 300);
        assert!(first.usage["completion_tokens"].as_u64().unwrap() > 12);
        assert_eq!(first.usage["prompt_tokens_details"]["cached_tokens"], 0, "nothing to cache on the first request");

        let mut history = vec![prompt.clone()];
        history.extend(answered(&first.calls[0], "REJECTED: phase p1: tasks p1-models and p1-config overlap in scope"));
        let second = parse_sse(&post(port, &request(PLANNER, history.clone(), &offered, true)).await.body);
        assert_eq!(second.calls.len(), 1);
        assert_eq!(serde_json::from_str::<Value>(&second.calls[0].2).unwrap(), crate::mock::mock_plan(false, false), "the fixed plan");
        assert_ne!(second.calls[0].0, first.calls[0].0, "call ids are unique");
        assert!(second.usage["prompt_tokens_details"]["cached_tokens"].as_u64().unwrap() > 0);

        history.extend(answered(&second.calls[0], "ACCEPTED. Reply with a 2-4 line summary of the plan and end your turn."));
        let third = parse_sse(&post(port, &request(PLANNER, history, &offered, true)).await.body);
        assert!(third.calls.is_empty() && third.finish == "stop", "{third:?}");
        assert!(third.text.contains("3 phases"), "{}", third.text);
    }

    /// A goal that asks for a swarm gets a fourth phase: ten tiny same-shaped tasks for
    /// worker-swarm, one per module — and the plan validates against the default pattern as it
    /// ships (ten tasks is past the shared pool's eight, within the swarm's 32). The roles list in
    /// the brief names worker-swarm every time; only the goal decides.
    #[tokio::test]
    async fn a_goal_asking_for_a_swarm_gets_a_phase_of_swarm_tasks() {
        let port = server().await;
        let offered = ["mantra_submit_plan"];
        let brief = |goal: &str| user(&format!("[mantra:plan]\nUSER REQUEST:\n{goal}\n\nProject directory: /tmp/x\nWorker roles you can assign:\n- worker-swarm — up to 16 at once: Many tiny, independent tasks at once (model luna, effort auto)\n"));
        let pattern = crate::engine::pattern::Pattern::builtin();
        for (goal, swarm) in [("build a tiny todo CLI, its tests by a swarm", true), ("build a tiny todo CLI", false)] {
            let r = parse_sse(&post(port, &request(PLANNER, vec![brief(goal)], &offered, true)).await.body);
            let sent: Value = serde_json::from_str(&r.calls[0].2).unwrap();
            assert_eq!(sent, crate::mock::mock_plan(true, swarm), "{goal}");
            let mut plan = crate::engine::plan::Plan::from_value(&crate::mock::mock_plan(false, swarm)).unwrap();
            plan.normalize();
            assert!(plan.validate(&pattern).is_ok(), "{goal}: {:?}", plan.validate(&pattern));
            let swarm_tasks: Vec<usize> = plan.phases.iter().map(|ph| ph.tasks.iter().filter(|t| t.role == "worker-swarm").count()).collect();
            if swarm {
                assert_eq!(swarm_tasks, vec![0, 0, 10, 0], "one phase of ten swarm tasks, after the features");
            } else {
                assert!(swarm_tasks.iter().all(|n| *n == 0), "{swarm_tasks:?}");
            }
        }
        assert!(crate::mock::mock_plan_said(true).contains("4 phases") && crate::mock::mock_plan_said(true).contains("17 tasks"));
    }

    #[tokio::test]
    async fn a_planner_rejected_over_and_over_stops_resubmitting() {
        let port = server().await;
        let offered = ["mantra_submit_plan"];
        let mut history = vec![user("[mantra:plan]\nbuild it")];
        for _ in 0..3 {
            let p = parse_sse(&post(port, &request(PLANNER, history.clone(), &offered, true)).await.body);
            assert_eq!(p.calls.len(), 1, "{p:?}");
            history.extend(answered(&p.calls[0], "REJECTED: still wrong"));
        }
        let last = parse_sse(&post(port, &request(PLANNER, history, &offered, true)).await.body);
        assert!(last.calls.is_empty() && !last.text.is_empty(), "{last:?}");
    }

    #[tokio::test]
    async fn the_worker_writes_inside_its_scope_then_reports_its_status() {
        let port = server().await;
        let system = "You build things.\nmantra-role: worker\n## Mantra protocol\n## Your task: p2-api — HTTP API\nScope: src/api/**\nAcceptance: builds, tests pass\n";
        let prompt = user("Implement the REST handlers.\n\n(Context — overall project goal: demo)");
        let first = parse_sse(&post(port, &request(system, vec![prompt.clone()], BUILTINS, true)).await.body);
        assert_eq!(first.calls.len(), 1, "{first:?}");
        assert_eq!(first.calls[0].1, "write_file");
        let args: Value = serde_json::from_str(&first.calls[0].2).unwrap();
        assert_eq!(args["path"], "src/api/p2_api.rs");
        assert!(args["content"].as_str().unwrap().contains("pub struct P2Api;"));

        let mut history = vec![prompt];
        history.extend(answered(&first.calls[0], "wrote src/api/p2_api.rs (9 lines)"));
        let check = parse_sse(&post(port, &request(system, history.clone(), BUILTINS, true)).await.body);
        assert_eq!(check.calls.len(), 1, "{check:?}");
        assert_eq!(check.calls[0].1, "bash");
        assert_eq!(serde_json::from_str::<Value>(&check.calls[0].2).unwrap()["command"], "grep -c 'pub fn' src/api/p2_api.rs");
        history.extend(answered(&check.calls[0], "2"));
        let report = parse_sse(&post(port, &request(system, history, BUILTINS, true)).await.body);
        assert!(report.calls.is_empty());
        assert!(report.text.contains("\n\nSTATUS: done\nSUMMARY: "), "{}", report.text);
    }

    /// In an Akasha group the worker notes its one fact first, then works and reports as before —
    /// and skips the note when the notes in its brief already hold it; a pushed note is
    /// acknowledged without redoing the task.
    #[tokio::test]
    async fn a_worker_in_a_group_notes_once_then_writes() {
        let port = server().await;
        let system = "mantra-role: worker\n## Your task: p1-models — Domain models\nScope: src/models/**\n";
        let offered: Vec<&str> = BUILTINS.iter().copied().chain(["mantra_akasha_note"]).collect();
        let prompt = user("Add the models.");
        let first = parse_sse(&post(port, &request(system, vec![prompt.clone()], &offered, true)).await.body);
        assert_eq!(first.calls.len(), 1, "{first:?}");
        assert_eq!(first.calls[0].1, "mantra_akasha_note");
        assert_eq!(serde_json::from_str::<Value>(&first.calls[0].2).unwrap()["text"], crate::mock::AKASHA_NOTE);
        let mut history = vec![prompt];
        history.extend(answered(&first.calls[0], "noted #1 (154/16k)"));
        let second = parse_sse(&post(port, &request(system, history.clone(), &offered, true)).await.body);
        assert_eq!(second.calls.first().map(|c| c.1.as_str()), Some("write_file"), "{second:?}");
        history.extend(answered(&second.calls[0], "wrote src/models/p1_models.rs (9 lines)"));
        let check = parse_sse(&post(port, &request(system, history.clone(), &offered, true)).await.body);
        assert_eq!(check.calls.first().map(|c| c.1.as_str()), Some("bash"), "{check:?}");
        history.extend(answered(&check.calls[0], "2"));
        let report = parse_sse(&post(port, &request(system, history, &offered, true)).await.body);
        assert!(report.calls.is_empty() && report.text.contains("STATUS: done"), "{report:?}");
        let briefed = user(&format!("Add the models.\n\n## Shared notes (akasha · builders)\n## akasha · builders · 1 note · 154/16k\n[1 · p1-config · 14:02] {}", crate::mock::AKASHA_NOTE));
        let skip = parse_sse(&post(port, &request(system, vec![briefed], &offered, true)).await.body);
        assert_eq!(skip.calls.first().map(|c| c.1.as_str()), Some("write_file"), "{skip:?}");
        // a pushed note is a turn of its own: acknowledged, the task not redone
        let pushed = parse_sse(&post(port, &request(system, vec![user("[akasha · w3] the API returns 201 on create now")], &offered, true)).await.body);
        assert!(pushed.calls.is_empty() && pushed.text.contains("STATUS: done"), "{pushed:?}");
        // the other members leave the note to the models worker: the group holds it once
        let config = "mantra-role: worker\n## Your task: p1-config — Config loader\nScope: src/config/**\n";
        let other = parse_sse(&post(port, &request(config, vec![user("Add the config loader.")], &offered, true)).await.body);
        assert_eq!(other.calls.first().map(|c| c.1.as_str()), Some("write_file"), "{other:?}");
    }

    /// `--demo` with the stand-in MCP server and vision: the worker reads the scene through bija's
    /// MCP client, takes one screenshot through Mantra's tool, then works as before and says what
    /// it saw. The screenshot's image arrives as bija sends it — a `user` message after the
    /// result — and must not read as a new prompt (it would restart the script).
    #[tokio::test]
    async fn a_worker_with_a_scene_server_and_vision_looks_then_works() {
        let port = start_with(Mock { screenshots: true, ..Mock::new(1000.0, 0) }).await.unwrap();
        let system = "mantra-role: worker\n## Your task: p2-api — HTTP API\nScope: src/api/**\n";
        let offered: Vec<&str> = BUILTINS.iter().copied().chain([DEMO_SCENE, "mantra_screenshot"]).collect();
        let prompt = user("Implement the REST handlers.");
        let scene = parse_sse(&post(port, &request(system, vec![prompt.clone()], &offered, true)).await.body);
        assert_eq!(scene.calls.iter().map(|c| c.1.as_str()).collect::<Vec<_>>(), vec![DEMO_SCENE], "{scene:?}");
        assert_eq!(serde_json::from_str::<Value>(&scene.calls[0].2).unwrap(), json!({}));
        let mut history = vec![prompt];
        history.extend(answered(&scene.calls[0], "Scene \"demo\": 3 objects\n- Cube (mesh) at (0, 0, 1)"));
        let shot = parse_sse(&post(port, &request(system, history.clone(), &offered, true)).await.body);
        assert_eq!(shot.calls.iter().map(|c| c.1.as_str()).collect::<Vec<_>>(), vec!["mantra_screenshot"], "{shot:?}");
        history.extend(answered(&shot.calls[0], "/runs/1-x/screenshots/p2-api-1.png (1×1)\n[image: /s/images/call-1-0.png]"));
        history.push(json!({"role": "user", "content": [{"type": "text", "text": "image from mantra_screenshot: /s/images/call-1-0.png"}, {"type": "image_url", "image_url": {"url": "data:image/png;base64,iVBORw0KGgo="}}]}));
        let write = parse_sse(&post(port, &request(system, history.clone(), &offered, true)).await.body);
        assert_eq!(write.calls.first().map(|c| c.1.as_str()), Some("write_file"), "the image is not a new prompt: {write:?}");
        history.extend(answered(&write.calls[0], "wrote src/api/p2_api.rs (9 lines)"));
        let check = parse_sse(&post(port, &request(system, history.clone(), &offered, true)).await.body);
        assert_eq!(check.calls.first().map(|c| c.1.as_str()), Some("bash"), "{check:?}");
        history.extend(answered(&check.calls[0], "2"));
        let report = parse_sse(&post(port, &request(system, history, &offered, true)).await.body);
        assert!(report.calls.is_empty(), "{report:?}");
        assert!(report.text.contains("the scene through `demo` (Scene \"demo\": 3 objects)") && report.text.contains("the screen (p2-api-1.png)"), "{}", report.text);
        assert!(report.text.contains("\n\nSTATUS: done\nSUMMARY: "), "{}", report.text);
    }

    /// Without `$MANTRA_SCREENSHOT_CMD` (the server's default) an offered `mantra_screenshot` is
    /// left alone — a demo never captures a real screen — and the scene alone is looked at.
    #[tokio::test]
    async fn a_worker_takes_no_screenshot_unless_the_capture_is_set_up() {
        let port = server().await;
        let system = "mantra-role: worker\n## Your task: p1-models — Domain models\nScope: src/models/**\n";
        let offered: Vec<&str> = BUILTINS.iter().copied().chain([DEMO_SCENE, "mantra_screenshot"]).collect();
        let prompt = user("Add the models.");
        let scene = parse_sse(&post(port, &request(system, vec![prompt.clone()], &offered, true)).await.body);
        assert_eq!(scene.calls.first().map(|c| c.1.as_str()), Some(DEMO_SCENE), "{scene:?}");
        let mut history = vec![prompt];
        history.extend(answered(&scene.calls[0], "Scene \"demo\": 3 objects"));
        let next = parse_sse(&post(port, &request(system, history, &offered, true)).await.body);
        assert_eq!(next.calls.first().map(|c| c.1.as_str()), Some("write_file"), "{next:?}");
    }

    #[test]
    fn an_image_message_is_part_of_the_step_before_it() {
        let carrier = json!({"role": "user", "content": [{"type": "text", "text": "image from view: /a.png"}, {"type": "image_url", "image_url": {"url": "data:image/png;base64,AA=="}}]});
        assert!(carries_images(&carrier));
        assert!(!carries_images(&json!({"role": "user", "content": [{"type": "text", "text": "look at this"}, {"type": "image_url", "image_url": {"url": "data:,"}}]})), "a person's own image is a prompt");
        assert!(!carries_images(&json!({"role": "user", "content": [{"type": "text", "text": "image from x: /a.png"}]})), "no image, no carrier");
        assert!(!carries_images(&json!({"role": "user", "content": "image from x: /a.png"})));
        let req = json!({"messages": [
            {"role": "system", "content": "mantra-role: worker"},
            {"role": "user", "content": "do it"},
            {"role": "assistant", "content": null, "tool_calls": [{"id": "c1", "type": "function", "function": {"name": "view", "arguments": "{}"}}]},
            {"role": "tool", "tool_call_id": "c1", "content": "a picture\n[image: /a.png]"},
            carrier
        ]});
        let t = Turn::read(&req).unwrap();
        assert_eq!((t.prompt.as_str(), t.step, t.last_result.as_str()), ("do it", 1, "a picture\n[image: /a.png]"));
        assert_eq!(t.results, vec![("view".to_string(), "a picture\n[image: /a.png]".to_string())]);
        assert_eq!(t.result_of("view").as_deref(), Some("a picture"));
    }

    #[tokio::test]
    async fn a_steered_worker_acknowledges_and_reports() {
        let port = server().await;
        let system = "mantra-role: worker\n## Your task: p1-models — Domain models\nScope: src/models/**\n";
        let r = parse_sse(&post(port, &request(system, vec![user("[from the orchestrator] rename it")], BUILTINS, true)).await.body);
        assert!(r.calls.is_empty() && r.text.contains("STATUS: done"), "{r:?}");
    }

    #[tokio::test]
    async fn the_orchestrator_spawns_the_whole_phase_in_one_message_then_waits() {
        let port = server().await;
        let system = "mantra-role: orchestrator\n";
        let offered = ["mantra_spawn", "mantra_wait"];
        let brief = user("[mantra:phase] Phase 1/3: Foundations\nCurrent phase:\n```json\n{\"id\":\"p1\",\"tasks\":[{\"id\":\"p1-models\"},{\"id\":\"p1-config\"}]}\n```\nSpawn the tasks.");
        let spawns = parse_sse(&post(port, &request(system, vec![brief.clone()], &offered, true)).await.body);
        let names: Vec<(&str, Value)> = spawns.calls.iter().map(|c| (c.1.as_str(), serde_json::from_str(&c.2).unwrap())).collect();
        assert_eq!(names, vec![("mantra_spawn", json!({"task_id": "p1-models"})), ("mantra_spawn", json!({"task_id": "p1-config"}))]);
        assert_ne!(spawns.calls[0].0, spawns.calls[1].0);

        let mut history = vec![brief, json!({"role": "assistant", "content": null, "tool_calls": spawns.calls.iter().map(|c| json!({"id": c.0, "type": "function", "function": {"name": c.1, "arguments": c.2}})).collect::<Vec<_>>()})];
        for c in &spawns.calls {
            history.push(json!({"role": "tool", "tool_call_id": c.0, "content": "spawned"}));
        }
        let wait = parse_sse(&post(port, &request(system, history.clone(), &offered, true)).await.body);
        assert_eq!(wait.calls.iter().map(|c| c.1.as_str()).collect::<Vec<_>>(), vec!["mantra_wait"]);
        history.extend(answered(&wait.calls[0], "OK — end your turn now."));
        let done = parse_sse(&post(port, &request(system, history, &offered, true)).await.body);
        assert!(done.calls.is_empty() && done.text.contains("p1-models, p1-config"), "{done:?}");
    }

    #[tokio::test]
    async fn the_gate_reports_a_pass() {
        let port = server().await;
        let r = parse_sse(&post(port, &request("mantra-role: gate\n", vec![user("[mantra:gate] Phase 1 — QA round 1/3")], &["mantra_gate_report"], true)).await.body);
        assert_eq!(r.calls[0].1, "mantra_gate_report");
        assert_eq!(serde_json::from_str::<Value>(&r.calls[0].2).unwrap()["pass"], true);
    }

    #[tokio::test]
    async fn solo_answers_a_probe_with_ok_and_anything_else_with_a_look_around() {
        let port = server().await;
        let probe = parse_sse(&post(port, &request("", vec![user("Reply with exactly: OK")], BUILTINS, true)).await.body);
        assert_eq!(probe.text, "OK");
        assert!(probe.calls.is_empty() && probe.finish == "stop");
        let look = parse_sse(&post(port, &request("", vec![user("add a --version flag")], BUILTINS, true)).await.body);
        assert_eq!(look.calls[0].1, "list_dir");
    }

    #[tokio::test]
    async fn a_request_without_tools_gets_text_never_a_call() {
        // bija's compaction call: the whole history, no tools, a request for a summary.
        let port = server().await;
        let r = parse_sse(&post(port, &request(PLANNER, vec![user("[mantra:plan] build it"), user("Write a hand-off summary for your future self.")], &[], true)).await.body);
        assert!(r.calls.is_empty() && r.text.contains("summary"), "{r:?}");
    }

    #[tokio::test]
    async fn a_tool_the_request_does_not_offer_is_never_called() {
        let port = server().await;
        let r = parse_sse(&post(port, &request(PLANNER, vec![user("[mantra:plan] build it")], BUILTINS, true)).await.body);
        assert!(r.calls.is_empty(), "{r:?}");
        assert!(r.text.contains("mantra_submit_plan"), "{}", r.text);
    }

    #[tokio::test]
    async fn without_stream_the_answer_is_one_chat_completion() {
        let port = server().await;
        let r = post(port, &request("mantra-role: worker\n## Your task: p1-models — Domain models\nScope: src/models/**\n", vec![user("do it")], BUILTINS, false)).await;
        assert_eq!(r.status, 200);
        assert!(r.headers.contains("content-type: application/json"), "{}", r.headers);
        let v: Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(v["object"], "chat.completion");
        assert_eq!(v["model"], "demo-model");
        let choice = &v["choices"][0];
        assert_eq!(choice["finish_reason"], "tool_calls");
        assert_eq!(choice["message"]["content"], Value::Null);
        let call = &choice["message"]["tool_calls"][0];
        assert_eq!(call["function"]["name"], "write_file");
        let args: Value = serde_json::from_str(call["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args["path"], "src/models/p1_models.rs");
        assert!(v["usage"]["prompt_tokens"].as_u64().unwrap() > 0);
        assert!(v["usage"]["prompt_tokens_details"]["cached_tokens"].is_u64());
    }

    #[tokio::test]
    async fn a_malformed_request_is_a_400_in_the_provider_shape() {
        let port = server().await;
        for body in ["{not json", "{\"model\":\"m\"}"] {
            let r = post(port, body).await;
            assert_eq!(r.status, 400, "{body}");
            let v: Value = serde_json::from_str(&r.body).unwrap();
            assert!(!v["error"]["message"].as_str().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn flaky_fails_every_nth_request_with_a_retryable_503() {
        let port = start_with(Mock::new(1000.0, 2)).await.unwrap();
        let body = request("", vec![user("Reply with exactly: OK")], BUILTINS, true);
        assert_eq!(post(port, &body).await.status, 200);
        let r = post(port, &body).await;
        assert_eq!(r.status, 503);
        assert!(r.headers.contains("retry-after: 1"), "{}", r.headers);
        assert_eq!(post(port, &body).await.status, 200);
        assert_eq!(Mock::new(1.0, 1).flaky, 0, "1 would fail every request forever");
    }

    /// Discovery on the demo's `bija` provider: every model the demo adds, with its window, read
    /// by the same parser a real provider's catalogue goes through.
    #[tokio::test]
    async fn the_model_listing_names_the_demo_models_with_their_windows() {
        let port = server().await;
        let r = get(port, "/v1/models").await;
        assert_eq!(r.status, 200);
        assert!(r.headers.contains("content-type: application/json"), "{}", r.headers);
        let found = crate::discover::parse_models(&r.body).expect("a model list");
        let demo = crate::config::bija_default_model_entries("bija");
        assert_eq!(found.len(), demo.len(), "{found:?}");
        for m in &demo {
            let f = found.iter().find(|f| f.id == m.model).unwrap_or_else(|| panic!("{} is not listed: {found:?}", m.model));
            assert_eq!(f.context, m.context_window, "{}", m.model);
        }
    }

    #[test]
    fn pieces_cut_on_character_boundaries() {
        let s = "ab↔cd↔ef";
        let p = pieces(s, 3);
        assert_eq!(p, vec!["ab↔", "cd↔", "ef"]);
        assert_eq!(p.concat(), s);
        assert!(pieces("", 64).is_empty());
        assert_eq!(pieces("abc", 0), vec!["a", "b", "c"], "a zero width still makes progress");
    }

    #[test]
    fn the_turn_is_read_from_the_last_prompt_on() {
        let req = json!({"messages": [
            {"role": "system", "content": [{"type": "text", "text": "x\n  mantra-role: gate  \n"}]},
            {"role": "user", "content": "first"},
            {"role": "assistant", "content": "done"},
            {"role": "user", "content": "second"},
            {"role": "assistant", "content": null, "tool_calls": []},
            {"role": "tool", "tool_call_id": "c", "content": "result"}
        ], "tools": [{"type": "function", "function": {"name": "bash"}}]});
        let t = Turn::read(&req).unwrap();
        assert_eq!((t.role.as_str(), t.prompt.as_str(), t.step, t.last_result.as_str()), ("gate", "second", 1, "result"));
        assert_eq!(t.offered, vec!["bash"]);
        assert!(t.has_history);
        let bare = Turn::read(&json!({"messages": []})).unwrap();
        assert_eq!((bare.role.as_str(), bare.step), ("solo", 0));
    }
}
