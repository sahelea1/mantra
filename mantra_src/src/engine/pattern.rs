//! Patterns: reusable agent workflows (roles + flow) stored as TOML.

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const KINDS: &[&str] = &["planner", "manager", "orchestrator", "worker", "gate"];
pub const COLORS: &[&str] = &["saffron", "violet", "teal", "cyan", "green", "rose", "red", "amber", "blue", "gray"];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Role {
    /// planner | manager | orchestrator | worker | gate
    pub kind: String,
    pub glyph: String,
    pub color: String,
    /// Model alias from models.toml
    pub model: String,
    /// low | medium | high | xhigh | max (whatever the model offers), or `auto`: Mantra picks per
    /// spawn from the role's job (`auto_effort`).
    pub effort: String,
    /// read-only | workspace-write
    pub sandbox: String,
    /// Codex approval policy for agents of this role: "never" (default — never asks; requests
    /// land in the inbox only if a tool forces the question), "on-request", or "untrusted".
    pub permission: String,
    pub description: String,
    pub instructions: String,
    /// Tripwire when an agent of this role uses more tokens than this.
    pub max_tokens: Option<u64>,
    /// Model alias to fall back to when an agent of this role twice sits through
    /// `settings.first_token_minutes` with no output at all (a gateway that accepts the turn and
    /// never answers). Unset: the second time goes to the manager like any other watchdog case.
    pub fallback: Option<String>,
}

impl Default for Role {
    fn default() -> Self {
        Role {
            kind: "worker".into(),
            glyph: "◇".into(),
            color: "teal".into(),
            model: "sol".into(),
            effort: "medium".into(),
            sandbox: "workspace-write".into(),
            permission: "never".into(),
            description: String::new(),
            instructions: String::new(),
            max_tokens: None,
            fallback: None,
        }
    }
}

/// `Role.effort` value that lets Mantra choose the reasoning effort per spawn.
pub const EFFORT_AUTO: &str = "auto";

/// What `effort = "auto"` resolves to for one spawn. Planning, supervising and gating are
/// `medium`; a worker is `low` when its role says it is small (`worker-small`) and `medium`
/// otherwise; a finale step is `high`, whatever its kind. A role that names a concrete effort never
/// comes through here.
pub fn auto_effort(kind: &str, role_name: &str, finale: bool) -> &'static str {
    if finale {
        return "high";
    }
    match kind {
        "worker" if role_name.to_lowercase().contains("small") => "low",
        _ => "medium",
    }
}

/// Values `Role.permission` may take.
pub const PERMISSIONS: &[&str] = &["never", "on-request", "untrusted"];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct PatternSettings {
    /// auto | worktree | shared
    pub isolation: String,
    pub max_parallel: usize,
    pub worker_retries: u32,
    /// Show the plan to the user for approval before building.
    pub review_plan: bool,
    /// fresh (new thread + handoff note per phase) | compact (same thread, compacted)
    pub orchestrator_context: String,
    pub stall_minutes: u64,
    pub gate_max_rounds: u32,
    pub check_timeout_secs: u64,
    pub max_tasks_per_phase: usize,
    /// Watchdog (WP7.2): seconds an expected-active agent may sit idle before the first nudge.
    pub watchdog_seconds: u64,
    /// Watchdog: seconds of continued idleness before the next escalation step (respawn the
    /// orchestrator / wake the orchestrator / wake the planner); the step after that is 2× this.
    pub watchdog_escalate_seconds: u64,
    /// How often (minutes) the orchestrator is shown every running worker's recent work and asked
    /// to check the parallel work stays coherent. 0 turns the periodic review off.
    pub review_minutes: u64,
    /// How often (minutes) the manager (`flow.manager`, if the pattern has one) gets a run-wide
    /// health digest — every agent's state, the journal — and is asked whether anything needs
    /// unsticking. 0 turns the digest off (the manager is then woken by escalations only).
    pub manager_minutes: u64,
    /// Minutes an agent's active turn may produce nothing at all (no tokens, no text — a silent
    /// gateway or an endless "Reconnecting… n/5") before it is restarted once; the second time the
    /// role's `fallback` model takes over, or the manager is asked. 0 turns the deadline off.
    pub first_token_minutes: u64,
    /// Workspace-write agents may use the network (package managers, docs). Codex's sandbox blocks it unless this is on.
    #[serde(default = "default_true")]
    pub network: bool,
    /// Extra directories every workspace-write agent may write (`~` expands to the home dir), on top of the toolchain caches Mantra adds automatically.
    pub writable_roots: Vec<String>,
}

fn default_true() -> bool {
    true
}

impl Default for PatternSettings {
    fn default() -> Self {
        PatternSettings {
            isolation: "auto".into(),
            max_parallel: 4,
            worker_retries: 2,
            review_plan: true,
            orchestrator_context: "fresh".into(),
            stall_minutes: 6,
            gate_max_rounds: 3,
            check_timeout_secs: 900,
            max_tasks_per_phase: 8,
            watchdog_seconds: 90,
            watchdog_escalate_seconds: 240,
            review_minutes: 3,
            manager_minutes: 5,
            network: true,
            writable_roots: vec![],
            first_token_minutes: 4,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct FinaleStep {
    pub role: String,
    pub task: String,
    /// Allow this step to spawn ad-hoc workers.
    pub may_spawn: bool,
}

impl Default for FinaleStep {
    fn default() -> Self {
        FinaleStep { role: String::new(), task: String::new(), may_spawn: false }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Flow {
    pub planner: String,
    /// The run-wide supervisor (kind = "manager"), or "" for none. It outlives phases: halts the
    /// run cannot sort out itself and agents the watchdog cannot get moving go to it first, and it
    /// gets a periodic health digest (`settings.manager_minutes`). It acts through the same tools
    /// the orchestrator and the user have — prompt, respawn, retry, resume — and hands plan
    /// changes up to the planner.
    pub manager: String,
    pub orchestrator: String,
    pub phase_gate: String,
    pub on_reprompt: String,
    pub finale: Vec<FinaleStep>,
}

impl Default for Flow {
    fn default() -> Self {
        Flow { planner: "planner".into(), manager: String::new(), orchestrator: "orchestrator".into(), phase_gate: "qa".into(), on_reprompt: "planner".into(), finale: vec![] }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Pattern {
    pub name: String,
    pub description: String,
    pub settings: PatternSettings,
    pub roles: BTreeMap<String, Role>,
    pub flow: Flow,
    /// Directory `load()` actually found this pattern in (project-local or `$MANTRA_HOME`) — not
    /// part of the file itself. `save()` writes back here so a project-local pattern stays the
    /// file `load()` will read next time, instead of always landing in `$MANTRA_HOME`.
    #[serde(skip)]
    pub source_dir: Option<PathBuf>,
}

impl Default for Pattern {
    fn default() -> Self {
        Pattern { name: String::new(), description: String::new(), settings: PatternSettings::default(), roles: BTreeMap::new(), flow: Flow::default(), source_dir: None }
    }
}

impl Pattern {
    /// The balanced built-in: plan → phases, each closed by a fresh read-only QA gate with
    /// automatic fix workers → one QA pass over the run's diff → planner verification.
    pub fn builtin() -> Pattern {
        builtin_default()
    }

    /// For small and medium tasks: no manager, one fresh QA read per gate, planner verification only.
    pub fn builtin_lean() -> Pattern {
        builtin_lean()
    }

    /// For releases and security-sensitive work: heavy QA, a security sweep and planner
    /// verification at the end, a manager on watch, a coherence review every few minutes.
    pub fn builtin_thorough() -> Pattern {
        builtin_thorough()
    }

    /// The names of the patterns that ship inside the binary (`load` finds them without a file).
    pub fn names() -> Vec<&'static str> {
        vec![NAME_DEFAULT, NAME_LEAN, NAME_THOROUGH, NAME_CLAUDE]
    }

    /// A built-in pattern by name (slug-insensitive), if `name` is one of `names()`.
    pub fn builtin_named(name: &str) -> Option<Pattern> {
        match crate::util::slug(name).as_str() {
            NAME_DEFAULT => Some(Pattern::builtin()),
            NAME_LEAN => Some(Pattern::builtin_lean()),
            NAME_THOROUGH => Some(Pattern::builtin_thorough()),
            NAME_CLAUDE => Some(Pattern::builtin_claude()),
            _ => None,
        }
    }

    /// The default pattern with planner/manager/orchestrator moved to a Claude Code model (`fable51`) and
    /// workers to another (`sonnet5`) — gate roles are left on their Codex models, so this exercises
    /// a genuinely mixed-backend run (`v02plan.md` §10.6). Built by editing `builtin()` in memory
    /// rather than a second definition, so the two patterns can never silently drift apart on
    /// anything but the models.
    pub fn builtin_claude() -> Pattern {
        let mut p = Pattern::builtin();
        p.name = NAME_CLAUDE.into();
        p.description = format!("{} — planner/orchestrator on Claude Code (fable51), workers on Claude Code (sonnet5)", p.description);
        for role in p.roles.values_mut() {
            match role.kind.as_str() {
                "planner" | "manager" | "orchestrator" => role.model = "fable51".into(),
                "worker" => role.model = "sonnet5".into(),
                _ => {}
            }
        }
        p
    }

    pub fn from_toml(s: &str) -> Result<Pattern> {
        let p: Pattern = toml::from_str(s).map_err(|e| anyhow!("{e}"))?;
        p.validate().map_err(|errs| anyhow!(errs.join("; ")))?;
        Ok(p)
    }

    pub fn to_toml(&self) -> String {
        toml::to_string_pretty(self).unwrap_or_default()
    }

    pub fn role(&self, name: &str) -> Option<&Role> {
        self.roles.get(name)
    }

    /// The manager role's name, when the flow names one that exists (`""` in `flow.manager`
    /// means the pattern runs without a manager — the planner is then the top of the chain).
    pub fn manager_role(&self) -> Option<&str> {
        let m = self.flow.manager.trim();
        (!m.is_empty() && self.roles.get(m).map(|r| r.kind == "manager").unwrap_or(false)).then_some(m)
    }

    pub fn worker_roles(&self) -> Vec<String> {
        self.roles.iter().filter(|(_, r)| r.kind == "worker").map(|(n, _)| n.clone()).collect()
    }

    /// Roles in a stable, meaningful display order.
    pub fn ordered_roles(&self) -> Vec<(String, Role)> {
        let rank = |k: &str| KINDS.iter().position(|x| *x == k).unwrap_or(9);
        let mut v: Vec<(String, Role)> = self.roles.iter().map(|(k, r)| (k.clone(), r.clone())).collect();
        v.sort_by(|a, b| rank(&a.1.kind).cmp(&rank(&b.1.kind)).then(a.0.cmp(&b.0)));
        v
    }

    pub fn validate(&self) -> std::result::Result<(), Vec<String>> {
        let mut errs = vec![];
        if self.name.trim().is_empty() {
            errs.push("pattern needs a name".into());
        }
        for (n, r) in &self.roles {
            if !KINDS.contains(&r.kind.as_str()) {
                errs.push(format!("role '{n}': kind must be one of {}", KINDS.join(", ")));
            }
            if crate::util::width(&r.glyph) != 1 {
                errs.push(format!("role '{n}': glyph must be exactly one narrow character"));
            }
            if !COLORS.contains(&r.color.as_str()) {
                errs.push(format!("role '{n}': color must be one of {}", COLORS.join(", ")));
            }
            if r.model.trim().is_empty() {
                errs.push(format!("role '{n}': model is empty"));
            }
            if !PERMISSIONS.contains(&r.permission.as_str()) {
                errs.push(format!("role '{n}': permission must be one of {} (got '{}')", PERMISSIONS.join(", "), r.permission));
            }
        }
        let need = |role: &str, kind: &str, what: &str, errs: &mut Vec<String>| match self.roles.get(role) {
            None => errs.push(format!("flow.{what} = '{role}' is not a defined role")),
            Some(r) if r.kind != kind => errs.push(format!("flow.{what} role '{role}' must have kind = \"{kind}\"")),
            _ => {}
        };
        need(&self.flow.planner, "planner", "planner", &mut errs);
        if !self.flow.manager.trim().is_empty() {
            need(&self.flow.manager, "manager", "manager", &mut errs);
        }
        need(&self.flow.orchestrator, "orchestrator", "orchestrator", &mut errs);
        need(&self.flow.phase_gate, "gate", "phase_gate", &mut errs);
        if !self.roles.contains_key(&self.flow.on_reprompt) {
            errs.push(format!("flow.on_reprompt = '{}' is not a defined role", self.flow.on_reprompt));
        }
        for (i, s) in self.flow.finale.iter().enumerate() {
            if !self.roles.contains_key(&s.role) {
                errs.push(format!("flow.finale[{i}] role '{}' is not defined", s.role));
            }
        }
        if self.worker_roles().is_empty() {
            errs.push("pattern needs at least one role with kind = \"worker\"".into());
        }
        if self.settings.max_parallel == 0 {
            errs.push("settings.max_parallel must be ≥ 1".into());
        }
        if self.settings.watchdog_escalate_seconds <= self.settings.watchdog_seconds {
            errs.push("settings.watchdog_escalate_seconds must be greater than watchdog_seconds".into());
        }
        if errs.is_empty() {
            Ok(())
        } else {
            Err(errs)
        }
    }

    pub fn path_for(name: &str) -> PathBuf {
        crate::config::patterns_dir().join(format!("{}.toml", crate::util::slug(name)))
    }

    pub fn save(&self) -> Result<PathBuf> {
        let dir = self.source_dir.clone().unwrap_or_else(crate::config::patterns_dir);
        let p = dir.join(format!("{}.toml", crate::util::slug(&self.name)));
        crate::config::atomic_write(&p, &format!("# Mantra pattern — edit here or in the Studio (ctrl+o → s)\n{}", self.to_toml()))?;
        Ok(p)
    }

    /// Load by name: `<project>/.mantra/patterns/` if a repo ships one (Mantra never creates it), then `~/.mantra/patterns/`, then built-in.
    pub fn load(name: &str, project: &Path) -> Result<Pattern> {
        let file = format!("{}.toml", crate::util::slug(name));
        for dir in [project.join(".mantra").join("patterns"), crate::config::patterns_dir()] {
            let p = dir.join(&file);
            if let Ok(s) = std::fs::read_to_string(&p) {
                let mut pat = Pattern::from_toml(&s).map_err(|e| anyhow!("{}: {e}", p.display()))?;
                pat.source_dir = Some(dir);
                return Ok(pat);
            }
        }
        if let Some(p) = Pattern::builtin_named(name) {
            return Ok(p);
        }
        Err(anyhow!("pattern '{name}' not found"))
    }

    pub fn list(project: &Path) -> Vec<String> {
        let mut v: Vec<String> = Pattern::names().into_iter().map(String::from).collect();
        for dir in [project.join(".mantra").join("patterns"), crate::config::patterns_dir()] {
            if let Ok(rd) = std::fs::read_dir(dir) {
                for e in rd.flatten() {
                    let p = e.path();
                    if p.extension().map(|x| x == "toml").unwrap_or(false) {
                        if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                            if !v.iter().any(|x| x == stem) {
                                v.push(stem.to_string());
                            }
                        }
                    }
                }
            }
        }
        v
    }
}

// ───────────────────────────── built-in patterns ─────────────────────────────
//
// Three patterns ship in the binary; `mantra-default-claude` is `mantra-default` with models moved.
// The roles they share (planner, manager, orchestrator, workers, the phase QA) are defined once.

pub const NAME_DEFAULT: &str = "mantra-default";
pub const NAME_LEAN: &str = "mantra-lean";
pub const NAME_THOROUGH: &str = "mantra-thorough";
pub const NAME_CLAUDE: &str = "mantra-default-claude";

/// The budget rule every built-in role ends with.
const BUDGET: &str = "Do not re-read files you already read. Do not run the full test suite more than once per turn unless it failed.";

const PLANNER_INSTRUCTIONS: &str = "You are the Planner of a multi-agent software team run by Mantra. You own the plan, never the code.
Look at the existing code only as far as the plan needs (an empty project needs no exploration).
Split the work into sequential PHASES. Tasks inside a phase run in parallel on isolated copies of the repo, so they must not depend on each other and must own disjoint files: declare each task's `scope` as path globs. Anything sequential goes into a later phase.
Size the plan to the goal: a small goal is one or two phases and a few tasks, submitted in your first turn. Small, well-bounded changes go to worker-small, broad or tricky work to worker-big.
Every phase ends with a gate: shell `checks` that run as-is on this machine (build, tests, lint; prefer what already exists) and a line on what QA should focus on. Keep the tooling later gates need (virtualenvs, node_modules, build caches) until the last phase; cleanup belongs there.
Write each worker prompt so it stands alone (workers see only their own prompt): the goal, the files, the interfaces to match, how to verify. Add `orchestrator_brief`: what to watch in this project, in a few lines.
Stop as soon as the plan is submitted.";

const MANAGER_INSTRUCTIONS: &str = "You are the Manager of a multi-agent software team run by Mantra. You supervise; you never edit code, run the project or write the plan.
Mantra wakes you with escalations (a gate or task out of attempts, a turn that keeps failing, an agent the watchdog could not move) and with a periodic health digest. Read the state first (mantra_status, mantra_journal, mantra_log on the agent involved), then make the smallest intervention that gets the run moving: a concrete hint, a sharper prompt, a fresh start, a different effort.
Changes to the plan, the tasks or the gate checks are the planner's: hand them up with mantra_ask. Ask the user only for what nobody on the team can decide (credentials, the machine, a trade-off that is theirs).
A healthy digest needs no action: mantra_wait. One intervention per agent per wake, then stop.";

const ORCHESTRATOR_INSTRUCTIONS: &str = "You are the Orchestrator of one phase. You run workers through Mantra tools; you never edit code, run tests or review diffs yourself.
Spawn the phase's tasks (sharpen a prompt only when it is vague or wrong), then mantra_wait. Mantra does the bookkeeping: saving output, retries, merging, gates and fix workers.
Act only on events: a worker finished, failed, left its scope, stalled or asked a question. Answer it, steer with mantra_prompt, or respawn with mantra_retry and a better prompt. Anything beyond your brief goes up with mantra_ask.
Silence is the normal answer: at most one message per event, none to a worker that is done. Keep turns short: act, then mantra_wait.";

const WORKER_SMALL_INSTRUCTIONS: &str = "You are a focused implementation worker. Do exactly your task inside its scope with the smallest correct change: no refactors, no extras.
Read only what the task needs. Before finishing, run only the build, tests or lint that cover your scope, once; fix what you broke and stop. Do not run the whole project's suite unless your scope is the whole project.
Report what changed and how you verified it.";

const WORKER_BIG_INSTRUCTIONS: &str = "You are a senior implementation worker. Build your task completely and robustly inside its scope, with tests for the behaviour you add.
Read only what the task needs. Before finishing, run the build and the tests that cover your scope, once; fix what you broke and stop. Do not run the whole project's suite unless your scope is the whole project.
Report what changed and how you verified it.";

const QA_INSTRUCTIONS: &str = "You are the QA agent of a phase gate. You verify and report; you never fix. Your role is read-only: do not edit, create or delete files.
Your prompt has the gate check results, the phase's diff and the previous round's report. Start from the diff and read a file in full only when the diff cannot answer a question. Look for what the checks cannot see: parallel changes that do not fit together (naming, interfaces, duplicated logic, missing wiring), new behaviour without tests, anything that misses the phase goal. Do not re-run checks Mantra already ran green.
Report once with mantra_gate_report. pass=true only when the goal is met and the checks are green. On fail list every finding as: file path, what is wrong, what to change, precise enough that a worker needs nothing else; set big=true only when the fixes span several files or need design work. Mantra sends your findings to a fix worker and starts a fresh QA round: you do not continue, so put everything in the report.";

const QA_HEAVY_INSTRUCTIONS: &str = "You are the final QA agent. You verify; you do not fix. Start from the run's diff and the gate reports in your prompt, not from the whole repo; read a file in full only when the diff cannot answer a question. Run the project's full test suite once.
Checklist: the diff meets the original requirements; new behaviour has tests; the phases fit together (naming, interfaces, dead or duplicated code); errors and edge cases at the boundaries; security basics (input validation and injection, path handling, authn/authz where it applies, secrets in code or logs, unsafe deserialization, new dependencies).
For each real defect spawn a fix worker with mantra_spawn_adhoc (the file, the exact change, a scope), then mantra_wait; when they finish re-check only what they touched. Skip nits and anything you cannot tie to a concrete line. Finish with mantra_gate_report: pass=true when nothing real is left.";

const QA_HEAVY_THOROUGH_INSTRUCTIONS: &str = "You are the heavy QA agent. Start from the run's diff and the gate reports in your prompt, not from the whole repo; read a file in full only when the diff cannot answer a question. Run the project's full test suite once.
Check the diff against the original requirements, then hunt defects: missing or weak tests, edge cases and error paths, inconsistencies between phases, dead or duplicated code. Fix what you find with the smallest change and add the missing tests; re-run only what you touched.
Skip nits. Finish with mantra_gate_report.";

const SECURITY_INSTRUCTIONS: &str = "You are the security agent. Review the run's diff and the gate reports in your prompt, not the whole repo, for vulnerabilities: injection, authn/authz gaps, secrets in code or logs, unsafe deserialization, path traversal, missing input validation, risky new dependencies.
Fix each real issue with the smallest change that keeps behaviour intact, and add a test for it. Skip style points and hypotheticals you cannot tie to a concrete path in the diff. Run the affected tests once at the end, then report with mantra_gate_report.";

const PLANNER_FINALE_TASK: &str = "Verify the original requirements against the run's diff and the gate and finale reports in this prompt. Read a file only to settle a doubt; run the plan's final checks once. If a requirement is really missing or broken, spawn a fix worker with mantra_spawn_adhoc (a precise prompt, a scope), mantra_wait, and re-verify only that. Spawn nothing for nits. Report with mantra_gate_report.";

const PLANNER_FINALE_TASK_THOROUGH: &str = "Check the full plan and the original requirements against the actual code, using the run's diff and the gate and finale reports in this prompt as the starting point. Verify behaviour: run it, and do visual checks if the project has a UI. If something is missing or wrong, spawn fix workers with mantra_spawn_adhoc, then verify that again. Report with mantra_gate_report.";

const QA_FINALE_TASK: &str = "Run the final QA pass over everything built in this run: start from the diff and the gate reports, verify, and have real defects fixed.";
const SECURITY_FINALE_TASK: &str = "Do a security sweep of everything built in this run, starting from the diff, and fix the issues you find.";

fn with_budget(instructions: &str) -> String {
    format!("{instructions}\n{BUDGET}")
}

fn planner_role() -> Role {
    Role {
        kind: "planner".into(),
        glyph: "✦".into(),
        color: "saffron".into(),
        model: "astra".into(),
        effort: EFFORT_AUTO.into(),
        sandbox: "read-only".into(),
        description: "Understands the goal, explores only what it needs, writes the phased plan".into(),
        instructions: with_budget(PLANNER_INSTRUCTIONS),
        ..Role::default()
    }
}

fn manager_role() -> Role {
    Role {
        kind: "manager".into(),
        glyph: "◈".into(),
        color: "blue".into(),
        model: "sol".into(),
        effort: EFFORT_AUTO.into(),
        sandbox: "read-only".into(),
        description: "Supervises the whole run: keeps every agent moving, unsticks what stalls".into(),
        instructions: with_budget(MANAGER_INSTRUCTIONS),
        ..Role::default()
    }
}

fn orchestrator_role() -> Role {
    Role {
        kind: "orchestrator".into(),
        glyph: "◉".into(),
        color: "violet".into(),
        model: "astra".into(),
        effort: EFFORT_AUTO.into(),
        sandbox: "read-only".into(),
        description: "Runs one phase at a time: spawns workers, watches them, corrects course".into(),
        instructions: with_budget(ORCHESTRATOR_INSTRUCTIONS),
        ..Role::default()
    }
}

fn worker_small_role() -> Role {
    Role {
        kind: "worker".into(),
        glyph: "◇".into(),
        color: "teal".into(),
        model: "luna".into(),
        effort: EFFORT_AUTO.into(),
        description: "Fast, focused changes in a small scope".into(),
        instructions: with_budget(WORKER_SMALL_INSTRUCTIONS),
        ..Role::default()
    }
}

fn worker_big_role() -> Role {
    Role {
        kind: "worker".into(),
        glyph: "◆".into(),
        color: "cyan".into(),
        model: "sol".into(),
        effort: EFFORT_AUTO.into(),
        description: "Larger features and tricky logic".into(),
        instructions: with_budget(WORKER_BIG_INSTRUCTIONS),
        ..Role::default()
    }
}

/// The phase gate: read-only, a fresh agent every round. It verifies and reports; fixes go to workers.
fn qa_role() -> Role {
    Role {
        kind: "gate".into(),
        glyph: "◎".into(),
        color: "green".into(),
        model: "terra".into(),
        effort: EFFORT_AUTO.into(),
        sandbox: "read-only".into(),
        description: "Phase gate (fresh each round, read-only): verifies the merged phase against its diff and reports findings".into(),
        instructions: with_budget(QA_INSTRUCTIONS),
        ..Role::default()
    }
}

/// A pattern with the roles every built-in shares and no manager, no finale yet.
fn base_pattern(name: &str, description: &str) -> Pattern {
    let mut p = Pattern { name: name.into(), description: description.into(), ..Pattern::default() };
    for (n, r) in [("planner", planner_role()), ("orchestrator", orchestrator_role()), ("worker-small", worker_small_role()), ("worker-big", worker_big_role()), ("qa", qa_role())] {
        p.roles.insert(n.into(), r);
    }
    p.flow = Flow { planner: "planner".into(), manager: String::new(), orchestrator: "orchestrator".into(), phase_gate: "qa".into(), on_reprompt: "planner".into(), finale: vec![] };
    p
}

fn finale(role: &str, task: &str, may_spawn: bool) -> FinaleStep {
    FinaleStep { role: role.into(), task: task.into(), may_spawn }
}

fn builtin_default() -> Pattern {
    let mut p = base_pattern(NAME_DEFAULT, "Balanced: plan → phases with a fresh read-only QA gate and automatic fix workers → one QA pass over the diff → planner verification");
    p.settings.review_minutes = 5;
    p.settings.manager_minutes = 10;
    p.roles.insert("manager".into(), manager_role());
    p.roles.insert(
        "qa-heavy".into(),
        Role {
            kind: "gate".into(),
            glyph: "◎".into(),
            color: "green".into(),
            model: "sol".into(),
            effort: EFFORT_AUTO.into(),
            description: "End-of-run QA on the run's diff, security checklist included; has defects fixed by workers".into(),
            instructions: with_budget(QA_HEAVY_INSTRUCTIONS),
            ..Role::default()
        },
    );
    p.flow.manager = "manager".into();
    p.flow.finale = vec![finale("qa-heavy", QA_FINALE_TASK, true), finale("planner", PLANNER_FINALE_TASK, true)];
    p
}

fn builtin_lean() -> Pattern {
    let mut p = base_pattern(NAME_LEAN, "Small and medium tasks: plan → phases with checks and one fresh QA read of the diff → planner verification. No manager");
    p.settings.max_parallel = 3;
    p.settings.gate_max_rounds = 2;
    p.settings.review_minutes = 0;
    p.flow.finale = vec![finale("planner", PLANNER_FINALE_TASK, true)];
    p
}

fn builtin_thorough() -> Pattern {
    let mut p = base_pattern(NAME_THOROUGH, "Releases and security-sensitive work: plan → phased build with gates → heavy QA → security sweep → final verification, a manager on watch");
    p.settings.review_minutes = 3;
    p.settings.manager_minutes = 5;
    p.roles.insert("manager".into(), manager_role());
    p.roles.insert(
        "qa-heavy".into(),
        Role {
            kind: "gate".into(),
            glyph: "◎".into(),
            color: "green".into(),
            model: "sol".into(),
            effort: "xhigh".into(),
            description: "Extensive end-of-run QA and test pass".into(),
            instructions: with_budget(QA_HEAVY_THOROUGH_INSTRUCTIONS),
            ..Role::default()
        },
    );
    p.roles.insert(
        "security".into(),
        Role {
            kind: "gate".into(),
            glyph: "▲".into(),
            color: "red".into(),
            model: "sol".into(),
            effort: "max".into(),
            description: "Security sweep and fixes".into(),
            instructions: with_budget(SECURITY_INSTRUCTIONS),
            ..Role::default()
        },
    );
    // The heavy flow spends tokens on purpose: efforts are explicit, not "auto".
    for (name, effort) in [("planner", "max"), ("manager", "high"), ("orchestrator", "high"), ("worker-small", "medium"), ("worker-big", "high"), ("qa", "high")] {
        if let Some(r) = p.roles.get_mut(name) {
            r.effort = effort.into();
        }
    }
    p.flow.manager = "manager".into();
    p.flow.finale = vec![finale("qa-heavy", QA_FINALE_TASK, false), finale("security", SECURITY_FINALE_TASK, false), finale("planner", PLANNER_FINALE_TASK_THOROUGH, true)];
    p
}


#[cfg(test)]
mod tests {
    use super::*;

    fn all_builtins() -> Vec<Pattern> {
        Pattern::names().into_iter().map(|n| Pattern::builtin_named(n).unwrap()).collect()
    }

    #[test]
    fn builtin_is_valid() {
        for p in all_builtins() {
            assert!(p.validate().is_ok(), "{}: {:?}", p.name, p.validate());
            let back = Pattern::from_toml(&p.to_toml()).unwrap();
            assert_eq!(back, p, "{} must survive a TOML round trip", p.name);
        }
        let p = Pattern::builtin();
        assert_eq!(p.name, "mantra-default");
        assert_eq!(p.flow.finale.len(), 2, "one qa-heavy pass, then the planner's verification");
    }

    #[test]
    fn names_list_every_builtin_and_each_loads_by_name() {
        let names = Pattern::names();
        for n in ["mantra-default", "mantra-lean", "mantra-thorough", "mantra-default-claude"] {
            assert!(names.contains(&n), "{n}");
            let p = Pattern::load(n, Path::new("/nonexistent")).unwrap_or_else(|e| panic!("{n}: {e}"));
            assert_eq!(p.name, n);
            assert!(Pattern::list(Path::new("/nonexistent")).contains(&n.to_string()));
        }
        assert!(Pattern::load("mantra-nope", Path::new("/nonexistent")).is_err());
        // slug-insensitive, like a file lookup
        assert_eq!(Pattern::builtin_named("Mantra Lean").map(|p| p.name), Some("mantra-lean".to_string()));
    }

    #[test]
    fn every_builtin_role_uses_a_default_registry_model_and_a_usable_effort() {
        let reg = crate::config::Registry::defaults();
        for p in all_builtins().into_iter().filter(|p| p.name != NAME_CLAUDE) {
            for (n, r) in &p.roles {
                assert!(reg.get(&r.model).is_some(), "{}: role '{n}' uses unknown model alias '{}'", p.name, r.model);
                if r.effort != EFFORT_AUTO {
                    assert!(reg.resolve(&r.model).efforts().contains(&r.effort), "{}: role '{n}' effort '{}' is not offered by {}", p.name, r.effort, r.model);
                }
            }
        }
    }

    #[test]
    fn default_pattern_shape() {
        let p = Pattern::builtin();
        assert_eq!(p.manager_role(), Some("manager"));
        assert_eq!((p.settings.manager_minutes, p.settings.review_minutes), (10, 5));
        let steps: Vec<&str> = p.flow.finale.iter().map(|s| s.role.as_str()).collect();
        assert_eq!(steps, vec!["qa-heavy", "planner"]);
        assert!(!p.roles.contains_key("security"), "security is a checklist inside qa-heavy, not a step");
        assert!(p.roles["qa-heavy"].instructions.to_lowercase().contains("security"), "the checklist lives in qa-heavy's prompt");
        assert!(p.flow.finale.iter().all(|s| s.may_spawn), "defects found at the end go to fix workers");
    }

    #[test]
    fn lean_pattern_shape() {
        let p = Pattern::builtin_lean();
        assert_eq!(p.manager_role(), None, "no manager");
        assert!(p.flow.manager.is_empty());
        assert!(!p.roles.values().any(|r| r.kind == "manager"));
        assert_eq!(p.settings.max_parallel, 3);
        assert_eq!(p.flow.finale.len(), 1);
        assert_eq!(p.flow.finale[0].role, "planner", "planner verification only");
        assert_eq!(p.roles["qa"].sandbox, "read-only");
        assert!(p.validate().is_ok());
    }

    #[test]
    fn thorough_pattern_is_the_heavy_flow() {
        let p = Pattern::builtin_thorough();
        assert_eq!(p.manager_role(), Some("manager"));
        assert_eq!((p.settings.manager_minutes, p.settings.review_minutes), (5, 3));
        let steps: Vec<&str> = p.flow.finale.iter().map(|s| s.role.as_str()).collect();
        assert_eq!(steps, vec!["qa-heavy", "security", "planner"]);
        assert_eq!(p.roles["planner"].effort, "max");
        assert_eq!(p.roles["qa-heavy"].effort, "xhigh");
        assert_eq!(p.roles["security"].effort, "max");
        assert!(p.roles.values().all(|r| r.effort != EFFORT_AUTO), "the heavy flow names its efforts");
    }

    #[test]
    fn manager_role_is_optional_and_validated() {
        let p = Pattern::builtin_thorough();
        assert_eq!(p.manager_role(), Some("manager"));
        assert_eq!(p.roles["manager"].kind, "manager");
        assert!(p.roles["manager"].sandbox == "read-only", "the manager never edits code");
        assert_eq!(p.settings.manager_minutes, 5);
        // no manager at all is fine
        let mut none = Pattern::builtin();
        none.flow.manager = String::new();
        assert!(none.validate().is_ok());
        assert_eq!(none.manager_role(), None);
        // an old pattern file written before the manager existed loads without one
        let old = p.to_toml().replace("manager = \"manager\"\n", "").replace("manager_minutes = 5", "");
        let old_p = Pattern::from_toml(&old).unwrap();
        assert_eq!(old_p.manager_role(), None);
        assert_eq!(old_p.settings.manager_minutes, 5, "the default applies");
        // flow.manager must name a role of kind manager
        let mut bad = Pattern::builtin();
        bad.flow.manager = "planner".into();
        let errs = bad.validate().unwrap_err();
        assert!(errs.iter().any(|e| e.contains("flow.manager") && e.contains("manager")), "{errs:?}");
        assert_eq!(bad.manager_role(), None);
        let mut missing = Pattern::builtin();
        missing.flow.manager = "nope".into();
        assert!(missing.validate().is_err());
        // the ordered roles put the manager between planner and orchestrator
        let kinds: Vec<String> = p.ordered_roles().into_iter().map(|(_, r)| r.kind).collect();
        assert_eq!(kinds[0], "planner");
        assert_eq!(kinds[1], "manager");
        assert_eq!(kinds[2], "orchestrator");
    }

    #[test]
    fn catches_errors() {
        let mut p = Pattern::builtin();
        p.flow.phase_gate = "nope".into();
        assert!(p.validate().is_err());
    }
    #[test]
    fn watchdog_settings_default_for_old_patterns() {
        // An old pattern TOML saved before WP7 has no watchdog_* keys at all — it must still load,
        // with the built-in defaults filled in by serde (the `#[serde(default)]` on the struct).
        let old = Pattern::builtin().to_toml().replace("watchdog_seconds = 90\nwatchdog_escalate_seconds = 240\n", "");
        assert!(!old.contains("watchdog_seconds"), "the fixture must actually be missing the field");
        let p = Pattern::from_toml(&old).unwrap();
        assert_eq!(p.settings.watchdog_seconds, 90);
        assert_eq!(p.settings.watchdog_escalate_seconds, 240);
    }

    #[test]
    fn first_token_deadline_and_fallback_default_for_old_patterns() {
        let toml = Pattern::builtin().to_toml();
        let old = toml.replace("first_token_minutes = 4", "");
        assert!(!old.contains("first_token_minutes"), "the fixture must actually be missing the field");
        let p = Pattern::from_toml(&old).unwrap();
        assert_eq!(p.settings.first_token_minutes, 4, "the default applies");
        assert!(p.roles.values().all(|r| r.fallback.is_none()), "no role has a fallback unless it names one");
        // a fallback round-trips through the TOML the Studio writes
        let mut with = Pattern::builtin();
        with.roles.get_mut("planner").unwrap().fallback = Some("sol".into());
        let back = Pattern::from_toml(&with.to_toml()).unwrap();
        assert_eq!(back.roles["planner"].fallback.as_deref(), Some("sol"));
        let zero = toml.replace("first_token_minutes = 4", "first_token_minutes = 0");
        assert_eq!(Pattern::from_toml(&zero).unwrap().settings.first_token_minutes, 0, "0 = off");
    }

    #[test]
    fn permission_defaults_off_and_validates() {
        let p = Pattern::builtin();
        for (n, r) in &p.roles {
            assert_eq!(r.permission, "never", "role '{n}' should default to permission = never");
        }
        let mut bad = Pattern::builtin();
        let name = bad.worker_roles()[0].clone();
        bad.roles.get_mut(&name).unwrap().permission = "sometimes".into();
        let errs = bad.validate().unwrap_err();
        assert!(errs.iter().any(|e| e.contains("permission") && e.contains("sometimes")), "{errs:?}");
    }

    #[test]
    fn builtin_claude_is_valid_and_moves_only_planner_orchestrator_worker_models() {
        let p = Pattern::builtin_claude();
        assert!(p.validate().is_ok(), "{:?}", p.validate());
        assert_eq!(p.name, "mantra-default-claude");
        for (name, role) in &p.roles {
            match role.kind.as_str() {
                "planner" | "manager" | "orchestrator" => assert_eq!(role.model, "fable51", "role '{name}'"),
                "worker" => assert_eq!(role.model, "sonnet5", "role '{name}'"),
                "gate" => assert_ne!(role.model, "fable51", "gate role '{name}' must keep its Codex model — only planner/orchestrator/worker move"),
                _ => {}
            }
        }
        // It is the new default with models moved, nothing else.
        let d = Pattern::builtin();
        assert_eq!(p.flow, d.flow);
        assert_eq!(p.settings, d.settings);
        assert_eq!(p.roles.keys().collect::<Vec<_>>(), d.roles.keys().collect::<Vec<_>>());
        for (n, r) in &p.roles {
            assert_eq!(r.instructions, d.roles[n].instructions, "role '{n}'");
            assert_eq!(r.effort, d.roles[n].effort, "role '{n}'");
        }
        // Loadable by name (`Pattern::load`), the same way "mantra-default" always is.
        let loaded = Pattern::load("mantra-default-claude", Path::new("/nonexistent")).unwrap();
        assert_eq!(loaded, p);
        assert!(Pattern::list(Path::new("/nonexistent")).contains(&"mantra-default-claude".to_string()));
    }

    /// Lean prompts: short, budget-aware, and the gate role is told never to fix.
    #[test]
    fn built_in_prompts_are_short_and_budget_aware() {
        for p in all_builtins() {
            for (n, r) in &p.roles {
                assert!(r.instructions.contains("Do not re-read files you already read"), "{}: role '{n}'", p.name);
                assert!(r.instructions.contains("full test suite more than once per turn unless it failed"), "{}: role '{n}'", p.name);
                assert!(r.instructions.chars().count() < 2000, "{}: role '{n}' is {} chars — keep it lean", p.name, r.instructions.chars().count());
            }
            let qa = &p.roles[&p.flow.phase_gate];
            assert_eq!(qa.sandbox, "read-only", "{}: the phase gate never writes", p.name);
            assert!(qa.instructions.contains("never fix"), "{}", p.name);
        }
        let d = Pattern::builtin();
        assert!(d.roles["worker-small"].instructions.contains("only the build, tests or lint that cover your scope"));
        assert!(d.roles["qa-heavy"].instructions.contains("run's diff") && d.roles["qa-heavy"].instructions.contains("gate reports"));
        assert!(Pattern::builtin_thorough().roles["security"].instructions.contains("run's diff"));
        assert!(d.flow.finale[1].task.contains("gate and finale reports"), "the planner verifies against the diff and the reports");
        assert!(d.flow.finale[1].task.contains("only") || d.flow.finale[1].task.contains("Spawn nothing for nits"));
    }

    #[test]
    fn auto_effort_follows_the_job() {
        assert_eq!(auto_effort("planner", "planner", false), "medium");
        assert_eq!(auto_effort("manager", "manager", false), "medium");
        assert_eq!(auto_effort("orchestrator", "orchestrator", false), "medium");
        assert_eq!(auto_effort("worker", "worker-big", false), "medium");
        assert_eq!(auto_effort("worker", "worker-small", false), "low");
        assert_eq!(auto_effort("gate", "qa", false), "medium");
        for kind in ["planner", "gate", "worker"] {
            assert_eq!(auto_effort(kind, "qa-heavy", true), "high", "finale roles think hard");
        }
    }

    #[test]
    fn auto_is_a_valid_role_effort_and_round_trips() {
        let mut p = Pattern::builtin_lean();
        p.roles.get_mut("worker-big").unwrap().effort = "auto".into();
        assert!(p.validate().is_ok());
        let back = Pattern::from_toml(&p.to_toml()).unwrap();
        assert_eq!(back.roles["worker-big"].effort, "auto");
    }
}
