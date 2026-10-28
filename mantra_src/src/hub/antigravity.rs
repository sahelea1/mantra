//! Native `agy` backend. The documented NDJSON protocol has user inputs and
//! init/step_update/result outputs, but no control messages. Turns are serialized
//! here; model/effort changes relaunch with the exact conversation ID.
use super::{
    activity, read_ndjson, read_stderr_tail, stderr_tail_text, token_usage, wait_or_kill,
    write_line, AgentId, Cmd, Exit, HubEvent, LineIn, SpawnSpec, StderrTail, Totals,
};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc;

#[derive(Debug, Clone)]
pub struct AntigravitySpawn {
    pub mcp_sock: Option<PathBuf>,
    /// The CLI has no system-prompt flag. Instructions accompany the first real
    /// user input, without creating an extra model turn.
    pub system_prompt: String,
}

fn build_args(
    base: &[String],
    model: &str,
    effort: &str,
    conversation: Option<&str>,
    sandbox: &str,
    approval: &str,
) -> Vec<String> {
    let mut args = base.to_vec();
    args.extend(
        [
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
        ]
        .map(str::to_string),
    );
    if !model.is_empty() {
        args.extend(["--model".into(), model.into()]);
    }
    if !effort.is_empty() {
        args.extend(["--effort".into(), effort.into()]);
    }
    if let Some(id) = conversation {
        args.extend(["--conversation".into(), id.into()]);
    }
    if approval != crate::config::BYPASS_SANDBOX_DANGER
        && sandbox != "danger-full-access"
        && sandbox != "none"
        && !sandbox.is_empty()
    {
        args.push("--sandbox".into());
    }
    if approval == "never" || approval == crate::config::BYPASS_SANDBOX_DANGER {
        args.push("--dangerously-skip-permissions".into());
    }
    args
}

fn user_line(text: &str, instructions: &mut Option<String>) -> Value {
    let content = match instructions.take().filter(|s| !s.is_empty()) {
        Some(s) => format!("<mantra_instructions>\n{s}\n</mantra_instructions>\n\n{text}"),
        None => text.to_string(),
    };
    json!({"event":"user", "message":{"content":content}})
}

async fn stop_child(stdin: &mut ChildStdin, child: &mut Child) {
    let _ = stdin.shutdown().await;
    wait_or_kill(child, Duration::from_millis(1200)).await;
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn run_antigravity_process(
    id: AgentId,
    spec: &SpawnSpec,
    command: &[String],
    ev: &mpsc::UnboundedSender<HubEvent>,
    cmds: &mut mpsc::UnboundedReceiver<Cmd>,
    conversation: &mut Option<String>,
    effort: &mut String,
    model: &mut String,
    replay: &mut Vec<String>,
    last_turn: &mut Option<String>,
) -> Exit {
    run_with_home(
        id,
        spec,
        command,
        ev,
        cmds,
        conversation,
        effort,
        model,
        replay,
        last_turn,
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn run_with_home(
    id: AgentId,
    spec: &SpawnSpec,
    command: &[String],
    ev: &mpsc::UnboundedSender<HubEvent>,
    cmds: &mut mpsc::UnboundedReceiver<Cmd>,
    conversation: &mut Option<String>,
    effort: &mut String,
    model: &mut String,
    replay: &mut Vec<String>,
    last_turn: &mut Option<String>,
    registration_home: Option<PathBuf>,
) -> Exit {
    let Some(cs) = &spec.antigravity else {
        return Exit::LaunchFailed("Antigravity backend missing spawn configuration".into());
    };
    if spec.sandbox == "read-only" && spec.approval != crate::config::BYPASS_SANDBOX_DANGER {
        return Exit::LaunchFailed("Antigravity's --sandbox does not enforce a read-only workspace; select a writable sandbox or another backend".into());
    }
    if !spec.mcp.is_empty() {
        return Exit::LaunchFailed("Antigravity cannot isolate per-agent external MCP servers; configure them in agy's own MCP settings instead".into());
    }
    let Some((program, base)) = command.split_first() else {
        return Exit::LaunchFailed("empty Antigravity command".into());
    };
    let resumed = conversation.is_some();
    let args = build_args(
        base,
        model,
        effort,
        conversation.as_deref(),
        &spec.sandbox,
        &spec.approval,
    );
    let bridge = cs
        .mcp_sock
        .as_ref()
        .filter(|_| !spec.dynamic_tools.is_empty());
    if !spec.dynamic_tools.is_empty() && bridge.is_none() {
        return Exit::LaunchFailed(
            "Antigravity dynamic tools require Mantra's MCP bridge socket".into(),
        );
    }
    if bridge.is_some() {
        // Workspace entries can override global server names. Never let a role's
        // Mantra calls silently bind to an unrelated server with the same name.
        match read_json_object(&spec.cwd.join(".agents/mcp_config.json")) {
            Ok(config) if config.get("mcpServers").and_then(Value::as_object).is_some_and(|servers| servers.contains_key("mantra")) => return Exit::LaunchFailed("The workspace defines an MCP server named 'mantra'; rename it before enabling Antigravity's Mantra tools".into()),
            Ok(_) => {}
            Err(error) => return Exit::LaunchFailed(format!("Antigravity workspace MCP configuration: {error}")),
        }
        let registration = tokio::task::spawn_blocking(move || {
            let home = registration_home.or_else(dirs::home_dir).ok_or_else(|| {
                "cannot locate home directory for Antigravity MCP registration".to_string()
            })?;
            let executable = std::env::current_exe().map_err(|e| e.to_string())?;
            register_bridge(&home, &executable)
        })
        .await;
        match registration {
            Ok(Ok(true)) => {
                let _ = ev.send(HubEvent::Notif { agent: id, method: "warning".into(), params: json!({"message":"Registered Mantra's environment-routed MCP bridge in Antigravity's global MCP configuration and scoped tool permissions"}) });
            }
            Ok(Ok(false)) => {}
            Ok(Err(e)) => {
                return Exit::LaunchFailed(format!("Antigravity MCP registration failed: {e}"))
            }
            Err(e) => {
                return Exit::LaunchFailed(format!("Antigravity MCP registration failed: {e}"))
            }
        }
    }
    let mut process = Command::new(program);
    process
        .args(args)
        .current_dir(&spec.cwd)
        .envs(spec.envs.iter().cloned())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    process
        .env_remove("MANTRA_AGY_SOCK")
        .env_remove("MANTRA_AGY_AGENT");
    if let Some(sock) = bridge {
        process
            .env("MANTRA_AGY_SOCK", sock)
            .env("MANTRA_AGY_AGENT", id.to_string());
    }
    #[cfg(unix)]
    process.process_group(0);
    let mut child = match process.spawn() {
        Ok(c) => c,
        Err(e) => return Exit::LaunchFailed(format!("failed to start `{program}`: {e}")),
    };
    let _group = super::ProcessGroup::new(child.id());
    let (Some(mut stdin), Some(stdout), Some(stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        let _ = child.kill().await;
        return Exit::LaunchFailed("Antigravity missing stdio pipes".into());
    };
    let (lines_tx, mut lines) = mpsc::unbounded_channel();
    tokio::spawn(read_ndjson(stdout, lines_tx, format!("agy {id}"), |_| true));
    let tail = StderrTail::default();
    tokio::spawn(read_stderr_tail(stderr, tail.clone(), "agy"));
    let mut tr = Translator::new(id, model.clone());
    let mut queue: VecDeque<String> = replay.drain(..).collect();
    let mut inflight = false;
    let mut instructions = (!resumed).then(|| cs.system_prompt.clone());
    let mut restart = false;
    let mut submitted_before_ready = false;
    let mut interrupt_deadline: Option<tokio::time::Instant> = None;
    let mut bridge_write = None;
    let mut bridge_rx = None;
    let request_scope = crate::util::uuid_v4();
    let mut bridge_generation = 0u64;
    let mut pending_calls = HashMap::<String, String>::new();
    let mut pending_bridge_events = Vec::new();
    loop {
        // Never submit the next prompt until a result closes the current turn.
        if !inflight && !restart {
            if let Some(text) = queue.pop_front() {
                submitted_before_ready |= !tr.ready;
                *last_turn = Some(text.clone());
                if write_line(&mut stdin, &user_line(&text, &mut instructions))
                    .await
                    .is_err()
                {
                    let _ = ev.send(super::stdin_lost(id, "Antigravity", text));
                    replay.extend(queue.drain(..));
                    stop_child(&mut stdin, &mut child).await;
                    return Exit::Crashed("Antigravity stdin closed".into());
                }
                inflight = true;
                if tr.ready {
                    for event in tr.start_turn() {
                        let _ = ev.send(event);
                    }
                }
            }
        }
        if restart && !inflight && cmds.is_empty() {
            replay.extend(queue.drain(..));
            stop_child(&mut stdin, &mut child).await;
            return Exit::Restarting {
                reason: "restarting Antigravity to apply model/effort".into(),
                fresh: false,
            };
        }
        tokio::select! {
            _ = async { match interrupt_deadline { Some(t) => tokio::time::sleep_until(t).await, None => std::future::pending().await } } => {
                // SIGINT may terminate the CLI or merely cancel its current turn. Bound
                // implementations that do neither, then resume the same conversation.
                for event in tr.finish_interrupted() { let _ = ev.send(event); }
                replay.extend(queue.drain(..));
                stop_child(&mut stdin, &mut child).await;
                return Exit::Restarting { reason: "Antigravity interrupted".into(), fresh: false };
            }
            cmd = cmds.recv() => match cmd.unwrap_or(Cmd::Shutdown) {
                Cmd::Turn { text } | Cmd::Steer { text } => queue.push_back(text),
                Cmd::Shutdown => {
                    if inflight { signal_interrupt(&child); }
                    stop_child(&mut stdin, &mut child).await;
                    return Exit::Shutdown;
                }
                Cmd::Interrupt => if inflight && interrupt_deadline.is_none() {
                    tr.interrupted = true;
                    signal_interrupt(&child);
                    interrupt_deadline = Some(tokio::time::Instant::now() + Duration::from_millis(1500));
                },
                Cmd::SetModel(next) => { *model = next; restart = true; }
                Cmd::SetEffort(next) => { *effort = next; restart = true; }
                Cmd::Restart => {
                    if inflight { for event in tr.finish_interrupted() { let _ = ev.send(event); } signal_interrupt(&child); }
                    replay.extend(queue.drain(..));
                    stop_child(&mut stdin, &mut child).await;
                    return Exit::Restarting { reason: "restart requested".into(), fresh: false };
                }
                Cmd::Reconfigure { spec, command, resume_text } => {
                    replay.extend(queue.drain(..));
                    if inflight { signal_interrupt(&child); }
                    stop_child(&mut stdin, &mut child).await;
                    return Exit::Reconfigure { spec, command, resume_text };
                }
                Cmd::Compact => { let _ = ev.send(HubEvent::CmdFailed { agent: id, what: "compact", error: "Antigravity has no documented headless compaction control".into(), text: None }); }
                Cmd::Shell { command } => { let _ = ev.send(HubEvent::CmdFailed { agent: id, what: "shell", error: "Antigravity does not support raw shell commands through its stream protocol".into(), text: Some(command) }); }
                Cmd::SetApproval(_) => { let _ = ev.send(super::unsupported_approval(id, "Antigravity")); }
                Cmd::Archive | Cmd::SetName(_) | Cmd::SetVision(_) => {}
                Cmd::Bridge(stream) => {
                    bridge_generation += 1;
                    pending_calls.clear();
                    pending_bridge_events.clear();
                    let (rd, wr) = stream.into_split();
                    let (tx, rx) = mpsc::unbounded_channel();
                    tokio::spawn(read_ndjson(rd, tx, format!("agy MCP {id}"), |_| true));
                    bridge_write = Some(wr);
                    bridge_rx = Some(rx);
                }
                Cmd::Respond { id: rid, result } => {
                    if let Some(call) = rid.as_str().and_then(|s| pending_calls.remove(s)) {
                        let success = result.get("success").and_then(Value::as_bool).unwrap_or(true);
                        let text = result.pointer("/contentItems/0/text").and_then(Value::as_str).unwrap_or("");
                        let items = result.get("contentItems").filter(|_| !super::answer_images(&result).is_empty());
                        super::claude::reply_bridge_call(&mut bridge_write, &call, success, text, items, id).await;
                    }
                }
                Cmd::RespondErr { id: rid, message } => {
                    if let Some(call) = rid.as_str().and_then(|s| pending_calls.remove(s)) {
                        super::claude::reply_bridge_call(&mut bridge_write, &call, false, &message, None, id).await;
                    }
                }
            },
            line = super::claude::recv_bridge(&mut bridge_rx), if bridge_rx.is_some() => match line {
                Some(LineIn::Line(v)) => {
                    if v.get("list").and_then(Value::as_bool) == Some(true) {
                        if let Some(w) = bridge_write.as_mut() { let _ = write_line(w, &super::claude::mcp_tool_list(&spec.dynamic_tools)).await; }
                    } else if let Some(call) = v.get("call").and_then(Value::as_str) {
                        let request = format!("agy-call-{request_scope}-{bridge_generation}-{call}");
                        pending_calls.insert(request.clone(), call.into());
                        let event = HubEvent::Request { agent: id, id: json!(request), method: "item/tool/call".into(), params: json!({"tool":v.get("tool"),"arguments":v.get("args").cloned().unwrap_or_else(|| json!({}))}) };
                        if tr.ready { let _ = ev.send(event); } else { pending_bridge_events.push(event); }
                    }
                }
                Some(LineIn::Closed) | None => { bridge_write = None; bridge_rx = None; pending_calls.clear(); pending_bridge_events.clear(); }
            },
            line = lines.recv() => match line {
                Some(LineIn::Line(v)) => {
                    let is_result = v.get("event").and_then(Value::as_str) == Some("result");
                    if is_result && !tr.ready {
                        replay.extend(queue.drain(..));
                        let reason = v.pointer("/result/error").and_then(Value::as_str).unwrap_or("Antigravity returned a result before conversation initialization").to_string();
                        stop_child(&mut stdin, &mut child).await;
                        return Exit::LaunchFailed(reason);
                    }
                    for mut event in tr.on_line(&v, resumed) {
                        if let HubEvent::Ready { thread_id, resumed, .. } = &mut event {
                            *conversation = Some(thread_id.clone());
                            if submitted_before_ready { *resumed = false; }
                        }
                        let _ = ev.send(event);
                    }
                    if !is_result && inflight && tr.ready { for event in tr.start_turn() { let _ = ev.send(event); } }
                    if tr.ready { for event in pending_bridge_events.drain(..) { let _ = ev.send(event); } }
                    if is_result { inflight = false; interrupt_deadline = None; }
                }
                Some(LineIn::Closed) | None => {
                    replay.extend(queue.drain(..));
                    if tr.interrupted {
                        for event in tr.finish_interrupted() { let _ = ev.send(event); }
                        let _ = super::reap_exit_code(&mut child).await;
                        return Exit::Restarting { reason: "Antigravity interrupted".into(), fresh: false };
                    }
                    let code = super::reap_exit_code(&mut child).await;
                    let reason = format!("Antigravity exited (code {code:?}): {}", stderr_tail_text(&tail, " / "));
                    return if tr.ready { Exit::Crashed(reason) } else { Exit::LaunchFailed(reason) };
                }
            },
        }
    }
}

fn signal_interrupt(child: &Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id().filter(|p| *p > 1 && *p <= i32::MAX as u32) {
        // Its private process group includes shell tools, so cancellation does not
        // leave an attached command running after its parent exits.
        unsafe {
            libc::killpg(pid as i32, libc::SIGINT);
        }
    }
}

struct Translator {
    agent: AgentId,
    model: String,
    ready: bool,
    turn_open: bool,
    interrupted: bool,
    turn_number: u64,
    texts: HashMap<u64, String>,
    closed_steps: HashSet<u64>,
    had_text: bool,
    last_context: (u64, u64),
    item_scope: String,
}
impl Translator {
    fn new(agent: AgentId, model: String) -> Self {
        Self {
            agent,
            model,
            ready: false,
            turn_open: false,
            interrupted: false,
            turn_number: 0,
            texts: HashMap::new(),
            closed_steps: HashSet::new(),
            had_text: false,
            last_context: (0, 0),
            item_scope: crate::util::uuid_v4(),
        }
    }
    fn notif(&self, method: &str, params: Value) -> HubEvent {
        HubEvent::Notif {
            agent: self.agent,
            method: method.into(),
            params,
        }
    }
    fn start_turn(&mut self) -> Vec<HubEvent> {
        if self.turn_open {
            return vec![];
        }
        self.turn_open = true;
        self.turn_number += 1;
        self.had_text = false;
        self.texts.clear();
        self.closed_steps.clear();
        vec![self.notif("turn/started", json!({}))]
    }
    fn finish_interrupted(&mut self) -> Vec<HubEvent> {
        self.interrupted = false;
        if !std::mem::take(&mut self.turn_open) {
            return vec![];
        }
        vec![self.notif("turn/completed", json!({"turn":{"status":"interrupted"}}))]
    }
    fn on_line(&mut self, v: &Value, resumed: bool) -> Vec<HubEvent> {
        let mut out = vec![];
        match v.get("event").and_then(Value::as_str).unwrap_or("") {
            "init" if !self.ready => {
                let Some(cid) = v
                    .get("conversation_id")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                else {
                    return out;
                };
                self.ready = true;
                if let Some(m) = v.pointer("/init/model").and_then(Value::as_str) {
                    self.model = m.into();
                }
                out.push(HubEvent::Ready {
                    agent: self.agent,
                    thread_id: cid.into(),
                    model: self.model.clone(),
                    resumed,
                });
            }
            "step_update" => {
                let Some(step) = v.get("step_update") else {
                    return out;
                };
                out.extend(self.start_turn());
                out.push(activity(
                    self.agent,
                    step.get("tool_name").and_then(Value::as_str),
                ));
                let index = step.get("step_index").and_then(Value::as_u64).unwrap_or(0);
                let done = step.get("state").and_then(Value::as_str) == Some("DONE");
                if self.closed_steps.contains(&index) {
                    return out;
                }
                let item_id = format!("agy-{}-{}-{index}", self.item_scope, self.turn_number);
                match step.get("step_type").and_then(Value::as_str).unwrap_or("") {
                    "agent_response" => {
                        if !self.texts.contains_key(&index) {
                            out.push(self.notif(
                                "item/started",
                                json!({"item":{"id":item_id,"type":"agentMessage","text":""}}),
                            ));
                        }
                        let delta = step.get("text_delta").and_then(Value::as_str).unwrap_or("");
                        self.texts.entry(index).or_default().push_str(delta);
                        self.had_text |= !delta.is_empty();
                        if !delta.is_empty() {
                            out.push(self.notif(
                                "item/agentMessage/delta",
                                json!({"itemId":item_id,"delta":delta}),
                            ));
                        }
                        if done {
                            out.push(self.notif("item/completed", json!({"item":{"id":item_id,"type":"agentMessage","text":self.texts.get(&index)}})));
                        }
                    }
                    "tool" => {
                        let name = step
                            .get("tool_name")
                            .or_else(|| step.pointer("/tool_info/name"))
                            .and_then(Value::as_str)
                            .unwrap_or("tool");
                        let info = step.get("tool_info").cloned().unwrap_or_else(|| json!({}));
                        let mut item = tool_item(&item_id, name, &info);
                        if done {
                            item["status"] = json!(if info.get("error").is_some() {
                                "failed"
                            } else {
                                "completed"
                            });
                            let output = info.get("output").cloned().unwrap_or(Value::Null);
                            if name == "run_command" {
                                item["aggregatedOutput"] = output;
                            } else if item["type"] == "dynamicToolCall" {
                                item["contentItems"] = json!([{"type":"text","text":output.as_str().map(str::to_owned).unwrap_or_else(|| output.to_string())}]);
                            } else {
                                item["result"] = output;
                            }
                        }
                        out.push(self.notif(
                            if done {
                                "item/completed"
                            } else {
                                "item/started"
                            },
                            json!({"item":item}),
                        ));
                    }
                    _ => {}
                }
                if done {
                    self.closed_steps.insert(index);
                    if step.get("step_type").and_then(Value::as_str) == Some("agent_response") {
                        if let Some(usage) = step.get("usage") {
                            self.last_context =
                                (count(usage, "input_tokens"), count(usage, "output_tokens"));
                        }
                    }
                }
            }
            "result" => {
                let Some(result) = v.get("result") else {
                    return out;
                };
                out.extend(self.start_turn());
                // A terminal response is the same text already delivered by deltas.
                // Fall back only for CLI versions/runs that omit response steps.
                let text = result.get("response").and_then(Value::as_str).unwrap_or("");
                if !self.had_text && !text.is_empty() {
                    out.push(self.notif("item/completed", json!({"item":{"id":format!("agy-{}-{}-result",self.item_scope,self.turn_number),"type":"agentMessage","text":text}})));
                }
                if let Some(usage) = result.get("usage") {
                    // Result usage is cumulative over the session: assign, never add.
                    out.push(token_usage(
                        self.agent,
                        self.last_context.0,
                        self.last_context.1,
                        Totals {
                            tokens: count(usage, "total_tokens"),
                            cached: count(usage, "cache_read_tokens"),
                            window: None,
                        },
                    ));
                }
                let status = match result
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("ERROR")
                {
                    "SUCCESS" if !self.interrupted => "completed",
                    "CANCELED" | "INTERRUPTED" => "interrupted",
                    _ if self.interrupted => "interrupted",
                    _ => "failed",
                };
                let error = if status == "failed" {
                    json!({"message":result.get("error").and_then(Value::as_str).unwrap_or("Antigravity turn failed")})
                } else {
                    Value::Null
                };
                out.push(self.notif(
                    "turn/completed",
                    json!({"turn":{"status":status,"error":error}}),
                ));
                self.turn_open = false;
                self.interrupted = false;
            }
            _ => {}
        }
        out
    }
}
fn count(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(Value::as_u64).unwrap_or(0)
}
fn tool_item(id: &str, name: &str, info: &Value) -> Value {
    let arguments = info.get("parameters").cloned().unwrap_or_else(|| json!({}));
    if name == "run_command" {
        json!({"id":id,"type":"commandExecution","status":"inProgress","command":arguments.get("CommandLine").and_then(Value::as_str).unwrap_or("")})
    } else if matches!(
        name,
        "write_to_file" | "replace_file_content" | "multi_replace_file_content"
    ) {
        let path = arguments
            .get("TargetFile")
            .or_else(|| arguments.get("target_file"))
            .and_then(Value::as_str)
            .unwrap_or("");
        json!({"id":id,"type":"fileChange","status":"inProgress","changes":[{"path":path,"kind":{"type":"modified"},"diff":""}]})
    } else if let Some(tool) = name
        .strip_prefix("mcp__mantra__")
        .or_else(|| name.starts_with("mantra_").then_some(name))
    {
        json!({"id":id,"type":"dynamicToolCall","tool":tool,"arguments":arguments,"status":"inProgress"})
    } else {
        json!({"id":id,"type":"mcpToolCall","server":"antigravity","tool":name,"arguments":arguments,"status":"inProgress"})
    }
}

// agy has no process-scoped MCP flag. Register one generic environment-routed
// adapter; its shared config never contains an agent ID, socket, or role tools.
fn register_bridge(home: &std::path::Path, executable: &std::path::Path) -> Result<bool, String> {
    use std::os::{fd::AsRawFd, unix::fs::OpenOptionsExt};
    let gemini = home.join(".gemini");
    ensure_directory(&gemini)?;
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(gemini.join("mantra-mcp.lock"))
        .map_err(|e| format!("open MCP registration lock: {e}"))?;
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            break;
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::WouldBlock || std::time::Instant::now() >= deadline {
            return Err(format!("lock MCP registration: {error}"));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let config_dir = gemini.join("config");
    let settings_dir = gemini.join("antigravity-cli");
    ensure_directory(&config_dir)?;
    ensure_directory(&settings_dir)?;
    let config_path = config_dir.join("mcp_config.json");
    let settings_path = settings_dir.join("settings.json");
    let mut config = read_json_object(&config_path)?;
    let mut settings = read_json_object(&settings_path)?;
    let entry = json!({"command":executable,"args":["mcp-bridge","--from-env"]});
    let servers = config
        .entry("mcpServers")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| "mcpServers must be an object".to_string())?;
    let mut config_changed = false;
    match servers.get("mantra") {
        Some(existing) if existing == &entry => {}
        Some(existing) if managed_bridge_entry(existing) => { servers.insert("mantra".into(), entry); config_changed = true; }
        Some(_) => return Err("MCP server 'mantra' already exists with a different configuration; rename or remove that entry before enabling Mantra tools".into()),
        None => { servers.insert("mantra".into(), entry); config_changed = true; }
    }
    let permissions = settings
        .entry("permissions")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| "settings permissions must be an object".to_string())?;
    let allow = permissions
        .entry("allow")
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .ok_or_else(|| "settings permissions.allow must be an array".to_string())?;
    if allow.iter().any(|v| !v.is_string()) {
        return Err("settings permissions.allow must contain only strings".into());
    }
    let settings_changed = !allow.iter().any(|v| v.as_str() == Some("mcp(mantra/*)"));
    if settings_changed {
        allow.push(json!("mcp(mantra/*)"));
    }
    if config_changed {
        write_json_atomic(&config_path, &Value::Object(config))?;
    }
    if settings_changed {
        write_json_atomic(&settings_path, &Value::Object(settings))?;
    }
    Ok(config_changed || settings_changed)
}
// Older installs wrote only these two fields. Treat that exact signature and a Mantra
// executable basename as ours; extra env, args, or options are the user's configuration.
fn managed_bridge_entry(entry: &Value) -> bool {
    let Some(object) = entry.as_object() else { return false; };
    let Some(command) = object.get("command").and_then(Value::as_str) else { return false; };
    let path = std::path::Path::new(command);
    object.len() == 2
        && path.is_absolute()
        && path.file_name().is_some_and(|name| name == "mantra")
        && object.get("args") == Some(&json!(["mcp-bridge", "--from-env"]))
}

fn ensure_directory(path: &std::path::Path) -> Result<(), String> {
    use std::os::unix::fs::DirBuilderExt;
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => Ok(()),
        Ok(_) => Err(format!(
            "{} must be a real directory, not a symlink or file",
            path.display()
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => std::fs::DirBuilder::new()
            .mode(0o700)
            .create(path)
            .or_else(|e| {
                if e.kind() == std::io::ErrorKind::AlreadyExists
                    && std::fs::symlink_metadata(path)?.is_dir()
                {
                    Ok(())
                } else {
                    Err(e)
                }
            })
            .map_err(|e| format!("create {}: {e}", path.display())),
        Err(e) => Err(format!("inspect {}: {e}", path.display())),
    }
}
fn read_json_object(path: &std::path::Path) -> Result<serde_json::Map<String, Value>, String> {
    use std::{io::Read, os::unix::fs::OpenOptionsExt};
    let mut file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(serde_json::Map::new()),
        Err(e) => return Err(format!("read {}: {e}", path.display())),
    };
    if !file.metadata().map_err(|e| e.to_string())?.is_file() {
        return Err(format!("{} must be a regular JSON file", path.display()));
    }
    let mut text = String::new();
    file.read_to_string(&mut text)
        .map_err(|e| format!("read {}: {e}", path.display()))?;
    // Parse errors never echo credentials from the file.
    serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|v| v.as_object().cloned())
        .ok_or_else(|| {
            format!(
                "{} must contain a JSON object; existing contents were preserved",
                path.display()
            )
        })
}
fn write_json_atomic(path: &std::path::Path, value: &Value) -> Result<(), String> {
    use std::{io::Write, os::unix::fs::OpenOptionsExt};
    let tmp = path.with_extension(format!("mantra-{}.tmp", crate::util::uuid_v4()));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|e| e.to_string())?;
        serde_json::to_writer_pretty(&mut file, value).map_err(|e| e.to_string())?;
        file.write_all(b"\n")
            .and_then(|_| file.sync_all())
            .map_err(|e| e.to_string())?;
        if std::fs::symlink_metadata(path).is_ok_and(|m| !m.is_file()) {
            return Err(format!("{} is no longer a regular file", path.display()));
        }
        std::fs::rename(&tmp, path).map_err(|e| format!("save {}: {e}", path.display()))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("mantra-agy-test-{}", crate::util::uuid_v4()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn spec(&self, cid: &str) -> SpawnSpec {
            SpawnSpec {
                cwd: self.0.clone(),
                model: "fixture-model".into(),
                provider: "antigravity".into(),
                effort: "medium".into(),
                approval: "never".into(),
                sandbox: "workspace-write".into(),
                developer_instructions: String::new(),
                dynamic_tools: vec![],
                config: Default::default(),
                extra_args: vec![],
                resume_thread: None,
                thread_name: None,
                backend: super::super::Backend::Antigravity,
                envs: vec![
                    ("FAKE_CID".into(), cid.into()),
                    (
                        "FAKE_LOG".into(),
                        self.0
                            .join(format!("{cid}.log"))
                            .to_string_lossy()
                            .into_owned(),
                    ),
                ],
                claude: None,
                bija: None,
                antigravity: Some(AntigravitySpawn {
                    mcp_sock: None,
                    system_prompt: "Role instructions".into(),
                }),
                mcp: vec![],
            }
        }
        fn command(&self) -> Vec<String> {
            let script = self.0.join("fake_agy.py");
            std::fs::write(&script, FAKE_AGY).unwrap();
            vec!["python3".into(), script.to_string_lossy().into_owned()]
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    const FAKE_AGY: &str = r#"
import json, os, signal, socket, sys, time, select
args=sys.argv[1:]
assert '--continue' not in args and '-p' not in args
assert args[args.index('--input-format')+1]=='stream-json'
cid=args[args.index('--conversation')+1] if '--conversation' in args else os.environ['FAKE_CID']
with open(os.environ['FAKE_LOG'],'a') as f: f.write(json.dumps({'args':args,'cid':cid,'agent':os.environ.get('MANTRA_AGY_AGENT')})+'\n')
def emit(v): print(json.dumps(v),flush=True)
def init(): emit({'event':'init','conversation_id':cid,'init':{'model':args[args.index('--model')+1]}})
late=os.environ.get('FAKE_LATE_INIT')=='1'
if not late: init()
bridge=None
if 'MANTRA_AGY_SOCK' in os.environ:
    bridge=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM);bridge.connect(os.environ['MANTRA_AGY_SOCK']);bridge=bridge.makefile('rw')
    bridge.write(json.dumps({'hello':True,'agent':int(os.environ['MANTRA_AGY_AGENT'])})+'\n');bridge.flush()
signal.signal(signal.SIGINT,lambda *_:sys.exit(0))
turn=0
for line in sys.stdin:
    v=json.loads(line); assert v['event']=='user'
    text=v['message']['content'];turn+=1
    if late and turn==1:init()
    with open(os.environ['FAKE_LOG'],'a') as f:f.write(json.dumps({'text':text})+'\n')
    if text=='slow':time.sleep(10)
    if bridge:
        bridge.write(json.dumps({'list':True})+'\n');bridge.flush()
        tools=json.loads(bridge.readline())['tools'];assert len(tools)==1
        tool=tools[0]['name'];assert tool=='mantra_role_'+os.environ['MANTRA_AGY_AGENT']
        bridge.write(json.dumps({'call':str(turn),'tool':tool,'args':{'cid':cid}})+'\n');bridge.flush()
        result=json.loads(bridge.readline());assert result['text']==cid and result['call']==str(turn)
    emit({'event':'step_update','step_update':{'step_index':turn*10,'step_type':'agent_response','state':'ACTIVE','text_delta':'hello '}})
    # A second user line here is a protocol violation: the caller must queue it.
    time.sleep(.08)
    assert not select.select([sys.stdin],[],[],0)[0], 'overlapping user turns'
    emit({'event':'step_update','step_update':{'step_index':turn*10,'step_type':'agent_response','state':'DONE','text_delta':cid,'usage':{'input_tokens':100,'output_tokens':2}}})
    emit({'event':'result','result':{'conversation_id':cid,'status':'SUCCESS','response':'hello '+cid,'num_turns':turn,'usage':{'input_tokens':turn*100,'output_tokens':turn*2,'total_tokens':turn*102,'cache_read_tokens':turn*20}}})
"#;
    async fn next(events: &mut mpsc::UnboundedReceiver<HubEvent>) -> HubEvent {
        tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("event timeout")
            .expect("events closed")
    }
    fn notif<'a>(events: &'a [HubEvent], method: &str) -> Vec<&'a Value> {
        events
            .iter()
            .filter_map(|e| match e {
                HubEvent::Notif {
                    method: m, params, ..
                } if m == method => Some(params),
                _ => None,
            })
            .collect()
    }
    #[test]
    fn stream_deltas_usage_status_and_restart_item_ids() {
        let mut tr = Translator::new(7, "model".into());
        let ready = tr.on_line(
            &json!({"event":"init","conversation_id":"conversation","init":{}}),
            false,
        );
        assert!(matches!(ready[0], HubEvent::Ready { resumed: false, .. }));
        let mut events = vec![];
        for (state, text) in [("ACTIVE", "Hel"), ("DONE", "lo")] {
            events.extend(tr.on_line(&json!({"event":"step_update","step_update":{"step_index":3,"step_type":"agent_response","state":state,"text_delta":text,"usage":{"input_tokens":10302,"output_tokens":582,"cache_read_tokens":8113}}}),false));
        }
        events.extend(tr.on_line(&json!({"event":"result","result":{"status":"SUCCESS","response":"Hello","usage":{"input_tokens":10302,"output_tokens":582,"total_tokens":10884,"cache_read_tokens":8113}}}),false));
        assert_eq!(notif(&events, "turn/started").len(), 1);
        assert_eq!(notif(&events, "item/completed")[0]["item"]["text"], "Hello");
        assert_eq!(
            notif(&events, "item/agentMessage/delta")
                .iter()
                .map(|p| p["delta"].as_str().unwrap())
                .collect::<String>(),
            "Hello"
        );
        let usage = notif(&events, "thread/tokenUsage/updated")[0];
        assert_eq!(usage["tokenUsage"]["total"]["totalTokens"], 10884);
        assert_eq!(usage["tokenUsage"]["last"]["inputTokens"], 10302);
        let old_id = notif(&events, "item/completed")[0]["item"]["id"].clone();
        let mut restarted = Translator::new(7, "model".into());
        let events=restarted.on_line(&json!({"event":"result","result":{"status":"ERROR","response":"new output","error":"failed"}}),true);
        assert_ne!(old_id, notif(&events, "item/completed")[0]["item"]["id"]);
        assert_eq!(
            notif(&events, "turn/completed")[0]["turn"]["status"],
            "failed"
        );
    }
    #[test]
    fn documented_flags_pin_resume_without_shared_continue_or_fake_controls() {
        let args = build_args(
            &[],
            "gemini-custom",
            "high",
            Some("specific-conversation"),
            "workspace-write",
            "on-request",
        );
        assert!(args
            .windows(2)
            .any(|p| p == ["--conversation", "specific-conversation"]));
        assert!(args.contains(&"--sandbox".into()));
        assert!(!args.iter().any(|v| matches!(
            v.as_str(),
            "--continue" | "--mcp-config" | "--system-prompt" | "--autocompact" | "-p"
        )));
        assert!(!args.contains(&"--dangerously-skip-permissions".into()));
        let ordinary = build_args(&[], "model", "", None, "workspace-write", "never");
        assert!(ordinary.contains(&"--dangerously-skip-permissions".into()));
        assert!(ordinary.contains(&"--sandbox".into()));
        let bypass = build_args(
            &[],
            "model",
            "",
            None,
            "workspace-write",
            crate::config::BYPASS_SANDBOX_DANGER,
        );
        assert!(bypass.contains(&"--dangerously-skip-permissions".into()));
        assert!(!bypass.contains(&"--sandbox".into()));
    }
    #[test]
    fn registration_preserves_settings_and_converges_for_concurrent_agents() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = Fixture::new();
        std::fs::create_dir_all(fixture.0.join(".gemini/config")).unwrap();
        std::fs::create_dir_all(fixture.0.join(".gemini/antigravity-cli")).unwrap();
        let config = fixture.0.join(".gemini/config/mcp_config.json");
        let settings = fixture.0.join(".gemini/antigravity-cli/settings.json");
        std::fs::write(&config,r#"{"custom":123,"mcpServers":{"user":{"command":"user-mcp","env":{"TOKEN":"secret"}}}}"#).unwrap();
        std::fs::write(&settings,r#"{"model":"user-model","permissions":{"allow":["command(git)"],"deny":["mcp(other/*)"]}}"#).unwrap();
        std::thread::scope(|scope| {
            let tasks: Vec<_> = (0..6)
                .map(|_| {
                    scope.spawn(|| {
                        register_bridge(&fixture.0, Path::new("/usr/bin/mantra")).unwrap()
                    })
                })
                .collect();
            assert_eq!(
                tasks
                    .into_iter()
                    .map(|t| t.join().unwrap())
                    .filter(|changed| *changed)
                    .count(),
                1
            );
        });
        let c = read_json_object(&config).unwrap();
        let s = read_json_object(&settings).unwrap();
        assert_eq!(c["custom"], 123);
        assert_eq!(c["mcpServers"]["user"]["env"]["TOKEN"], "secret");
        assert_eq!(
            c["mcpServers"]["mantra"],
            json!({"command":"/usr/bin/mantra","args":["mcp-bridge","--from-env"]})
        );
        assert_eq!(
            s["permissions"]["allow"],
            json!(["command(git)", "mcp(mantra/*)"])
        );
        assert_eq!(s["permissions"]["deny"], json!(["mcp(other/*)"]));
        assert_eq!(
            std::fs::metadata(config).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(settings).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    #[test]
    fn registration_relocates_only_the_exact_managed_bridge() {
        let fixture = Fixture::new();
        register_bridge(&fixture.0, Path::new("/old/bin/mantra")).unwrap();
        let path = fixture.0.join(".gemini/config/mcp_config.json");
        let settings = fixture.0.join(".gemini/antigravity-cli/settings.json");
        let original_settings = std::fs::read(&settings).unwrap();
        assert!(register_bridge(&fixture.0, Path::new("/new/bin/mantra")).unwrap());
        assert_eq!(read_json_object(&path).unwrap()["mcpServers"]["mantra"]["command"], "/new/bin/mantra");
        assert_eq!(std::fs::read(&settings).unwrap(), original_settings);
        assert!(!register_bridge(&fixture.0, Path::new("/new/bin/mantra")).unwrap());

        for entry in [
            json!({"command":"/old/bin/mantra","args":["mcp-bridge","--from-env"],"env":{"CUSTOM":"yes"}}),
            json!({"command":"/old/bin/mantra","args":["mcp-bridge","--from-env","extra"]}),
            json!({"command":"/old/bin/other","args":["mcp-bridge","--from-env"]}),
            json!({"command":"mantra","args":["mcp-bridge","--from-env"]}),
        ] {
            let contents = json!({"mcpServers":{"mantra":entry}}).to_string();
            std::fs::write(&path, &contents).unwrap();
            assert!(register_bridge(&fixture.0, Path::new("/new/bin/mantra")).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), contents);
            assert_eq!(std::fs::read(&settings).unwrap(), original_settings);
        }
    }

    #[test]
    fn registration_refuses_collision_malformed_and_symlink_without_overwriting() {
        use std::os::unix::fs::symlink;
        for contents in [
            r#"{"mcpServers":{"mantra":{"command":"someone-else"}}}"#,
            "malformed secret",
        ] {
            let fixture = Fixture::new();
            std::fs::create_dir_all(fixture.0.join(".gemini/config")).unwrap();
            let path = fixture.0.join(".gemini/config/mcp_config.json");
            std::fs::write(&path, contents).unwrap();
            assert!(register_bridge(&fixture.0, Path::new("/mantra")).is_err());
            assert_eq!(std::fs::read_to_string(path).unwrap(), contents);
        }
        let fixture = Fixture::new();
        std::fs::create_dir_all(fixture.0.join(".gemini/config")).unwrap();
        let victim = fixture.0.join("victim.json");
        std::fs::write(&victim, "{}").unwrap();
        symlink(&victim, fixture.0.join(".gemini/config/mcp_config.json")).unwrap();
        assert!(register_bridge(&fixture.0, Path::new("/mantra")).is_err());
        assert_eq!(std::fs::read_to_string(victim).unwrap(), "{}");
    }
    #[tokio::test]
    async fn subprocess_queues_turns_ready_precedes_start_and_resumes_exact_conversation() {
        let fixture = Fixture::new();
        let mut spec = fixture.spec("own-conversation");
        spec.envs.push(("FAKE_LATE_INIT".into(), "1".into()));
        let command = fixture.command();
        let (tx, mut cmds) = mpsc::unbounded_channel();
        let (ev, mut events) = mpsc::unbounded_channel();
        let home = fixture.0.clone();
        let task = tokio::spawn(async move {
            let mut cid = Some("resume-this-id".into());
            let mut effort = "medium".into();
            let mut model = "fixture-model".into();
            let mut replay = vec!["first".into()];
            let mut last = None;
            let exit = run_with_home(
                9,
                &spec,
                &command,
                &ev,
                &mut cmds,
                &mut cid,
                &mut effort,
                &mut model,
                &mut replay,
                &mut last,
                Some(home),
            )
            .await;
            (exit, cid, replay)
        });
        tx.send(Cmd::Turn {
            text: "second".into(),
        })
        .unwrap();
        let mut ready = false;
        let mut completed = 0;
        while completed < 2 {
            match next(&mut events).await {
                HubEvent::Ready {
                    thread_id, resumed, ..
                } => {
                    assert_eq!(thread_id, "resume-this-id");
                    assert!(!resumed, "replay already supplies resume input");
                    ready = true;
                }
                HubEvent::Notif { method, .. } if method == "turn/started" => {
                    assert!(ready, "Ready must precede turn notifications")
                }
                HubEvent::Notif { method, params, .. } if method == "turn/completed" => {
                    assert_eq!(params["turn"]["status"], "completed");
                    completed += 1;
                }
                _ => {}
            }
        }
        tx.send(Cmd::Shutdown).unwrap();
        let (exit, cid, replay) = task.await.unwrap();
        assert!(matches!(exit, Exit::Shutdown));
        assert_eq!(cid.as_deref(), Some("resume-this-id"));
        assert!(replay.is_empty());
        let log = std::fs::read_to_string(fixture.0.join("own-conversation.log")).unwrap();
        let entries: Vec<Value> = log
            .lines()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect();
        assert_eq!(entries[1]["text"], "first");
        assert_eq!(entries[2]["text"], "second");
    }
    #[tokio::test]
    async fn subprocess_model_change_replays_queue_and_interrupt_is_bounded() {
        let fixture = Fixture::new();
        let spec = fixture.spec("switch");
        let command = fixture.command();
        let (tx, mut cmds) = mpsc::unbounded_channel();
        let (ev, mut events) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            let mut cid = None;
            let mut effort = "medium".into();
            let mut model = "fixture-model".into();
            let mut replay = vec![];
            let mut last = None;
            let exit = run_antigravity_process(
                8,
                &spec,
                &command,
                &ev,
                &mut cmds,
                &mut cid,
                &mut effort,
                &mut model,
                &mut replay,
                &mut last,
            )
            .await;
            (exit, cid, model, effort, replay)
        });
        while !matches!(next(&mut events).await, HubEvent::Ready { .. }) {}
        tx.send(Cmd::Turn {
            text: "first".into(),
        })
        .unwrap();
        while !matches!(next(&mut events).await,HubEvent::Notif {method,..} if method=="turn/started")
        {
        }
        tx.send(Cmd::SetModel("second-model".into())).unwrap();
        tx.send(Cmd::SetEffort("high".into())).unwrap();
        tx.send(Cmd::Turn {
            text: "held".into(),
        })
        .unwrap();
        let (exit, cid, model, effort, replay) = tokio::time::timeout(Duration::from_secs(4), task)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(exit, Exit::Restarting { fresh: false, .. }));
        assert_eq!(cid.as_deref(), Some("switch"));
        assert_eq!(model, "second-model");
        assert_eq!(effort, "high");
        assert_eq!(replay, vec!["held"]);

        let spec = fixture.spec("interrupt");
        let command = fixture.command();
        let (tx, mut cmds) = mpsc::unbounded_channel();
        let (ev, mut events) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            run_antigravity_process(
                8,
                &spec,
                &command,
                &ev,
                &mut cmds,
                &mut None,
                &mut "medium".into(),
                &mut "fixture-model".into(),
                &mut vec![],
                &mut None,
            )
            .await
        });
        while !matches!(next(&mut events).await, HubEvent::Ready { .. }) {}
        tx.send(Cmd::Turn {
            text: "slow".into(),
        })
        .unwrap();
        // Wait until the subprocess has consumed its prompt before sending SIGINT.
        for _ in 0..100 {
            if std::fs::read_to_string(fixture.0.join("interrupt.log"))
                .unwrap_or_default()
                .contains("slow")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        tx.send(Cmd::Interrupt).unwrap();
        let exit = tokio::time::timeout(Duration::from_secs(4), task)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(exit, Exit::Restarting { fresh: false, .. }));
    }

    #[tokio::test]
    async fn concurrent_subprocesses_route_native_role_tools_to_their_own_agents() {
        let fixture = Fixture::new();
        let socket = fixture.0.join("bridge.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let (ev, mut events) = mpsc::unbounded_channel();
        let mut senders = HashMap::new();
        let mut tasks = vec![];
        for agent in [41, 42] {
            let mut spec = fixture.spec(&format!("conversation-{agent}"));
            spec.dynamic_tools = vec![
                json!({"name":format!("mantra_role_{agent}"),"description":"this agent only","inputSchema":{"type":"object","properties":{"cid":{"type":"string"}},"required":["cid"]}}),
            ];
            spec.antigravity.as_mut().unwrap().mcp_sock = Some(socket.clone());
            let command = fixture.command();
            let home = fixture.0.clone();
            let ev = ev.clone();
            let (tx, mut cmds) = mpsc::unbounded_channel();
            senders.insert(agent, tx);
            tasks.push(tokio::spawn(async move {
                run_with_home(
                    agent,
                    &spec,
                    &command,
                    &ev,
                    &mut cmds,
                    &mut None,
                    &mut "medium".into(),
                    &mut "fixture-model".into(),
                    &mut vec![],
                    &mut None,
                    Some(home),
                )
                .await
            }));
        }
        let routes = senders.clone();
        let accept = tokio::spawn(async move {
            for _ in 0..2 {
                let (stream, _) = listener.accept().await.unwrap();
                let (agent, stream) = super::super::read_bridge_hello(stream).await.unwrap();
                routes
                    .get(&agent)
                    .unwrap()
                    .send(Cmd::Bridge(stream))
                    .unwrap();
            }
        });
        for tx in senders.values() {
            tx.send(Cmd::Turn {
                text: "use your role tool".into(),
            })
            .unwrap();
        }
        let mut calls = HashSet::new();
        let mut completed = HashSet::new();
        while completed.len() < 2 {
            match next(&mut events).await {
                HubEvent::Request {
                    agent,
                    id,
                    method,
                    params,
                } => {
                    assert_eq!(method, "item/tool/call");
                    assert_eq!(params["tool"], format!("mantra_role_{agent}"));
                    let cid = format!("conversation-{agent}");
                    assert_eq!(params["arguments"]["cid"], cid);
                    assert!(calls.insert(agent));
                    senders[&agent].send(Cmd::Respond {id,result:json!({"success":true,"contentItems":[{"type":"text","text":cid}]})}).unwrap();
                }
                HubEvent::Notif {
                    agent,
                    method,
                    params,
                } if method == "turn/completed" => {
                    assert_eq!(params["turn"]["status"], "completed");
                    completed.insert(agent);
                }
                _ => {}
            }
        }
        assert_eq!(calls, HashSet::from([41, 42]));
        for tx in senders.values() {
            tx.send(Cmd::Shutdown).unwrap();
        }
        for task in tasks {
            assert!(matches!(task.await.unwrap(), Exit::Shutdown));
        }
        accept.await.unwrap();
        let config =
            std::fs::read_to_string(fixture.0.join(".gemini/config/mcp_config.json")).unwrap();
        assert!(!config.contains("conversation-"));
        assert!(!config.contains("bridge.sock"));
        assert!(!fixture.0.join(".agents").exists());
    }
}
