//! Resuming a run from its `state.json` (`mantra runs resume <id>`, `/runs` ⏎).
//!
//! A resume never tries to replay what the old process was in the middle of. It restarts the
//! Conductor at the nearest *safe boundary* of the saved stage:
//!
//! | saved stage                | what happens                                                   |
//! |----------------------------|----------------------------------------------------------------|
//! | Setup                      | workspace setup again (same id, same run dir)                  |
//! | Planning                   | planner re-attached (or fresh) and asked to submit the plan    |
//! | Review                     | the saved plan is put back up for review (no agent needed)     |
//! | Phase · working            | fresh orchestrator; running workers re-attached to their       |
//! |                            | threads when the worktree is still there, otherwise re-spawned |
//! | Phase · merging/checks/gate| the (idempotent) merge → checks → gate sequence runs again     |
//! | Phase · handoff            | straight on to the next phase / the finale                     |
//! | Finale i                   | finale step i starts over (ad-hoc fixes re-spawned)            |
//! | Done / Failed              | opened read-only — `/land` still works for a finished run      |

use super::git;
use super::pattern::Pattern;
use super::plan::{Plan, Task};
use super::run::{Ctx, Pending, PhaseRecord, PhaseStep, Run, Stage, WState, Worker};
use super::state::{self, RunState};
use std::path::PathBuf;
use std::time::{Duration, Instant};

impl Run {
    /// A `Run` rebuilt from disk. Nothing is spawned yet — `resume_boot` does that.
    pub fn from_state(st: &RunState, pattern: Pattern, plan: Option<Plan>, dir: PathBuf) -> Run {
        let mut run = Run::new(st.project.clone(), pattern, st.brief.clone());
        run.id = st.id.clone();
        run.dir = dir;
        run.stage = st.stage.clone();
        run.ws = st.ws.clone();
        run.plan = plan;
        run.plan_version = st.plan_version;
        run.handoff = st.handoff.clone();
        run.started_unix = st.started_unix;
        // a finished run written before `finished_unix` existed: its last save is the closest
        run.finished_unix = st.finished_unix.or_else(|| st.finished().then_some(st.updated_unix));
        run.pending = st.pending.clone();
        run.restore_clock(st, crate::util::unix_secs());
        run.history = st.history.iter().map(|h| PhaseRecord { name: h.name.clone(), workers: h.workers.clone(), duration: Duration::from_secs(h.secs), ended: Instant::now() }).collect();
        run
    }

    /// Carry the active-time clock over from disk: the saved total, then a new active span from
    /// `now` — unless the run is already over, in which case it stays frozen. A state file from
    /// before the clock existed falls back to its wall-clock span (up to `finished_unix` when stamped).
    pub(super) fn restore_clock(&mut self, st: &RunState, now: u64) {
        // `Some(0)` is a real reading (halted within its first second, say) — only a file without
        // the field at all falls back, or a run parked for hours right after a halt resumes at hours.
        let secs = st.active_secs.unwrap_or_else(|| st.finished_unix.unwrap_or(st.updated_unix).saturating_sub(st.started_unix));
        self.set_clock(secs, if self.is_active() { Some(now) } else { None });
    }

    /// Pick the run back up at a safe boundary (see the module doc).
    pub fn resume_boot(&mut self, ctx: &mut dyn Ctx, st: &RunState) {
        let label = state::stage_label(&self.stage);
        let was = st.halted.as_ref().map(|h| format!(" (it was halted: {h})")).unwrap_or_default();
        self.log("↻", "saffron", format!("resumed at {label}{was}"));
        if let Some(ws) = self.ws.clone() {
            if ws.worktree && !ws.integ.is_dir() {
                match git::reattach(&ws) {
                    Ok(()) => self.log("⎇", "blue", format!("re-created the integration worktree at {}", ws.integ.display())),
                    Err(e) => {
                        self.fail_run(ctx, format!("the integration worktree is gone and could not be re-created: {e}"));
                        return;
                    }
                }
            }
        }
        // A transition the pause boundary deferred (`Pending`) is performed below instead of the
        // stage's usual boot step — once. Before the first phase the planner simply submits its
        // plan again, and a due gate is reached by the merge → checks → gate sequence anyway
        // (which regenerates the check results its prompt needs), so those are dropped here.
        let pending = self.pending.take();
        if let (Some(p), Stage::Setup | Stage::Planning | Stage::Review) = (&pending, &self.stage) {
            self.log("‖", "amber", format!("{} was due at the pause — the plan is submitted again instead", p.label()));
        }
        match self.stage.clone() {
            Stage::Setup => self.start(ctx),
            Stage::Planning => self.resume_planning(ctx, st),
            Stage::Review => {
                if self.plan.is_some() {
                    self.want_review = true;
                    self.log("✎", "saffron", "the plan is up for your review again");
                } else {
                    self.resume_planning(ctx, st);
                }
            }
            Stage::Phase { idx, step } => {
                if self.plan.as_ref().and_then(|p| p.phases.get(idx)).is_none() {
                    self.fail_run(ctx, "plan.json is missing (or has no such phase) — this run cannot be resumed".into());
                    return;
                }
                self.resume_manager(ctx, st);
                self.restore_workers(st);
                match pending {
                    Some(p @ (Pending::Phase { .. } | Pending::Finale { .. } | Pending::Handoff { .. })) => self.perform_pending(ctx, p),
                    other => {
                        if let Some(p) = other {
                            self.log("‖", "amber", format!("{} was due at the pause — re-running merge/checks reaches it instead", p.label()));
                        }
                        match step {
                            PhaseStep::Orchestrating => self.resume_orchestrating(ctx, idx, st),
                            PhaseStep::Handoff => {
                                self.stage = Stage::Phase { idx, step: PhaseStep::Handoff };
                                self.handoff_note_done = true;
                                self.cleanup_done = true;
                                self.maybe_next_phase(ctx);
                            }
                            PhaseStep::Merging | PhaseStep::Checks { .. } | PhaseStep::Gate { .. } => {
                                // Every task had finished; merging is idempotent, so start the gate
                                // sequence from the top.
                                self.stage = Stage::Phase { idx, step: PhaseStep::Orchestrating };
                                self.check_phase_done(ctx);
                                if matches!(self.stage, Stage::Phase { step: PhaseStep::Orchestrating, .. }) {
                                    // state.json disagreed with itself (a task isn't done after all) —
                                    // then the phase is simply still being worked on.
                                    self.resume_orchestrating(ctx, idx, st);
                                }
                            }
                        }
                    }
                }
            }
            Stage::Finale { idx } => {
                self.resume_manager(ctx, st);
                self.restore_workers(st);
                // Unfinished ad-hoc fixes lost their agent with the old process: fresh attempts.
                let redo: Vec<(Task, String, Option<String>)> = self
                    .workers
                    .iter_mut()
                    .filter(|w| w.adhoc && !matches!(w.state, WState::Done | WState::Failed(_) | WState::Cancelled))
                    .map(|w| {
                        w.state = WState::Cancelled;
                        (w.task.clone(), w.prompt.clone(), w.effort.clone())
                    })
                    .collect();
                // the previous step reported while the run was paused: its successor was due
                match pending {
                    Some(p @ Pending::Finale { .. }) => self.perform_pending(ctx, p),
                    _ => self.start_finale(ctx, idx),
                }
                for (task, prompt, effort) in redo {
                    self.spawn_task(ctx, task, Some(prompt), effort);
                }
            }
            Stage::Done => self.log("✦", "saffron", "this run is finished — /land merges its branch; delete it from /runs when you're done with it"),
            Stage::Failed(why) => self.log("✗", "red", format!("this run had failed ({why}) — opened read-only; delete it from /runs")),
        }
        self.save_state();
    }

    /// The manager (`flow.manager`) comes back with its own thread when the state has one — its
    /// memory of the run so far is the point of it — otherwise fresh, briefed like a respawn.
    fn resume_manager(&mut self, ctx: &mut dyn Ctx, st: &RunState) {
        if !self.has_manager() {
            return;
        }
        let thread = st.agents.iter().find(|a| a.slot == "manager").and_then(|a| a.thread_id.clone());
        let attached = thread.is_some();
        let m = self.spawn_manager_with(ctx, thread);
        self.mark_edge(m);
        let brief = self.manager_brief(if attached { "[mantra:resume] Mantra restarted; you are re-attached as this run's manager. What you knew still holds — here is the run as it stands now." } else { "[mantra:resume] Mantra restarted; you are this run's new manager." });
        ctx.prompt(m, brief);
        self.log("◈", "blue", if attached { "manager re-attached" } else { "manager started (fresh — no saved thread)" });
    }

    fn resume_planning(&mut self, ctx: &mut dyn Ctx, st: &RunState) {
        self.stage = Stage::Planning;
        let thread = st.agents.iter().find(|a| a.slot == "planner").and_then(|a| a.thread_id.clone());
        let attached = thread.is_some();
        let p = self.spawn_planner_with(ctx, thread);
        self.mark_edge(p);
        let prompt = if attached {
            format!("[mantra:resume] Mantra restarted while you were planning. Whatever you already explored still counts — continue and submit the plan with mantra_submit_plan.\n\n{}", self.planning_prompt())
        } else {
            self.planning_prompt()
        };
        ctx.prompt(p, prompt);
        self.log("✦", "saffron", if attached { "planner re-attached — finishing the plan" } else { "planner is exploring and planning" });
    }

    /// Phase `idx` was being worked on: a fresh orchestrator, workers re-attached or re-spawned.
    fn resume_orchestrating(&mut self, ctx: &mut dyn Ctx, idx: usize, st: &RunState) {
        self.stage = Stage::Phase { idx, step: PhaseStep::Orchestrating };
        self.phase_started = Instant::now();
        // The orchestrator's old process (and its context) is gone: always a fresh one, briefed
        // with the current state below.
        let o = self.spawn_orchestrator(ctx);
        let unfinished: Vec<usize> = self.workers.iter().enumerate().filter(|(_, w)| matches!(w.state, WState::Queued | WState::Preparing | WState::Running | WState::Retrying(_))).map(|(i, _)| i).collect();
        let mut attached = 0;
        let mut respawned = 0;
        for wi in unfinished {
            let w = &self.workers[wi];
            let slot = format!("worker:{}#{}", w.task.id, w.attempt);
            let thread = st.agents.iter().find(|a| a.slot == slot).and_then(|a| a.thread_id.clone());
            let dir = w.wt.clone().filter(|d| d.is_dir());
            match (w.state == WState::Running, dir, thread) {
                (true, Some(dir), Some(t)) => {
                    let branch = w.branch.clone();
                    self.launch_worker_with(ctx, wi, dir, branch, Some(t));
                    attached += 1;
                }
                _ => {
                    let task = w.task.clone();
                    let prompt = w.prompt.clone();
                    let effort = w.effort.clone();
                    self.workers[wi].state = WState::Cancelled;
                    self.spawn_task(ctx, task, Some(prompt), effort);
                    respawned += 1;
                }
            }
        }
        if attached + respawned > 0 {
            self.log("↻", "violet", format!("workers: {attached} re-attached, {respawned} re-spawned"));
        }
        let phase = self.plan.as_ref().and_then(|p| p.phases.get(idx)).cloned().unwrap_or_default();
        let n = self.plan.as_ref().map(|p| p.phases.len()).unwrap_or(0);
        let status = self.status_text(ctx);
        let phase_json = serde_json::to_string_pretty(&phase).unwrap_or_default();
        let handoff = if self.handoff.is_empty() { String::new() } else { format!("Handoff from the previous phase:\n{}\n\n", self.handoff) };
        self.mark_edge(o);
        ctx.prompt(
            o,
            format!(
                "[mantra:resume] Mantra restarted while phase {}/{} ({}) was in progress; you are its new orchestrator.\nGoal: {}\n\n{handoff}Current phase:\n```json\n{phase_json}\n```\nWorkers right now (attempts already made are listed; running ones were re-attached or re-spawned by Mantra — do not spawn them again):\n{status}\nSpawn only tasks that have no attempt yet (mantra_spawn), then call mantra_wait.",
                idx + 1,
                n,
                phase.name,
                phase.goal
            ),
        );
        self.fill_slots(ctx);
        self.check_phase_done(ctx);
    }

    /// Workers from `state.json` → `Run.workers` (no agents attached yet). A worktree that no
    /// longer exists on disk is dropped (`wt = None`), so a later merge skips it.
    fn restore_workers(&mut self, st: &RunState) {
        let phase_tasks: Vec<Task> = match self.stage {
            Stage::Phase { idx, .. } => self.plan.as_ref().and_then(|p| p.phases.get(idx)).map(|ph| ph.tasks.clone()).unwrap_or_default(),
            _ => vec![],
        };
        self.workers = st
            .workers
            .iter()
            .map(|w| {
                let task = phase_tasks.iter().find(|t| t.id == w.task).cloned().unwrap_or_else(|| Task { id: w.task.clone(), title: w.title.clone(), role: w.role.clone(), prompt: w.prompt.clone(), scope: vec![], acceptance: String::new(), effort: w.effort.clone() });
                let state = match w.state.as_str() {
                    "done" => WState::Done,
                    "failed" => WState::Failed(w.error.clone()),
                    "cancelled" => WState::Cancelled,
                    "running" => WState::Running,
                    _ => WState::Queued,
                };
                Worker {
                    task,
                    prompt: w.prompt.clone(),
                    effort: w.effort.clone(),
                    agent: None,
                    attempt: w.attempt,
                    state,
                    report: w.report.clone(),
                    wt: w.worktree.clone().filter(|d| d.is_dir()),
                    branch: w.branch.clone(),
                    spawned: Instant::now(),
                    finished: None,
                    tripwires: vec![],
                    adhoc: w.adhoc,
                    paused: false,
                    stall_flagged: false,
                    budget_flagged: false,
                    context_override: None,
                    idle_prompts: vec![],
                }
            })
            .collect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Agent;
    use crate::engine::git::Workspace;
    use crate::engine::plan::{Gate, Phase};
    use crate::engine::run::{JobOut, JobTag, Send, SpawnReq};
    use crate::engine::state::{AgentState, WorkerState, FORMAT};
    use crate::hub::AgentId;
    use serde_json::Value;
    use std::collections::HashMap;

    #[test]
    fn restore_clock_carries_active_time_over() {
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        let st = RunState { started_unix: 100, updated_unix: 160, active_secs: Some(45), ..Default::default() };
        run.restore_clock(&st, 1000);
        assert_eq!(run.elapsed_at(1010).as_secs(), 55, "saved total plus the new span");
        // an older state file (no clock) falls back to its wall-clock span
        let old = RunState { started_unix: 100, updated_unix: 160, ..Default::default() };
        run.restore_clock(&old, 1000);
        assert_eq!(run.elapsed_at(1000).as_secs(), 60);
        // a run halted within its first second and left overnight: 0 is a reading, not "no clock"
        let parked = RunState { started_unix: 100, updated_unix: 30000, active_secs: Some(0), ..Default::default() };
        run.restore_clock(&parked, 40000);
        assert_eq!(run.elapsed_at(40003).as_secs(), 3, "the hours it sat halted are not active time");
        // a finished run stays frozen
        run.stage = Stage::Done;
        run.restore_clock(&st, 1000);
        assert_eq!(run.elapsed_at(5000).as_secs(), 45);
    }

    struct Fake {
        agents: HashMap<AgentId, Agent>,
        next: AgentId,
        spawned: Vec<(String, Option<String>)>,
        prompts: Vec<(AgentId, String)>,
        jobs: Vec<String>,
    }
    impl Fake {
        fn new() -> Fake {
            Fake { agents: HashMap::new(), next: 1, spawned: vec![], prompts: vec![], jobs: vec![] }
        }
        fn add(&mut self, r: &SpawnReq, resume: Option<String>) -> AgentId {
            let id = self.next;
            self.next += 1;
            self.agents.insert(id, Agent::new(id, &r.name, &r.role_name, r.cwd.clone()));
            self.spawned.push((r.name.clone(), resume));
            id
        }
    }
    impl Ctx for Fake {
        fn spawn(&mut self, r: SpawnReq) -> AgentId {
            self.add(&r, None)
        }
        fn spawn_resumed(&mut self, r: SpawnReq, t: String) -> AgentId {
            self.add(&r, Some(t))
        }
        fn prompt(&mut self, a: AgentId, text: String) {
            self.prompts.push((a, text));
        }
        fn prompt_mode(&mut self, a: AgentId, text: String, _m: Send) {
            self.prompts.push((a, text));
        }
        fn interrupt(&mut self, _a: AgentId) {}
        fn compact(&mut self, _a: AgentId) {}
        fn stop(&mut self, _a: AgentId, _archive: bool) {}
        fn tool_result(&mut self, _a: AgentId, _req: Value, _t: String, _ok: bool) {}
        fn set_effort(&mut self, _a: AgentId, e: &str) -> String {
            e.to_string()
        }
        fn agent(&self, a: AgentId) -> Option<&Agent> {
            self.agents.get(&a)
        }
        fn job(&mut self, tag: JobTag, _f: Box<dyn FnOnce() -> JobOut + std::marker::Send>) {
            self.jobs.push(format!("{tag:?}"));
        }
        fn notify(&mut self, _t: &str) {}
    }

    fn plan() -> Plan {
        let task = |id: &str| Task { id: id.into(), title: id.to_uppercase(), role: "worker-small".into(), prompt: format!("do {id}"), ..Default::default() };
        Plan {
            title: "t".into(),
            summary: "s".into(),
            orchestrator_brief: "b".into(),
            phases: vec![Phase { id: "p1".into(), name: "one".into(), goal: "g".into(), tasks: vec![task("a"), task("b"), task("c"), task("d")], gate: Gate::default() }, Phase { id: "p2".into(), name: "two".into(), ..Default::default() }],
            final_checks: vec![],
        }
    }

    fn worker(task: &str, state: &str, attempt: u32, wt: Option<PathBuf>) -> WorkerState {
        WorkerState { task: task.into(), title: task.to_uppercase().into(), role: "worker-small".into(), state: state.into(), attempt, branch: format!("mantra-w/r/{task}-{attempt}"), worktree: wt, prompt: format!("do {task}"), report: if state == "done" { "Summary: fine".into() } else { String::new() }, ..Default::default() }
    }

    fn state(tmp: &PathBuf, stage: Stage, workers: Vec<WorkerState>, agents: Vec<AgentState>) -> RunState {
        RunState {
            format: FORMAT,
            id: "r".into(),
            project: tmp.clone(),
            brief: "brief".into(),
            pattern: "mantra-default".into(),
            plan_version: 1,
            stage,
            ws: Some(Workspace { worktree: true, repo: tmp.clone(), integ: tmp.join("integration"), branch: "mantra/r".into(), base_branch: "main".into(), git_common_dir: None, note: None }),
            workers,
            agents,
            ..Default::default()
        }
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mantra-resume-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("integration")).unwrap();
        d
    }

    #[test]
    fn orchestrating_phase_gets_a_fresh_orchestrator_and_workers_reattach_or_respawn() {
        let dir = tmp("orch");
        std::fs::create_dir_all(dir.join("wt-b")).unwrap();
        let st = state(
            &dir,
            Stage::Phase { idx: 0, step: PhaseStep::Orchestrating },
            vec![
                worker("a", "done", 1, Some(dir.join("wt-a"))),           // finished: kept as is
                worker("b", "running", 2, Some(dir.join("wt-b"))),        // worktree + thread → re-attach
                worker("c", "running", 1, Some(dir.join("wt-gone"))),     // worktree gone → fresh attempt
                worker("d", "queued", 1, None),                           // never started → fresh attempt
            ],
            vec![
                AgentState { slot: "worker:b#2".into(), thread_id: Some("thr-b".into()), ..Default::default() },
                AgentState { slot: "orchestrator".into(), thread_id: Some("thr-o".into()), ..Default::default() },
            ],
        );
        let mut run = Run::from_state(&st, Pattern::builtin(), Some(plan()), dir.join("run"));
        let mut ctx = Fake::new();
        run.resume_boot(&mut ctx, &st);

        assert!(ctx.spawned.iter().any(|(n, r)| n == "orchestrator" && r.is_none()), "orchestrator is always fresh: {:?}", ctx.spawned);
        assert!(ctx.spawned.iter().any(|(n, r)| n == "b" && r.as_deref() == Some("thr-b")), "b re-attaches to its thread: {:?}", ctx.spawned);
        assert!(!ctx.spawned.iter().any(|(n, _)| n == "a"), "a finished before the crash");
        // c and d get new attempts (worktree jobs were requested for them)
        let c_attempts: Vec<u32> = run.workers.iter().filter(|w| w.task.id == "c").map(|w| w.attempt).collect();
        assert_eq!(c_attempts, vec![1, 2]);
        assert!(run.workers.iter().any(|w| w.task.id == "c" && w.attempt == 1 && w.state == WState::Cancelled));
        assert!(run.workers.iter().any(|w| w.task.id == "d" && w.attempt == 2 && w.state == WState::Preparing));
        assert_eq!(ctx.jobs.iter().filter(|j| j.contains("WorkerWt")).count(), 2, "{:?}", ctx.jobs);
        let (o, _) = ctx.prompts.iter().find(|(_, t)| t.contains("[mantra:resume] Mantra restarted while phase 1/2")).expect("orchestrator briefed");
        assert_eq!(Some(*o), run.orchestrator);
        assert!(ctx.prompts.iter().any(|(_, t)| t.contains("your worktree still holds your changes")), "re-attached worker told to continue");
        assert_eq!(run.stage, Stage::Phase { idx: 0, step: PhaseStep::Orchestrating });
        assert!(run.dir.join("state.json").is_file(), "state is re-saved after boot");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn review_puts_the_saved_plan_back_up_without_spawning() {
        let dir = tmp("review");
        let st = state(&dir, Stage::Review, vec![], vec![]);
        let mut run = Run::from_state(&st, Pattern::builtin(), Some(plan()), dir.join("run"));
        let mut ctx = Fake::new();
        run.resume_boot(&mut ctx, &st);
        assert!(run.want_review);
        assert!(ctx.spawned.is_empty());
        assert_eq!(run.stage, Stage::Review);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn planning_reattaches_the_planner_when_its_thread_is_known() {
        let dir = tmp("planning");
        let st = state(&dir, Stage::Planning, vec![], vec![AgentState { slot: "planner".into(), thread_id: Some("thr-p".into()), ..Default::default() }]);
        let mut run = Run::from_state(&st, Pattern::builtin(), None, dir.join("run"));
        let mut ctx = Fake::new();
        run.resume_boot(&mut ctx, &st);
        assert_eq!(ctx.spawned, vec![("planner".to_string(), Some("thr-p".to_string()))]);
        assert!(ctx.prompts.iter().any(|(_, t)| t.starts_with("[mantra:resume]") && t.contains("mantra_submit_plan")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn gate_stage_reruns_the_merge_when_every_task_is_done() {
        let dir = tmp("gate");
        let mut p = plan();
        p.phases[0].tasks.truncate(2);
        let st = state(&dir, Stage::Phase { idx: 0, step: PhaseStep::Gate { round: 2 } }, vec![worker("a", "done", 1, Some(dir.join("integration"))), worker("b", "done", 1, None)], vec![]);
        let mut run = Run::from_state(&st, Pattern::builtin(), Some(p), dir.join("run"));
        let mut ctx = Fake::new();
        run.resume_boot(&mut ctx, &st);
        assert_eq!(run.stage, Stage::Phase { idx: 0, step: PhaseStep::Merging });
        assert!(ctx.jobs.iter().any(|j| j.contains("Merge")), "{:?}", ctx.jobs);
        assert_eq!(ctx.spawned, vec![("manager".to_string(), None)], "no phase agent is needed until the checks come back — only the run-wide manager comes back (fresh: no saved thread)");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn phase_resume_reattaches_the_manager_to_its_thread() {
        let dir = tmp("manager");
        let st = state(&dir, Stage::Phase { idx: 0, step: PhaseStep::Orchestrating }, vec![worker("a", "done", 1, None)], vec![AgentState { slot: "manager".into(), thread_id: Some("thr-m".into()), ..Default::default() }]);
        let mut run = Run::from_state(&st, Pattern::builtin(), Some(plan()), dir.join("run"));
        let mut ctx = Fake::new();
        run.resume_boot(&mut ctx, &st);
        assert!(ctx.spawned.contains(&("manager".to_string(), Some("thr-m".to_string()))), "{:?}", ctx.spawned);
        let m = run.manager.expect("manager set");
        assert!(ctx.prompts.iter().any(|(a, t)| *a == m && t.contains("[mantra:resume]") && t.contains("re-attached") && t.contains("phase 1")), "{:?}", ctx.prompts);
        // a pattern without a manager spawns none
        let mut p = Pattern::builtin();
        p.flow.manager = String::new();
        let mut run2 = Run::from_state(&st, p, Some(plan()), dir.join("run2"));
        let mut ctx2 = Fake::new();
        run2.resume_boot(&mut ctx2, &st);
        assert!(!ctx2.spawned.iter().any(|(n, _)| n == "manager"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn handoff_stage_moves_on_to_the_next_phase() {
        let dir = tmp("handoff");
        let st = state(&dir, Stage::Phase { idx: 0, step: PhaseStep::Handoff }, vec![worker("a", "done", 1, None)], vec![]);
        let mut run = Run::from_state(&st, Pattern::builtin(), Some(plan()), dir.join("run"));
        let mut ctx = Fake::new();
        run.resume_boot(&mut ctx, &st);
        assert_eq!(run.stage, Stage::Phase { idx: 1, step: PhaseStep::Orchestrating });
        assert!(ctx.spawned.iter().any(|(n, _)| n == "orchestrator"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn completed_phases_survive_a_resume() {
        let dir = tmp("history");
        let mut st = state(&dir, Stage::Finale { idx: 0 }, vec![], vec![]);
        st.history = vec![crate::engine::state::PhaseHistory { name: "one".into(), workers: vec![("a".into(), "A".into(), "◇".into(), true)], secs: 7 }];
        let run = Run::from_state(&st, Pattern::builtin(), Some(plan()), dir.join("run"));
        assert_eq!(run.history.len(), 1);
        assert_eq!(run.history[0].name, "one");
        assert_eq!(run.history[0].duration, Duration::from_secs(7));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn finished_runs_open_read_only() {
        let dir = tmp("done");
        let st = state(&dir, Stage::Done, vec![], vec![]);
        let mut run = Run::from_state(&st, Pattern::builtin(), Some(plan()), dir.join("run"));
        let mut ctx = Fake::new();
        run.resume_boot(&mut ctx, &st);
        assert!(ctx.spawned.is_empty() && ctx.prompts.is_empty());
        assert!(!run.is_active());
        assert!(run.pulse.iter().any(|p| p.text.contains("/land")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_restored_run_performs_its_pending_transition_once() {
        let dir = tmp("pending");
        // paused in the phase-1 handoff, with phase 2 already due (the pause boundary held it)
        let mut st = state(&dir, Stage::Phase { idx: 0, step: PhaseStep::Handoff }, vec![worker("a", "done", 1, None)], vec![]);
        st.halted = Some("paused by you".into());
        st.pending = Some(Pending::Phase { idx: 1 });
        let mut run = Run::from_state(&st, Pattern::builtin(), Some(plan()), dir.join("run"));
        assert_eq!(run.pending, Some(Pending::Phase { idx: 1 }));
        let mut ctx = Fake::new();
        run.resume_boot(&mut ctx, &st);
        assert_eq!(run.stage, Stage::Phase { idx: 1, step: PhaseStep::Orchestrating });
        assert_eq!(ctx.spawned.iter().filter(|(n, _)| n == "orchestrator").count(), 1, "{:?}", ctx.spawned);
        assert_eq!(ctx.prompts.iter().filter(|(_, t)| t.contains("[mantra:phase] Phase 2/2")).count(), 1, "{:?}", ctx.prompts);
        assert!(run.pending.is_none() && !run.halted());
        let saved = RunState::load(&run.dir).unwrap();
        assert!(saved.pending.is_none() && saved.halted.is_none(), "performed once: the saved state no longer carries it");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_finished_run_comes_back_with_its_duration_frozen() {
        let dir = tmp("frozen");
        let mut st = state(&dir, Stage::Done, vec![], vec![]);
        st.started_unix = 1000;
        st.finished_unix = Some(1129);
        st.updated_unix = 5000;
        assert_eq!(Run::from_state(&st, Pattern::builtin(), Some(plan()), dir.join("run")).elapsed(), Duration::from_secs(129));
        // written before the completion stamp existed: the last save is the closest
        st.finished_unix = None;
        assert_eq!(Run::from_state(&st, Pattern::builtin(), Some(plan()), dir.join("run")).elapsed(), Duration::from_secs(4000));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
