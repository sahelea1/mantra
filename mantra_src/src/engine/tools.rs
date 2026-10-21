//! Tools Mantra gives agents (Codex "dynamic tools", executed by Mantra itself).

use serde_json::{json, Value};
use std::path::Path;

fn tool(name: &str, description: &str, props: Value, required: &[&str]) -> Value {
    json!({
        "type": "function",
        "name": name,
        "description": description,
        "inputSchema": { "type": "object", "properties": props, "required": required }
    })
}

fn agent_tools() -> Vec<Value> {
    vec![
        tool("mantra_status", "Get a compact status of every agent in the run (state, activity, progress, tokens).", json!({}), &[]),
        tool(
            "mantra_log",
            "Read an agent's recent log (commands, edits, messages).",
            json!({"agent": {"type": "string", "description": "task id or role name"}, "lines": {"type": "integer", "description": "how many recent log entries (default 30)"}, "full": {"type": "boolean"}}),
            &["agent"],
        ),
    ]
}

/// `mantra_ask`: one rung up the chain of command. `up` names who answers.
fn ask_tool(up: &str) -> Value {
    tool(
        "mantra_ask",
        &format!("Ask the {up} when a decision is not yours to make or you are unsure and it matters (it changes the outcome, the scope, or another agent's work). The answer comes back as a message; if you cannot continue without it, end your turn and wait."),
        json!({"question": {"type": "string", "description": "what you need decided, with the options you see"}}),
        &["question"],
    )
}

/// The tools every worker gets: just the one it needs to not guess.
pub fn worker_tools() -> Vec<Value> {
    vec![ask_tool("orchestrator")]
}

pub fn planner_tools(worker_roles: &[String]) -> Vec<Value> {
    let mut v = vec![
        tool(
            "mantra_ask_user",
            "Ask the user. Only for decisions that change what is being built, its scope, or a trade-off only they can make — everything else you decide yourself. The run keeps going; their answer arrives as a [from the user] message.",
            json!({"question": {"type": "string"}}),
            &["question"],
        ),
        tool(
            "mantra_resume_run",
            "Resume a halted run (gate or attempts exhausted) because the plan is right as it is. `note` goes to the agent that was stuck as concrete guidance. To change tasks or gate checks call mantra_revise_plan instead — that resumes the run by itself.",
            json!({"note": {"type": "string"}}),
            &[],
        ),
        tool("mantra_prompt", "Send a message to an agent — e.g. answer a finale agent's question. For the orchestrator use mantra_brief_orchestrator.", json!({"agent": {"type": "string"}, "message": {"type": "string"}}), &["agent", "message"]),
        read_phase_tool(),
        tool("mantra_read_plan", "Read the full plan (every phase), as last submitted — mantra_read_phase only shows the current one. Use before revising a phase you haven't seen in this thread.", json!({}), &[]),
        tool("mantra_submit_plan", "Submit the phased plan. Mantra validates it; on errors fix them and submit again.", super::plan::plan_schema(worker_roles)["properties"].clone(), &["plan"]),
        tool(
            "mantra_revise_plan",
            "Replace the plan with a revised version (completed phases are kept as they are). Use after the user re-prompts.",
            {
                let mut p = super::plan::plan_schema(worker_roles)["properties"].clone();
                p["reason"] = json!({"type": "string"});
                p
            },
            &["plan"],
        ),
        tool("mantra_pause_agents", "Pause (interrupt) agents so they stop working until resumed.", json!({"agents": {"type": "array", "items": {"type": "string"}}, "reason": {"type": "string"}}), &["agents"]),
        tool("mantra_resume_agents", "Resume paused agents.", json!({"agents": {"type": "array", "items": {"type": "string"}}}), &["agents"]),
        tool("mantra_brief_orchestrator", "Send instructions to the orchestrator (it acts on them right away).", json!({"message": {"type": "string"}}), &["message"]),
    ];
    v.extend(agent_tools());
    v.extend(spawner_tools(worker_roles));
    v.push(gate_report_tool());
    v
}

fn spawner_tools(worker_roles: &[String]) -> Vec<Value> {
    vec![
        tool(
            "mantra_spawn_adhoc",
            "Spawn an ad-hoc worker for a fix (only during the final verification step).",
            json!({"title": {"type": "string"}, "role": {"type": "string", "enum": worker_roles}, "prompt": {"type": "string"}, "scope": {"type": "array", "items": {"type": "string"}}}),
            &["title", "role", "prompt"],
        ),
        tool("mantra_wait", "End your turn and wait. Mantra wakes you with events (worker finished/failed, etc.).", json!({}), &[]),
    ]
}

/// Shared by the planner (a respawn now sends only the current phase, and this is how it reads
/// live task states without the whole plan) and the orchestrator (which had it already).
fn read_phase_tool() -> Value {
    tool("mantra_read_phase", "Read the current phase: goal, tasks, gate. Only the current phase is shown.", json!({}), &[])
}

fn gate_report_tool() -> Value {
    tool(
        "mantra_gate_report",
        "Report the gate verdict. pass=true only when the criteria are met and checks are green. On fail, `summary` lists every finding as file, what is wrong, what to change.",
        json!({
            "pass": {"type": "boolean"},
            "summary": {"type": "string"},
            "big": {"type": "boolean", "description": "fail only: the fixes span several files or need design work (a bigger worker takes them)"}
        }),
        &["pass", "summary"],
    )
}

pub fn orchestrator_tools() -> Vec<Value> {
    let mut v = vec![
        read_phase_tool(),
        tool(
            "mantra_spawn",
            "Start the worker for a task of the current phase. Optionally sharpen its prompt or override reasoning effort.",
            json!({"task_id": {"type": "string"}, "prompt": {"type": "string", "description": "optional replacement prompt"}, "effort": {"type": "string"}}),
            &["task_id"],
        ),
        tool("mantra_prompt", "Send a message to a running worker (steers its current turn) or a follow-up to an idle one.", json!({"agent": {"type": "string"}, "message": {"type": "string"}}), &["agent", "message"]),
        tool("mantra_interrupt", "Interrupt an agent's current turn.", json!({"agent": {"type": "string"}}), &["agent"]),
        tool("mantra_set_effort", "Change an agent's reasoning effort (applies from its next turn).", json!({"agent": {"type": "string"}, "effort": {"type": "string"}}), &["agent", "effort"]),
        tool("mantra_retry", "Respawn a failed/stuck worker with a fresh thread, optionally with a better prompt.", json!({"task_id": {"type": "string"}, "prompt": {"type": "string"}}), &["task_id"]),
        tool("mantra_wait", "End your turn and wait. Mantra wakes you with events (worker finished/failed/tripwire/question).", json!({}), &[]),
        ask_tool("planner"),
    ];
    v.extend(agent_tools());
    v
}

/// The manager's tools (`flow.manager`): everything it needs to see the whole run and to get any
/// agent moving again, plus the two rungs above it — the planner (for plan changes) and the user.
/// It never edits code, so no spawning of new tasks and no gate report.
pub fn manager_tools() -> Vec<Value> {
    let mut v = vec![
        tool(
            "mantra_journal",
            "Read the run's recent journal: what Mantra and every agent did, in order (newest last). Start here when something looks wrong.",
            json!({"lines": {"type": "integer", "description": "how many recent lines (default 40, max 200)"}}),
            &[],
        ),
        tool("mantra_prompt", "Send a message to any agent: steer a running one, follow up an idle one, or answer a question it asked. Name it by task id or role (planner, orchestrator, gate, finale).", json!({"agent": {"type": "string"}, "message": {"type": "string"}}), &["agent", "message"]),
        tool("mantra_interrupt", "Interrupt an agent's current turn (it keeps its thread; follow up with mantra_prompt).", json!({"agent": {"type": "string"}}), &["agent"]),
        tool("mantra_set_effort", "Change an agent's reasoning effort (applies from its next turn) — heavier for a hard problem it keeps getting wrong, lighter for one that overthinks.", json!({"agent": {"type": "string"}, "effort": {"type": "string"}}), &["agent", "effort"]),
        tool(
            "mantra_respawn",
            "Restart an agent in place with a fresh thread — planner, orchestrator, gate, finale, or a task id. The new agent is briefed with the current state plus your note. For an agent that is looping, confused or unresponsive. Doing this during an escalated halt also resumes the run.",
            json!({"agent": {"type": "string"}, "note": {"type": "string", "description": "what the fresh agent must know or do differently"}}),
            &["agent"],
        ),
        tool(
            "mantra_retry",
            "Respawn a failed or stuck worker task with a fresh thread, optionally with a better prompt. Doing this during an escalated halt also resumes the run (the task gets one more attempt).",
            json!({"task_id": {"type": "string"}, "prompt": {"type": "string", "description": "optional replacement prompt"}}),
            &["task_id"],
        ),
        tool("mantra_pause_agents", "Pause (interrupt) agents so they stop working until resumed.", json!({"agents": {"type": "array", "items": {"type": "string"}}, "reason": {"type": "string"}}), &["agents"]),
        tool("mantra_resume_agents", "Resume paused agents.", json!({"agents": {"type": "array", "items": {"type": "string"}}}), &["agents"]),
        tool("mantra_brief_orchestrator", "Send instructions to the orchestrator of the current phase (it acts on them right away).", json!({"message": {"type": "string"}}), &["message"]),
        tool(
            "mantra_resume_run",
            "Resume a halted run (gate or attempts exhausted, an agent's turn failed) because the plan is right as it is. `note` goes to the agent that was stuck as concrete guidance (an exhausted task gets one more attempt). If the tasks or the gate checks themselves are wrong, ask the planner instead (mantra_ask): it can revise the plan, which resumes the run by itself.",
            json!({"note": {"type": "string"}}),
            &[],
        ),
        ask_tool("planner"),
        tool(
            "mantra_ask_user",
            "Ask the user. Only when nobody in the team can decide: credentials, the machine itself, or a trade-off that is theirs. The run keeps going; their answer arrives as a [from the user] message.",
            json!({"question": {"type": "string"}}),
            &["question"],
        ),
        tool("mantra_wait", "End your turn and wait. Mantra wakes you with the next escalation or health digest.", json!({}), &[]),
    ];
    v.extend(agent_tools());
    v
}

pub fn gate_tools(may_spawn: bool, worker_roles: &[String]) -> Vec<Value> {
    let mut v = vec![gate_report_tool(), ask_tool("orchestrator")];
    v.extend(agent_tools());
    if may_spawn {
        v.extend(spawner_tools(worker_roles));
    }
    v
}

/// Akasha (experimental): the tools of an agent whose role is in one of the pattern's `[[groups]]`,
/// added to its role's own when the feature is on (`Run::akasha_req`). They act on the group's block.
pub fn akasha_tools() -> Vec<Value> {
    vec![
        tool("mantra_akasha_read", "Read your group's shared notes: what the agents working beside you chose to share, and what Mantra noted for them (tasks done or failed, files changed).", json!({}), &[]),
        tool(
            "mantra_akasha_note",
            "Add a note to your group's shared notes: a fact about the codebase another agent would need, a decision, an interface you created, a pitfall — short, one thing per note. Forget what went stale. `urgent` reaches the others mid-turn: use it only for a change that affects their work right now.",
            json!({"text": {"type": "string", "description": "the note, at most 1000 characters"}, "urgent": {"type": "boolean", "description": "push it to the busy members now (default false)"}}),
            &["text"],
        ),
        tool("mantra_akasha_amend", "Replace a shared note's text (by its #id) when what it says changed.", json!({"id": {"type": "integer"}, "text": {"type": "string"}}), &["id", "text"]),
        tool("mantra_akasha_forget", "Remove a shared note that went stale (by its #id). The block is small: forget what no longer holds.", json!({"id": {"type": "integer"}}), &["id"]),
    ]
}

pub fn architect_tools() -> Vec<Value> {
    vec![
        tool("mantra_read_pattern", "Read the pattern currently open in the Studio (TOML).", json!({}), &[]),
        tool(
            "mantra_write_pattern",
            "Replace the open pattern with new TOML. Mantra validates it and shows the result live; on errors, fix and retry.",
            json!({"toml": {"type": "string"}}),
            &["toml"],
        ),
    ]
}

/// The tools that hand a model an image: offered only to an agent whose model takes images
/// (`ModelEntry.vision`), executed by Mantra, answered with an `inputImage` beside the text.
pub const VISION_TOOLS: [&str; 2] = ["mantra_screenshot", "mantra_view_image"];

pub fn vision_tools() -> Vec<Value> {
    vec![
        tool(
            "mantra_screenshot",
            "Capture the screen of the machine Mantra runs on (the host, where the user sees their apps) and see it as an image; the PNG's path comes back too. Each capture costs context (a thousand tokens or more), so capture when you need to look, and pass region when you know where to look.",
            json!({
                "region": {"type": "string", "description": "only this part of the screen: \"x,y,w,h\" in pixels, e.g. \"0,0,800,600\""},
                "display": {"type": "string", "description": "which display or output to capture (an X11 display such as \":0\", or a Wayland output name); default: the current one"}
            }),
            &[],
        ),
        tool(
            "mantra_view_image",
            "Look at an image file: PNG, JPEG, GIF or WebP, at most 8 MB. Returns the image and its size. The path is relative to your working directory, or absolute.",
            json!({"path": {"type": "string", "description": "path of the image file"}}),
            &["path"],
        ),
    ]
}

/// A `contentItems` text entry (the Codex app-server `ContentItem` shape).
pub fn text_item(text: &str) -> Value {
    json!({"type": "inputText", "text": text})
}

/// A `contentItems` image entry: the bytes as a `data:` URL.
pub fn image_item(mime: &str, bytes: &[u8]) -> Value {
    json!({"type": "inputImage", "imageUrl": format!("data:{mime};base64,{}", crate::util::b64(bytes))})
}

/// Run a vision tool for one agent (a run agent, or Solo): the `contentItems` to answer with and
/// whether it worked. `cwd` is what a relative `path` is read against; a capture is saved as the
/// next free `<stem>-<n>.png` in `shots`, owner-only (the host's desktop is nobody else's to
/// read). The path is always in the text as well, absolute, so a model can come back to the file
/// with its backend's own tools. Blocking (a capture can take seconds): callers run it as a job.
pub fn vision_call(tool: &str, args: &Value, cwd: &Path, shots: &Path, stem: &str) -> (Vec<Value>, bool) {
    let arg = |k: &str| args.get(k).and_then(|v| v.as_str()).map(str::trim).filter(|v| !v.is_empty());
    let got = match tool {
        "mantra_screenshot" => {
            // One capture at a time in the process: two at once would both take the first free
            // `<stem>-<n>.png` (and there is one screen to capture anyway).
            static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());
            let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
            private_dir(shots).and_then(|()| crate::util::screenshot(arg("region"), arg("display"), &next_shot(shots, stem))).and_then(|cap| {
                owner_only(&cap.path);
                let (mime, bytes) = crate::util::read_image(&cap.path)?;
                let path = std::fs::canonicalize(&cap.path).unwrap_or(cap.path.clone());
                let mut text = format!("Screenshot of the screen of the machine Mantra runs on, saved to {} ({}, via {})", path.display(), describe(mime, &bytes), cap.via);
                if let Some(r) = arg("region").filter(|_| cap.note.is_none()) {
                    text.push_str(&format!(", region {r}"));
                }
                text.push('.');
                if let Some(note) = cap.note {
                    text.push_str(&format!(" Note: {note}."));
                }
                Ok((text, mime, bytes))
            })
        }
        "mantra_view_image" => match arg("path") {
            None => Err("path is required: the image file to look at".to_string()),
            Some(p) => {
                let path = cwd.join(crate::util::expand_tilde(p, dirs::home_dir().as_deref()));
                crate::util::read_image(&path).map(|(mime, bytes)| (format!("{} ({})", path.display(), describe(mime, &bytes)), mime, bytes))
            }
        },
        other => Err(format!("{other} is not a vision tool")),
    };
    match got {
        Ok((text, mime, bytes)) => (vec![text_item(&text), image_item(mime, &bytes)], true),
        Err(e) => (vec![text_item(&e)], false),
    }
}

/// `image/png, 1920x1080 px, 812.3 KiB` (the pixel size where the header says it).
fn describe(mime: &str, bytes: &[u8]) -> String {
    let size = crate::util::human_bytes(bytes.len() as u64);
    match crate::util::png_size(bytes) {
        Some((w, h)) => format!("{mime}, {w}x{h} px, {size}"),
        None => format!("{mime}, {size}"),
    }
}

/// `dir`, created owner-only (0700) — and made so if it was there already: it is Mantra's own
/// (`<run dir>/screenshots`, `~/.mantra/screenshots`), whatever the umask or the home's mode.
fn private_dir(dir: &Path) -> Result<(), String> {
    let failed = |e: std::io::Error| format!("cannot create {}: {e}", dir.display());
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir).map_err(failed)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).map_err(failed)
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(dir).map_err(failed)
}

/// A capture the tool wrote at the umask's mode, made owner-only (0600).
fn owner_only(file: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    let _ = file;
}

/// `<shots>/<stem>-<n>.png` with the first `n` not taken: numbering needs no state, and a resumed
/// run never writes over an earlier capture.
fn next_shot(shots: &Path, stem: &str) -> std::path::PathBuf {
    let stem = match crate::util::slug(stem) {
        s if s.is_empty() => "agent".to_string(),
        s => s,
    };
    (1..).map(|n| shots.join(format!("{stem}-{n}.png"))).find(|p| !p.exists()).unwrap_or_else(|| shots.join(format!("{stem}.png")))
}

pub const PLANNER_PROTOCOL: &str = r#"
mantra-role: planner
## Mantra protocol
- Submit your plan by calling `mantra_submit_plan`. Never just write the plan as text or into a file.
- Mantra validates the plan and returns errors; fix them and resubmit.
- When re-prompted by the user during a run ([mantra:reprompt]): read state with `mantra_status`/`mantra_log`,
  pause agents that would waste work (`mantra_pause_agents`), revise the plan (`mantra_revise_plan`) if needed,
  and tell the orchestrator what changed (`mantra_brief_orchestrator`). Then summarize what you did in 2-4 lines.
- You are the top of the chain of command. Questions the orchestrator passes up ([mantra:question]) and halts
  Mantra could not resolve ([mantra:escalation]: gate or attempts exhausted) land with you. Decide yourself
  whenever the answer keeps the end product and the plan's intent; ask the user (`mantra_ask_user`) only when
  it changes what is being built, its scope, or is a trade-off only they can make.
- On an escalation act with exactly one of: `mantra_revise_plan` (fix this phase's tasks or checks — the run
  resumes by itself), `mantra_resume_run` (the plan is right; a note for the stuck agent), `mantra_ask_user`.
- Plan hygiene: keep the tooling later gates need (virtualenvs, node_modules, build caches) until the last
  phase — cleanup tasks belong in the final phase, never in one whose gate still needs them. Gate `checks`
  must run as-is on this machine; prefer what exists (`python3 -m pytest`, `cargo test`) over bootstrapping.
- In the final verification step you may spawn ad-hoc workers (`mantra_spawn_adhoc`), then `mantra_wait`,
  and finish with `mantra_gate_report`.
- Keep the plan proportional to the goal: a small goal is 1-2 phases, submitted in your first turn. An
  empty project needs no exploration: plan from scratch. Submit early; mantra_revise_plan exists for
  later corrections.
"#;

pub const ORCHESTRATOR_PROTOCOL: &str = r#"
mantra-role: orchestrator
## Mantra protocol
- Agents are addressed by task id (e.g. p1-auth) or role name.
- Start of a phase: spawn every task (mantra_spawn), then call mantra_wait and end your turn.
- Mantra wakes you with [mantra:event] messages. Handle them briefly, then mantra_wait again.
- Mantra automatically: saves each worker's output, retries API errors, merges work, runs the gate,
  and moves to the next phase. You don't need to do any of that.
- Never prompt a worker whose task is done — its report is final and it won't answer. After spawning,
  call mantra_wait. At most one mantra_prompt per event; silence (mantra_wait) is the normal answer.
- QUESTION events: a worker or the gate stopped to ask you something. Answer it promptly with mantra_prompt —
  decide yourself when it stays within the phase and the plan; when it would change the plan, the scope or
  the product, pass it up to the planner with mantra_ask (you get the answer as a [from the planner] message).
- [mantra:review]: every few minutes Mantra shows you each worker's recent work. Check the parallel work stays
  coherent — with each other and with the phase goal — and steer only where something is actually off.
- [mantra:handoff]: write a handoff note (≤10 lines) for your successor: decisions, risks, anything the next phase must know.
- A sandbox/environment error reported by a worker is never transient: never answer retry, never respawn
  for it (Mantra halts on it); if it reaches you anyway, pass it up with mantra_ask at once, quoting the
  error. Keep answers to workers short and decisive.
"#;

pub const MANAGER_PROTOCOL: &str = r#"
mantra-role: manager
## Mantra protocol
- Agents are addressed by task id (e.g. p1-auth) or role: planner, orchestrator, gate, finale.
- Mantra wakes you with [mantra:escalation] (the run is halted until someone acts — you are first in
  line, the planner after you), [mantra:watchdog] (an agent nobody could get moving) and [mantra:health]
  (a periodic digest). Read, decide, act, then call mantra_wait.
- Smallest fix first: a concrete hint (mantra_prompt, or mantra_resume_run(note) when halted) → a sharper
  prompt or a fresh start (mantra_retry / mantra_respawn) → a change to the plan or the gate checks (mantra_ask
  the planner; it revises the plan, which resumes the run) → the user (mantra_ask_user), last of all.
- A halted run continues only through mantra_resume_run, mantra_retry/mantra_respawn, or the planner revising
  the plan. A message alone (mantra_prompt) does not resume it.
- Never prompt a worker whose task is done. At most one message per agent per wake; on a healthy digest
  the right answer is just mantra_wait.
- You never edit code, never commit, never run the project: you steer the agents that do.
- Environment errors (a broken sandbox): no retries, ever — escalate to the planner at once.
"#;

pub const WORKER_PROTOCOL: &str = r#"
mantra-role: worker
## Mantra protocol
- You work in your own isolated copy of the repository. Only change files inside your scope.
- Don't commit, don't push, don't switch branches — Mantra handles version control.
- Unsure about a decision that matters (it changes the outcome, the scope, or another agent's work)? Don't
  guess: call `mantra_ask` — the orchestrator answers, or passes it further up. If you cannot continue without
  the answer, end your turn without a STATUS line; you'll be woken with the answer.
- If you are blocked (missing info, impossible task), stop and say so.
- Environment errors are not yours to fix. If a command fails before it runs (bwrap, user namespaces,
  the sandbox refusing to start), or a write is refused because your role is read-only (that belongs
  to a worker), do not retry or work around it: report it once with mantra_ask quoting the exact error,
  then end your turn with STATUS: blocked.
- End your final message with exactly:
STATUS: done | blocked
SUMMARY: <2-5 lines: what you changed, how you verified it, anything the next agent must know>
"#;

pub const GATE_PROTOCOL: &str = r#"
mantra-role: gate
## Mantra protocol
- You work on the integrated result. You verify and report; you never fix it yourself.
- Your prompt holds the check results, the diff and the reports so far: start there, read further only to settle a doubt.
- When done, call `mantra_gate_report` with pass=true/false and a summary. Then end your turn.
- On fail the summary is the work order for a fix worker: every finding with file, what is wrong, what to change.
  Set big=true only if the fixes span several files or need design work.
- Mantra re-runs the gate checks after a pass. Never report pass while a gate check fails. A check that is wrong,
  or cannot pass on this machine, is a plan problem: say so with `mantra_ask` (it goes up the chain; the planner
  can amend the checks) and wait.
"#;

/// Appended to a member's instructions together with `akasha_tools`.
pub const AKASHA_PROTOCOL: &str = r#"
## Shared notes (akasha)
- You share one small block of notes with the other agents of your group. Mantra shows it at the end of
  its prompts to you; `mantra_akasha_read` shows it any time.
- Note (`mantra_akasha_note`) what an agent working beside you would need: a fact about the codebase, a
  decision, an interface you created, a pitfall. Short, one thing per note; `mantra_akasha_amend` it when
  it changes, `mantra_akasha_forget` it when it goes stale. Not a log of your work: Mantra notes what you
  finished and which files you changed.
- `urgent` reaches the others mid-turn: only for a change that affects their work right now.
"#;

pub const ARCHITECT_PROMPT: &str = r#"
mantra-role: architect
You are the Pattern Architect inside Mantra's Studio. You design agent workflows ("patterns") with the user.
A pattern is TOML with: name, description, [settings], [roles.<name>] (kind = planner|manager|orchestrator|worker|gate,
glyph = one narrow symbol like ✦ ◉ ◇ ◆ ◎ ▲ ■ ● ★, color = saffron|violet|teal|cyan|green|rose|red|amber|blue|gray,
model = alias, effort = auto|low|medium|high|xhigh|max (auto: Mantra picks per spawn from the role's job), sandbox, description, instructions), and [flow]
(planner, manager ("" = none: a run-wide supervisor that unsticks agents and resolves halts before the
planner is bothered), orchestrator, phase_gate, on_reprompt, and [[flow.finale]] steps with role, task, may_spawn).
[settings] also has manager_minutes (how often the manager gets a health digest; 0 = escalations only) and
context_cap (the most tokens of context any agent is told it has, 200000 in the built-ins; 0 = the model's full window).
A role may also set fallback = alias: the model that takes over when an agent of that role sits through
settings.first_token_minutes (default 4; 0 = off) twice with no output at all.
A role may also set mcp = ["name", …]: the MCP servers (from the user's settings.toml [mcp_servers]) its
agents get, e.g. mcp = ["blender"] for a role that drives Blender; ["*"] = every enabled server; unset = none.
Optional [[groups]] (Akasha, experimental: roles that share one block of notes) have name, roles (each role in
one group at most), shared_chars (1024-262144, default 16384), push = none|urgent|all (default urgent), auto (default true).
Always call mantra_read_pattern first, then mantra_write_pattern with the complete updated TOML.
If validation fails, fix and retry. Finally explain the change in 1-3 sentences.
"#;
