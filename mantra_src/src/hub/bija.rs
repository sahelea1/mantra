//! Bija backend: one long-lived `bija` process per agent — Mantra's own harness (`mantra_src/bija/`)
//! serving its stdio protocol: NDJSON ops on stdin (`prompt`, `steer`, `interrupt`, `compact`,
//! `set`, `shell`, `tool_result`, `shutdown`), NDJSON events on stdout. Like `hub/claude.rs`, it
//! translates everything into the Codex-shaped `HubEvent::Notif` methods `Agent::apply`
//! (`agent.rs`) already understands, so the engine and the UI need no backend-specific branches.
//! The protocol is bija's own contract, documented in `bija/SPEC.md` (§4 the ops and events, §9.2
//! this side of it); a hand-written sample of every event shape lives in
//! `src/testdata/bija-events.jsonl` and drives the translator test at the bottom of this file,
//! beside tests of the process itself against a stand-in written in sh.
//!
//! Where it differs from Claude Code, it is because bija is Mantra's own. The role's `mantra_*`
//! tools need no MCP bridge: they go to bija in `--tools-file`, a call to one comes back as a
//! `tool_call` event that becomes the same `HubEvent::Request{method:"item/tool/call"}` the Codex
//! path produces, and the Conductor's `Cmd::Respond` goes back down as a `tool_result` op. A model
//! or effort change needs no relaunch: `set` switches the live session at its next API call. And a
//! process is asked to stop before anything kills it: `shutdown` lets bija close its transcript and
//! then end its running command and background jobs, given `SHUTDOWN_GRACE` on a restart and
//! `hub::QUIT_GRACE` when Mantra quits.
//!
//! Each session lives in `$MANTRA_HOME/bija/sessions/<id>/`: Mantra writes the role's system prompt
//! (`system.md`) and tool list (`tools.json`) there before every launch, bija keeps its transcript
//! beside them, and a relaunch after a crash is `--resume <id>` — bija repairs the transcript's
//! tail and carries on, so a crash loses little. A transcript that ended mid-turn comes back as
//! `ready{pending:true}`: bija finishes that turn by itself, and text for the agent until then
//! joins it as a steer rather than queueing a second turn. Text that arrives before `ready` waits
//! in `replay`, since a launch can still fail and take the pipe with it.

use super::{compaction, hold_text, read_ndjson, read_stderr_tail, stderr_tail_text, stdin_lost, token_usage, wait_or_kill, write_line, AgentId, Cmd, Exit, HubEvent, LineIn, SpawnSpec, StderrTail, Totals};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc;

/// Bija-only spawn configuration, resolved by `app::spawn_agent` from the model's `ProviderEntry`
/// and the role (kept out of `hub` proper so it stays decoupled from `config::Registry`, as
/// `ClaudeSpawn` is).
#[derive(Debug, Clone)]
pub struct BijaSpawn {
    /// The API root, e.g. `https://api.openai.com/v1` or `http://127.0.0.1:11434/v1`.
    pub base_url: String,
    /// `openai-chat` | `anthropic` (`ProviderEntry::bija_wire`).
    pub wire: String,
    /// The variable holding the key — its value rides in `SpawnSpec.envs`, never on argv. `None`
    /// for a provider without a key (a local server).
    pub api_key_env: Option<String>,
    /// The window bija manages against (a run's context cap already applied) and the percentage
    /// of it at which bija compacts on its own.
    pub context_window: u64,
    pub compact_pct: u8,
    /// The role's instructions, written to `system.md` (bija puts its own preamble first).
    pub system_prompt: String,
    /// `$MANTRA_HOME/bija`: sessions live in `<home>/sessions/<id>/`.
    pub home: PathBuf,
    /// bija's own sandbox vocabulary: `read-only` | `workspace-write` | `full` (`sandbox_mode`).
    pub sandbox: String,
    /// Writable roots beyond the cwd for `workspace-write`.
    pub writable_roots: Vec<PathBuf>,
    /// Network inside sandboxed commands.
    pub network: bool,
    /// The model takes images (`ModelEntry.vision`): `--vision` offers bija's own screenshot and
    /// view-image tools, as Mantra offers its own.
    pub vision: bool,
}

/// Mantra's sandbox names (Codex's) in bija's vocabulary.
pub fn sandbox_mode(mantra: &str) -> &'static str {
    match mantra {
        "read-only" => "read-only",
        "danger-full-access" => "full",
        _ => "workspace-write",
    }
}

/// How long `shutdown` gets before the process is killed when one agent stops while Mantra runs
/// (a restart, an agent shut down on its own). bija interrupts a running turn and closes its
/// transcript before anything else, then gives its background jobs and leftover command groups one
/// 1 s SIGTERM grace together (`bija/src/agent.rs` `shutdown`, `bija/src/tools/shell.rs`) — so well
/// over that, or the kill lands mid-cleanup and orphans them. Quitting Mantra waits
/// `hub::QUIT_GRACE` for every agent together.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// How much of a built-in's result its card keeps.
const BUILTIN_SHOWN: usize = 400;

/// What an item abandoned by a `retry` reads as instead of its partial text (`Translator`).
const RETRIED: &str = "⟲ (retried — partial output dropped)";

/// Why a command that needs a live session is refused before `ready`.
const STARTING: &str = "bija is still starting — try again in a moment";

/// Runs one `bija` process for the lifetime of a single spawn attempt, with `run_claude_process`'s
/// contract: `session_id` is the agent's `thread_id` slot (a relaunch passes `--resume` with it),
/// a process that exits before `ready` is `Exit::LaunchFailed`, one that dies after it
/// `Exit::Crashed`. A model, effort or vision change never relaunches — `set` applies it at bija's
/// next API call — but is remembered here so a later relaunch starts with it. Prompt and steer text
/// that arrives before `ready` waits in `replay` (a launch can still fail, and with it the pipe),
/// and goes out as a steer while bija continues a turn a crash cut short.
///
/// `turn_cut` outlives the attempt like `replay`: this process ended unexpectedly while a turn was
/// open. bija's own resume normally takes that turn over (`ready{pending:true}`); when the relaunch
/// says `resumed` but nothing pending (an older bija that closed the turn on a signal, a transcript
/// that lost its tail), the hub sends `last_turn` again as a new prompt — once — and reports the
/// `Ready` as no resume, since nothing is lost and the engine must not prompt a second time.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_bija_process(
    id: AgentId,
    spec: &SpawnSpec,
    bija_cmd: &[String],
    ev: &mpsc::UnboundedSender<HubEvent>,
    cmds: &mut mpsc::UnboundedReceiver<Cmd>,
    session_id: &mut Option<String>,
    effort: &mut String,
    model: &mut String,
    vision: &mut bool,
    replay: &mut Vec<String>,
    last_turn: &mut Option<String>,
    turn_cut: &mut bool,
) -> Exit {
    let Some(bs) = spec.bija.as_ref() else {
        return Exit::LaunchFailed("internal error: Bija backend spawned without BijaSpawn config".into());
    };
    // `--vision` as the model now in use says, not as it was at spawn (`Cmd::SetVision`).
    let bs = &BijaSpawn { vision: *vision, ..bs.clone() };
    let Some((prog, base_args)) = bija_cmd.split_first() else {
        return Exit::LaunchFailed("empty bija command".into());
    };
    let (sid, resume) = launch_identity(id, session_id.as_deref(), &bs.home);
    let files = match prepare_session(&bs.home, &sid, &bs.system_prompt, &spec.dynamic_tools, &spec.mcp) {
        Ok(f) => f,
        Err(e) => return Exit::LaunchFailed(format!("couldn't prepare bija session {sid} under {}: {e}", bs.home.display())),
    };
    let name = if resume { None } else { spec.thread_name.as_deref() };
    let args = build_args(&spec.cwd, bs, &files, model, effort, &sid, resume, name, base_args);

    let mut command = Command::new(prog);
    command.args(&args).current_dir(&spec.cwd).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    // `app::spawn_agent` resolved the key into here under `--api-key-env`'s name.
    for (k, v) in &spec.envs {
        command.env(k, v);
    }
    crate::mlog!("bija {id}: spawn {prog} {}", args.join(" "));
    #[cfg(unix)]
    command.process_group(0);
    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => return Exit::LaunchFailed(format!("failed to start `{}`: {e}", bija_cmd.join(" "))),
    };
    let _group = super::ProcessGroup::new(child.id());
    let (Some(mut stdin), Some(stdout), Some(stderr)) = (child.stdin.take(), child.stdout.take(), child.stderr.take()) else {
        let _ = child.kill().await;
        return Exit::LaunchFailed("bija: missing stdio pipes".into());
    };
    let (line_tx, mut line_rx) = mpsc::unbounded_channel::<LineIn>();
    tokio::spawn(read_ndjson(stdout, line_tx, format!("bija {id}"), is_chatter));
    let stderr_tail = StderrTail::default();
    let mut stderr_done = tokio::spawn(read_stderr_tail(stderr, stderr_tail.clone(), "bija"));

    let mut tr = Translator::new(id, &sid, bs.context_window);
    let mut ready = false;
    loop {
        tokio::select! {
            c = cmds.recv() => {
                // Until `ready` this process may still fail to come up, and text written to it
                // would die with it. Held, it goes out at `ready` — this launch's or the next
                // one's (`replay` outlives the attempt).
                let Some(c) = hold_text(!ready, replay, c.unwrap_or(Cmd::Shutdown)) else { continue };
                match &c {
                    Cmd::Shutdown => {
                        shut_down(&mut stdin, &mut child).await;
                        return Exit::Shutdown;
                    }
                    Cmd::Restart => {
                        // Ended like a shutdown, so its command and jobs die with it (a SIGKILL
                        // would orphan their process groups); `--resume` carries on from there.
                        shut_down(&mut stdin, &mut child).await;
                        return Exit::Restarting { reason: "restart requested".into(), fresh: false };
                    }
                    Cmd::Shell { command } if !ready => {
                        let _ = ev.send(HubEvent::CmdFailed { agent: id, what: "shell", error: STARTING.into(), text: Some(command.clone()) });
                        continue;
                    }
                    Cmd::Compact if !ready => {
                        let _ = ev.send(HubEvent::CmdFailed { agent: id, what: "compact", error: STARTING.into(), text: None });
                        continue;
                    }
                    Cmd::Shell { command } if tr.turn_open => {
                        // bija runs a shell command only between turns, like Codex's `!`.
                        let _ = ev.send(HubEvent::CmdFailed { agent: id, what: "shell", error: "a turn is running — shell commands run between turns".into(), text: Some(command.clone()) });
                        continue;
                    }
                    Cmd::Turn { text } => *last_turn = Some(text.clone()),
                    Cmd::SetEffort(e) => *effort = e.clone(),
                    Cmd::SetModel(m) => *model = m.clone(),
                    Cmd::SetVision(v) => *vision = *v,
                    Cmd::SetApproval(_) => {
                        let _ = ev.send(super::unsupported_approval(id, "Bija"));
                    }
                    Cmd::Compact if !tr.turn_open => tr.idle_compact = true,
                    _ => {}
                }
                let op = match &c {
                    // While bija finishes the turn a crash cut short, a new prompt would queue a
                    // second turn behind it: it joins that turn as a steer instead.
                    Cmd::Turn { text } if tr.continuing => json!({"op": "steer", "text": text}),
                    _ => match op_for(&c) {
                        Some(op) => op,
                        None => continue,
                    },
                };
                if write_line(&mut stdin, &op).await.is_err() {
                    // The process is going away; its exit is reported where stdout closes. Only
                    // text the user typed is worth handing back.
                    if let Cmd::Turn { text } | Cmd::Steer { text } = c {
                        let _ = ev.send(stdin_lost(id, "bija", text));
                    }
                    continue;
                }
                if let Some((call, ok, text)) = respond_parts(&c) {
                    for e in tr.on_respond(&call, ok, &text, &respond_images(&c)) {
                        let _ = ev.send(e);
                    }
                }
            }
            l = line_rx.recv() => {
                match l {
                    Some(LineIn::Line(v)) => {
                        for mut e in tr.on_event(&v) {
                            let is_ready = matches!(e, HubEvent::Ready { .. });
                            let mut again = None;
                            if let HubEvent::Ready { thread_id, resumed, .. } = &mut e {
                                *session_id = Some(thread_id.clone());
                                ready = true;
                                // `resumed` here already means "and nothing pending" (`on_ready`).
                                if std::mem::take(turn_cut) && *resumed {
                                    if let Some(text) = last_turn.clone() {
                                        crate::mlog!("bija {id}: resumed with nothing pending after a turn was cut off — sending its prompt again");
                                        *resumed = false;
                                        // Like a turn bija continues itself: text until its end
                                        // joins it as a steer.
                                        tr.continuing = true;
                                        again = Some(text);
                                    }
                                }
                            }
                            let _ = ev.send(e);
                            if let Some(text) = again {
                                if write_line(&mut stdin, &json!({"op": "prompt", "text": text})).await.is_err() {
                                    let _ = ev.send(stdin_lost(id, "bija", text));
                                }
                            }
                            if is_ready {
                                // Anything held (before this `ready`, or across a crash-backoff
                                // wait) goes out now that there is a process to take it — folded
                                // into the turn bija is continuing, if it is continuing one.
                                let op = if tr.continuing { "steer" } else { "prompt" };
                                for text in replay.drain(..) {
                                    *last_turn = Some(text.clone());
                                    if write_line(&mut stdin, &json!({"op": op, "text": text})).await.is_err() {
                                        let _ = ev.send(stdin_lost(id, "bija", text));
                                    }
                                }
                            }
                        }
                    }
                    Some(LineIn::Closed) | None => {
                        let code = super::reap_exit_code(&mut child).await;
                        let code_s = code.map(|c| c.to_string()).unwrap_or_else(|| "unknown".into());
                        // stdout can close before the stderr reader has seen the line that says
                        // why; the pipe ends with the process, so this wait is short.
                        let _ = tokio::time::timeout(Duration::from_millis(500), &mut stderr_done).await;
                        let tail = stderr_tail_text(&stderr_tail, "\n");
                        let last = super::last_significant_stderr_line(&tail);
                        let last = last.strip_prefix("bija: ").unwrap_or(&last);
                        let reason = if last.is_empty() { format!("bija exited (code {code_s})") } else { format!("bija exited (code {code_s}): {last}") };
                        if ready {
                            // Before ready no turn can have opened: a failed launch keeps what the
                            // last crash said.
                            *turn_cut = tr.turn_open;
                        }
                        return if ready { Exit::Crashed(reason) } else { Exit::LaunchFailed(reason) };
                    }
                }
            }
        }
    }
}

/// Which session this launch is for, and whether it resumes one: the agent's own session when
/// bija already wrote it to disk, else a new one. A session id that never reached disk (the first
/// process died before writing it) is started fresh under the same id; one that could escape the
/// sessions directory (a hand-edited run journal) is replaced by a fresh uuid.
fn launch_identity(agent: AgentId, wanted: Option<&str>, home: &Path) -> (String, bool) {
    match wanted {
        Some(sid) if valid_session_id(sid) => {
            let on_disk = session_dir(home, sid).join("session.json").is_file();
            if !on_disk {
                crate::mlog!("bija {agent}: session {sid} was never written; starting it fresh");
            }
            (sid.to_string(), on_disk)
        }
        Some(sid) => {
            crate::mlog!("bija {agent}: refusing session id {:?}; starting a new session", crate::util::trunc(sid, 60));
            (crate::util::uuid_v4(), false)
        }
        None => (crate::util::uuid_v4(), false),
    }
}

/// The ids bija itself accepts: letters, digits, `-` and `_`, never a leading dot.
fn valid_session_id(sid: &str) -> bool {
    !sid.is_empty() && !sid.starts_with('.') && sid.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn session_dir(home: &Path, sid: &str) -> PathBuf {
    home.join("sessions").join(sid)
}

/// The files Mantra hands bija for one session.
struct SessionFiles {
    system: PathBuf,
    tools: PathBuf,
    /// `mcp.json`, when the agent has MCP servers — or had some, and this launch has none.
    mcp: Option<PathBuf>,
}

/// Creates the session directory (0700: the system prompt is the role's whole brief) and writes
/// `system.md`, `tools.json` and, when the agent has MCP servers, `mcp.json` (the shape Claude
/// Code's `--mcp-config` takes) — again on every launch, so a resume picks up instructions, tools
/// or servers that changed since. A session that had servers and has none now gets an empty
/// `mcp.json` (and `--mcp-file`): bija would otherwise go on reading the path it stored.
fn prepare_session(home: &Path, sid: &str, system_prompt: &str, tools: &[Value], mcp: &[(String, crate::config::McpServer)]) -> std::io::Result<SessionFiles> {
    let dir = session_dir(home, sid);
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(&dir)?;
    let mcp_path = dir.join("mcp.json");
    let files = SessionFiles { system: dir.join("system.md"), tools: dir.join("tools.json"), mcp: (!mcp.is_empty() || mcp_path.exists()).then(|| mcp_path.clone()) };
    std::fs::write(&files.system, system_prompt)?;
    std::fs::write(&files.tools, Value::Array(tools.to_vec()).to_string())?;
    if files.mcp.is_some() {
        let servers: serde_json::Map<String, Value> = mcp.iter().map(|(name, s)| (name.clone(), json!({"command": s.command, "args": s.args, "env": s.env}))).collect();
        std::fs::write(&mcp_path, json!({"mcpServers": servers}).to_string())?;
    }
    Ok(files)
}

/// `[base…] --home H --session|--resume ID --cwd C --provider-url U --wire W [--api-key-env K]
/// --model M --effort E|none --context-window N --compact-at P --system-file S --tools-file T
/// [--mcp-file F] --sandbox MODE [--writable DIR]… [--network] [--vision] [--name NAME]` (SPEC
/// §9.2). A resume repeats every flag, so a model or effort changed since the last launch is
/// picked up and saved, and the MCP servers start again; only the name is left to the session
/// (`set` may have renamed it since).
#[allow(clippy::too_many_arguments)]
fn build_args(cwd: &Path, bs: &BijaSpawn, files: &SessionFiles, model: &str, effort: &str, session_id: &str, resume: bool, name: Option<&str>, base_args: &[String]) -> Vec<String> {
    let mut a: Vec<String> = base_args.to_vec();
    let mut flag = |k: &str, v: String| {
        a.push(k.to_string());
        a.push(v);
    };
    flag("--home", bs.home.to_string_lossy().into_owned());
    flag(if resume { "--resume" } else { "--session" }, session_id.to_string());
    flag("--cwd", cwd.to_string_lossy().into_owned());
    flag("--provider-url", bs.base_url.clone());
    flag("--wire", bs.wire.clone());
    if let Some(k) = &bs.api_key_env {
        flag("--api-key-env", k.clone());
    }
    flag("--model", model.to_string());
    // Always said, so a resume onto a model without effort control clears a stored one.
    flag("--effort", if effort.is_empty() { "none".into() } else { effort.to_string() });
    if bs.context_window > 0 {
        flag("--context-window", bs.context_window.to_string());
    }
    // bija refuses anything over 100 as a usage error; a hand-edited models.toml can say more.
    flag("--compact-at", bs.compact_pct.min(100).to_string());
    flag("--system-file", files.system.to_string_lossy().into_owned());
    flag("--tools-file", files.tools.to_string_lossy().into_owned());
    if let Some(mcp) = &files.mcp {
        flag("--mcp-file", mcp.to_string_lossy().into_owned());
    }
    flag("--sandbox", bs.sandbox.clone());
    for root in &bs.writable_roots {
        flag("--writable", root.to_string_lossy().into_owned());
    }
    if bs.network {
        a.push("--network".into());
    }
    if bs.vision {
        a.push("--vision".into());
    }
    if let Some(n) = name.filter(|n| !n.trim().is_empty()) {
        a.push("--name".into());
        a.push(n.to_string());
    }
    a
}

/// The op a `Cmd` becomes on bija's stdin (SPEC §9.2). `None` for the commands with no bija
/// equivalent: `Archive` (the session simply stays on disk), `SetApproval` (reported as unsupported
/// by the process loop; bija's native tools never ask),
/// `Restart` (a relaunch, not an op) and `Bridge` (Claude Code's MCP side channel).
fn op_for(c: &Cmd) -> Option<Value> {
    match c {
        Cmd::Turn { text } => Some(json!({"op": "prompt", "text": text})),
        Cmd::Steer { text } => Some(json!({"op": "steer", "text": text})),
        Cmd::Interrupt => Some(json!({"op": "interrupt"})),
        Cmd::Compact => Some(json!({"op": "compact"})),
        Cmd::SetEffort(e) => Some(json!({"op": "set", "effort": e})),
        Cmd::SetModel(m) => Some(json!({"op": "set", "model": m})),
        Cmd::SetVision(v) => Some(json!({"op": "set", "vision": v})),
        Cmd::SetName(n) => Some(json!({"op": "set", "name": n})),
        Cmd::Shell { command } => Some(json!({"op": "shell", "command": command})),
        Cmd::Respond { .. } | Cmd::RespondErr { .. } => respond_parts(c).map(|(id, ok, text)| {
            let mut op = json!({"op": "tool_result", "id": id, "ok": ok, "text": text});
            // A screenshot (or any image the engine answers with) goes as base64 beside the text;
            // bija saves it under the session and sends it to the model its wire's way.
            let images = respond_images(c);
            if !images.is_empty() {
                op["images"] = images.iter().map(|(mime, b64)| json!({"mime": mime, "b64": b64})).collect();
            }
            op
        }),
        Cmd::Shutdown => Some(json!({"op": "shutdown"})),
        Cmd::Archive | Cmd::SetApproval(_) | Cmd::Restart | Cmd::Bridge(_) => None,
    }
}

/// `(call id, ok, text)` of an answer to an external tool call. The engine answers in Codex's
/// shape (`{"contentItems":[{"type":"inputText","text":…}],"success":…}`); bija's calls carry
/// string ids, so any other id cannot be one of them.
fn respond_parts(c: &Cmd) -> Option<(String, bool, String)> {
    match c {
        Cmd::Respond { id, result } => {
            let ok = result.get("success").and_then(|s| s.as_bool()).unwrap_or(true);
            let text = result
                .get("contentItems")
                .and_then(|c| c.as_array())
                .map(|items| items.iter().filter_map(|i| i.get("text").and_then(|t| t.as_str())).collect::<Vec<_>>().join("\n"))
                .unwrap_or_default();
            id.as_str().map(|id| (id.to_string(), ok, text))
        }
        Cmd::RespondErr { id, message } => id.as_str().map(|id| (id.to_string(), false, message.clone())),
        _ => None,
    }
}

/// The images in an answer to an external tool call (`inputImage` items with a data URL).
fn respond_images(c: &Cmd) -> Vec<(String, String)> {
    match c {
        Cmd::Respond { result, .. } => super::answer_images(result),
        _ => vec![],
    }
}

/// Asks bija to finish — it interrupts a running turn first, so the transcript is consistent for
/// the next `--resume` — and kills it if it is not gone within `SHUTDOWN_GRACE`.
async fn shut_down(stdin: &mut ChildStdin, child: &mut Child) {
    let _ = write_line(stdin, &json!({"op": "shutdown"})).await;
    wait_or_kill(child, SHUTDOWN_GRACE).await;
}

/// The streaming events, one per token or output chunk: never worth a log line each
/// (`read_ndjson` logs every other line).
fn is_chatter(v: &Value) -> bool {
    matches!(v.get("ev").and_then(|e| e.as_str()), Some("text_delta" | "reasoning_delta" | "tool_output"))
}

/// What a tool call's `item/started` said, so its `item/completed` can say it again:
/// `Agent::apply_item` takes a tool's name, a command's text and a file's path from whichever
/// event it is applying, and would blank them otherwise.
#[derive(Debug, Clone)]
enum Tool {
    Command(String),
    File(String),
    /// A built-in that is neither (read_file, list_dir, glob, grep), shown as an MCP-style call.
    Builtin { tool: String, args: Value },
    /// A tool of one of the agent's MCP servers (`mcp__<server>__<tool>`), run by bija itself.
    Mcp { server: String, tool: String, args: Value },
    /// An external (`--tools-file`) tool waiting for the engine's answer. `answered` once
    /// `Cmd::Respond` completed the item, so bija's own `tool_done` for it doesn't do it again.
    External { tool: String, args: Value, answered: bool },
}

/// Folds bija's events into the Codex-shaped `HubEvent`s `Agent::apply` understands (SPEC §9.2).
/// Pure — no I/O — so the whole mapping is tested on the fixture below.
///
/// A `retry` after the call already streamed text or reasoning abandons those ids: the retry
/// streams under fresh ones and the old ones never get a `message` (SPEC §4.2). `Agent` has no way
/// to remove an item — the web view addresses items by position — so each abandoned item is
/// completed with its partial text replaced by `RETRIED`. Keeping the partial text, even marked,
/// would let it pass for the reply: it becomes `final_message` when nothing follows it, and the
/// engine reads `STATUS:`/`GATE:` anywhere in that.
struct Translator {
    agent: AgentId,
    /// The session this process was launched for: what `ready` reports if it names none.
    session: String,
    /// The window Mantra configured (`--context-window`), for a `usage` event that carries none.
    window: u64,
    /// Between `turn_started` and `turn_done`. A shell command is refused then, and a `compact`
    /// is bija's to run at the turn's end.
    turn_open: bool,
    /// A `compact` op went out while no turn was open. Its outcome is a `compacted` event, or a
    /// `warning` about the failed compaction that App must hear as a failed command — otherwise
    /// the agent reads "compacting" forever.
    idle_compact: bool,
    /// `ready` said `pending`: bija is finishing the turn the last process was in, until its
    /// `turn_done`. Text for it then goes out as a steer, not as a second turn.
    continuing: bool,
    /// Assistant message ids that already got their `item/started` but not yet their `message`.
    messages_started: HashSet<String>,
    /// Reasoning items still streaming; each is completed once its round moves on.
    reasoning_open: Vec<String>,
    tools: HashMap<String, Tool>,
    /// The latest `usage` event's totals, which a compaction's gauge updates carry too.
    usage: Totals,
    /// Compactions so far, for unique item ids.
    compactions: u32,
}

impl Translator {
    fn new(agent: AgentId, session: &str, window: u64) -> Translator {
        Translator {
            agent,
            session: session.to_string(),
            window,
            turn_open: false,
            idle_compact: false,
            continuing: false,
            messages_started: HashSet::new(),
            reasoning_open: vec![],
            tools: HashMap::new(),
            usage: Totals { window: Some(window).filter(|w| *w > 0), ..Totals::default() },
            compactions: 0,
        }
    }

    fn notif(&self, method: &str, params: Value) -> HubEvent {
        HubEvent::Notif { agent: self.agent, method: method.into(), params }
    }

    fn on_event(&mut self, v: &Value) -> Vec<HubEvent> {
        let mut out = vec![];
        match v.get("ev").and_then(|e| e.as_str()).unwrap_or("") {
            "ready" => self.on_ready(v, &mut out),
            "turn_started" => {
                self.turn_open = true;
                self.idle_compact = false;
                out.push(self.notif("turn/started", json!({"turn": {"id": text(v, "turn")}})));
            }
            "text_delta" => {
                let id = text(v, "id");
                self.close_reasoning(&mut out);
                self.start_message(&id, &mut out);
                out.push(self.notif("item/agentMessage/delta", json!({"itemId": id, "delta": text(v, "text")})));
            }
            "message" => {
                let id = text(v, "id");
                self.close_reasoning(&mut out);
                self.start_message(&id, &mut out);
                self.messages_started.remove(&id);
                out.push(self.notif("item/completed", json!({"item": {"id": id, "type": "agentMessage", "text": text(v, "text")}})));
            }
            "reasoning_delta" => {
                let id = text(v, "id");
                if !self.reasoning_open.contains(&id) {
                    self.reasoning_open.push(id.clone());
                }
                out.push(self.notif("item/reasoning/summaryTextDelta", json!({"itemId": id, "delta": text(v, "text")})));
            }
            "tool_started" => {
                self.close_reasoning(&mut out);
                let id = text(v, "id");
                let (tool, item) = started_item(&id, &text(v, "name"), v.get("args").unwrap_or(&Value::Null));
                self.tools.insert(id, tool);
                out.push(self.notif("item/started", json!({"item": item})));
            }
            "tool_output" => {
                let id = text(v, "id");
                // Only a command has an output stream to append to.
                if matches!(self.tools.get(&id), Some(Tool::Command(_)) | None) {
                    out.push(self.notif("item/commandExecution/outputDelta", json!({"itemId": id, "delta": text(v, "text")})));
                }
            }
            "tool_done" => self.on_tool_done(v, &mut out),
            "tool_call" => self.on_tool_call(v, &mut out),
            "usage" => {
                let window = v.get("window").and_then(|w| w.as_u64()).filter(|w| *w > 0).unwrap_or(self.window);
                self.usage = Totals { tokens: num(v, "total"), cached: num(v, "cached"), window: Some(window).filter(|w| *w > 0) };
                out.push(token_usage(self.agent, num(v, "last_in"), num(v, "last_out"), self.usage));
            }
            "retry" => {
                let wait = fmt_wait(num(v, "wait_ms"));
                let msg = format!("retrying ({}/{}) in {wait} — {}", num(v, "attempt"), num(v, "max"), crate::util::trunc(&text(v, "error"), 160));
                self.abandon_streamed(&mut out);
                // What Codex says while it reconnects: `Agent` shows the agent as retrying.
                out.push(self.notif("error", json!({"error": {"message": msg}, "willRetry": true})));
            }
            "compacted" => self.on_compacted(v, &mut out),
            "turn_done" => self.on_turn_done(v, &mut out),
            "warning" => {
                let msg = text(v, "message");
                if msg.starts_with("tool_result for unknown call id") {
                    // The engine answering a call it has just interrupted (another tool after
                    // `mantra_wait`): bija ended the turn first, so the answer finds no call.
                    // Expected, and already in the log (`read_ndjson`) — not the agent's news.
                } else if self.idle_compact && msg.to_lowercase().contains("compact") {
                    self.idle_compact = false;
                    out.push(HubEvent::CmdFailed { agent: self.agent, what: "compact", error: msg, text: None });
                } else {
                    out.push(self.notif("warning", json!({"message": msg})));
                }
            }
            // An op bija could not take (a Mantra bug, or a race with the end of a turn).
            "error" => out.push(self.notif("warning", json!({"message": format!("bija: {}", text(v, "message"))}))),
            // `status` answers an op Mantra never sends; `exit` precedes stdout closing, which is
            // where the exit itself is handled.
            _ => {}
        }
        out
    }

    /// A resumed session that bija continues by itself (`pending`) lost nothing: telling App it
    /// was resumed would reset the agent's turn and have the engine re-prompt it, so a second turn
    /// would queue behind the one bija is already finishing.
    fn on_ready(&mut self, v: &Value, out: &mut Vec<HubEvent>) {
        let session = Some(text(v, "session")).filter(|s| !s.is_empty()).unwrap_or_else(|| self.session.clone());
        let flag = |k: &str| v.get(k).and_then(|b| b.as_bool()).unwrap_or(false);
        self.continuing = flag("pending");
        out.push(HubEvent::Ready { agent: self.agent, thread_id: session, model: text(v, "model"), resumed: flag("resumed") && !flag("pending") });
    }

    fn start_message(&mut self, id: &str, out: &mut Vec<HubEvent>) {
        if self.messages_started.insert(id.to_string()) {
            out.push(self.notif("item/started", json!({"item": {"id": id, "type": "agentMessage", "text": ""}})));
        }
    }

    /// What a `retry` does to the items its call had streamed (see `Translator`): a non-empty
    /// `text`/`summary` replaces what the deltas built, so the partial text is gone from `Agent`.
    fn abandon_streamed(&mut self, out: &mut Vec<HubEvent>) {
        let mut messages: Vec<String> = self.messages_started.drain().collect();
        messages.sort();
        for id in messages {
            out.push(self.notif("item/completed", json!({"item": {"id": id, "type": "agentMessage", "text": RETRIED}})));
        }
        for id in std::mem::take(&mut self.reasoning_open) {
            out.push(self.notif("item/completed", json!({"item": {"id": id, "type": "reasoning", "summary": [RETRIED]}})));
        }
    }

    fn close_reasoning(&mut self, out: &mut Vec<HubEvent>) {
        for id in std::mem::take(&mut self.reasoning_open) {
            // No `summary`: `Agent` keeps the text the deltas built and just marks it done.
            out.push(self.notif("item/completed", json!({"item": {"id": id, "type": "reasoning"}})));
        }
    }

    fn on_tool_call(&mut self, v: &Value, out: &mut Vec<HubEvent>) {
        self.close_reasoning(out);
        let id = text(v, "id");
        let tool = text(v, "name");
        let args = v.get("args").cloned().unwrap_or_else(|| json!({}));
        out.push(self.notif("item/started", json!({"item": {"id": id, "type": "dynamicToolCall", "tool": tool, "arguments": args, "status": "inProgress"}})));
        // The same request the Codex path produces, so `App::on_request` / `Run::on_tool_call`
        // answer it unchanged; the answer comes back as `Cmd::Respond` with this id.
        out.push(HubEvent::Request { agent: self.agent, id: Value::String(id.clone()), method: "item/tool/call".into(), params: json!({"tool": tool, "arguments": args}) });
        self.tools.insert(id, Tool::External { tool, args, answered: false });
    }

    /// The engine answered an external call: the item completes now, not when bija gets round to
    /// its `tool_done`. An image in the answer stays on the item as the `inputImage` it came as.
    fn on_respond(&mut self, id: &str, ok: bool, result: &str, images: &[(String, String)]) -> Vec<HubEvent> {
        let Some(Tool::External { tool, args, answered }) = self.tools.get_mut(id) else { return vec![] };
        if std::mem::replace(answered, true) {
            return vec![];
        }
        let mut item = dynamic_item(id, tool, args, ok, result);
        for (mime, b64) in images {
            push_content_item(&mut item, json!({"type": "inputImage", "imageUrl": format!("data:{mime};base64,{b64}")}));
        }
        vec![self.notif("item/completed", json!({"item": item}))]
    }

    fn on_tool_done(&mut self, v: &Value, out: &mut Vec<HubEvent>) {
        let id = text(v, "id");
        let ok = v.get("ok").and_then(|b| b.as_bool()).unwrap_or(false);
        let status = if ok { "completed" } else { "failed" };
        let result = text(v, "result");
        // No `tool_started` came for a call bija refused outright (an unknown tool name): classify
        // it by name, so the failure still shows.
        let tool = self.tools.remove(&id).unwrap_or_else(|| started_item(&id, &text(v, "name"), &json!({})).0);
        let item = match tool {
            Tool::Command(command) => {
                // bija ends the text it gives the model with "[exit code N]" (and says "(no
                // output)" for none); the exit code travels on its own here and the card shows it
                // as ✓ / ✗, so the output is what the command printed, as Codex's is.
                let code = v.get("exit_code").and_then(|c| c.as_i64());
                let shown = match code {
                    Some(c) => {
                        let tail = format!("[exit code {c}]");
                        let body = result.strip_suffix(&tail).map(|b| b.strip_suffix('\n').unwrap_or(b)).unwrap_or(&result);
                        if body == "(no output)" { "" } else { body }
                    }
                    None => &result,
                };
                json!({
                    "id": id, "type": "commandExecution", "command": command, "status": status, "aggregatedOutput": shown,
                    "exitCode": code, "durationMs": v.get("duration_ms").and_then(|d| d.as_u64()),
                })
            }
            Tool::File(path) => {
                let mut changes: Vec<Value> = v
                    .get("changes")
                    .and_then(|c| c.as_array())
                    .map(|a| a.iter().map(|c| json!({"path": text(c, "path"), "kind": {"type": text(c, "kind")}, "diff": text(c, "diff")})).collect())
                    .unwrap_or_default();
                if changes.is_empty() {
                    // A refused edit changed nothing, but the card still names the file it tried.
                    changes.push(json!({"path": path, "kind": {"type": "modified"}, "diff": ""}));
                }
                json!({"id": id, "type": "fileChange", "status": status, "changes": changes})
            }
            // Only the head is shown (a whole file read is the model's, not the card's). Images a
            // tool returned are files under the session dir by now; the card names them, as Codex
            // describes an image — `inputImage` for a tool of bija's, MCP content for a server's.
            Tool::Builtin { tool, args } => {
                let mut item = dynamic_item(&id, &tool, &args, ok, &crate::util::trunc(&result, BUILTIN_SHOWN));
                for (_, path) in done_images(v) {
                    push_content_item(&mut item, json!({"type": "inputImage", "imageUrl": path}));
                }
                item
            }
            Tool::Mcp { server, tool, args } => {
                let text = crate::util::trunc(&result, BUILTIN_SHOWN);
                let images = done_images(v);
                let result = if images.is_empty() {
                    json!(text)
                } else {
                    let mut content = vec![json!({"type": "text", "text": text})];
                    content.extend(images.iter().map(|(mime, path)| json!({"type": "image", "mimeType": mime, "path": path})));
                    json!({"content": content})
                };
                json!({"id": id, "type": "mcpToolCall", "server": server, "tool": tool, "arguments": args, "status": status, "result": result})
            }
            Tool::External { answered: true, .. } => return,
            Tool::External { tool, args, .. } => dynamic_item(&id, &tool, &args, ok, &result),
        };
        out.push(self.notif("item/completed", json!({"item": item})));
    }

    /// `from` is the context before, `to` the summary's size. Without a `turn` it was an idle
    /// compaction, which is the answer the `compact` op was waiting for.
    fn on_compacted(&mut self, v: &Value, out: &mut Vec<HubEvent>) {
        self.compactions += 1;
        let id = format!("bija-compact-{}", self.compactions);
        out.extend(compaction(self.agent, &id, num(v, "from"), num(v, "to"), self.usage));
        if v.get("turn").map(|t| t.is_null()).unwrap_or(true) {
            self.idle_compact = false;
        }
    }

    fn on_turn_done(&mut self, v: &Value, out: &mut Vec<HubEvent>) {
        self.close_reasoning(out);
        self.turn_open = false;
        self.continuing = false;
        // Whatever a turn left open (an interrupted call) cannot complete any more.
        self.tools.clear();
        self.messages_started.clear();
        let status = Some(text(v, "status")).filter(|s| !s.is_empty()).unwrap_or_else(|| "completed".into());
        let error = if status == "failed" { turn_error(v.get("error")) } else { Value::Null };
        out.push(self.notif("turn/completed", json!({"turn": {"id": text(v, "turn"), "status": status, "error": error}})));
    }
}

fn text(v: &Value, k: &str) -> String {
    v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string()
}

fn num(v: &Value, k: &str) -> u64 {
    v.get(k).and_then(|x| x.as_u64()).unwrap_or(0)
}

/// What a built-in's `item/started` looks like (SPEC §9.2): `bash`/`job` are commands,
/// `write_file`/`edit_file`/`delete` file changes, every other built-in a dynamic tool call under
/// its own name — `read_file`, not an MCP `bija.read_file`: they are bija's own tools, as the
/// role's `mantra_*` ones are Mantra's. A tool of an MCP server, `mcp__<server>__<tool>`, is the
/// `mcpToolCall{server, tool}` Codex reports for one.
fn started_item(id: &str, name: &str, args: &Value) -> (Tool, Value) {
    if let Some((server, tool)) = super::mcp_tool_name(name) {
        let item = json!({"id": id, "type": "mcpToolCall", "server": server, "tool": tool, "arguments": args, "status": "inProgress"});
        return (Tool::Mcp { server: server.to_string(), tool: tool.to_string(), args: args.clone() }, item);
    }
    match name {
        "bash" | "job" => {
            let command = if name == "bash" { text(args, "command") } else { job_label(args) };
            (Tool::Command(command.clone()), json!({"id": id, "type": "commandExecution", "command": command, "status": "inProgress"}))
        }
        "write_file" | "edit_file" | "delete" => {
            let path = text(args, "path");
            (Tool::File(path.clone()), json!({"id": id, "type": "fileChange", "status": "inProgress", "changes": [{"path": path, "kind": {"type": "modified"}, "diff": ""}]}))
        }
        _ => (Tool::Builtin { tool: name.to_string(), args: args.clone() }, json!({"id": id, "type": "dynamicToolCall", "tool": name, "arguments": args, "status": "inProgress"})),
    }
}

/// `job wait 3`: the background-job tool as the command line it stands for.
fn job_label(args: &Value) -> String {
    let job = match args.get("id") {
        Some(Value::Number(n)) => format!(" {n}"),
        Some(Value::String(s)) if !s.is_empty() => format!(" {s}"),
        _ => String::new(),
    };
    format!("job {}{job}", Some(text(args, "action")).filter(|a| !a.is_empty()).unwrap_or_else(|| "list".into()))
}

fn dynamic_item(id: &str, tool: &str, args: &Value, ok: bool, result: &str) -> Value {
    json!({"id": id, "type": "dynamicToolCall", "tool": tool, "arguments": args, "status": if ok { "completed" } else { "failed" }, "contentItems": [{"type": "inputText", "text": result}]})
}

fn push_content_item(item: &mut Value, content: Value) {
    if let Some(items) = item.get_mut("contentItems").and_then(|c| c.as_array_mut()) {
        items.push(content);
    }
}

/// The `(mime, path)` of each image a `tool_done` lists (`images: ["<session dir>/images/<call
/// id>-<n>.<ext>", …]`, the files a screenshot or an MCP image became). bija names each file by
/// the type it checked from the bytes, so the extension is the type. None from a bija that lists none.
fn done_images(v: &Value) -> Vec<(String, String)> {
    let Some(images) = v.get("images").and_then(|i| i.as_array()) else { return vec![] };
    let mime = |path: &str| match path.rsplit('.').next().unwrap_or("").to_ascii_lowercase().as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => "image/png",
    };
    images.iter().filter_map(|i| i.as_str()).map(|path| (mime(path).to_string(), path.to_string())).collect()
}

/// A failed turn's error in Codex's shape. The status goes into the message (`HTTP 503: …`, as bija
/// itself prints it), so `agent::refine` can still classify the kinds with no `codexErrorInfo`.
fn turn_error(e: Option<&Value>) -> Value {
    let Some(e) = e.filter(|e| e.is_object()) else { return json!({"message": "turn failed"}) };
    let message = Some(text(e, "message")).filter(|m| !m.trim().is_empty()).unwrap_or_else(|| "turn failed".into());
    let message = match e.get("status").and_then(|s| s.as_u64()) {
        Some(s) if !message.contains(&format!("HTTP {s}")) => format!("HTTP {s}: {message}"),
        _ => message,
    };
    let mut out = json!({"message": message});
    if let Some(info) = codex_error_info(&text(e, "kind")) {
        out["codexErrorInfo"] = json!(info);
    }
    out
}

/// bija's `ErrorKind` → the `codexErrorInfo` key `agent::classify` maps to Mantra's `ErrKind`.
fn codex_error_info(kind: &str) -> Option<&'static str> {
    match kind {
        "transient" => Some("serverOverloaded"),
        "rate_limit" => Some("rateLimitExceeded"),
        "auth" => Some("unauthorized"),
        "context" => Some("contextWindowExceeded"),
        "bad_request" => Some("badRequest"),
        "usage" => Some("usageLimitExceeded"),
        _ => None, // interrupted, internal: nothing to classify
    }
}

fn fmt_wait(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else {
        format!("{}s", (ms + 500) / 1000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{Agent, ErrKind, Kind, Level, Signal, Status};

    fn spawn_cfg(home: &Path) -> BijaSpawn {
        BijaSpawn {
            base_url: "http://127.0.0.1:11434/v1".into(),
            wire: "openai-chat".into(),
            api_key_env: Some("OLLAMA_KEY".into()),
            context_window: 128_000,
            compact_pct: 80,
            system_prompt: "You are the worker.".into(),
            home: home.to_path_buf(),
            sandbox: "workspace-write".into(),
            writable_roots: vec![PathBuf::from("/data/cache")],
            network: true,
            vision: false,
        }
    }

    fn value_after<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
        args.windows(2).find(|w| w[0] == flag).map(|w| w[1].as_str())
    }

    #[test]
    fn build_args_says_everything_on_a_fresh_launch_and_again_on_a_resume() {
        let home = PathBuf::from("/m/bija");
        let bs = spawn_cfg(&home);
        let files = SessionFiles { system: home.join("sessions/s1/system.md"), tools: home.join("sessions/s1/tools.json"), mcp: None };
        let a = build_args(Path::new("/work"), &bs, &files, "local-model", "high", "s1", false, Some("Notes · worker"), &["--no-bwrap".into()]);
        assert_eq!(a[0], "--no-bwrap", "the configured command's own arguments come first");
        for (flag, want) in [
            ("--home", "/m/bija"),
            ("--session", "s1"),
            ("--cwd", "/work"),
            ("--provider-url", "http://127.0.0.1:11434/v1"),
            ("--wire", "openai-chat"),
            ("--api-key-env", "OLLAMA_KEY"),
            ("--model", "local-model"),
            ("--effort", "high"),
            ("--context-window", "128000"),
            ("--compact-at", "80"),
            ("--system-file", "/m/bija/sessions/s1/system.md"),
            ("--tools-file", "/m/bija/sessions/s1/tools.json"),
            ("--sandbox", "workspace-write"),
            ("--writable", "/data/cache"),
            ("--name", "Notes · worker"),
        ] {
            assert_eq!(value_after(&a, flag), Some(want), "{flag} in {a:?}");
        }
        assert!(a.contains(&"--network".to_string()));
        assert!(!a.contains(&"--resume".to_string()));
        // bija refuses a percentage over 100 as a usage error: a hand-edited one is clamped.
        let over = BijaSpawn { compact_pct: 120, ..bs.clone() };
        let a2 = build_args(Path::new("/work"), &over, &files, "m", "", "s1", false, None, &[]);
        assert_eq!(value_after(&a2, "--compact-at"), Some("100"));
        assert!(!a.iter().any(|x| x.contains("sk-")), "never a key on argv");

        // A resume names the same session and repeats the flags (a changed model is picked up);
        // the name is the session's own by now, and a model without effort control says so.
        let no_key = BijaSpawn { api_key_env: None, network: false, writable_roots: vec![], ..bs };
        let r = build_args(Path::new("/work"), &no_key, &files, "local-coder", "", "s1", true, None, &[]);
        assert_eq!(value_after(&r, "--resume"), Some("s1"));
        assert!(!r.contains(&"--session".to_string()) && !r.contains(&"--name".to_string()));
        assert_eq!(value_after(&r, "--model"), Some("local-coder"));
        assert_eq!(value_after(&r, "--effort"), Some("none"));
        assert!(!r.contains(&"--api-key-env".to_string()) && !r.contains(&"--network".to_string()) && !r.contains(&"--writable".to_string()));
        assert!(!a.contains(&"--mcp-file".to_string()) && !a.contains(&"--vision".to_string()) && !r.contains(&"--vision".to_string()), "no servers, no images: neither flag");

        // MCP servers and a model that takes images: `--mcp-file` and `--vision`, again on a resume
        let with = SessionFiles { mcp: Some(home.join("sessions/s1/mcp.json")), ..files };
        let eye = BijaSpawn { vision: true, ..no_key };
        for resume in [false, true] {
            let v = build_args(Path::new("/work"), &eye, &with, "m", "", "s1", resume, None, &[]);
            assert_eq!(value_after(&v, "--mcp-file"), Some("/m/bija/sessions/s1/mcp.json"), "{v:?}");
            assert!(v.contains(&"--vision".to_string()), "{v:?}");
        }
    }

    #[test]
    fn sandbox_names_map_to_bijas_vocabulary() {
        assert_eq!(sandbox_mode("read-only"), "read-only");
        assert_eq!(sandbox_mode("workspace-write"), "workspace-write");
        assert_eq!(sandbox_mode("danger-full-access"), "full");
        assert_eq!(sandbox_mode(""), "workspace-write", "the safe default for anything unknown");
    }

    #[test]
    fn a_session_is_prepared_privately_and_resumed_only_once_it_is_on_disk() {
        let home = std::env::temp_dir().join(format!("mantra-bija-prepare-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let tools = vec![json!({"type": "function", "name": "mantra_status", "description": "status", "inputSchema": {"type": "object"}})];
        let files = prepare_session(&home, "s-1", "be brief", &tools, &[]).unwrap();
        assert!(files.mcp.is_none() && !session_dir(&home, "s-1").join("mcp.json").exists(), "no servers, no mcp.json");
        assert_eq!(std::fs::read_to_string(&files.system).unwrap(), "be brief");
        let back: Value = serde_json::from_str(&std::fs::read_to_string(&files.tools).unwrap()).unwrap();
        assert_eq!(back, Value::Array(tools));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(session_dir(&home, "s-1")).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "the role's brief is nobody else's business");
        }
        // rewritten on the next launch: a resume picks up changed instructions
        prepare_session(&home, "s-1", "be thorough", &[], &[]).unwrap();
        assert_eq!(std::fs::read_to_string(&files.system).unwrap(), "be thorough");

        // the agent's MCP servers: `mcp.json` beside tools.json, in Claude Code's config shape
        let blender = crate::config::McpServer { command: "uvx".into(), args: vec!["blender-mcp".into()], env: [("BLENDER_PORT".to_string(), "9876".to_string())].into(), enabled: true };
        let with = prepare_session(&home, "s-1", "be thorough", &[], &[("blender".into(), blender)]).unwrap();
        let mcp = with.mcp.expect("a path for --mcp-file");
        assert_eq!(mcp, session_dir(&home, "s-1").join("mcp.json"));
        let cfg: Value = serde_json::from_str(&std::fs::read_to_string(&mcp).unwrap()).unwrap();
        assert_eq!(cfg, json!({"mcpServers": {"blender": {"command": "uvx", "args": ["blender-mcp"], "env": {"BLENDER_PORT": "9876"}}}}));
        // and emptied when a later launch has none, so the resumed bija stops starting them
        assert_eq!(prepare_session(&home, "s-1", "be thorough", &[], &[]).unwrap().mcp.as_ref(), Some(&mcp));
        let cfg: Value = serde_json::from_str(&std::fs::read_to_string(&mcp).unwrap()).unwrap();
        assert_eq!(cfg, json!({"mcpServers": {}}));

        assert!(!launch_identity(1, Some("s-1"), &home).1, "no session.json yet: started fresh under the same id");
        assert_eq!(launch_identity(1, Some("s-1"), &home).0, "s-1");
        std::fs::write(session_dir(&home, "s-1").join("session.json"), "{}").unwrap();
        assert_eq!(launch_identity(1, Some("s-1"), &home), ("s-1".to_string(), true));
        let (fresh, resume) = launch_identity(1, None, &home);
        assert!(valid_session_id(&fresh) && !resume);
        for bad in ["../../etc", "a/b", ".hidden", ""] {
            let (sid, resume) = launch_identity(1, Some(bad), &home);
            assert!(sid != bad && valid_session_id(&sid) && !resume, "{bad:?} → {sid}");
        }
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn commands_become_the_documented_ops() {
        assert_eq!(op_for(&Cmd::Turn { text: "go".into() }), Some(json!({"op": "prompt", "text": "go"})));
        assert_eq!(op_for(&Cmd::Steer { text: "also".into() }), Some(json!({"op": "steer", "text": "also"})));
        assert_eq!(op_for(&Cmd::Interrupt), Some(json!({"op": "interrupt"})));
        assert_eq!(op_for(&Cmd::Compact), Some(json!({"op": "compact"})));
        assert_eq!(op_for(&Cmd::SetEffort("high".into())), Some(json!({"op": "set", "effort": "high"})), "a live switch, no relaunch");
        assert_eq!(op_for(&Cmd::SetModel("m2".into())), Some(json!({"op": "set", "model": "m2"})));
        assert_eq!(op_for(&Cmd::SetVision(false)), Some(json!({"op": "set", "vision": false})));
        assert_eq!(op_for(&Cmd::SetName("run · qa".into())), Some(json!({"op": "set", "name": "run · qa"})));
        assert_eq!(op_for(&Cmd::Shell { command: "ls".into() }), Some(json!({"op": "shell", "command": "ls"})));
        assert_eq!(op_for(&Cmd::Shutdown), Some(json!({"op": "shutdown"})));
        let answer = Cmd::Respond { id: json!("call_1"), result: json!({"contentItems": [{"type": "inputText", "text": "a"}, {"type": "inputText", "text": "b"}], "success": true}) };
        assert_eq!(op_for(&answer), Some(json!({"op": "tool_result", "id": "call_1", "ok": true, "text": "a\nb"})));
        let refused = Cmd::Respond { id: json!("call_2"), result: json!({"contentItems": [{"type": "inputText", "text": "no"}], "success": false}) };
        assert_eq!(op_for(&refused), Some(json!({"op": "tool_result", "id": "call_2", "ok": false, "text": "no"})));
        assert_eq!(op_for(&Cmd::RespondErr { id: json!("call_3"), message: "boom".into() }), Some(json!({"op": "tool_result", "id": "call_3", "ok": false, "text": "boom"})));
        assert_eq!(op_for(&Cmd::Respond { id: json!(7), result: json!({}) }), None, "a numeric id is no bija call");
        for none in [Cmd::Archive, Cmd::SetApproval("never".into()), Cmd::Restart] {
            assert_eq!(op_for(&none), None, "{none:?}");
        }
    }

    fn turn_status(events: &[HubEvent]) -> Option<Value> {
        events.iter().find_map(|e| match e {
            HubEvent::Notif { method, params, .. } if method == "turn/completed" => Some(params["turn"].clone()),
            _ => None,
        })
    }

    #[test]
    fn failed_turns_carry_the_kind_and_the_status() {
        let mut tr = Translator::new(1, "s", 200_000);
        let done = |kind: &str, status: Option<u16>, message: &str| json!({"ev": "turn_done", "turn": "t1", "status": "failed", "error": {"kind": kind, "status": status, "message": message}});
        let t = turn_status(&tr.on_event(&done("auth", Some(401), "invalid api key"))).unwrap();
        assert_eq!((t["status"].as_str(), t["error"]["codexErrorInfo"].as_str(), t["error"]["message"].as_str()), (Some("failed"), Some("unauthorized"), Some("HTTP 401: invalid api key")));
        let t = turn_status(&tr.on_event(&done("context", Some(400), "HTTP 400: maximum context length"))).unwrap();
        assert_eq!(t["error"]["message"], "HTTP 400: maximum context length", "never says the status twice");
        assert_eq!(t["error"]["codexErrorInfo"], "contextWindowExceeded");
        let t = turn_status(&tr.on_event(&done("internal", None, "tool round cap"))).unwrap();
        assert!(t["error"].get("codexErrorInfo").is_none() && t["error"]["message"] == "tool round cap");
        for (kind, info) in [("transient", "serverOverloaded"), ("rate_limit", "rateLimitExceeded"), ("bad_request", "badRequest"), ("usage", "usageLimitExceeded")] {
            assert_eq!(codex_error_info(kind), Some(info));
        }
        assert_eq!(codex_error_info("interrupted"), None);
        // an interrupted turn has no error; a malformed one still fails readably
        let t = turn_status(&tr.on_event(&json!({"ev": "turn_done", "turn": "t2", "status": "interrupted", "error": null}))).unwrap();
        assert!(t["status"] == "interrupted" && t["error"].is_null());
        let t = turn_status(&tr.on_event(&json!({"ev": "turn_done", "turn": "t3", "status": "failed", "error": "??"}))).unwrap();
        assert_eq!(t["error"]["message"], "turn failed");
    }

    /// A pending resume is bija finishing the old turn by itself: Mantra must not treat it as a
    /// lost turn (reset, re-prompt). A plain resume is a resume.
    #[test]
    fn ready_reports_a_resume_only_when_the_old_turn_is_over() {
        let ready = |resumed: bool, pending: bool, session: &str| json!({"ev": "ready", "session": session, "dir": "/x", "model": "m", "wire": "openai_chat", "resumed": resumed, "pid": 1, "pending": pending});
        let get = |events: Vec<HubEvent>| match events.into_iter().next() {
            Some(HubEvent::Ready { thread_id, resumed, model, .. }) => (thread_id, resumed, model),
            other => panic!("expected Ready, got {other:?}"),
        };
        let mut tr = Translator::new(3, "s-launch", 1);
        assert_eq!(get(tr.on_event(&ready(false, false, "s-1"))), ("s-1".to_string(), false, "m".to_string()));
        assert!(get(tr.on_event(&ready(true, false, "s-1"))).1);
        assert!(!get(tr.on_event(&ready(true, true, "s-1"))).1, "bija continues the turn itself");
        assert!(tr.continuing, "text for it is a steer until that turn is done");
        tr.on_event(&json!({"ev": "turn_done", "turn": "t4", "status": "completed", "error": null}));
        assert!(!tr.continuing);
        assert_eq!(get(tr.on_event(&ready(false, false, ""))).0, "s-launch", "the launched id when ready names none");
    }

    /// An idle `compact` that fails is reported by bija as a warning only: App must hear it as a
    /// failed command (which clears its "compacting" state), and only then.
    #[test]
    fn a_failed_idle_compaction_is_a_failed_command_and_other_warnings_stay_warnings() {
        let mut tr = Translator::new(2, "s", 200_000);
        let warn = json!({"ev": "warning", "message": "compaction failed: HTTP 503: overloaded"});
        assert!(matches!(tr.on_event(&warn).as_slice(), [HubEvent::Notif { method, .. }] if method == "warning"), "nobody asked for a compaction");
        tr.idle_compact = true;
        assert!(matches!(tr.on_event(&json!({"ev": "warning", "message": "bwrap unavailable — commands run unsandboxed"})).as_slice(), [HubEvent::Notif { method, .. }] if method == "warning"));
        // the engine answering a call it interrupted a moment ago: expected, so no amber line
        assert!(tr.on_event(&json!({"ev": "warning", "message": "tool_result for unknown call id call_9: ignored"})).is_empty());
        assert!(tr.idle_compact, "and it is no compaction news either");
        match tr.on_event(&warn).as_slice() {
            [HubEvent::CmdFailed { what: "compact", error, .. }] => assert!(error.contains("503"), "{error}"),
            other => panic!("expected the compaction to fail as a command, got {other:?}"),
        }
        assert!(!tr.idle_compact);
        // a successful idle compaction clears the expectation too, with the gauge dance
        tr.idle_compact = true;
        let out = tr.on_event(&json!({"ev": "compacted", "from": 90_000, "to": 3_000}));
        assert!(!tr.idle_compact);
        let methods: Vec<&str> = out.iter().filter_map(|e| if let HubEvent::Notif { method, .. } = e { Some(method.as_str()) } else { None }).collect();
        assert_eq!(methods, ["thread/tokenUsage/updated", "item/started", "item/completed", "thread/tokenUsage/updated"]);
    }

    /// Bija never sends `tool_started` for a call it refused outright (an unknown tool): the
    /// failure is still shown, classified by name; a second answer to one external call is a no-op.
    #[test]
    fn unannounced_tool_results_still_show_and_answers_complete_once() {
        let mut tr = Translator::new(1, "s", 200_000);
        let out = tr.on_event(&json!({"ev": "tool_done", "turn": "t1", "id": "c9", "name": "web_search", "ok": false, "result": "unknown tool web_search; available: read_file, …", "duration_ms": 0}));
        let item = match out.as_slice() {
            [HubEvent::Notif { method, params, .. }] if method == "item/completed" => params["item"].clone(),
            other => panic!("{other:?}"),
        };
        assert_eq!((item["type"].as_str(), item["tool"].as_str(), item["status"].as_str()), (Some("dynamicToolCall"), Some("web_search"), Some("failed")));
        tr.on_event(&json!({"ev": "tool_call", "turn": "t1", "id": "c10", "name": "mantra_wait", "args": {}}));
        assert_eq!(tr.on_respond("c10", true, "done", &[]).len(), 1);
        assert!(tr.on_respond("c10", true, "done", &[]).is_empty(), "answered already");
        assert!(tr.on_respond("nope", true, "x", &[]).is_empty(), "an unknown id completes nothing");
        assert!(tr.on_event(&json!({"ev": "tool_done", "turn": "t1", "id": "c10", "name": "mantra_wait", "ok": true, "result": "done", "duration_ms": 5})).is_empty(), "bija's own tool_done for an answered call adds nothing");
        assert_eq!(job_label(&json!({"action": "wait", "id": 3})), "job wait 3");
        assert_eq!(job_label(&json!({})), "job list");
    }

    fn completed(out: &[HubEvent]) -> Value {
        match out {
            [HubEvent::Notif { method, params, .. }] if method == "item/completed" => params["item"].clone(),
            other => panic!("{other:?}"),
        }
    }

    /// A server's tool, `mcp__<server>__<tool>`, is an `mcpToolCall{server, tool}` from start to
    /// end, its text result plain text; the images a `tool_done` lists ride along as MCP image
    /// content, those of a tool of bija's own as `inputImage` items.
    #[test]
    fn mcp_tools_are_their_servers_and_tool_images_stay_on_the_card() {
        let mut tr = Translator::new(1, "s", 200_000);
        let mut agent = Agent::new(1, "t", "worker", PathBuf::from("/tmp/bijamcp"));
        let started = tr.on_event(&json!({"ev": "tool_started", "turn": "t1", "id": "c1", "name": "mcp__demo__scene_info", "args": {}}));
        let item = match started.as_slice() {
            [HubEvent::Notif { method, params, .. }] if method == "item/started" => params["item"].clone(),
            other => panic!("{other:?}"),
        };
        assert_eq!((item["type"].as_str(), item["server"].as_str(), item["tool"].as_str()), (Some("mcpToolCall"), Some("demo"), Some("scene_info")));
        let done = completed(&tr.on_event(&json!({"ev": "tool_done", "turn": "t1", "id": "c1", "name": "mcp__demo__scene_info", "ok": true, "result": "3 objects: Cube, Camera, Light", "duration_ms": 9})));
        assert_eq!((done["server"].as_str(), done["tool"].as_str(), done["status"].as_str(), done["result"].as_str()), (Some("demo"), Some("scene_info"), Some("completed"), Some("3 objects: Cube, Camera, Light")));
        for (m, p) in [("item/started", json!({"item": item})), ("item/completed", json!({"item": done}))] {
            agent.apply(m, &p);
        }
        let tools: Vec<(String, String)> = agent.items.iter().filter_map(|i| if let Kind::Tool { name, result, .. } = &i.kind { Some((name.clone(), result.clone())) } else { None }).collect();
        assert_eq!(tools, vec![("demo.scene_info".to_string(), "3 objects: Cube, Camera, Light".to_string())]);

        tr.on_event(&json!({"ev": "tool_started", "turn": "t1", "id": "c2", "name": "mcp__demo__render_preview", "args": {"width": 64}}));
        // bija lists the files it saved, each named by its checked type
        let img = json!(["/b/sessions/s/images/c2-1.png", "/b/sessions/s/images/c2-2.jpg"]);
        let done = completed(&tr.on_event(&json!({"ev": "tool_done", "turn": "t1", "id": "c2", "name": "mcp__demo__render_preview", "ok": true, "result": "rendered", "images": img})));
        assert_eq!(done["result"], json!({"content": [{"type": "text", "text": "rendered"}, {"type": "image", "mimeType": "image/png", "path": "/b/sessions/s/images/c2-1.png"}, {"type": "image", "mimeType": "image/jpeg", "path": "/b/sessions/s/images/c2-2.jpg"}]}));
        tr.on_event(&json!({"ev": "tool_started", "turn": "t1", "id": "c3", "name": "screenshot", "args": {}}));
        let done = completed(&tr.on_event(&json!({"ev": "tool_done", "turn": "t1", "id": "c3", "name": "screenshot", "ok": true, "result": "saved", "images": ["/b/s.png"]})));
        assert_eq!(done["contentItems"], json!([{"type": "inputText", "text": "saved"}, {"type": "inputImage", "imageUrl": "/b/s.png"}]));
        // the card's image marker names the file, even when the text never does
        agent.apply("item/completed", &json!({"item": done}));
        let marker = agent.items.iter().rev().find_map(|i| if let Kind::Tool { image, .. } = &i.kind { image.clone() } else { None });
        assert_eq!(marker.as_deref(), Some("/b/s.png"));
        // a server name the model made up still splits; a call bija refused outright too
        let refused = completed(&tr.on_event(&json!({"ev": "tool_done", "turn": "t1", "id": "c4", "name": "mcp__nope__x", "ok": false, "result": "unknown tool"})));
        assert_eq!((refused["type"].as_str(), refused["server"].as_str(), refused["status"].as_str()), (Some("mcpToolCall"), Some("nope"), Some("failed")));
    }

    /// The engine's answer with an image (a screenshot): the op carries it as base64 beside the
    /// text, and the card keeps the `inputImage`; an answer without one is the op it always was.
    #[test]
    fn an_answer_with_an_image_goes_to_bija_as_tool_result_images() {
        let shot = Cmd::Respond { id: json!("c5"), result: json!({"contentItems": [{"type": "inputText", "text": "saved /r/screenshots/w-1.png (1920×1080)"}, {"type": "inputImage", "imageUrl": "data:image/png;base64,iVBORw0KGgo="}], "success": true}) };
        assert_eq!(op_for(&shot), Some(json!({"op": "tool_result", "id": "c5", "ok": true, "text": "saved /r/screenshots/w-1.png (1920×1080)", "images": [{"mime": "image/png", "b64": "iVBORw0KGgo="}]})));
        let plain = Cmd::Respond { id: json!("c6"), result: json!({"contentItems": [{"type": "inputText", "text": "ok"}], "success": true}) };
        assert!(op_for(&plain).unwrap().get("images").is_none(), "no image, no images key");
        let mut tr = Translator::new(1, "s", 200_000);
        tr.on_event(&json!({"ev": "tool_call", "turn": "t1", "id": "c5", "name": "mantra_screenshot", "args": {}}));
        let (call, ok, text) = respond_parts(&shot).unwrap();
        let item = completed(&tr.on_respond(&call, ok, &text, &respond_images(&shot)));
        assert_eq!(item["contentItems"][1], json!({"type": "inputImage", "imageUrl": "data:image/png;base64,iVBORw0KGgo="}));
        assert_eq!(item["tool"].as_str(), Some("mantra_screenshot"));
    }

    /// Feeds the hand-written session in `src/testdata/bija-events.jsonl` (SPEC §4.2: a full turn
    /// with streamed text and reasoning, a failing `cargo test` with streamed output, an
    /// `edit_file` change, a `read_file`, an external `mantra_*` call answered by `Cmd::Respond`,
    /// usage, a retry, a compaction, two retries that abandon a streamed reasoning item and a
    /// streamed partial report — then a turn that fails after its retries) through the
    /// translator into a real `Agent`, and checks what the user and the engine end up seeing.
    #[test]
    fn translates_the_recorded_bija_session_into_what_agent_expects() {
        let raw = include_str!("../testdata/bija-events.jsonl");
        let mut tr = Translator::new(1, "s-launch", 200_000);
        let mut agent = Agent::new(1, "w", "worker", PathBuf::from("/tmp/bijafix"));
        let (mut ready, mut requests, mut turns, mut files_changed) = (vec![], vec![], vec![], vec![]);
        for line in raw.lines().filter(|l| !l.trim().is_empty()) {
            let v: Value = serde_json::from_str(line).expect("the fixture is valid NDJSON");
            let mut events = tr.on_event(&v);
            if v["ev"] == "tool_call" {
                // What `run_bija_process` does with the engine's answer to the request above.
                events.extend(tr.on_respond("call_4Zx8", true, "phase 1/2: 1 of 2 tasks done", &[]));
            }
            for e in events {
                match e {
                    HubEvent::Ready { thread_id, resumed, model, .. } => ready.push((thread_id, resumed, model)),
                    HubEvent::Request { id, method, params, .. } => requests.push((id, method, params)),
                    HubEvent::Notif { method, params, .. } => {
                        for s in agent.apply(&method, &params) {
                            match s {
                                Signal::TurnDone { status, kind, .. } => turns.push((status, kind, agent.final_message.clone())),
                                Signal::FilesChanged(paths) => files_changed.extend(paths),
                                _ => {}
                            }
                        }
                    }
                    other => panic!("nothing in this session fails a command: {other:?}"),
                }
            }
        }

        assert_eq!(ready, vec![("0b7c1f3e-2a51-4f6e-9d2a-5c1e8f9a7b10".to_string(), false, "gpt-5.6-sol".to_string())]);
        assert_eq!(requests.len(), 1, "{requests:?}");
        assert_eq!((requests[0].0.as_str(), requests[0].1.as_str()), (Some("call_4Zx8"), "item/tool/call"));
        assert_eq!(requests[0].2, json!({"tool": "mantra_status", "arguments": {"note": "tests fixed"}}));

        // the turns: one report with the worker's STATUS line, then a failure after the retries
        assert_eq!(turns.len(), 2, "{turns:?}");
        assert_eq!(turns[0].0, "completed");
        assert_eq!(turns[0].2.as_deref(), Some("greet() now says hi; the tests pass.\nSTATUS: done"), "the last message of the turn is its report");
        assert_eq!((turns[1].0.as_str(), turns[1].1), ("failed", Some(ErrKind::Transient)));
        assert!(matches!(&agent.status, Status::Failed(m) if m.contains("HTTP 503") && m.contains("after 8 attempts")), "{:?}", agent.status);
        assert!(!agent.busy());

        let find = |pred: &dyn Fn(&Kind) -> bool| agent.items.iter().filter(|i| pred(&i.kind)).collect::<Vec<_>>();
        let reasoning = find(&|k| matches!(k, Kind::Reasoning));
        let texts = |items: &[&crate::agent::Item]| items.iter().map(|i| (i.id.clone(), i.text.clone(), i.done)).collect::<Vec<_>>();
        assert_eq!(
            texts(&reasoning),
            vec![
                ("t1-r1".to_string(), "The task wants greet() to say hi. Run the tests first.".to_string(), true),
                ("t1-r2".to_string(), RETRIED.to_string(), true),
                ("t1-r3".to_string(), "All green; report it.".to_string(), true),
            ],
            "streamed reasoning becomes one item; one a retry abandoned is closed without its partial text"
        );
        // A retry after streamed text: the abandoned partial (which already says "STATUS: do…")
        // is closed as retried, never left streaming or read as the report.
        assert_eq!(
            texts(&find(&|k| matches!(k, Kind::Agent))),
            vec![
                ("t1-m1".to_string(), "Running the tests first.".to_string(), true),
                ("t1-m3".to_string(), RETRIED.to_string(), true),
                ("t1-m4".to_string(), "greet() now says hi; the tests pass.\nSTATUS: done".to_string(), true),
            ]
        );
        match find(&|k| matches!(k, Kind::Command { .. })).as_slice() {
            [c] => match &c.kind {
                Kind::Command { cmd, output, exit, status, dur_ms } => {
                    assert_eq!((cmd.as_str(), *exit, status.as_str(), *dur_ms), ("cargo test", Some(101), "failed", Some(812)));
                    // the exit code is on the item; the output is what the command printed
                    assert!(output.contains("test greets ... FAILED") && !output.contains("[exit code"), "{output}");
                }
                _ => unreachable!(),
            },
            other => panic!("one command expected, got {}", other.len()),
        }
        match find(&|k| matches!(k, Kind::Files { .. })).as_slice() {
            [f] => match &f.kind {
                Kind::Files { changes, status } => {
                    assert_eq!(status, "completed");
                    assert_eq!((changes[0].path.as_str(), changes[0].kind.as_str()), ("src/lib.rs", "modified"));
                }
                _ => unreachable!(),
            },
            other => panic!("one file change expected, got {}", other.len()),
        }
        assert_eq!(files_changed, vec!["src/lib.rs".to_string()]);
        assert_eq!((agent.files["src/lib.rs"].adds, agent.files["src/lib.rs"].dels), (1, 1));
        let tools: Vec<(String, String, String)> = agent
            .items
            .iter()
            .filter_map(|i| match &i.kind {
                Kind::Tool { name, result, status, .. } => Some((name.clone(), result.clone(), status.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(tools.len(), 2, "read_file and the one external call, each once: {tools:?}");
        assert!(tools[0].0 == "read_file" && tools[0].2 == "completed" && tools[0].1.contains("greet"), "{tools:?}");
        assert!(tools[0].1.starts_with("     1\tpub fn greet"), "a built-in's text result reads as text, not a quoted JSON string: {:?}", tools[0].1);
        assert_eq!(tools[1], ("mantra_status".to_string(), "phase 1/2: 1 of 2 tasks done".to_string(), "completed".to_string()));
        assert!(agent.items.iter().any(|i| matches!(i.kind, Kind::Compaction { from: 171_000, to: Some(4_100) })), "the compaction keeps its from → to");

        // the gauge: the last call's context, the session's running totals, the configured window
        assert_eq!((agent.ctx_used, agent.tokens_total, agent.tokens_cached, agent.ctx_window), (4_340, 214_251, 172_288, Some(200_000)));
        // bija's one-time sandbox notice is housekeeping, not an amber line on every card
        assert!(agent.items.iter().any(|i| matches!(i.kind, Kind::Notice { level: Level::Info }) && i.text.contains("bwrap unavailable")));
        assert!(!agent.items.iter().any(|i| matches!(i.kind, Kind::Notice { level: Level::Warn }) && i.text.contains("bwrap")));
    }

    // ── the process itself, against a stand-in written in sh ──

    /// A stand-in bija: finds `--home` and the session id in its argv, writes session.json the
    /// way bija does, says `ready`, logs every op to `$OPS_LOG`, answers a prompt (or a steer) with
    /// a one-message turn, and exits on `shutdown`. `$CRASH_ONCE` makes the first launch die right
    /// after `ready`, `$FAIL_FIRST` before it, `$DIE_MID_TURN` inside its first turn (and resumes
    /// with nothing pending, as a bija that lost the turn would); `$PENDING` makes it continue an open turn on start;
    /// `$SHUTDOWN_DELAY` makes it take that many seconds to finish after `shutdown` (a running
    /// command's grace, say), touching `$FINISHED` once it has.
    const FAKE_BIJA: &str = r#"home=""; sid=""; resumed=false; prev="$0"
for a in "$@"; do
  case "$prev" in --home) home="$a";; --session) sid="$a";; --resume) sid="$a"; resumed=true;; esac
  prev="$a"
done
printf '%s\n' "$*" >> "$ARGS_LOG"
mkdir -p "$home/sessions/$sid" && printf '{}' > "$home/sessions/$sid/session.json"
if [ -n "$FAIL_FIRST" ] && [ ! -e "$FAIL_FIRST" ]; then : > "$FAIL_FIRST"; sleep 0.3; echo 'bija: provider unreachable (simulated)' >&2; exit 1; fi
printf '{"ev":"ready","session":"%s","dir":"x","model":"m","wire":"openai_chat","resumed":%s,"pid":1,"pending":%s}\n' "$sid" "$resumed" "${PENDING:-false}"
if [ -n "$PENDING" ]; then printf '{"ev":"turn_started","turn":"t1"}\n'; fi
if [ -n "$CRASH_ONCE" ] && [ ! -e "$CRASH_ONCE" ]; then : > "$CRASH_ONCE"; echo 'bija: provider stream broke (simulated)' >&2; exit 1; fi
while IFS= read -r line; do
  printf '%s\n' "$line" >> "$OPS_LOG"
  if [ -n "$DIE_MID_TURN" ] && [ ! -e "$DIE_MID_TURN" ] && case "$line" in *'"op":"prompt"'*) true;; *) false;; esac; then
    : > "$DIE_MID_TURN"; printf '{"ev":"turn_started","turn":"t1"}\n{"ev":"exit","code":143}\n'; exit 143
  fi
  case "$line" in
    *'"op":"prompt"'*) printf '{"ev":"turn_started","turn":"t1"}\n{"ev":"message","turn":"t1","id":"t1-m1","text":"hi"}\n{"ev":"turn_done","turn":"t1","status":"completed","error":null}\n';;
    *'"op":"steer"'*) printf '{"ev":"message","turn":"t1","id":"t1-m2","text":"steered"}\n{"ev":"turn_done","turn":"t1","status":"completed","error":null}\n';;
    *'"op":"shutdown"'*) sleep "${SHUTDOWN_DELAY:-0}"; printf '{"ev":"exit","code":0}\n'; [ -z "$FINISHED" ] || : > "$FINISHED"; exit 0;;
  esac
done"#;

    struct Fake {
        dir: PathBuf,
    }

    impl Fake {
        fn new(tag: &str) -> Fake {
            let dir = std::env::temp_dir().join(format!("mantra-bija-fake-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Fake { dir }
        }
        fn spec(&self, envs: Vec<(String, String)>) -> SpawnSpec {
            let mut envs = envs;
            envs.push(("OPS_LOG".into(), self.dir.join("ops.log").to_string_lossy().into_owned()));
            envs.push(("ARGS_LOG".into(), self.dir.join("args.log").to_string_lossy().into_owned()));
            SpawnSpec {
                cwd: self.dir.clone(),
                model: "m".into(),
                provider: "local".into(),
                effort: "medium".into(),
                approval: "never".into(),
                sandbox: "workspace-write".into(),
                developer_instructions: "You are the worker.".into(),
                dynamic_tools: vec![json!({"type": "function", "name": "mantra_status", "description": "status", "inputSchema": {"type": "object"}})],
                config: Default::default(),
                extra_args: vec![],
                resume_thread: None,
                thread_name: Some("Notes · worker".into()),
                backend: crate::config::ProviderKind::Bija,
                envs,
                claude: None,
                bija: Some(BijaSpawn { system_prompt: "You are the worker.".into(), ..spawn_cfg(&self.dir.join("home")) }),
                mcp: vec![],
            }
        }
        fn read(&self, f: &str) -> String {
            std::fs::read_to_string(self.dir.join(f)).unwrap_or_default()
        }
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    async fn next(rx: &mut mpsc::UnboundedReceiver<HubEvent>) -> HubEvent {
        tokio::time::timeout(Duration::from_secs(20), rx.recv()).await.expect("the agent task answers in time").expect("the agent task is alive")
    }

    async fn wait_for_file(path: &Path, needle: &str) -> String {
        for _ in 0..100 {
            let s = std::fs::read_to_string(path).unwrap_or_default();
            if s.contains(needle) {
                return s;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        std::fs::read_to_string(path).unwrap_or_default()
    }

    fn sh(script: &str) -> Vec<String> {
        ["sh", "-c", script].iter().map(|s| s.to_string()).collect()
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_bija_agent_comes_up_takes_a_turn_switches_model_live_and_shuts_down_cleanly() {
        let fake = Fake::new("turn");
        let (ev, mut rx) = mpsc::unbounded_channel();
        let (ctl, cmds) = mpsc::unbounded_channel();
        tokio::spawn(super::super::agent_task(4, fake.spec(vec![]), sh(FAKE_BIJA), ev, cmds, Duration::ZERO));
        let sid = match next(&mut rx).await {
            HubEvent::Ready { thread_id, resumed, .. } => {
                assert!(!resumed);
                thread_id
            }
            other => panic!("expected Ready, got {other:?}"),
        };
        let sdir = fake.dir.join("home").join("sessions").join(&sid);
        assert_eq!(std::fs::read_to_string(sdir.join("system.md")).unwrap(), "You are the worker.");
        assert!(std::fs::read_to_string(sdir.join("tools.json")).unwrap().contains("mantra_status"));
        let args = fake.read("args.log");
        assert!(args.contains(&format!("--session {sid}")) && args.contains("--name Notes · worker") && args.contains("--effort medium"), "{args}");

        ctl.send(Cmd::Turn { text: "go".into() }).unwrap();
        let mut methods = vec![];
        loop {
            match next(&mut rx).await {
                HubEvent::Notif { method, .. } => {
                    let end = method == "turn/completed";
                    methods.push(method);
                    if end {
                        break;
                    }
                }
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(methods, ["turn/started", "item/started", "item/completed", "turn/completed"]);

        ctl.send(Cmd::SetModel("m2".into())).unwrap();
        ctl.send(Cmd::SetEffort("high".into())).unwrap();
        let ops = wait_for_file(&fake.dir.join("ops.log"), "\"effort\":\"high\"").await;
        assert!(ops.contains(r#"{"model":"m2","op":"set"}"#) || ops.contains(r#""op":"set","model":"m2""#), "{ops}");
        ctl.send(Cmd::SetApproval("on-request".into())).unwrap();
        assert!(matches!(next(&mut rx).await, HubEvent::CmdFailed { what: "approval", error, .. } if error.contains("does not support")));
        assert!(!fake.read("ops.log").contains("approval"), "an unsupported policy must not be sent as a bija op");
        ctl.send(Cmd::Shutdown).unwrap();
        assert!(matches!(next(&mut rx).await, HubEvent::Exited { agent: 4 }), "a model switch never relaunched it, and shutdown ends it");
        assert!(fake.read("ops.log").contains("\"op\":\"shutdown\""));
        assert_eq!(fake.read("args.log").lines().count(), 1, "one launch in all");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_crash_after_ready_relaunches_with_resume_and_the_stderr_line() {
        let fake = Fake::new("crash");
        let marker = fake.dir.join("crashed-once");
        let (ev, mut rx) = mpsc::unbounded_channel();
        let (ctl, cmds) = mpsc::unbounded_channel();
        tokio::spawn(super::super::agent_task(5, fake.spec(vec![("CRASH_ONCE".into(), marker.to_string_lossy().into_owned())]), sh(FAKE_BIJA), ev, cmds, Duration::ZERO));
        let first = match next(&mut rx).await {
            HubEvent::Ready { thread_id, .. } => thread_id,
            other => panic!("expected Ready, got {other:?}"),
        };
        match next(&mut rx).await {
            HubEvent::Crashed { reason, restarting, attempt, .. } => {
                assert!(restarting && attempt == 1, "a crash after ready gets the long budget");
                assert_eq!(reason, "bija exited (code 1): provider stream broke (simulated)");
            }
            other => panic!("expected Crashed, got {other:?}"),
        }
        match next(&mut rx).await {
            HubEvent::Ready { thread_id, resumed, .. } => assert!(thread_id == first && resumed, "the same session, resumed"),
            other => panic!("expected the relaunch's Ready, got {other:?}"),
        }
        let args = fake.read("args.log");
        assert!(args.lines().nth(1).is_some_and(|l| l.contains(&format!("--resume {first}")) && !l.contains("--name")), "{args}");
        drop(ctl);
        assert!(matches!(next(&mut rx).await, HubEvent::Exited { agent: 5 }));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_bija_that_dies_before_ready_is_a_launch_failure_with_its_reason() {
        let fake = Fake::new("launch");
        let (ev, mut rx) = mpsc::unbounded_channel();
        let (ctl, cmds) = mpsc::unbounded_channel();
        let script = "echo 'bija: --provider-url is required (or BIJA_PROVIDER_URL)' >&2; exit 2";
        tokio::spawn(super::super::agent_task(6, fake.spec(vec![]), sh(script), ev, cmds, Duration::ZERO));
        let mut attempts = vec![];
        loop {
            if let HubEvent::Crashed { attempt, restarting, reason, .. } = next(&mut rx).await {
                assert_eq!(reason, "bija exited (code 2): --provider-url is required (or BIJA_PROVIDER_URL)");
                attempts.push(attempt);
                if !restarting {
                    break;
                }
            }
        }
        assert_eq!(attempts, vec![1, 2, 3], "the short launch budget, then parked");
        drop(ctl);
        assert!(matches!(next(&mut rx).await, HubEvent::Exited { agent: 6 }));
    }

    /// Collects notification methods up to and including `turn/completed`.
    async fn turn_methods(rx: &mut mpsc::UnboundedReceiver<HubEvent>) -> Vec<String> {
        let mut methods = vec![];
        loop {
            match next(rx).await {
                HubEvent::Notif { method, .. } => {
                    let end = method == "turn/completed";
                    methods.push(method);
                    if end {
                        return methods;
                    }
                }
                other => panic!("unexpected {other:?}"),
            }
        }
    }

    /// A prompt sent at spawn, before `ready`, survives a launch that fails: held, not written into
    /// a pipe that dies with its process, and delivered once a launch comes up.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_prompt_sent_before_ready_survives_a_failed_launch() {
        let fake = Fake::new("early");
        let marker = fake.dir.join("failed-once");
        let (ev, mut rx) = mpsc::unbounded_channel();
        let (ctl, cmds) = mpsc::unbounded_channel();
        tokio::spawn(super::super::agent_task(9, fake.spec(vec![("FAIL_FIRST".into(), marker.to_string_lossy().into_owned())]), sh(FAKE_BIJA), ev, cmds, Duration::ZERO));
        ctl.send(Cmd::Turn { text: "early bird".into() }).unwrap();
        ctl.send(Cmd::Shell { command: "ls".into() }).unwrap();
        match next(&mut rx).await {
            HubEvent::CmdFailed { what: "shell", text, error, .. } => assert!(text.as_deref() == Some("ls") && error.contains("starting"), "{error}"),
            other => panic!("a shell command needs a live session: {other:?}"),
        }
        match next(&mut rx).await {
            HubEvent::Crashed { restarting, reason, .. } => assert!(restarting && reason.contains("provider unreachable"), "{reason}"),
            other => panic!("expected the failed launch, got {other:?}"),
        }
        assert!(matches!(next(&mut rx).await, HubEvent::Ready { .. }));
        assert_eq!(turn_methods(&mut rx).await, ["turn/started", "item/started", "item/completed", "turn/completed"]);
        let ops = fake.read("ops.log");
        assert_eq!(ops.matches("early bird").count(), 1, "{ops}");
        assert!(ops.contains(r#""op":"prompt""#), "{ops}");
        drop(ctl);
        assert!(matches!(next(&mut rx).await, HubEvent::Exited { agent: 9 }));
    }

    /// After a host crash bija continues the open turn itself (`pending`): the engine's resume
    /// prompt joins that turn as a steer instead of queueing a second one — until it is done.
    #[cfg(unix)]
    #[tokio::test]
    async fn text_for_a_turn_bija_is_continuing_goes_out_as_a_steer() {
        let fake = Fake::new("pending");
        let (ev, mut rx) = mpsc::unbounded_channel();
        let (ctl, cmds) = mpsc::unbounded_channel();
        tokio::spawn(super::super::agent_task(10, fake.spec(vec![("PENDING".into(), "true".into())]), sh(FAKE_BIJA), ev, cmds, Duration::ZERO));
        ctl.send(Cmd::Turn { text: "[mantra:resume] carry on".into() }).unwrap();
        match next(&mut rx).await {
            HubEvent::Ready { resumed, .. } => assert!(!resumed, "a continued turn is no lost turn"),
            other => panic!("expected Ready, got {other:?}"),
        }
        assert_eq!(turn_methods(&mut rx).await, ["turn/started", "item/started", "item/completed", "turn/completed"]);
        let ops = fake.read("ops.log");
        assert!(ops.contains(r#""op":"steer""#) && ops.contains("[mantra:resume] carry on") && !ops.contains(r#""op":"prompt""#), "{ops}");
        // That turn is over: the next text is a turn of its own.
        ctl.send(Cmd::Turn { text: "next".into() }).unwrap();
        turn_methods(&mut rx).await;
        assert!(wait_for_file(&fake.dir.join("ops.log"), r#""op":"prompt""#).await.contains("next"));
        drop(ctl);
        assert!(matches!(next(&mut rx).await, HubEvent::Exited { agent: 10 }));
    }

    /// A bija that dies inside a turn and comes back with nothing pending (an older one that closed
    /// the turn on its way out, a transcript that lost its tail) has dropped that turn: the hub sends
    /// its text again, once, and the relaunch's `Ready` is no resume — so the engine, which would
    /// prompt a resumed busy agent itself, does not prompt a second time.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_turn_cut_off_and_not_resumed_by_bija_is_prompted_again_once() {
        let fake = Fake::new("cut");
        let marker = fake.dir.join("died-once");
        let (ev, mut rx) = mpsc::unbounded_channel();
        let (ctl, cmds) = mpsc::unbounded_channel();
        tokio::spawn(super::super::agent_task(12, fake.spec(vec![("DIE_MID_TURN".into(), marker.to_string_lossy().into_owned())]), sh(FAKE_BIJA), ev, cmds, Duration::ZERO));
        assert!(matches!(next(&mut rx).await, HubEvent::Ready { resumed: false, .. }));
        ctl.send(Cmd::Turn { text: "build the parser".into() }).unwrap();
        assert!(matches!(next(&mut rx).await, HubEvent::Notif { method, .. } if method == "turn/started"));
        match next(&mut rx).await {
            HubEvent::Crashed { reason, restarting, .. } => assert!(restarting && reason == "bija exited (code 143)", "{reason}"),
            other => panic!("expected Crashed, got {other:?}"),
        }
        match next(&mut rx).await {
            HubEvent::Ready { resumed, .. } => assert!(!resumed, "the turn is sent again, so nothing was lost"),
            other => panic!("expected the relaunch's Ready, got {other:?}"),
        }
        assert_eq!(turn_methods(&mut rx).await, ["turn/started", "item/started", "item/completed", "turn/completed"]);
        let ops = fake.read("ops.log");
        assert_eq!(ops.matches(r#""op":"prompt""#).count(), 2, "the first launch's prompt and the one sent again: {ops}");
        assert_eq!(ops.matches("build the parser").count(), 2, "{ops}");
        assert!(fake.read("args.log").lines().nth(1).is_some_and(|l| l.contains("--resume")));

        // A crash between turns leaves nothing to send again.
        ctl.send(Cmd::Restart).unwrap();
        assert!(matches!(next(&mut rx).await, HubEvent::Crashed { restarting: true, attempt: 0, .. }));
        assert!(matches!(next(&mut rx).await, HubEvent::Ready { resumed: true, .. }));
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(fake.read("ops.log").matches(r#""op":"prompt""#).count(), 2, "sent again once, not on every relaunch");
        drop(ctl);
        assert!(matches!(next(&mut rx).await, HubEvent::Exited { agent: 12 }));
    }

    /// A live switch to a model that takes images, or does not: `set{vision}` reaches bija at once
    /// (its tools follow at the next model call), and every later launch says `--vision` or not
    /// as the model in use does — a crash or a restart never brings back the spawn's choice.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_vision_switch_reaches_bija_and_every_later_launch() {
        let fake = Fake::new("vision");
        let (ev, mut rx) = mpsc::unbounded_channel();
        let (ctl, cmds) = mpsc::unbounded_channel();
        let mut spec = fake.spec(vec![]);
        if let Some(b) = spec.bija.as_mut() {
            b.vision = true;
        }
        tokio::spawn(super::super::agent_task(13, spec, sh(FAKE_BIJA), ev, cmds, Duration::ZERO));
        assert!(matches!(next(&mut rx).await, HubEvent::Ready { .. }));
        let launch = |n: usize| fake.read("args.log").lines().nth(n).map(|l| l.split(' ').any(|w| w == "--vision"));
        assert_eq!(launch(0), Some(true), "spawned for a model that sees");
        ctl.send(Cmd::SetModel("plain".into())).unwrap();
        ctl.send(Cmd::SetVision(false)).unwrap();
        let ops = wait_for_file(&fake.dir.join("ops.log"), r#""vision":false"#).await;
        assert!(ops.contains(r#"{"op":"set","vision":false}"#), "{ops}");
        ctl.send(Cmd::Restart).unwrap();
        assert!(matches!(next(&mut rx).await, HubEvent::Crashed { restarting: true, attempt: 0, .. }));
        assert!(matches!(next(&mut rx).await, HubEvent::Ready { resumed: true, .. }));
        assert_eq!(launch(1), Some(false), "the relaunch has no --vision: {}", fake.read("args.log"));
        ctl.send(Cmd::SetVision(true)).unwrap();
        ctl.send(Cmd::Restart).unwrap();
        assert!(matches!(next(&mut rx).await, HubEvent::Crashed { restarting: true, attempt: 0, .. }));
        assert!(matches!(next(&mut rx).await, HubEvent::Ready { resumed: true, .. }));
        assert_eq!(launch(2), Some(true), "and back: {}", fake.read("args.log"));
        drop(ctl);
        assert!(matches!(next(&mut rx).await, HubEvent::Exited { agent: 13 }));
    }

    /// A restart asks bija to shut down (its commands and jobs die with it) before the relaunch.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_restart_shuts_bija_down_before_it_resumes() {
        let fake = Fake::new("restart");
        let (ev, mut rx) = mpsc::unbounded_channel();
        let (ctl, cmds) = mpsc::unbounded_channel();
        tokio::spawn(super::super::agent_task(11, fake.spec(vec![]), sh(FAKE_BIJA), ev, cmds, Duration::ZERO));
        let first = match next(&mut rx).await {
            HubEvent::Ready { thread_id, .. } => thread_id,
            other => panic!("expected Ready, got {other:?}"),
        };
        ctl.send(Cmd::Restart).unwrap();
        assert!(matches!(next(&mut rx).await, HubEvent::Crashed { restarting: true, attempt: 0, .. }));
        match next(&mut rx).await {
            HubEvent::Ready { thread_id, resumed, .. } => assert!(thread_id == first && resumed),
            other => panic!("expected the relaunch's Ready, got {other:?}"),
        }
        assert!(fake.read("ops.log").contains(r#""op":"shutdown""#), "asked, not killed");
        assert!(fake.read("args.log").lines().nth(1).is_some_and(|l| l.contains(&format!("--resume {first}"))));
        drop(ctl);
        assert!(matches!(next(&mut rx).await, HubEvent::Exited { agent: 11 }));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_shell_command_during_a_turn_is_refused_without_reaching_bija() {
        let fake = Fake::new("shell");
        // a stand-in whose turn never ends
        let script = r#"printf '{"ev":"ready","session":"s1","dir":"x","model":"m","wire":"openai_chat","resumed":false,"pid":1,"pending":false}\n'
while IFS= read -r line; do printf '%s\n' "$line" >> "$OPS_LOG"; case "$line" in *'"op":"prompt"'*) printf '{"ev":"turn_started","turn":"t1"}\n';; esac; done"#;
        let (ev, mut rx) = mpsc::unbounded_channel();
        let (ctl, cmds) = mpsc::unbounded_channel();
        tokio::spawn(super::super::agent_task(8, fake.spec(vec![]), sh(script), ev, cmds, Duration::ZERO));
        assert!(matches!(next(&mut rx).await, HubEvent::Ready { .. }));
        ctl.send(Cmd::Turn { text: "go".into() }).unwrap();
        assert!(matches!(next(&mut rx).await, HubEvent::Notif { method, .. } if method == "turn/started"));
        ctl.send(Cmd::Shell { command: "ls".into() }).unwrap();
        match next(&mut rx).await {
            HubEvent::CmdFailed { what: "shell", text, .. } => assert_eq!(text.as_deref(), Some("ls")),
            other => panic!("expected the shell command to be refused, got {other:?}"),
        }
        assert!(!fake.read("ops.log").contains("\"op\":\"shell\""));
        drop(ctl);
        assert!(matches!(next(&mut rx).await, HubEvent::Exited { agent: 8 }), "a stand-in that ignores shutdown is killed after the grace");
    }

    /// Quitting waits for bija: a process that takes a second to finish after `shutdown` is let
    /// finish, not killed with the runtime a moment later — that kill would orphan its commands.
    #[cfg(unix)]
    #[tokio::test]
    async fn quitting_waits_for_a_bija_that_takes_a_moment_to_finish() {
        let fake = Fake::new("quit");
        let finished = fake.dir.join("finished");
        let (ev, mut rx) = mpsc::unbounded_channel();
        let mut hub = super::super::Hub::new(vec![], vec![], sh(FAKE_BIJA), ev, false);
        let id = hub.alloc_id();
        hub.spawn(id, fake.spec(vec![("SHUTDOWN_DELAY".into(), "1".into()), ("FINISHED".into(), finished.to_string_lossy().into_owned())]));
        assert!(matches!(next(&mut rx).await, HubEvent::Ready { .. }));
        let asked = std::time::Instant::now();
        assert!(hub.shutdown_all().wait().await, "it finished within the quit grace");
        assert!(asked.elapsed() >= Duration::from_secs(1), "the wait lasted as long as bija needed: {:?}", asked.elapsed());
        assert!(finished.exists(), "the stand-in ran to its end instead of being killed mid-shutdown");
        assert!(fake.read("ops.log").contains(r#""op":"shutdown""#));
        assert!(matches!(next(&mut rx).await, HubEvent::Exited { agent } if agent == id));
    }

    /// …but only so long: one that never exits costs a quit the limit, not its five-second
    /// restart grace — and a Hub without bija agents makes a quit wait for nothing.
    #[cfg(unix)]
    #[tokio::test]
    async fn quitting_waits_for_bija_only_so_long() {
        let fake = Fake::new("quit-stuck");
        let script = r#"printf '{"ev":"ready","session":"s1","dir":"x","model":"m","wire":"openai_chat","resumed":false,"pid":1,"pending":false}\n'
while IFS= read -r line; do :; done; sleep 30"#;
        let (ev, mut rx) = mpsc::unbounded_channel();
        let mut hub = super::super::Hub::new(vec![], vec![], sh(script), ev.clone(), false);
        let id = hub.alloc_id();
        hub.spawn(id, fake.spec(vec![]));
        assert!(matches!(next(&mut rx).await, HubEvent::Ready { .. }));
        let asked = std::time::Instant::now();
        assert!(!hub.shutdown_all().wait_at_most(Duration::from_millis(300)).await, "still running");
        assert!(asked.elapsed() < Duration::from_secs(2), "{:?}", asked.elapsed());
        let mut empty = super::super::Hub::new(vec![], vec![], sh(script), ev, false);
        assert!(empty.shutdown_all().wait_at_most(Duration::ZERO).await);
    }
}
