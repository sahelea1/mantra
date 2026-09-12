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

pub fn planner_tools(worker_roles: &[String]) -> Vec<Value> {
    let mut v = vec![
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
        tool("mantra_wait", "End your turn and wait. Mantra wakes you with events (worker finished/failed/tripwire).", json!({}), &[]),
    ];
    v.extend(agent_tools());
    v
}

pub fn gate_tools(may_spawn: bool, worker_roles: &[String]) -> Vec<Value> {
    let mut v = vec![gate_report_tool()];
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
- [mantra:handoff]: write a handoff note (≤10 lines) for your successor: decisions, risks, anything the next phase must know.
"#;

pub const WORKER_PROTOCOL: &str = r#"
mantra-role: worker
## Mantra protocol
- You work in your own isolated copy of the repository. Only change files inside your scope.
- Don't commit, don't push, don't switch branches — Mantra handles version control.
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
"#;

pub const ARCHITECT_PROMPT: &str = r#"
mantra-role: architect
You are the Pattern Architect inside Mantra's Studio. You design agent workflows ("patterns") with the user.
A pattern is TOML with: name, description, [settings], [roles.<name>] (kind = planner|orchestrator|worker|gate,
glyph = one narrow symbol like ✦ ◉ ◇ ◆ ◎ ▲ ■ ● ★, color = saffron|violet|teal|cyan|green|rose|red|amber|blue|gray,
model = alias, effort = low|medium|high|xhigh|max, sandbox, description, instructions), and [flow]
(planner, orchestrator, phase_gate, on_reprompt, and [[flow.finale]] steps with role, task, may_spawn).
Always call mantra_read_pattern first, then mantra_write_pattern with the complete updated TOML.
If validation fails, fix and retry. Finally explain the change in 1-3 sentences.
"#;
