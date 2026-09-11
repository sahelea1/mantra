//! Patterns: reusable agent workflows (roles + flow) stored as TOML.

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const KINDS: &[&str] = &["planner", "orchestrator", "worker", "gate"];
pub const COLORS: &[&str] = &["saffron", "violet", "teal", "cyan", "green", "rose", "red", "amber", "blue", "gray"];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Role {
    /// planner | orchestrator | worker | gate
    pub kind: String,
    pub glyph: String,
    pub color: String,
    /// Model alias from models.toml
    pub model: String,
    pub effort: String,
    /// read-only | workspace-write
    pub sandbox: String,
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
            description: String::new(),
            instructions: String::new(),
            max_tokens: None,
        }
    }
}

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
    pub orchestrator: String,
    pub phase_gate: String,
    pub on_reprompt: String,
    pub finale: Vec<FinaleStep>,
}

impl Default for Flow {
    fn default() -> Self {
        Flow { planner: "planner".into(), orchestrator: "orchestrator".into(), phase_gate: "qa".into(), on_reprompt: "planner".into(), finale: vec![] }
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
}

impl Default for Pattern {
    fn default() -> Self {
        Pattern { name: String::new(), description: String::new(), settings: PatternSettings::default(), roles: BTreeMap::new(), flow: Flow::default() }
    }
}

impl Pattern {
    pub fn builtin() -> Pattern {
        toml::from_str(DEFAULT_PATTERN).expect("built-in pattern must parse")
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
        }
        let need = |role: &str, kind: &str, what: &str, errs: &mut Vec<String>| match self.roles.get(role) {
            None => errs.push(format!("flow.{what} = '{role}' is not a defined role")),
            Some(r) if r.kind != kind => errs.push(format!("flow.{what} role '{role}' must have kind = \"{kind}\"")),
            _ => {}
        };
        need(&self.flow.planner, "planner", "planner", &mut errs);
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
        let p = Self::path_for(&self.name);
        crate::config::atomic_write(&p, &format!("# Mantra pattern — edit here or in the Studio (ctrl+o → s)\n{}", self.to_toml()))?;
        Ok(p)
    }

    /// Load by name: `<project>/.mantra/patterns/` if a repo ships one (Mantra never creates it), then `~/.mantra/patterns/`, then built-in.
    pub fn load(name: &str, project: &Path) -> Result<Pattern> {
        let file = format!("{}.toml", crate::util::slug(name));
        for dir in [project.join(".mantra").join("patterns"), crate::config::patterns_dir()] {
            let p = dir.join(&file);
            if let Ok(s) = std::fs::read_to_string(&p) {
                return Pattern::from_toml(&s).map_err(|e| anyhow!("{}: {e}", p.display()));
            }
        }
        if crate::util::slug(name) == "mantra-default" {
            return Ok(Pattern::builtin());
        }
        Err(anyhow!("pattern '{name}' not found"))
    }

    pub fn list(project: &Path) -> Vec<String> {
        let mut v = vec!["mantra-default".to_string()];
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
Explore the repository first (read files, list directories) so your plan fits the real code.
Design a plan made of sequential PHASES. Inside a phase, every task runs IN PARALLEL on its own
isolated copy of the repo, so tasks in the same phase must not depend on each other and should
touch disjoint files (declare each task's `scope` as path globs). Anything sequential goes into a later phase.
Pick the right worker role per task: small, well-bounded changes → worker-small; broad or tricky work → worker-big.
Every phase ends with a gate: list shell `checks` that must pass (build, tests, lint) and what QA should focus on.
Write prompts for workers that are self-contained: they only see their own prompt, not the whole plan.
Also write `orchestrator_brief`: how the orchestrator should supervise this particular project.
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
(worker finished, failed, went out of scope, stalled), decide what to do: accept, steer the worker
with mantra_prompt, or respawn it with a better prompt via mantra_retry. Keep turns short:
act, then call mantra_wait. Don't poll in loops — Mantra wakes you.
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
logic, everything wired together, build and tests green. Fix problems yourself until the gate criteria are met.
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
    fn catches_errors() {
        let mut p = Pattern::builtin();
        p.flow.phase_gate = "nope".into();
        assert!(p.validate().is_err());
    }
}
