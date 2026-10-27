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
    /// land in the inbox only if a tool forces the question), "on-request", "untrusted", or
    /// "BYPASS-SANDBOX-DANGER" (never asks AND overrides this role's sandbox).
    pub permission: String,
    pub description: String,
    pub instructions: String,
    /// Tripwire when an agent of this role uses more tokens than this.
    pub max_tokens: Option<u64>,
    /// Model alias to fall back to when an agent of this role twice sits through
    /// `settings.first_token_minutes` with no output at all (a gateway that accepts the turn and
    /// never answers). Unset: the second time goes to the manager like any other watchdog case.
    pub fallback: Option<String>,
    /// MCP servers this role's agents get (names from `settings.mcp_servers`, or `"*"` for every
    /// enabled one). Empty = none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mcp: Vec<String>,
    /// How many agents of this (worker) role may run at once — a class of its own, counted apart
    /// from the rest. A role without it shares `settings.max_parallel` with the other roles
    /// without one. `worker-swarm` is the built-in user: many tiny tasks, 16 at a time, without
    /// crowding the big and small workers out of their slots.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel: Option<usize>,
    /// Whether this role may spawn a temporary Akasha swarm in Solo. Disabled by default.
    #[serde(default)]
    pub can_spawn_swarm: bool,
}

impl Default for Role {
    fn default() -> Self {
        Role {
            mcp: vec![],
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
            parallel: None,
            can_spawn_swarm: false,
        }
    }
}

/// `Role.effort` value that lets Mantra choose the reasoning effort per spawn.
pub const EFFORT_AUTO: &str = "auto";

/// What `effort = "auto"` resolves to for one spawn. Planning, supervising and gating are
/// `medium`; a worker is `low` when its role says it is small or one of a swarm (`worker-small`,
/// `worker-swarm`) and `medium` otherwise; a finale step is `high`, whatever its kind. A role
/// that names a concrete effort never comes through here.
pub fn auto_effort(kind: &str, role_name: &str, finale: bool) -> &'static str {
    if finale {
        return "high";
    }
    let name = role_name.to_lowercase();
    match kind {
        "worker" if name.contains("small") || name.contains("swarm") => "low",
        _ => "medium",
    }
}

/// Values `Role.permission` may take.
pub const PERMISSIONS: &[&str] = &["never", "on-request", "untrusted", crate::config::BYPASS_SANDBOX_DANGER];

/// Values `Group.push` may take: which new notes reach a group's busy members mid-turn.
pub const PUSH_MODES: &[&str] = &["none", "urgent", "all"];
/// The range `Group.shared_chars` must fall in: below 1k a block holds two notes, above 256k it
/// is a second transcript every member re-reads.
pub const SHARED_CHARS_MIN: usize = 1_024;
pub const SHARED_CHARS_MAX: usize = 262_144;

/// The numbers `--parallel N` and a role's `parallel` accept. bija is light, so the ceiling is
/// generous; past `PARALLEL_NOTE_ABOVE` the provider's rate limits are what binds, and the run says so.
pub const PARALLEL_RANGE: std::ops::RangeInclusive<usize> = 1..=128;
pub const PARALLEL_NOTE_ABOVE: usize = 32;

/// Parses a parallelism count (`--parallel N`, a role's `parallel` typed in the Studio), refusing
/// anything outside `PARALLEL_RANGE` with a sentence that says what fits.
pub fn parse_parallel(s: &str) -> std::result::Result<usize, String> {
    let bad = || format!("'{s}' is not a number of workers from {} to {}", PARALLEL_RANGE.start(), PARALLEL_RANGE.end());
    let n: usize = s.trim().parse().map_err(|_| bad())?;
    if PARALLEL_RANGE.contains(&n) {
        Ok(n)
    } else {
        Err(bad())
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
    /// Tokens of context every spawned agent is told it has, whatever its model's window: the
    /// effective window is `min(model window, context_cap)`, so the backend compacts there. A
    /// 500k-token context let a worker cycle tools for hours (6.8M tokens, mostly cached input,
    /// on one task); 200k keeps it honest. 0 = the model's full window.
    pub context_cap: u64,
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
            context_cap: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(default)]
pub struct FinaleStep {
    pub role: String,
    pub task: String,
    /// Allow this step to spawn ad-hoc workers.
    pub may_spawn: bool,
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
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
    /// Akasha groups (experimental, `[[groups]]`): agents that share one block of notes. Empty for
    /// every pattern written before them, and inert until `mantra experimental enable akasha`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<Group>,
}

/// One Akasha group of a pattern: which roles share a block, how big it is, what gets pushed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Group {
    pub name: String,
    /// Role names of this pattern; a role belongs to at most one group.
    pub roles: Vec<String>,
    /// The block's capacity in characters (1024..=262144).
    pub shared_chars: usize,
    /// Which new notes reach busy members mid-turn: "none" | "urgent" | "all".
    pub push: String,
    /// Whether the Conductor adds one-line entries for members' task done/failed and files changed.
    pub auto: bool,
}

impl Default for Group {
    fn default() -> Self {
        Group { name: String::new(), roles: vec![], shared_chars: 16_384, push: "urgent".into(), auto: true }
    }
}

impl Pattern {
    /// Auto starts with one planner, which designs a concrete workflow with the user.
    pub fn builtin_auto() -> Pattern {
        let mut p = Pattern { name: NAME_AUTO.into(), description: "Auto: design a workflow with the planner for this task".into(), ..Pattern::default() };
        p.roles.insert("planner".into(), planner_role());
        p.flow.orchestrator.clear();
        p.flow.phase_gate.clear();
        p
    }

    pub fn is_auto(&self) -> bool { self.name == NAME_AUTO }

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
        vec![NAME_DEFAULT, NAME_LEAN, NAME_THOROUGH, NAME_AUTO]
    }

    /// A built-in pattern by name (slug-insensitive), if `name` is one of `names()`.
    pub fn builtin_named(name: &str) -> Option<Pattern> {
        match crate::util::slug(name).as_str() {
            NAME_AUTO => Some(Pattern::builtin_auto()),
            NAME_DEFAULT => Some(Pattern::builtin()),
            NAME_LEAN => Some(Pattern::builtin_lean()),
            NAME_THOROUGH => Some(Pattern::builtin_thorough()),
            _ => None,
        }
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

    /// `--parallel N` on the run's own copy: one number the user chose, for every class — the
    /// shared pool (`max_parallel`) and each role with a cap of its own (`parallel`). Phases run
    /// one after another and a plan may give a phase no more than `max_tasks_per_phase` tasks
    /// (`phase_task_limit`), so that cap rises with `n` too — or N past it could never be reached
    /// by the shared pool. A smaller `n` leaves the cap alone: a phase of 32 tasks four at a time
    /// is still a fine plan.
    pub fn set_parallel(&mut self, n: usize) {
        self.settings.max_parallel = n;
        self.settings.max_tasks_per_phase = self.settings.max_tasks_per_phase.max(n);
        for role in self.roles.values_mut().filter(|r| r.parallel.is_some()) {
            role.parallel = Some(n);
        }
    }

    /// How many agents of `role` may run at once: the role's own `parallel`, or — a role without
    /// one, or a name this pattern does not have — the shared `settings.max_parallel`.
    pub fn cap_of(&self, role: &str) -> usize {
        self.roles.get(role).and_then(|r| r.parallel).unwrap_or(self.settings.max_parallel)
    }

    /// Whether `role` is a class of its own (`parallel` set) rather than one of the shared pool.
    pub fn has_own_cap(&self, role: &str) -> bool {
        self.roles.get(role).map(|r| r.parallel.is_some()).unwrap_or(false)
    }

    /// The most tasks a phase that uses `roles` may hold: `settings.max_tasks_per_phase`, or twice
    /// the largest own cap among them when that is more — a swarm phase is meant to be wider than
    /// the shared pool, and a 32-task phase of 16-at-once workers is two waves, not a bad plan.
    pub fn phase_task_limit<'a>(&self, roles: impl IntoIterator<Item = &'a str>) -> usize {
        let widest = roles.into_iter().filter_map(|r| self.roles.get(r).and_then(|r| r.parallel)).max().unwrap_or(0);
        self.settings.max_tasks_per_phase.max(widest.saturating_mul(2))
    }

    /// One line on how many run at once: the shared pool, then each worker class of its own
    /// (`4 at once · worker-swarm 16`) — what a briefing or a journal line says about the width.
    pub fn parallel_summary(&self) -> String {
        let mut s = format!("{} at once", self.settings.max_parallel);
        for (n, r) in self.roles.iter().filter(|(_, r)| r.kind == "worker") {
            if let Some(cap) = r.parallel {
                s.push_str(&format!(" · {n} {cap}"));
            }
        }
        s
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
            match r.parallel {
                Some(p) if !PARALLEL_RANGE.contains(&p) => errs.push(format!("role '{n}': parallel must be from {} to {} (got {p})", PARALLEL_RANGE.start(), PARALLEL_RANGE.end())),
                // only workers are spawned many at a time; on any other kind the number would be read by nothing
                Some(_) if r.kind != "worker" => errs.push(format!("role '{n}': parallel is for worker roles (this one is a {})", r.kind)),
                _ => {}
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
        if !self.is_auto() {
            need(&self.flow.orchestrator, "orchestrator", "orchestrator", &mut errs);
            need(&self.flow.phase_gate, "gate", "phase_gate", &mut errs);
        } else if self.roles.len() != 1 || !self.flow.orchestrator.is_empty() || !self.flow.phase_gate.is_empty() || !self.flow.manager.is_empty() || !self.flow.finale.is_empty() || !self.groups.is_empty() {
            errs.push("Auto is a planner-only bootstrap; give a concrete workflow another name".into());
        }
        if !self.roles.contains_key(&self.flow.on_reprompt) {
            errs.push(format!("flow.on_reprompt = '{}' is not a defined role", self.flow.on_reprompt));
        }
        for (i, s) in self.flow.finale.iter().enumerate() {
            if !self.roles.contains_key(&s.role) {
                errs.push(format!("flow.finale[{i}] role '{}' is not defined", s.role));
            }
        }
        if !self.is_auto() && self.worker_roles().is_empty() {
            errs.push("pattern needs at least one role with kind = \"worker\"".into());
        }
        if self.settings.max_parallel == 0 {
            errs.push("settings.max_parallel must be ≥ 1".into());
        }
        if self.settings.watchdog_escalate_seconds <= self.settings.watchdog_seconds {
            errs.push("settings.watchdog_escalate_seconds must be greater than watchdog_seconds".into());
        }
        self.validate_groups(&mut errs);
        if errs.is_empty() {
            Ok(())
        } else {
            Err(errs)
        }
    }

    /// `[[groups]]`: named once each, made of this pattern's roles, a role in one group at most
    /// (an agent reads and writes exactly one block), with a capacity and push mode Mantra knows.
    fn validate_groups(&self, errs: &mut Vec<String>) {
        let mut names: Vec<&str> = vec![];
        // role → the label of the group that has it
        let mut owner: BTreeMap<&str, String> = BTreeMap::new();
        for (i, g) in self.groups.iter().enumerate() {
            let name = g.name.trim();
            let label = if name.is_empty() { format!("groups[{i}]") } else { format!("group '{name}'") };
            if name.is_empty() {
                errs.push(format!("groups[{i}]: name is empty"));
            } else if name != g.name {
                // the block, `/akasha <group>` and every header key off the name as written
                errs.push(format!("{label}: the name has spaces around it (\"{}\")", g.name));
            } else if names.contains(&name) {
                errs.push(format!("{label}: the name is used twice"));
            } else {
                names.push(name);
            }
            for r in &g.roles {
                if !self.roles.contains_key(r) {
                    errs.push(format!("{label}: role '{r}' is not a defined role"));
                    continue;
                }
                match owner.get(r.as_str()) {
                    Some(other) if *other == label => errs.push(format!("{label}: role '{r}' is listed twice")),
                    Some(other) => errs.push(format!("{label}: role '{r}' is already in {other} (a role belongs to one group at most)")),
                    None => {
                        owner.insert(r.as_str(), label.clone());
                    }
                }
            }
            if !(SHARED_CHARS_MIN..=SHARED_CHARS_MAX).contains(&g.shared_chars) {
                errs.push(format!("{label}: shared_chars must be from {SHARED_CHARS_MIN} to {SHARED_CHARS_MAX} (got {})", g.shared_chars));
            }
            if !PUSH_MODES.contains(&g.push.as_str()) {
                errs.push(format!("{label}: push must be one of {} (got '{}')", PUSH_MODES.join(", "), g.push));
            }
        }
    }

    #[cfg_attr(not(test), allow(dead_code))] // only tests ask since the fresh-install variants went
    pub fn path_for(name: &str) -> PathBuf {
        crate::config::patterns_dir().join(format!("{}.toml", crate::util::slug(name)))
    }

    pub fn save(&self) -> Result<PathBuf> {
        let dir = self.source_dir.clone().unwrap_or_else(crate::config::patterns_dir);
        let p = dir.join(format!("{}.toml", crate::util::slug(&self.name)));
        crate::config::atomic_write(&p, &format!("# Mantra pattern — edit here or in the Studio (ctrl+o → s)\n{}", self.to_toml()))?;
        Ok(p)
    }

    /// Where pattern files are looked up, in order: `<project>/.mantra/patterns/` if a repo ships
    /// one (Mantra never creates it), then `~/.mantra/patterns/`.
    fn dirs(project: &Path) -> [PathBuf; 2] {
        [project.join(".mantra").join("patterns"), crate::config::patterns_dir()]
    }

    /// The file `load` reads for `name`, if there is one.
    fn file_of(name: &str, project: &Path) -> Option<PathBuf> {
        let file = format!("{}.toml", crate::util::slug(name));
        Pattern::dirs(project).into_iter().map(|d| d.join(&file)).find(|p| p.is_file())
    }

    /// Load by name: a file (`dirs`), then a built-in, then — for a built-in 0.8 retired
    /// (`RETIRED`) — `mantra-default`, with one line in the log.
    pub fn load(name: &str, project: &Path) -> Result<Pattern> {
        let file = format!("{}.toml", crate::util::slug(name));
        for dir in Pattern::dirs(project) {
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
        if is_retired(name) {
            log_retired_once(name);
            return Ok(Pattern::builtin());
        }
        Err(anyhow!("pattern '{name}' not found"))
    }

    pub fn list(project: &Path) -> Vec<String> {
        let mut v: Vec<String> = Pattern::names().into_iter().map(String::from).collect();
        for dir in Pattern::dirs(project) {
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

/// The patterns list API the web's patterns page and the TUI's run picker are built on: a pattern
/// as a list shows it, a copy under a new name, a file deleted — each with the reasons it refuses.
impl Pattern {
    /// Where this pattern comes from, as a list shows it: "default" (the built-in
    /// `mantra-default`), "built-in", "edited" (a file shadows a built-in) or "yours".
    pub fn tag(&self) -> &'static str {
        match (Pattern::builtin_named(&self.name).is_some(), self.source_dir.is_some()) {
            (true, false) if self.name == NAME_DEFAULT => "default",
            (true, false) => "built-in",
            (true, true) => "edited",
            (false, _) => "yours",
        }
    }

    /// What a list of patterns shows of this one (the web's patterns page and run picker): enough
    /// to choose by — the roles with their model and width — without the instructions.
    pub fn summary(&self) -> PatternSummary {
        let roles = self
            .ordered_roles()
            .into_iter()
            .map(|(name, r)| RoleSummary { name, kind: r.kind, glyph: r.glyph, model: r.model, effort: r.effort, parallel: r.parallel })
            .collect();
        PatternSummary { name: self.name.clone(), description: self.description.clone(), tag: self.tag(), max_parallel: self.settings.max_parallel, roles, error: None }
    }

    /// `summary()` of every pattern `list` names, in its order; one whose file does not load
    /// still shows — it may be mid-edit — with the reason in `error` and nothing else.
    pub fn summaries(project: &Path) -> Vec<PatternSummary> {
        Pattern::list(project)
            .into_iter()
            .map(|name| match Pattern::load(&name, project) {
                Ok(p) => p.summary(),
                Err(e) => {
                    // the reason, without the file's path in front of it (the row already names it)
                    let e = e.to_string();
                    let why = e.split_once(".toml: ").map(|(_, why)| why).unwrap_or(&e).lines().next().unwrap_or_default().to_string();
                    let tag = if Pattern::builtin_named(&name).is_some() { "edited" } else { "yours" };
                    PatternSummary { name, description: String::new(), tag, max_parallel: 0, roles: vec![], error: Some(why) }
                }
            })
            .collect()
    }

    /// A copy of this pattern under `name` (slugged), validated and written beside the original —
    /// a project's own pattern stays in the project, anything else goes to `~/.mantra/patterns/`.
    /// Refused, with every reason: a name that is empty, a built-in's, or another pattern's file
    /// already; and whatever `validate` finds. Returns the copy as `load` will read it, and its path.
    pub fn save_as(&self, name: &str, project: &Path) -> std::result::Result<(Pattern, PathBuf), Vec<String>> {
        let slug = crate::util::slug(name);
        if slug.is_empty() {
            return Err(vec!["a pattern needs a name".into()]);
        }
        if slug != crate::util::slug(&self.name) {
            if Pattern::builtin_named(&slug).is_some() {
                return Err(vec![format!("'{slug}' is a built-in pattern — pick another name")]);
            }
            if let Some(p) = Pattern::file_of(&slug, project) {
                return Err(vec![format!("a pattern named '{slug}' exists already ({}) — pick another name or delete it first", p.display())]);
            }
        }
        let copy = Pattern { name: slug, source_dir: Some(self.source_dir.clone().unwrap_or_else(crate::config::patterns_dir)), ..self.clone() };
        copy.validate()?;
        let path = copy.save().map_err(|e| vec![format!("could not save {}: {e}", copy.name)])?;
        Ok((copy, path))
    }

    /// Delete the file `load` reads for `name` (an edited built-in is then the built-in again).
    /// Refused for a built-in without a file, a name nothing answers to, and a file that ships
    /// with the project — Mantra does not delete files in your repository.
    pub fn delete_file(name: &str, project: &Path) -> std::result::Result<PathBuf, String> {
        let Some(path) = Pattern::file_of(name, project) else {
            return Err(if Pattern::builtin_named(name).is_some() { format!("'{name}' is built in — there is no file to delete") } else { format!("no pattern '{name}'") });
        };
        let [shipped, _] = Pattern::dirs(project);
        if path.starts_with(shipped) {
            return Err(format!("'{name}' comes with this project ({}) — Mantra does not delete files in your repository", path.display()));
        }
        std::fs::remove_file(&path).map_err(|e| format!("could not delete {}: {e}", path.display()))?;
        Ok(path)
    }
}

/// One pattern as a list shows it (`Pattern::summary`): serialized as is for the web.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct PatternSummary {
    pub name: String,
    pub description: String,
    /// "default" | "built-in" | "edited" | "yours" (`Pattern::tag`).
    pub tag: &'static str,
    /// The width of the shared pool: the worker roles without a `parallel` of their own.
    pub max_parallel: usize,
    /// In display order (`ordered_roles`).
    pub roles: Vec<RoleSummary>,
    /// Why the pattern's file does not load; everything but the name and tag is empty then.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// One role of a `PatternSummary`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct RoleSummary {
    pub name: String,
    pub kind: String,
    pub glyph: String,
    pub model: String,
    pub effort: String,
    /// The role's own cap (`Role::parallel`); `None` = it shares `max_parallel`.
    pub parallel: Option<usize>,
}

// ───────────────────────────── built-in patterns ─────────────────────────────
//
// Three patterns ship in the binary. The roles they share (planner, manager, orchestrator, the
// three worker classes, the phase QA) are defined once.

pub const NAME_DEFAULT: &str = "mantra-default";
pub const NAME_AUTO: &str = "auto";
pub const NAME_LEAN: &str = "mantra-lean";
pub const NAME_THOROUGH: &str = "mantra-thorough";

/// Built-ins 0.8 retired. The swarm became a worker class of every pattern (`worker-swarm`), and
/// with the built-ins on bija themselves the bija and Claude Code copies of the default had
/// nothing left to differ in. A name of one in settings.toml, a run's state or `--pattern` reads
/// as `mantra-default`; a file of that name is somebody's own pattern and loads as such.
pub const RETIRED: &[&str] = &["mantra-swarm", "mantra-default-bija", "mantra-default-claude"];

/// Whether `name` (slug-insensitive) is one of the `RETIRED` built-ins.
pub fn is_retired(name: &str) -> bool {
    RETIRED.contains(&crate::util::slug(name).as_str())
}

/// The name a pattern called `name` goes by now: `mantra-default` for a retired built-in that no
/// file answers to, else `name` itself — what a session shows and saves as its pattern for new
/// runs, so a settings.toml from 0.7 stops naming one that is gone.
pub fn current_name(name: &str, project: &Path) -> String {
    if is_retired(name) && Pattern::file_of(name, project).is_none() {
        NAME_DEFAULT.into()
    } else {
        name.to_string()
    }
}

/// The one log line a retired name gets, once per name and process — `load` runs on every web
/// snapshot, and a session started with `--pattern mantra-swarm` keeps asking for it.
fn log_retired_once(name: &str) {
    static SEEN: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
    let slug = crate::util::slug(name);
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    if !seen.contains(&slug) {
        crate::mlog!("pattern '{slug}' was retired in 0.8 — loading {NAME_DEFAULT} (every built-in has the worker-swarm class now)");
        seen.push(slug);
    }
}

/// The budget rule every built-in role ends with.
const BUDGET: &str = "Do not re-read files you already read. Do not run the full test suite more than once per turn unless it failed.";

const PLANNER_INSTRUCTIONS: &str = "You are the Planner of a multi-agent software team run by Mantra. You own the plan, never the code.
Look at the existing code only as far as the plan needs (an empty project needs no exploration).
Split the work into sequential PHASES. Tasks inside a phase run in parallel on isolated copies of the repo, so they must not depend on each other and must own disjoint files: declare each task's `scope` as path globs. Anything sequential goes into a later phase.
Size the plan to the goal: a small goal is one or two phases and a few tasks, submitted in your first turn. Small, well-bounded changes go to worker-small, broad or tricky work to worker-big.
Use worker-swarm only when a phase is many (6+) small, independent, same-shaped tasks (per-file migrations, a test per module, repetitive edits); then cut one task per unit of work. Everything else goes to worker-small or worker-big.
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

const WORKER_SWARM_INSTRUCTIONS: &str = "You are one worker of a swarm: many agents at once, each on one small, self-contained, mechanical task. Your prompt is all you need: do exactly that task with the smallest change that does it.
Do not explore outside your scope, refactor, or touch anything the task does not name. Verify with the build, tests or lint of the files you touched, once; never the whole suite.
Report in two lines: what changed, how you verified it.";

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

/// The swarm class: many tiny, independent tasks at once, counted apart from the big and small
/// workers (`parallel`), so a 20-task phase of per-file edits neither waits four at a time nor
/// takes the slots a hard task needs. The planner is told to use it only where it fits.
fn worker_swarm_role() -> Role {
    Role {
        kind: "worker".into(),
        glyph: "∴".into(),
        color: "amber".into(),
        model: "luna".into(),
        effort: EFFORT_AUTO.into(),
        description: "Many tiny, independent tasks at once".into(),
        instructions: with_budget(WORKER_SWARM_INSTRUCTIONS),
        parallel: Some(16),
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
    for (n, r) in [("planner", planner_role()), ("orchestrator", orchestrator_role()), ("worker-small", worker_small_role()), ("worker-big", worker_big_role()), ("worker-swarm", worker_swarm_role()), ("qa", qa_role())] {
        p.roles.insert(n.into(), r);
    }
    p.flow = Flow { planner: "planner".into(), manager: String::new(), orchestrator: "orchestrator".into(), phase_gate: "qa".into(), on_reprompt: "planner".into(), finale: vec![] };
    // 200k is plenty for any one task; a bigger window only lets an agent cycle tools for hours.
    p.settings.context_cap = 200_000;
    // The workers share one Akasha block — inert until akasha is switched on, and then it works
    // out of the box.
    p.groups = vec![Group { name: "builders".into(), roles: p.worker_roles(), ..Group::default() }];
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
    for (name, effort) in [("planner", "max"), ("manager", "high"), ("orchestrator", "high"), ("worker-small", "medium"), ("worker-big", "high"), ("worker-swarm", "low"), ("qa", "high")] {
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
        Pattern::names().into_iter().filter(|n| *n != NAME_AUTO).map(|n| Pattern::builtin_named(n).unwrap()).collect()
    }

    #[test]
    fn auto_is_only_a_valid_planner_bootstrap() {
        let auto = Pattern::builtin_auto();
        assert!(auto.validate().is_ok());
        assert!(auto.is_auto());
        assert_eq!(auto.roles.len(), 1);
        assert!(auto.worker_roles().is_empty());
        assert_eq!(Pattern::from_toml(&auto.to_toml()).unwrap(), auto);
        let mut malformed = auto.clone();
        malformed.roles.insert("worker".into(), Role::default());
        assert!(malformed.validate().is_err(), "Auto cannot disguise a partially configured executable workflow");
        let concrete = Pattern { name: "custom".into(), ..auto };
        assert!(concrete.validate().is_err(), "a concrete workflow still needs orchestrator, worker and gate");
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
        assert_eq!(Pattern::names(), vec!["mantra-default", "mantra-lean", "mantra-thorough", "auto"]);
        for n in Pattern::names() {
            let p = Pattern::load(n, Path::new("/nonexistent")).unwrap_or_else(|e| panic!("{n}: {e}"));
            assert_eq!(p.name, n);
            assert!(Pattern::list(Path::new("/nonexistent")).contains(&n.to_string()));
        }
        assert!(Pattern::load("mantra-nope", Path::new("/nonexistent")).is_err());
        // slug-insensitive, like a file lookup
        assert_eq!(Pattern::builtin_named("Mantra Lean").map(|p| p.name), Some("mantra-lean".to_string()));
    }

    /// The built-ins 0.8 retired load as `mantra-default` — from settings.toml, a run's state or
    /// `--pattern` alike, all of which go through `load` — and are no longer listed; a file of
    /// that name is somebody's own pattern and loads as such.
    #[test]
    fn retired_builtins_load_as_the_default_unless_a_file_has_the_name() {
        crate::web::snapshot::tests::with_mantra_home(|| {
            let project = crate::config::home().join("project");
            for n in RETIRED.iter().copied().chain(["Mantra Swarm"]) {
                assert!(is_retired(n), "{n}");
                assert!(Pattern::builtin_named(n).is_none(), "{n} is not a built-in any more");
                assert_eq!(Pattern::load(n, &project).unwrap(), Pattern::builtin(), "{n}");
                assert_eq!(current_name(n, &project), NAME_DEFAULT, "{n}");
                assert!(!Pattern::list(&project).iter().any(|l| is_retired(l)), "not listed without a file");
            }
            assert_eq!(current_name("mine", &project), "mine", "anything else is left as it is");
            assert!(!is_retired(NAME_DEFAULT));
            // a file of the user's under a retired name is theirs
            let mine = Pattern { name: "mantra-swarm".into(), description: "my old swarm".into(), ..Pattern::builtin_lean() };
            mine.save().unwrap();
            let loaded = Pattern::load("mantra-swarm", &project).unwrap();
            assert_eq!((loaded.name.as_str(), loaded.description.as_str(), loaded.tag()), ("mantra-swarm", "my old swarm", "yours"));
            assert_eq!(current_name("mantra-swarm", &project), "mantra-swarm");
            assert!(Pattern::list(&project).contains(&"mantra-swarm".to_string()));
            assert_eq!(Pattern::load("mantra-default-bija", &project).unwrap().name, NAME_DEFAULT, "the others still migrate");
        });
    }

    #[test]
    fn every_builtin_role_uses_a_supported_alias_when_explicitly_configured() {
        let mut reg = crate::config::Registry::defaults();
        reg.providers.push(crate::config::bija_default_provider());
        reg.models.extend(crate::config::bija_default_model_entries("bija"));
        for p in all_builtins() {
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

    /// Every built-in caps the context at 200k; a pattern file from before the setting existed
    /// (or one written by hand without it) keeps the model's full window.
    #[test]
    fn the_context_cap_is_200k_in_the_builtins_and_off_for_old_patterns() {
        for p in all_builtins() {
            assert_eq!(p.settings.context_cap, 200_000, "{}", p.name);
        }
        let toml = Pattern::builtin().to_toml();
        assert!(toml.contains("context_cap = 200000"), "the Studio's TOML carries it");
        let old = toml.replace("context_cap = 200000", "");
        assert_eq!(Pattern::from_toml(&old).unwrap().settings.context_cap, 0, "missing = 0 = the model's full window");
        let custom = toml.replace("context_cap = 200000", "context_cap = 64000");
        assert_eq!(Pattern::from_toml(&custom).unwrap().settings.context_cap, 64_000);
    }

    #[test]
    fn permission_defaults_off_and_validates() {
        let p = Pattern::builtin();
        for (n, r) in &p.roles {
            assert_eq!(r.permission, "never", "role '{n}' should default to permission = never");
        }
        let mut bad = Pattern::builtin();
        let name = bad.worker_roles()[0].clone();
        bad.roles.get_mut(&name).unwrap().permission = crate::config::BYPASS_SANDBOX_DANGER.into();
        assert!(bad.validate().is_ok());
        let restored = Pattern::from_toml(&bad.to_toml()).unwrap();
        assert_eq!(restored.roles[&name].permission, crate::config::BYPASS_SANDBOX_DANGER);
        assert_eq!(restored.roles[&name].sandbox, bad.roles[&name].sandbox, "permission is separate from the sandbox field");
        bad.roles.get_mut(&name).unwrap().permission = "sometimes".into();
        let errs = bad.validate().unwrap_err();
        assert!(errs.iter().any(|e| e.contains("permission") && e.contains("sometimes")), "{errs:?}");
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
        assert_eq!(auto_effort("worker", "worker-swarm", false), "low", "a swarm task is small by definition");
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

    /// Every built-in has the swarm class: `worker-swarm` beside the small and big workers — on
    /// `luna`, low effort (explicit in the heavy flow, `auto` → low elsewhere), 16 at once of its
    /// own — all three workers in one `builders` Akasha group (inert until akasha is on), and a
    /// planner told when the class fits and when it does not.
    #[test]
    fn every_builtin_has_the_swarm_class_the_builders_group_and_the_planner_paragraph() {
        for p in all_builtins() {
            let s = &p.roles["worker-swarm"];
            assert_eq!((s.kind.as_str(), s.glyph.as_str(), s.color.as_str(), s.model.as_str(), s.parallel), ("worker", "∴", "amber", "luna", Some(16)), "{}", p.name);
            assert_eq!(s.description, "Many tiny, independent tasks at once");
            let effort = if s.effort == EFFORT_AUTO { auto_effort(&s.kind, "worker-swarm", false) } else { s.effort.as_str() };
            assert_eq!(effort, "low", "{}", p.name);
            assert!(s.instructions.contains("one small, self-contained, mechanical task") && s.instructions.contains("Report in two lines") && s.instructions.ends_with(BUDGET), "{}", s.instructions);
            // the big and small workers share the pool; the swarm counts apart
            assert_eq!((p.roles["worker-small"].parallel, p.roles["worker-big"].parallel), (None, None), "{}", p.name);
            assert_eq!((p.cap_of("worker-swarm"), p.cap_of("worker-big"), p.cap_of("worker-small")), (16, p.settings.max_parallel, p.settings.max_parallel));
            assert!(p.has_own_cap("worker-swarm") && !p.has_own_cap("worker-big") && !p.has_own_cap("nope"));
            assert_eq!(p.groups, vec![Group { name: "builders".into(), roles: vec!["worker-big".into(), "worker-small".into(), "worker-swarm".into()], shared_chars: 16_384, push: "urgent".into(), auto: true }], "{}", p.name);
            let planner = &p.roles["planner"].instructions;
            assert!(planner.contains("Use worker-swarm only when a phase is many (6+) small, independent, same-shaped tasks (per-file migrations, a test per module, repetitive edits); then cut one task per unit of work. Everything else goes to worker-small or worker-big."), "{planner}");
            assert!(planner.starts_with("You are the Planner") && planner.ends_with(BUDGET));
            let toml = p.to_toml();
            assert!(toml.contains("parallel = 16") && toml.contains("[[groups]]") && toml.contains("name = \"builders\""), "{toml}");
        }
        assert_eq!(Pattern::builtin().parallel_summary(), "4 at once · worker-swarm 16");
        assert_eq!(Pattern::builtin_lean().parallel_summary(), "3 at once · worker-swarm 16");
    }

    /// `parallel` is 1..=128 and for workers only; a role written without it (every role of a
    /// file from before it) writes no `parallel` key back, and one with it round-trips.
    #[test]
    fn role_parallel_is_validated_and_round_trips() {
        for (role, n, words) in [("worker-swarm", 0, &["role 'worker-swarm'", "parallel must be from 1 to 128", "got 0"][..]), ("worker-big", 129, &["from 1 to 128", "got 129"]), ("qa", 4, &["role 'qa'", "parallel is for worker roles", "gate"])] {
            let mut p = Pattern::builtin();
            p.roles.get_mut(role).unwrap().parallel = Some(n);
            let errs = p.validate().expect_err(role).join(" | ");
            for w in words {
                assert!(errs.contains(w), "{role}={n}: '{w}' missing in {errs}");
            }
            assert!(Pattern::from_toml(&p.to_toml()).is_err(), "the same through the TOML");
        }
        for n in [1, 128] {
            let mut p = Pattern::builtin();
            p.roles.get_mut("worker-big").unwrap().parallel = Some(n);
            assert!(p.validate().is_ok(), "{n}: {:?}", p.validate());
            assert_eq!(Pattern::from_toml(&p.to_toml()).unwrap(), p);
        }
        // a role of a file from before the field: none, and none written back
        let old = Pattern::builtin().to_toml().replace("parallel = 16\n", "");
        let p = Pattern::from_toml(&old).unwrap();
        assert_eq!(p.roles["worker-swarm"].parallel, None, "it shares the pool then");
        assert_eq!(p.cap_of("worker-swarm"), p.settings.max_parallel);
        assert!(!p.to_toml().lines().any(|l| l.starts_with("parallel =")), "nothing new in an old file");
    }

    /// `--parallel N` sets every class: the shared pool and each role with a cap of its own —
    /// never giving one to a role without — and room for N tasks a phase when the pattern had less.
    #[test]
    fn set_parallel_sets_every_class() {
        let mut p = Pattern::builtin();
        p.set_parallel(8);
        assert_eq!((p.settings.max_parallel, p.cap_of("worker-swarm"), p.cap_of("worker-big")), (8, 8, 8));
        assert_eq!((p.roles["worker-big"].parallel, p.roles["worker-swarm"].parallel), (None, Some(8)), "the big worker still shares the pool");
        assert_eq!(p.settings.max_tasks_per_phase, 8, "8 fits the pattern's 8 tasks a phase");
        p.set_parallel(64);
        assert_eq!((p.settings.max_parallel, p.cap_of("worker-swarm"), p.settings.max_tasks_per_phase), (64, 64, 64));
        p.set_parallel(2);
        assert_eq!((p.settings.max_parallel, p.cap_of("worker-swarm"), p.settings.max_tasks_per_phase), (2, 2, 64), "a smaller N leaves the phase cap");
        assert!(p.validate().is_ok());
    }

    /// A phase may hold `max_tasks_per_phase` tasks, or twice the widest own cap among the roles
    /// it uses — so a 32-task swarm phase fits the default pattern as it is.
    #[test]
    fn the_phase_task_limit_widens_for_a_class_of_its_own() {
        let p = Pattern::builtin();
        assert_eq!(p.phase_task_limit(["worker-big", "worker-small"]), 8);
        assert_eq!(p.phase_task_limit(["worker-big", "worker-swarm"]), 32);
        assert_eq!(p.phase_task_limit(["worker-nope"]), 8, "an unknown role is the shared pool");
        assert_eq!(p.phase_task_limit([]), 8);
        let mut narrow = Pattern::builtin();
        narrow.roles.get_mut("worker-swarm").unwrap().parallel = Some(2);
        assert_eq!(narrow.phase_task_limit(["worker-swarm"]), 8, "never below the setting");
    }

    /// `summary` is what a list shows — name, description, tag, the pool's width and every role
    /// with its model and cap — and `summaries` covers every listed pattern, a broken file too.
    #[test]
    fn summaries_name_the_roles_with_their_caps_and_say_why_a_file_does_not_load() {
        crate::web::snapshot::tests::with_mantra_home(|| {
            let project = crate::config::home().join("project");
            let s = Pattern::builtin().summary();
            assert_eq!((s.name.as_str(), s.tag, s.max_parallel, s.error.as_ref()), (NAME_DEFAULT, "default", 4, None));
            assert_eq!(s.description, Pattern::builtin().description);
            assert_eq!(s.roles.first().map(|r| (r.name.as_str(), r.kind.as_str())), Some(("planner", "planner")), "in display order");
            let swarm = s.roles.iter().find(|r| r.name == "worker-swarm").unwrap();
            assert_eq!((swarm.glyph.as_str(), swarm.model.as_str(), swarm.effort.as_str(), swarm.parallel), ("∴", "luna", "auto", Some(16)));
            assert_eq!(s.roles.iter().find(|r| r.name == "worker-big").unwrap().parallel, None);
            let json = serde_json::to_value(&s).unwrap();
            assert!(json.get("error").is_none() && json["roles"].as_array().map(|r| r.len()) == Some(s.roles.len()), "{json}");
            assert_eq!(Pattern::builtin_lean().summary().tag, "built-in");
            // a file shadowing a built-in is "edited", one of the user's own "yours", a broken one says why
            Pattern::builtin_lean().save().unwrap();
            Pattern { name: "mine".into(), ..Pattern::builtin() }.save().unwrap();
            std::fs::write(crate::config::patterns_dir().join("broken.toml"), "name = [").unwrap();
            let all = Pattern::summaries(&project);
            let tag = |n: &str| all.iter().find(|s| s.name == n).map(|s| s.tag);
            assert_eq!((tag(NAME_DEFAULT), tag(NAME_LEAN), tag(NAME_THOROUGH), tag("mine")), (Some("default"), Some("edited"), Some("built-in"), Some("yours")));
            let broken = all.iter().find(|s| s.name == "broken").expect("a broken file still shows");
            assert!(broken.error.as_deref().map(|e| !e.is_empty() && !e.contains(".toml:")).unwrap_or(false), "{:?}", broken.error);
            assert!(broken.roles.is_empty() && broken.tag == "yours");
        });
    }

    /// `save_as` writes a validated copy under a new name and hands it back as `load` reads it;
    /// it refuses an empty name, a built-in's, an existing file's and an invalid pattern, with
    /// every reason. `delete_file` removes the user's file (an edited built-in is the built-in
    /// again) and refuses what has no file or ships with the project.
    #[test]
    fn save_as_copies_under_a_new_name_and_delete_file_removes_only_files_of_yours() {
        crate::web::snapshot::tests::with_mantra_home(|| {
            let project = crate::config::home().join("project");
            let base = Pattern::builtin();
            let (copy, path) = base.save_as("My Split", &project).unwrap();
            assert_eq!((copy.name.as_str(), copy.tag()), ("my-split", "yours"));
            assert_eq!(path, crate::config::patterns_dir().join("my-split.toml"));
            assert_eq!(Pattern::load("my-split", &project).unwrap(), copy, "what load reads");
            assert_eq!(base.name, NAME_DEFAULT, "the original is untouched");
            for (name, why) in [(" ", "needs a name"), ("mantra-lean", "is a built-in"), ("my-split", "exists already")] {
                let errs = base.save_as(name, &project).expect_err(name);
                assert!(errs.iter().any(|e| e.contains(why)), "{name}: {errs:?}");
            }
            assert!(copy.save_as("my-split", &project).is_ok(), "its own name is a plain save");
            let mut bad = copy.clone();
            bad.flow.phase_gate = "nope".into();
            bad.roles.get_mut("planner").unwrap().permission = "sometimes".into();
            assert_eq!(bad.save_as("other", &project).expect_err("invalid").len(), 2, "every reason");
            assert!(!crate::config::patterns_dir().join("other.toml").exists(), "nothing written");
            // delete
            assert_eq!(Pattern::delete_file("my-split", &project).unwrap(), path);
            assert!(Pattern::load("my-split", &project).is_err());
            assert!(Pattern::delete_file("my-split", &project).unwrap_err().contains("no pattern 'my-split'"));
            assert!(Pattern::delete_file(NAME_LEAN, &project).unwrap_err().contains("built in"));
            Pattern { description: "my lean".into(), ..Pattern::builtin_lean() }.save().unwrap();
            assert_eq!(Pattern::load(NAME_LEAN, &project).unwrap().tag(), "edited");
            Pattern::delete_file(NAME_LEAN, &project).unwrap();
            assert_eq!(Pattern::load(NAME_LEAN, &project).unwrap(), Pattern::builtin_lean(), "the built-in again");
            let shipped = project.join(".mantra").join("patterns");
            std::fs::create_dir_all(&shipped).unwrap();
            std::fs::write(shipped.join("team.toml"), Pattern { name: "team".into(), ..Pattern::builtin() }.to_toml()).unwrap();
            assert!(Pattern::delete_file("team", &project).unwrap_err().contains("comes with this project"));
            assert!(shipped.join("team.toml").exists());
        });
    }

    /// `[[groups]]` is validated like everything else in a pattern: each row is one broken group
    /// and the words its error must contain.
    #[test]
    fn groups_are_validated() {
        let g = |name: &str, roles: &[&str]| Group { name: name.into(), roles: roles.iter().map(|r| r.to_string()).collect(), ..Group::default() };
        let cases: Vec<(&str, Vec<Group>, &[&str])> = vec![
            ("empty name", vec![g(" ", &["worker-small"])], &["groups[0]", "name is empty"]),
            ("spaces around the name", vec![g("builders ", &["worker-small"])], &["group 'builders'", "spaces around it", "\"builders \""]),
            ("same name twice", vec![g("a", &["worker-small"]), g("a", &["worker-big"])], &["group 'a'", "used twice"]),
            ("unknown role", vec![g("a", &["worker-huge"])], &["group 'a'", "'worker-huge'", "not a defined role"]),
            ("role in two groups", vec![g("a", &["worker-small"]), g("b", &["worker-small"])], &["group 'b'", "'worker-small'", "already in group 'a'"]),
            ("role twice in one group", vec![g("a", &["worker-small", "worker-small"])], &["group 'a'", "listed twice"]),
            ("too small", vec![Group { shared_chars: 1_023, ..g("a", &["worker-small"]) }], &["shared_chars", "1024", "262144", "1023"]),
            ("too big", vec![Group { shared_chars: 262_145, ..g("a", &["worker-small"]) }], &["shared_chars", "262145"]),
            ("push value", vec![Group { push: "loud".into(), ..g("a", &["worker-small"]) }], &["push", "none, urgent, all", "'loud'"]),
        ];
        for (what, groups, words) in cases {
            let p = Pattern { groups, ..Pattern::builtin() };
            let errs = p.validate().expect_err(what);
            let all = errs.join(" | ");
            for w in words {
                assert!(all.contains(w), "{what}: '{w}' missing in {all}");
            }
            // the same errors through the TOML the architect writes (`mantra_write_pattern`)
            assert!(Pattern::from_toml(&p.to_toml()).is_err(), "{what}");
        }
        // every edge that is fine
        for (shared_chars, push) in [(1_024, "none"), (262_144, "all"), (16_384, "urgent")] {
            let p = Pattern { groups: vec![Group { shared_chars, push: push.into(), ..g("a", &["worker-small", "planner"]) }, g("b", &["worker-big"])], ..Pattern::builtin() };
            assert!(p.validate().is_ok(), "{shared_chars} {push}: {:?}", p.validate());
        }
    }

    /// A pattern file from before groups has none, and writes none back: its TOML is byte for byte
    /// what it was. A group written by hand with only a name and roles gets the defaults.
    #[test]
    fn patterns_without_groups_round_trip_unchanged_and_groups_take_defaults() {
        for p in all_builtins() {
            // the built-ins have `builders` now; a file written before groups is one without
            let old = Pattern { groups: vec![], ..p };
            let toml = old.to_toml();
            assert!(!toml.contains("groups"), "{}: nothing new in an old pattern's file", old.name);
            let back = Pattern::from_toml(&toml).unwrap();
            assert_eq!(back, old);
            assert_eq!(back.to_toml(), toml, "{}", old.name);
        }
        let hand = format!("{}\n[[groups]]\nname = \"builders\"\nroles = [\"worker-small\"]\n", Pattern { groups: vec![], ..Pattern::builtin() }.to_toml());
        let p = Pattern::from_toml(&hand).unwrap();
        assert_eq!(p.groups, vec![Group { name: "builders".into(), roles: vec!["worker-small".into()], ..Group::default() }]);
        assert_eq!((p.groups[0].shared_chars, p.groups[0].push.as_str(), p.groups[0].auto), (16_384, "urgent", true));
        assert_eq!(Pattern::from_toml(&p.to_toml()).unwrap(), p, "a group round-trips");
    }

    /// A role's `mcp` round-trips through the pattern's TOML; a pattern from before it (every
    /// built-in) loads with none on every role and writes no `mcp` key back.
    #[test]
    fn role_mcp_round_trips_and_old_patterns_are_unchanged() {
        for p in all_builtins() {
            assert!(p.roles.values().all(|r| r.mcp.is_empty()), "{}", p.name);
            assert!(!p.to_toml().lines().any(|l| l.trim_start().starts_with("mcp =")), "{}: nothing new in an old pattern's file", p.name);
        }
        let mut p = Pattern::builtin();
        let first = p.roles.keys().next().cloned().unwrap();
        p.roles.get_mut(&first).unwrap().mcp = vec!["blender".into(), "*".into()];
        let toml = p.to_toml();
        assert!(toml.lines().any(|l| l.starts_with("mcp = [")), "{toml}");
        let back = Pattern::from_toml(&toml).unwrap();
        assert_eq!(back.roles[&first].mcp, vec!["blender".to_string(), "*".into()]);
        assert_eq!(back, p);
        assert_eq!(Pattern::from_toml(&back.to_toml()).unwrap(), p, "and again");
        assert!(back.roles.iter().filter(|(n, _)| **n != first).all(|(_, r)| r.mcp.is_empty()), "the other roles keep none");
    }

    #[test]
    fn parallel_counts_are_1_to_128() {
        assert_eq!(parse_parallel("1"), Ok(1));
        assert_eq!(parse_parallel(" 24 "), Ok(24));
        assert_eq!(parse_parallel("128"), Ok(128));
        for bad in ["0", "129", "-3", "eight", ""] {
            let e = parse_parallel(bad).unwrap_err();
            assert!(e.contains("from 1 to 128"), "{bad}: {e}");
        }
    }
}
