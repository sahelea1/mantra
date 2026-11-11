use super::*;
use crate::agent::Agent;
use std::collections::HashMap;

/// A minimal fake `Ctx` for unit-testing `Run`'s halt/resume behaviour without a real hub.
struct TestCtx {
    agents: HashMap<AgentId, Agent>,
    next: AgentId,
    interrupted: Vec<AgentId>,
    prompts: Vec<(AgentId, String)>,
    /// Every `ctx.spawn`'s instructions, by the id it was given — for tests that check what a
    /// freshly spawned agent was actually told (`spawn` itself ignores the `SpawnReq`).
    spawned_instructions: Vec<(AgentId, String)>,
    /// Every `ctx.spawn`: who it was for and with what effort / thread name / working dir.
    spawns: Vec<Spawned>,
    /// The tag of every background job the engine asked for (the closures are never run, but
    /// for the vision tools': see `finished`).
    jobs: Vec<String>,
    stopped: Vec<AgentId>,
    carried: Vec<(AgentId, AgentId)>,
    efforts_set: Vec<(AgentId, String)>,
    renamed: Vec<(AgentId, String)>,
    /// Every `ctx.spawn`'s role model alias, by the id it was given.
    spawned_models: Vec<(AgentId, String)>,
    /// What `ctx.notify` was told, in order.
    notified: Vec<String>,
    /// Every `ctx.tool_result_items`: to whom, the `contentItems`, whether it worked.
    items: Vec<(AgentId, Vec<Value>, bool)>,
    /// The vision jobs, run as they were asked for (on this thread), waiting for `settle`.
    finished: Vec<(JobTag, JobOut)>,
    /// Every `ctx.spawn_resumed`: the agent it gave and the thread it was re-attached to.
    resumed: Vec<(AgentId, String)>,
}
#[derive(Clone, Debug)]
struct Spawned {
    name: String,
    role_name: String,
    effort: Option<String>,
    thread_name: Option<String>,
    cwd: PathBuf,
    sandbox: String,
    model: String,
    context_override: Option<u64>,
    /// The names of the tools it was offered.
    tools: Vec<String>,
    /// The MCP servers it asked for.
    mcp: Vec<String>,
    git_write: bool,
}
impl TestCtx {
    // Tests below exercise the Conductor's decisions without making a provider call.
    fn new() -> TestCtx {
        TestCtx { agents: HashMap::new(), next: 1, interrupted: vec![], prompts: vec![], spawned_instructions: vec![], spawns: vec![], jobs: vec![], stopped: vec![], carried: vec![], efforts_set: vec![], renamed: vec![], spawned_models: vec![], notified: vec![], items: vec![], finished: vec![], resumed: vec![] }
    }
    /// Hands every finished vision job back to `run`, as the app's event loop would.
    fn settle(&mut self, run: &mut Run) {
        for (tag, out) in std::mem::take(&mut self.finished) {
            run.on_job(self, tag, out);
        }
    }
    fn spawned(&self, name: &str) -> Vec<Spawned> {
        self.spawns.iter().filter(|s| s.name == name).cloned().collect()
    }
    /// Register a fake agent that looks busy (a live turn in progress).
    fn add_busy(&mut self) -> AgentId {
        let id = self.next;
        self.next += 1;
        let mut a = Agent::new(id, "a", "worker", PathBuf::from("."));
        a.turn_active = true;
        self.agents.insert(id, a);
        id
    }
    /// Register a fake agent that is idle (no turn in progress) — the watchdog's target state.
    fn add_idle(&mut self) -> AgentId {
        let id = self.next;
        self.next += 1;
        self.agents.insert(id, Agent::new(id, "a", "worker", PathBuf::from(".")));
        id
    }
    /// Back-date an agent's `last_event` so it looks idle for `secs` (the watchdog's only
    /// signal), without an actual sleep.
    fn set_idle_secs(&mut self, a: AgentId, secs: u64) {
        if let Some(ag) = self.agents.get_mut(&a) {
            ag.last_event = Instant::now() - Duration::from_secs(secs);
            ag.turn_active = false;
            ag.awaiting_start = false;
        }
    }
    /// Back-date `last_event` while the turn stays live: a busy agent that has gone quiet,
    /// which is the state the ladder ignores and `watchdog_tick` only journals about.
    fn set_silent_secs(&mut self, a: AgentId, secs: u64) {
        if let Some(ag) = self.agents.get_mut(&a) {
            ag.last_event = Instant::now() - Duration::from_secs(secs);
            ag.turn_active = true;
        }
    }
}

fn planner_feature_run(name: &str, pattern: Pattern) -> (Run, TestCtx, AgentId) {
    let mut run = Run::new(PathBuf::from("."), pattern, name.into());
    run.dir = std::env::temp_dir().join(format!("mantra-planner-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&run.dir);
    run.stage = Stage::Planning;
    let mut ctx = TestCtx::new();
    let p = ctx.add_idle();
    ctx.agents.get_mut(&p).unwrap().approval = "on-request".into();
    run.planner = Some(p);
    (run, ctx, p)
}

#[test]
fn akasha_loop_revises_one_phase_and_completes_only_after_saving() {
    let pattern = Pattern::builtin_named("mantra-akasha").unwrap();
    let (mut run, mut ctx, p) = planner_feature_run("akasha-loop", pattern);
    let plan = json!({"plan": {"title": "Loop", "phases": [{"name": "Implement", "tasks": []}]}});
    assert!(run.handle_tool(&mut ctx, p, "mantra_submit_plan", &plan).1);
    run.on_turn_done(&mut ctx, p, "completed", None, None);
    assert_eq!(run.stage, Stage::Review);
    run.approve_plan(&mut ctx);
    assert!(run.loop_executing());
    assert!(run.orchestrator.is_none() && run.gate_agent.is_none());
    assert!(!run.handle_tool(&mut ctx, p, "mantra_revise_plan", &plan).1);
    assert!(!run.handle_tool(&mut ctx, p, "mantra_revise_pattern", &json!({})).1);
    run.set_pattern_changes(&mut ctx, true).unwrap();
    assert!(Pattern::from_toml(&std::fs::read_to_string(run.dir.join("pattern.toml")).unwrap()).unwrap().settings.allow_pattern_changes);
    assert!(run.handle_tool(&mut ctx, p, "mantra_revise_plan", &plan).1);
    assert_eq!(run.plan_version, 2);
    assert!(run.handle_tool(&mut ctx, p, "mantra_finish_plan", &json!({"summary": "Focused checks passed."})).1);
    run.on_turn_done(&mut ctx, p, "completed", None, None);
    assert_eq!(run.stage, Stage::Phase { idx: 0, step: PhaseStep::Handoff });
    assert_eq!(run.history.len(), 1);
    assert!(ctx.jobs.iter().any(|j| j.contains("Cleanup")));
    run.on_job(&mut ctx, JobTag::Cleanup { phase: 0 }, JobOut::Cleanup(Ok(String::new()), git::ReviewDiff::default()));
    assert_eq!(run.stage, Stage::Done);
    assert!(run.workers.is_empty());
    let _ = std::fs::remove_dir_all(&run.dir);
}

#[test]
fn auto_workflow_requires_user_approval_and_survives_resume() {
    let (mut run, mut ctx, p) = planner_feature_run("auto-approval", Pattern::builtin_auto());
    assert!(run.planning_prompt().contains("[mantra:auto]"));
    let (_, ok) = run.handle_tool(&mut ctx, p, "mantra_submit_plan", &json!({"plan": {}}));
    assert!(!ok, "Auto cannot submit a plan before creating its workflow");
    let proposed = Pattern::builtin_lean();
    let (reply, ok) = run.handle_tool(&mut ctx, p, "mantra_revise_pattern", &json!({"toml": proposed.to_toml(), "reason": "A short task needs a lean workflow; one QA gate is enough", "approved": true}));
    assert!(ok && reply.starts_with("PROPOSED"), "{reply}");
    assert!(run.pattern.is_auto(), "an agent-supplied approved field cannot grant consent");
    assert!(run.expected_active().is_empty(), "approval wait is intentional idleness");
    let st = RunState::load(&run.dir).unwrap();
    let mut restored = Run::from_state(&st, Pattern::builtin_auto(), None, run.dir.clone());
    let mut fresh = TestCtx::new();
    restored.resume_boot(&mut fresh, &st);
    assert!(restored.pending_pattern.is_some() && restored.question.is_some());
    assert!(fresh.prompts.is_empty(), "resume does not poll or imply approval");
    restored.user_input(&mut fresh, "approve");
    assert_eq!(restored.pattern.name, proposed.name);
    assert!(restored.pending_pattern.is_none());
    let disk = Pattern::from_toml(&std::fs::read_to_string(run.dir.join("pattern.toml")).unwrap()).unwrap();
    assert_eq!(disk.name, proposed.name, "resume uses the concrete workflow after approval");
    let planner = restored.planner.unwrap();
    let (reply, ok) = restored.handle_tool(&mut fresh, planner, "mantra_submit_plan", &json!({"plan": {"title": "small task", "phases": [{"name": "Build", "tasks": [{"id": "a", "title": "Task", "role": "worker-small", "prompt": "Implement the requested small task", "scope": ["src/a/**"]}]}]}}));
    assert!(ok, "{reply}");
    restored.on_turn_done(&mut fresh, planner, "completed", None, None);
    assert_eq!(restored.stage, Stage::Review, "Auto becomes a normal concrete run after bootstrapping");
    let _ = std::fs::remove_dir_all(&run.dir);
}

#[test]
fn workflow_rejection_and_dont_ask_mode_have_distinct_boundaries() {
    let (mut run, mut ctx, p) = planner_feature_run("approval-boundary", Pattern::builtin());
    let mut next = Pattern::builtin_lean();
    next.name = "adapted".into();
    let args = json!({"toml": next.to_toml(), "reason": "The manager topology is unnecessary for this task"});
    assert!(run.handle_tool(&mut ctx, p, "mantra_revise_pattern", &args).1);
    run.user_input(&mut ctx, "Do not approve this change");
    assert_eq!(run.pattern.name, super::super::pattern::NAME_DEFAULT);
    assert!(run.pending_pattern.is_none());
    ctx.agents.get_mut(&p).unwrap().approval = "never".into();
    assert!(run.handle_tool(&mut ctx, p, "mantra_revise_pattern", &args).1);
    assert_eq!(run.pattern.name, "adapted");
    assert!(run.pending_pattern.is_none() && run.question.is_none());
    let other = ctx.add_idle();
    assert!(!run.handle_tool(&mut ctx, other, "mantra_revise_pattern", &args).1);
    assert!(!run.handle_tool(&mut ctx, other, "mantra_kill", &json!({"agent": "planner", "reason": "override"})).1);
    let _ = std::fs::remove_dir_all(&run.dir);
}

#[test]
fn direct_user_message_routes_to_planner_and_planner_can_steer_worker() {
    let (mut run, mut ctx, p) = planner_feature_run("planner-routing", Pattern::builtin());
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
    let w = ctx.add_idle();
    run.workers.push(mk_worker("task", "worker-small", w, WState::Running));
    assert!(run.direct(&mut ctx, "task", "new requirement", Send::Force));
    assert!(ctx.prompts.iter().any(|(id, text)| *id == p && text.contains("new requirement")));
    assert!(ctx.prompts.iter().all(|(id, _)| *id != w));
    assert!(run.handle_tool(&mut ctx, p, "mantra_prompt", &json!({"agent": "task", "message": "Emergency: stop the repeated write"})).1);
    assert!(ctx.prompts.iter().any(|(id, text)| *id == w && text.contains("Emergency")));
    assert!(run.handle_tool(&mut ctx, p, "mantra_kill", &json!({"agent": "task", "reason": "repeated writes"})).1);
    assert!(ctx.stopped.contains(&w));
    assert_eq!(run.workers[0].state, WState::Cancelled);
    let _ = std::fs::remove_dir_all(&run.dir);
}

#[test]
fn planner_external_wait_is_bounded_persisted_and_wakes_once() {
    let (mut run, mut ctx, p) = planner_feature_run("external-wait", Pattern::builtin());
    let args = json!({"seconds": 30, "condition": "latest required CI pipeline for this SHA is terminal", "on_reentry": "Read CI once, distinguish cancelled from failed, and report evidence"});
    assert!(!run.handle_tool(&mut ctx, p, "mantra_wait_for_external_condition", &json!({"seconds": 86401, "condition": "CI", "on_reentry": "check"})).1);
    assert!(run.handle_tool(&mut ctx, p, "mantra_wait_for_external_condition", &args).1);
    run.on_turn_done(&mut ctx, p, "completed", None, None);
    assert!(ctx.prompts.is_empty(), "waiting does not trigger the missing-plan nudge");
    assert!(run.expected_active().is_empty());
    let st = RunState::load(&run.dir).unwrap();
    let mut restored = Run::from_state(&st, run.pattern.clone(), None, run.dir.clone());
    let mut fresh = TestCtx::new();
    restored.resume_boot(&mut fresh, &st);
    assert!(fresh.prompts.is_empty());
    restored.planner_wait_tick(&mut fresh);
    assert!(fresh.prompts.is_empty(), "not due yet");
    restored.planner_wait.as_mut().unwrap().due_unix = 0;
    restored.planner_wait_tick(&mut fresh);
    restored.planner_wait_tick(&mut fresh);
    assert_eq!(fresh.prompts.len(), 1);
    assert!(fresh.prompts[0].1.contains("[mantra:external-condition]") && fresh.prompts[0].1.contains("latest required CI"));
    assert!(restored.planner_wait.is_none());
    assert!(RunState::load(&run.dir).unwrap().planner_wait.is_none());
    let _ = std::fs::remove_dir_all(&run.dir);
}

#[test]
fn planner_finale_external_wait_does_not_advance_or_wake_early_on_resume() {
    let (mut run, mut ctx, p) = planner_feature_run("finale-wait", Pattern::builtin_lean());
    run.stage = Stage::Finale { idx: 0 };
    run.finale_agent = Some(p);
    assert!(run.handle_tool(&mut ctx, p, "mantra_wait_for_external_condition", &json!({"seconds": 30, "condition": "CI is complete", "on_reentry": "Check required CI, then report the verification verdict"})).1);
    run.on_turn_done(&mut ctx, p, "completed", None, None);
    assert_eq!(run.stage, Stage::Finale { idx: 0 }, "scheduled waiting is not a verification report");
    assert!(ctx.prompts.is_empty());
    let st = RunState::load(&run.dir).unwrap();
    let mut restored = Run::from_state(&st, run.pattern.clone(), None, run.dir.clone());
    let mut fresh = TestCtx::new();
    restored.resume_boot(&mut fresh, &st);
    assert_eq!(restored.stage, Stage::Finale { idx: 0 });
    assert!(fresh.prompts.is_empty(), "restart retains the wait instead of starting verification early");
    assert_eq!(restored.finale_agent, restored.planner);
    let _ = std::fs::remove_dir_all(&run.dir);
}

#[test]
fn planner_scope_update_changes_live_validation_and_refuses_overlap() {
    let (mut run, mut ctx, p) = planner_feature_run("scope-update", Pattern::builtin());
    run.plan = Some(Plan::from_value(&json!({"plan": {"title": "Build", "phases": [{"name": "Build", "tasks": [
        {"id": "a", "title": "A", "role": "worker-small", "prompt": "Implement the first independent component", "scope": ["src/a/**"]},
        {"id": "b", "title": "B", "role": "worker-small", "prompt": "Implement the second independent component", "scope": ["src/b/**"]}
    ]}]}})).unwrap());
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
    let w = ctx.add_idle();
    let mut worker = mk_worker("a", "worker-small", w, WState::Running);
    worker.task = run.plan.as_ref().unwrap().phases[0].tasks[0].clone();
    worker.tripwires = vec!["src/shared/api.rs".into()];
    run.workers.push(worker);
    let (reply, ok) = run.handle_tool(&mut ctx, p, "mantra_set_task_scope", &json!({"task_id": "a", "scope_globs": ["src/a/**", "src/shared/**"], "reason": "Shared API belongs to this task"}));
    assert!(ok, "{reply}");
    assert!(run.workers[0].tripwires.is_empty());
    assert_eq!(run.workers[0].task.scope, run.plan.as_ref().unwrap().phases[0].tasks[0].scope);
    assert!(!run.handle_tool(&mut ctx, p, "mantra_set_task_scope", &json!({"task_id": "a", "scope_globs": ["src/**"], "reason": "overlap"})).1);
    assert_eq!(run.workers[0].task.scope, vec!["src/a/**", "src/shared/**"]);
    let _ = std::fs::remove_dir_all(&run.dir);
}

#[test]
fn auto_sample_uses_configured_aliases_and_refreshes_planner_after_tool_result() {
    let mut auto = Pattern::builtin_auto();
    auto.roles.get_mut("planner").unwrap().model = "budget".into();
    let (mut run, mut ctx, p) = planner_feature_run("auto-model-tools", auto);
    run.set_known_models([("budget".into(), 128_000)].into_iter().collect());
    let prompt = run.planning_prompt();
    assert!(prompt.contains("model = \"budget\""));
    assert!(!prompt.contains("model = \"sol\"") && !prompt.contains("model = \"astra\""));
    let mut concrete = Pattern::builtin_lean();
    for role in concrete.roles.values_mut() { role.model = "budget".into(); }
    concrete.roles.get_mut("planner").unwrap().instructions = "Updated planner contract".into();
    ctx.agents.get_mut(&p).unwrap().approval = "never".into();
    ctx.agents.get_mut(&p).unwrap().turn_active = true;
    let (reply, ok) = run.handle_tool(&mut ctx, p, "mantra_revise_pattern", &json!({"toml": concrete.to_toml(), "reason": "Create the task workflow"}));
    assert!(ok, "{reply}");
    assert_eq!(run.planner, Some(p), "the caller lives until its tool result is delivered");
    assert!(!ctx.stopped.contains(&p));
    ctx.agents.get_mut(&p).unwrap().turn_active = false;
    run.on_turn_done(&mut ctx, p, "completed", None, None);
    let next = run.planner.unwrap();
    assert_ne!(next, p);
    assert!(ctx.stopped.contains(&p));
    assert!(ctx.spawned_instructions.iter().any(|(id, instructions)| *id == next && instructions.contains("Updated planner contract")));
    let _ = std::fs::remove_dir_all(&run.dir);
}

#[test]
fn workflow_replacement_rejects_unknown_primary_and_fallback_models() {
    let (mut run, _ctx, _p) = planner_feature_run("pattern-model-validation", Pattern::builtin());
    run.set_known_models([("known".into(), 128_000)].into_iter().collect());
    let mut candidate = Pattern::builtin_lean();
    for role in candidate.roles.values_mut() { role.model = "known".into(); }
    assert!(run.validate_pattern_replacement(&candidate).is_ok());
    candidate.roles.get_mut("planner").unwrap().fallback = Some("missing".into());
    assert!(run.validate_pattern_replacement(&candidate).unwrap_err().contains("fallback"));
    candidate.roles.get_mut("planner").unwrap().fallback = Some("known".into());
    candidate.roles.get_mut("worker-small").unwrap().model = "missing".into();
    assert!(run.validate_pattern_replacement(&candidate).unwrap_err().contains("worker-small"));
    let _ = std::fs::remove_dir_all(&run.dir);
}

#[test]
fn planner_emergency_stop_of_supervisor_is_recoverable_without_self_interrupt() {
    let (mut run, mut ctx, p) = planner_feature_run("kill-supervisor", Pattern::builtin());
    run.plan = Some(one_task_phase());
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
    let orch = ctx.add_busy();
    run.orchestrator = Some(orch);
    ctx.agents.get_mut(&p).unwrap().turn_active = true;
    assert!(run.handle_tool(&mut ctx, p, "mantra_kill", &json!({"agent": "orchestrator", "reason": "irrecoverable loop"})).1);
    assert!(run.halted());
    assert!(!ctx.interrupted.contains(&p), "the caller must receive the tool result");
    run.on_turn_done(&mut ctx, orch, "failed", Some("late process error".into()), None);
    assert_eq!(run.orchestrator, Some(orch), "late errors do not restart the killed role");
    let (reply, ok) = run.handle_tool(&mut ctx, p, "mantra_respawn", &json!({"agent": "orchestrator", "note": "Continue with one action per event"}));
    assert!(ok, "{reply}");
    assert_ne!(run.orchestrator, Some(orch));
    assert!(!run.halted());
    let _ = std::fs::remove_dir_all(&run.dir);
}

/// A minimal `Worker` for tests that don't need a real workspace/prepare cycle.
fn mk_worker(id: &str, role: &str, agent: AgentId, state: WState) -> Worker {
    Worker {
        task: Task { id: id.into(), role: role.into(), ..Default::default() },
        prompt: String::new(),
        effort: None,
        agent: Some(agent),
        attempt: 1,
        state,
        report: String::new(),
        wt: None,
        branch: String::new(),
        spawned: Instant::now(),
        finished: None,
        tripwires: vec![],
        adhoc: false,
        paused: false,
        stall_flagged: false,
        budget_flagged: false,
        context_override: None,
        idle_prompts: vec![],
        transient: false,
        model: None,
        status_asked: false,
        active: std::time::Duration::ZERO,
        superseded: false,
        owner_fix_failed: false, owner_fixing: None, fix_started: None,
    }
}
impl Ctx for TestCtx {
    fn spawn_resumed(&mut self, r: SpawnReq, thread: String) -> AgentId {
        let id = self.spawn(r);
        self.resumed.push((id, thread));
        id
    }
    fn spawn(&mut self, r: SpawnReq) -> AgentId {
        let id = self.next;
        self.next += 1;
        self.agents.insert(id, Agent::new(id, "a", "worker", PathBuf::from(".")));
        let tools = r.tools.iter().filter_map(|t| t.get("name").and_then(|n| n.as_str()).map(String::from)).collect();
        self.spawns.push(Spawned { name: r.name.clone(), role_name: r.role_name.clone(), effort: r.effort.clone(), thread_name: r.thread_name.clone(), cwd: r.cwd.clone(), sandbox: r.role.sandbox.clone(), model: r.role.model.clone(), context_override: r.context_override, tools, mcp: r.mcp.clone(), git_write: r.git_write });
        self.spawned_instructions.push((id, r.instructions));
        self.spawned_models.push((id, r.role.model));
        id
    }
    fn prompt(&mut self, a: AgentId, text: String) {
        self.prompts.push((a, text));
    }
    fn prompt_mode(&mut self, a: AgentId, text: String, _mode: Send) {
        self.prompts.push((a, text));
    }
    fn carry_over(&mut self, from: AgentId, to: AgentId) {
        self.carried.push((from, to));
    }
    fn interrupt(&mut self, a: AgentId) {
        self.interrupted.push(a);
        if let Some(ag) = self.agents.get_mut(&a) {
            ag.turn_active = false;
        }
    }
    fn compact(&mut self, _a: AgentId) {}
    fn stop(&mut self, a: AgentId, _archive: bool) {
        self.stopped.push(a);
    }
    fn tool_result(&mut self, _a: AgentId, _req: Value, _text: String, _ok: bool) {}
    fn tool_result_items(&mut self, a: AgentId, _req: Value, items: Vec<Value>, ok: bool) {
        self.items.push((a, items, ok));
    }
    fn set_effort(&mut self, a: AgentId, effort: &str) -> String {
        self.efforts_set.push((a, effort.to_string()));
        effort.to_string()
    }
    fn set_thread_name(&mut self, a: AgentId, name: &str) {
        self.renamed.push((a, name.to_string()));
    }
    fn agent(&self, a: AgentId) -> Option<&Agent> {
        self.agents.get(&a)
    }
    fn job(&mut self, tag: JobTag, f: Box<dyn FnOnce() -> JobOut + std::marker::Send>) {
        self.jobs.push(format!("{tag:?}"));
        if matches!(tag, JobTag::Vision { .. }) {
            self.finished.push((tag, f()));
        }
    }
    fn notify(&mut self, text: &str) {
        self.notified.push(text.to_string());
    }
}

/// Akasha in the Conductor: the block table, tool dispatch, what members are
/// told, pushes to busy members, the Conductor's own entries, and the feature switched off.
mod akasha {
    use super::*;
    use crate::engine::pattern::Group;
    use crate::engine::state::RunState;

    fn grouped(push: &str, auto: bool, cap: usize) -> Pattern {
        let mut p = Pattern::builtin();
        p.groups = vec![Group { name: "builders".into(), roles: vec!["worker-big".into(), "worker-small".into()], shared_chars: cap, push: push.into(), auto }];
        p
    }

    /// A pattern without groups (the built-ins have `builders`; a file from before groups has none).
    fn ungrouped() -> Pattern {
        Pattern { groups: vec![], ..Pattern::builtin() }
    }

    /// A run of `pattern` in its first phase, Akasha `on`, writing into a scratch dir of its own.
    fn run_of(pattern: Pattern, tag: &str) -> Run {
        let mut run = Run::new(PathBuf::from("."), pattern, "build it".into());
        run.dir = scratch_dir(&format!("akasha-{tag}"));
        run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
        run
    }

    /// A worker of a member role, running, its agent mid-turn.
    fn member(ctx: &mut TestCtx, run: &mut Run, id: &str) -> AgentId {
        let a = ctx.add_busy();
        run.workers.push(mk_worker(id, "worker-small", a, WState::Running));
        a
    }

    /// A worker of a member role started the way the Conductor starts one (spawn + brief).
    fn spawn_member(ctx: &mut TestCtx, run: &mut Run, id: &str) -> AgentId {
        let mut w = mk_worker(id, "worker-small", 0, WState::Preparing);
        w.agent = None;
        run.workers.push(w);
        let wi = run.workers.len() - 1;
        run.launch_worker(ctx, wi, PathBuf::from("."), String::new());
        run.workers[wi].agent.expect("spawned")
    }

    fn call(ctx: &mut TestCtx, run: &mut Run, a: AgentId, tool: &str, args: Value) -> (String, bool) {
        run.handle_tool(ctx, a, tool, &args)
    }

    fn last_prompt(ctx: &TestCtx, a: AgentId) -> String {
        ctx.prompts.iter().rev().find(|(x, _)| *x == a).map(|(_, t)| t.clone()).unwrap_or_default()
    }

    fn instructions_of(ctx: &TestCtx, a: AgentId) -> String {
        ctx.spawned_instructions.iter().find(|(x, _)| *x == a).map(|(_, i)| i.clone()).unwrap_or_default()
    }

    fn pushes(ctx: &TestCtx, to: AgentId) -> Vec<String> {
        ctx.prompts.iter().filter(|(x, t)| *x == to && t.starts_with("[akasha · ")).map(|(_, t)| t.clone()).collect()
    }

    fn texts(run: &Run) -> Vec<String> {
        run.akasha["builders"].entries.iter().map(|e| e.text.clone()).collect()
    }

    #[test]
    fn blocks_come_from_the_groups_and_only_members_are_in_one() {
        let mut ctx = TestCtx::new();
        let mut run = run_of(grouped("urgent", true, 4096), "table");
        assert_eq!(run.akasha.keys().map(String::as_str).collect::<Vec<_>>(), vec!["builders"]);
        assert_eq!((run.akasha["builders"].cap, run.akasha["builders"].entries.len()), (4096, 0));
        run.akasha.get_mut("builders").unwrap().note("p1-a", "kept", false, 0).unwrap();
        run.sync_akasha_groups();
        assert_eq!(run.akasha["builders"].entries.len(), 1, "turning it on again keeps what is there");
        let w = member(&mut ctx, &mut run, "p1-a");
        let o = ctx.add_idle();
        run.orchestrator = Some(o);
        assert_eq!(run.group_of(w), Some("builders"));
        assert_eq!(run.group_of(o), None, "the orchestrator's role is in no group");
        assert_eq!(run.group_of(999), None, "nor is an agent outside the run");
        run.pattern.groups.clear();
        run.sync_akasha_groups();
        assert_eq!(run.group_of(w), None);
        assert!(run.akasha.is_empty());
        assert!(run_of(ungrouped(), "table-plain").akasha.is_empty(), "no groups, no blocks");
        let _ = std::fs::remove_dir_all(&run.dir);
    }

    #[test]
    fn a_pattern_without_groups_exposes_no_akasha_tools() {
        let mut ctx = TestCtx::new();
        let mut run = run_of(ungrouped(), "ungrouped");
        run.stage = Stage::Setup;
        run.start(&mut ctx);
        let said: Vec<String> = pulse_texts(&run).into_iter().filter(|l| l.starts_with("akasha")).collect();
        assert!(said.is_empty());
        assert!(run.akasha.is_empty(), "no blocks");
        run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
        let a = spawn_member(&mut ctx, &mut run, "p1-a");
        assert!(!ctx.spawned("p1-a")[0].tools.iter().any(|t| t.starts_with("mantra_akasha_")), "no tools: {:?}", ctx.spawned("p1-a")[0].tools);
        assert!(!instructions_of(&ctx, a).contains("Shared notes"));
        let (r, ok) = call(&mut ctx, &mut run, a, "mantra_akasha_note", json!({"text": "x"}));
        assert!(!ok && r.contains("not in a group"), "{r}");
        let _ = std::fs::remove_dir_all(&run.dir);
    }

    #[test]
    fn members_get_the_tools_and_their_prompts_end_with_the_notes() {
        let mut ctx = TestCtx::new();
        let mut run = run_of(grouped("urgent", true, 16_384), "inject");
        let a = spawn_member(&mut ctx, &mut run, "p1-a");
        let tools = ctx.spawned("p1-a")[0].tools.clone();
        for t in ["mantra_akasha_read", "mantra_akasha_note", "mantra_akasha_amend", "mantra_akasha_forget", "mantra_ask"] {
            assert!(tools.iter().any(|x| x == t), "{t} in {tools:?}");
        }
        assert!(instructions_of(&ctx, a).contains("## Shared notes (akasha)"), "the spawn brief introduces the tools");
        assert!(!last_prompt(&ctx, a).contains("## Shared notes (akasha ·"), "an empty block is not shown");
        call(&mut ctx, &mut run, a, "mantra_akasha_note", json!({"text": "models live in src/models"}));
        // the next member's spawn brief ends with the block
        let b = spawn_member(&mut ctx, &mut run, "p1-b");
        let brief = last_prompt(&ctx, b);
        let tail = brief.split("\n\n## Shared notes (akasha · builders)\n").nth(1).unwrap_or_else(|| panic!("no notes in: {brief}"));
        assert!(tail.starts_with("## akasha · builders · 1 note · ") && tail.ends_with("] models live in src/models"), "{tail}");
        // and so does a retry prompt
        run.continue_queue.push((a, Instant::now(), "[mantra:retry] Your previous turn failed (x). Continue exactly where you left off.".into()));
        run.last_tick = Instant::now() - Duration::from_secs(2);
        run.tick(&mut ctx);
        let retry = last_prompt(&ctx, a);
        assert!(retry.starts_with("[mantra:retry]") && retry.contains("\n\n## Shared notes (akasha · builders)\n## akasha · builders · 1 note"), "{retry}");
        // the orchestrator is in no group: its tools and its prompts stay as they were
        let o = run.spawn_orchestrator(&mut ctx);
        assert!(!ctx.spawned("orchestrator")[0].tools.iter().any(|t| t.starts_with("mantra_akasha_")));
        assert_eq!(run.with_akasha(o, "[mantra:watchdog] nudge".into()), "[mantra:watchdog] nudge");
        let _ = std::fs::remove_dir_all(&run.dir);
    }

    #[test]
    fn the_tools_act_on_the_members_block_and_refuse_everyone_else() {
        let mut ctx = TestCtx::new();
        let mut run = run_of(grouped("none", true, 16_384), "tools");
        let a = member(&mut ctx, &mut run, "p1-a");
        let (r, ok) = call(&mut ctx, &mut run, a, "mantra_akasha_note", json!({"text": "  the config loader lives in src/config.rs ", "urgent": true}));
        assert!(ok && r.starts_with("noted #1 (") && r.ends_with("/16k)"), "{r}");
        assert!(call(&mut ctx, &mut run, a, "mantra_akasha_note", json!({"text": "temporary"})).0.starts_with("noted #2"));
        let (r, ok) = call(&mut ctx, &mut run, a, "mantra_akasha_amend", json!({"id": "#1", "text": "the config loader lives in src/cfg.rs"}));
        assert!(ok && r.starts_with("amended #1"), "{r}");
        let (r, ok) = call(&mut ctx, &mut run, a, "mantra_akasha_forget", json!({"id": 2}));
        assert!(ok && r.starts_with("forgot #2"), "{r}");
        let (r, ok) = call(&mut ctx, &mut run, a, "mantra_akasha_forget", json!({"id": 2}));
        assert!(!ok && r.contains("no note #2"), "{r}");
        let (r, ok) = call(&mut ctx, &mut run, a, "mantra_akasha_amend", json!({"text": "no id"}));
        assert!(!ok && r.contains("id"), "{r}");
        let (r, ok) = call(&mut ctx, &mut run, a, "mantra_akasha_read", json!({}));
        assert!(ok && r.starts_with("## akasha · builders · 1 note · ") && r.contains("\n![1 · p1-a · ") && r.ends_with("the config loader lives in src/cfg.rs"), "{r}");
        // one journal line for each change, and not one toast
        let said: Vec<String> = pulse_texts(&run).into_iter().filter(|l| l.starts_with("akasha ·")).collect();
        assert_eq!(said, vec!["akasha · p1-a noted #1 (urgent): the config loader lives in src/config.rs", "akasha · p1-a noted #2: temporary", "akasha · p1-a amended #1: the config loader lives in src/cfg.rs", "akasha · p1-a forgot #2"]);
        assert!(ctx.notified.is_empty());
        call(&mut ctx, &mut run, a, "mantra_akasha_note", json!({"text": "a long one\n".repeat(30)}));
        let last = pulse_texts(&run).pop().unwrap_or_default();
        assert!(last.chars().count() <= 100 && !last.contains('\n'), "{last}");
        // outside the group: the orchestrator, and an agent the run does not know
        let o = ctx.add_idle();
        run.orchestrator = Some(o);
        for who in [o, 999] {
            let (r, ok) = call(&mut ctx, &mut run, who, "mantra_akasha_read", json!({}));
            assert!(!ok && r.contains("not in a group"), "{r}");
        }
        let _ = std::fs::remove_dir_all(&run.dir);
    }

    #[test]
    fn urgent_notes_reach_busy_members_only_one_delivery_per_window() {
        let mut ctx = TestCtx::new();
        let mut run = run_of(grouped("urgent", true, 16_384), "push");
        let a = member(&mut ctx, &mut run, "p1-a");
        let b = member(&mut ctx, &mut run, "p1-b");
        let c = member(&mut ctx, &mut run, "p1-c");
        ctx.set_idle_secs(c, 0);
        let o = ctx.add_busy();
        run.orchestrator = Some(o);
        call(&mut ctx, &mut run, a, "mantra_akasha_note", json!({"text": "a plain fact"}));
        call(&mut ctx, &mut run, a, "mantra_akasha_note", json!({"text": "Config::load now returns Result", "urgent": true}));
        run.akasha_tick(&mut ctx);
        assert_eq!(pushes(&ctx, b), vec!["[akasha · p1-a] Config::load now returns Result"], "the urgent one only");
        assert!(pushes(&ctx, a).is_empty() && pushes(&ctx, c).is_empty() && pushes(&ctx, o).is_empty(), "not the author, not an idle member, not an outsider");
        // two more inside the window wait, then go as one
        call(&mut ctx, &mut run, a, "mantra_akasha_note", json!({"text": "second", "urgent": true}));
        call(&mut ctx, &mut run, a, "mantra_akasha_note", json!({"text": "third", "urgent": true}));
        run.akasha_tick(&mut ctx);
        assert_eq!(pushes(&ctx, b).len(), 1, "at most one delivery per 20 s");
        *run.akasha_live.pushed.get_mut(&b).unwrap() -= Duration::from_secs(21);
        run.akasha_tick(&mut ctx);
        assert_eq!(pushes(&ctx, b)[1], "[akasha · p1-a] second\n\n[akasha · p1-a] third");
        // a member that went idle before its window opened gets nothing (its next prompt has it)
        call(&mut ctx, &mut run, a, "mantra_akasha_note", json!({"text": "fourth", "urgent": true}));
        ctx.set_idle_secs(b, 0);
        *run.akasha_live.pushed.get_mut(&b).unwrap() -= Duration::from_secs(21);
        run.akasha_tick(&mut ctx);
        assert_eq!(pushes(&ctx, b).len(), 2);
        assert!(run.akasha_live.outbox.is_empty());
        // nor does one whose turn already yielded with mantra_wait
        ctx.agents.get_mut(&b).unwrap().turn_active = true;
        run.yielded.insert(b);
        call(&mut ctx, &mut run, a, "mantra_akasha_note", json!({"text": "fifth", "urgent": true}));
        *run.akasha_live.pushed.get_mut(&b).unwrap() -= Duration::from_secs(21);
        run.akasha_tick(&mut ctx);
        assert_eq!(pushes(&ctx, b).len(), 2);
        assert!(pushes(&ctx, c).is_empty(), "the idle member was never prompted");
        let _ = std::fs::remove_dir_all(&run.dir);
    }

    /// A member being stopped — by hand (the interrupt is on its way, the turn not over yet) or
    /// paused by the orchestrator — is not busy for a push: the steer would land in the turn
    /// being stopped, and a message clears the hand stop, putting the agent back to work.
    #[test]
    fn a_member_being_stopped_or_paused_gets_no_push() {
        let mut ctx = TestCtx::new();
        let mut run = run_of(grouped("urgent", true, 16_384), "stopping");
        let a = member(&mut ctx, &mut run, "p1-a");
        let b = member(&mut ctx, &mut run, "p1-b");
        let c = member(&mut ctx, &mut run, "p1-c");
        call(&mut ctx, &mut run, a, "mantra_akasha_note", json!({"text": "queued before the stop", "urgent": true}));
        assert!(run.akasha_live.outbox.contains_key(&b) && run.akasha_live.outbox.contains_key(&c));
        ctx.agents.get_mut(&b).unwrap().stopped_by_user = true;
        let wc = run.worker_idx(c).unwrap();
        run.workers[wc].paused = true;
        assert!(ctx.agents[&b].busy() && ctx.agents[&c].busy(), "both turns are still open");
        run.akasha_tick(&mut ctx);
        assert!(pushes(&ctx, b).is_empty() && pushes(&ctx, c).is_empty(), "what waited is not delivered after the stop");
        assert!(run.akasha_live.outbox.is_empty());
        call(&mut ctx, &mut run, a, "mantra_akasha_note", json!({"text": "noted during the stop", "urgent": true}));
        assert!(run.akasha_live.outbox.is_empty(), "nor queued for them");
        assert!(ctx.agents[&b].stopped_by_user, "the hand stop stands");
        // talked to again (the stop is over) and resumed: pushes reach them as before
        ctx.agents.get_mut(&b).unwrap().stopped_by_user = false;
        run.workers[wc].paused = false;
        call(&mut ctx, &mut run, a, "mantra_akasha_note", json!({"text": "after", "urgent": true}));
        run.akasha_tick(&mut ctx);
        assert_eq!((pushes(&ctx, b), pushes(&ctx, c)), (vec!["[akasha · p1-a] after".to_string()], vec!["[akasha · p1-a] after".to_string()]));
        let _ = std::fs::remove_dir_all(&run.dir);
    }

    /// A push goes as the note reads when it is delivered: amended in the window, with its new
    /// text; forgotten (retracted as wrong), not at all — and with nothing left, no push.
    #[test]
    fn a_push_goes_as_the_note_reads_at_delivery() {
        let mut ctx = TestCtx::new();
        let mut run = run_of(grouped("urgent", true, 16_384), "push-now");
        let a = member(&mut ctx, &mut run, "p1-a");
        let b = member(&mut ctx, &mut run, "p1-b");
        call(&mut ctx, &mut run, a, "mantra_akasha_note", json!({"text": "first", "urgent": true}));
        run.akasha_tick(&mut ctx);
        assert_eq!(pushes(&ctx, b).len(), 1, "the window is open now");
        call(&mut ctx, &mut run, a, "mantra_akasha_note", json!({"text": "Config::load returns Option", "urgent": true}));
        call(&mut ctx, &mut run, a, "mantra_akasha_note", json!({"text": "the tests live in tests/ (wrong)", "urgent": true}));
        call(&mut ctx, &mut run, a, "mantra_akasha_amend", json!({"id": 2, "text": "Config::load returns Result"}));
        call(&mut ctx, &mut run, a, "mantra_akasha_forget", json!({"id": 3}));
        *run.akasha_live.pushed.get_mut(&b).unwrap() -= Duration::from_secs(21);
        run.akasha_tick(&mut ctx);
        assert_eq!(pushes(&ctx, b)[1], "[akasha · p1-a] Config::load returns Result");
        call(&mut ctx, &mut run, a, "mantra_akasha_note", json!({"text": "oops", "urgent": true}));
        call(&mut ctx, &mut run, a, "mantra_akasha_forget", json!({"id": 4}));
        *run.akasha_live.pushed.get_mut(&b).unwrap() -= Duration::from_secs(21);
        let window = run.akasha_live.pushed[&b];
        run.akasha_tick(&mut ctx);
        assert_eq!(pushes(&ctx, b).len(), 2, "everything that waited was forgotten");
        assert!(run.akasha_live.outbox.is_empty());
        assert_eq!(run.akasha_live.pushed[&b], window, "and no window was spent on nothing");
        let _ = std::fs::remove_dir_all(&run.dir);
    }

    /// A push that lands while a done worker answers the orchestrator's follow-up asked nothing:
    /// the orchestrator still gets its answer, and the push reads as Mantra's, not the user's.
    #[test]
    fn a_push_into_a_follow_up_does_not_pass_for_the_users_message() {
        let mut ctx = TestCtx::new();
        let mut run = run_of(grouped("urgent", true, 16_384), "follow-up");
        let o = ctx.add_idle();
        run.orchestrator = Some(o);
        let a = member(&mut ctx, &mut run, "p1-a");
        let b = member(&mut ctx, &mut run, "p1-b");
        ctx.agents.get_mut(&a).unwrap().final_message = Some("STATUS: done".into());
        run.on_turn_done(&mut ctx, a, "completed", None, None);
        assert_eq!(run.workers[0].state, WState::Done);
        // the orchestrator asks the done worker something; it is answering when p1-b's urgent note comes
        ctx.agents.get_mut(&a).unwrap().push_user("[from the orchestrator] which file holds the parser?");
        assert!(ctx.agents[&a].busy());
        call(&mut ctx, &mut run, b, "mantra_akasha_note", json!({"text": "the parser moved to src/parse.rs", "urgent": true}));
        run.akasha_tick(&mut ctx);
        let push = pushes(&ctx, a).pop().expect("pushed into the answering turn");
        // what `app::prompt_agent` echoes into the transcript
        ctx.agents.get_mut(&a).unwrap().push_user(&push);
        assert_eq!(ctx.agents[&a].tail_lines(1), vec!["⇢ akasha · p1-b".to_string()], "Mantra's line, not one the user typed");
        run.on_turn_done(&mut ctx, a, "completed", None, None);
        let last = pulse_texts(&run).into_iter().rev().find(|l| l.contains("p1-a done")).unwrap_or_default();
        assert!(last.contains("p1-a done again in"), "{last}");
        let heard = ctx.prompts.iter().filter(|(to, t)| *to == o && t.contains("p1-a answered the follow-up")).count() + run.orch_inbox.iter().filter(|t| t.contains("p1-a answered the follow-up")).count();
        assert_eq!(heard, 1, "{:?} {:?}", ctx.prompts, run.orch_inbox);
        let _ = std::fs::remove_dir_all(&run.dir);
    }

    /// The manager's own retries, which `manager_tick` sends ahead of `tick`, end with the
    /// notes too when its role is in a group — like every member's retry prompt.
    #[test]
    fn a_grouped_managers_retries_carry_the_notes() {
        let mut ctx = TestCtx::new();
        let mut pattern = grouped("urgent", true, 16_384);
        pattern.groups[0].roles.push("manager".into());
        let mut run = run_of(pattern, "manager");
        let m = ctx.add_idle();
        run.manager = Some(m);
        assert_eq!(run.group_of(m), Some("builders"));
        run.akasha.get_mut("builders").unwrap().note("p1-a", "models live in src/models", false, 0).unwrap();
        run.continue_queue.push((m, Instant::now(), "[mantra:retry] Your previous turn failed (x). Continue exactly where you left off.".into()));
        run.manager_tick(&mut ctx);
        let retry = ctx.prompts.iter().find(|(x, t)| *x == m && t.starts_with("[mantra:retry]")).map(|(_, t)| t.clone()).expect("the retry went out");
        assert!(retry.contains("\n\n## Shared notes (akasha · builders)\n## akasha · builders · 1 note"), "{retry}");
        let _ = std::fs::remove_dir_all(&run.dir);
    }

    #[test]
    fn push_all_sends_every_note_and_push_none_sends_none() {
        for (push, urgent, expect) in [("all", false, 1), ("none", true, 0), ("urgent", false, 0)] {
            let mut ctx = TestCtx::new();
            let mut run = run_of(grouped(push, true, 16_384), &format!("push-{push}"));
            let a = member(&mut ctx, &mut run, "p1-a");
            let b = member(&mut ctx, &mut run, "p1-b");
            call(&mut ctx, &mut run, a, "mantra_akasha_note", json!({"text": "a fact", "urgent": urgent}));
            run.akasha_tick(&mut ctx);
            assert_eq!(pushes(&ctx, b).len(), expect, "push = {push}, urgent = {urgent}");
            let _ = std::fs::remove_dir_all(&run.dir);
        }
    }

    #[test]
    fn the_conductor_notes_what_members_finished_failed_and_changed() {
        let mut ctx = TestCtx::new();
        let mut run = run_of(grouped("all", true, 16_384), "auto");
        let o = ctx.add_idle();
        run.orchestrator = Some(o);
        let a = member(&mut ctx, &mut run, "p1-a");
        let b = member(&mut ctx, &mut run, "p1-b");
        if let Some(ag) = ctx.agents.get_mut(&a) {
            ag.final_message = Some("Implemented it.\n\nSTATUS: done\nSUMMARY: added Config with a loader; tests pass".into());
            for f in ["src/a.rs", "src/b.rs", "src/c.rs", "src/d.rs", "src/e.rs"] {
                ag.files.insert(f.into(), Default::default());
            }
        }
        run.on_turn_done(&mut ctx, a, "completed", None, None);
        assert_eq!(texts(&run), vec!["[p1-a · done] added Config with a loader; tests pass", "[p1-a · changed] src/a.rs, src/b.rs, src/c.rs, +2 more"]);
        assert!(run.akasha["builders"].entries.iter().all(|e| e.author == "mantra" && !e.urgent));
        run.akasha_tick(&mut ctx);
        assert!(pushes(&ctx, b).is_empty(), "the Conductor's entries are never pushed, even with push = all");
        // a follow-up the done worker answers is not a second completion
        if let Some(ag) = ctx.agents.get_mut(&a) {
            ag.push_user("[from the orchestrator] which file?");
            ag.final_message = Some("STATUS: done\nSUMMARY: src/config.rs".into());
        }
        run.on_turn_done(&mut ctx, a, "completed", None, None);
        assert_eq!(texts(&run).len(), 2);
        // failed: the error's first line and the attempt; blocked: the report's head
        run.on_turn_done(&mut ctx, b, "failed", Some("the build script exited 101\nstack backtrace".into()), None);
        assert_eq!(texts(&run)[2], "[p1-b · failed, attempt 1] the build script exited 101");
        let c = member(&mut ctx, &mut run, "p1-c");
        if let Some(ag) = ctx.agents.get_mut(&c) {
            ag.final_message = Some("STATUS: blocked\nSUMMARY: the spec names no database".into());
        }
        run.on_turn_done(&mut ctx, c, "completed", None, None);
        assert_eq!(texts(&run)[3], "[p1-c · failed, attempt 1] blocked: the spec names no database");
        assert_eq!(texts(&run).len(), 4);
        assert!(!pulse_texts(&run).iter().any(|l| l.starts_with("akasha ·")), "they mirror lines the journal already has");
        let _ = std::fs::remove_dir_all(&run.dir);
    }

    #[test]
    fn a_full_block_drops_the_conductors_entries_and_says_so_once() {
        let mut ctx = TestCtx::new();
        let mut run = run_of(grouped("urgent", true, 1024), "full");
        let o = ctx.add_idle();
        run.orchestrator = Some(o);
        let a = member(&mut ctx, &mut run, "p1-a");
        let (r, ok) = call(&mut ctx, &mut run, a, "mantra_akasha_note", json!({"text": "x".repeat(950)}));
        assert!(ok, "{r}");
        for (who, id) in [(a, "p1-a"), (member(&mut ctx, &mut run, "p1-b"), "p1-b")] {
            if let Some(ag) = ctx.agents.get_mut(&who) {
                ag.final_message = Some(format!("STATUS: done\nSUMMARY: {id} finished a task whose summary does not fit in what is left"));
            }
            run.on_turn_done(&mut ctx, who, "completed", None, None);
        }
        assert_eq!(run.akasha["builders"].entries.len(), 1, "the agent's note stays, the Conductor's entries are dropped");
        let full: Vec<String> = pulse_texts(&run).into_iter().filter(|l| l.contains("is full")).collect();
        assert_eq!(full, vec!["akasha · builders is full — Mantra's own entries are dropped until notes are forgotten".to_string()], "once, not per drop");
        // room again: they come back
        call(&mut ctx, &mut run, a, "mantra_akasha_forget", json!({"id": 1}));
        let c = member(&mut ctx, &mut run, "p1-c");
        if let Some(ag) = ctx.agents.get_mut(&c) {
            ag.final_message = Some("STATUS: done\nSUMMARY: fits now".into());
        }
        run.on_turn_done(&mut ctx, c, "completed", None, None);
        assert_eq!(texts(&run), vec!["[p1-c · done] fits now"]);
        let _ = std::fs::remove_dir_all(&run.dir);
    }

    #[test]
    fn the_blocks_survive_a_save_and_a_resume() {
        let mut run = run_of(grouped("urgent", true, 16_384), "state");
        run.akasha.get_mut("builders").unwrap().note("p1-a", "kept across a restart", true, 7).unwrap();
        run.save_state();
        let st = RunState::load(&run.dir).unwrap();
        assert_eq!(st.akasha, run.akasha);
        let back = Run::from_state(&st, run.pattern.clone(), None, run.dir.clone());
        assert_eq!(back.akasha, run.akasha, "the restored block is kept, not rebuilt empty");
        assert_eq!(back.group_of_role("worker-small"), Some("builders"));
        let _ = std::fs::remove_dir_all(&run.dir);
    }
}

/// The reported bug: a done worker the user messaged finished again and the pulse read
/// "✓ p1-a done in …" twice, the orchestrator woken for a second completion. The second
/// one says what it is, and the orchestrator hears about the task once.
#[test]
fn a_done_worker_finishing_again_is_a_follow_up_not_a_second_completion() {
    use crate::engine::plan::{Phase, Plan};
    let mut ctx = TestCtx::new();
    let o = ctx.add_idle();
    let a = ctx.add_busy();
    let b = ctx.add_busy();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    let task = |id: &str| Task { id: id.into(), role: "worker-small".into(), ..Default::default() };
    run.plan = Some(Plan { phases: vec![Phase { id: "p1".into(), tasks: vec![task("p1-a"), task("p1-b")], ..Default::default() }], ..Default::default() });
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
    run.orchestrator = Some(o);
    run.workers.push(mk_worker("p1-a", "worker-small", a, WState::Running));
    run.workers.push(mk_worker("p1-b", "worker-small", b, WState::Running));
    ctx.agents.get_mut(&a).unwrap().final_message = Some("STATUS: done".into());
    run.on_turn_done(&mut ctx, a, "completed", None, None);
    assert!(run.pulse.back().unwrap().text.contains("p1-a done in"), "{:?}", run.pulse.back().map(|p| &p.text));
    ctx.agents.get_mut(&a).unwrap().push_user("also add a test for the empty case");
    run.on_turn_done(&mut ctx, a, "completed", None, None);
    assert!(run.pulse.back().unwrap().text.contains("p1-a done again (after your message) in"), "{:?}", run.pulse.back().map(|p| &p.text));
    // `@p1-a …` from the stage arrives as "[from the user] …": the user's message too
    ctx.agents.get_mut(&a).unwrap().push_user("[from the user] and document it");
    run.on_turn_done(&mut ctx, a, "completed", None, None);
    assert!(run.pulse.back().unwrap().text.contains("p1-a done again (after your message) in"), "{:?}", run.pulse.back().map(|p| &p.text));
    let heard = |run: &Run, ctx: &TestCtx, what: &str| ctx.prompts.iter().filter(|(to, t)| *to == o && t.contains(what)).count() + run.orch_inbox.iter().filter(|t| t.contains(what)).count();
    assert_eq!(heard(&run, &ctx, "p1-a finished"), 1, "{:?}", ctx.prompts);
    assert_eq!(heard(&run, &ctx, "p1-a answered"), 0, "the user's side conversation wakes nobody");
    // the orchestrator's own follow-up to a done worker gets its answer — as an answer
    ctx.agents.get_mut(&a).unwrap().push_user("[from the orchestrator] which file holds the parser?");
    run.on_turn_done(&mut ctx, a, "completed", None, None);
    assert!(run.pulse.back().unwrap().text.contains("p1-a done again in"), "{:?}", run.pulse.back().map(|p| &p.text));
    assert_eq!(heard(&run, &ctx, "p1-a finished"), 1, "{:?}", ctx.prompts);
    assert_eq!(heard(&run, &ctx, "p1-a answered the follow-up"), 1, "{:?} {:?}", ctx.prompts, run.orch_inbox);
}

#[test]
fn halt_interrupts_busy_agents_and_resume_reprompts_them() {
    let mut ctx = TestCtx::new();
    let busy = ctx.add_busy();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    run.orchestrator = Some(busy); // must be a run agent to be found & interrupted
    run.halt(&mut ctx, HaltReason::ProviderRejected, Some(busy), "qa (sol via zai): Unexpected message role.".into());
    assert!(run.halted());
    assert!(ctx.interrupted.contains(&busy), "the busy agent must be interrupted");
    assert!(run.alerts.iter().any(|a| a.contains("Unexpected message role")));
    assert_eq!(run.halt.as_ref().unwrap().reason, HaltReason::ProviderRejected);

    run.resume(&mut ctx);
    assert!(!run.halted());
    assert!(ctx.prompts.iter().any(|(a, t)| *a == busy && t.contains("[mantra:resume]")), "resume must re-prompt the halted agent");
}

#[test]
fn user_pause_is_quiet_but_still_a_halt() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    run.toggle_pause(&mut ctx);
    assert!(run.halted());
    assert_eq!(run.halt.as_ref().unwrap().reason, HaltReason::User);
    assert!(run.alerts.is_empty(), "a manual pause is not an alert");
    run.toggle_pause(&mut ctx);
    assert!(!run.halted(), "space toggles a manual pause back off");
}

#[test]
fn run_clock_counts_active_time_only() {
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    run.set_clock(0, Some(1000));
    assert_eq!(run.elapsed_at(1005).as_secs(), 5);
    // a 10s pause inside a 30s run → 20s of active time
    run.clock_stop_at(1010);
    assert_eq!(run.elapsed_at(1015).as_secs(), 10, "frozen while halted");
    run.clock_start_at(1020);
    assert_eq!(run.elapsed_at(1030).as_secs(), 20);
    // a stopped run is frozen the same way, and a resume does not restart it
    run.stage = Stage::Done;
    run.clock_stop_at(1030);
    run.clock_start_at(1100);
    assert_eq!(run.elapsed_at(1200).as_secs(), 20, "a finished run keeps its duration");
    // stopping twice is harmless
    run.clock_stop_at(1300);
    assert_eq!(run.elapsed_at(1400).as_secs(), 20);
}

#[test]
fn user_pause_freezes_the_run_clock() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    run.set_clock(12, Some(crate::util::unix_secs()));
    run.toggle_pause(&mut ctx);
    assert!(run.active_since.is_none(), "space stops the clock");
    assert_eq!(run.elapsed().as_secs(), 12);
    run.toggle_pause(&mut ctx);
    assert!(run.active_since.is_some(), "resume starts a new active span");
    assert_eq!(run.active_secs, 12);
}

#[test]
fn halt_and_resume_bracket_an_active_span() {
    let mut ctx = TestCtx::new();
    let busy = ctx.add_busy();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    run.orchestrator = Some(busy);
    run.halt(&mut ctx, HaltReason::AgentTurnFailed, Some(busy), "boom".into());
    assert!(run.active_since.is_none());
    run.resume(&mut ctx);
    assert!(run.active_since.is_some());
    run.fail_run(&mut ctx, "gave up".into());
    assert!(run.active_since.is_none(), "a stopped run is frozen");
}

/// The built-in pattern with its manager removed: the chain of command then ends at the
/// planner, which is what the planner-escalation tests below pin down.
fn no_manager() -> Pattern {
    let mut p = Pattern::builtin();
    p.flow.manager = String::new();
    p
}

fn one_task_phase() -> Plan {
    Plan { phases: vec![Phase { id: "p1".into(), name: "p1".into(), tasks: vec![Task { id: "t1".into(), role: "worker-small".into(), ..Default::default() }], ..Default::default() }], ..Default::default() }
}

/// Let the fix worker of gate round `round` do its job the way the engine sees it: its
/// workspace job returns, it starts, its turn completes (which re-runs the checks, and with no
/// checks and no workspace goes straight on to the next QA round).
fn finish_fix(ctx: &mut TestCtx, run: &mut Run, round: u32) {
    let id = format!("fix-{round}");
    let attempt = run.workers.iter().rev().find(|w| w.task.id == id).map(|w| w.attempt).expect("a fix worker was created");
    run.on_job(ctx, JobTag::WorkerWt { task: id.clone(), attempt }, JobOut::Wt(Ok((PathBuf::from("."), String::new()))));
    let agent = run.workers.iter().rev().find(|w| w.task.id == id).and_then(|w| w.agent).expect("the fix worker started");
    ctx.agents.get_mut(&agent).unwrap().final_message = Some("STATUS: done\nSUMMARY: fixed what was listed".into());
    run.on_turn_done(ctx, agent, "completed", None, None);
}

/// A QA agent's turn ends with this verdict.
fn gate_says(ctx: &mut TestCtx, run: &mut Run, gate: AgentId, text: &str) {
    ctx.agents.get_mut(&gate).unwrap().final_message = Some(text.into());
    run.on_turn_done(ctx, gate, "completed", None, None);
}

/// Two identical failing QA verdicts with a fix in between: the loop protection halts. Returns
/// the second (fresh) QA agent. `run` must be at gate round 1 with `gate` as its agent.
fn drive_gate_to_halt(ctx: &mut TestCtx, run: &mut Run, gate: AgentId, blocked: &str) -> AgentId {
    gate_says(ctx, run, gate, blocked);
    assert!(!run.halted(), "the first blocked round goes to a fix worker, it does not halt");
    finish_fix(ctx, run, 1);
    let second = run.gate_agent.expect("round 2 has a fresh QA agent");
    assert_ne!(second, gate, "every round is a fresh agent");
    gate_says(ctx, run, second, blocked);
    assert!(run.halted());
    second
}

// WP7.2 watchdog tests ---------------------------------------------------------------------

#[test]
fn watchdog_nudges_then_respawns_then_wakes_planner() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
    let planner = ctx.add_idle();
    let orch = ctx.add_idle();
    run.planner = Some(planner);
    run.orchestrator = Some(orch);
    run.plan = Some(one_task_phase());
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };

    // 90s idle (default watchdog_seconds): nudge the orchestrator itself.
    ctx.set_idle_secs(orch, 90);
    run.watchdog_tick(&mut ctx);
    assert!(ctx.prompts.iter().any(|(a, t)| *a == orch && t.contains("[mantra:watchdog]")), "expected a nudge at 90s, got {:?}", ctx.prompts);

    // 240s idle (default watchdog_escalate_seconds): respawn the orchestrator.
    ctx.prompts.clear();
    ctx.set_idle_secs(orch, 240);
    run.watchdog_tick(&mut ctx);
    let new_orch = run.orchestrator.expect("orchestrator still set after a respawn");
    assert_ne!(new_orch, orch, "240s idle must respawn the orchestrator (a new agent id)");
    assert!(ctx.prompts.iter().any(|(a, t)| *a == new_orch && t.contains("[mantra:respawn]")));

    // 480s idle (2x escalate) on the respawned orchestrator: wake the planner instead.
    ctx.prompts.clear();
    ctx.set_idle_secs(new_orch, 480);
    run.watchdog_tick(&mut ctx);
    assert!(ctx.prompts.iter().any(|(a, t)| *a == planner && t.contains("did not act")), "expected the planner to be woken at 480s, got {:?}", ctx.prompts);
}

#[test]
fn watchdog_leaves_a_legitimately_sleeping_orchestrator_alone() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    let orch = ctx.add_idle();
    let worker_agent = ctx.add_busy();
    run.orchestrator = Some(orch);
    run.plan = Some(one_task_phase());
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
    run.workers.push(mk_worker("t1", "worker-small", worker_agent, WState::Running));
    ctx.set_idle_secs(orch, 500); // long idle, but a worker is running — this is legitimate
    run.watchdog_tick(&mut ctx);
    assert!(ctx.prompts.is_empty(), "an orchestrator asleep in mantra_wait while a worker runs must not be nudged: {:?}", ctx.prompts);
}

#[test]
fn watchdog_does_not_nudge_an_idle_worker_waiting_for_its_swarm() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    let worker = ctx.add_idle();
    run.plan = Some(one_task_phase());
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
    run.workers.push(mk_worker("t1", "worker-small", worker, WState::Running));
    ctx.set_idle_secs(worker, 500);
    ctx.agents.get_mut(&worker).unwrap().swarm_waiting = true;
    run.watchdog_tick(&mut ctx);
    assert!(ctx.prompts.iter().all(|(a, _)| *a != worker));
    assert!(run.watchdog_idle(worker).is_none());
}

/// The case the ladder structurally cannot see: a planner that is *busy* — connected, turn
/// live — but has emitted nothing for minutes. From the outside that is indistinguishable
/// from a crash, so it earns one pulse line and nothing else: no nudge, no respawn, no halt.
#[test]
fn a_busy_but_silent_agent_is_journalled_once_per_spell() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    let planner = ctx.add_busy();
    ready(&mut ctx, planner);
    run.planner = Some(planner);
    run.stage = Stage::Planning;
    let past_stall = run.pattern.settings.stall_minutes * 60 + 30;
    let lines = |r: &Run| r.pulse.iter().filter(|p| p.text.contains("no output for")).count();

    ctx.set_silent_secs(planner, past_stall);
    run.watchdog_tick(&mut ctx);
    assert_eq!(lines(&run), 1, "a long silent planning turn must say so once: {:?}", run.pulse.iter().map(|p| p.text.clone()).collect::<Vec<_>>());
    assert!(ctx.prompts.is_empty(), "a working agent is never nudged for being quiet: {:?}", ctx.prompts);
    assert!(!run.halted(), "quiet is informational — it must never stop the run");

    // Still silent on the next tick: the feed must not fill up with the same observation.
    run.watchdog_tick(&mut ctx);
    assert_eq!(lines(&run), 1, "one line per spell, not one per tick");

    // It speaks, then goes quiet again — a second spell, reported afresh.
    ctx.agents.get_mut(&planner).unwrap().last_event = Instant::now();
    run.watchdog_tick(&mut ctx);
    ctx.set_silent_secs(planner, past_stall);
    run.watchdog_tick(&mut ctx);
    assert_eq!(lines(&run), 2, "a later silent spell is news again");
    assert!(ctx.prompts.is_empty(), "{:?}", ctx.prompts);
    assert!(!run.halted());
}

/// An agent idle for 40 minutes and just prompted has a 40-minute-old `last_event`; that is not
/// silence of the turn that just began.
#[test]
fn a_freshly_prompted_agent_is_not_silent_but_a_stale_turn_is() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    let planner = ctx.add_idle();
    ready(&mut ctx, planner);
    run.planner = Some(planner);
    run.stage = Stage::Planning;
    let stall_secs = run.pattern.settings.stall_minutes * 60;
    let lines = |r: &Run| r.pulse.iter().filter(|p| p.text.contains("no output for")).count();

    ctx.set_idle_secs(planner, 40 * 60);
    {
        let ag = ctx.agents.get_mut(&planner).unwrap();
        ag.awaiting_start = true;
        ag.turn_wait_since = Some(Instant::now());
    }
    run.watchdog_tick(&mut ctx);
    assert_eq!(lines(&run), 0, "{:?}", run.pulse.iter().map(|p| p.text.clone()).collect::<Vec<_>>());

    {
        let ag = ctx.agents.get_mut(&planner).unwrap();
        ag.turn_wait_since = Some(Instant::now() - Duration::from_secs(stall_secs + 60));
    }
    run.watchdog_tick(&mut ctx);
    assert_eq!(lines(&run), 1);
}

#[test]
fn f3_loop_protection_respawns_a_repeatedly_prompted_idle_worker() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    let orch = ctx.add_idle();
    // "idle" per the F3 rule: the turn ended (interrupted) but the worker is still `Running`.
    let worker_agent = ctx.add_idle();
    run.orchestrator = Some(orch);
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
    run.workers.push(mk_worker("t1", "worker-small", worker_agent, WState::Running));

    for n in 0..2 {
        let (msg, ok) = run.handle_tool(&mut ctx, orch, "mantra_prompt", &json!({"agent": "t1", "message": "keep going"}));
        assert!(ok, "prompt #{n} should be forwarded normally: {msg}");
        assert!(msg.starts_with("sent to"), "prompt #{n}: {msg}");
    }
    let (msg, ok) = run.handle_tool(&mut ctx, orch, "mantra_prompt", &json!({"agent": "t1", "message": "keep going"}));
    assert!(ok, "{msg}");
    assert!(msg.contains("respawned"), "the third idle prompt within 5 minutes must respawn the worker instead of forwarding: {msg}");
    assert_eq!(run.workers.len(), 2, "the F3 respawn must produce a new worker attempt");
    assert_eq!(run.workers[0].state, WState::Cancelled);
    assert_eq!(run.workers[1].attempt, 2);
}

#[test]
fn tool_guard_refuses_steering_a_done_worker_outside_orchestrating() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    let orch = ctx.add_idle();
    let worker_agent = ctx.add_idle();
    run.orchestrator = Some(orch);
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Gate { round: 1 } }; // merging/gating, not Orchestrating
    run.workers.push(mk_worker("t1", "worker-small", worker_agent, WState::Done));

    let (msg, ok) = run.handle_tool(&mut ctx, orch, "mantra_prompt", &json!({"agent": "t1", "message": "hi"}));
    assert!(!ok);
    assert!(msg.contains("wait for the handoff"), "{msg}");

    let (msg, ok) = run.handle_tool(&mut ctx, orch, "mantra_interrupt", &json!({"agent": "t1"}));
    assert!(!ok, "{msg}");
}

#[test]
fn respawn_planner_during_review_keeps_stage_review() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    let planner = ctx.add_idle();
    run.planner = Some(planner);
    run.stage = Stage::Review;
    run.plan = Some(Plan { title: "x".into(), ..Default::default() });
    run.plan_version = 1;

    run.respawn(&mut ctx, planner, None).unwrap();

    assert_eq!(run.stage, Stage::Review, "respawning the planner mid-review must not change the stage");
    let new_planner = run.planner.expect("planner still set after respawn");
    assert_ne!(new_planner, planner);
    assert!(ctx.prompts.iter().any(|(a, t)| *a == new_planner && t.contains("[mantra:respawn]")));
}

/// `Stage::Review` (right after the plan is submitted) has no current phase for
/// `current_phase()` to show — the respawned planner must still get the plan it just wrote,
/// not an empty `Current phase:` block it can't recover from.
#[test]
fn respawn_planner_during_review_attaches_the_plan_not_an_empty_phase() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    let planner = ctx.add_idle();
    run.planner = Some(planner);
    run.stage = Stage::Review;
    run.plan = Some(one_task_phase());
    run.plan_version = 1;

    run.respawn(&mut ctx, planner, None).unwrap();

    let new_planner = run.planner.expect("planner still set after respawn");
    let prompt = ctx.prompts.iter().find(|(a, t)| *a == new_planner && t.contains("[mantra:respawn]")).map(|(_, t)| t.clone()).expect("planner briefed");
    assert!(prompt.contains("\"p1\""), "the plan just submitted must be attached, not an empty phase block: {prompt}");
    assert!(!prompt.contains("Current phase:\n```json\n\n```"), "{prompt}");
}

/// The planner can read the full plan on demand — the tool the respawn prompt now points to
/// instead of promising something no tool ever delivered.
#[test]
fn mantra_read_plan_returns_the_full_plan() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    let planner = ctx.add_idle();
    run.planner = Some(planner);
    let (msg, ok) = run.handle_tool(&mut ctx, planner, "mantra_read_plan", &json!({}));
    assert!(!ok, "{msg}");
    let mut plan = one_task_phase();
    plan.phases.push(Phase { id: "p2".into(), name: "two".into(), tasks: vec![Task { id: "t2".into(), role: "worker-small".into(), ..Default::default() }], ..Default::default() });
    run.plan = Some(plan);

    let (msg, ok) = run.handle_tool(&mut ctx, planner, "mantra_read_plan", &json!({}));
    assert!(ok, "{msg}");
    assert!(msg.contains("\"p1\"") && msg.contains("\"p2\""), "every phase must be in it, not just the current one: {msg}");
}

/// The finale steps used `spawn_gate_at_round`'s sandbox-aware note too; a read-only finale
/// role must be told it cannot write, exactly like a per-phase gate.
#[test]
fn start_finale_gives_a_read_only_role_the_sandbox_note() {
    let mut ctx = TestCtx::new();
    let mut pattern = Pattern::builtin();
    pattern.roles.get_mut("qa-heavy").unwrap().sandbox = "read-only".into();
    let mut run = Run::new(PathBuf::from("."), pattern, "test".into());
    run.plan = Some(one_task_phase());

    run.start_finale(&mut ctx, 0);

    let agent = run.finale_agent.expect("a finale agent must be spawned");
    let instructions = ctx.spawned_instructions.iter().find(|(id, _)| *id == agent).map(|(_, i)| i.clone()).expect("instructions captured");
    assert!(instructions.contains("Your role cannot write files"), "{instructions}");
}

/// A respawned planner used to be handed the whole plan (every phase, every task) — exactly
/// the huge context a respawn is meant to save. It gets only the current phase now, plus
/// `mantra_read_phase` for live task states; the full plan is one call away if it needs it.
#[test]
fn respawn_planner_sends_only_the_current_phase() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    let planner = ctx.add_idle();
    run.planner = Some(planner);
    let mut plan = one_task_phase();
    plan.phases.push(Phase { id: "p2".into(), name: "two".into(), tasks: vec![Task { id: "t2".into(), role: "worker-small".into(), ..Default::default() }], ..Default::default() });
    run.plan = Some(plan);
    run.plan_version = 3;
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };

    run.respawn(&mut ctx, planner, None).unwrap();

    let new_planner = run.planner.expect("planner still set after respawn");
    let prompt = ctx.prompts.iter().find(|(a, t)| *a == new_planner && t.contains("[mantra:respawn]")).map(|(_, t)| t.clone()).expect("planner briefed");
    assert!(prompt.contains("plan v3"), "{prompt}");
    assert!(prompt.contains("\"p1\""), "the current phase must be included: {prompt}");
    assert!(!prompt.contains("\"p2\""), "a later phase must not be attached — that is what mantra_read_phase / mantra_revise_plan are for: {prompt}");
    assert!(prompt.contains("mantra_read_phase"), "{prompt}");
}

// Worker classes ------------------------------------------------------------------------------

/// One phase of `big` worker-big tasks, then `swarm` worker-swarm tasks, disjoint scopes.
fn class_phase(big: usize, swarm: usize) -> Plan {
    let task = |id: String, role: &str| Task { scope: vec![format!("src/{id}/**")], id, role: role.into(), prompt: "do one small thing well".into(), ..Default::default() };
    let mut tasks: Vec<Task> = (0..big).map(|i| task(format!("b{i}"), "worker-big")).collect();
    tasks.extend((0..swarm).map(|i| task(format!("s{i}"), "worker-swarm")));
    Plan { phases: vec![Phase { id: "p1".into(), name: "p1".into(), tasks, ..Default::default() }], ..Default::default() }
}

/// A run in phase 1 of `plan` with every task of it spawned, in plan order; its directory is
/// a scratch one of its own (`tag`).
fn spawned_phase(ctx: &mut TestCtx, plan: Plan, tag: &str) -> Run {
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    run.dir = scratch_dir(tag);
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
    for t in plan.phases[0].tasks.clone() {
        run.spawn_task(ctx, t, None, None);
    }
    run.plan = Some(plan);
    run
}

fn holding(run: &Run, role: &str) -> usize {
    run.workers.iter().filter(|w| w.task.role == role && holds_slot(w)).count()
}

fn queued(run: &Run) -> Vec<&str> {
    run.workers.iter().filter(|w| w.state == WState::Queued).map(|w| w.task.id.as_str()).collect()
}

/// Each class fills its own slots: under `max_parallel` = 4 and worker-swarm's own 16, the big
/// workers take the shared pool (all 3 of them, or 4 of 6) and 16 swarm tasks run beside them;
/// what is left queues in its own class, and the journal names that class's cap.
#[test]
fn each_worker_class_fills_its_own_slots() {
    for (big, running_big, queued_big) in [(3, 3, 0), (6, 4, 2)] {
        let mut ctx = TestCtx::new();
        let run = spawned_phase(&mut ctx, class_phase(big, 20), &format!("classes-fill-{big}"));
        assert_eq!((run.pattern.settings.max_parallel, run.pattern.cap_of("worker-swarm")), (4, 16));
        assert_eq!((holding(&run, "worker-big"), holding(&run, "worker-swarm")), (running_big, 16), "{big} big tasks: {running_big} + 16 at once");
        let q = queued(&run);
        assert_eq!(q.iter().filter(|t| t.starts_with('b')).count(), queued_big);
        assert_eq!(q.iter().filter(|t| t.starts_with('s')).copied().collect::<Vec<_>>(), vec!["s16", "s17", "s18", "s19"]);
        let said: Vec<&str> = run.pulse.iter().map(|p| p.text.as_str()).collect();
        assert!(said.contains(&"s16 queued (worker-swarm: 16 at once)"), "{said:?}");
        if queued_big > 0 {
            assert!(said.contains(&"b4 queued (max_parallel = 4)"), "{said:?}");
        }
        let _ = std::fs::remove_dir_all(&run.dir);
    }
}

/// `fill_slots` walks the whole queue in order and admits each task against its own class:
/// a queued swarm task behind a big one that has to wait still starts when a swarm slot frees,
/// and the big one starts when a slot of the pool does — never the other way round.
#[test]
fn a_queued_swarm_task_behind_a_waiting_big_one_starts() {
    let mut ctx = TestCtx::new();
    let mut run = spawned_phase(&mut ctx, class_phase(5, 17), "classes-queue");
    assert_eq!(queued(&run), vec!["b4", "s16"], "the big task queued first");
    let done = |run: &mut Run, id: &str| run.workers.iter_mut().find(|w| w.task.id == id).unwrap().state = WState::Done;
    done(&mut run, "s0");
    run.fill_slots(&mut ctx);
    assert_eq!(queued(&run), vec!["b4"], "the swarm task went past the waiting big one");
    assert_eq!((holding(&run, "worker-big"), holding(&run, "worker-swarm")), (4, 16));
    done(&mut run, "s1");
    run.fill_slots(&mut ctx);
    assert_eq!(queued(&run), vec!["b4"], "a free swarm slot is not the pool's");
    done(&mut run, "b0");
    run.fill_slots(&mut ctx);
    assert!(queued(&run).is_empty());
    assert_eq!((holding(&run, "worker-big"), holding(&run, "worker-swarm")), (4, 15));
    let _ = std::fs::remove_dir_all(&run.dir);
}

/// A fix worker (`fix-…`, a big or small worker) and a task naming a role the pattern does
/// not have count in the shared pool; `--parallel N` widens every class to N.
#[test]
fn the_shared_pool_takes_fix_workers_and_unknown_roles_and_parallel_widens_every_class() {
    let mut ctx = TestCtx::new();
    let mut run = spawned_phase(&mut ctx, class_phase(3, 0), "classes-pool");
    run.spawn_task(&mut ctx, Task { id: "fix-1".into(), role: "worker-small".into(), prompt: "fix it".into(), ..Default::default() }, None, None);
    run.spawn_task(&mut ctx, Task { id: "odd".into(), role: "worker-gone".into(), prompt: "do it".into(), ..Default::default() }, None, None);
    assert_eq!(queued(&run), vec!["odd"], "3 big + the fix worker fill the pool of 4");
    assert_eq!(run.slots_taken("worker-gone"), 4);
    let mut ctx = TestCtx::new();
    let mut p = Pattern::builtin();
    p.set_parallel(8);
    let mut run = Run::new(PathBuf::from("."), p, "test".into());
    run.dir = scratch_dir("classes-wide");
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
    for t in class_phase(10, 10).phases[0].tasks.clone() {
        run.spawn_task(&mut ctx, t, None, None);
    }
    assert_eq!((holding(&run, "worker-big"), holding(&run, "worker-swarm")), (8, 8), "--parallel 8: 8 in every class");
    let _ = std::fs::remove_dir_all(&run.dir);
}

/// The spawn line says how full the worker's class is, this one included — the width a run
/// actually reached, which the swarm e2e reads back from the journal.
#[test]
fn the_spawn_line_says_how_full_the_class_is() {
    let mut ctx = TestCtx::new();
    let mut run = spawned_phase(&mut ctx, class_phase(2, 3), "classes-spawn-line");
    for (i, id) in ["b0", "s0", "s1"].iter().enumerate() {
        run.on_job(&mut ctx, JobTag::WorkerWt { task: id.to_string(), attempt: 1 }, JobOut::Wt(Ok((PathBuf::from("."), format!("br{i}")))));
    }
    let said: Vec<&str> = run.pulse.iter().map(|p| p.text.as_str()).collect();
    for want in ["spawned b0 (worker-big · 2/4 at once)", "spawned s0 (worker-swarm · 3/16 at once)", "spawned s1 (worker-swarm · 3/16 at once)"] {
        assert!(said.contains(&want), "{want}: {said:?}");
    }
    let _ = std::fs::remove_dir_all(&run.dir);
}

/// The run brief lists each worker role with how many of it run at once — the swarm its own
/// 16, the others the pool they share — and the wider task limit of a swarm phase.
#[test]
fn the_run_brief_lists_each_worker_role_with_its_cap() {
    let run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    let brief = run.planning_prompt();
    for want in ["- worker-swarm — up to 16 at once: Many tiny, independent tasks at once (model luna, effort auto)", "- worker-big — up to 4 at once, shared with worker-small: ", "- worker-small — up to 4 at once, shared with worker-big: ", "Constraints: at most 8 tasks per phase (32 in a phase that uses worker-swarm); "] {
        assert!(brief.contains(want), "{want}\n{brief}");
    }
    assert!(!brief.contains("run at once;"), "the old single number is gone: {brief}");
    let mut p = Pattern::builtin();
    p.roles.retain(|n, _| n != "worker-swarm" && n != "worker-small");
    p.groups.clear();
    let brief = Run::new(PathBuf::from("."), p, "test".into()).planning_prompt();
    assert!(brief.contains("- worker-big — up to 4 at once: ") && brief.contains("Constraints: at most 8 tasks per phase; "), "{brief}");
    let manager = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into()).manager_brief("lead");
    assert!(manager.contains("tasks in parallel: 4 at once · worker-swarm 16;"), "{manager}");
}

// Stopping a run -------------------------------------------------------------------------------

/// `stop` ends the run for good: busy agents interrupted, every agent shut down, the workers
/// still queued or running cancelled, a halt and its alerts gone, `Failed(STOPPED_BY_USER)`
/// saved — "stopped" on disk — without a notification; a job still in flight starts nothing,
/// and the stopped run reopens read-only, spawning nothing.
#[test]
fn stop_shuts_down_every_agent_and_ends_the_run_stopped() {
    let mut ctx = TestCtx::new();
    let mut run = spawned_phase(&mut ctx, class_phase(5, 0), "stop");
    let (orch, idle_worker, busy_worker) = (ctx.add_busy(), ctx.add_idle(), ctx.add_busy());
    run.orchestrator = Some(orch);
    run.workers[0].agent = Some(idle_worker);
    run.workers[0].state = WState::Running;
    run.workers[1].agent = Some(busy_worker);
    run.workers[1].state = WState::Running;
    run.workers[2].state = WState::Done;
    run.halt(&mut ctx, HaltReason::AgentTurnFailed, Some(busy_worker), "the gateway is down".into());
    // the halt interrupted them; busy again, as an agent mid-turn when the user stops looks
    for a in [orch, busy_worker] {
        ctx.agents.get_mut(&a).unwrap().turn_active = true;
    }
    ctx.interrupted.clear();
    ctx.notified.clear();
    let spawns = ctx.spawns.len();
    run.stop(&mut ctx);
    assert_eq!(ctx.interrupted, vec![orch, busy_worker], "the busy ones are interrupted");
    let mut stopped = ctx.stopped.clone();
    stopped.sort();
    assert_eq!(stopped, vec![orch, idle_worker, busy_worker], "every agent of the run is shut down");
    let states: Vec<&str> = run.workers.iter().map(|w| worker_state_str(&w.state)).collect();
    assert_eq!(states, vec!["cancelled", "cancelled", "done", "cancelled", "cancelled"], "b3 and b4 were queued or preparing");
    assert!(!run.halted() && run.alerts.is_empty() && !run.is_active());
    assert_eq!(run.stage, Stage::Failed(STOPPED_BY_USER.into()));
    assert!(run.pulse.iter().any(|p| p.text == "run stopped by you"));
    assert!(ctx.notified.is_empty(), "no notification for the user's own key: {:?}", ctx.notified);
    let st = RunState::load(&run.dir).expect("saved");
    assert_eq!((st.stage_label(), st.finished(), st.halted.is_none()), ("stopped".to_string(), true, true));
    assert!(st.finished_unix.is_some());
    // a merge that was in flight does not start a gate in the stopped run
    run.on_job(&mut ctx, JobTag::Merge { phase: 0 }, JobOut::Merge(git::MergeResult { merged: vec!["b2".into()], conflicts: vec![], blocked: vec![], log: String::new() }));
    assert_eq!((ctx.spawns.len(), &run.stage), (spawns, &Stage::Failed(STOPPED_BY_USER.into())));
    // stopping twice changes nothing
    let lines = run.pulse.len();
    run.stop(&mut ctx);
    assert_eq!(run.pulse.len(), lines);
    // reopened from `/runs`: read-only, nothing spawned
    let mut ctx2 = TestCtx::new();
    let mut again = Run::from_state(&st, Pattern::builtin(), run.plan.clone(), run.dir.clone());
    again.resume_boot(&mut ctx2, &st);
    assert!(ctx2.spawns.is_empty() && !again.is_active());
    assert!(again.pulse.iter().any(|p| p.text.starts_with("you stopped this run — opened read-only")), "{:?}", again.pulse.iter().map(|p| p.text.clone()).collect::<Vec<_>>());
    let _ = std::fs::remove_dir_all(&run.dir);
}

#[test]
fn successful_handoff_remembers_the_done_agent_not_an_earlier_failed_attempt() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "completion".into());
    run.dir = scratch_dir("retired-worker-completion");
    run.plan = Some(class_phase(1, 0));
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Gate { round: 0 } };
    let failed = ctx.add_idle();
    let done = ctx.add_idle();
    run.workers.push(mk_worker("b0", "worker-big", failed, WState::Failed("previous attempt".into())));
    let mut latest = mk_worker("b0", "worker-big", done, WState::Done);
    latest.attempt = 2;
    run.workers.push(latest);
    run.begin_handoff(&mut ctx, 0);
    assert!(run.completed_agents.contains(&done));
    assert!(!run.completed_agents.contains(&failed));
    run.workers.clear();
    assert!(run.completed_agents.contains(&done), "the completion survives the next phase clearing its worker list");
    let _ = std::fs::remove_dir_all(&run.dir);
}

/// Review fix: the built-in default pattern's last finale step reuses `self.planner` as the
/// finale agent id (`start_finale`), so `respawn`/`watchdog_role_tag` must disambiguate via
/// `Stage::Finale` before falling back to the plain planner check — otherwise this shared id
/// silently misroutes to `respawn_planner` (wrong resume prompt, dangling `finale_agent`).
#[test]
fn respawn_shared_planner_finale_agent_routes_to_finale_not_planner() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    let planner = ctx.add_idle();
    run.planner = Some(planner);
    run.finale_agent = Some(planner); // shared id, exactly as `start_finale` leaves it
    run.stage = Stage::Finale { idx: 1 }; // built-in pattern's last finale step (1) is the planner
    run.plan = Some(Plan { title: "x".into(), ..Default::default() });

    run.respawn(&mut ctx, planner, None).unwrap();

    assert!(matches!(run.stage, Stage::Finale { idx: 1 }), "must stay on the finale step, got {:?}", run.stage);
    assert_eq!(run.finale_agent, Some(planner), "the shared id keeps serving as the finale agent");
    assert!(ctx.prompts.iter().any(|(a, t)| *a == planner && t.contains("[mantra:finale]")), "expected the finale step's own prompt, got {:?}", ctx.prompts);
    assert!(!ctx.prompts.iter().any(|(_, t)| t.contains("Continue from stage")), "must not fall through to the planner respawn's resume prompt");
}

/// Review fix: `respawn_gate` must reuse the gate's *current* round (via
/// `spawn_gate_at_round`), not silently reset it to 1 — `gate_max_rounds` and the L4
/// identical-blocker halt are both keyed off `round`.
#[test]
fn respawn_gate_preserves_the_current_round() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    let gate = ctx.add_idle();
    run.gate_agent = Some(gate);
    run.plan = Some(one_task_phase());
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Gate { round: 2 } };

    run.respawn(&mut ctx, gate, None).unwrap();

    match &run.stage {
        Stage::Phase { step: PhaseStep::Gate { round }, .. } => assert_eq!(*round, 2, "a respawned gate must keep its round, not reset to 1"),
        other => panic!("expected Phase{{Gate}}, got {other:?}"),
    }
    let new_gate = run.gate_agent.expect("gate agent still set after respawn");
    assert_ne!(new_gate, gate);
}

/// Review fix: if the stage has already moved on (gate agent still registered but the run
/// left `Stage::Phase{Gate}`, e.g. the round just finished), `respawn` must surface an error
/// instead of silently no-op'ing and letting the caller show a false "respawned" toast.
#[test]
fn respawn_gate_after_stage_moved_on_is_an_error() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    let gate = ctx.add_idle();
    run.gate_agent = Some(gate);
    run.stage = Stage::Review; // no longer Phase{Gate} — phase_idx() is None

    let err = run.respawn(&mut ctx, gate, None).unwrap_err();
    assert!(err.contains("already moved on"), "{err}");
    assert_eq!(run.gate_agent, Some(gate), "no-op must not tear down the still-registered agent");
}

/// Same guard for the finale agent: if `stage` is no longer `Stage::Finale`, `respawn` must
/// return an error rather than `Ok(())` with nothing having happened.
#[test]
fn respawn_finale_after_stage_moved_on_is_an_error() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    let finale = ctx.add_idle();
    run.finale_agent = Some(finale);
    run.stage = Stage::Review; // no longer Stage::Finale

    let err = run.respawn(&mut ctx, finale, None).unwrap_err();
    assert!(err.contains("already moved on"), "{err}");
    assert_eq!(run.finale_agent, Some(finale), "no-op must not tear down the still-registered agent");
}

// L4 gate-loop protection -------------------------------------------------------------------

#[test]
fn gate_loop_protection_halts_on_two_identical_blockers() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    let gate = ctx.add_idle();
    run.gate_agent = Some(gate);
    run.plan = Some(one_task_phase());
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Gate { round: 1 } };
    let blocked = "GATE: fail — Blocked: shell sandbox fails before any command runs (bwrap)";
    drive_gate_to_halt(&mut ctx, &mut run, gate, blocked);
    assert_eq!(run.halt.as_ref().unwrap().reason, HaltReason::GateExhausted);
    assert!(run.halt.as_ref().unwrap().message.contains("same blocker twice"), "two consecutive identical blockers must halt, not spend the remaining rounds");
}

/// The gate never fixes: a failing verdict goes to a fix worker named `fix-<round>`, scoped to
/// the files the findings name, with the findings as its prompt — not to the orchestrator and
/// not back to the gate agent.
#[test]
fn a_failing_gate_report_goes_to_a_fix_worker_not_the_orchestrator() {
    let dir = std::env::temp_dir().join(format!("mantra-fixscope-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/lib.rs"), "").unwrap();
    let mut ctx = TestCtx::new();
    let mut run = Run::new(dir.clone(), Pattern::builtin(), "test".into());
    let gate = ctx.add_idle();
    let orch = ctx.add_idle();
    ready(&mut ctx, orch);
    run.gate_agent = Some(gate);
    run.orchestrator = Some(orch);
    run.plan = Some(one_task_phase());
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Gate { round: 1 } };
    gate_says(&mut ctx, &mut run, gate, "GATE: fail — src/lib.rs:12 exports `foo` but main.rs calls `bar`; rename it. Also docs/new-file.md should describe it.");

    assert!(!run.halted());
    assert!(matches!(run.stage, Stage::Phase { step: PhaseStep::Fix { round: 1 }, .. }), "{:?}", run.stage);
    assert!(ctx.stopped.contains(&gate), "the QA agent is done — archived, never prompted again");
    assert_eq!(run.gate_agent, None);
    assert!(!ctx.prompts.iter().any(|(a, _)| *a == gate), "the gate agent is not told to keep going: {:?}", ctx.prompts);
    assert!(!ctx.prompts.iter().any(|(a, _)| *a == orch), "the orchestrator is not involved: {:?}", ctx.prompts);
    let w = run.workers.iter().find(|w| w.task.id == "fix-1").expect("fix-1 was created");
    assert_eq!(w.task.role, "worker-small");
    assert!(w.adhoc, "a fix worker works on the integrated copy");
    assert!(w.prompt.contains("src/lib.rs:12 exports `foo`"), "the findings are its prompt: {}", w.prompt);
    assert_eq!(w.task.scope, vec!["src/lib.rs".to_string(), "docs/new-file.md".to_string()], "scoped to the files the findings name (existing, or a new file with an extension)");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_big_flag_on_the_report_sends_the_fix_to_worker_big() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    let gate = ctx.add_idle();
    run.gate_agent = Some(gate);
    run.plan = Some(one_task_phase());
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Gate { round: 1 } };
    let (_, ok) = run.handle_tool(&mut ctx, gate, "mantra_gate_report", &json!({"pass": false, "summary": "the auth flow is wired in three places inconsistently", "big": true}));
    assert!(ok);
    run.on_turn_done(&mut ctx, gate, "completed", None, None);
    assert_eq!(run.workers.iter().find(|w| w.task.id == "fix-1").unwrap().task.role, "worker-big");
    // a pattern without those worker names falls back to its closest worker role
    let mut p = Pattern::builtin();
    let small = p.roles.remove("worker-small").unwrap();
    p.roles.insert("tiny-fixer".into(), small);
    let r2 = Run::new(PathBuf::from("."), p, "test".into());
    assert_eq!(r2.fix_role(false), "tiny-fixer", "no small role left: the first worker role it has");
    assert_eq!(r2.fix_role(true), "worker-big");
}

/// A pass is verified by the checks; when they still fail the fix goes to a worker too.
#[test]
fn checks_failing_after_a_qa_pass_go_to_a_fix_worker() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
    let orch = ctx.add_idle();
    ready(&mut ctx, orch);
    let gate = ctx.add_idle();
    run.orchestrator = Some(orch);
    run.gate_agent = Some(gate);
    run.plan = Some(one_task_phase());
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Checks { round: 1 } };
    run.qa_ok = true; // QA passed this round; these are its verify checks
    let fail = || vec![CheckResult { cmd: "uv venv .venv".into(), ok: false, code: Some(2), output: "error: Permission denied".into(), secs: 0 }];
    run.on_job(&mut ctx, JobTag::Checks { phase: 0, round: 1, gen: run.checks_gen }, JobOut::Checks(fail(), vec![], git::ReviewDiff::default(), None));
    assert!(!run.halted());
    assert!(matches!(run.stage, Stage::Phase { step: PhaseStep::Fix { round: 1 }, .. }), "{:?}", run.stage);
    let w = run.workers.iter().find(|w| w.task.id == "fix-1").expect("a fix worker takes the failing checks");
    assert!(w.prompt.contains("Permission denied") && w.prompt.contains("uv venv .venv"), "{}", w.prompt);
    assert!(!ctx.prompts.iter().any(|(a, _)| *a == gate || *a == orch), "neither the gate nor the orchestrator is asked: {:?}", ctx.prompts);
}

/// After the fix worker finishes: checks again, then a FRESH QA agent that sees the previous
/// verdict, the fix worker's report and the check results.
#[test]
fn after_the_fix_the_checks_rerun_and_a_fresh_qa_round_follows() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    let gate = ctx.add_idle();
    run.gate_agent = Some(gate);
    run.plan = Some(one_task_phase());
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Gate { round: 1 } };
    gate_says(&mut ctx, &mut run, gate, "GATE: fail — README needs a usage section");
    // the fix worker, through the job/turn cycle
    let attempt = run.workers.iter().find(|w| w.task.id == "fix-1").unwrap().attempt;
    run.on_job(&mut ctx, JobTag::WorkerWt { task: "fix-1".into(), attempt }, JobOut::Wt(Ok((PathBuf::from("."), String::new()))));
    let fixer = run.workers.iter().find(|w| w.task.id == "fix-1").and_then(|w| w.agent).unwrap();
    assert!(ctx.spawned("fix-1").iter().any(|s| s.role_name == "worker-small"));
    ctx.agents.get_mut(&fixer).unwrap().final_message = Some("STATUS: done\nSUMMARY: added the usage section".into());
    run.on_turn_done(&mut ctx, fixer, "completed", None, None);
    let second = run.gate_agent.expect("round 2 starts");
    assert_ne!(second, gate);
    assert!(matches!(run.stage, Stage::Phase { step: PhaseStep::Gate { round: 2 }, .. }), "{:?}", run.stage);
    let (_, prompt) = ctx.prompts.iter().rev().find(|(a, _)| *a == second).expect("the fresh agent is briefed");
    assert!(prompt.contains("QA round 2/3"), "{prompt}");
    assert!(prompt.contains("Previous QA round (1) said fail: GATE: fail — README needs a usage section"), "{prompt}");
    assert!(prompt.contains("fix-1: STATUS: done") && prompt.contains("added the usage section"), "the fix worker's report is shown: {prompt}");
    assert!(prompt.contains("Gate checks") && prompt.contains("diff"), "{prompt}");
    assert_eq!(ctx.spawned("qa").len(), 1, "one fresh QA agent for round 2 (the first was seeded by the test)");
}

/// The first QA round of a phase (a real spawn) is a fresh read-only agent whose instructions
/// say it never fixes.
#[test]
fn the_phase_gate_agent_is_fresh_read_only_and_told_not_to_fix() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    run.plan = Some(one_task_phase());
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Checks { round: 0 } };
    run.spawn_gate(&mut ctx, 0);
    let g = run.gate_agent.unwrap();
    let sp = &ctx.spawned("qa")[0];
    assert_eq!(sp.sandbox, "read-only");
    let instr = &ctx.spawned_instructions.iter().find(|(id, _)| *id == g).unwrap().1;
    assert!(instr.contains("never fix") && instr.contains("Your role cannot write files"), "{instr}");
    // a second round replaces it: the first is archived, the second is a different agent
    run.spawn_gate_at_round(&mut ctx, 0, 2);
    let g2 = run.gate_agent.unwrap();
    assert_ne!(g, g2);
    assert!(ctx.stopped.contains(&g));
}

#[test]
fn merge_conflicts_go_to_a_fix_worker_because_the_gate_cannot_merge() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    run.plan = Some(one_task_phase());
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Checks { round: 0 } };
    run.conflicts = vec!["mantra-w/r/p1-a-1".into()];
    run.on_job(&mut ctx, JobTag::Checks { phase: 0, round: 0, gen: run.checks_gen }, JobOut::Checks(vec![], vec![], git::ReviewDiff::default(), None));
    assert!(run.gate_agent.is_none(), "no QA agent reads a tree that is not merged yet");
    let w = run.workers.iter().find(|w| w.task.id == "fix-0").expect("a worker merges the branches, before QA round 1");
    assert_eq!(w.task.role, "worker-big");
    assert!(w.prompt.contains("git merge") && w.prompt.contains("mantra-w/r/p1-a-1"), "{}", w.prompt);
    assert!(matches!(run.stage, Stage::Phase { step: PhaseStep::Fix { round: 0 }, .. }), "{:?}", run.stage);
    finish_fix(&mut ctx, &mut run, 0);
    assert!(run.conflicts.is_empty(), "the fix worker resolved them");
    let g = run.gate_agent.expect("the QA round follows");
    assert!(matches!(run.stage, Stage::Phase { step: PhaseStep::Gate { round: 1 }, .. }), "the merge fix does not use up a QA round: {:?}", run.stage);
    let (_, p) = ctx.prompts.iter().rev().find(|(a, _)| *a == g).unwrap();
    assert!(p.contains("QA round 1/3") && p.contains("fix-0: STATUS: done"), "the first QA agent sees what the merge fix did: {p}");
}

#[test]
fn a_qa_agent_that_ends_without_a_report_is_asked_twice_then_the_run_halts() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
    let planner = ctx.add_idle();
    let gate = ctx.add_idle();
    run.planner = Some(planner);
    run.gate_agent = Some(gate);
    run.plan = Some(one_task_phase());
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Gate { round: 1 } };
    for n in 1..=2 {
        ctx.prompts.clear();
        run.on_turn_done(&mut ctx, gate, "completed", None, None);
        assert!(!run.halted(), "nudge {n}");
        assert!(ctx.prompts.iter().any(|(a, t)| *a == gate && t.contains("mantra_gate_report")), "{:?}", ctx.prompts);
    }
    run.on_turn_done(&mut ctx, gate, "completed", None, None);
    assert!(run.halted());
    assert_eq!(run.halt.as_ref().unwrap().reason, HaltReason::GateExhausted);
    assert!(run.workers.is_empty(), "nothing to hand a fix worker");
}

/// A fix worker that cannot do its job halts the run (the orchestrator cannot drive this
/// loop); the same task retried after the halt picks the round back up.
#[test]
fn a_blocked_fix_worker_halts_and_the_resume_starts_it_over() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
    let planner = ctx.add_idle();
    let orch = ctx.add_idle();
    ready(&mut ctx, orch);
    let gate = ctx.add_idle();
    run.planner = Some(planner);
    run.orchestrator = Some(orch);
    run.gate_agent = Some(gate);
    run.plan = Some(one_task_phase());
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Gate { round: 1 } };
    gate_says(&mut ctx, &mut run, gate, "GATE: fail — src/a.rs is wrong");
    let attempt = run.workers.iter().find(|w| w.task.id == "fix-1").unwrap().attempt;
    run.on_job(&mut ctx, JobTag::WorkerWt { task: "fix-1".into(), attempt }, JobOut::Wt(Ok((PathBuf::from("."), String::new()))));
    let fixer = run.workers[0].agent.unwrap();
    ctx.agents.get_mut(&fixer).unwrap().final_message = Some("STATUS: blocked\nSUMMARY: cannot write src/a.rs".into());
    ctx.prompts.clear();
    run.on_turn_done(&mut ctx, fixer, "completed", None, None);
    assert!(run.halted());
    assert_eq!(run.halt.as_ref().unwrap().reason, HaltReason::AttemptsExhausted);
    assert!(!ctx.prompts.iter().any(|(a, t)| *a == orch && t.contains("FAILED")), "not the orchestrator's: {:?}", ctx.prompts);
    run.resume(&mut ctx);
    assert!(run.workers.iter().filter(|w| w.task.id == "fix-1").count() == 2, "a second attempt of the same fix task");
    assert!(matches!(run.stage, Stage::Phase { step: PhaseStep::Fix { round: 1 }, .. }), "{:?}", run.stage);
}

/// The QA report arrives while the run is halted: the fix is remembered (and saved), not started.
#[test]
fn a_fail_report_during_a_pause_defers_the_fix_until_the_resume() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    let gate = ctx.add_idle();
    run.gate_agent = Some(gate);
    run.plan = Some(one_task_phase());
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Gate { round: 1 } };
    run.toggle_pause(&mut ctx);
    assert!(run.halted());
    gate_says(&mut ctx, &mut run, gate, "GATE: fail — fix src/a.rs");
    match &run.pending {
        Some(Pending::Fix { idx: 0, round: 1, findings, big: false }) => assert!(findings.contains("src/a.rs")),
        other => panic!("expected a deferred fix, got {other:?}"),
    }
    assert!(run.workers.is_empty());
    run.toggle_pause(&mut ctx);
    assert!(run.workers.iter().any(|w| w.task.id == "fix-1"), "the resume starts the fix worker");
    assert!(matches!(run.stage, Stage::Phase { step: PhaseStep::Fix { round: 1 }, .. }), "{:?}", run.stage);
    assert!(run.pending.is_none());
}

/// The watchdog watches the fix worker like any other worker.
#[test]
fn the_watchdog_expects_the_fix_worker_to_be_busy() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    run.plan = Some(one_task_phase());
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Fix { round: 1 } };
    let a = ctx.add_busy();
    let mut w = mk_worker("fix-1", "worker-small", a, WState::Running);
    w.adhoc = true;
    run.workers.push(w);
    assert!(run.expected_active().iter().any(|(id, e)| *id == a && *e == Expect::Working("fix-1".into())));
}

/// A fix worker's out-of-scope edit is logged, not sent to the orchestrator.
#[test]
fn a_fix_workers_scope_miss_does_not_wake_the_orchestrator() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    let orch = ctx.add_idle();
    ready(&mut ctx, orch);
    run.orchestrator = Some(orch);
    run.plan = Some(one_task_phase());
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Fix { round: 1 } };
    let a = ctx.add_busy();
    let mut w = mk_worker("fix-1", "worker-small", a, WState::Running);
    w.adhoc = true;
    w.task.scope = vec!["src/a.rs".into()];
    run.workers.push(w);
    run.on_files_changed(&mut ctx, a, &["src/b.rs".into()]);
    assert!(run.orch_inbox.is_empty() && ctx.prompts.is_empty(), "{:?}", ctx.prompts);
    assert!(run.pulse.iter().any(|p| p.text.contains("tripwire")));
}

/// A plan revision sends the phase back to building: a fix worker of the gate that no longer
/// stands is stopped, not left editing the shared copy.
#[test]
fn a_plan_revision_stops_a_running_fix_worker() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    run.plan = Some(one_task_phase());
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Fix { round: 1 } };
    let orch = ctx.add_idle();
    run.orchestrator = Some(orch);
    let a = ctx.add_busy();
    let mut w = mk_worker("fix-1", "worker-small", a, WState::Running);
    w.adhoc = true;
    run.workers.push(w);
    run.return_to_orchestrating(&mut ctx);
    assert!(ctx.stopped.contains(&a));
    assert_eq!(run.workers[0].state, WState::Cancelled);
    assert_eq!(run.stage, Stage::Phase { idx: 0, step: PhaseStep::Orchestrating });
}

#[test]
fn the_big_flag_is_read_leniently() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    let g = ctx.add_idle();
    for (v, want) in [(json!(true), true), (json!("true"), true), (json!("no"), false), (json!(null), false)] {
        run.handle_tool(&mut ctx, g, "mantra_gate_report", &json!({"pass": false, "summary": "x", "big": v}));
        assert_eq!(run.gate_big, want, "{v}");
    }
    run.handle_tool(&mut ctx, g, "mantra_gate_report", &json!({"pass": false, "summary": "x"}));
    assert!(!run.gate_big);
}

/// WP-2.5: QA says the check is wrong, not the work: the planner is asked, no fix worker.
#[test]
fn a_plan_defect_report_goes_to_the_planner_not_to_a_fix_worker() {
    for report in [
        json!({"pass": false, "summary": "check 3 (`cargo foo`) cannot pass here: no such target", "plan_defect": true}),
        json!({"pass": false, "summary": "PLAN DEFECT: check 3 (`cargo foo`) cannot pass here: no such target"}),
    ] {
        let mut ctx = TestCtx::new();
        let (mut run, planner, gate) = gate_run_with_checks(&mut ctx, 1);
        let (_, ok) = run.handle_tool(&mut ctx, gate, "mantra_gate_report", &report);
        assert!(ok);
        run.on_turn_done(&mut ctx, gate, "completed", None, None);
        assert_eq!(run.halt.as_ref().expect("halted").reason, HaltReason::GateExhausted);
        assert!(!run.workers.iter().any(|w| w.task.id == "fix-1"), "no fix worker");
        assert!(ctx.spawned("fix-1").is_empty());
        assert!(ctx.prompts.iter().any(|(a, p)| *a == planner && p.contains("cannot pass here: no such target") && p.contains("plan defect")), "{:?}", ctx.prompts);
    }
}

/// WP-2.6: the orchestrator answered QA and passed the question up; QA is waiting, not silent.
#[test]
fn qa_waiting_for_the_planner_is_not_nudged_for_a_report() {
    let mut ctx = TestCtx::new();
    let (mut run, _planner, gate) = gate_run_with_checks(&mut ctx, 1);
    let orch = ctx.add_idle();
    run.orchestrator = Some(orch);
    // QA's own question was answered; the orchestrator's question to the planner is open
    run.pending_questions.insert(orch, "may the check use network?".into());
    gate_says(&mut ctx, &mut run, gate, "I asked; waiting.");
    assert!(!ctx.prompts.iter().any(|(a, p)| *a == gate && p.contains("[mantra:gate]")), "{:?}", ctx.prompts);
    assert_eq!(run.gate_nudges, 0);
    assert!(!run.halted());
    assert!(run.expected_active().iter().all(|(a, _)| *a != gate), "the watchdog leaves the waiting QA agent alone");
    // without the open question it is expected to act
    run.pending_questions.clear();
    assert!(run.expected_active().iter().any(|(a, _)| *a == gate));
}

/// WP-2.6: a fix worker that asked and then wrote `STATUS: blocked` waits; after the answer and
/// a `STATUS: done` turn the fix is over.
#[test]
fn a_fix_worker_that_asked_and_wrote_blocked_waits_instead_of_halting() {
    let mut ctx = TestCtx::new();
    let (mut run, _planner, _gate) = gate_run_with_checks(&mut ctx, 1);
    run.start_fix(&mut ctx, 0, 1, "src/a.rs is wrong".into(), false);
    let attempt = run.workers.iter().find(|w| w.task.id == "fix-1").unwrap().attempt;
    run.on_job(&mut ctx, JobTag::WorkerWt { task: "fix-1".into(), attempt }, JobOut::Wt(Ok((PathBuf::from("."), String::new()))));
    let agent = run.workers.iter().find(|w| w.task.id == "fix-1").and_then(|w| w.agent).unwrap();
    run.pending_questions.insert(agent, "which module?".into());
    ctx.agents.get_mut(&agent).unwrap().final_message = Some("STATUS: blocked\nSUMMARY: need an answer".into());
    run.on_turn_done(&mut ctx, agent, "completed", None, None);
    assert!(!run.halted(), "{:?}", run.halt.as_ref().map(|h| &h.message));
    assert!(run.pulse.iter().any(|p| p.text.contains("waits for an answer")));
    assert!(matches!(run.stage, Stage::Phase { step: PhaseStep::Fix { .. }, .. }));
    assert!(ctx.jobs.is_empty());
    // the answer arrives and the worker finishes
    ctx.agents.get_mut(&agent).unwrap().final_message = Some("STATUS: done\nSUMMARY: fixed".into());
    run.on_turn_done(&mut ctx, agent, "completed", None, None);
    assert!(!run.halted());
    assert!(!run.pending_questions.contains_key(&agent));
    assert!(ctx.jobs.iter().any(|j| j.starts_with("FixChecks")), "on_fix_done ran: {:?}", ctx.jobs);
}

/// The plan with the current phase's gate checks replaced.
fn revised_checks(run: &Run, checks: &[&str]) -> serde_json::Value {
    let mut plan = run.plan.clone().unwrap();
    plan.phases[0].gate.checks = checks.iter().map(|c| c.to_string()).collect();
    json!({"plan": serde_json::to_value(&plan).unwrap(), "reason": "the check was wrong"})
}

/// The plan's tasks with prompts long enough for a revision to be accepted.
fn give_prompts(run: &mut Run) {
    for p in run.plan.iter_mut().flat_map(|p| p.phases.iter_mut()) {
        for t in &mut p.tasks {
            t.prompt = "do the work described for this task".into();
        }
    }
}

fn ok_check(cmd: &str) -> Vec<CheckResult> {
    vec![CheckResult { cmd: cmd.into(), ok: true, code: Some(0), output: String::new(), secs: 0 }]
}

/// WP-2.7: a blocked fix worker of a gate whose check was revised is not restarted with its old
/// brief; the revised checks run and a fresh QA round sees them.
#[test]
fn a_gate_revision_cancels_a_blocked_fix_worker_and_reruns_the_checks() {
    let mut ctx = TestCtx::new();
    let (mut run, planner, gate) = gate_run_with_checks(&mut ctx, 1);
    give_prompts(&mut run);
    run.start_fix(&mut ctx, 0, 1, "src/a.rs is wrong".into(), false);
    let fa = ctx.add_idle();
    {
        let w = run.workers.iter_mut().find(|w| w.task.id == "fix-1").unwrap();
        w.agent = Some(fa);
        w.state = WState::Failed("blocked".into());
    }
    let _ = gate;
    run.halt = Some(Halt { reason: HaltReason::AttemptsExhausted, agent: Some(fa), message: "fix-1 blocked".into(), full: "fix-1 blocked".into(), since: Instant::now() });
    ctx.jobs.clear();
    let (msg, ok) = run.handle_tool(&mut ctx, planner, "mantra_revise_plan", &revised_checks(&run, &["new-check --flag"]));
    assert!(ok, "{msg}");
    assert!(!run.halted());
    let fix = run.workers.iter().find(|w| w.task.id == "fix-1").unwrap();
    assert_eq!(fix.state, WState::Cancelled);
    assert!(ctx.stopped.contains(&fa));
    assert!(ctx.spawned("fix-1").len() <= 1, "not respawned: {:?}", ctx.spawns);
    assert_eq!(run.workers.iter().filter(|w| w.task.id == "fix-1" && w.state != WState::Cancelled).count(), 0);
    assert_eq!(ctx.jobs.len(), 1, "{:?}", ctx.jobs);
    assert!(ctx.jobs[0].starts_with("Checks"), "{:?}", ctx.jobs);
    run.on_job(&mut ctx, JobTag::Checks { phase: 0, round: 1, gen: run.checks_gen }, JobOut::Checks(ok_check("new-check --flag"), vec![], git::ReviewDiff::default(), None));
    assert!(matches!(run.stage, Stage::Phase { step: PhaseStep::Gate { round: 1 }, .. }), "{:?}", run.stage);
    let g = run.gate_agent.expect("a fresh QA agent");
    assert!(ctx.prompts.iter().any(|(a, p)| *a == g && p.contains("new-check --flag")), "{:?}", ctx.prompts);
}

/// WP-2.7: a live QA round is abandoned when its gate is revised.
#[test]
fn a_gate_revision_stops_the_live_qa_agent_and_starts_the_checks() {
    let mut ctx = TestCtx::new();
    let (mut run, planner, gate) = gate_run_with_checks(&mut ctx, 1);
    give_prompts(&mut run);
    let gen = run.checks_gen;
    run.qa_ok = true;
    let (msg, ok) = run.handle_tool(&mut ctx, planner, "mantra_revise_plan", &revised_checks(&run, &["other-check"]));
    assert!(ok, "{msg}");
    assert!(ctx.stopped.contains(&gate));
    assert!(run.gate_agent.is_none());
    assert!(!run.qa_ok);
    assert!(run.checks_gen > gen);
    assert!(matches!(run.stage, Stage::Phase { step: PhaseStep::Checks { round: 1 }, .. }), "{:?}", run.stage);
    assert_eq!(ctx.jobs.len(), 1, "{:?}", ctx.jobs);
}

/// WP-2.7: a gate round deferred during a halt is dropped by a gate revision; the QA prompt
/// after the re-run shows fresh results.
#[test]
fn a_gate_revision_drops_a_deferred_gate_round() {
    let mut ctx = TestCtx::new();
    let (mut run, planner, _gate) = gate_run_with_checks(&mut ctx, 1);
    give_prompts(&mut run);
    halt_by_hand(&mut run);
    run.spawn_gate_at_round(&mut ctx, 0, 2);
    assert!(matches!(run.pending, Some(Pending::Gate { .. })), "{:?}", run.pending);
    run.checks = vec![CheckResult { cmd: "stale-check".into(), ok: false, code: Some(1), output: "x".into(), secs: 0 }];
    let (msg, ok) = run.handle_tool(&mut ctx, planner, "mantra_revise_plan", &revised_checks(&run, &["fresh-check"]));
    assert!(ok, "{msg}");
    assert!(run.pending.is_none());
    assert!(run.pulse.iter().any(|p| p.text.contains("dropped")));
    assert!(run.checks.is_empty());
    run.on_job(&mut ctx, JobTag::Checks { phase: 0, round: 1, gen: run.checks_gen }, JobOut::Checks(ok_check("fresh-check"), vec![], git::ReviewDiff::default(), None));
    let g = run.gate_agent.expect("QA started");
    let p = ctx.prompts.iter().rev().find(|(a, _)| *a == g).map(|(_, p)| p.clone()).unwrap();
    assert!(p.contains("fresh-check") && !p.contains("stale-check"), "{p}");
}

/// WP-2.7: a revision of a later phase leaves the gate alone.
#[test]
fn a_revision_of_a_later_phase_leaves_the_gate_alone() {
    let mut ctx = TestCtx::new();
    let (mut run, planner, gate) = gate_run_with_checks(&mut ctx, 1);
    let mut plan = two_phase_plan();
    plan.phases[0].gate.checks = vec!["true".into()];
    run.plan = Some(plan);
    give_prompts(&mut run);
    let mut plan = run.plan.clone().unwrap();
    plan.phases[1].gate.checks = vec!["later-check".into()];
    let (msg, ok) = run.handle_tool(&mut ctx, planner, "mantra_revise_plan", &json!({"plan": serde_json::to_value(&plan).unwrap(), "reason": "later"}));
    assert!(ok, "{msg}");
    assert_eq!(run.gate_agent, Some(gate));
    assert!(!ctx.stopped.contains(&gate));
    assert!(ctx.jobs.is_empty(), "{:?}", ctx.jobs);
    assert!(matches!(run.stage, Stage::Phase { step: PhaseStep::Gate { round: 1 }, .. }), "{:?}", run.stage);
}

/// WP-2.7: the revision's notes reach the orchestrator before the "no attempt yet" list.
#[test]
fn the_kick_carries_the_queued_notes_first() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
    let orch = ctx.add_idle();
    ready(&mut ctx, orch);
    run.orchestrator = Some(orch);
    run.plan = Some(one_task_phase());
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
    run.orch_inbox.push("The planner revised the current phase:\n- task t1 was ADDED".into());
    run.kick_orchestrator(&mut ctx, &[]);
    let (_, t) = ctx.prompts.last().expect("kicked");
    assert!(t.find("ADDED").unwrap() < t.find("no attempt yet").unwrap(), "{t}");
    assert!(run.orch_inbox.is_empty());
}

#[test]
fn paths_in_finds_the_files_a_report_names() {
    let dir = std::env::temp_dir().join(format!("mantra-paths-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("src/web")).unwrap();
    std::fs::write(dir.join("src/web/app.js"), "").unwrap();
    std::fs::write(dir.join("Makefile"), "").unwrap();
    let t = "Problems: `src/web/app.js:40:5` calls a removed helper (see b/src/web/app.js too); the e.g. v1.2 and and/or are prose; ../etc/passwd and /etc/hosts are not ours; \
             a brand new tests/new_test.py should exist; https://example.com/x.html is a link. Makefile has a typo. Also src/web.";
    let got = paths_in(t, &dir);
    assert_eq!(got, vec!["src/web/app.js".to_string(), "tests/new_test.py".to_string(), "src/web".to_string()], "{got:?}");
    assert!(paths_in("nothing to see, v2.0 and and/or", &dir).is_empty());
    let many: String = (0..30).map(|i| format!("src/f{i}.rs ")).collect();
    assert_eq!(paths_in(&many, &dir).len(), 12, "capped");
    std::fs::remove_dir_all(&dir).ok();
}

// v0.3 chain of command -------------------------------------------------------------------

/// An agent with a thread, so `wake_orch`/`review_tick` treat it as reachable.
fn ready(ctx: &mut TestCtx, a: AgentId) {
    if let Some(ag) = ctx.agents.get_mut(&a) {
        ag.thread_id = Some(format!("thread-{a}"));
        ag.status = Status::Idle;
    }
}

#[test]
fn worker_question_travels_up_and_the_answer_comes_back() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    let orch = ctx.add_idle();
    ready(&mut ctx, orch);
    let worker_agent = ctx.add_busy();
    run.orchestrator = Some(orch);
    run.plan = Some(one_task_phase());
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
    run.workers.push(mk_worker("t1", "worker-small", worker_agent, WState::Running));

    let (msg, ok) = run.handle_tool(&mut ctx, worker_agent, "mantra_ask", &json!({"question": "offset or cursor pagination?"}));
    assert!(ok, "{msg}");
    assert!(run.waiting_for_answer(worker_agent));
    let orch_prompt = ctx.prompts.iter().find(|(a, _)| *a == orch).map(|(_, t)| t.clone()).expect("the idle orchestrator is woken with the question");
    assert!(orch_prompt.contains("QUESTION from t1") && orch_prompt.contains("cursor pagination"), "{orch_prompt}");

    // the worker stops to wait: its idle turn end is not "done"
    ctx.agents.get_mut(&worker_agent).unwrap().turn_active = false;
    ctx.agents.get_mut(&worker_agent).unwrap().final_message = Some("Waiting for the decision.".into());
    run.on_turn_done(&mut ctx, worker_agent, "completed", None, None);
    assert_eq!(run.workers[0].state, WState::Running, "a waiting worker stays running");
    assert!(run.status_text(&ctx).contains("WAITING"), "{}", run.status_text(&ctx));
    let expected = run.expected_active();
    assert!(!expected.iter().any(|(a, _)| *a == worker_agent), "a waiting worker is not the watchdog's problem");
    assert!(expected.iter().any(|(a, e)| *a == orch && *e == Expect::Orchestrating), "the orchestrator is expected to answer: {expected:?}");

    // the orchestrator answers — no done-guard, no F3 counting, the question is closed
    ctx.prompts.clear();
    let (msg, ok) = run.handle_tool(&mut ctx, orch, "mantra_prompt", &json!({"agent": "t1", "message": "cursor, page size 50"}));
    assert!(ok && msg.contains("answer"), "{msg}");
    assert!(!run.waiting_for_answer(worker_agent));
    assert!(ctx.prompts.iter().any(|(a, t)| *a == worker_agent && t.contains("(answering your question)") && t.contains("cursor")));

    // and now a real completion is a completion
    ctx.agents.get_mut(&worker_agent).unwrap().final_message = Some("Done.\nSTATUS: done\nSUMMARY: cursor pagination".into());
    run.on_turn_done(&mut ctx, worker_agent, "completed", None, None);
    assert_eq!(run.workers[0].state, WState::Done);
}

#[test]
fn orchestrator_question_goes_to_the_planner_and_a_brief_answers_it() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    let planner = ctx.add_idle();
    let orch = ctx.add_idle();
    ready(&mut ctx, orch);
    run.planner = Some(planner);
    run.orchestrator = Some(orch);
    run.plan = Some(one_task_phase());
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };

    let (msg, ok) = run.handle_tool(&mut ctx, orch, "mantra_ask", &json!({"question": "may I drop the CSV export?"}));
    assert!(ok, "{msg}");
    let p = ctx.prompts.iter().find(|(a, _)| *a == planner).map(|(_, t)| t.clone()).expect("planner is asked");
    assert!(p.contains("[mantra:question]") && p.contains("mantra_brief_orchestrator") && p.contains("mantra_ask_user"), "{p}");
    assert!(run.expected_active().iter().any(|(a, e)| *a == planner && *e == Expect::Answering), "the planner must answer: {:?}", run.expected_active());

    let (msg, ok) = run.handle_tool(&mut ctx, planner, "mantra_brief_orchestrator", &json!({"message": "Keep the CSV export."}));
    assert!(ok, "{msg}");
    assert!(!run.waiting_for_answer(orch));
    assert!(!run.expected_active().iter().any(|(_, e)| *e == Expect::Answering));
    // briefs are delivered when the planner's turn ends (so one turn can send several)
    run.on_turn_done(&mut ctx, planner, "completed", None, None);
    assert!(ctx.prompts.iter().any(|(a, t)| *a == orch && t.contains("Keep the CSV export")), "the brief reaches the idle orchestrator: {:?}", ctx.prompts);
}

#[test]
fn ask_user_shows_a_question_and_the_reply_answers_it() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    let planner = ctx.add_idle();
    run.planner = Some(planner);
    run.stage = Stage::Planning;

    let (msg, ok) = run.handle_tool(&mut ctx, planner, "mantra_ask_user", &json!({"question": "Postgres or SQLite?"}));
    assert!(ok, "{msg}");
    assert_eq!(run.question.as_ref().map(|q| q.text.as_str()), Some("Postgres or SQLite?"));
    assert!(run.alerts.iter().any(|a| a.contains("Postgres")), "the inbox shows it");
    assert!(run.expected_active().is_empty(), "a planner waiting on the user is not idle");
    // its turn ends without a plan: no nudge, no failure
    run.on_turn_done(&mut ctx, planner, "completed", None, None);
    assert!(!ctx.prompts.iter().any(|(_, t)| t.contains("haven't submitted")));
    assert!(run.is_active());

    run.user_input(&mut ctx, "SQLite, keep it simple");
    assert!(run.question.is_none());
    assert!(run.alerts.is_empty());
    let (a, t) = ctx.prompts.last().expect("the answer is a prompt");
    assert_eq!(*a, planner);
    assert!(t.contains("[from the user] (answering your question") && t.contains("SQLite"), "{t}");
}

#[test]
fn gate_exhaustion_escalates_to_the_planner_and_resume_run_lets_it_continue() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
    let planner = ctx.add_idle();
    let gate = ctx.add_idle();
    run.planner = Some(planner);
    run.gate_agent = Some(gate);
    run.plan = Some(one_task_phase());
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Gate { round: 1 } };
    let blocked = "GATE: fail — the venv is gone and uv cannot recreate it";
    let gate = drive_gate_to_halt(&mut ctx, &mut run, gate, blocked);
    assert_eq!(run.halt.as_ref().unwrap().reason, HaltReason::GateExhausted);
    let esc = ctx.prompts.iter().find(|(a, t)| *a == planner && t.contains("[mantra:escalation]")).map(|(_, t)| t.clone()).expect("the planner is handed the halt");
    assert!(esc.contains("mantra_revise_plan") && esc.contains("mantra_resume_run") && esc.contains("mantra_ask_user"), "{esc}");
    assert!(run.halt_hint().contains("planner"));

    // the planner: plan is right, here's a hint — the run resumes with a fresh QA agent that
    // gets the note, and the planner itself is not told to "continue where you left off"
    ctx.prompts.clear();
    let (msg, ok) = run.handle_tool(&mut ctx, planner, "mantra_resume_run", &json!({"note": "the venv is at .venv — do not recreate it"}));
    assert!(ok, "{msg}");
    assert!(!run.halted());
    let fresh = run.gate_agent.expect("no checks in this phase: a fresh QA agent is the gate");
    assert_ne!(fresh, gate, "the stuck agent is replaced, not nudged");
    assert!(ctx.prompts.iter().any(|(a, t)| *a == fresh && t.contains("[mantra:gate]") && t.contains("QA round 1/") && t.contains("[from the planner]") && t.contains(".venv")), "{:?}", ctx.prompts);
    assert!(!ctx.prompts.iter().any(|(a, _)| *a == gate), "the old agent is not woken for a turn it will not finish: {:?}", ctx.prompts);
    assert!(!ctx.prompts.iter().any(|(a, t)| *a == planner && t.contains("[mantra:resume]")));
    assert!(matches!(run.stage, Stage::Phase { step: PhaseStep::Gate { round: 1 }, .. }), "{:?}", run.stage);
}

#[test]
fn identical_verify_failures_escalate_before_the_rounds_run_out() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
    let planner = ctx.add_idle();
    let gate = ctx.add_idle();
    run.planner = Some(planner);
    run.gate_agent = Some(gate);
    run.plan = Some(one_task_phase());
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Checks { round: 1 } };
    run.qa_ok = true; // QA passed this round; these are its verify checks
    let fail = || vec![CheckResult { cmd: "uv venv .venv".into(), ok: false, code: Some(2), output: "error: Permission denied".into(), secs: 0 }];
    // QA passed, the checks still fail: fix-1 takes the output…
    run.on_job(&mut ctx, JobTag::Checks { phase: 0, round: 1, gen: run.checks_gen }, JobOut::Checks(fail(), vec![], git::ReviewDiff::default(), None));
    assert!(!run.halted());
    let w = run.workers.iter().find(|w| w.task.id == "fix-1").expect("a fix worker");
    assert!(w.prompt.contains("Permission denied"), "{}", w.prompt);
    // …and when the very same failure is still there afterwards, nobody is fixing it
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Checks { round: 1 } };
    run.on_job(&mut ctx, JobTag::FixChecks { phase: 0, round: 1, gen: run.checks_gen }, JobOut::Checks(fail(), vec![], git::ReviewDiff::default(), None));
    assert!(run.halted(), "the identical failure twice means nobody is fixing it");
    assert!(run.halt.as_ref().unwrap().message.contains("identically"));
    assert!(ctx.prompts.iter().any(|(a, t)| *a == planner && t.contains("[mantra:escalation]") && t.contains("uv venv")));
}

/// A fix that changes the failure moves on to a fresh QA round that sees the new check output.
#[test]
fn a_changed_check_failure_after_a_fix_goes_to_the_next_qa_round() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
    let gate = ctx.add_idle();
    run.gate_agent = Some(gate);
    run.plan = Some(one_task_phase());
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Checks { round: 1 } };
    run.qa_ok = true; // QA passed this round; these are its verify checks
    let fail = |out: &str| vec![CheckResult { cmd: "cargo test".into(), ok: false, code: Some(101), output: out.into(), secs: 0 }];
    run.on_job(&mut ctx, JobTag::Checks { phase: 0, round: 1, gen: run.checks_gen }, JobOut::Checks(fail("first failure"), vec![], git::ReviewDiff::default(), None));
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Checks { round: 1 } };
    run.on_job(&mut ctx, JobTag::FixChecks { phase: 0, round: 1, gen: run.checks_gen }, JobOut::Checks(fail("a different failure"), vec![], git::ReviewDiff { stat: " src/a.rs | 2 +-".into(), diff: "+new line".into(), truncated: false }, None));
    assert!(!run.halted());
    let g2 = run.gate_agent.expect("round 2");
    assert!(matches!(run.stage, Stage::Phase { step: PhaseStep::Gate { round: 2 }, .. }), "{:?}", run.stage);
    let (_, p) = ctx.prompts.iter().rev().find(|(a, _)| *a == g2).unwrap();
    assert!(p.contains("a different failure") && p.contains("src/a.rs | 2 +-") && p.contains("+new line"), "the fresh QA agent sees the checks and the diff: {p}");
    // the last round has no fix after it: a failure there halts
    let mut r2 = Run::new(PathBuf::from("."), no_manager(), "test".into());
    r2.plan = Some(one_task_phase());
    r2.stage = Stage::Phase { idx: 0, step: PhaseStep::Checks { round: 3 } };
    r2.qa_ok = true;
    r2.on_job(&mut ctx, JobTag::Checks { phase: 0, round: 3, gen: run.checks_gen }, JobOut::Checks(fail("still"), vec![], git::ReviewDiff::default(), None));
    assert!(r2.halted());
    assert_eq!(r2.halt.as_ref().unwrap().reason, HaltReason::GateExhausted);
    assert!(r2.workers.is_empty(), "no fix worker after the last round");
}

#[test]
fn revising_the_plan_while_halted_reopens_changed_tasks_and_resumes() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    let planner = ctx.add_idle();
    let orch = ctx.add_idle();
    ready(&mut ctx, orch);
    let gate = ctx.add_idle();
    let worker_agent = ctx.add_idle();
    run.planner = Some(planner);
    run.orchestrator = Some(orch);
    run.gate_agent = Some(gate);
    let mut plan = one_task_phase();
    plan.phases[0].tasks[0].prompt = "build it, then delete the venv".into();
    run.plan = Some(plan.clone());
    run.plan_version = 1;
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Gate { round: 3 } };
    run.workers.push(mk_worker("t1", "worker-small", worker_agent, WState::Done));
    run.halt(&mut ctx, HaltReason::GateExhausted, Some(gate), "phase 1 gate not passed after 3 rounds".into());

    // while halted nothing may start
    let (msg, ok) = run.handle_tool(&mut ctx, orch, "mantra_spawn", &json!({"task_id": "t1"}));
    assert!(!ok && msg.contains("halted"), "{msg}");

    // the planner fixes the task's prompt
    plan.phases[0].tasks[0].prompt = "build it; leave the venv in place".into();
    ctx.prompts.clear();
    let (msg, ok) = run.handle_tool(&mut ctx, planner, "mantra_revise_plan", &json!({"plan": serde_json::to_value(&plan).unwrap(), "reason": "keep the venv"}));
    assert!(ok, "{msg}");
    assert!(msg.contains("back to building"), "{msg}");
    assert!(!run.halted(), "the revision is the fix — the run resumes by itself");
    assert!(matches!(run.stage, Stage::Phase { idx: 0, step: PhaseStep::Orchestrating }), "{:?}", run.stage);
    assert_eq!(run.workers[0].state, WState::Cancelled, "the done worker of a changed task is re-opened");
    assert!(run.gate_agent.is_none(), "the gate is spawned afresh once the phase is rebuilt");
    let note = ctx.prompts.iter().find(|(a, _)| *a == orch).map(|(_, t)| t.clone()).expect("the orchestrator is told");
    assert!(note.contains("t1 CHANGED after it was done") && note.contains("mantra_spawn"), "{note}");
    // and spawning the re-opened task is accepted again
    let (msg, ok) = run.handle_tool(&mut ctx, orch, "mantra_spawn", &json!({"task_id": "t1"}));
    assert!(ok, "{msg}");
    assert_eq!(run.workers.len(), 2);
}

#[test]
fn review_tick_hands_the_orchestrator_a_digest_of_running_workers() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    let orch = ctx.add_idle();
    ready(&mut ctx, orch);
    let worker_agent = ctx.add_busy();
    ctx.agents.get_mut(&worker_agent).unwrap().activity = "$ cargo test".into();
    run.orchestrator = Some(orch);
    run.plan = Some(one_task_phase());
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
    run.workers.push(mk_worker("t1", "worker-small", worker_agent, WState::Running));
    run.pattern.settings.review_minutes = 1;

    run.review_tick(&mut ctx);
    assert!(ctx.prompts.is_empty(), "not due yet");
    run.last_review = Instant::now() - Duration::from_secs(61);
    run.review_tick(&mut ctx);
    let (a, t) = ctx.prompts.last().expect("the review wakes the orchestrator");
    assert_eq!(*a, orch);
    assert!(t.contains("[mantra:review]") && t.contains("### t1") && t.contains("$ cargo test"), "{t}");
    assert!(run.last_review.elapsed().as_secs() < 5, "the clock restarts");
    run.pattern.settings.review_minutes = 0;
    run.last_review = Instant::now() - Duration::from_secs(600);
    ctx.prompts.clear();
    run.review_tick(&mut ctx);
    assert!(ctx.prompts.is_empty(), "0 turns the review off");
}

// A user's hand stop (ctrl+c / `x`) --------------------------------------------------------

/// The bug this guards: a planner stopped by hand ends its turn "interrupted" with no plan,
/// which used to walk straight into the planner-nudge branch and prompt it again — the user's
/// stop undone a second later.
#[test]
fn hand_stopped_planner_is_not_nudged_back_to_work() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    let planner = ctx.add_busy();
    run.planner = Some(planner);
    run.stage = Stage::Planning;
    // exactly what `App::interrupt_by_user` leaves behind: the flag set, the turn killed
    ctx.agents.get_mut(&planner).unwrap().stopped_by_user = true;
    ctx.agents.get_mut(&planner).unwrap().turn_active = false;

    run.on_turn_done(&mut ctx, planner, "interrupted", None, None);

    assert!(!ctx.prompts.iter().any(|(_, t)| t.contains("haven't submitted")), "a hand-stopped planner must not be nudged: {:?}", ctx.prompts);
    assert!(ctx.prompts.is_empty(), "nothing at all may be sent to it: {:?}", ctx.prompts);
    assert_eq!(run.planner_nudges, 0);
    assert!(run.is_active(), "the run keeps going — only this agent stopped");
    assert!(run.pulse.iter().any(|p| p.text.contains("stopped by you")), "the journal says why nothing happened: {:?}", run.pulse.iter().map(|p| p.text.clone()).collect::<Vec<_>>());
}

/// The other restart path: `Cmd::Interrupt` can race the turn, so the stop can surface as a
/// failed turn ("agent restarting") instead of an interrupted one — the flag, not the status
/// string, must be what stops the retry ladder.
#[test]
fn hand_stop_also_swallows_a_racing_transient_failure() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    let planner = ctx.add_idle();
    run.planner = Some(planner);
    run.stage = Stage::Planning;
    ctx.agents.get_mut(&planner).unwrap().stopped_by_user = true;

    run.on_turn_done(&mut ctx, planner, "failed", Some("agent restarting".into()), Some(ErrKind::Transient));

    assert!(ctx.prompts.is_empty(), "no backoff retry for a hand-stopped agent: {:?}", ctx.prompts);
    assert!(run.continue_queue.is_empty(), "nothing queued to re-prompt it later");
    assert!(!run.halted());
}

/// A resumed Claude session found unusable (hub/claude.rs's dead-resume detection) surfaces as
/// a failed turn while the hub relaunches a *fresh* process with no history: the engine must
/// re-brief the agent as a respawn — never spend a retry, never halt on "agent turn failed".
#[test]
fn a_dead_resumed_session_respawns_the_agent_instead_of_failing_it() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    let planner = ctx.add_idle();
    run.planner = Some(planner);
    run.stage = Stage::Planning;

    run.on_turn_done(&mut ctx, planner, "failed", Some("resumed session is unusable - relaunching fresh".into()), None);

    assert!(!run.halted(), "a dead session is never a halt: {:?}", run.halt.as_ref().map(|h| h.message.clone()));
    let new_planner = run.planner.expect("planner still set");
    assert_ne!(new_planner, planner, "a fresh agent is briefed, not the one whose session is gone");
    assert!(ctx.prompts.iter().any(|(a, t)| *a == new_planner && t.contains("[mantra:plan]")), "the planner is re-briefed with its planning prompt: {:?}", ctx.prompts);
    assert!(run.continue_queue.is_empty(), "no [mantra:retry] continuation — the old thread has nothing to continue");
    assert!(!run.retry_counts.contains_key(&planner), "not a retry");
}

#[test]
fn process_stop_cancels_a_previously_scheduled_retry_only_for_that_agent() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    let planner = ctx.add_idle();
    let other = ctx.add_idle();
    run.planner = Some(planner);
    run.stage = Stage::Planning;
    run.continue_queue.push((planner, Instant::now(), "retry stopped planner".into()));
    run.continue_queue.push((other, Instant::now(), "retry other agent".into()));
    run.retry_counts.insert(planner, 2);
    run.agent_stopped_by_user(planner);
    assert!(!run.retry_counts.contains_key(&planner));
    assert_eq!(run.continue_queue.len(), 1);
    assert_eq!(run.continue_queue[0].0, other);
    assert!(run.is_active(), "an individual stop keeps the rest of the run");
}

#[test]
fn watchdog_leaves_a_hand_stopped_planner_alone() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    let planner = ctx.add_idle();
    run.planner = Some(planner);
    run.stage = Stage::Planning;
    ctx.set_idle_secs(planner, 600); // well past watchdog_seconds and both escalations
    ctx.agents.get_mut(&planner).unwrap().stopped_by_user = true;

    run.watchdog_tick(&mut ctx);
    assert!(ctx.prompts.is_empty(), "a deliberate stop is not idleness: {:?}", ctx.prompts);
    assert!(!run.halted(), "and it must never escalate into a halt");

    // the user types to it again (`prompt_agent` clears the flag): the ladder starts over
    ctx.agents.get_mut(&planner).unwrap().stopped_by_user = false;
    ctx.set_idle_secs(planner, 100);
    run.watchdog_tick(&mut ctx);
    assert!(ctx.prompts.iter().any(|(a, t)| *a == planner && t.contains("[mantra:watchdog]")), "the watchdog resumes its job once the stop is lifted: {:?}", ctx.prompts);
}

#[test]
fn hand_stopped_worker_is_not_reported_to_the_orchestrator() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    let orch = ctx.add_idle();
    ready(&mut ctx, orch);
    let worker_agent = ctx.add_busy();
    run.orchestrator = Some(orch);
    run.plan = Some(one_task_phase());
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
    run.workers.push(mk_worker("t1", "worker-small", worker_agent, WState::Running));
    ctx.agents.get_mut(&worker_agent).unwrap().stopped_by_user = true;
    ctx.agents.get_mut(&worker_agent).unwrap().turn_active = false;

    run.on_turn_done(&mut ctx, worker_agent, "interrupted", None, None);

    assert!(ctx.prompts.is_empty(), "the orchestrator must not be told to prompt/retry it: {:?}", ctx.prompts);
    assert_eq!(run.workers[0].state, WState::Running, "the task is still this worker's — it just stopped");
    assert!(run.status_text(&ctx).contains("(STOPPED by the user)"), "{}", run.status_text(&ctx));
}

#[test]
fn messaging_a_hand_stopped_worker_again_lifts_the_stop() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    let orch = ctx.add_idle();
    ready(&mut ctx, orch);
    let worker_agent = ctx.add_busy();
    run.orchestrator = Some(orch);
    run.plan = Some(one_task_phase());
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
    run.workers.push(mk_worker("t1", "worker-small", worker_agent, WState::Running));
    ctx.agents.get_mut(&worker_agent).unwrap().stopped_by_user = true;
    ctx.agents.get_mut(&worker_agent).unwrap().turn_active = false;
    run.on_turn_done(&mut ctx, worker_agent, "interrupted", None, None);
    assert!(ctx.prompts.is_empty());

    // someone messages it again — `app::prompt_agent` clears the flag — and a later interrupted
    // turn is handled normally again.
    ctx.agents.get_mut(&worker_agent).unwrap().stopped_by_user = false;
    run.on_turn_done(&mut ctx, worker_agent, "interrupted", None, None);
    assert!(ctx.prompts.iter().any(|(a, t)| *a == orch && t.contains("was interrupted")), "normal handling is back: {:?}", ctx.prompts);
    assert!(!run.status_text(&ctx).contains("STOPPED"));
}

/// A hand-stopped agent whose process happens to restart must not be told to carry on.
#[test]
fn hand_stop_survives_a_process_restart() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    let planner = ctx.add_idle();
    run.planner = Some(planner);
    run.stage = Stage::Planning;
    ctx.agents.get_mut(&planner).unwrap().stopped_by_user = true;

    run.on_ready(&mut ctx, planner, true, true);
    assert!(!ctx.prompts.iter().any(|(_, t)| t.contains("[mantra:resume]")), "{:?}", ctx.prompts);
    assert!(run.agent_meta.contains_key(&planner), "the bookkeeping still happens");

    ctx.agents.get_mut(&planner).unwrap().stopped_by_user = false;
    run.on_ready(&mut ctx, planner, true, true);
    assert!(ctx.prompts.iter().any(|(a, t)| *a == planner && t.contains("[mantra:resume]")), "a plain crash+resume still gets its prompt: {:?}", ctx.prompts);
}

// WP12.4/L1: EnvironmentBroken -------------------------------------------------------------

#[test]
fn environment_broken_halts_once_with_the_sandbox_hint() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    let worker_agent = ctx.add_busy();
    run.workers.push(mk_worker("t1", "worker-small", worker_agent, WState::Running));

    run.on_environment_broken(&mut ctx, worker_agent, "bwrap: setting up uid map: Permission denied".into());
    assert!(run.halted());
    assert_eq!(run.halt.as_ref().unwrap().reason, HaltReason::Environment);
    assert!(run.alerts.iter().any(|a| a.to_lowercase().contains("sandbox")));

    let first = run.halt.as_ref().unwrap().message.clone();
    run.on_environment_broken(&mut ctx, worker_agent, "bwrap: a different failure".into());
    assert_eq!(run.halt.as_ref().unwrap().message, first, "a second EnvironmentBroken must not overwrite the first halt");
}

/// A worker's `mantra_ask` naming a broken sandbox must halt the run right there — never
/// bounce up to the orchestrator as an ordinary question ("transient, retry", a respawn, …).
#[test]
fn worker_environment_question_halts_instead_of_reaching_the_orchestrator() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    let orch = ctx.add_idle();
    ready(&mut ctx, orch);
    let worker_agent = ctx.add_busy();
    run.orchestrator = Some(orch);
    run.plan = Some(one_task_phase());
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
    run.workers.push(mk_worker("t1", "worker-small", worker_agent, WState::Running));

    let (msg, ok) = run.handle_tool(&mut ctx, worker_agent, "mantra_ask", &json!({"question": "bwrap: loopback: Failed RTM_NEWADDR"}));
    assert!(ok, "{msg}");
    assert!(run.halted());
    assert_eq!(run.halt.as_ref().unwrap().reason, HaltReason::Environment);
    assert!(!run.waiting_for_answer(worker_agent), "this is a halt, not an open question");
    assert!(ctx.prompts.is_empty(), "the orchestrator inbox must never see a sandbox failure: {:?}", ctx.prompts);
}

// Manager tests (v0.4) ---------------------------------------------------------------------

/// A run halted on gate exhaustion, with the built-in pattern (which has a manager).
fn halted_gate_run(ctx: &mut TestCtx) -> (Run, AgentId, AgentId) {
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    let planner = ctx.add_idle();
    let gate = ctx.add_idle();
    run.planner = Some(planner);
    run.gate_agent = Some(gate);
    run.plan = Some(one_task_phase());
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Gate { round: 1 } };
    let gate = drive_gate_to_halt(ctx, &mut run, gate, "GATE: fail — the venv is gone");
    (run, planner, gate)
}

#[test]
fn escalation_goes_to_the_manager_first_and_to_the_planner_when_it_does_nothing() {
    let mut ctx = TestCtx::new();
    let (mut run, planner, _gate) = halted_gate_run(&mut ctx);
    let manager = run.manager.expect("the halt spawns the manager");
    let esc = ctx.prompts.iter().find(|(a, t)| *a == manager && t.contains("[mantra:escalation]")).map(|(_, t)| t.clone()).expect("the manager is handed the halt");
    assert!(esc.contains("mantra_resume_run") && esc.contains("mantra_retry") && esc.contains("mantra_respawn") && esc.contains("mantra_ask"), "{esc}");
    assert!(esc.contains("first in line"), "{esc}");
    assert!(!ctx.prompts.iter().any(|(a, t)| *a == planner && t.contains("[mantra:escalation]")), "the planner is not bothered while the manager has it");
    assert!(run.manager_escalation.is_some(), "the halt is in the manager's hands, with a deadline");

    // The manager ends its turn without acting: one reminder…
    ctx.prompts.clear();
    run.on_turn_done(&mut ctx, manager, "completed", None, None);
    assert!(ctx.prompts.iter().any(|(a, t)| *a == manager && t.contains("still halted")), "{:?}", ctx.prompts);
    assert!(!ctx.prompts.iter().any(|(a, _)| *a == planner));
    // …then the planner gets it, with the same message.
    ctx.prompts.clear();
    run.on_turn_done(&mut ctx, manager, "completed", None, None);
    let esc = ctx.prompts.iter().find(|(a, t)| *a == planner && t.contains("[mantra:escalation]")).map(|(_, t)| t.clone()).expect("the planner is next");
    assert!(esc.contains("mantra_revise_plan") && esc.contains("gate stuck"), "{esc}");
    assert!(run.manager_escalation.is_none());
    assert!(run.halted(), "still halted: now it is the planner's");
}

#[test]
fn manager_escalation_deadline_hands_the_halt_to_the_planner() {
    let mut ctx = TestCtx::new();
    let (mut run, planner, _gate) = halted_gate_run(&mut ctx);
    let manager = run.manager.unwrap();
    ready(&mut ctx, manager);
    run.manager_escalation.as_mut().unwrap().deadline = Instant::now() - Duration::from_secs(1);
    ctx.prompts.clear();
    run.manager_tick(&mut ctx);
    assert!(ctx.prompts.iter().any(|(a, t)| *a == planner && t.contains("[mantra:escalation]")), "{:?}", ctx.prompts);
    assert!(run.manager_escalation.is_none());
    // a manager that is waiting on the planner (it asked) is not overtaken by the deadline
    let (mut run2, planner2, _) = halted_gate_run(&mut ctx);
    let m2 = run2.manager.unwrap();
    ready(&mut ctx, m2);
    let (_, ok) = run2.handle_tool(&mut ctx, m2, "mantra_ask", &json!({"question": "the check needs a venv — amend the gate?"}));
    assert!(ok);
    assert!(ctx.prompts.iter().any(|(a, t)| *a == planner2 && t.contains("[mantra:question]") && t.contains("mantra_prompt")));
    ctx.prompts.clear();
    run2.manager_escalation.as_mut().unwrap().deadline = Instant::now() - Duration::from_secs(1);
    run2.manager_tick(&mut ctx);
    assert!(!ctx.prompts.iter().any(|(a, t)| *a == planner2 && t.contains("[mantra:escalation]")), "the planner already has the question: {:?}", ctx.prompts);
    // the planner answers the manager by name, and the answer comes back marked as such
    let (_, ok) = run2.handle_tool(&mut ctx, planner2, "mantra_prompt", &json!({"agent": "manager", "message": "yes — I am revising the gate"}));
    assert!(ok);
    assert!(ctx.prompts.iter().any(|(a, t)| *a == m2 && t.contains("[from the planner] (answering your question)")));
    assert!(!run2.waiting_for_answer(m2));
}

#[test]
fn manager_resume_run_lifts_the_halt_and_the_note_reaches_the_stuck_agent() {
    let mut ctx = TestCtx::new();
    let (mut run, planner, gate) = halted_gate_run(&mut ctx);
    let manager = run.manager.unwrap();
    ctx.prompts.clear();
    let (msg, ok) = run.handle_tool(&mut ctx, manager, "mantra_resume_run", &json!({"note": "the venv is at .venv — do not recreate it"}));
    assert!(ok, "{msg}");
    assert!(!run.halted());
    assert!(run.manager_escalation.is_none());
    let g2 = run.gate_agent.expect("a fresh QA agent takes over the gate");
    assert_ne!(g2, gate);
    assert!(ctx.prompts.iter().any(|(a, t)| *a == g2 && t.contains("[from the manager]") && t.contains(".venv")), "the note reaches the agent that replaces the stuck one: {:?}", ctx.prompts);
    assert!(!ctx.prompts.iter().any(|(a, t)| *a == manager && t.contains("[mantra:resume]")), "the manager itself is not told to continue");
    assert!(!ctx.prompts.iter().any(|(a, _)| *a == planner));
    // and a later idle turn of the manager is nothing special any more
    ctx.prompts.clear();
    run.on_turn_done(&mut ctx, manager, "completed", None, None);
    assert!(ctx.prompts.is_empty(), "{:?}", ctx.prompts);
}

#[test]
fn manager_retry_during_an_attempts_exhausted_halt_resumes_and_respawns_the_task() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    let planner = ctx.add_idle();
    let orch = ctx.add_idle();
    let w = ctx.add_idle();
    run.planner = Some(planner);
    run.orchestrator = Some(orch);
    run.plan = Some(one_task_phase());
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
    let mut wk = mk_worker("t1", "worker-small", w, WState::Failed("tests never pass".into()));
    wk.attempt = 5;
    run.workers.push(wk);
    run.halt_and_escalate(&mut ctx, HaltReason::AttemptsExhausted, Some(w), "t1 has used all 5 attempts — not respawning.".into());
    let manager = run.manager.expect("spawned for the escalation");
    assert!(ctx.prompts.iter().any(|(a, t)| *a == manager && t.contains("[mantra:escalation]") && t.contains("t1 has used all")));
    let before = run.max_attempts();
    ctx.prompts.clear();
    let (msg, ok) = run.handle_tool(&mut ctx, manager, "mantra_retry", &json!({"task_id": "t1", "prompt": "run the tests with -x and fix the first failure only"}));
    assert!(ok, "{msg}");
    assert!(msg.contains("the run resumed"), "{msg}");
    assert!(!run.halted());
    assert_eq!(run.max_attempts(), before + 1, "the retry is the extra attempt");
    let newest = run.workers.iter().rev().find(|x| x.task.id == "t1").unwrap();
    assert_eq!(newest.attempt, 6);
    assert!(matches!(newest.state, WState::Preparing | WState::Queued), "{:?}", newest.state);
    assert_eq!(newest.prompt, "run the tests with -x and fix the first failure only");
}

#[test]
fn manager_respawn_restarts_any_agent_and_never_itself() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    let planner = ctx.add_idle();
    let orch = ctx.add_idle();
    run.planner = Some(planner);
    run.orchestrator = Some(orch);
    run.plan = Some(one_task_phase());
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
    let manager = run.ensure_manager(&mut ctx).unwrap();
    assert!(ctx.prompts.iter().any(|(a, t)| *a == manager && t.contains("[mantra:manage]") && t.contains("mantra_wait")));
    let (msg, ok) = run.handle_tool(&mut ctx, manager, "mantra_respawn", &json!({"agent": "orchestrator", "note": "you kept prompting a finished worker"}));
    assert!(ok, "{msg}");
    let new_orch = run.orchestrator.unwrap();
    assert_ne!(new_orch, orch);
    assert!(ctx.prompts.iter().any(|(a, t)| *a == new_orch && t.contains("finished worker") && t.contains("[mantra:respawn]")));
    let (msg, ok) = run.handle_tool(&mut ctx, manager, "mantra_respawn", &json!({"agent": "manager"}));
    assert!(!ok, "{msg}");
    // the user can respawn the manager, and the fresh one is briefed as a manager
    run.respawn(&mut ctx, manager, None).unwrap();
    let m2 = run.manager.unwrap();
    assert_ne!(m2, manager);
    assert!(ctx.prompts.iter().any(|(a, t)| *a == m2 && t.contains("new manager")));
    assert_eq!(run.name_of(m2), "manager");
    assert_eq!(run.role_name_of(m2).as_deref(), Some("manager"));
}

#[test]
fn watchdog_rung_three_wakes_the_manager_before_the_planner() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    let planner = ctx.add_idle();
    let orch = ctx.add_idle();
    run.planner = Some(planner);
    run.orchestrator = Some(orch);
    run.plan = Some(one_task_phase());
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
    ctx.set_idle_secs(orch, 90);
    run.watchdog_tick(&mut ctx);
    ctx.set_idle_secs(orch, 240);
    run.watchdog_tick(&mut ctx);
    let new_orch = run.orchestrator.unwrap();
    ctx.prompts.clear();
    ctx.set_idle_secs(new_orch, 480);
    run.watchdog_tick(&mut ctx);
    let manager = run.manager.expect("rung 3 brings in the manager");
    assert!(ctx.prompts.iter().any(|(a, t)| *a == manager && t.contains("[mantra:watchdog]") && t.contains("orchestrator")), "{:?}", ctx.prompts);
    assert!(!ctx.prompts.iter().any(|(a, t)| *a == planner && t.contains("did not act")), "the planner waits its turn");
    assert!(run.expected_active().iter().any(|(a, e)| *a == manager && *e == Expect::Managing));
    // the manager finishes no turn about it by its deadline: the planner, as before
    ready(&mut ctx, manager);
    ctx.set_idle_secs(manager, 30);
    run.manager_watchdog.as_mut().unwrap().deadline = Instant::now() - Duration::from_secs(1);
    ctx.prompts.clear();
    run.watchdog_tick(&mut ctx);
    assert!(ctx.prompts.iter().any(|(a, t)| *a == planner && t.contains("did not act")), "{:?}", ctx.prompts);
    assert!(run.manager_watchdog.is_none());
}

#[test]
fn a_completed_manager_turn_closes_its_watchdog_case_and_a_busy_one_is_waited_for() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    let planner = ctx.add_idle();
    let orch = ctx.add_idle();
    run.planner = Some(planner);
    run.orchestrator = Some(orch);
    run.plan = Some(one_task_phase());
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
    let manager = run.ensure_manager(&mut ctx).unwrap();
    ready(&mut ctx, manager);
    run.manager_watchdog = Some(ManagerCase { deadline: Instant::now() - Duration::from_secs(1), about: "orchestrator".into(), respawned: false });
    // busy past the deadline: not overtaken
    ctx.agents.get_mut(&manager).unwrap().turn_active = true;
    run.watchdog_tick(&mut ctx);
    assert!(run.manager_watchdog.is_some());
    assert!(!ctx.prompts.iter().any(|(a, _)| *a == planner));
    // its turn completes: the case is closed, the planner never hears of it
    run.on_turn_done(&mut ctx, manager, "completed", None, None);
    assert!(run.manager_watchdog.is_none());
    run.watchdog_tick(&mut ctx);
    assert!(!ctx.prompts.iter().any(|(a, _)| *a == planner));
}

#[test]
fn a_silent_manager_is_respawned_once_for_a_case_then_the_planner_gets_it() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    let planner = ctx.add_idle();
    let orch = ctx.add_busy();
    run.planner = Some(planner);
    run.orchestrator = Some(orch);
    run.plan = Some(one_task_phase());
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
    let manager = run.ensure_manager(&mut ctx).unwrap();
    ready(&mut ctx, manager);
    run.manager_watchdog = Some(ManagerCase { deadline: Instant::now() + Duration::from_secs(600), about: "t1".into(), respawned: false });
    ctx.set_idle_secs(manager, 240);
    ctx.prompts.clear();
    run.watchdog_tick(&mut ctx);
    let m2 = run.manager.unwrap();
    assert_ne!(m2, manager, "rung 2: a fresh manager");
    assert!(ctx.prompts.iter().any(|(a, t)| *a == m2 && t.contains("OPEN WATCHDOG CASE") && t.contains("t1")), "{:?}", ctx.prompts);
    assert!(run.manager_watchdog.as_ref().map(|c| c.respawned).unwrap_or(false));
    // the fresh one is just as silent: the planner, not a third manager
    ready(&mut ctx, m2);
    ctx.set_idle_secs(m2, 240);
    ctx.prompts.clear();
    run.watchdog_tick(&mut ctx);
    assert_eq!(run.manager, Some(m2), "no respawn loop");
    assert!(ctx.prompts.iter().any(|(a, t)| *a == planner && t.contains("did not act") && t.contains("t1")), "{:?}", ctx.prompts);
    assert!(run.manager_watchdog.is_none());
}

#[test]
fn a_dropped_manager_hands_its_watchdog_case_to_the_planner() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    let planner = ctx.add_idle();
    let orch = ctx.add_idle();
    run.planner = Some(planner);
    run.orchestrator = Some(orch);
    run.plan = Some(one_task_phase());
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
    let manager = run.ensure_manager(&mut ctx).unwrap();
    run.manager_watchdog = Some(ManagerCase { deadline: Instant::now() + Duration::from_secs(600), about: "orchestrator".into(), respawned: false });
    ctx.prompts.clear();
    run.on_turn_done(&mut ctx, manager, "failed", Some("stream closed".into()), Some(ErrKind::Other));
    assert!(run.manager.is_none());
    assert!(run.manager_watchdog.is_none());
    assert!(ctx.prompts.iter().any(|(a, t)| *a == planner && t.contains("did not act") && t.contains("orchestrator")), "{:?}", ctx.prompts);
    assert!(!run.halted());
    // a health digest brings a fresh manager back
    run.last_health = Instant::now() - Duration::from_secs(11 * 60);
    run.health_tick(&mut ctx);
    assert!(run.manager.is_some(), "the next digest spawns a fresh manager");
}

#[test]
fn manager_retry_refused_by_the_done_guard_does_not_lift_the_halt() {
    let mut ctx = TestCtx::new();
    let (mut run, planner, gate) = halted_gate_run(&mut ctx);
    let manager = run.manager.unwrap();
    // the phase is gating: every task is done, so a retry of one is refused…
    let w = ctx.add_idle();
    run.workers.push(mk_worker("t1", "worker-small", w, WState::Done));
    ctx.prompts.clear();
    let (msg, ok) = run.handle_tool(&mut ctx, manager, "mantra_retry", &json!({"task_id": "t1"}));
    assert!(!ok, "{msg}");
    assert!(run.halted(), "…and a refused tool must not resume the run");
    assert!(run.manager_escalation.is_some());
    assert!(!ctx.prompts.iter().any(|(a, _)| *a == gate || *a == planner), "{:?}", ctx.prompts);
    // a respawn of the gate itself is fine and does resume
    let (msg, ok) = run.handle_tool(&mut ctx, manager, "mantra_respawn", &json!({"agent": "gate", "note": "the venv is at .venv"}));
    assert!(ok, "{msg}");
    assert!(!run.halted());
    let g2 = run.gate_agent.unwrap();
    assert_ne!(g2, gate);
    assert!(ctx.prompts.iter().any(|(a, t)| *a == g2 && t.contains(".venv")), "{:?}", ctx.prompts);
}

#[test]
fn a_failed_turn_goes_to_the_manager_and_then_to_the_user_not_the_planner() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    let planner = ctx.add_idle();
    let gate = ctx.add_idle();
    run.planner = Some(planner);
    run.gate_agent = Some(gate);
    run.plan = Some(one_task_phase());
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Gate { round: 1 } };
    // first failure past retries: the free respawn (WP7.3); the second: the halt
    run.on_turn_done(&mut ctx, gate, "failed", Some("boom".into()), Some(ErrKind::Other));
    let gate2 = run.gate_agent.unwrap();
    assert_ne!(gate2, gate);
    assert!(!run.halted());
    run.on_turn_done(&mut ctx, gate2, "failed", Some("boom again".into()), Some(ErrKind::Other));
    assert!(run.halted());
    assert_eq!(run.halt.as_ref().unwrap().reason, HaltReason::AgentTurnFailed);
    let manager = run.manager.expect("the manager gets the failed turn");
    let esc = ctx.prompts.iter().find(|(a, t)| *a == manager && t.contains("[mantra:escalation]")).map(|(_, t)| t.clone()).unwrap();
    assert!(esc.contains("mantra_respawn") && esc.contains("left for the user") && !esc.contains("- mantra_revise_plan"), "{esc}");
    assert!(!ctx.prompts.iter().any(|(a, t)| *a == planner && t.contains("[mantra:escalation]")));
    // the manager respawns the gate: that lifts the halt
    ctx.prompts.clear();
    let (msg, ok) = run.handle_tool(&mut ctx, manager, "mantra_respawn", &json!({"agent": "gate", "note": "the earlier turns died on a huge diff — work file by file"}));
    assert!(ok, "{msg}");
    assert!(!run.halted() && run.manager_escalation.is_none());
    let gate3 = run.gate_agent.unwrap();
    assert_ne!(gate3, gate2);
    assert!(ctx.prompts.iter().any(|(a, t)| *a == gate3 && t.contains("file by file")));
    // …and when it does nothing instead, the planner is NOT dragged in: the band is the user's
    let mut ctx2 = TestCtx::new();
    let mut run2 = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    let planner2 = ctx2.add_idle();
    let g = ctx2.add_idle();
    run2.planner = Some(planner2);
    run2.gate_agent = Some(g);
    run2.plan = Some(one_task_phase());
    run2.stage = Stage::Phase { idx: 0, step: PhaseStep::Gate { round: 1 } };
    run2.halt_turn_failed(&mut ctx2, Some(g), "qa: boom".into());
    let m2 = run2.manager.unwrap();
    run2.on_turn_done(&mut ctx2, m2, "completed", None, None);
    run2.on_turn_done(&mut ctx2, m2, "completed", None, None);
    assert!(run2.halted() && run2.manager_escalation.is_none());
    assert!(!ctx2.prompts.iter().any(|(a, t)| *a == planner2 && t.contains("[mantra:escalation]")), "{:?}", ctx2.prompts);
    // without a manager, exactly the old behaviour: a plain halt for the user
    let mut ctx3 = TestCtx::new();
    let mut run3 = Run::new(PathBuf::from("."), no_manager(), "test".into());
    let p3 = ctx3.add_idle();
    run3.planner = Some(p3);
    run3.stage = Stage::Planning;
    run3.halt_turn_failed(&mut ctx3, Some(p3), "the planner has been unresponsive".into());
    assert!(run3.halted() && run3.manager.is_none());
    assert!(ctx3.prompts.iter().all(|(_, t)| !t.contains("[mantra:escalation]")));
}

#[test]
fn respawning_a_worker_with_a_note_keeps_its_task_prompt() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    let orch = ctx.add_idle();
    let w = ctx.add_idle();
    run.orchestrator = Some(orch);
    run.plan = Some(one_task_phase());
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
    let mut wk = mk_worker("t1", "worker-small", w, WState::Running);
    wk.prompt = "Build the parser module with tests.".into();
    run.workers.push(wk);
    let manager = run.ensure_manager(&mut ctx).unwrap();
    let (msg, ok) = run.handle_tool(&mut ctx, manager, "mantra_respawn", &json!({"agent": "t1", "note": "do not touch the lexer"}));
    assert!(ok, "{msg}");
    let newest = run.workers.iter().rev().find(|x| x.task.id == "t1").unwrap();
    assert_eq!(newest.attempt, 2);
    assert!(newest.prompt.contains("Build the parser module with tests.") && newest.prompt.contains("do not touch the lexer"), "{}", newest.prompt);
}

#[test]
fn a_halt_does_not_interrupt_the_manager_nor_resume_prompt_it() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    let planner = ctx.add_idle();
    let w = ctx.add_busy();
    run.planner = Some(planner);
    run.plan = Some(one_task_phase());
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
    run.workers.push(mk_worker("t1", "worker-small", w, WState::Running));
    let manager = run.ensure_manager(&mut ctx).unwrap();
    ctx.agents.get_mut(&manager).unwrap().turn_active = true; // mid-turn: it just called a tool
    run.halt_and_escalate(&mut ctx, HaltReason::AttemptsExhausted, Some(w), "t1 has used all attempts".into());
    assert!(ctx.interrupted.contains(&w));
    assert!(!ctx.interrupted.contains(&manager), "the manager keeps its turn");
    assert!(ctx.prompts.iter().any(|(a, t)| *a == manager && t.contains("[mantra:escalation]")));
    ctx.prompts.clear();
    run.resume(&mut ctx);
    assert!(ctx.prompts.iter().any(|(a, t)| *a == w && t.contains("[mantra:resume]")));
    assert!(!ctx.prompts.iter().any(|(a, t)| *a == manager && t.contains("[mantra:resume]")), "{:?}", ctx.prompts);
}

#[test]
fn a_failing_manager_is_dropped_not_halted_on() {
    let mut ctx = TestCtx::new();
    let (mut run, planner, _gate) = halted_gate_run(&mut ctx);
    let manager = run.manager.unwrap();
    ctx.prompts.clear();
    // a non-transient failure (past Codex's own retries) drops it at once
    run.on_turn_done(&mut ctx, manager, "failed", Some("stream closed".into()), Some(ErrKind::Other));
    assert!(run.manager.is_none(), "the manager is dropped");
    assert!(run.halted(), "the original halt stands…");
    assert_eq!(run.halt.as_ref().unwrap().reason, HaltReason::GateExhausted, "…and is not replaced by an AgentTurnFailed about the manager");
    assert!(ctx.prompts.iter().any(|(a, t)| *a == planner && t.contains("[mantra:escalation]")), "what it held goes to the planner: {:?}", ctx.prompts);
    // the next event brings a fresh manager
    run.resume(&mut ctx);
    run.manager_inbox.push("[mantra:health] test".into());
    run.wake_manager(&mut ctx);
    assert!(run.manager.is_some());
}

#[test]
fn health_digest_reaches_an_idle_manager_with_the_overview() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    let planner = ctx.add_idle();
    let orch = ctx.add_busy();
    let w = ctx.add_busy();
    run.planner = Some(planner);
    run.orchestrator = Some(orch);
    run.plan = Some(one_task_phase());
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
    run.workers.push(mk_worker("t1", "worker-small", w, WState::Running));
    let manager = run.ensure_manager(&mut ctx).unwrap();
    ready(&mut ctx, manager);
    run.last_health = Instant::now() - Duration::from_secs(11 * 60);
    run.last_tick = Instant::now() - Duration::from_secs(2);
    ctx.prompts.clear();
    run.tick(&mut ctx);
    let d = ctx.prompts.iter().find(|(a, t)| *a == manager && t.contains("[mantra:health]")).map(|(_, t)| t.clone()).expect("digest");
    assert!(d.contains("Run overview") && d.contains("- t1 [worker-small]") && d.contains("orchestrator [") && d.contains("Recent journal"), "{d}");
    // not again right away, and never while halted
    ctx.prompts.clear();
    run.last_tick = Instant::now() - Duration::from_secs(2);
    run.tick(&mut ctx);
    assert!(ctx.prompts.is_empty());
    let mut off = Pattern::builtin();
    off.settings.manager_minutes = 0;
    let mut quiet = Run::new(PathBuf::from("."), off, "test".into());
    quiet.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
    quiet.plan = Some(one_task_phase());
    quiet.last_health = Instant::now() - Duration::from_secs(60 * 60);
    quiet.health_tick(&mut ctx);
    assert!(quiet.manager.is_none(), "manager_minutes = 0: no digest, no manager spawned for one");
}

#[test]
fn manager_status_journal_and_ask_user() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    let planner = ctx.add_idle();
    run.planner = Some(planner);
    run.plan = Some(one_task_phase());
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
    let manager = run.ensure_manager(&mut ctx).unwrap();
    run.log("✓", "green", "something happened");
    let (st, ok) = run.handle_tool(&mut ctx, manager, "mantra_status", &json!({}));
    assert!(ok && st.contains("Stage:") && st.contains("Agents:") && st.contains("- manager ["), "{st}");
    let (j, ok) = run.handle_tool(&mut ctx, manager, "mantra_journal", &json!({"lines": 5}));
    assert!(ok && j.contains("something happened"), "{j}");
    let (_, ok) = run.handle_tool(&mut ctx, manager, "mantra_ask_user", &json!({"question": "the key expired — which one should I use?"}));
    assert!(ok);
    assert!(run.alerts.iter().any(|a| a.starts_with("the manager asks:")));
    ctx.prompts.clear();
    run.user_input(&mut ctx, "use OPENROUTER_API_KEY_2");
    assert!(ctx.prompts.iter().any(|(a, t)| *a == planner && t.contains("[from the user]") && t.contains("mantra_prompt")), "{:?}", ctx.prompts);
    assert!(ctx.prompts.iter().all(|(a, _)| *a != manager), "user answers enter through the planner");
    assert!(run.question.is_none() && run.alerts.is_empty());
}

#[test]
fn brief_from_the_manager_does_not_answer_a_question_the_orchestrator_asked_the_planner() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    let planner = ctx.add_idle();
    let orch = ctx.add_idle();
    run.planner = Some(planner);
    run.orchestrator = Some(orch);
    run.plan = Some(one_task_phase());
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
    let manager = run.ensure_manager(&mut ctx).unwrap();
    let (_, ok) = run.handle_tool(&mut ctx, orch, "mantra_ask", &json!({"question": "rename the module?"}));
    assert!(ok && run.waiting_for_answer(orch));
    let (_, ok) = run.handle_tool(&mut ctx, manager, "mantra_brief_orchestrator", &json!({"message": "keep going on t1"}));
    assert!(ok);
    assert!(run.waiting_for_answer(orch), "the manager's brief is not the planner's answer");
    assert!(run.orch_inbox.iter().any(|m| m.starts_with("[from the manager]")));
    let (_, ok) = run.handle_tool(&mut ctx, planner, "mantra_brief_orchestrator", &json!({"message": "yes, rename it"}));
    assert!(ok && !run.waiting_for_answer(orch));
}

// Effort, titles, thread names, finale prompts ---------------------------------------------

fn mixed_phase() -> Plan {
    Plan {
        phases: vec![Phase {
            id: "p1".into(),
            name: "p1".into(),
            tasks: vec![Task { id: "s1".into(), role: "worker-small".into(), ..Default::default() }, Task { id: "b1".into(), role: "worker-big".into(), ..Default::default() }],
            ..Default::default()
        }],
        ..Default::default()
    }
}

/// Start `task` and let its workspace come back, so its agent is spawned (in `/tmp/wt-<id>`).
fn launch(ctx: &mut TestCtx, run: &mut Run, task: Task, effort: Option<String>) {
    let id = task.id.clone();
    run.spawn_task(ctx, task, None, effort);
    let wi = run.workers.iter().position(|w| w.task.id == id).unwrap();
    run.launch_worker(ctx, wi, PathBuf::from(format!("/tmp/wt-{id}")), String::new());
}

#[test]
fn worker_and_resumed_fix_prompts_inherit_ticket_rules_without_credential_lines() {
    crate::web::snapshot::tests::with_mantra_home(|| {
        let mut ctx = TestCtx::new();
        let brief = "Build the requested integration.\nNoel tickets #14 and #15: German above English in every status block.\nAPI_KEY=must-not-reach-workers\nexport access_token=private-value\n-----BEGIN PRIVATE KEY-----\nprivate-key-material\n-----END PRIVATE KEY-----\nPreserve the decision log.";
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), brief.into());
        run.plan = Some(one_task_phase());
        run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
        let task = Task { id: "ticket-14".into(), role: "worker-small".into(), prompt: "Implement ticket #14.".into(), ..Default::default() };
        launch(&mut ctx, &mut run, task, None);
        let first = run.workers[0].agent.unwrap();
        let fix = Task { id: "fix-1".into(), role: "worker-small".into(), prompt: "Repair ticket #15.".into(), ..Default::default() };
        run.spawn_task(&mut ctx, fix, None, None);
        let wi = run.workers.len() - 1;
        run.launch_worker_with(&mut ctx, wi, PathBuf::from("/tmp/fix-rules"), String::new(), Some("saved-session".into()));
        let resumed = run.workers[wi].agent.unwrap();
        for agent in [first, resumed] {
            let prompt = &ctx.prompts.iter().rev().find(|(a, _)| *a == agent).unwrap().1;
            assert!(prompt.contains("Noel tickets #14 and #15: German above English"), "{prompt}");
            assert!(prompt.contains("Preserve the decision log."));
            assert!(!prompt.contains("must-not-reach-workers") && !prompt.contains("private-value") && !prompt.contains("private-key-material"));
            let instructions = &ctx.spawned_instructions.iter().find(|(a, _)| *a == agent).unwrap().1;
            assert!(instructions.contains("Retry-After") && instructions.contains("Never blindly repeat a patch"));
            assert!(instructions.contains("never invent model credits or session identifiers"));
        }
        assert!(ctx.prompts.iter().any(|(a, p)| *a == resumed && p.starts_with("[mantra:resume]")));
    });
}

#[test]
fn inherited_brief_is_bounded_and_marks_missing_rules() {
    let prompt = worker_brief_context(&"ä".repeat(20_000));
    assert!(prompt.chars().count() < 13_000);
    assert!(prompt.contains("brief clipped at 12000 characters"));
    assert!(prompt.contains("ask for missing ticket/project rules"));
}

#[test]
fn refused_history_merge_halts_without_an_automatic_merge_fixer() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
    run.plan = Some(one_task_phase());
    run.ws = Some(git::Workspace { worktree: true, repo: PathBuf::from("."), integ: PathBuf::from("."), branch: "mantra/test".into(), base_branch: "main".into(), git_common_dir: None, note: None, base_commit: None, initial_commit: false, nested: vec![] });
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Merging };
    let agent = ctx.add_idle();
    run.workers.push(mk_worker("t1", "worker-small", agent, WState::Done));
    run.on_job(&mut ctx, JobTag::Merge { phase: 0 }, JobOut::Merge(git::MergeResult {
        merged: vec![], conflicts: vec![], blocked: vec!["t1: decision-log safeguard: DECISIONS.md lost heading #106".into()], log: "refused".into(),
    }));
    assert!(run.halted());
    assert!(run.halt.as_ref().unwrap().full.contains("DECISIONS.md lost heading #106"));
    assert_eq!(run.stage, Stage::Phase { idx: 0, step: PhaseStep::Orchestrating });
    assert!(ctx.jobs.is_empty(), "no checks or fix worker may bypass a refused merge");
    assert_eq!(run.workers.len(), 1);
    assert_eq!(run.workers[0].state, WState::Done);
    run.resume(&mut ctx);
    assert!(!run.halted());
    assert_eq!(run.stage, Stage::Phase { idx: 0, step: PhaseStep::Merging });
    assert!(ctx.jobs.iter().any(|j| j.contains("Merge")));
}

#[test]
fn refused_phase_commit_halts_and_explicit_resume_retries_without_archiving_twice() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
    run.plan = Some(two_phase_plan());
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Handoff };
    run.handoff_note_done = true;
    run.on_job(&mut ctx, JobTag::Cleanup { phase: 0 }, JobOut::Cleanup(Err("decision-log safeguard: lost heading".into()), git::ReviewDiff::default()));
    assert!(run.halted() && !run.cleanup_done);
    assert_eq!(run.stage, Stage::Phase { idx: 0, step: PhaseStep::Handoff });
    let history = run.history.len();
    run.resume(&mut ctx);
    assert!(!run.halted());
    assert_eq!(run.history.len(), history);
    assert!(ctx.jobs.iter().any(|j| j.contains("Cleanup")));
    assert_eq!(run.stage, Stage::Phase { idx: 0, step: PhaseStep::Handoff });
    run.on_job(&mut ctx, JobTag::Cleanup { phase: 0 }, JobOut::Cleanup(Ok("committed".into()), git::ReviewDiff::default()));
    assert_eq!(run.stage, Stage::Phase { idx: 1, step: PhaseStep::Orchestrating });
}

#[test]
fn planner_and_gate_challenge_contract_and_security_protocol_requirements() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "Review Noel's ticket: German above English.".into());
    let planner = run.spawn_planner(&mut ctx);
    let instructions = &ctx.spawned_instructions.iter().find(|(a, _)| *a == planner).unwrap().1;
    assert!(instructions.contains("abandoned/pending/cancelled attempts"));
    assert!(instructions.contains("GitLab auto-cancel") && instructions.contains("commit SHA"));
    run.plan = Some(one_task_phase());
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Gate { round: 1 } };
    run.spawn_gate_at_round(&mut ctx, 0, 1);
    let gate = run.gate_agent.unwrap();
    let instructions = &ctx.spawned_instructions.iter().find(|(a, _)| *a == gate).unwrap().1;
    assert!(instructions.contains("authoritative protocol/provider specification"));
    assert!(instructions.contains("signed/covered fields") && instructions.contains("trusted issuer/endpoints"));
    assert!(instructions.contains("negative test") && instructions.contains("replay protection"));
    assert!(ctx.prompts.iter().any(|(a, p)| *a == gate && p.contains("German above English")));
}

#[test]
fn auto_effort_is_resolved_per_spawn_from_the_roles_job() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    run.plan = Some(mixed_phase());
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
    // the built-in default sets `auto` on every role
    run.spawn_planner(&mut ctx);
    run.spawn_orchestrator(&mut ctx);
    run.ensure_manager(&mut ctx);
    for who in ["planner", "orchestrator", "manager"] {
        assert_eq!(ctx.spawned(who)[0].effort.as_deref(), Some("medium"), "{who}");
    }
    let tasks = mixed_phase().phases[0].tasks.clone();
    launch(&mut ctx, &mut run, tasks[0].clone(), None);
    launch(&mut ctx, &mut run, tasks[1].clone(), None);
    assert_eq!(ctx.spawned("s1")[0].effort.as_deref(), Some("low"), "worker-small");
    assert_eq!(ctx.spawned("b1")[0].effort.as_deref(), Some("medium"), "worker-big");
    assert_eq!(ctx.spawned("s1")[0].cwd, PathBuf::from("/tmp/wt-s1"));
    // an explicit request still wins: the orchestrator's override, a task's own effort
    let mut t = Task { id: "s2".into(), role: "worker-small".into(), ..Default::default() };
    launch(&mut ctx, &mut run, t.clone(), Some("xhigh".into()));
    assert_eq!(ctx.spawned("s2")[0].effort.as_deref(), Some("xhigh"));
    t.id = "s3".into();
    t.effort = Some("high".into());
    launch(&mut ctx, &mut run, t.clone(), None);
    assert_eq!(ctx.spawned("s3")[0].effort.as_deref(), Some("high"));
    t.id = "s4".into();
    t.effort = None;
    launch(&mut ctx, &mut run, t, Some("auto".into()));
    assert_eq!(ctx.spawned("s4")[0].effort.as_deref(), Some("low"), "an orchestrator asking for auto gets the role's auto");
    // the gate is a medium job
    run.spawn_gate(&mut ctx, 0);
    assert_eq!(ctx.spawned("qa")[0].effort.as_deref(), Some("medium"));
}

#[test]
fn a_role_with_a_named_effort_is_left_alone_and_finale_roles_think_hard() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin_thorough(), "test".into());
    run.plan = Some(mixed_phase());
    run.spawn_planner(&mut ctx);
    assert_eq!(ctx.spawned("planner")[0].effort, None, "the role's own effort applies downstream, untouched");
    // the default's finale: the qa-heavy role is `auto`, which in a finale step is `high`
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    run.plan = Some(mixed_phase());
    let planner = ctx.add_idle();
    run.planner = Some(planner);
    run.start_finale(&mut ctx, 0);
    assert_eq!(ctx.spawned("qa-heavy")[0].effort.as_deref(), Some("high"));
    // the planner's own verification step reuses the planner agent: its effort is raised
    run.start_finale(&mut ctx, 1);
    assert_eq!(ctx.efforts_set, vec![(planner, "high".to_string())]);
    // while a named planner effort stays what it was
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin_thorough(), "test".into());
    run.plan = Some(mixed_phase());
    let planner = ctx.add_idle();
    run.planner = Some(planner);
    run.start_finale(&mut ctx, 2);
    assert!(ctx.efforts_set.is_empty());
    assert_eq!(ctx.spawned("security").len(), 0);
}

#[test]
fn the_run_has_a_title_and_every_thread_carries_it() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "Ich benutze schon seit langer Zeit ein Tool für Notizen".into());
    assert_eq!(run.title, "Ich benutze schon seit langer Zeit", "the goal's first six words");
    let id = run.id.clone();
    assert!(id.ends_with("-ich-benutze-schon-seit-l"), "{id}");
    let planner = run.spawn_planner(&mut ctx);
    assert_eq!(ctx.spawned("planner")[0].thread_name.as_deref(), Some("Ich benutze schon seit langer Zeit · planner"));
    run.spawn_orchestrator(&mut ctx);
    assert_eq!(ctx.spawned("orchestrator")[0].thread_name.as_deref(), Some("Ich benutze schon seit langer Zeit · orchestrator"));

    // the plan comes in: its title names the run, the id stays, live threads are renamed
    let plan = json!({"title": "Notes sync", "summary": "S", "phases": [{"name": "A", "tasks": [
        {"id": "a1", "title": "x", "role": "worker-small", "prompt": "do the first thing well", "scope": ["src/a/**"]}], "gate": {"checks": ["true"]}}]});
    let (msg, ok) = run.handle_tool(&mut ctx, planner, "mantra_submit_plan", &json!({"plan": plan}));
    assert!(ok, "{msg}");
    assert_eq!(run.title, "Notes sync");
    assert_eq!(run.id, id, "the id is the stable key");
    assert!(ctx.renamed.contains(&(planner, "Notes sync · planner".to_string())), "{:?}", ctx.renamed);
    assert_eq!(ctx.renamed.len(), 2, "planner and orchestrator: {:?}", ctx.renamed);
    // agents spawned from here on are named after it
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Checks { round: 0 } };
    run.spawn_gate(&mut ctx, 0);
    assert_eq!(ctx.spawned("qa")[0].thread_name.as_deref(), Some("Notes sync · qa"));
    // the same title again renames nothing
    let n = ctx.renamed.len();
    let (_, ok) = run.handle_tool(&mut ctx, planner, "mantra_submit_plan", &json!({"plan": plan}));
    assert!(ok);
    assert_eq!(ctx.renamed.len(), n);
    // an empty title leaves the old one
    assert_eq!(title_from_goal("   "), "new run");
    assert_eq!(title_from_goal("\"fix the parser,\" he said."), "fix the parser,\" he said");
    assert_eq!(title_from_goal(&"word ".repeat(40)), "word word word word word word");
}

#[test]
fn the_title_and_gate_notes_survive_a_save_and_a_resume() {
    let dir = std::env::temp_dir().join(format!("mantra-title-{}", std::process::id()));
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "build a notes app with sync".into());
    run.dir = dir.clone();
    run.title = "Notes sync".into();
    run.gate_notes.push("phase 1 · QA round 1: pass — fine".into());
    run.save_state();
    let st = crate::engine::state::RunState::load(&dir).unwrap();
    assert_eq!(st.title, "Notes sync");
    assert_eq!(st.gate_notes, vec!["phase 1 · QA round 1: pass — fine".to_string()]);
    let back = Run::from_state(&st, Pattern::builtin(), None, dir.clone());
    assert_eq!(back.title, "Notes sync");
    assert_eq!(back.gate_notes, run.gate_notes);
    // a state file from before titles: the goal's first words stand in
    let mut old = st.clone();
    old.title = String::new();
    assert_eq!(old.display_title(), "build a notes app with sync");
    assert_eq!(Run::from_state(&old, Pattern::builtin(), None, dir.clone()).title, "build a notes app with sync");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_finale_starts_from_the_diff_and_the_reports_not_the_whole_repo() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "make a thing".into());
    run.plan = Some(Plan { title: "T".into(), summary: "a thing".into(), ..Default::default() });
    run.note_gate(0, 1, false, "src/a.rs misses a test");
    run.note_gate(0, 2, true, "all good now");
    run.run_diff = Some(git::ReviewDiff { stat: " src/a.rs | 4 ++--".into(), diff: "+let x = 1;".into(), truncated: false });
    run.start_finale(&mut ctx, 0);
    let qa = run.finale_agent.expect("qa-heavy started");
    let (_, prompt) = ctx.prompts.iter().find(|(a, _)| *a == qa).unwrap();
    assert!(prompt.contains("phase 1 · QA round 1: fail — src/a.rs misses a test") && prompt.contains("phase 1 · QA round 2: pass — all good now"), "{prompt}");
    assert!(prompt.contains("src/a.rs | 4 ++--") && prompt.contains("+let x = 1;"), "{prompt}");
    assert!(prompt.contains("start here, read a file in full only if the diff cannot answer"), "{prompt}");
    let instr = &ctx.spawned_instructions.iter().find(|(id, _)| *id == qa).unwrap().1;
    assert!(!instr.contains("You may edit files"), "the QA pass verifies and has defects fixed by workers: {instr}");
    assert!(instr.contains("run's diff") && instr.contains("spawn a fix worker"), "{instr}");
    // its report joins the notes the next step starts from
    ctx.agents.get_mut(&qa).unwrap().final_message = Some("GATE: pass — nothing real left".into());
    run.on_turn_done(&mut ctx, qa, "completed", None, None);
    assert!(run.gate_notes.iter().any(|n| n.starts_with("finale 1 · qa-heavy: pass — GATE: pass")), "{:?}", run.gate_notes);
    assert!(run.gate_notes.len() == 3);
    // without a captured diff the prompt says where to read it
    let mut r2 = Run::new(PathBuf::from("."), Pattern::builtin(), "make a thing".into());
    r2.start_finale(&mut ctx, 0);
    let q2 = r2.finale_agent.unwrap();
    assert!(ctx.prompts.iter().any(|(a, t)| *a == q2 && t.contains("not captured")));
}

#[test]
fn the_planner_finale_names_what_it_may_do_when_read_only() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "make a thing".into());
    let planner = ctx.add_idle();
    run.planner = Some(planner);
    run.start_finale(&mut ctx, 1);
    assert_eq!(run.finale_agent, Some(planner));
    let (_, p) = ctx.prompts.iter().find(|(a, _)| *a == planner).unwrap();
    assert!(p.contains("mantra_spawn_adhoc"), "{p}");
    assert!(run.pattern.flow.finale[1].task.contains("gate and finale reports"));
}

#[test]
fn checks_are_a_background_job_that_also_brings_the_phase_diff() {
    let dir = std::env::temp_dir().join(format!("mantra-jobs-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let ws = crate::engine::git::Workspace { worktree: false, repo: dir.clone(), integ: dir.clone(), branch: String::new(), base_branch: String::new(), git_common_dir: None, note: None, base_commit: None, initial_commit: false, nested: vec![] };
    let mut ctx = TestCtx::new();
    let mut run = Run::new(dir.clone(), Pattern::builtin(), "test".into());
    run.ws = Some(ws);
    let mut plan = one_task_phase();
    plan.phases[0].gate.checks = vec!["true".into()];
    run.plan = Some(plan);
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Merging };
    run.run_checks(&mut ctx, 0, 0);
    run.run_checks(&mut ctx, 0, 1);
    run.run_checks_after_fix(&mut ctx, 0, 1);
    assert_eq!(ctx.jobs, vec!["Checks { phase: 0, round: 0, gen: 1 }", "Checks { phase: 0, round: 1, gen: 2 }", "FixChecks { phase: 0, round: 1, gen: 3 }"]);
    // with no checks defined: the first round still needs the diff (a job); a verify has nothing to do
    run.plan.as_mut().unwrap().phases[0].gate.checks.clear();
    ctx.jobs.clear();
    run.run_checks(&mut ctx, 0, 0);
    assert_eq!(ctx.jobs, vec!["Checks { phase: 0, round: 0, gen: 4 }"]);
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Gate { round: 1 } };
    run.run_checks(&mut ctx, 0, 1);
    assert_eq!(run.stage, Stage::Phase { idx: 0, step: PhaseStep::Handoff }, "a verify with no checks hands off");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn checks_and_diff_job_runs_the_commands_and_reviews_the_workspace() {
    // the real closure, on a real (git) directory
    let dir = std::env::temp_dir().join(format!("mantra-jobrun-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let sh = |args: &[&str]| assert!(std::process::Command::new("git").arg("-C").arg(&dir).args(args).output().unwrap().status.success());
    sh(&["init", "-q"]);
    std::fs::write(dir.join("a.txt"), "one\n").unwrap();
    sh(&["add", "-A"]);
    sh(&["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "init"]);
    std::fs::write(dir.join("a.txt"), "one\ntwo\n").unwrap();
    let ws = crate::engine::git::Workspace { worktree: false, repo: dir.clone(), integ: dir.clone(), branch: String::new(), base_branch: String::new(), git_common_dir: None, note: None, base_commit: None, initial_commit: false, nested: vec![] };
    struct Capture(Vec<JobOut>);
    impl Capture {
        fn run(&mut self, f: Box<dyn FnOnce() -> JobOut + std::marker::Send>) {
            self.0.push(f());
        }
    }
    // drive launch_checks through a ctx that runs the closure
    struct RunJobs(TestCtx, Capture);
    impl Ctx for RunJobs {
        fn spawn(&mut self, r: SpawnReq) -> AgentId { self.0.spawn(r) }
        fn prompt(&mut self, a: AgentId, t: String) { self.0.prompt(a, t) }
        fn prompt_mode(&mut self, a: AgentId, t: String, m: Send) { self.0.prompt_mode(a, t, m) }
        fn interrupt(&mut self, a: AgentId) { self.0.interrupt(a) }
        fn compact(&mut self, a: AgentId) { self.0.compact(a) }
        fn stop(&mut self, a: AgentId, x: bool) { self.0.stop(a, x) }
        fn tool_result(&mut self, a: AgentId, r: Value, t: String, ok: bool) { self.0.tool_result(a, r, t, ok) }
        fn set_effort(&mut self, a: AgentId, e: &str) -> String { self.0.set_effort(a, e) }
        fn agent(&self, a: AgentId) -> Option<&Agent> { self.0.agent(a) }
        fn job(&mut self, _tag: JobTag, f: Box<dyn FnOnce() -> JobOut + std::marker::Send>) { self.1.run(f) }
        fn notify(&mut self, t: &str) { self.0.notify(t) }
    }
    let mut ctx = RunJobs(TestCtx::new(), Capture(vec![]));
    let mut run = Run::new(dir.clone(), Pattern::builtin(), "test".into());
    run.ws = Some(ws);
    let mut plan = one_task_phase();
    plan.phases[0].gate.checks = vec!["test -f a.txt".into(), "exit 3".into()];
    run.plan = Some(plan);
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Merging };
    run.run_checks(&mut ctx, 0, 0);
    match ctx.1 .0.pop() {
        Some(JobOut::Checks(results, _, diff, _)) => {
            assert_eq!(results.iter().map(|c| c.ok).collect::<Vec<_>>(), vec![true, false]);
            assert!(diff.stat.contains("a.txt") && diff.diff.contains("+two"), "{diff:?}");
        }
        _ => panic!("expected the checks job's result"),
    }
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn check_summary_prints_the_exit_code_plainly_and_points_at_the_log() {
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    run.dir = PathBuf::from("/tmp/run-x");
    run.checks = vec![
        CheckResult { cmd: "false".into(), ok: false, code: Some(1), output: "boom".into(), secs: 1 },
        CheckResult { cmd: "sleep 99".into(), ok: false, code: None, output: String::new(), secs: 5 },
    ];
    run.checks_label = "phase-1-checks-3".into();
    let s = run.check_summary();
    assert!(s.contains("$ false → FAIL (exit 1)"), "{s}");
    assert!(s.contains("FAIL (timed out)"), "{s}");
    assert!(s.contains("(full output of every check: /tmp/run-x/checks/phase-1-checks-3.log)"), "{s}");
}

#[test]
fn only_the_newest_checks_run_counts() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
    let mut plan = one_task_phase();
    plan.phases[0].gate.checks = vec!["true".into()];
    run.plan = Some(plan);
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Merging };
    run.run_checks(&mut ctx, 0, 0);
    let first = run.checks_gen;
    run.run_checks(&mut ctx, 0, 0);
    assert_eq!(run.checks_gen, first + 1);
    let said = |run: &Run| run.pulse.iter().filter(|p| p.text.contains("superseded")).count();
    // the older run lands first: nothing happens
    run.on_job(&mut ctx, JobTag::Checks { phase: 0, round: 0, gen: first }, JobOut::Checks(vec![], vec![], git::ReviewDiff::default(), None));
    assert_eq!(run.stage, Stage::Phase { idx: 0, step: PhaseStep::Checks { round: 0 } });
    assert!(run.gate_agent.is_none());
    assert_eq!(said(&run), 1);
    // the newest one is handled
    run.on_job(&mut ctx, JobTag::Checks { phase: 0, round: 0, gen: first + 1 }, JobOut::Checks(vec![], vec![], git::ReviewDiff::default(), None));
    assert!(run.gate_agent.is_some(), "the current run's result starts QA");
    assert_eq!(said(&run), 1);
    // a result for a run that a stage change made meaningless is ignored too
    run.checks_gen += 1;
    let g = run.checks_gen;
    run.on_job(&mut ctx, JobTag::Checks { phase: 0, round: 1, gen: g }, JobOut::Checks(vec![], vec![], git::ReviewDiff::default(), None));
    assert_eq!(said(&run), 2, "the stage is no longer Checks");
    assert!(matches!(run.stage, Stage::Phase { step: PhaseStep::Gate { round: 1 }, .. }));
}

/// A halt by hand, the way a stage-level test needs one.
fn halt_by_hand(run: &mut Run) {
    run.halt = Some(Halt { reason: HaltReason::GateExhausted, agent: None, message: "test halt".into(), full: "test halt".into(), since: Instant::now() });
}

#[test]
fn a_fix_worker_finishing_during_a_halt_defers_the_checks_until_the_resume() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
    let mut plan = one_task_phase();
    plan.phases[0].gate.checks = vec!["true".into()];
    run.plan = Some(plan);
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Gate { round: 1 } };
    run.start_fix(&mut ctx, 0, 1, "src/a.rs is wrong".into(), false);
    halt_by_hand(&mut run);
    finish_fix(&mut ctx, &mut run, 1);
    assert!(ctx.jobs.is_empty(), "nothing starts while halted: {:?}", ctx.jobs);
    assert_eq!(run.pending, Some(Pending::Checks { idx: 0, round: 1, after_fix: true }));
    run.resume(&mut ctx);
    assert_eq!(ctx.jobs.len(), 1, "{:?}", ctx.jobs);
    assert!(ctx.jobs[0].starts_with("FixChecks"), "{:?}", ctx.jobs);
    assert!(run.pending.is_none());
}

fn green() -> Vec<CheckResult> {
    vec![CheckResult { cmd: "true".into(), ok: true, code: Some(0), output: String::new(), secs: 0 }]
}

fn gate_run_with_checks(ctx: &mut TestCtx, round: u32) -> (Run, AgentId, AgentId) {
    let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
    let planner = ctx.add_idle();
    let gate = ctx.add_idle();
    run.planner = Some(planner);
    run.gate_agent = Some(gate);
    let mut plan = one_task_phase();
    plan.phases[0].gate.checks = vec!["true".into()];
    run.plan = Some(plan);
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Gate { round } };
    (run, planner, gate)
}

#[test]
fn green_checks_after_a_resumed_failed_round_start_qa_instead_of_a_handoff() {
    let mut ctx = TestCtx::new();
    let (mut run, planner, _gate) = gate_run_with_checks(&mut ctx, 2);
    run.prev_gate = Some((2, false, "src/a.rs is wrong".into()));
    halt_by_hand(&mut run);
    let (msg, ok) = run.handle_tool(&mut ctx, planner, "mantra_resume_run", &json!({"note": ""}));
    assert!(ok, "{msg}");
    assert_eq!(ctx.jobs.len(), 1, "{:?}", ctx.jobs);
    run.on_job(&mut ctx, JobTag::Checks { phase: 0, round: 1, gen: run.checks_gen }, JobOut::Checks(green(), vec![], git::ReviewDiff::default(), None));
    assert!(matches!(run.stage, Stage::Phase { step: PhaseStep::Gate { round: 1 }, .. }), "QA decides, the green checks alone do not: {:?}", run.stage);
    assert!(run.gate_agent.is_some());
    assert!(run.pending.is_none());
}

#[test]
fn green_checks_after_a_qa_pass_hand_off() {
    let mut ctx = TestCtx::new();
    let (mut run, _planner, gate) = gate_run_with_checks(&mut ctx, 1);
    let (_, ok) = run.handle_tool(&mut ctx, gate, "mantra_gate_report", &json!({"pass": true, "summary": "coherent"}));
    assert!(ok);
    run.on_turn_done(&mut ctx, gate, "completed", None, None);
    assert_eq!(ctx.jobs.len(), 1, "{:?}", ctx.jobs);
    assert!(run.qa_ok);
    run.on_job(&mut ctx, JobTag::Checks { phase: 0, round: 1, gen: run.checks_gen }, JobOut::Checks(green(), vec![], git::ReviewDiff::default(), None));
    assert!(!matches!(run.stage, Stage::Phase { step: PhaseStep::Gate { .. }, .. }), "{:?}", run.stage);
    assert!(run.gate_agent.is_none(), "the passing QA agent is archived by the handoff");
}

/// The 22:14 race: the fix worker finished during a halt and the resume's own verify both ran
/// checks; the second (green) result handed the phase off although QA had not seen the fix.
#[test]
fn a_fix_finished_during_a_halt_leads_to_one_checks_run_and_a_qa_round() {
    let mut ctx = TestCtx::new();
    let (mut run, _planner, _gate) = gate_run_with_checks(&mut ctx, 1);
    run.start_fix(&mut ctx, 0, 1, "src/a.rs is wrong".into(), false);
    halt_by_hand(&mut run);
    finish_fix(&mut ctx, &mut run, 1);
    run.resume(&mut ctx);
    assert_eq!(ctx.jobs.len(), 1, "exactly one checks run: {:?}", ctx.jobs);
    run.on_job(&mut ctx, JobTag::FixChecks { phase: 0, round: 1, gen: run.checks_gen }, JobOut::Checks(green(), vec![], git::ReviewDiff::default(), None));
    assert!(matches!(run.stage, Stage::Phase { step: PhaseStep::Gate { round: 2 }, .. }), "{:?}", run.stage);
    assert!(run.gate_agent.is_some());
}

// Pause boundary --------------------------------------------------------------------------

fn two_phase_plan() -> Plan {
    let mut p = one_task_phase();
    p.phases.push(Phase { id: "p2".into(), name: "p2".into(), tasks: vec![Task { id: "t2".into(), role: "worker-small".into(), ..Default::default() }], ..Default::default() });
    p
}

/// The pause race from the field: paused during the phase-1 gate with the QA report already
/// in, the run used to verify, hand off and start phase 2 during the pause — the fresh
/// orchestrator's spawns were refused and it sat idle after the resume until a manual retry.
#[test]
fn pause_at_the_gate_defers_the_handoff_and_phase_two_starts_on_resume() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
    let orch = ctx.add_idle();
    ready(&mut ctx, orch);
    let gate = ctx.add_busy();
    let w = ctx.add_idle();
    run.orchestrator = Some(orch);
    run.gate_agent = Some(gate);
    run.plan = Some(two_phase_plan());
    run.workers.push(mk_worker("t1", "worker-small", w, WState::Done));
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Gate { round: 1 } };

    // the QA report lands, then the pause, then the interrupted turn ends
    let (_, ok) = run.handle_tool(&mut ctx, gate, "mantra_gate_report", &json!({"pass": true, "summary": "coherent"}));
    assert!(ok);
    run.toggle_pause(&mut ctx);
    let agents_before = ctx.next;
    ctx.prompts.clear();
    run.on_turn_done(&mut ctx, gate, "interrupted", None, None);

    assert!(run.halted());
    assert_eq!(run.pending, Some(Pending::Handoff { idx: 0 }), "the pass is recorded; the handoff waits for the resume");
    assert_eq!(run.stage, Stage::Phase { idx: 0, step: PhaseStep::Gate { round: 1 } }, "the phase must not advance while paused");
    assert_eq!(ctx.next, agents_before, "no agent may be spawned while paused");
    assert!(ctx.prompts.is_empty(), "no agent may be started while paused: {:?}", ctx.prompts);
    assert_eq!(run.gate_agent, Some(gate));

    // resume: the handoff happens (once), and the phase moves on by itself from there
    run.toggle_pause(&mut ctx);
    assert!(!run.halted() && run.pending.is_none());
    assert_eq!(run.stage, Stage::Phase { idx: 0, step: PhaseStep::Handoff });
    assert!(ctx.prompts.iter().any(|(a, t)| *a == orch && t.contains("[mantra:handoff]")), "{:?}", ctx.prompts);
    assert!(!ctx.prompts.iter().any(|(a, _)| *a == gate), "the archived gate agent gets no 'continue' prompt: {:?}", ctx.prompts);
    ctx.agents.get_mut(&orch).unwrap().final_message = Some("phase 1 built t1".into());
    run.on_turn_done(&mut ctx, orch, "completed", None, None);
    run.on_job(&mut ctx, JobTag::Cleanup { phase: 0 }, JobOut::Cleanup(Ok("committed".into()), git::ReviewDiff::default()));
    assert_eq!(run.stage, Stage::Phase { idx: 1, step: PhaseStep::Orchestrating });
    let orch2 = run.orchestrator.expect("phase 2 has an orchestrator");
    assert!(ctx.prompts.iter().any(|(a, t)| *a == orch2 && t.contains("[mantra:phase] Phase 2/2")), "{:?}", ctx.prompts);
    // …whose spawn is accepted: phase-2 workers start without anyone retrying anything
    let (msg, ok) = run.handle_tool(&mut ctx, orch2, "mantra_spawn", &json!({"task_id": "t2"}));
    assert!(ok, "{msg}");
    assert!(run.workers.iter().any(|w| w.task.id == "t2" && w.state == WState::Preparing), "{msg}");
}

#[test]
fn a_phase_start_due_while_paused_happens_once_on_resume() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
    let orch = ctx.add_idle();
    run.orchestrator = Some(orch);
    run.plan = Some(two_phase_plan());
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Handoff };
    run.handoff_note_done = true;
    run.toggle_pause(&mut ctx);
    let agents_before = ctx.next;
    // the phase's cleanup commit finishes during the pause: phase 2 is due
    run.on_job(&mut ctx, JobTag::Cleanup { phase: 0 }, JobOut::Cleanup(Ok("committed".into()), git::ReviewDiff::default()));
    assert_eq!(run.pending, Some(Pending::Phase { idx: 1 }));
    assert_eq!(run.stage, Stage::Phase { idx: 0, step: PhaseStep::Handoff });
    assert_eq!(ctx.next, agents_before, "no orchestrator is spawned while paused");
    assert!(!ctx.prompts.iter().any(|(_, t)| t.contains("[mantra:phase]")), "{:?}", ctx.prompts);
    let started = |c: &TestCtx| c.prompts.iter().filter(|(_, t)| t.contains("[mantra:phase] Phase 2/2")).count();

    run.toggle_pause(&mut ctx);
    assert_eq!(run.stage, Stage::Phase { idx: 1, step: PhaseStep::Orchestrating });
    assert_eq!(started(&ctx), 1, "{:?}", ctx.prompts);
    assert!(run.pending.is_none());
    // a later pause/resume does not start it again
    run.toggle_pause(&mut ctx);
    run.toggle_pause(&mut ctx);
    assert_eq!(started(&ctx), 1, "{:?}", ctx.prompts);
}

#[test]
fn a_finale_step_reported_while_paused_waits_for_the_resume() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
    let planner = ctx.add_idle();
    let finale = ctx.add_busy();
    run.planner = Some(planner);
    run.finale_agent = Some(finale);
    run.plan = Some(one_task_phase());
    run.stage = Stage::Finale { idx: 0 };
    let (_, ok) = run.handle_tool(&mut ctx, finale, "mantra_gate_report", &json!({"pass": true, "summary": "all green"}));
    assert!(ok);
    run.toggle_pause(&mut ctx);
    let agents_before = ctx.next;
    ctx.prompts.clear();
    run.on_turn_done(&mut ctx, finale, "interrupted", None, None);
    assert_eq!(run.pending, Some(Pending::Finale { idx: 1, note: None }));
    assert_eq!(run.stage, Stage::Finale { idx: 0 });
    assert_eq!(ctx.next, agents_before);
    assert!(ctx.prompts.is_empty(), "{:?}", ctx.prompts);

    run.toggle_pause(&mut ctx);
    assert_eq!(run.stage, Stage::Finale { idx: 1 });
    assert!(run.pending.is_none());
    assert_eq!(ctx.prompts.iter().filter(|(_, t)| t.contains("[mantra:finale] Step 2/")).count(), 1, "{:?}", ctx.prompts);
}

/// An orchestrator (re)spawned during a pause had its spawns refused and went idle with no
/// event ahead to wake it: the resume must get it going, not the watchdog minutes later.
#[test]
fn resume_wakes_an_idle_orchestrator_that_has_tasks_to_spawn() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
    let orch = ctx.add_idle();
    run.orchestrator = Some(orch);
    run.plan = Some(one_task_phase());
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
    run.toggle_pause(&mut ctx);
    run.respawn(&mut ctx, orch, None).unwrap();
    let orch2 = run.orchestrator.unwrap();
    let (msg, ok) = run.handle_tool(&mut ctx, orch2, "mantra_spawn", &json!({"task_id": "t1"}));
    assert!(!ok && msg.contains("REFUSED"), "{msg}");
    ctx.prompts.clear();

    run.toggle_pause(&mut ctx);
    assert!(ctx.prompts.iter().any(|(a, t)| *a == orch2 && t.contains("[mantra:resume]") && t.contains("t1") && t.contains("mantra_spawn")), "{:?}", ctx.prompts);
    let (msg, ok) = run.handle_tool(&mut ctx, orch2, "mantra_spawn", &json!({"task_id": "t1"}));
    assert!(ok, "{msg}");
    // nothing to wake it about once every task has an attempt
    ctx.prompts.clear();
    run.toggle_pause(&mut ctx);
    run.toggle_pause(&mut ctx);
    assert!(!ctx.prompts.iter().any(|(a, _)| *a == orch2), "{:?}", ctx.prompts);
    // one that is waiting on the planner's answer is woken by the answer, not kicked
    run.workers.clear();
    run.pending_questions.insert(orch2, "which lexer?".into());
    run.toggle_pause(&mut ctx);
    run.toggle_pause(&mut ctx);
    assert!(!ctx.prompts.iter().any(|(a, t)| *a == orch2 && t.contains("[mantra:resume]")), "{:?}", ctx.prompts);
}

/// The stage moved on during the halt (the planner added a task, so the phase went back to
/// building): the handoff that was due before is stale and must not run on resume.
#[test]
fn a_plan_revision_during_the_pause_drops_the_deferred_handoff() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
    let planner = ctx.add_idle();
    let orch = ctx.add_idle();
    ready(&mut ctx, orch);
    let gate = ctx.add_busy();
    let w = ctx.add_idle();
    run.planner = Some(planner);
    run.orchestrator = Some(orch);
    run.gate_agent = Some(gate);
    let mut plan = one_task_phase();
    plan.phases[0].tasks[0].prompt = "build the slugify function".into();
    run.plan = Some(plan.clone());
    run.plan_version = 1;
    run.workers.push(mk_worker("t1", "worker-small", w, WState::Done));
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Gate { round: 1 } };
    let (_, ok) = run.handle_tool(&mut ctx, gate, "mantra_gate_report", &json!({"pass": true, "summary": "coherent"}));
    assert!(ok);
    run.toggle_pause(&mut ctx);
    run.on_turn_done(&mut ctx, gate, "interrupted", None, None);
    assert_eq!(run.pending, Some(Pending::Handoff { idx: 0 }));

    plan.phases[0].tasks.push(Task { id: "t1b".into(), role: "worker-small".into(), prompt: "add the unit tests for slugify".into(), ..Default::default() });
    let (msg, ok) = run.handle_tool(&mut ctx, planner, "mantra_revise_plan", &json!({"plan": serde_json::to_value(&plan).unwrap(), "reason": "one more"}));
    assert!(ok && msg.contains("back to building"), "{msg}");
    assert!(run.halted(), "a user pause is not lifted by a revision");
    assert_eq!(run.stage, Stage::Phase { idx: 0, step: PhaseStep::Orchestrating });
    assert!(run.pending.is_none(), "the handoff is stale: {:?}", run.pending);

    ctx.prompts.clear();
    run.toggle_pause(&mut ctx);
    assert_eq!(run.stage, Stage::Phase { idx: 0, step: PhaseStep::Orchestrating }, "the stale handoff must not run");
    assert!(!ctx.prompts.iter().any(|(_, t)| t.contains("[mantra:handoff]")), "{:?}", ctx.prompts);
    let (msg, ok) = run.handle_tool(&mut ctx, orch, "mantra_spawn", &json!({"task_id": "t1b"}));
    assert!(ok, "{msg}");
}

/// A gate respawn during a pause starts nothing (the pause boundary) — and says so, keeping
/// the note for the fresh agent that starts on resume.
#[test]
fn a_gate_respawn_during_the_pause_is_deferred_with_its_note() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
    let gate = ctx.add_busy();
    run.gate_agent = Some(gate);
    run.plan = Some(one_task_phase());
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Gate { round: 2 } };
    run.toggle_pause(&mut ctx);
    let agents_before = ctx.next;
    ctx.prompts.clear();
    let what = run.respawn(&mut ctx, gate, Some("work file by file".into())).unwrap();
    assert_eq!(what, RESPAWN_DEFERRED);
    assert_eq!(ctx.next, agents_before, "no agent may be spawned while paused");
    assert!(ctx.prompts.is_empty(), "{:?}", ctx.prompts);
    assert_eq!(run.pending, Some(Pending::Gate { idx: 0, round: 2, note: Some("work file by file".into()) }));
    assert_eq!(run.gate_agent, None);

    run.toggle_pause(&mut ctx);
    let g2 = run.gate_agent.expect("the fresh gate agent");
    assert_ne!(g2, gate);
    assert_eq!(run.stage, Stage::Phase { idx: 0, step: PhaseStep::Gate { round: 2 } }, "the round is kept");
    assert!(ctx.prompts.iter().any(|(a, t)| *a == g2 && t.starts_with("[mantra:gate]")), "{:?}", ctx.prompts);
    assert!(ctx.prompts.iter().any(|(a, t)| *a == g2 && t == "[mantra:respawn] work file by file"), "{:?}", ctx.prompts);
    assert!(run.pending.is_none());
    // outside a pause it is immediate, as before
    assert_eq!(run.respawn(&mut ctx, g2, None).unwrap(), RESPAWNED);
    assert!(run.gate_agent.is_some_and(|g| g != g2));
}

/// A pause that interrupts the gate agent before it reports is not a failed round.
#[test]
fn a_gate_round_interrupted_by_the_pause_is_not_a_failed_round() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
    let gate = ctx.add_busy();
    run.gate_agent = Some(gate);
    run.plan = Some(one_task_phase());
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Gate { round: 1 } };
    run.toggle_pause(&mut ctx);
    run.on_turn_done(&mut ctx, gate, "interrupted", None, None);
    assert_eq!(run.stage, Stage::Phase { idx: 0, step: PhaseStep::Gate { round: 1 } }, "the round is not burnt");
    assert!(run.pending.is_none() && run.last_gate_blocker.is_none());
    // a second such pause does not trip the identical-blocker halt either
    run.toggle_pause(&mut ctx);
    run.toggle_pause(&mut ctx);
    run.on_turn_done(&mut ctx, gate, "interrupted", None, None);
    assert_eq!(run.halt.as_ref().map(|h| h.reason), Some(HaltReason::User), "{:?}", run.halt.as_ref().map(|h| &h.message));
    ctx.prompts.clear();
    run.toggle_pause(&mut ctx);
    let fresh = run.gate_agent.expect("the interrupted round is taken over by a fresh QA agent");
    assert_ne!(fresh, gate);
    assert!(ctx.stopped.contains(&gate));
    assert!(ctx.prompts.iter().any(|(a, t)| *a == fresh && t.contains("[mantra:gate]") && t.contains("QA round 1/")), "{:?}", ctx.prompts);
    assert!(!ctx.prompts.iter().any(|(a, _)| *a == gate), "the interrupted agent is not prompted again: {:?}", ctx.prompts);
    assert!(matches!(run.stage, Stage::Phase { step: PhaseStep::Gate { round: 1 }, .. }));
}

#[test]
fn a_finished_run_keeps_its_duration() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    let now = crate::util::unix_secs();
    run.started_unix = now - 129;
    run.set_clock(0, Some(now - 129));
    assert!(run.elapsed() >= Duration::from_secs(129), "a running run's clock keeps going");
    run.finish(&mut ctx);
    assert_eq!(run.stage, Stage::Done);
    let at = run.finished_unix.expect("completion is stamped");
    assert!((now..=now + 5).contains(&at), "{at} vs {now}");
    let done = run.elapsed();
    assert!((129..=134).contains(&done.as_secs()), "{done:?}");
    assert!(run.pulse.iter().any(|p| p.text.contains(&format!("run complete in {}", fmt_dur(done)))), "the journal and the screen agree");
    // an hour later the result screen still reads the same
    run.started_unix = now - 3600 - 129;
    run.finished_unix = Some(now - 3600);
    assert_eq!(run.elapsed(), Duration::from_secs(129));
    // a stopped run is frozen the same way
    let mut failed = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    failed.fail_run(&mut ctx, "planner did not submit a valid plan".into());
    assert!(failed.finished_unix.is_some());
}

/// A planner spending minutes exploring nothing (an empty project) is exactly the waste this
/// checks for: `planning_prompt` — used both for a fresh planner and a resumed/respawned one —
/// must tell it up front there is nothing to explore.
#[test]
fn planning_prompt_skips_exploration_for_an_empty_project() {
    let dir = std::env::temp_dir().join(format!("mantra-empty-prompt-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join(".git")).unwrap();
    std::fs::create_dir_all(dir.join(".mantra")).unwrap();
    std::fs::write(dir.join(".gitignore"), "").unwrap();

    let run = Run::new(dir.clone(), Pattern::builtin(), "test".into());
    let prompt = run.planning_prompt();
    assert!(prompt.contains("The project directory is empty (greenfield)"), "{prompt}");
    assert!(!prompt.contains("Explore the project"), "{prompt}");

    std::fs::write(dir.join("main.rs"), "fn main() {}").unwrap();
    let prompt = run.planning_prompt();
    assert!(prompt.contains("Explore the project, then call mantra_submit_plan."), "{prompt}");
    assert!(!prompt.contains("greenfield"), "{prompt}");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn direct_ignores_a_whitespace_only_message() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    let orch = ctx.add_idle();
    run.orchestrator = Some(orch);
    let planner = ctx.add_idle();
    run.planner = Some(planner);
    run.stage = Stage::Planning;

    assert!(!run.direct(&mut ctx, "orchestrator", "   ", Send::Auto), "an empty message is not delivered");
    assert!(ctx.prompts.is_empty(), "nothing must be sent: {:?}", ctx.prompts);
    assert!(run.pulse.is_empty(), "and nothing logged for it");

    assert!(run.direct(&mut ctx, "orchestrator", "status?", Send::Auto), "a real message still goes through");
    assert!(ctx.prompts.iter().any(|(a, t)| *a == planner && t.contains("status?")));
    assert!(ctx.prompts.iter().all(|(a, _)| *a != orch));
}

// mantra_wait really ends the turn ----------------------------------------------------------

/// After `mantra_wait` the turn is over: the first tool call in it is refused and interrupts the
/// turn, and any later call in the same turn is refused again without a second interrupt.
#[test]
fn a_tool_call_after_mantra_wait_is_refused_and_interrupts_the_turn_once() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    let orch = ctx.add_idle();
    ready(&mut ctx, orch);
    run.orchestrator = Some(orch);

    let (text, ok) = run.handle_tool(&mut ctx, orch, "mantra_wait", &json!({}));
    assert!(ok && text.starts_with("OK"), "{text}");

    let (text, ok) = run.handle_tool(&mut ctx, orch, "mantra_status", &json!({}));
    assert!(ok && text.starts_with("Your turn already ended when you called mantra_wait"), "{text}");
    assert_eq!(ctx.interrupted, vec![orch], "the turn is interrupted at the first refused call");

    let (text, _) = run.handle_tool(&mut ctx, orch, "mantra_wait", &json!({}));
    assert!(text.starts_with("Your turn already ended"), "{text}");
    let (text, _) = run.handle_tool(&mut ctx, orch, "mantra_status", &json!({}));
    assert!(text.starts_with("Your turn already ended"), "{text}");
    assert_eq!(ctx.interrupted, vec![orch], "one interrupt per turn, however many calls follow");
}

/// A yielded turn that ends "interrupted" (by the refusal above) is a finished turn: nothing is
/// logged as interrupted, it counts as completed (so the manager's watchdog case closes), and the
/// yield is cleared.
#[test]
fn a_yielded_turn_that_ends_interrupted_counts_as_completed() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    let manager = ctx.add_idle();
    ready(&mut ctx, manager);
    run.manager = Some(manager);
    run.manager_watchdog = Some(ManagerCase { deadline: Instant::now(), about: "t1".into(), respawned: false });

    run.handle_tool(&mut ctx, manager, "mantra_wait", &json!({}));
    run.handle_tool(&mut ctx, manager, "mantra_status", &json!({})); // refused: interrupts the turn
    run.on_turn_done(&mut ctx, manager, "interrupted", None, None);

    assert!(!run.pulse.iter().any(|p| p.text.contains("interrupted")), "{:?}", run.pulse.iter().map(|p| p.text.clone()).collect::<Vec<_>>());
    assert!(!run.yielded.contains(&manager), "the yield ends with the turn");
    assert!(run.manager_watchdog.is_none(), "the turn counts as completed, so the manager's watchdog case closes");
}

/// Two `mantra_status` calls in one turn with nothing changed between them: the second says so and
/// asks for `mantra_wait`. A new turn starts from a fresh look, and a change shows up in full.
#[test]
fn a_repeated_status_with_nothing_new_says_so_and_yields() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    let orch = ctx.add_idle();
    ready(&mut ctx, orch);
    run.orchestrator = Some(orch);

    let (first, _) = run.handle_tool(&mut ctx, orch, "mantra_status", &json!({}));
    assert!(!first.contains("No change"), "{first}");
    let (second, _) = run.handle_tool(&mut ctx, orch, "mantra_status", &json!({}));
    assert!(second.starts_with("No change since your last mantra_status. Call mantra_wait now"), "{second}");
    assert!(run.yielded.contains(&orch), "a further call in this turn is refused");

    run.on_turn_done(&mut ctx, orch, "completed", None, None);
    let worker_agent = ctx.add_busy();
    run.workers.push(mk_worker("t1", "worker-small", worker_agent, WState::Running));
    let (third, _) = run.handle_tool(&mut ctx, orch, "mantra_status", &json!({}));
    assert!(third.contains("t1") && !third.contains("No change"), "{third}");
}

// First-token deadline ---------------------------------------------------------------------

/// A fresh scratch run directory (journal, state and pattern files land here, not in `~/.mantra`).
fn scratch_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("mantra-run-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

/// Make `a` look like it has an active turn that has produced nothing for `secs`.
fn go_silent(ctx: &mut TestCtx, a: AgentId, secs: u64) {
    let ag = ctx.agents.get_mut(&a).unwrap();
    ag.turn_active = true;
    ag.status = Status::Busy;
    ag.thread_id = Some(format!("thread-{a}"));
    ag.turn_wait_since = Some(Instant::now() - Duration::from_secs(secs));
}

#[test]
fn first_token_deadline_restarts_once_then_falls_back_to_the_roles_model() {
    let dir = scratch_dir("fallback");
    let mut ctx = TestCtx::new();
    let mut pat = no_manager();
    pat.roles.get_mut("planner").unwrap().fallback = Some("luna".into());
    let primary = pat.roles["planner"].model.clone();
    let mut run = Run::new(PathBuf::from("."), pat, "test".into());
    run.dir = dir.clone();
    let planner = ctx.add_idle();
    run.planner = Some(planner);
    run.stage = Stage::Planning;

    // under the 4-minute default: nothing happens, however silent it is
    go_silent(&mut ctx, planner, 3 * 60);
    run.watchdog_tick(&mut ctx);
    assert_eq!(run.planner, Some(planner), "3 minutes is inside the deadline");
    assert!(ctx.prompts.is_empty(), "{:?}", ctx.prompts);

    // first strike: the turn is restarted on a fresh process, same model
    go_silent(&mut ctx, planner, 4 * 60 + 5);
    run.watchdog_tick(&mut ctx);
    let second = run.planner.unwrap();
    assert_ne!(second, planner, "the silent planner is replaced");
    assert_eq!(ctx.spawned_models.last().unwrap().1, primary, "the first restart stays on the same model");
    assert!(ctx.prompts.iter().any(|(a, t)| *a == second && t.contains("produced no output") && t.contains("restarted")), "{:?}", ctx.prompts);
    assert!(run.pulse.iter().any(|p| p.text.contains("planner: no output for") && p.text.contains("restarting")), "{:?}", run.pulse.iter().map(|p| p.text.clone()).collect::<Vec<_>>());
    assert!(!run.halted());

    // the very next tick does not strike again: the new agent has no silent spell yet
    run.watchdog_tick(&mut ctx);
    assert_eq!(run.planner, Some(second));

    // second strike on the same slot: the role's fallback takes over, and the log says so
    go_silent(&mut ctx, second, 4 * 60 + 5);
    run.watchdog_tick(&mut ctx);
    let third = run.planner.unwrap();
    assert_ne!(third, second);
    assert_eq!(ctx.spawned_models.last().unwrap().1, "luna", "respawned on the fallback alias");
    assert!(run.pulse.iter().any(|p| p.glyph == "⇄" && p.text == format!("planner: {primary} → fallback luna (no output)")), "{:?}", run.pulse.iter().map(|p| p.text.clone()).collect::<Vec<_>>());
    assert_eq!(run.pattern.roles["planner"].model, primary, "the role's model is not rewritten");
    assert_eq!(run.slot_models.get("planner").map(String::as_str), Some("luna"), "the slot is pinned to the fallback: its respawns and a resume keep it");
    assert!(!run.halted());

    // the fallback gets its own restart; if it is silent too there is nobody left to fall back to
    go_silent(&mut ctx, third, 4 * 60 + 5);
    run.watchdog_tick(&mut ctx);
    let fourth = run.planner.unwrap();
    assert_ne!(fourth, third, "the new model is restarted once before anything else");
    assert!(!run.halted());
    go_silent(&mut ctx, fourth, 4 * 60 + 5);
    run.watchdog_tick(&mut ctx);
    assert!(run.halted(), "no fallback left and no manager: the run halts for the user");
    assert_eq!(run.halt.as_ref().unwrap().reason, HaltReason::AgentTurnFailed);
    assert!(run.halt.as_ref().unwrap().message.contains("twice in a row"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn first_token_deadline_without_a_fallback_escalates_to_the_manager() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    assert!(run.pattern.roles["planner"].fallback.is_none());
    let planner = ctx.add_idle();
    run.planner = Some(planner);
    run.stage = Stage::Planning;

    go_silent(&mut ctx, planner, 5 * 60);
    run.watchdog_tick(&mut ctx);
    let second = run.planner.unwrap();
    assert_ne!(second, planner, "first strike restarts");
    assert!(run.manager.is_none() && !run.halted());

    ctx.prompts.clear();
    go_silent(&mut ctx, second, 5 * 60);
    run.watchdog_tick(&mut ctx);
    assert_eq!(run.planner, Some(second), "no fallback: the silent agent is left for the manager to deal with");
    let manager = run.manager.expect("the manager is woken");
    assert!(ctx.prompts.iter().any(|(a, t)| *a == manager && t.contains("[mantra:watchdog]") && t.contains("planner") && t.contains("no output")), "{:?}", ctx.prompts);
    assert!(run.manager_watchdog.is_some(), "a case is open, with the usual deadline");
    assert!(!run.halted(), "an escalation, not a halt");

    // one silent spell is one strike: the next ticks add nothing
    let n = ctx.prompts.len();
    run.watchdog_tick(&mut ctx);
    run.watchdog_tick(&mut ctx);
    assert_eq!(ctx.prompts.len(), n, "{:?}", ctx.prompts);
}

#[test]
fn first_token_deadline_is_only_for_active_silent_turns() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
    let planner = ctx.add_idle();
    run.planner = Some(planner);
    run.stage = Stage::Planning;

    // already produced something (the wait was cleared by the first output): never a strike
    go_silent(&mut ctx, planner, 30 * 60);
    ctx.agents.get_mut(&planner).unwrap().turn_wait_since = None;
    run.watchdog_tick(&mut ctx);
    assert_eq!(run.planner, Some(planner), "a turn that has spoken is not silent, however long it runs");

    // waiting on an approval is expected silence
    go_silent(&mut ctx, planner, 30 * 60);
    ctx.agents.get_mut(&planner).unwrap().status = Status::Waiting;
    run.watchdog_tick(&mut ctx);
    assert_eq!(run.planner, Some(planner));

    // no active turn: the idle ladder owns it, not this deadline
    go_silent(&mut ctx, planner, 30 * 60);
    {
        let ag = ctx.agents.get_mut(&planner).unwrap();
        ag.turn_active = false;
        ag.awaiting_start = false;
    }
    run.watchdog_tick(&mut ctx);
    assert_eq!(run.planner, Some(planner));

    // switched off
    run.pattern.settings.first_token_minutes = 0;
    go_silent(&mut ctx, planner, 30 * 60);
    run.watchdog_tick(&mut ctx);
    assert_eq!(run.planner, Some(planner), "first_token_minutes = 0 turns the deadline off");
    assert!(!run.halted());
}

#[test]
fn a_completed_turn_clears_the_first_token_strikes_and_workers_are_keyed_by_task() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
    let planner = ctx.add_idle();
    run.planner = Some(planner);
    run.stage = Stage::Planning;
    go_silent(&mut ctx, planner, 5 * 60);
    run.watchdog_tick(&mut ctx);
    assert_eq!(run.first_token_strikes.get("planner"), Some(&1));
    let second = run.planner.unwrap();
    run.on_turn_done(&mut ctx, second, "completed", None, None);
    assert!(run.first_token_strikes.is_empty(), "a turn that finished means the gateway answered");

    let w = ctx.add_busy();
    run.workers.push(mk_worker("t1", "worker-small", w, WState::Running));
    assert_eq!(run.first_token_key(w).as_deref(), Some("worker:t1"), "a worker's slot is its task, which survives the respawn");
}

// Quiet manager digests --------------------------------------------------------------------

#[test]
fn health_digest_is_skipped_when_nothing_changed_since_the_last_one() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    let planner = ctx.add_idle();
    let orch = ctx.add_busy();
    let w = ctx.add_busy();
    run.planner = Some(planner);
    run.orchestrator = Some(orch);
    run.plan = Some(one_task_phase());
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
    run.workers.push(mk_worker("t1", "worker-small", w, WState::Running));
    let manager = run.ensure_manager(&mut ctx).unwrap();
    ready(&mut ctx, manager);
    let digests = |ctx: &TestCtx| ctx.prompts.iter().filter(|(a, t)| *a == manager && t.contains("[mantra:health]")).count();
    let rearm = |run: &mut Run| {
        run.last_health = Instant::now() - Duration::from_secs((run.pattern.settings.manager_minutes + 1) * 60);
        run.last_tick = Instant::now() - Duration::from_secs(2);
    };

    // the first digest has nothing to compare with
    rearm(&mut run);
    run.tick(&mut ctx);
    assert_eq!(digests(&ctx), 1);

    // nothing happened: the interval elapses, no digest, no journal line, no manager turn
    ctx.prompts.clear();
    let seq = run.pulse_seq;
    for _ in 0..3 {
        rearm(&mut run);
        run.tick(&mut ctx);
    }
    assert_eq!(digests(&ctx), 0, "an unchanged run costs the manager nothing: {:?}", ctx.prompts);
    assert_eq!(run.pulse_seq, seq, "and logs nothing about the skipped digest");

    // a journal entry
    run.log("✓", "green", "something happened");
    rearm(&mut run);
    run.tick(&mut ctx);
    assert_eq!(digests(&ctx), 1, "a journal entry earns a digest");

    // a worker changing state
    ctx.prompts.clear();
    rearm(&mut run);
    run.tick(&mut ctx);
    assert_eq!(digests(&ctx), 0);
    run.workers[0].state = WState::Done;
    rearm(&mut run);
    run.tick(&mut ctx);
    assert_eq!(digests(&ctx), 1, "a worker state change earns a digest");

    // an agent changing state
    ctx.prompts.clear();
    rearm(&mut run);
    run.tick(&mut ctx);
    assert_eq!(digests(&ctx), 0);
    ctx.agents.get_mut(&orch).unwrap().turn_active = false;
    ctx.agents.get_mut(&orch).unwrap().status = Status::Idle;
    rearm(&mut run);
    run.tick(&mut ctx);
    assert_eq!(digests(&ctx), 1, "an agent state change earns a digest");

    // the manager's own comings and goings are not news
    ctx.prompts.clear();
    rearm(&mut run);
    run.tick(&mut ctx);
    assert_eq!(digests(&ctx), 0);
    ctx.agents.get_mut(&manager).unwrap().status = Status::Busy;
    rearm(&mut run);
    run.tick(&mut ctx);
    assert_eq!(digests(&ctx), 0, "{:?}", ctx.prompts);
}

// Full error text --------------------------------------------------------------------------

#[test]
fn halt_keeps_the_full_error_in_the_journal_and_state_but_the_short_form_on_the_pulse() {
    let dir = scratch_dir("fullerr");
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
    run.dir = dir.clone();
    let gate = ctx.add_busy();
    run.gate_agent = Some(gate);
    let err = format!("harness said: {}END", "the model produced a reply the harness cannot parse; ".repeat(12));
    assert!(err.chars().count() > 400);

    // the first failure of a gate gets its free respawn; the second halts
    run.on_turn_done(&mut ctx, gate, "failed", Some(err.clone()), Some(ErrKind::Other));
    assert!(!run.halted());
    let gate2 = run.gate_agent.unwrap();
    run.on_turn_done(&mut ctx, gate2, "failed", Some(err.clone()), Some(ErrKind::Other));
    let h = run.halt.as_ref().expect("halted");
    assert!(h.full.contains(&err), "the halt record keeps the whole error");
    assert!(h.message.ends_with('…') && h.message.chars().count() < err.chars().count(), "the band and the pulse keep the short form: {}", h.message);

    let pulse = run.pulse.iter().find(|p| p.glyph == "⛔").expect("a halted line").text.clone();
    assert!(pulse.ends_with('…') && !pulse.contains("END"), "pulse: {pulse}");
    let journal = std::fs::read_to_string(dir.join("journal.jsonl")).unwrap();
    assert!(journal.lines().any(|l| l.contains("halted (full)") && l.contains(&err)), "the journal has the whole text");
    let saved = RunState::load(&dir).unwrap();
    assert_eq!(saved.halted.as_deref(), Some(h.full.as_str()), "state.json carries the full message");
    assert!(run.overview_text(&ctx).contains(&err), "and so does what the manager reads");
    assert!(ctx.notified.iter().all(|n| !n.contains("END")), "the desktop notification stays short");

    // the sandbox halt keeps the whole command error too
    let dir2 = scratch_dir("fullenv");
    let mut env = Run::new(PathBuf::from("."), no_manager(), "test".into());
    env.dir = dir2.clone();
    let w = ctx.add_busy();
    env.on_environment_broken(&mut ctx, w, format!("bwrap: {}TAIL", "no permissions to create a new namespace; ".repeat(10)));
    let eh = env.halt.as_ref().unwrap();
    assert!(eh.full.contains("TAIL") && !eh.message.contains("TAIL"), "{} / {}", eh.message, eh.full);
    assert!(std::fs::read_to_string(dir2.join("journal.jsonl")).unwrap().contains("TAIL"));
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&dir2);
}

#[test]
fn a_short_halt_message_is_not_journalled_twice() {
    let dir = scratch_dir("shorterr");
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
    run.dir = dir.clone();
    run.halt(&mut ctx, HaltReason::GateExhausted, None, "phase 1 gate not passed".into());
    let journal = std::fs::read_to_string(dir.join("journal.jsonl")).unwrap();
    assert_eq!(journal.lines().filter(|l| l.contains("phase 1 gate not passed")).count(), 1, "{journal}");
    assert_eq!(run.halt.as_ref().unwrap().full, "phase 1 gate not passed");
    let _ = std::fs::remove_dir_all(&dir);
}

// Persistence ------------------------------------------------------------------------------

#[test]
fn state_is_saved_on_a_user_pause_a_dropped_pending_and_by_the_tick_after_any_journal_line() {
    let dir = scratch_dir("persist");
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
    run.dir = dir.clone();

    // a user pause is a halt like any other: a crash right after it must reload as paused
    run.toggle_pause(&mut ctx);
    assert_eq!(RunState::load(&dir).unwrap().halted.as_deref(), Some("paused by you"));
    run.toggle_pause(&mut ctx);
    assert_eq!(RunState::load(&dir).unwrap().halted, None);

    // a transition the pause boundary dropped must not come back on the next resume
    run.pending = Some(Pending::Phase { idx: 1 });
    run.save_state();
    assert!(RunState::load(&dir).unwrap().pending.is_some());
    run.drop_pending("the plan changed");
    assert!(RunState::load(&dir).unwrap().pending.is_none());

    // everything else that journals is flushed by the next tick (once, not per line)
    run.stage = Stage::Planning;
    run.log("☰", "saffron", "plan v1");
    assert_eq!(RunState::load(&dir).unwrap().stage, Stage::Setup, "not written per line");
    run.last_tick = Instant::now() - Duration::from_secs(2);
    run.tick(&mut ctx);
    assert_eq!(RunState::load(&dir).unwrap().stage, Stage::Planning, "the tick flushed it");
    assert!(!run.state_dirty);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn attempts_exhausted_halt_keeps_the_workers_whole_last_failure() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
    run.dir = scratch_dir("attempts");
    let w = ctx.add_idle();
    let err = format!("upstream said: {}END", "connection reset by the gateway; ".repeat(15));
    let mut worker = mk_worker("t1", "worker-small", w, WState::Failed(err.clone()));
    worker.attempt = run.max_attempts();
    run.workers.push(worker);
    let task = Task { id: "t1".into(), role: "worker-small".into(), ..Default::default() };
    let out = run.spawn_task(&mut ctx, task, None, None);
    assert!(out.starts_with("REFUSED"), "{out}");
    let h = run.halt.as_ref().expect("halted");
    assert_eq!(h.reason, HaltReason::AttemptsExhausted);
    assert!(h.full.contains(&err) && h.full.starts_with("t1 has used all "), "{}", h.full);
    assert!(!h.message.contains("END") && h.message.starts_with("t1 has used all "), "the short form keeps the task-id prefix the web view parses: {}", h.message);
    assert_eq!(RunState::load(&run.dir).unwrap().halted.as_deref(), Some(h.full.as_str()));
    let _ = std::fs::remove_dir_all(&run.dir);
}

fn pulse_texts(run: &Run) -> Vec<String> {
    run.pulse.iter().map(|p| p.text.clone()).collect()
}

#[test]
fn a_gateway_outage_retries_a_worker_without_costing_an_attempt() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
    let w = ctx.add_idle();
    run.workers.push(mk_worker("t1", "worker-small", w, WState::Running));
    for _ in 0..(run.pattern.settings.worker_retries + 5) {
        run.on_turn_done(&mut ctx, w, "failed", Some("unexpected status 502 Bad Gateway: upstream provider returned an error".into()), Some(ErrKind::Other));
        assert!(matches!(run.workers[0].state, WState::Retrying(_)), "{:?}", run.workers[0].state);
    }
    assert!(!run.halted());
    assert_eq!(run.workers[0].attempt, 1);
    assert!(!run.retry_counts.contains_key(&w), "the counted ladder was never touched");
    assert_eq!(run.continue_queue.len() as u32, run.pattern.settings.worker_retries + 5);
    assert!(run.continue_queue.iter().all(|(a, _, m)| *a == w && m.contains("not your fault")));
}

#[test]
fn workers_failed_by_a_dead_gateway_respawn_on_resume() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
    let orch = ctx.add_idle();
    let w = ctx.add_idle();
    run.orchestrator = Some(orch);
    run.workers.push(mk_worker("t1", "worker-small", w, WState::Running));
    run.workers[0].prompt = "build the thing".into();
    run.halt(&mut ctx, HaltReason::AgentTurnFailed, Some(orch), "orchestrator: HTTP 503".into());
    // the worker's own turn dies on the gateway while the run is halted
    run.on_turn_done(&mut ctx, w, "failed", Some("503 Service Unavailable".into()), Some(ErrKind::Transient));
    assert!(matches!(run.workers[0].state, WState::Failed(_)) && run.workers[0].transient);
    assert!(run.continue_queue.is_empty(), "nothing retries into a dead gateway while halted");
    // a restart keeps the flag on disk
    run.dir = scratch_dir("outage");
    run.save_state();
    let saved = RunState::load(&run.dir).unwrap();
    assert!(saved.workers[0].transient && saved.workers[0].state == "failed");
    let _ = std::fs::remove_dir_all(&run.dir);

    run.toggle_pause(&mut ctx);
    assert!(!run.halted());
    assert_eq!(run.workers[0].state, WState::Running);
    assert!(!run.workers[0].transient);
    assert_eq!(run.workers[0].agent, Some(w), "the same agent, in the same worktree, carries on");
    assert_eq!(run.workers[0].attempt, 1);
    let prompts: Vec<&String> = ctx.prompts.iter().filter(|(a, _)| *a == w).map(|(_, t)| t).collect();
    assert_eq!(prompts.len(), 1, "{prompts:?}");
    assert!(prompts[0].starts_with("[mantra:resume]") && prompts[0].contains("Continue exactly where you left off") && prompts[0].contains("build the thing"));
    assert!(pulse_texts(&run).iter().any(|t| t == "respawning t1 after the gateway outage"), "{:?}", pulse_texts(&run));
}

#[test]
fn retrying_a_gateway_failed_worker_does_not_spend_an_attempt() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
    let w = ctx.add_idle();
    let before = run.max_attempts();
    run.workers.push(mk_worker("t1", "worker-small", w, WState::Failed("502".into())));
    run.workers[0].transient = true;
    run.retry_worker(&mut ctx, w, None);
    assert_eq!(run.max_attempts(), before + 1);
}

#[test]
fn a_replaced_attempts_agent_is_stopped_at_once() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
    let w = ctx.add_idle();
    run.workers.push(mk_worker("t1", "worker-small", w, WState::Running));
    run.retry_worker(&mut ctx, w, None);
    assert!(ctx.stopped.contains(&w), "the cancelled attempt's agent got stop: {:?}", ctx.stopped);
    assert_eq!(run.workers[0].state, WState::Cancelled);
}

#[test]
fn a_workers_active_time_counts_busy_ticks_only() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
    let (busy, idle) = (ctx.add_busy(), ctx.add_idle());
    run.workers.push(mk_worker("t1", "worker-small", busy, WState::Running));
    run.workers.push(mk_worker("t2", "worker-small", idle, WState::Running));
    run.last_tick = Instant::now() - Duration::from_secs(3);
    run.tick(&mut ctx);
    assert!(run.workers[0].active >= Duration::from_secs(3) && run.workers[0].active < Duration::from_secs(5), "{:?}", run.workers[0].active);
    assert_eq!(run.workers[1].active, Duration::ZERO, "an idle agent is not at work");
}

#[test]
fn a_phase_report_comes_from_the_newest_attempt_and_says_superseded() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
    run.dir = scratch_dir("phase-report-newest");
    run.plan = Some(class_phase(1, 0));
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Gate { round: 0 } };
    let (a1, a2, c) = (ctx.add_idle(), ctx.add_idle(), ctx.add_idle());
    run.workers.push(mk_worker("b0", "worker-big", a1, WState::Cancelled));
    let mut second = mk_worker("b0", "worker-big", a2, WState::Done);
    second.attempt = 2;
    run.workers.push(second);
    let mut gone = mk_worker("b1", "worker-big", c, WState::Cancelled);
    gone.superseded = true;
    run.workers.push(gone);
    run.begin_handoff(&mut ctx, 0);
    let b0 = std::fs::read_to_string(run.dir.join("phase-1/b0.md")).unwrap();
    assert!(b0.contains("state: Done\nattempt: 2"), "{b0}");
    let b1 = std::fs::read_to_string(run.dir.join("phase-1/b1.md")).unwrap();
    assert!(b1.contains("state: Done (superseded by a plan revision)"), "{b1}");
    let _ = std::fs::remove_dir_all(&run.dir);
}

#[test]
fn an_exit_code_reads_as_a_word_and_a_number() {
    assert_eq!(crate::util::exit_label(Some(0i64)), "exit 0");
    assert_eq!(crate::util::exit_label(Some(137i32)), "exit 137");
    assert_eq!(crate::util::exit_label::<i64>(None), "exit ?");
}

#[test]
fn a_rejected_effort_is_remapped_and_the_turn_retried_once() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
    let w = ctx.add_idle();
    run.workers.push(mk_worker("t1", "worker-small", w, WState::Running));
    {
        let ag = ctx.agents.get_mut(&w).unwrap();
        ag.effort = "medium".into();
        ag.model_alias = "deepseek".into();
    }
    let err = "unexpected status 400: DeepSeek V4.1 reasoning_effort must be low, high, xhigh, max";
    run.on_turn_done(&mut ctx, w, "failed", Some(err.into()), Some(ErrKind::ProviderRejected));
    assert_eq!(ctx.efforts_set, vec![(w, "low".to_string())], "medium sits between low and high: the lower one wins the tie");
    assert!(!run.halted());
    assert_eq!(run.continue_queue.len(), 1);
    assert!(pulse_texts(&run).iter().any(|t| t == "a: effort medium is not offered by deepseek — using low" || t.ends_with("effort medium is not offered by deepseek — using low")), "{:?}", pulse_texts(&run));
    // a model that rejects the new one too is the ordinary 400 again
    ctx.agents.get_mut(&w).unwrap().effort = "low".into();
    run.on_turn_done(&mut ctx, w, "failed", Some(err.into()), Some(ErrKind::ProviderRejected));
    assert_eq!(ctx.efforts_set.len(), 1);
    assert!(run.halted() && run.halt.as_ref().unwrap().reason == HaltReason::ProviderRejected);
}

#[test]
fn an_effort_error_that_lists_nothing_falls_through() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
    let w = ctx.add_idle();
    run.workers.push(mk_worker("t1", "worker-small", w, WState::Running));
    run.on_turn_done(&mut ctx, w, "failed", Some("unexpected status 400: invalid reasoning_effort".into()), Some(ErrKind::ProviderRejected));
    assert!(ctx.efforts_set.is_empty());
    assert!(run.halted() && run.halt.as_ref().unwrap().reason == HaltReason::ProviderRejected);
}

#[test]
fn the_halt_for_a_dead_gateway_says_what_to_do() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
    let planner = ctx.add_idle();
    let w = ctx.add_busy();
    run.planner = Some(planner);
    run.stage = Stage::Planning;
    run.turn_fail_retried.insert("planner".into());
    run.retry_counts.insert(planner, 99);
    run.workers.push(mk_worker("t1", "worker-small", w, WState::Running));
    // a busy agent that has not failed: that is not "the gateway is down"
    run.on_turn_done(&mut ctx, planner, "failed", Some("502 Bad Gateway".into()), Some(ErrKind::Transient));
    assert!(run.halted());
    assert!(!run.halt.as_ref().unwrap().message.contains("gateway looks down"), "{}", run.halt.as_ref().unwrap().message);

    let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
    run.planner = Some(planner);
    run.stage = Stage::Planning;
    run.turn_fail_retried.insert("planner".into());
    run.retry_counts.insert(planner, 99);
    run.workers.push(mk_worker("t1", "worker-small", w, WState::Running));
    run.transport_errs.insert(w, Instant::now());
    run.on_turn_done(&mut ctx, planner, "failed", Some("502 Bad Gateway".into()), Some(ErrKind::Transient));
    let h = run.halt.as_ref().expect("halted");
    assert!(h.message.ends_with("the model gateway looks down — the run resumes where it was once you press resume (space / Resume); failed workers are respawned automatically"), "{}", h.message);
    assert_eq!(run.halt_hint(), "space resumes once the gateway is back — the workers the outage took are respawned for free", "the band offers no key a halted run refuses");
    assert!(h.full.ends_with("failed workers are respawned automatically"));
}

/// The dead-gateway halt of a fixed agent is not handed to the manager either: it would
/// fail its turn on the same gateway. A planner failure that is not the gateway still is.
#[test]
fn a_dead_gateway_halt_does_not_wake_the_manager() {
    for (outage, err) in [(true, "502 Bad Gateway"), (false, "the model returned an empty response")] {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        run.dir = scratch_dir("gateway-manager");
        assert!(run.has_manager());
        let planner = ctx.add_idle();
        let w = ctx.add_busy();
        run.planner = Some(planner);
        run.stage = Stage::Planning;
        run.turn_fail_retried.insert("planner".into());
        run.retry_counts.insert(planner, 99);
        run.workers.push(mk_worker("t1", "worker-small", w, WState::Running));
        run.transport_errs.insert(w, Instant::now());
        run.on_turn_done(&mut ctx, planner, "failed", Some(err.into()), if outage { Some(ErrKind::Transient) } else { None });
        let h = run.halt.as_ref().expect("halted");
        assert_eq!(h.reason, HaltReason::AgentTurnFailed);
        assert_eq!(h.message.ends_with(GATEWAY_DOWN_HINT), outage, "{}", h.message);
        assert_eq!(ctx.spawns.iter().any(|s| s.name == "manager"), !outage, "{err}: the manager is asked only when it is not the gateway");
        let _ = std::fs::remove_dir_all(&run.dir);
    }
}

// A gateway outage under a fix worker ---------------------------------------------------------

/// A `TestCtx` whose jobs run: the closure's result is kept with its tag, for the test to
/// hand back through `on_job` — the way the app's job runner would.
struct JobCtx(TestCtx, Vec<(JobTag, JobOut)>);
impl Ctx for JobCtx {
    fn spawn(&mut self, r: SpawnReq) -> AgentId {
        self.0.spawn(r)
    }
    fn prompt(&mut self, a: AgentId, t: String) {
        self.0.prompt(a, t)
    }
    fn prompt_mode(&mut self, a: AgentId, t: String, m: Send) {
        self.0.prompt_mode(a, t, m)
    }
    fn interrupt(&mut self, a: AgentId) {
        self.0.interrupt(a)
    }
    fn compact(&mut self, a: AgentId) {
        self.0.compact(a)
    }
    fn stop(&mut self, a: AgentId, x: bool) {
        self.0.stop(a, x)
    }
    fn tool_result(&mut self, a: AgentId, r: Value, t: String, ok: bool) {
        self.0.tool_result(a, r, t, ok)
    }
    fn set_effort(&mut self, a: AgentId, e: &str) -> String {
        self.0.set_effort(a, e)
    }
    fn agent(&self, a: AgentId) -> Option<&Agent> {
        self.0.agent(a)
    }
    fn job(&mut self, tag: JobTag, f: Box<dyn FnOnce() -> JobOut + std::marker::Send>) {
        self.1.push((tag, f()));
    }
    fn notify(&mut self, t: &str) {
        self.0.notify(t)
    }
}

/// Phase 1 at gate round 1's fix: a planner that is idle, a fix worker `fix-1` on the
/// integration copy `integ`, and a pattern with a manager that has not been spawned yet.
fn run_in_fix_round(ctx: &mut TestCtx, integ: &Path) -> (Run, AgentId, AgentId) {
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    run.dir = scratch_dir("fix-outage");
    let planner = ctx.add_idle();
    ready(ctx, planner);
    run.planner = Some(planner);
    run.plan = Some(one_task_phase());
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Fix { round: 1 } };
    run.ws = Some(crate::engine::git::Workspace { worktree: true, repo: PathBuf::from("."), integ: integ.to_path_buf(), branch: "mantra/r".into(), base_branch: "main".into(), git_common_dir: None, note: None, base_commit: None, initial_commit: false, nested: vec![] });
    let w = ctx.add_idle();
    let mut fix = mk_worker("fix-1", "worker-small", w, WState::Running);
    fix.adhoc = true;
    fix.wt = Some(integ.to_path_buf());
    run.workers.push(fix);
    (run, planner, w)
}

/// The gateway outlasted the fix worker's retries: the run halts for the user with the
/// gateway wording, and neither the manager nor the planner — who run through the same
/// gateway — is woken. The resume respawns the fix worker where it was: on the integration
/// copy, for free.
#[test]
fn a_fix_worker_the_gateway_failed_halts_without_waking_anyone_and_is_respawned_on_resume() {
    let integ = scratch_dir("fix-outage-integ");
    std::fs::create_dir_all(&integ).unwrap();
    let mut ctx = TestCtx::new();
    let (mut run, planner, w) = run_in_fix_round(&mut ctx, &integ);
    run.gateway_streak.insert(w, Run::GATEWAY_RETRIES);
    let attempts_before = run.max_attempts();
    run.on_turn_done(&mut ctx, w, "failed", Some("unexpected status 503 Service Unavailable".into()), Some(ErrKind::Transient));

    let h = run.halt.as_ref().expect("halted");
    assert_eq!(h.reason, HaltReason::AgentTurnFailed, "the dead-gateway halt, whose band and web hint already fit");
    assert!(h.message.starts_with("fix-1: unexpected status 503") && h.message.ends_with(GATEWAY_DOWN_HINT), "{}", h.message);
    assert!(run.workers[0].transient && matches!(run.workers[0].state, WState::Failed(_)));
    assert!(!ctx.spawns.iter().any(|s| s.name == "manager"), "no manager is spawned for an outage: {:?}", ctx.spawns.iter().map(|s| s.name.clone()).collect::<Vec<_>>());
    assert!(run.manager_escalation.is_none() && !run.escalation_open, "nobody is escalated to");
    assert!(ctx.prompts.iter().all(|(a, _)| *a != planner), "the planner is not woken: {:?}", ctx.prompts);
    assert!(pulse_texts(&run).iter().any(|t| t.starts_with("not escalated")), "{:?}", pulse_texts(&run));

    // the gateway is back; the user presses space — the fix worker's process is gone
    ctx.agents.get_mut(&w).unwrap().status = Status::Stopped;
    let mut jobs = JobCtx(ctx, vec![]);
    run.resume(&mut jobs);
    assert!(!run.halted());
    assert_eq!(run.max_attempts(), attempts_before + 1, "the outage's attempt was not one");
    assert!(pulse_texts(&run).iter().any(|t| t == "respawning fix-1 after the gateway outage"), "{:?}", pulse_texts(&run));
    let fresh = run.workers.iter().rev().find(|w| w.task.id == "fix-1").expect("a new attempt");
    assert!(fresh.attempt == 2 && fresh.adhoc && fresh.state == WState::Preparing, "attempt {} {:?}", fresh.attempt, fresh.state);
    assert_eq!(run.workers[0].state, WState::Cancelled, "the dead attempt is closed");
    assert!(matches!(run.stage, Stage::Phase { step: PhaseStep::Fix { round: 1 }, .. }), "still the same fix round: {:?}", run.stage);
    // its workspace job answers with the integration copy, not a worker worktree
    let (tag, out) = jobs.1.pop().expect("the fix worker's workspace job ran");
    assert!(matches!(&tag, JobTag::WorkerWt { task, attempt: 2 } if task == "fix-1"), "{tag:?}");
    assert!(matches!(&out, JobOut::Wt(Ok((dir, branch))) if *dir == integ && branch.is_empty()), "the integration copy");
    run.on_job(&mut jobs, tag, out);
    let spawned = jobs.0.spawned("fix-1");
    assert_eq!(spawned.len(), 1, "{:?}", jobs.0.spawns.iter().map(|s| s.name.clone()).collect::<Vec<_>>());
    assert_eq!(spawned[0].cwd, integ);
    assert!(spawned[0].git_write, "a fix worker on the integration copy asks for a writable .git (the app lifts Codex's sandbox)");
    let fresh = run.workers.iter().rev().find(|w| w.task.id == "fix-1").unwrap();
    assert!(fresh.state == WState::Running && fresh.wt.as_ref() == Some(&integ));
    assert!(jobs.0.prompts.iter().any(|(a, _)| Some(*a) == fresh.agent), "the fresh fix worker is given its task");
    let _ = std::fs::remove_dir_all(&run.dir);
    let _ = std::fs::remove_dir_all(&integ);
}

/// A fix worker that fails for its own reasons is still the chain of command's problem: the
/// manager gets it first, as before.
#[test]
fn a_fix_worker_that_fails_for_its_own_reasons_still_escalates() {
    let integ = scratch_dir("fix-fail-integ");
    let mut ctx = TestCtx::new();
    let (mut run, _planner, w) = run_in_fix_round(&mut ctx, &integ);
    run.on_turn_done(&mut ctx, w, "failed", Some("the model returned an empty response".into()), None);
    let h = run.halt.as_ref().expect("halted");
    assert_eq!(h.reason, HaltReason::AttemptsExhausted);
    assert!(h.message.contains("fix-1 could not fix what gate round 1 found"), "{}", h.message);
    assert!(!h.message.contains("gateway looks down"));
    assert!(!run.workers[0].transient);
    assert!(ctx.spawns.iter().any(|s| s.name == "manager"), "the manager is spawned and asked");
    assert!(run.manager_escalation.is_some());
    let _ = std::fs::remove_dir_all(&run.dir);
}

/// Only a failed turn's own error is read for an outage: a blocked fix worker whose report
/// talks about timeouts and a 503 from the code under test is still escalated.
#[test]
fn a_blocked_report_that_mentions_timeouts_is_not_an_outage() {
    let integ = scratch_dir("fix-blocked-integ");
    let mut ctx = TestCtx::new();
    let (mut run, _planner, w) = run_in_fix_round(&mut ctx, &integ);
    ctx.agents.get_mut(&w).unwrap().final_message = Some("status: blocked\nthe API test gets a timeout and a 503 from the stub server; the gate's check cannot pass here".into());
    run.on_turn_done(&mut ctx, w, "completed", None, None);
    let h = run.halt.as_ref().expect("halted");
    assert_eq!(h.reason, HaltReason::AttemptsExhausted);
    assert!(h.message.contains("it is blocked") && !h.message.contains("gateway looks down"), "{}", h.message);
    assert!(run.manager_escalation.is_some(), "the chain of command is asked");
    let _ = std::fs::remove_dir_all(&run.dir);
}

/// `halt_and_escalate` itself, handed a transport error (a task out of attempts whose last
/// failure was a 503): the halt stands, with the gateway wording, and nobody is woken — the
/// manager and the planner share the outage.
#[test]
fn an_escalated_halt_over_a_transport_error_wakes_nobody() {
    for pattern in [Pattern::builtin(), no_manager()] {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), pattern, "test".into());
        run.dir = scratch_dir("escalate-503");
        let planner = ctx.add_idle();
        run.planner = Some(planner);
        let w = ctx.add_idle();
        let mut worker = mk_worker("t1", "worker-small", w, WState::Failed("unexpected status 503 Service Unavailable".into()));
        worker.attempt = run.max_attempts();
        run.workers.push(worker);
        let out = run.spawn_task(&mut ctx, Task { id: "t1".into(), role: "worker-small".into(), ..Default::default() }, None, None);
        assert!(out.starts_with("REFUSED"), "{out}");
        let h = run.halt.as_ref().expect("halted");
        assert_eq!(h.reason, HaltReason::AttemptsExhausted, "the halt keeps its own reason (the web view offers Retry t1 from it)");
        assert!(h.message.ends_with(GATEWAY_DOWN_RETRY_HINT), "no worker is left to respawn: {}", h.message);
        assert!(!run.escalation_open && run.manager_escalation.is_none());
        assert!(!ctx.spawns.iter().any(|s| s.name == "manager"));
        assert!(ctx.prompts.iter().all(|(a, _)| *a != planner), "{:?}", ctx.prompts);
        let _ = std::fs::remove_dir_all(&run.dir);
    }
}

// Per-agent model switches ------------------------------------------------------------------

fn known(aliases: &[(&str, u64)]) -> HashMap<String, u64> {
    aliases.iter().map(|(a, w)| (a.to_string(), *w)).collect()
}

/// `set_agent_model` is a switch for that agent: the task's next attempt keeps it, the role's
/// other tasks do not see it, the pattern copy is untouched, and switching back forgets it.
#[test]
fn a_live_model_switch_sticks_to_that_task_and_not_to_its_role() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
    run.dir = scratch_dir("agent-model");
    run.set_known_models(known(&[("luna", 200_000), ("nova", 128_000)]));
    let role_model = run.role("worker-small").model.clone();
    run.plan = Some(one_task_phase());
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
    let w = ctx.add_idle();
    ready(&mut ctx, w);
    run.workers.push(mk_worker("t1", "worker-small", w, WState::Running));
    run.on_ready(&mut ctx, w, false, false);
    run.set_agent_model(w, "nova", "by you");
    assert_eq!(run.workers[0].model.as_deref(), Some("nova"));
    assert_eq!(run.pattern.role("worker-small").unwrap().model, role_model, "the role is not rewritten");
    assert_eq!(run.agent_meta[&w].model_alias, "nova", "state.json carries it");
    // a respawn of the same task runs on the switched model …
    run.retry_worker(&mut ctx, w, None);
    let again = run.workers.iter().rev().find(|w| w.task.id == "t1").map(|w| (w.attempt, w.model.clone())).unwrap();
    assert_eq!(again, (2, Some("nova".to_string())));
    run.on_job(&mut ctx, JobTag::WorkerWt { task: "t1".into(), attempt: 2 }, JobOut::Wt(Ok((PathBuf::from("."), String::new()))));
    assert_eq!(ctx.spawned("t1").last().unwrap().model, "nova");
    // … another task of the same role does not
    run.spawn_task(&mut ctx, Task { id: "t2".into(), role: "worker-small".into(), ..Default::default() }, None, None);
    run.on_job(&mut ctx, JobTag::WorkerWt { task: "t2".into(), attempt: 1 }, JobOut::Wt(Ok((PathBuf::from("."), String::new()))));
    assert_eq!(ctx.spawned("t2").last().unwrap().model, role_model);
    // switching back to the role's model is no longer a choice of its own
    let a2 = run.workers.iter().rev().find(|w| w.task.id == "t1").and_then(|w| w.agent).unwrap();
    run.set_agent_model(a2, &role_model, "by you");
    assert_eq!(run.workers.iter().rev().find(|w| w.task.id == "t1").unwrap().model, None);
    let _ = std::fs::remove_dir_all(&run.dir);
}

/// A fixed agent's switch belongs to its slot: the planner respawned keeps the model, and
/// so does the next orchestrator of the run — not the role's model from the pattern.
#[test]
fn a_fixed_agents_model_switch_survives_its_respawn() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
    run.dir = scratch_dir("slot-model");
    run.set_known_models(known(&[("astra", 200_000), ("nova", 128_000)]));
    run.stage = Stage::Planning;
    let p = run.spawn_planner(&mut ctx);
    let role_model = run.role(&run.pattern.flow.planner.clone()).model.clone();
    assert_eq!(ctx.spawned("planner")[0].model, role_model);
    run.set_agent_model(p, "nova", "by you");
    run.respawn(&mut ctx, p, None).unwrap();
    assert_eq!(ctx.spawned("planner").last().unwrap().model, "nova", "the respawned planner keeps the switch");
    // the orchestrator slot is its own
    run.plan = Some(one_task_phase());
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
    let o = run.spawn_orchestrator(&mut ctx);
    assert_eq!(ctx.spawned("orchestrator")[0].model, run.role("orchestrator").model, "untouched by the planner's switch");
    run.set_agent_model(o, "nova", "by you");
    run.respawn(&mut ctx, o, None).unwrap();
    assert_eq!(ctx.spawned("orchestrator").last().unwrap().model, "nova");
    let _ = std::fs::remove_dir_all(&run.dir);
}

#[test]
fn a_backend_switch_checkpoint_never_pairs_new_model_with_old_session() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
    run.dir = scratch_dir("switch-checkpoint");
    run.set_known_models(known(&[("astra", 200_000), ("nova", 128_000)]));
    run.stage = Stage::Planning;
    let planner = run.spawn_planner(&mut ctx);
    ctx.agents.get_mut(&planner).unwrap().thread_id = Some("old-backend-only".into());
    run.on_ready(&mut ctx, planner, false, false);
    run.forget_agent_session(planner);
    run.set_agent_model(planner, "nova", "by you");
    let saved = RunState::load(&run.dir).unwrap();
    let slot = saved.agents.iter().find(|a| a.slot == "planner").unwrap();
    assert_eq!(slot.model_alias, "nova");
    assert_eq!(slot.thread_id, None, "a crash before Ready must start a fresh compatible session");
    let _ = std::fs::remove_dir_all(&run.dir);
}

/// A finale agent's switch is for its step: a respawn of that step keeps it, the next step —
/// another role — starts on that role's own model.
#[test]
fn a_finale_switch_does_not_carry_over_to_the_next_step() {
    let mut ctx = TestCtx::new();
    let mut pat = no_manager();
    pat.flow.finale = vec![
        crate::engine::pattern::FinaleStep { role: "qa".into(), task: "review".into(), may_spawn: false },
        crate::engine::pattern::FinaleStep { role: "worker-big".into(), task: "docs".into(), may_spawn: false },
    ];
    let mut run = Run::new(PathBuf::from("."), pat, "test".into());
    run.dir = scratch_dir("finale-model");
    run.plan = Some(one_task_phase());
    run.start_finale(&mut ctx, 0);
    let f = run.finale_agent.expect("a finale agent");
    run.set_agent_model(f, "nova", "by you");
    run.respawn(&mut ctx, f, None).unwrap();
    assert_eq!(ctx.spawned("qa").last().unwrap().model, "nova", "the respawned step keeps the switch");
    run.start_finale(&mut ctx, 1);
    assert_eq!(ctx.spawned("worker-big").last().unwrap().model, run.role("worker-big").model, "the next step is another role's");
    assert!(run.slot_models.is_empty());
    let _ = std::fs::remove_dir_all(&run.dir);
}

/// A switched agent that goes silent twice falls back like any other: the role's fallback
/// replaces the model the user picked, which no longer outranks the role.
#[test]
fn the_fallback_replaces_a_switched_model_that_went_silent() {
    let mut ctx = TestCtx::new();
    let mut pat = no_manager();
    pat.roles.get_mut("planner").unwrap().fallback = Some("luna".into());
    let mut run = Run::new(PathBuf::from("."), pat, "test".into());
    run.dir = scratch_dir("slot-fallback");
    run.stage = Stage::Planning;
    let p = run.spawn_planner(&mut ctx);
    run.set_agent_model(p, "nova", "by you");
    ctx.agents.get_mut(&p).unwrap().model_alias = "nova".into();
    let key = run.first_token_key(p).unwrap();
    run.first_token_strikes.insert(key, 1);
    run.on_first_token_deadline(&mut ctx, p, Duration::from_secs(300));
    assert_eq!(ctx.spawned("planner").last().unwrap().model, "luna", "on the fallback, not back on the switch");
    assert_eq!(run.slot_models.get("planner").map(String::as_str), Some("luna"), "the fallback replaces the switch as the slot's pin");
    let _ = std::fs::remove_dir_all(&run.dir);
}

#[test]
fn billing_fallback_keeps_worker_files_and_attempt_and_alerts_the_owner() {
    let mut ctx = TestCtx::new();
    let mut pat = no_manager();
    pat.roles.get_mut("worker-small").unwrap().fallback = Some("claude-fallback".into());
    let mut run = Run::new(PathBuf::from("."), pat, "test".into());
    run.dir = scratch_dir("billing-fallback");
    run.plan = Some(one_task_phase());
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
    let a = ctx.add_busy();
    ctx.agents.get_mut(&a).unwrap().model_alias = "openrouter-primary".into();
    let mut worker = mk_worker("t1", "worker-small", a, WState::Running);
    worker.wt = Some(PathBuf::from("existing-worktree"));
    worker.branch = "existing-branch".into();
    worker.prompt = "Finish the partial implementation".into();
    worker.attempt = run.max_attempts();
    run.workers.push(worker);
    run.retry_counts.insert(a, 2);
    run.continue_queue.push((a, Instant::now(), "old retry".into()));
    run.on_turn_done(&mut ctx, a, "failed", Some("HTTP 402: Insufficient credits".into()), Some(ErrKind::ProviderRejected));
    let fresh = run.workers[0].agent.unwrap();
    assert_ne!(fresh, a);
    assert!(!run.halted());
    assert_eq!(run.workers.len(), 1, "billing does not create a counted attempt");
    assert_eq!(run.workers[0].attempt, run.max_attempts());
    assert_eq!(run.workers[0].branch, "existing-branch");
    assert_eq!(ctx.spawned("t1")[0].cwd, PathBuf::from("existing-worktree"));
    assert_eq!(ctx.spawned("t1")[0].model, "claude-fallback", "a different backend alias is accepted");
    assert_eq!(run.workers[0].model.as_deref(), Some("claude-fallback"));
    assert!(run.continue_queue.is_empty() && !run.retry_counts.contains_key(&a));
    assert!(run.alerts.iter().any(|alert| alert.contains("fallback claude-fallback")));
    assert!(ctx.notified.iter().any(|alert| alert.contains("openrouter-primary → fallback claude-fallback (usage limit)")), "{:?}", ctx.notified);
    // A late error from the stopped primary cannot restore the halt.
    run.on_turn_done(&mut ctx, a, "failed", Some("HTTP 402: Insufficient credits".into()), Some(ErrKind::UsageLimit));
    assert!(!run.halted());
    // The fallback is tried once; exhaustion there has no recursive fallback loop.
    run.on_turn_done(&mut ctx, fresh, "failed", Some("HTTP 429: quota exhausted".into()), Some(ErrKind::Transient));
    assert_eq!(run.halt.as_ref().unwrap().reason, HaltReason::UsageLimit);
    let _ = std::fs::remove_dir_all(&run.dir);
}

#[test]
fn quota_fallback_restarts_a_fixed_slot_before_transport_retries() {
    let mut ctx = TestCtx::new();
    let mut pat = no_manager();
    pat.roles.get_mut("planner").unwrap().fallback = Some("fallback-api".into());
    let mut run = Run::new(PathBuf::from("."), pat, "test".into());
    run.dir = scratch_dir("quota-fixed-fallback");
    run.stage = Stage::Planning;
    let a = run.spawn_planner(&mut ctx);
    run.on_turn_done(&mut ctx, a, "failed", Some("HTTP 429: quota exceeded".into()), Some(ErrKind::Transient));
    assert_ne!(run.planner, Some(a));
    assert_eq!(ctx.spawned("planner").last().unwrap().model, "fallback-api");
    assert!(!run.halted() && run.continue_queue.is_empty());
    let fresh = run.planner.unwrap();
    ctx.agents.get_mut(&fresh).unwrap().model_alias = "fallback-api".into();
    run.on_ready(&mut ctx, fresh, false, false);
    run.save_state();
    assert_eq!(RunState::load(&run.dir).unwrap().agents.iter().find(|agent| agent.slot == "planner").unwrap().model_alias, "fallback-api");
    let _ = std::fs::remove_dir_all(&run.dir);
}

#[test]
fn respawn_resolves_only_its_own_failure_and_discards_old_retry_state() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
    run.dir = scratch_dir("respawn-recovery");
    run.stage = Stage::Planning;
    let a = run.spawn_planner(&mut ctx);
    run.halt(&mut ctx, HaltReason::UsageLimit, Some(a), "primary quota exhausted".into());
    run.alerts.push("unrelated owner alert".into());
    run.retry_counts.insert(a, 2);
    run.continue_queue.push((a, Instant::now(), "old retry".into()));
    assert_eq!(run.respawn(&mut ctx, a, None).unwrap(), RESPAWNED);
    let fresh = run.planner.unwrap();
    assert_ne!(fresh, a);
    assert!(!run.halted());
    assert_eq!(run.alerts, vec!["unrelated owner alert"]);
    assert!(!run.retry_counts.contains_key(&a) && run.continue_queue.is_empty());
    assert_eq!(RunState::load(&run.dir).unwrap().halted, None);
    run.on_turn_done(&mut ctx, a, "failed", Some("HTTP 402".into()), Some(ErrKind::UsageLimit));
    assert!(!run.halted(), "late old-process failure is ignored");
    let other = ctx.add_idle();
    run.orchestrator = Some(other);
    run.halt(&mut ctx, HaltReason::ProviderRejected, Some(other), "orchestrator request rejected".into());
    run.respawn(&mut ctx, fresh, None).unwrap();
    assert_eq!(run.halt.as_ref().unwrap().agent, Some(other));
    assert!(run.alerts.iter().any(|alert| alert == "orchestrator request rejected"));
    let _ = std::fs::remove_dir_all(&run.dir);
}

// The context cap ---------------------------------------------------------------------------

/// Every spawn is told `min(model window, context_cap)`: a big model is capped, a small one
/// keeps its own window, a halved assumption is capped too, and `0` leaves the window alone.
#[test]
fn the_context_cap_bounds_the_window_a_spawn_is_told() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
    run.dir = scratch_dir("context-cap");
    assert_eq!(run.pattern.settings.context_cap, 200_000, "the built-in default");
    let planner_model = run.role(&run.pattern.flow.planner.clone()).model.clone();
    let worker_model = run.role("worker-small").model.clone();
    run.set_known_models(known(&[(planner_model.as_str(), 1_000_000), (worker_model.as_str(), 128_000)]));
    run.stage = Stage::Planning;
    run.spawn_planner(&mut ctx);
    assert_eq!(ctx.spawned("planner")[0].context_override, Some(200_000), "a 1M model is told 200k");
    run.plan = Some(one_task_phase());
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
    run.spawn_task(&mut ctx, Task { id: "t1".into(), role: "worker-small".into(), ..Default::default() }, None, None);
    run.on_job(&mut ctx, JobTag::WorkerWt { task: "t1".into(), attempt: 1 }, JobOut::Wt(Ok((PathBuf::from("."), String::new()))));
    assert_eq!(ctx.spawned("t1")[0].context_override, Some(128_000), "a 128k model keeps its window");
    // a halved assumption from an earlier attempt is still capped
    let role_of = |model: &str| Role { model: model.into(), ..Role::default() };
    let (planner_role, worker_role, unknown_role) = (role_of(&planner_model), role_of(&worker_model), role_of("unknown-alias"));
    assert_eq!(run.context_for(&planner_role, Some(100_000)), Some(100_000));
    assert_eq!(run.context_for(&planner_role, Some(400_000)), Some(200_000));
    assert_eq!(run.context_for(&unknown_role, None), Some(200_000), "nothing known: the cap stands");
    // 0 = the model's own window, whatever it is
    run.pattern.settings.context_cap = 0;
    assert_eq!(run.context_for(&planner_role, None), Some(1_000_000));
    assert_eq!(run.context_for(&planner_role, Some(100_000)), Some(100_000));
    assert_eq!(run.context_for(&worker_role, Some(400_000)), Some(128_000), "an override cannot inflate the model's limit");
    assert_eq!(run.context_for(&unknown_role, None), None);
    let orchestrator_window = run.context_for(&run.role(&run.pattern.flow.orchestrator), None);
    run.respawn_orchestrator(&mut ctx, None);
    assert_eq!(ctx.spawned("orchestrator")[0].context_override, orchestrator_window);
    let _ = std::fs::remove_dir_all(&run.dir);
}

/// A role's `context` joins the cap and the model's window: the smallest of the three wins, on
/// whatever model the spawn really uses, and `context_ceiling` reports it for a live switch.
#[test]
fn a_role_context_limit_is_one_more_ceiling() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
    run.dir = scratch_dir("role-context");
    run.set_known_models(known(&[("big", 1_000_000), ("small", 128_000)]));
    run.pattern.settings.context_cap = 500_000;
    let role = |model: &str, context: Option<u64>| Role { model: model.into(), context, ..Role::default() };
    assert_eq!(run.context_for(&role("big", Some(250_000)), None), Some(250_000));
    assert_eq!(run.context_for(&role("small", Some(250_000)), None), Some(128_000), "the model's window is smaller");
    assert_eq!(run.context_for(&role("big", None), None), Some(500_000));
    run.pattern.settings.context_cap = 0;
    assert_eq!(run.context_for(&role("big", None), None), Some(1_000_000));
    assert_eq!(run.context_for(&role("nope", Some(250_000)), None), Some(250_000), "unknown alias: the role limit stands");
    run.pattern.settings.context_cap = 300_000;
    assert_eq!(run.context_for(&role("nope", Some(250_000)), None), Some(250_000));
    // the planner's role limit reaches the spawn and the ceiling
    let planner = run.pattern.flow.planner.clone();
    run.pattern.roles.get_mut(&planner).unwrap().context = Some(64_000);
    run.stage = Stage::Planning;
    run.spawn_planner(&mut ctx);
    assert_eq!(ctx.spawned("planner")[0].context_override, Some(64_000));
    let pa = run.planner.unwrap();
    assert_eq!(run.context_ceiling(pa), Some(64_000));
    run.pattern.roles.get_mut(&planner).unwrap().context = None;
    assert_eq!(run.context_ceiling(pa), Some(300_000));
    let _ = std::fs::remove_dir_all(&run.dir);
}

/// A model with a big window of its own, told 200k by the cap, that still fills its context
/// is not on an *assumed* window: it is not halved. One the registry only assumes 200k for is
/// — the window it was told, which a smaller cap makes smaller than 200k.
#[test]
fn a_capped_window_is_not_mistaken_for_an_assumed_one() {
    // (the model's window in the registry, the window the agent was told, what it is halved to)
    let assumed = crate::discover::ASSUMED_CONTEXT;
    for (window, told, halved) in [(1_000_000u64, 200_000u64, None), (assumed, assumed, Some(assumed / 2)), (assumed, 150_000, Some(75_000)), (128_000, 128_000, None)] {
        let mut ctx = TestCtx::new();
        let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
        run.dir = scratch_dir("cap-shrink");
        run.set_known_models(known(&[("big", window)]));
        let w = ctx.add_idle();
        let ag = ctx.agents.get_mut(&w).unwrap();
        ag.model_alias = "big".into();
        ag.ctx_window = Some(told);
        run.workers.push(mk_worker("t1", "worker-small", w, WState::Running));
        run.shrink_assumed_context(&mut ctx, w);
        assert_eq!(run.workers[0].context_override, halved, "window {window}, told {told}: an assumed window under a smaller cap is halved too");
        let _ = std::fs::remove_dir_all(&run.dir);
    }
}

// The owner file ----------------------------------------------------------------------------

/// A run claims its directory while it is live, refreshes the claim with every flush, and
/// gives it back when it ends — or when the `Run` is dropped by a process that is still alive.
#[test]
fn a_run_owns_its_directory_while_it_is_live() {
    use crate::engine::state::Owner;
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
    run.dir = scratch_dir("owner");
    let dir = run.dir.clone();
    run.start(&mut ctx);
    let o = Owner::load(&dir).expect("owner.json written at start");
    assert_eq!(o.pid, std::process::id());
    assert!(o.alive(), "a live owner: another Mantra's resume of it would be refused");
    run.stage = Stage::Planning;
    run.save_state();
    assert!(Owner::load(&dir).is_some(), "refreshed with the flush");
    run.fail_run(&mut ctx, "boom".into());
    assert!(Owner::load(&dir).is_none(), "released once the run is over");
    // a live run dropped by its process (a clean quit) releases it too
    let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
    run.dir = dir.clone();
    run.start(&mut ctx);
    assert!(Owner::load(&dir).is_some());
    drop(run);
    assert!(Owner::load(&dir).is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

// MCP servers and vision ------------------------------------------------------------------

/// Every way an agent of a role comes to be hands the role's `mcp` on: planner, orchestrator,
/// manager, workers, the phase gate, a finale step, and one re-attached on resume.
#[test]
fn every_spawn_carries_its_roles_mcp_servers() {
    let mut ctx = TestCtx::new();
    let mut p = Pattern::builtin();
    for (name, r) in p.roles.iter_mut() {
        r.mcp = vec![format!("{name}-tools"), "*".into()];
    }
    let mut run = Run::new(PathBuf::from("."), p, "test".into());
    run.plan = Some(mixed_phase());
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
    run.spawn_planner(&mut ctx);
    run.spawn_orchestrator(&mut ctx);
    run.ensure_manager(&mut ctx);
    for t in mixed_phase().phases[0].tasks.clone() {
        launch(&mut ctx, &mut run, t, None);
    }
    run.spawn_gate(&mut ctx, 0);
    run.planner = Some(ctx.add_idle());
    run.start_finale(&mut ctx, 0);
    run.spawn_planner_with(&mut ctx, Some("thread-1".into()));
    let names: Vec<&str> = ctx.spawns.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, vec!["planner", "orchestrator", "manager", "s1", "b1", "qa", "qa-heavy", "planner"]);
    for s in &ctx.spawns {
        assert_eq!(s.mcp, vec![format!("{}-tools", s.role_name), "*".to_string()], "{}", s.name);
    }
    // a role that asks for none (every built-in one) gets none
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    run.spawn_planner(&mut ctx);
    assert!(ctx.spawned("planner")[0].mcp.is_empty());
}

/// The screenshot and view-image tools reach an agent only when the app said its model takes
/// images — never by default, never for the other roles.
#[test]
fn the_vision_tools_go_only_to_a_model_that_sees() {
    let has_vision = |s: &Spawned| tools::VISION_TOOLS.iter().all(|t| s.tools.iter().any(|x| x == t));
    let has_any = |s: &Spawned| s.tools.iter().any(|x| tools::VISION_TOOLS.contains(&x.as_str()));
    let mut p = Pattern::builtin();
    p.roles.get_mut("worker-small").expect("worker-small").model = "eye".into();
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), p, "test".into());
    run.plan = Some(mixed_phase());
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
    // never told what sees: nobody gets them
    launch(&mut ctx, &mut run, Task { id: "s0".into(), role: "worker-small".into(), ..Default::default() }, None);
    assert!(!has_any(&ctx.spawned("s0")[0]), "{:?}", ctx.spawned("s0")[0].tools);
    run.set_vision_models(["eye".to_string()].into_iter().collect());
    for t in mixed_phase().phases[0].tasks.clone() {
        launch(&mut ctx, &mut run, t, None);
    }
    assert!(has_vision(&ctx.spawned("s1")[0]), "{:?}", ctx.spawned("s1")[0].tools);
    assert!(ctx.spawned("s1")[0].tools.contains(&"mantra_ask".to_string()), "beside the role's own tools");
    assert!(!has_any(&ctx.spawned("b1")[0]), "worker-big's model does not see");
    run.spawn_planner(&mut ctx);
    assert!(!has_any(&ctx.spawned("planner")[0]));
    // a role whose model sees gets them on every path, the planner's included
    let planner_model = run.role("planner").model;
    run.set_vision_models([planner_model].into_iter().collect());
    run.spawn_planner_with(&mut ctx, Some("thread-1".into()));
    assert!(has_vision(ctx.spawned("planner").last().unwrap()));
}

/// `mantra_screenshot` and `mantra_view_image` answer with the text and an `inputImage`: the
/// capture via `$MANTRA_SCREENSHOT_CMD` (here a script copying a generated PNG), saved as
/// `<run dir>/screenshots/<agent>-<n>.png`; an image read from the agent's cwd within its caps.
#[test]
fn a_screenshot_and_an_image_come_back_as_input_images() {
    let dir = scratch_dir("vision");
    std::fs::create_dir_all(&dir).unwrap();
    let png = crate::util::tests::tiny_png(5, 3);
    std::fs::write(dir.join("src.png"), &png).unwrap();
    let script = dir.join("shot.sh");
    std::fs::write(&script, format!("#!/bin/sh\ncp {} \"$1\"\n", dir.join("src.png").display())).unwrap();
    // the only test that sets it: every other capture test passes its template explicitly
    std::env::set_var(crate::util::SCREENSHOT_VAR, format!("sh {} {{out}}", script.display()));
    let mut ctx = TestCtx::new();
    let mut run = Run::new(dir.clone(), Pattern::builtin(), "test".into());
    run.dir = dir.join("run");
    run.set_vision_models(["eye".to_string()].into_iter().collect());
    let a = ctx.add_idle();
    if let Some(ag) = ctx.agents.get_mut(&a) {
        ag.model_alias = "eye".into();
        ag.cwd = dir.clone();
    }
    run.workers.push(mk_worker("p1-ui", "worker-small", a, WState::Running));
    let want_url = format!("data:image/png;base64,{}", crate::util::b64(&png));

    run.on_tool_call(&mut ctx, a, json!(1), "mantra_screenshot", &json!({}));
    assert!(ctx.items.is_empty() && ctx.jobs.iter().any(|j| j.starts_with("Vision")), "captured as a job, off the event loop: {:?}", ctx.jobs);
    ctx.settle(&mut run);
    let (who, items, ok) = ctx.items.last().cloned().expect("an answer");
    assert!(ok && who == a, "{items:?}");
    let text = items[0]["text"].as_str().unwrap_or_default().to_string();
    let shot = run.dir.join("screenshots").join("p1-ui-1.png");
    assert!(text.contains(&format!("{}", std::fs::canonicalize(&shot).unwrap().display())) && text.contains("image/png, 5x3 px") && text.contains("via MANTRA_SCREENSHOT_CMD"), "{text}");
    assert_eq!(items[1], json!({"type": "inputImage", "imageUrl": want_url}));
    assert_eq!(std::fs::read(&shot).unwrap(), png);
    {
        // the host's screen is nobody else's to read: an owner-only directory and file
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!((mode(&run.dir.join("screenshots")), mode(&shot)), (0o700, 0o600));
    }
    run.on_tool_call(&mut ctx, a, json!(2), "mantra_screenshot", &json!({"region": "0,0,5,3"}));
    ctx.settle(&mut run);
    assert!(run.dir.join("screenshots").join("p1-ui-2.png").exists(), "the next capture gets the next number");
    assert!(ctx.items.last().unwrap().0 == a && ctx.items.last().unwrap().2);
    assert!(run.pulse.iter().any(|l| l.text.contains("p1-ui took a screenshot")), "journaled");

    // view_image: relative to the agent's cwd, by its bytes, within 8 MB
    run.on_tool_call(&mut ctx, a, json!(3), "mantra_view_image", &json!({"path": "src.png"}));
    ctx.settle(&mut run);
    let (_, items, ok) = ctx.items.last().cloned().unwrap();
    assert!(ok && items[0]["text"].as_str().unwrap_or_default().contains("src.png (image/png, 5x3 px"), "{items:?}");
    assert_eq!(items[1]["imageUrl"], json!(want_url));
    std::fs::write(dir.join("notes.png"), "not an image").unwrap();
    let big = std::fs::File::create(dir.join("big.png")).unwrap();
    big.set_len(crate::util::IMAGE_MAX_BYTES + 1).unwrap();
    for (args, says) in [(json!({"path": "notes.png"}), "is not a PNG, JPEG, GIF or WebP image"), (json!({"path": "big.png"}), "images go up to 8.0 MiB"), (json!({}), "path is required"), (json!({"path": "gone.png"}), "cannot read")] {
        run.on_tool_call(&mut ctx, a, json!(4), "mantra_view_image", &args);
        ctx.settle(&mut run);
        let (_, items, ok) = ctx.items.last().cloned().unwrap();
        assert!(!ok && items.len() == 1 && items[0]["text"].as_str().unwrap_or_default().contains(says), "{args}: {items:?}");
    }

    // an agent whose model does not see is refused, and nothing is captured
    let b = ctx.add_idle();
    run.workers.push(mk_worker("p1-api", "worker-small", b, WState::Running));
    let jobs = ctx.jobs.len();
    run.on_tool_call(&mut ctx, b, json!(5), "mantra_screenshot", &json!({}));
    assert_eq!(ctx.jobs.len(), jobs, "refused at once, no job");
    let (_, items, ok) = ctx.items.last().cloned().unwrap();
    assert!(!ok && items[0]["text"].as_str().unwrap_or_default().contains("does not take images"), "{items:?}");
    assert!(!run.dir.join("screenshots").join("p1-api-1.png").exists());
}

/// A planner whose role has the chain `pa` → `pb` → `pc` (`fallback`, `fallback2`).
fn chain_run(tag: &str) -> (Run, TestCtx) {
    let mut pat = no_manager();
    {
        let r = pat.roles.get_mut("planner").unwrap();
        r.model = "pa".into();
        r.fallback = Some("pb".into());
        r.fallback2 = Some("pc".into());
    }
    let mut run = Run::new(PathBuf::from("."), pat, "test".into());
    run.dir = scratch_dir(tag);
    run.stage = Stage::Planning;
    (run, TestCtx::new())
}

#[test]
fn a_usage_limit_walks_the_fallback_chain_then_halts() {
    let (mut run, mut ctx) = chain_run("chain-walk");
    let a = run.spawn_planner(&mut ctx);
    assert_eq!(ctx.spawned("planner").last().unwrap().model, "pa");
    run.on_turn_done(&mut ctx, a, "failed", Some("You've hit your 5-hour limit · resets 9:20pm (UTC)".into()), Some(ErrKind::UsageLimit));
    let b = run.planner.unwrap();
    assert_ne!(b, a);
    assert_eq!(ctx.spawned("planner").last().unwrap().model, "pb", "respawned on the first fallback");
    assert!(!run.halted());
    assert!(!ctx.prompts.iter().any(|(_, t)| t.contains("[mantra:retry]")), "a usage limit is not retried: {:?}", ctx.prompts);
    assert!(ctx.prompts.iter().any(|(x, t)| *x == b && t.contains("[mantra:fallback] The previous model (pa) hit its provider's usage limit") && t.contains("replacement on pb")), "{:?}", ctx.prompts);
    assert!(run.pulse.iter().any(|p| p.glyph == "⇄" && p.text == "planner: pa → fallback pb (usage limit)"), "{:?}", run.pulse.iter().map(|p| p.text.clone()).collect::<Vec<_>>());
    assert_eq!(run.pattern.roles["planner"].model, "pa", "the role's model is not rewritten");
    run.on_turn_done(&mut ctx, b, "failed", Some("You've hit your weekly limit".into()), Some(ErrKind::UsageLimit));
    let c = run.planner.unwrap();
    assert_eq!(ctx.spawned("planner").last().unwrap().model, "pc", "the second fallback");
    assert!(!run.halted());
    run.on_turn_done(&mut ctx, c, "failed", Some("You've hit your weekly limit".into()), Some(ErrKind::UsageLimit));
    assert_eq!(run.halt.as_ref().map(|h| h.reason), Some(HaltReason::UsageLimit), "the chain is spent");
    assert_eq!(run.fallback_used.get("planner").cloned().unwrap_or_default(), ["pa", "pb", "pc"]);
    let _ = std::fs::remove_dir_all(&run.dir);
}

#[test]
fn a_fallback_skips_models_on_the_troubled_provider_and_later_spawns_start_on_one() {
    let (mut run, mut ctx) = chain_run("chain-provider");
    run.set_known_providers(HashMap::from([("pa".to_string(), "P1".to_string()), ("pb".into(), "P1".into()), ("pc".into(), "P2".into())]));
    let gate = run.pattern.flow.phase_gate.clone();
    {
        let r = run.pattern.roles.get_mut(&gate).unwrap();
        r.model = "pa".into();
        r.fallback = Some("pc".into());
    }
    let a = run.spawn_planner(&mut ctx);
    run.on_turn_done(&mut ctx, a, "failed", Some("usage limit".into()), Some(ErrKind::UsageLimit));
    assert_eq!(ctx.spawned("planner").last().unwrap().model, "pc", "pb shares pa's provider, which just hit its limit");
    assert!(!run.halted());
    // a gate on the troubled provider starts on its fallback
    run.plan = Some(one_task_phase());
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
    run.spawn_gate_at_round(&mut ctx, 0, 1);
    assert_eq!(ctx.spawned(&gate).last().map(|s| s.model.clone()).as_deref(), Some("pc"));
    assert!(run.pulse.iter().any(|p| p.glyph == "⇄" && p.text.contains("starts on fallback pc") && p.text.contains("pa's provider hit its usage limit")), "{:?}", run.pulse.iter().map(|p| p.text.clone()).collect::<Vec<_>>());
    let _ = std::fs::remove_dir_all(&run.dir);
}

#[test]
fn a_pinned_worker_keeps_its_model_on_a_troubled_provider_and_is_warned_once() {
    let mut ctx = TestCtx::new();
    let mut pat = no_manager();
    pat.roles.get_mut("worker-small").unwrap().fallback = Some("other".into());
    let mut run = Run::new(PathBuf::from("."), pat, "test".into());
    run.dir = scratch_dir("pinned-trouble");
    run.plan = Some(one_task_phase());
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
    run.set_known_providers(HashMap::from([("x".to_string(), "P1".to_string()), ("other".into(), "P2".into())]));
    let a = ctx.add_busy();
    let mut w = mk_worker("t1", "worker-small", a, WState::Running);
    w.model = Some("x".into());
    run.workers.push(w);
    run.mark_trouble("x", 3600, "hit its usage limit");
    for _ in 0..2 {
        run.launch_worker(&mut ctx, 0, PathBuf::from("wt"), "b".into());
    }
    assert!(ctx.spawned("t1").iter().all(|s| s.model == "x"), "the user's pin wins");
    let warned = run.pulse.iter().filter(|p| p.glyph == "⚠" && p.text == "worker:t1: runs on x as you chose, although its provider hit its usage limit").count();
    assert_eq!(warned, 1, "{:?}", run.pulse.iter().map(|p| p.text.clone()).collect::<Vec<_>>());
    let _ = std::fs::remove_dir_all(&run.dir);
}

#[test]
fn a_rejected_request_falls_back_when_the_chain_has_a_model() {
    let (mut run, mut ctx) = chain_run("chain-rejected");
    let a = run.spawn_planner(&mut ctx);
    run.on_turn_done(&mut ctx, a, "failed", Some("Unexpected message role.".into()), Some(ErrKind::ProviderRejected));
    assert!(!run.halted());
    assert_eq!(ctx.spawned("planner").last().unwrap().model, "pb");
    assert!(run.pulse.iter().any(|p| p.text == "planner: pa → fallback pb (rejected)"));
    assert!(ctx.prompts.iter().any(|(_, t)| t.contains("was rejected by its provider (Unexpected message role.)")), "{:?}", ctx.prompts);
    // without a chain: the existing halt
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
    run.dir = scratch_dir("chain-rejected-none");
    run.stage = Stage::Planning;
    let a = run.spawn_planner(&mut ctx);
    run.on_turn_done(&mut ctx, a, "failed", Some("Unexpected message role.".into()), Some(ErrKind::ProviderRejected));
    assert_eq!(run.halt.as_ref().map(|h| h.reason), Some(HaltReason::ProviderRejected));
    let _ = std::fs::remove_dir_all(&run.dir);
}

#[test]
fn a_halt_about_one_agents_provider_lets_the_others_finish_their_turn() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
    run.dir = scratch_dir("soft-halt");
    run.plan = Some(one_task_phase());
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
    let p = ctx.add_idle();
    run.planner = Some(p);
    let w = ctx.add_busy();
    run.workers.push(mk_worker("t1", "worker-small", w, WState::Running));
    run.on_turn_done(&mut ctx, p, "failed", Some("invalid api key".into()), Some(ErrKind::Auth));
    assert_eq!(run.halt.as_ref().map(|h| h.reason), Some(HaltReason::Auth));
    assert!(!ctx.interrupted.contains(&w), "the healthy worker keeps its turn");
    assert!(run.pulse.iter().any(|x| x.glyph == "‖" && x.text == "1 agent(s) keep working until their turn ends — the halt is about planner only"), "{:?}", run.pulse.iter().map(|x| x.text.clone()).collect::<Vec<_>>());
    // a user pause still stops everyone
    run.resume(&mut ctx);
    run.halt(&mut ctx, HaltReason::User, None, "paused".into());
    assert!(ctx.interrupted.contains(&w));
    let _ = std::fs::remove_dir_all(&run.dir);
}

#[test]
fn fallback_used_survives_a_resume() {
    let (mut run, _ctx) = chain_run("fallback-used-state");
    run.fallback_used.insert("planner".into(), vec!["pa".into(), "pb".into()]);
    run.save_state();
    let st = RunState::load(&run.dir).unwrap();
    assert_eq!(st.fallback_used.get("planner").cloned().unwrap_or_default(), ["pa", "pb"]);
    let back = Run::from_state(&st, run.pattern.clone(), None, run.dir.clone());
    assert_eq!(back.fallback_used, run.fallback_used);
    let _ = std::fs::remove_dir_all(&run.dir);
}

fn empty_turn_setup(tag: &str, fallback: bool) -> (Run, TestCtx, AgentId) {
    let mut ctx = TestCtx::new();
    let mut pat = no_manager();
    if fallback {
        pat.roles.get_mut("worker-small").unwrap().fallback = Some("claude-fallback".into());
    }
    let mut run = Run::new(PathBuf::from("."), pat, "test".into());
    run.dir = scratch_dir(tag);
    run.plan = Some(one_task_phase());
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
    let a = ctx.add_busy();
    ctx.agents.get_mut(&a).unwrap().model_alias = "primary".into();
    let mut w = mk_worker("t1", "worker-small", a, WState::Running);
    w.wt = Some(PathBuf::from("existing-worktree"));
    run.workers.push(w);
    (run, ctx, a)
}

fn empty_turn(ctx: &mut TestCtx, a: AgentId) {
    let ag = ctx.agents.get_mut(&a).unwrap();
    ag.last_turn_empty = true;
    ag.final_message = None;
}

#[test]
fn an_empty_worker_turn_is_asked_again_once() {
    let (mut run, mut ctx, a) = empty_turn_setup("empty-retry", false);
    empty_turn(&mut ctx, a);
    run.on_turn_done(&mut ctx, a, "completed", None, None);
    assert!(matches!(run.workers[0].state, WState::Retrying(_)), "not done");
    assert!(run.pulse.iter().any(|p| p.text.contains("empty turn from primary (no text, no tool call) — asking again")));
    assert_eq!(run.continue_queue.len(), 1);
    assert!(run.continue_queue[0].2.starts_with("[mantra:retry] Your previous turn ended without any output"));
    let _ = std::fs::remove_dir_all(&run.dir);
}

#[test]
fn a_second_empty_turn_falls_back_or_fails_the_task() {
    let (mut run, mut ctx, a) = empty_turn_setup("empty-chain", true);
    for _ in 0..2 {
        empty_turn(&mut ctx, a);
        run.on_turn_done(&mut ctx, a, "completed", None, None);
    }
    assert_ne!(run.workers[0].agent, Some(a));
    assert_eq!(ctx.spawned("t1").last().unwrap().model, "claude-fallback");
    assert!(run.pulse.iter().any(|p| p.text.contains("fallback claude-fallback (empty turns)")));
    let _ = std::fs::remove_dir_all(&run.dir);

    let (mut run, mut ctx, a) = empty_turn_setup("empty-nochain", false);
    let orch = ctx.add_idle();
    ready(&mut ctx, orch);
    run.orchestrator = Some(orch);
    for _ in 0..2 {
        empty_turn(&mut ctx, a);
        run.on_turn_done(&mut ctx, a, "completed", None, None);
    }
    assert!(matches!(run.workers[0].state, WState::Failed(ref e) if e.contains("empty turn twice from primary")), "{:?}", run.workers[0].state);
    let told = run.orch_inbox.iter().chain(ctx.prompts.iter().map(|(_, t)| t)).any(|t| t.contains("t1 FAILED"));
    assert!(told, "{:?} {:?}", run.orch_inbox, ctx.prompts);
    let _ = std::fs::remove_dir_all(&run.dir);
}

#[test]
fn two_empty_fix_worker_turns_halt_without_running_checks() {
    let (mut run, mut ctx, a) = empty_turn_setup("empty-fix", false);
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Fix { round: 1 } };
    run.workers[0].task.id = "fix-1".into();
    run.workers[0].adhoc = true;
    for _ in 0..2 {
        empty_turn(&mut ctx, a);
        run.on_turn_done(&mut ctx, a, "completed", None, None);
    }
    assert!(run.halted(), "escalated");
    assert!(!ctx.jobs.iter().any(|j| j.to_lowercase().contains("check")), "{:?}", ctx.jobs);
    let _ = std::fs::remove_dir_all(&run.dir);
}

#[test]
fn an_empty_planner_turn_is_retried_not_nudged_for_a_plan() {
    let (mut run, mut ctx, p) = planner_feature_run("empty-planner", Pattern::builtin());
    empty_turn(&mut ctx, p);
    run.on_turn_done(&mut ctx, p, "completed", None, None);
    assert!(run.continue_queue.iter().any(|(a, _, m)| *a == p && m.starts_with("[mantra:retry]")));
    assert!(!ctx.prompts.iter().any(|(_, t)| t.contains("You haven't submitted a valid plan")));
    assert_eq!(run.planner_nudges, 0);
    let _ = std::fs::remove_dir_all(&run.dir);
}

#[test]
fn a_worker_without_a_status_line_is_asked_once() {
    let (mut run, mut ctx, a) = empty_turn_setup("no-status", false);
    ctx.agents.get_mut(&a).unwrap().final_message = Some("I changed some files.".into());
    run.on_turn_done(&mut ctx, a, "completed", None, None);
    assert_ne!(run.workers[0].state, WState::Done);
    assert!(ctx.prompts.iter().any(|(x, t)| *x == a && t.starts_with("[mantra] Your turn ended without the closing lines")));
    assert!(run.pulse.iter().any(|p| p.text.contains("t1 ended its turn without a STATUS line")));
    run.on_turn_done(&mut ctx, a, "completed", None, None);
    assert_eq!(run.workers[0].state, WState::Done);
    let _ = std::fs::remove_dir_all(&run.dir);
}

#[test]
fn respawning_the_planner_carries_the_queue_and_records_the_replacement() {
    let (mut run, mut ctx, p) = planner_feature_run("carry-over", Pattern::builtin());
    run.respawn_planner(&mut ctx, None);
    let new = run.planner.unwrap();
    assert_ne!(new, p);
    assert_eq!(ctx.carried, [(p, new)]);
    assert_eq!(run.replaced, [(p, new)]);
    let _ = std::fs::remove_dir_all(&run.dir);
}

#[test]
fn a_prompt_sent_just_before_a_halt_comes_back_with_the_resume_prompt() {
    let (mut run, mut ctx, a) = empty_turn_setup("redeliver", false);
    let orch = ctx.add_idle();
    run.orchestrator = Some(orch);
    let name = run.name_of(a);
    let (_, ok) = run.handle_tool(&mut ctx, orch, "mantra_prompt", &json!({"agent": name, "message": "use the new parser"}));
    assert!(ok);
    run.halt_full(&mut ctx, HaltReason::AgentTurnFailed, Some(a), "x".into(), "x".into());
    ctx.prompts.clear();
    run.resume(&mut ctx);
    let resume = ctx.prompts.iter().find(|(x, t)| *x == a && t.starts_with("[mantra:resume]")).expect("resume prompt").1.clone();
    assert!(resume.contains("Sent to you just before the pause") && resume.contains("- [from the orchestrator] use the new parser"), "{resume}");
    let _ = std::fs::remove_dir_all(&run.dir);
}

#[test]
fn the_orchestrator_cannot_prompt_a_worker_through_a_halt() {
    let (mut run, mut ctx, a) = empty_turn_setup("hold", false);
    let orch = ctx.add_idle();
    run.orchestrator = Some(orch);
    let name = run.name_of(a);
    run.halt_full(&mut ctx, HaltReason::AgentTurnFailed, Some(a), "x".into(), "x".into());
    ctx.prompts.clear();
    let (text, ok) = run.handle_tool(&mut ctx, orch, "mantra_prompt", &json!({"agent": name, "message": "carry on"}));
    assert!(ok && text.starts_with("held — the run is halted"), "{text}");
    assert!(ctx.prompts.is_empty());
    run.resume(&mut ctx);
    assert!(ctx.prompts.iter().any(|(x, t)| *x == a && t.contains("- [from the orchestrator] carry on")), "{:?}", ctx.prompts);
    let _ = std::fs::remove_dir_all(&run.dir);
}

// ───────────────────────── WP-3.2: a run adopts changes to its saved pattern ─────────────────────────

fn valid_phase() -> Plan {
    let mut plan = one_task_phase();
    plan.phases[0].tasks[0].prompt = "Write the one function that is asked for and test it.".into();
    plan
}

/// What a save of the saved pattern does to the run: `edit` applied to the pattern the run last
/// synchronised with, adopted into the run copy.
fn adopt_edit(run: &mut Run, ctx: &mut TestCtx, edit: impl FnOnce(&mut Pattern)) -> Result<Vec<String>, String> {
    let base = run.pattern_base.clone().expect("a base");
    let mut theirs = Pattern::from_toml(&base).unwrap();
    edit(&mut theirs);
    let a = crate::engine::pattern::adopt(&base, &run.pattern, &theirs)?.expect("something to adopt");
    run.adopt_pattern(ctx, a, theirs.to_toml(), "the Studio")
}

#[test]
fn adopting_a_changed_role_model_switches_the_slot_and_drops_the_pin() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
    run.dir = scratch_dir("adopt-pin");
    run.stage = Stage::Planning;
    let planner_role = run.pattern.flow.planner.clone();
    let old_model = run.role(&planner_role).model;
    run.slot_models.insert("planner".into(), "nova".into());
    run.fallback_used.insert("planner".into(), vec![old_model.clone()]);
    let roles = adopt_edit(&mut run, &mut ctx, |p| p.roles.get_mut("planner").unwrap().model = "luna".into()).unwrap();
    assert_eq!(roles, vec![planner_role.clone()]);
    assert_eq!(run.pattern.roles[&planner_role].model, "luna");
    assert!(run.slot_models.get("planner").is_none() && run.fallback_used.get("planner").is_none());
    let said: Vec<String> = run.pulse.iter().map(|p| p.text.clone()).collect();
    assert!(said.iter().any(|t| t == "planner: your per-agent model nova gives way to the role's new model luna"), "{said:?}");
    assert!(said.iter().any(|t| t.starts_with("pattern updated from the Studio (1 change(s)): roles.planner.model:")), "{said:?}");
    let p = run.spawn_planner(&mut ctx);
    assert_eq!(ctx.spawned("planner")[0].model, "luna", "the next planner is on the new model ({p})");
    assert_eq!(run.pattern_base.as_deref().map(|b| Pattern::from_toml(b).unwrap().roles[&planner_role].model.clone()).as_deref(), Some("luna"));
    assert_eq!(std::fs::read_to_string(run.dir.join("pattern.base.toml")).unwrap(), run.pattern_base.clone().unwrap());
    assert!(std::fs::read_to_string(run.dir.join("pattern.toml")).unwrap().contains("luna"), "the run copy is saved");
    let _ = std::fs::remove_dir_all(&run.dir);
}

#[test]
fn adopting_changes_the_gate_of_the_current_phase_and_keeps_the_runs_own_settings() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
    run.dir = scratch_dir("adopt-gate");
    run.pattern.set_parallel(8); // `--parallel 8`: a run-local change
    run.plan = Some(valid_phase());
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
    run.pattern.settings.allow_pattern_changes = true;
    let gate = run.pattern.flow.phase_gate.clone();
    adopt_edit(&mut run, &mut ctx, |p| {
        p.roles.get_mut("qa").unwrap().model = "nova".into();
        p.settings.context_cap = 50_000;
    }).unwrap();
    assert_eq!((run.pattern.settings.max_parallel, run.pattern.settings.context_cap), (8, 50_000), "the run's own width stays, the user's cap arrives");
    assert!(run.pattern.settings.allow_pattern_changes, "the run-level permission is not the pattern's to change");
    run.spawn_gate_at_round(&mut ctx, 0, 1);
    assert_eq!(ctx.spawned(&gate).last().map(|s| s.model.clone()).as_deref(), Some("nova"), "the gate of the phase that is running now");
    let _ = std::fs::remove_dir_all(&run.dir);
}

#[test]
fn adopting_leaves_a_finished_worker_and_drops_the_pin_of_a_running_one() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
    run.dir = scratch_dir("adopt-workers");
    run.plan = Some(valid_phase());
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
    let (a, b) = (ctx.add_busy(), ctx.add_busy());
    let mut done = mk_worker("t0", "worker-small", a, WState::Done);
    done.model = Some("old".into());
    let mut live = mk_worker("t1", "worker-small", b, WState::Running);
    live.model = Some("nova".into());
    run.workers.push(done);
    run.workers.push(live);
    let roles = adopt_edit(&mut run, &mut ctx, |p| p.roles.get_mut("worker-small").unwrap().model = "zeta".into()).unwrap();
    assert_eq!(roles, vec!["worker-small".to_string()]);
    assert_eq!(run.workers[0].model.as_deref(), Some("old"), "a finished attempt keeps its record");
    assert_eq!(run.workers[1].model, None);
    assert!(run.pulse.iter().any(|p| p.text == "worker:t1#1: your per-agent model nova gives way to the role's new model zeta"));
    let _ = std::fs::remove_dir_all(&run.dir);
}

#[test]
fn adopting_refuses_what_the_run_cannot_take_and_keeps_its_copy() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
    run.dir = scratch_dir("adopt-refuse");
    run.plan = Some(valid_phase());
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
    let before = run.pattern.clone();
    let old_base = run.pattern_base.clone();
    // the plan still has a task for worker-small
    let e = adopt_edit(&mut run, &mut ctx, |p| { p.roles.remove("worker-small"); }).unwrap_err();
    assert!(e.contains("worker-small"), "{e}");
    assert_eq!(run.pattern, before);
    assert_eq!(run.pattern_base, old_base, "and the base is where it was");
    let _ = std::fs::remove_dir_all(&run.dir);
}

#[test]
fn adopting_an_orchestrator_model_change_leaves_the_agent_to_the_live_switch_but_new_instructions_respawn_it() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
    run.dir = scratch_dir("adopt-orch");
    run.plan = Some(valid_phase());
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
    let o = run.spawn_orchestrator(&mut ctx);
    let roles = adopt_edit(&mut run, &mut ctx, |p| p.roles.get_mut("orchestrator").unwrap().model = "luna".into()).unwrap();
    assert_eq!(roles, vec!["orchestrator".to_string()]);
    assert_eq!(run.orchestrator, Some(o), "the app switches the live agent in place");
    assert!(!ctx.stopped.contains(&o));
    adopt_edit(&mut run, &mut ctx, |p| p.roles.get_mut("orchestrator").unwrap().instructions = "Be brief.".into()).unwrap();
    assert_ne!(run.orchestrator, Some(o), "new instructions need a new process");
    assert!(ctx.stopped.contains(&o));
    assert!(ctx.prompts.iter().any(|(_, t)| t.contains("The workflow was updated by the user; continue the phase.")), "{:?}", ctx.prompts);
    let _ = std::fs::remove_dir_all(&run.dir);
}

// ── WP-3.3: the planner keeps its memory across restarts and respawns ──

/// A run in phase 1 `Orchestrating` of a two-task plan, saved to disk and reopened the way
/// `resume_run` does; `thread` is the saved planner session (None: no session was saved).
fn resumed_in_phase(tag: &str, thread: Option<&str>, model: Option<&str>) -> (Run, TestCtx) {
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "build the thing".into());
    run.dir = scratch_dir(tag);
    run.plan = Some(class_phase(2, 0));
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
    run.save_state();
    let mut st = RunState::load(&run.dir).unwrap();
    if let Some(t) = thread {
        let alias = model.map(String::from).unwrap_or_else(|| run.role(&run.pattern.flow.planner.clone()).model);
        st.agents.push(AgentState { slot: "planner".into(), name: "planner".into(), role: run.pattern.flow.planner.clone(), provider: "openai".into(), model_alias: alias, effort: "high".into(), thread_id: Some(t.into()), cwd: "/tmp".into() });
    }
    let mut again = Run::from_state(&st, Pattern::builtin(), run.plan.clone(), run.dir.clone());
    if model.is_some() {
        // the saved alias is no longer configured: it cannot come back as a pin
        let own = again.role(&again.pattern.flow.planner.clone()).model;
        again.set_known_models(HashMap::from([(own, 100_000)]));
    }
    let mut ctx = TestCtx::new();
    again.resume_boot(&mut ctx, &st);
    (again, ctx)
}

#[test]
fn a_planner_with_a_saved_session_is_reattached_on_its_first_use_after_a_phase_resume() {
    let (mut run, mut ctx) = resumed_in_phase("planner-reattach", Some("t-plan"), None);
    assert!(run.planner.is_none() && ctx.spawned("planner").is_empty(), "no planner at boot");
    run.escalate_to_planner(&mut ctx, "stuck".into());
    let p = run.planner.expect("spawned lazily");
    assert_eq!(ctx.resumed, vec![(p, "t-plan".to_string())]);
    let prompt = &ctx.prompts.iter().rev().find(|(a, _)| *a == p).unwrap().1;
    assert!(prompt.starts_with("[mantra:resume] Mantra restarted; you are re-attached"), "{prompt}");
    let _ = std::fs::remove_dir_all(&run.dir);
}

#[test]
fn a_planner_saved_on_another_model_is_not_reattached() {
    let (mut run, mut ctx) = resumed_in_phase("planner-other-model", Some("t-plan"), Some("some-other-alias"));
    run.escalate_to_planner(&mut ctx, "stuck".into());
    assert!(ctx.resumed.is_empty());
    let p = run.planner.unwrap();
    assert!(ctx.prompts.iter().rev().find(|(a, _)| *a == p).unwrap().1.starts_with("[mantra:respawn] You are this run's planner"));
    let _ = std::fs::remove_dir_all(&run.dir);
}

#[test]
fn a_planner_without_a_saved_session_starts_with_a_context_pack() {
    let (mut run, mut ctx) = resumed_in_phase("planner-pack", None, None);
    run.escalate_to_planner(&mut ctx, "stuck".into());
    let p = run.planner.unwrap();
    assert!(ctx.resumed.is_empty());
    let prompt = &ctx.prompts.iter().rev().find(|(a, _)| *a == p).unwrap().1;
    assert!(prompt.starts_with("[mantra:respawn] You are this run's planner"), "{prompt}");
    assert!(prompt.contains("build the thing") && prompt.contains("\"id\": \"b0\"") && prompt.contains("CURRENT") && prompt.contains("[mantra:escalation]"), "{prompt}");
    let _ = std::fs::remove_dir_all(&run.dir);
}

#[test]
fn a_change_that_does_not_concern_the_planner_does_not_respawn_it() {
    let mut ctx = TestCtx::new();
    let mut run = spawned_phase(&mut ctx, class_phase(1, 0), "planner-desc");
    let p = run.spawn_planner(&mut ctx);
    let mut pattern = run.pattern.clone();
    let name = pattern.flow.planner.clone();
    pattern.roles.get_mut(&name).unwrap().description = "a different description".into();
    run.install_pattern(&mut ctx, PatternProposal { pattern, reason: "wording".into() });
    assert_eq!(run.planner, Some(p));
    assert!(!ctx.stopped.contains(&p));
    let _ = std::fs::remove_dir_all(&run.dir);
}

#[test]
fn a_respawned_planner_inherits_the_queue_and_its_predecessors_last_words() {
    let mut ctx = TestCtx::new();
    let mut run = spawned_phase(&mut ctx, class_phase(1, 0), "planner-continuity");
    let old = run.spawn_planner(&mut ctx);
    ctx.agents.get_mut(&old).unwrap().push_user("decide about the cache layer");
    run.respawn_planner(&mut ctx, Some("[mantra:watchdog] stalled".into()));
    let new = run.planner.unwrap();
    assert_ne!(new, old);
    assert!(ctx.carried.contains(&(old, new)));
    let prompt = &ctx.prompts.iter().rev().find(|(a, _)| *a == new).unwrap().1;
    assert!(prompt.contains("For continuity, what the previous planner session held:") && prompt.contains("decide about the cache layer"), "{prompt}");
    assert!(run.pulse.iter().any(|p| p.text == "planner respawned (watchdog)"));
    let _ = std::fs::remove_dir_all(&run.dir);
}

#[test]
fn respawn_reasons_are_told() {
    assert_eq!(respawn_why(None), "by you");
    assert_eq!(respawn_why(Some("[mantra:fallback] x")), "fallback model");
    assert_eq!(respawn_why(Some("The workflow was updated by the user; continue")), "workflow changed");
    assert_eq!(respawn_why(Some("Your previous process lost its session")), "session lost");
    assert_eq!(respawn_why(Some("try again")), "requested");
}

// ── WP-3.4: a halted, quit or resumed run says so everywhere ──

#[test]
fn a_halted_runs_status_text_starts_with_the_halt() {
    let mut ctx = TestCtx::new();
    let mut run = spawned_phase(&mut ctx, class_phase(1, 0), "status-halted");
    let calm = run.status_fingerprint(&ctx);
    assert!(!run.status_text(&ctx).contains("RUN HALTED"));
    run.halt(&mut ctx, HaltReason::GateExhausted, None, "the gate gave up".into());
    let text = run.status_text(&ctx);
    assert!(text.starts_with("RUN HALTED (gate exhausted) for "), "{text}");
    assert!(text.contains("the gate gave up") && text.contains("Nothing starts until it is resumed"), "{text}");
    assert_ne!(calm, run.status_fingerprint(&ctx), "a halt is a change a status reader must see");
    let _ = std::fs::remove_dir_all(&run.dir);
}

#[test]
fn runs_are_labelled_halted_closed_or_live_with_the_state_first() {
    use crate::engine::state::RunSummary;
    let dir = scratch_dir("labels");
    std::fs::create_dir_all(&dir).unwrap();
    let st = |halted: Option<&str>, stage: Stage| RunState { stage, halted: halted.map(String::from), ..Default::default() };
    let row = |s: RunState| RunSummary { id: "1-x".into(), dir: dir.clone(), project_key: "p".into(), state: Ok(s), updated_unix: 0 };
    let working = Stage::Phase { idx: 1, step: PhaseStep::Orchestrating };
    assert_eq!(row(st(Some("gate gave up"), working.clone())).stage_label(), "halted · phase 2 · working");
    assert_eq!(row(st(None, working.clone())).stage_label(), "closed · phase 2 · working", "no owner.json: nobody has it open");
    assert_eq!(row(st(None, Stage::Done)).stage_label(), "done");
    crate::engine::state::Owner::this_process().write(&dir);
    assert_eq!(row(st(None, working)).stage_label(), "phase 2 · working", "a live owner");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn resuming_a_paused_run_says_you_had_paused_it_and_lists_the_roles() {
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "goal".into());
    run.dir = scratch_dir("resume-roles");
    run.plan = Some(class_phase(1, 0));
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
    run.save_state();
    let mut st = RunState::load(&run.dir).unwrap();
    st.halted = Some("paused by you".into());
    st.closed_unix = Some(5);
    let mut again = Run::from_state(&st, Pattern::builtin(), run.plan.clone(), run.dir.clone());
    again.closed_unix = st.closed_unix;
    let mut ctx = TestCtx::new();
    again.resume_boot(&mut ctx, &st);
    let lines: Vec<String> = again.pulse.iter().map(|p| p.text.clone()).collect();
    assert!(lines.iter().any(|l| l.ends_with("(you had paused it)")), "{lines:?}");
    assert!(lines.iter().any(|l| l.starts_with("roles: planner ") && l.contains(" · orchestrator ")), "{lines:?}");
    assert!(again.closed_unix.is_none() && RunState::load(&again.dir).unwrap().closed_unix.is_none());
    let _ = std::fs::remove_dir_all(&run.dir);
}

// WP-2.8 / 2.9 / 2.10 -------------------------------------------------------------------------

#[test]
fn fit_or_point_keeps_whole_lines_and_says_where_the_rest_is() {
    use helpers::fit_or_point;
    assert_eq!(fit_or_point("short\ntext", 100, None), "short\ntext");
    let text = "line one\nline two\nline three";
    let cut = fit_or_point(text, 20, None);
    assert_eq!(cut, "line one\nline two\n[cut here]");
    let pointed = fit_or_point(text, 20, Some(Path::new("/x/gate/r.md")));
    assert!(pointed.ends_with("[cut here — the full text is in /x/gate/r.md]") && !pointed.contains("three"), "{pointed}");
    // no line break before the limit: cut at a char boundary, still marked
    assert!(fit_or_point(&"é".repeat(40), 11, None).ends_with("[cut here]"));
}

#[test]
fn fix_ids_are_fix_and_digits_only() {
    assert!(helpers::is_fix_id("fix-1") && helpers::is_fix_id("fix-12"));
    assert!(!helpers::is_fix_id("fix-login") && !helpers::is_fix_id("fix-") && !helpers::is_fix_id("xfix-1"));
}

#[test]
fn a_plan_may_not_use_a_fix_worker_id() {
    let pattern = Pattern::builtin();
    let mut plan = one_task_phase();
    plan.phases[0].tasks[0].prompt = "do the thing described here".into();
    plan.phases[0].tasks[0].id = "fix-2".into();
    let errs = plan.validate(&pattern).unwrap_err();
    assert!(errs.iter().any(|e| e.contains("'fix-2' is reserved for Mantra's fix workers")), "{errs:?}");
    plan.phases[0].tasks[0].id = "fix-login".into();
    assert!(plan.validate(&pattern).is_ok());
    // and such a task is an ordinary worker, not an ad-hoc one on the integration copy
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Orchestrating };
    run.spawn_task(&mut ctx, plan.phases[0].tasks[0].clone(), None, None);
    assert!(!run.workers.last().unwrap().adhoc);
}

#[test]
fn paths_in_drops_what_lies_outside_the_workspace() {
    let dir = std::env::temp_dir().join(format!("mantra-paths-out-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    let t = format!("see /etc/x.conf and ~/a/b.txt and {}/src/x.rs and src/x.rs", dir.display());
    assert_eq!(paths_in(&t, &dir), vec!["src/x.rs".to_string()]);
    assert!(paths_in("/home/u/.mantra/runs/r1/gate.md ~/notes/a.md", &dir).is_empty());
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_long_gate_report_reaches_the_fix_worker_and_the_next_qa_round_by_file() {
    let mut ctx = TestCtx::new();
    let (mut run, _planner, gate) = gate_run_with_checks(&mut ctx, 1);
    run.dir = scratch_dir("long-gate-report");
    let findings = format!("{}\nLAST-FINDING-LINE src/a.rs is wrong", "filler line of the report ".repeat(600));
    assert!(findings.len() > 15_000);
    let (_, ok) = run.handle_tool(&mut ctx, gate, "mantra_gate_report", &json!({"pass": false, "summary": findings}));
    assert!(ok);
    run.on_turn_done(&mut ctx, gate, "completed", None, None);
    let file = run.last_gate_file.clone().expect("the report was written");
    assert!(file.starts_with(run.dir.join("gate")) && file.file_name().unwrap().to_string_lossy().ends_with("-round-1-fail.md"), "{file:?}");
    let body = std::fs::read_to_string(&file).unwrap();
    assert!(body.starts_with("# phase 1 · QA round 1 · fail · plan v") && body.contains("LAST-FINDING-LINE"));
    assert!(std::fs::read_to_string(run.dir.join("gate-reports.md")).unwrap().contains("→ gate/phase-1-qa-01-round-1-fail.md"));
    let fix = run.workers.iter().find(|w| w.task.id == "fix-1").expect("a fix worker");
    assert!(fix.prompt.contains("LAST-FINDING-LINE"), "the fix worker gets the whole report");
    assert!(run.orch_inbox.iter().any(|m| m.starts_with("Gate round 1: QA failed the phase — the fix goes to fix-1 (") && m.contains(&file.display().to_string())), "{:?}", run.orch_inbox);
    // the next QA round: the report is past 12k, so it is cut at a line and points to the file
    finish_fix(&mut ctx, &mut run, 1);
    run.on_job(&mut ctx, JobTag::FixChecks { phase: 0, round: 1, gen: run.checks_gen }, JobOut::Checks(green(), vec![], git::ReviewDiff::default(), None));
    let g2 = run.gate_agent.expect("round 2");
    let (_, p) = ctx.prompts.iter().rev().find(|(a, _)| *a == g2).unwrap();
    assert!(p.contains(&format!("[cut here — the full text is in {}]", file.display())), "{p}");
    let _ = std::fs::remove_dir_all(&run.dir);
}

#[test]
fn a_qa_pass_tells_the_orchestrator_without_waking_it() {
    let mut ctx = TestCtx::new();
    let (mut run, _planner, gate) = gate_run_with_checks(&mut ctx, 1);
    run.dir = scratch_dir("qa-pass-inbox");
    run.handle_tool(&mut ctx, gate, "mantra_gate_report", &json!({"pass": true, "summary": "coherent"}));
    run.on_turn_done(&mut ctx, gate, "completed", None, None);
    assert!(run.orch_inbox.contains(&"Gate round 1: QA passed — Mantra verifies the checks.".to_string()), "{:?}", run.orch_inbox);
    let _ = std::fs::remove_dir_all(&run.dir);
}

#[test]
fn mantra_log_still_reads_the_retired_gate_agent() {
    let mut ctx = TestCtx::new();
    let (mut run, planner, gate) = gate_run_with_checks(&mut ctx, 1);
    run.dir = scratch_dir("log-retired-gate");
    run.start_fix(&mut ctx, 0, 1, "src/a.rs is wrong".into(), false);
    assert!(run.gate_agent.is_none());
    let (msg, ok) = run.handle_tool(&mut ctx, planner, "mantra_log", &json!({"agent": "gate"}));
    assert!(ok, "{msg}");
    assert!(!msg.contains("unknown agent"), "{msg}");
    assert_eq!(run.resolve("gate"), Some(gate));
    let _ = std::fs::remove_dir_all(&run.dir);
}

#[test]
fn mantra_log_cuts_at_an_entry_and_says_so() {
    let mut ctx = TestCtx::new();
    let (mut run, planner, gate) = gate_run_with_checks(&mut ctx, 1);
    let ag = ctx.agents.get_mut(&gate).unwrap();
    for i in 0..40 {
        ag.items.push(crate::agent::Item { id: format!("i{i}"), kind: crate::agent::Kind::Agent, text: format!("entry {i} {}", "x".repeat(1000)), done: true, version: 0, expanded: false, started: None, cache: None });
    }
    let (msg, ok) = run.handle_tool(&mut ctx, planner, "mantra_log", &json!({"agent": "gate", "lines": 40}));
    assert!(ok);
    assert!(msg.starts_with("[earlier entries omitted]\n[agent] entry "), "{}", &msg[..60]);
    assert!(msg.len() <= 16_100 && msg.contains("entry 39"));
}

#[test]
fn the_fix_prompt_shows_the_check_results() {
    let mut ctx = TestCtx::new();
    let (mut run, _planner, _gate) = gate_run_with_checks(&mut ctx, 1);
    run.dir = scratch_dir("fix-prompt-checks");
    run.checks = vec![CheckResult { cmd: "cargo test".into(), ok: false, code: Some(101), output: "assertion failed: left == right".into(), secs: 1 }];
    run.start_fix(&mut ctx, 0, 1, "src/a.rs is wrong".into(), false);
    let fix = run.workers.iter().find(|w| w.task.id == "fix-1").unwrap();
    assert!(fix.prompt.contains("The gate checks as they stand") && fix.prompt.contains("assertion failed: left == right"), "{}", fix.prompt);
    let _ = std::fs::remove_dir_all(&run.dir);
}

/// A fix worker's `SpawnReq` as the engine builds it (the fix runs, then its workspace job lands).
fn fix_spawn(worktree: bool) -> Spawned {
    let mut ctx = TestCtx::new();
    let (mut run, _planner, _gate) = gate_run_with_checks(&mut ctx, 1);
    run.dir = scratch_dir("fix-sandbox");
    if worktree {
        run.ws = Some(git::Workspace { worktree: true, repo: PathBuf::from("."), integ: PathBuf::from("."), branch: "mantra/test".into(), base_branch: "main".into(), git_common_dir: None, note: None, base_commit: None, initial_commit: false, nested: vec![] });
    }
    run.start_fix(&mut ctx, 0, 1, "src/a.rs is wrong".into(), false);
    finish_fix(&mut ctx, &mut run, 1);
    let _ = std::fs::remove_dir_all(&run.dir);
    ctx.spawned("fix-1").pop().expect("the fix worker was spawned")
}

#[test]
fn a_fix_worker_keeps_the_role_sandbox_and_asks_for_git_only_in_worktree_mode() {
    let role_sandbox = Pattern::builtin().roles.get("worker-small").map(|r| r.sandbox.clone()).unwrap_or_default();
    let shared = fix_spawn(false);
    assert!(!shared.git_write);
    assert_eq!(shared.sandbox, role_sandbox, "the role's own sandbox, never forced");
    let wt = fix_spawn(true);
    assert!(wt.git_write);
    assert_eq!(wt.sandbox, role_sandbox, "the engine leaves the sandbox alone; the app lifts it for Codex");
}

#[test]
fn a_fix_that_changed_nothing_is_journaled_and_qa_is_told() {
    for (same, expect) in [(true, true), (false, false)] {
        let mut ctx = TestCtx::new();
        let (mut run, _planner, _gate) = gate_run_with_checks(&mut ctx, 1);
        run.dir = scratch_dir("fix-no-change");
        run.tree_print = Some("print-a".into());
        run.start_fix(&mut ctx, 0, 1, "src/a.rs is wrong".into(), false);
        assert_eq!(run.fix_base_print.as_deref(), Some("print-a"));
        run.stage = Stage::Phase { idx: 0, step: PhaseStep::Checks { round: 1 } };
        let after = if same { "print-a" } else { "print-b" };
        run.on_job(&mut ctx, JobTag::FixChecks { phase: 0, round: 1, gen: run.checks_gen }, JobOut::Checks(green(), vec![], git::ReviewDiff::default(), Some(after.into())));
        let qa = run.gate_agent.expect("round 2");
        let (_, p) = ctx.prompts.iter().rev().find(|(a, _)| *a == qa).unwrap();
        assert_eq!(p.contains("the fix worker of the last round changed no file in the workspace"), expect, "{p}");
        assert_eq!(run.pulse.iter().any(|j| j.text.contains("fix-1 changed no file in the workspace")), expect);
        let _ = std::fs::remove_dir_all(&run.dir);
    }
}

// ---- Milestone 6: a gate fix goes back to the worker that wrote the code ----

/// A run at gate round 1 of a phase with two finished tasks: `t1` owns `src/a.rs`, `t2` owns
/// `src/b.rs`. Returns the run, the QA agent and the two workers' agents. The run has a worktree
/// workspace and the workers have worktrees, so the owner path has jobs to run.
fn owner_run(ctx: &mut TestCtx) -> (Run, AgentId, AgentId, AgentId) {
    let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
    run.dir = std::env::temp_dir().join(format!("mantra-owner-{}-{}", std::process::id(), ctx.next));
    let gate = ctx.add_idle();
    let (a1, a2) = (ctx.add_idle(), ctx.add_idle());
    for a in [gate, a1, a2] {
        ready(ctx, a);
    }
    let task = |id: &str, scope: &str| Task { id: id.into(), role: "worker-small".into(), scope: vec![scope.into()], prompt: "do the work described for this task".into(), ..Default::default() };
    let mut plan = one_task_phase();
    plan.phases[0].tasks = vec![task("t1", "src/a.rs"), task("t2", "src/b.rs")];
    plan.phases[0].gate.checks = vec!["true".into()];
    run.plan = Some(plan.clone());
    let wt = std::env::temp_dir();
    run.ws = Some(Workspace { worktree: true, repo: wt.clone(), integ: wt.join("integ-nowhere"), branch: "run".into(), base_branch: "main".into(), git_common_dir: None, note: None, base_commit: None, initial_commit: false, nested: vec![] });
    for (t, a) in plan.phases[0].tasks.iter().zip([a1, a2]) {
        let mut w = mk_worker(&t.id, "worker-small", a, WState::Done);
        w.task = t.clone();
        w.wt = Some(wt.clone());
        w.branch = format!("mantra-w/{}", t.id);
        w.report = format!("STATUS: done\nSUMMARY: wrote {}", t.id);
        run.workers.push(w);
    }
    run.gate_agent = Some(gate);
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Gate { round: 1 } };
    (run, gate, a1, a2)
}

fn t(run: &Run, id: &str) -> usize {
    run.workers.iter().position(|w| w.task.id == id).unwrap()
}

#[test]
fn the_owner_of_the_findings_is_the_one_task_that_owns_every_file_they_name() {
    let mut ctx = TestCtx::new();
    let (mut run, _gate, a1, _a2) = owner_run(&mut ctx);
    let t1 = t(&run, "t1");
    assert_eq!(run.fix_owner_for(&ctx, 1, "src/a.rs:3 returns the wrong value"), Some(t1));
    assert_eq!(run.fix_owner_for(&ctx, 1, "src/a.rs is wrong and src/b.rs too"), None, "two tasks: a fix worker");
    assert_eq!(run.fix_owner_for(&ctx, 1, "the build is broken somewhere"), None, "no file named");
    assert_eq!(run.fix_owner_for(&ctx, 1, "src/zzz.rs does not exist yet"), None, "a file no task owns");
    assert_eq!(run.fix_owner_for(&ctx, 0, "src/a.rs conflicts"), None, "a merge-conflict fix is never an owner's");
    // QA names the task, with or without a file
    run.gate_fix_by = Some("t2".into());
    assert_eq!(run.fix_owner_for(&ctx, 1, "src/a.rs is wrong"), Some(t(&run, "t2")), "fix_by replaces the derivation");
    assert_eq!(run.fix_owner_for(&ctx, 1, "no file here"), Some(t(&run, "t2")));
    run.gate_fix_by = Some("fix-worker".into());
    assert_eq!(run.fix_owner_for(&ctx, 1, "src/a.rs is wrong"), None);
    run.gate_fix_by = Some("nobody".into());
    assert_eq!(run.fix_owner_for(&ctx, 1, "src/a.rs is wrong"), None, "an unknown task");
    run.gate_fix_by = None;
    // the file was changed by the worker's agent, though its scope does not cover it
    ctx.agents.get_mut(&a1).unwrap().files.insert("lib/shared.rs".into(), Default::default());
    assert_eq!(run.fix_owner_for(&ctx, 1, "lib/shared.rs is wrong"), Some(t1));
    // the setting, a big fix, a failed earlier owner fix
    run.gate_big = true;
    assert_eq!(run.fix_owner_for(&ctx, 1, "src/a.rs is wrong"), None);
    run.gate_big = false;
    run.workers[t1].owner_fix_failed = true;
    assert_eq!(run.fix_owner_for(&ctx, 1, "src/a.rs is wrong"), None);
    run.workers[t1].owner_fix_failed = false;
    run.pattern.settings.gate_fix = "fix-worker".into();
    assert_eq!(run.fix_owner_for(&ctx, 1, "src/a.rs is wrong"), None);
    run.pattern.settings.gate_fix = "owner".into();
    assert_eq!(run.fix_owner_for(&ctx, 1, "src/a.rs is wrong"), Some(t1));
}

#[test]
fn an_owner_that_cannot_take_the_job_is_not_chosen() {
    let mut ctx = TestCtx::new();
    let (mut run, _gate, a1, _a2) = owner_run(&mut ctx);
    ctx.agents.get_mut(&a1).unwrap().status = Status::Stopped;
    assert_eq!(run.fix_owner_for(&ctx, 1, "src/a.rs is wrong"), None, "stopped");
    ctx.agents.get_mut(&a1).unwrap().status = Status::Crashed("x".into());
    assert_eq!(run.fix_owner_for(&ctx, 1, "src/a.rs is wrong"), None, "crashed");
    ctx.agents.get_mut(&a1).unwrap().status = Status::Idle;
    ctx.agents.get_mut(&a1).unwrap().stopped_by_user = true;
    assert_eq!(run.fix_owner_for(&ctx, 1, "src/a.rs is wrong"), None, "stopped by hand");
    ctx.agents.get_mut(&a1).unwrap().stopped_by_user = false;
    ctx.agents.get_mut(&a1).unwrap().turn_active = true;
    assert_eq!(run.fix_owner_for(&ctx, 1, "src/a.rs is wrong"), None, "busy");
    ctx.agents.get_mut(&a1).unwrap().turn_active = false;
    let i = t(&run, "t1");
    run.workers[i].wt = Some(PathBuf::from("/nonexistent/mantra-worktree"));
    assert_eq!(run.fix_owner_for(&ctx, 1, "src/a.rs is wrong"), None, "its worktree is gone");
    run.workers[i].wt = Some(std::env::temp_dir());
    run.workers[i].state = WState::Failed("x".into());
    assert_eq!(run.fix_owner_for(&ctx, 1, "src/a.rs is wrong"), None, "not Done");
    run.workers[i].state = WState::Done;
    run.workers[i].adhoc = true;
    assert_eq!(run.fix_owner_for(&ctx, 1, "src/a.rs is wrong"), None, "an ad-hoc worker owns nothing");
}

#[test]
fn the_gate_report_carries_fix_by() {
    let mut ctx = TestCtx::new();
    let (mut run, gate, _a1, _a2) = owner_run(&mut ctx);
    let (_, ok) = run.handle_tool(&mut ctx, gate, "mantra_gate_report", &json!({"pass": false, "summary": "src/b.rs and src/a.rs are inconsistent", "fix_by": "t1"}));
    assert!(ok);
    assert_eq!(run.gate_fix_by.as_deref(), Some("t1"));
    run.on_turn_done(&mut ctx, gate, "completed", None, None);
    assert_eq!(run.fix_owner.as_deref(), Some("t1"), "QA named the task");
    assert!(!run.workers.iter().any(|w| w.task.id == "fix-1"));
}

/// The owner path from the verdict to the next QA round.
#[test]
fn a_failed_round_in_one_tasks_files_goes_back_to_its_own_worker() {
    let mut ctx = TestCtx::new();
    let (mut run, gate, a1, _a2) = owner_run(&mut ctx);
    let orch = ctx.add_idle();
    ready(&mut ctx, orch);
    run.orchestrator = Some(orch);
    gate_says(&mut ctx, &mut run, gate, "GATE: fail — src/a.rs:3 returns the wrong value");
    assert!(!run.halted());
    assert!(matches!(run.stage, Stage::Phase { step: PhaseStep::Fix { round: 1 }, .. }), "{:?}", run.stage);
    assert!(!run.workers.iter().any(|w| w.task.id == "fix-1"), "no fix worker");
    assert!(ctx.jobs.iter().any(|j| j.starts_with("OwnerSync")), "{:?}", ctx.jobs);
    assert_eq!(run.fix_owner.as_deref(), Some("t1"));
    assert!(run.journal_text(60).contains("gate round 1 found problems in t1's work — back to its own worker"));
    assert!(run.orch_inbox.iter().any(|l| l.contains("t1's own worker")), "{:?}", run.orch_inbox);
    assert!(!ctx.prompts.iter().any(|(a, _)| *a == a1), "nothing is said to it before its worktree is current");
    // the worktree is current: it is told
    run.on_job(&mut ctx, JobTag::OwnerSync { phase: 0, round: 1, task: "t1".into() }, JobOut::Text(Ok(String::new())));
    let (_, p) = ctx.prompts.iter().rev().find(|(a, _)| *a == a1).expect("the owner got a prompt");
    assert!(p.starts_with("[mantra:fix]") && p.contains("src/a.rs:3 returns the wrong value") && p.contains("src/a.rs"), "{p}");
    let i = t(&run, "t1");
    assert_eq!(run.workers[i].state, WState::Running);
    assert_eq!(run.workers[i].owner_fixing, Some(1));
    assert!(run.expected_active().iter().any(|(a, e)| *a == a1 && *e == Expect::Working("t1".into())));
    // it finishes
    ctx.agents.get_mut(&a1).unwrap().final_message = Some("STATUS: done\nSUMMARY: returns the right value now".into());
    run.on_turn_done(&mut ctx, a1, "completed", None, None);
    assert_eq!(run.workers[i].state, WState::Done);
    assert_eq!(run.workers[i].owner_fixing, None);
    assert!(run.gate_notes.iter().any(|n| n.contains("t1 (owner fix r1)")), "{:?}", run.gate_notes);
    assert!(!ctx.prompts.iter().any(|(a, p)| *a == orch && p.contains("t1 done again")), "the orchestrator is not woken for it");
    assert!(ctx.jobs.iter().any(|j| j.starts_with("OwnerMerge")), "{:?}", ctx.jobs);
    // its branch merges cleanly: the checks run again, then QA round 2 lists the owner's repair
    run.on_job(&mut ctx, JobTag::OwnerMerge { phase: 0, round: 1 }, JobOut::Merge(git::MergeResult { merged: vec!["t1".into()], conflicts: vec![], blocked: vec![], log: String::new() }));
    assert!(ctx.jobs.iter().any(|j| j.starts_with("FixChecks")), "{:?}", ctx.jobs);
    assert!(run.orch_inbox.iter().any(|l| l.starts_with("Gate fix r1 by t1 finished: SUMMARY: returns the right value now")), "{:?}", run.orch_inbox);
    run.on_job(&mut ctx, JobTag::FixChecks { phase: 0, round: 1, gen: run.checks_gen }, JobOut::Checks(green(), vec![], git::ReviewDiff::default(), None));
    assert!(matches!(run.stage, Stage::Phase { step: PhaseStep::Gate { round: 2 }, .. }), "{:?}", run.stage);
    let g2 = run.gate_agent.expect("round 2 has a fresh QA agent");
    let (_, p2) = ctx.prompts.iter().rev().find(|(a, _)| *a == g2).unwrap();
    assert!(p2.contains("t1 (its own worker, round 1)") && p2.contains("returns the right value now"), "{p2}");
}

#[test]
fn an_owner_that_is_blocked_hands_the_round_to_a_fix_worker_with_its_report() {
    let mut ctx = TestCtx::new();
    let (mut run, gate, a1, _a2) = owner_run(&mut ctx);
    gate_says(&mut ctx, &mut run, gate, "GATE: fail — src/a.rs:3 returns the wrong value");
    run.on_job(&mut ctx, JobTag::OwnerSync { phase: 0, round: 1, task: "t1".into() }, JobOut::Text(Ok(String::new())));
    ctx.agents.get_mut(&a1).unwrap().final_message = Some("STATUS: blocked\nSUMMARY: the spec contradicts itself".into());
    run.on_turn_done(&mut ctx, a1, "completed", None, None);
    assert!(!run.halted(), "no halt");
    let i = t(&run, "t1");
    assert_eq!(run.workers[i].state, WState::Done, "its original work is still merged");
    assert!(run.workers[i].owner_fix_failed);
    assert_eq!(run.fix_owner, None);
    let fix = run.workers.iter().find(|w| w.task.id == "fix-1").expect("a fix worker takes it");
    assert!(fix.prompt.contains("src/a.rs:3 returns the wrong value") && fix.prompt.contains("The task's own worker tried first and reported:") && fix.prompt.contains("the spec contradicts itself"), "{}", fix.prompt);
    assert!(run.journal_text(60).contains("t1 could not repair it (blocked) — a fix worker takes it"));
    assert!(run.orch_inbox.iter().any(|l| l.starts_with("Gate fix r1 by t1 could not be completed")), "{:?}", run.orch_inbox);
    assert!(!run.orch_inbox.iter().any(|l| l.contains("FAILED (blocked)")));
}

#[test]
fn an_owner_whose_turn_fails_hands_the_round_to_a_fix_worker() {
    let mut ctx = TestCtx::new();
    let (mut run, gate, a1, _a2) = owner_run(&mut ctx);
    gate_says(&mut ctx, &mut run, gate, "GATE: fail — src/a.rs:3 returns the wrong value");
    run.on_job(&mut ctx, JobTag::OwnerSync { phase: 0, round: 1, task: "t1".into() }, JobOut::Text(Ok(String::new())));
    run.on_turn_done(&mut ctx, a1, "failed", Some("model said no".into()), None);
    assert!(!run.halted());
    assert!(run.workers.iter().any(|w| w.task.id == "fix-1"));
    assert_eq!(run.workers[t(&run, "t1")].state, WState::Done);
}

#[test]
fn an_owner_that_cannot_sync_hands_the_round_to_a_fix_worker() {
    let mut ctx = TestCtx::new();
    let (mut run, gate, a1, _a2) = owner_run(&mut ctx);
    gate_says(&mut ctx, &mut run, gate, "GATE: fail — src/a.rs:3 returns the wrong value");
    run.on_job(&mut ctx, JobTag::OwnerSync { phase: 0, round: 1, task: "t1".into() }, JobOut::Text(Err("the task's worktree does not merge with the integrated copy: CONFLICT".into())));
    assert!(!ctx.prompts.iter().any(|(a, _)| *a == a1));
    assert!(run.workers.iter().any(|w| w.task.id == "fix-1"), "a fix worker takes it");
    assert_eq!(run.fix_owner, None);
    assert!(run.journal_text(60).contains("t1's worker cannot take the fix (the task's worktree does not merge with the integrated copy: CONFLICT) — a fix worker takes it"), "{}", run.journal_text(60));
    assert!(matches!(run.stage, Stage::Phase { step: PhaseStep::Fix { round: 1 }, .. }));
}

#[test]
fn an_owner_whose_repair_no_longer_merges_goes_to_a_fix_worker_that_merges_by_hand() {
    let mut ctx = TestCtx::new();
    let (mut run, gate, a1, _a2) = owner_run(&mut ctx);
    gate_says(&mut ctx, &mut run, gate, "GATE: fail — src/a.rs:3 returns the wrong value");
    run.on_job(&mut ctx, JobTag::OwnerSync { phase: 0, round: 1, task: "t1".into() }, JobOut::Text(Ok(String::new())));
    ctx.agents.get_mut(&a1).unwrap().final_message = Some("STATUS: done\nSUMMARY: fixed".into());
    run.on_turn_done(&mut ctx, a1, "completed", None, None);
    run.on_job(&mut ctx, JobTag::OwnerMerge { phase: 0, round: 1 }, JobOut::Merge(git::MergeResult { merged: vec![], conflicts: vec!["mantra-w/t1".into()], blocked: vec![], log: String::new() }));
    let fix = run.workers.iter().find(|w| w.task.id == "fix-1").expect("a fix worker merges it");
    assert!(fix.prompt.contains("The task's own worker repaired its files, but its branch no longer merges") && fix.prompt.contains("mantra-w/t1"), "{}", fix.prompt);
    assert_eq!(fix.task.role, "worker-big");
}

#[test]
fn a_blocked_integration_after_an_owner_repair_halts() {
    let mut ctx = TestCtx::new();
    let (mut run, gate, a1, _a2) = owner_run(&mut ctx);
    gate_says(&mut ctx, &mut run, gate, "GATE: fail — src/a.rs:3 returns the wrong value");
    run.on_job(&mut ctx, JobTag::OwnerSync { phase: 0, round: 1, task: "t1".into() }, JobOut::Text(Ok(String::new())));
    ctx.agents.get_mut(&a1).unwrap().final_message = Some("STATUS: done\nSUMMARY: fixed".into());
    run.on_turn_done(&mut ctx, a1, "completed", None, None);
    run.on_job(&mut ctx, JobTag::OwnerMerge { phase: 0, round: 1 }, JobOut::Merge(git::MergeResult { merged: vec![], conflicts: vec![], blocked: vec!["t1: commit failed".into()], log: String::new() }));
    assert!(run.halted());
    assert!(run.halt.as_ref().unwrap().message.contains("integration blocked"));
}

#[test]
fn with_gate_fix_set_to_fix_worker_the_round_goes_to_a_fix_worker_as_before() {
    let mut ctx = TestCtx::new();
    let (mut run, gate, a1, _a2) = owner_run(&mut ctx);
    run.pattern.settings.gate_fix = "fix-worker".into();
    gate_says(&mut ctx, &mut run, gate, "GATE: fail — src/a.rs:3 returns the wrong value");
    assert!(run.workers.iter().any(|w| w.task.id == "fix-1"));
    assert!(!ctx.jobs.iter().any(|j| j.starts_with("OwnerSync")), "{:?}", ctx.jobs);
    assert_eq!(run.fix_owner, None);
    assert!(!ctx.prompts.iter().any(|(a, _)| *a == a1));
    assert!(run.orch_inbox.iter().any(|l| l.contains("the fix goes to fix-1")), "{:?}", run.orch_inbox);
}

#[test]
fn a_plan_revision_during_an_owner_fix_interrupts_the_owner_and_reruns_the_checks() {
    let mut ctx = TestCtx::new();
    let (mut run, gate, a1, _a2) = owner_run(&mut ctx);
    let planner = ctx.add_idle();
    run.planner = Some(planner);
    let _ = gate;
    gate_says(&mut ctx, &mut run, gate, "GATE: fail — src/a.rs:3 returns the wrong value");
    run.on_job(&mut ctx, JobTag::OwnerSync { phase: 0, round: 1, task: "t1".into() }, JobOut::Text(Ok(String::new())));
    let i = t(&run, "t1");
    assert_eq!(run.workers[i].state, WState::Running);
    ctx.jobs.clear();
    let (msg, ok) = run.handle_tool(&mut ctx, planner, "mantra_revise_plan", &revised_checks(&run, &["new-check --flag"]));
    assert!(ok, "{msg}");
    assert!(ctx.interrupted.contains(&a1), "the owner is interrupted");
    assert_eq!(run.workers[i].state, WState::Done);
    assert_eq!(run.workers[i].owner_fixing, None);
    assert_eq!(run.fix_owner, None);
    assert!(ctx.jobs.iter().any(|j| j.starts_with("Checks")), "{:?}", ctx.jobs);
    // the interrupted turn ending is nobody's news
    run.on_turn_done(&mut ctx, a1, "interrupted", None, None);
    assert!(!run.orch_inbox.iter().any(|l| l.contains("interrupted")));
}

#[test]
fn a_halt_lifted_in_an_owner_fix_picks_up_where_it_is() {
    let mut ctx = TestCtx::new();
    let (mut run, gate, a1, _a2) = owner_run(&mut ctx);
    gate_says(&mut ctx, &mut run, gate, "GATE: fail — src/a.rs:3 returns the wrong value");
    run.on_job(&mut ctx, JobTag::OwnerSync { phase: 0, round: 1, task: "t1".into() }, JobOut::Text(Ok(String::new())));
    // still working: nothing new starts
    ctx.jobs.clear();
    run.resume_fix(&mut ctx, 0, 1);
    assert!(ctx.jobs.is_empty() && !run.workers.iter().any(|w| w.task.id == "fix-1"));
    // an owner that is not repairing any more and never finished: the fix starts over
    let i = t(&run, "t1");
    run.workers[i].state = WState::Done;
    run.workers[i].owner_fixing = None;
    ctx.agents.get_mut(&a1).unwrap().turn_active = false;
    run.fix_owner = Some("t1".into());
    run.resume_fix(&mut ctx, 0, 1);
    assert!(ctx.jobs.iter().any(|j| j.starts_with("FixChecks")), "it was done: the checks run {:?}", ctx.jobs);
}

#[test]
fn a_generic_fix_also_tells_the_orchestrator_how_it_ended() {
    let mut ctx = TestCtx::new();
    let mut run = Run::new(PathBuf::from("."), no_manager(), "test".into());
    let gate = ctx.add_idle();
    run.gate_agent = Some(gate);
    run.plan = Some(one_task_phase());
    run.stage = Stage::Phase { idx: 0, step: PhaseStep::Gate { round: 1 } };
    gate_says(&mut ctx, &mut run, gate, "GATE: fail — README needs a usage section");
    finish_fix(&mut ctx, &mut run, 1);
    assert!(run.orch_inbox.iter().any(|l| l == "Gate fix r1 by fix-1 finished: SUMMARY: fixed what was listed. Mantra re-runs the checks and QA."), "{:?}", run.orch_inbox);
}

#[test]
fn gate_fix_is_a_setting_that_old_pattern_files_default() {
    let p = Pattern::builtin();
    assert_eq!(p.settings.gate_fix, "owner");
    let toml = p.to_toml();
    let old = toml.replace("gate_fix = \"owner\"\n", "");
    assert!(!old.contains("gate_fix"), "the fixture must be missing the key");
    assert_eq!(Pattern::from_toml(&old).unwrap().settings.gate_fix, "owner");
    let mut bad = Pattern::builtin();
    bad.settings.gate_fix = "somebody".into();
    assert!(bad.validate().unwrap_err().iter().any(|e| e.contains("gate_fix")));
    bad.settings.gate_fix = "fix-worker".into();
    assert!(bad.validate().is_ok());
}
