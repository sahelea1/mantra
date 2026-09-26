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
        }
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
    pub fn builtin() -> Pattern {
        toml::from_str(DEFAULT_PATTERN).expect("built-in pattern must parse")
    }

    /// The default pattern with planner/manager/orchestrator moved to a Claude Code model (`fable51`) and
    /// workers to another (`sonnet5`) — gate roles are left on their Codex models, so this exercises
    /// a genuinely mixed-backend run (`v02plan.md` §10.6). Built by editing `builtin()` in memory
    /// rather than a second embedded TOML, so the two patterns can never silently drift apart on
    /// anything but the models.
    pub fn builtin_claude() -> Pattern {
        let mut p = Pattern::builtin();
        p.name = "mantra-default-claude".into();
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
        if crate::util::slug(name) == "mantra-default" {
            return Ok(Pattern::builtin());
        }
        if crate::util::slug(name) == "mantra-default-claude" {
            return Ok(Pattern::builtin_claude());
        }
        Err(anyhow!("pattern '{name}' not found"))
    }

    pub fn list(project: &Path) -> Vec<String> {
        let mut v = vec!["mantra-default".to_string(), "mantra-default-claude".to_string()];
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

pub const DEFAULT_PATTERN: &str = r#"
name = "mantra-default"
description = "Plan → phased parallel build with gates → heavy QA → security → final verification"

[settings]
isolation = "auto"            # auto | worktree | shared
max_parallel = 4
worker_retries = 2
review_plan = true
orchestrator_context = "fresh" # fresh | compact
stall_minutes = 6
gate_max_rounds = 3
check_timeout_secs = 900
max_tasks_per_phase = 8
watchdog_seconds = 90
watchdog_escalate_seconds = 240
review_minutes = 3             # the orchestrator re-reads every worker's recent work this often (0 = off)
manager_minutes = 5            # the manager gets a run-wide health digest this often (0 = escalations only)

[roles.planner]
kind = "planner"
glyph = "✦"
color = "saffron"
model = "astra"
effort = "max"
sandbox = "read-only"
description = "Understands the goal, explores the repo, writes the phased plan"
instructions = """
You are the Planner of a multi-agent software team run by Mantra.
Explore existing code first so the plan fits it; an empty project needs no exploration.
Design a plan made of sequential PHASES. Inside a phase, every task runs IN PARALLEL on its own
isolated copy of the repo, so tasks in the same phase must not depend on each other and should
touch disjoint files (declare each task's `scope` as path globs). Anything sequential goes into a later phase.
Pick the right worker role per task: small, well-bounded changes → worker-small; broad or tricky work → worker-big.
Every phase ends with a gate: list shell `checks` that must pass (build, tests, lint) and what QA should focus on.
Write prompts for workers that are self-contained: they only see their own prompt, not the whole plan.
Keep the tooling later gates need (virtualenvs, node_modules, build caches) in place until the last phase:
hygiene and cleanup belong in the final phase, never in a phase whose gate still needs them. Gate `checks`
must run as-is on this machine — prefer what already exists over bootstrapping tools.
Also write `orchestrator_brief`: how the orchestrator should supervise this particular project.
"""

[roles.manager]
kind = "manager"
glyph = "◈"
color = "blue"
model = "sol"
effort = "high"
sandbox = "read-only"
description = "Supervises the whole run: keeps every agent moving, unsticks what stalls, fixes course on the fly"
instructions = """
You are the Manager of a multi-agent software team run by Mantra. You never edit code yourself.
The planner designs the plan, an orchestrator runs one phase at a time, workers build in parallel,
gates check and merge. Your job is the whole run: that everyone who should be working is working,
that a stuck, looping or failing agent gets unstuck (a concrete hint, a sharper prompt, a fresh
start, a different effort), and that a halt is resolved by the team instead of waiting for a person.
Mantra wakes you with escalations (a gate or task out of attempts, an agent whose turns keep
failing, an agent the watchdog could not get moving) and with a periodic health digest.
Read the state first (mantra_status, mantra_log on the agent involved, mantra_journal), then make
the smallest intervention that gets the run moving again. Changes to the plan, the tasks or the
gate checks are the planner's to make: hand those up with mantra_ask. Ask the user only when
nobody in the team can decide (credentials, the machine, a trade-off that is theirs).
"""

[roles.orchestrator]
kind = "orchestrator"
glyph = "◉"
color = "violet"
model = "astra"
effort = "high"
sandbox = "read-only"
description = "Runs one phase at a time: spawns workers, watches them, corrects course"
instructions = """
You are the Orchestrator. You never edit code yourself; you run workers through Mantra tools.
Mantra handles the bookkeeping (saving outputs, retrying API errors, merging, gates). You handle judgment:
spawn the phase's tasks (you may sharpen their prompts), and when Mantra wakes you with an event
(worker finished, failed, went out of scope, stalled, asked a question), decide what to do: answer,
accept, steer the worker with mantra_prompt, or respawn it with a better prompt via mantra_retry.
Decisions above your brief go up to the planner (mantra_ask). Keep turns short: act, then call
mantra_wait. Don't poll in loops — Mantra wakes you.
"""

[roles.worker-small]
kind = "worker"
glyph = "◇"
color = "teal"
model = "luna"
effort = "medium"
description = "Fast, focused changes in a small scope"
instructions = "You are a focused implementation worker. Make the smallest correct change that satisfies the task. Run the relevant build/tests for what you touched."

[roles.worker-big]
kind = "worker"
glyph = "◆"
color = "cyan"
model = "sol"
effort = "high"
description = "Larger features and tricky logic"
instructions = "You are a senior implementation worker. Build the feature completely and robustly, with tests. Run the build and tests before finishing."

[roles.qa]
kind = "gate"
glyph = "◎"
color = "green"
model = "terra"
effort = "high"
description = "Phase gate: coherence, integration, tests — fixes what's broken"
instructions = """
You are the QA / Coherence agent guarding a phase gate. The parallel workers' branches were merged
into your working copy. Make the whole thing coherent: consistent naming and interfaces, no duplicated
logic, everything wired together, build and tests green. Fix small problems yourself when your sandbox
allows writes; otherwise report them precisely for a worker.
"""

[roles.qa-heavy]
kind = "gate"
glyph = "◎"
color = "green"
model = "sol"
effort = "xhigh"
description = "Extensive end-of-run QA and test pass"
instructions = "You are the heavy QA agent. Do an extensive end-to-end quality pass: run everything, add missing tests, fix defects and inconsistencies across the whole project."

[roles.security]
kind = "gate"
glyph = "▲"
color = "red"
model = "sol"
effort = "max"
description = "Security sweep and fixes"
instructions = "You are the security agent. Sweep the changes for vulnerabilities (injection, authz/authn gaps, secrets, unsafe deserialization, path traversal, missing validation, dependency risks) and fix them. Keep behaviour intact."

[flow]
planner = "planner"
manager = "manager"
orchestrator = "orchestrator"
phase_gate = "qa"
on_reprompt = "planner"

[[flow.finale]]
role = "qa-heavy"
task = "Run an extensive QA, test and coherence pass over everything built in this run. Fix what you find."

[[flow.finale]]
role = "security"
task = "Do a security sweep of everything built in this run and fix the issues you find."

[[flow.finale]]
role = "planner"
task = "Check the full plan and the original requirements against the actual code. Verify behaviour (run it, and do visual checks if the project has a UI). If something is missing or wrong, spawn workers to fix it, then verify again."
may_spawn = true
"#;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn builtin_is_valid() {
        let p = Pattern::builtin();
        assert!(p.validate().is_ok(), "{:?}", p.validate());
        assert_eq!(p.flow.finale.len(), 3);
        let back = Pattern::from_toml(&p.to_toml()).unwrap();
        assert_eq!(back, p);
    }
    #[test]
    fn manager_role_is_optional_and_validated() {
        let p = Pattern::builtin();
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
        let old = DEFAULT_PATTERN.replace("manager = \"manager\"\n", "").replace("manager_minutes = 5", "");
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
        let old = DEFAULT_PATTERN.replace("watchdog_seconds = 90\nwatchdog_escalate_seconds = 240\n", "");
        assert!(!old.contains("watchdog_seconds"), "the fixture must actually be missing the field");
        let p = Pattern::from_toml(&old).unwrap();
        assert_eq!(p.settings.watchdog_seconds, 90);
        assert_eq!(p.settings.watchdog_escalate_seconds, 240);
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
        // Loadable by name (`Pattern::load`), the same way "mantra-default" always is.
        let loaded = Pattern::load("mantra-default-claude", Path::new("/nonexistent")).unwrap();
        assert_eq!(loaded, p);
        assert!(Pattern::list(Path::new("/nonexistent")).contains(&"mantra-default-claude".to_string()));
    }
}
