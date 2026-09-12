//! The hub owns every agent's Codex process.
//!
//! One `codex app-server` process per agent gives crash isolation: when one dies, only that
//! agent restarts (with `thread/resume`), everything else keeps running.

use crate::rpc::{self, Conn, Incoming};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::process::Child;
use tokio::sync::mpsc;

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
    Shutdown,
}

#[derive(Debug)]
pub enum HubEvent {
    Ready { agent: AgentId, thread_id: String, model: String, resumed: bool },
    Notif { agent: AgentId, method: String, params: Value },
    Request { agent: AgentId, id: Value, method: String, params: Value },
    CmdFailed { agent: AgentId, what: &'static str, error: String, text: Option<String> },
    Crashed { agent: AgentId, reason: String, restarting: bool, attempt: u32 },
    Exited { agent: AgentId },
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
    last_spawn: Option<Instant>,
}

impl Hub {
    pub fn new(codex_cmd: Vec<String>, ev: mpsc::UnboundedSender<HubEvent>) -> Hub {
        Hub { ev, agents: HashMap::new(), next: 1, codex_cmd, last_spawn: None }
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
        let cmd = self.codex_cmd.clone();
        let now = Instant::now();
        let delay = match self.last_spawn {
            Some(prev) => SPAWN_STAGGER.saturating_sub(now.duration_since(prev)),
            None => Duration::ZERO,
        };
        self.last_spawn = Some(now);
        tokio::spawn(agent_task(id, spec, cmd, ev, rx, delay));
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
    }
}

const MAX_RESTARTS: u32 = 5;

async fn agent_task(
    id: AgentId,
    spec: SpawnSpec,
    codex_cmd: Vec<String>,
    ev: mpsc::UnboundedSender<HubEvent>,
    mut cmds: mpsc::UnboundedReceiver<Cmd>,
    initial_delay: Duration,
) {
    if initial_delay > Duration::ZERO {
        tokio::time::sleep(initial_delay).await;
    }
    let mut thread_id = spec.resume_thread.clone();
    let mut effort = spec.effort.clone();
    let mut model = spec.model.clone();
    let mut approval = spec.approval.clone();
    let mut restarts: u32 = 0;
    let mut window = Instant::now();

    'outer: loop {
        let started = run_process(id, &spec, &codex_cmd, &ev, &mut cmds, &mut thread_id, &mut effort, &mut model, &mut approval, restarts > 0).await;
        match started {
            Exit::Shutdown => {
                let _ = ev.send(HubEvent::Exited { agent: id });
                return;
            }
            Exit::Crashed(reason) => {
                if window.elapsed() > Duration::from_secs(600) {
                    window = Instant::now();
                    restarts = 0;
                }
                restarts += 1;
                let restarting = restarts <= MAX_RESTARTS;
                crate::mlog!("agent {id} crashed: {reason} (attempt {restarts}, restarting={restarting})");
                let _ = ev.send(HubEvent::Crashed { agent: id, reason, restarting, attempt: restarts });
                if restarting {
                    let backoff = Duration::from_millis(500 * (1u64 << restarts.min(6)));
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
                                Some(Cmd::Turn { text }) | Some(Cmd::Steer { text }) => {
                                    let _ = ev.send(HubEvent::CmdFailed { agent: id, what: "turn", error: "agent restarting".into(), text: Some(text) });
                                }
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
    Crashed(String),
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
    is_restart: bool,
) -> Exit {
    let (conn, mut inc, mut child) = match rpc::spawn(codex_cmd, &spec.extra_args, &spec.cwd) {
        Ok(x) => x,
        Err(e) => return Exit::Crashed(e.to_string()),
    };
    if let Err(e) = rpc::handshake(&conn).await {
        let _ = child.kill().await;
        return Exit::Crashed(format!("handshake failed: {e}"));
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
            return Exit::Crashed(if tail.trim().is_empty() {
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
                        return Exit::Crashed("restart requested".into());
                    }
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
    fn picks_the_last_non_noise_stderr_line() {
        assert_eq!(last_significant_stderr_line("boot ok\nWARNING: bubblewrap sandbox degraded\n"), "boot ok");
        assert_eq!(last_significant_stderr_line("panic: thread start failed"), "panic: thread start failed");
        // when every line is noise, fall back to the last one rather than showing nothing
        assert_eq!(last_significant_stderr_line("WARNING: a\nWARNING: bubblewrap: b\n"), "WARNING: bubblewrap: b");
        assert_eq!(last_significant_stderr_line(""), "");
        assert_eq!(last_significant_stderr_line("  \n  \n"), "");
    }
}
