//! Tools Mantra gives agents (Codex "dynamic tools", executed by Mantra itself).

use serde_json::{json, Value};

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

fn gate_report_tool() -> Value {
    tool(
        "mantra_gate_report",
        "Report the gate verdict. pass=true only when the criteria are met and checks are green.",
        json!({"pass": {"type": "boolean"}, "summary": {"type": "string"}}),
        &["pass", "summary"],
    )
}

pub fn orchestrator_tools() -> Vec<Value> {
    let mut v = vec![
        tool("mantra_read_phase", "Read the current phase: goal, tasks, gate. Only the current phase is shown.", json!({}), &[]),
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
- End your final message with exactly:
STATUS: done | blocked
SUMMARY: <2-5 lines: what you changed, how you verified it, anything the next agent must know>
"#;

pub const GATE_PROTOCOL: &str = r#"
mantra-role: gate
## Mantra protocol
- You work on the integrated result. You may edit any file needed to make things coherent and green.
- When done, call `mantra_gate_report` with pass=true/false and a short summary. Then end your turn.
- Mantra re-runs the gate checks after you report; if they fail you'll get another round.
- Never report pass while a gate check fails. A check that is wrong, or cannot pass on this machine, is a
  plan problem: say so with `mantra_ask` (it goes up the chain; the planner can amend the checks) and wait.
"#;

pub const ARCHITECT_PROMPT: &str = r#"
mantra-role: architect
You are the Pattern Architect inside Mantra's Studio. You design agent workflows ("patterns") with the user.
A pattern is TOML with: name, description, [settings], [roles.<name>] (kind = planner|manager|orchestrator|worker|gate,
glyph = one narrow symbol like ✦ ◉ ◇ ◆ ◎ ▲ ■ ● ★, color = saffron|violet|teal|cyan|green|rose|red|amber|blue|gray,
model = alias, effort = low|medium|high|xhigh|max, sandbox, description, instructions), and [flow]
(planner, manager ("" = none: a run-wide supervisor that unsticks agents and resolves halts before the
planner is bothered), orchestrator, phase_gate, on_reprompt, and [[flow.finale]] steps with role, task, may_spawn).
[settings] also has manager_minutes (how often the manager gets a health digest; 0 = escalations only).
Always call mantra_read_pattern first, then mantra_write_pattern with the complete updated TOML.
If validation fails, fix and retry. Finally explain the change in 1-3 sentences.
"#;
