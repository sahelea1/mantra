//! The plan is a validated contract the planner submits through a tool (never a free-form file).

use super::pattern::Pattern;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct Plan {
    pub title: String,
    pub summary: String,
    pub orchestrator_brief: String,
    pub phases: Vec<Phase>,
    pub final_checks: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct Phase {
    pub id: String,
    pub name: String,
    pub goal: String,
    pub tasks: Vec<Task>,
    pub gate: Gate,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct Task {
    pub id: String,
    pub title: String,
    pub role: String,
    pub prompt: String,
    pub scope: Vec<String>,
    pub acceptance: String,
    pub effort: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct Gate {
    pub checks: Vec<String>,
    pub focus: String,
    pub criteria: String,
}

/// The two halves of a rejection for a plan that arrived as text: what happened, and the one
/// thing that fixes it for good.
const AS_STRING: &str = "`plan` arrived as a string of JSON text, and that text is not valid JSON";
const AS_OBJECT: &str = "Pass `plan` as a JSON object in the call's arguments, not as a string: nothing in it needs escaping then.";

impl Plan {
    /// Parse leniently: accepts the plan object directly or wrapped as {"plan": {...}} or as a JSON
    /// string, and a nested list or object sent as its JSON text (`"phases": "[{…}]"`) — a model
    /// quirk that would otherwise be refused the same way every round. Bija already shapes its
    /// calls to the schema; this covers the other backends.
    pub fn from_value(v: &Value) -> Result<Plan, String> {
        let v = match v.get("plan") {
            Some(p) => p.clone(),
            None => v.clone(),
        };
        // A model that stringifies the whole plan has to escape every quote inside it twice — the
        // commands in gate `checks` break that at a few thousand characters, and a message that
        // only quotes the parser's column sends the model back to fiddle with the string. Say what
        // to do instead: the object itself, which needs no escaping at all.
        let mut v = match &v {
            Value::String(s) => serde_json::from_str::<Value>(s).map_err(|e| format!("{AS_STRING} ({e}). {AS_OBJECT}"))?,
            _ => v,
        };
        unstring(&mut v, "phases", Value::is_array);
        unstring(&mut v, "final_checks", Value::is_array);
        if let Some(Value::Array(phases)) = v.get_mut("phases") {
            for ph in phases.iter_mut() {
                unstring(ph, "tasks", Value::is_array);
                unstring(ph, "gate", Value::is_object);
                if let Some(gate) = ph.get_mut("gate") {
                    unstring(gate, "checks", Value::is_array);
                }
                if let Some(Value::Array(tasks)) = ph.get_mut("tasks") {
                    for t in tasks.iter_mut() {
                        unstring(t, "scope", Value::is_array);
                    }
                }
            }
        }
        serde_json::from_value::<Plan>(v).map_err(|e| format!("plan does not match the schema: {e}. {AS_OBJECT}"))
    }

    pub fn validate(&self, pattern: &Pattern) -> Result<(), Vec<String>> {
        if pattern.settings.planner_loop {
            return match self.phases.as_slice() {
                [phase] if phase.tasks.is_empty() && phase.gate == Gate::default() && self.final_checks.is_empty() => Ok(()),
                _ => Err(vec!["the Akasha loop needs one phase with no worker tasks, gate or automatic checks".into()]),
            };
        }
        let mut errs = vec![];
        if self.phases.is_empty() {
            errs.push("plan needs at least one phase".into());
        }
        let workers = pattern.worker_roles();
        let mut ids = HashSet::new();
        let mut phase_ids = HashSet::new();
        for (pi, ph) in self.phases.iter().enumerate() {
            let pname = if ph.name.is_empty() { format!("phase {}", pi + 1) } else { ph.name.clone() };
            if !ph.id.is_empty() && !phase_ids.insert(ph.id.clone()) {
                errs.push(format!("duplicate phase id '{}'", ph.id));
            }
            if ph.tasks.is_empty() {
                errs.push(format!("{pname}: has no tasks"));
            }
            // A phase that uses a class of its own (worker-swarm) may be wider than the setting.
            let limit = pattern.phase_task_limit(ph.tasks.iter().map(|t| t.role.as_str()));
            if ph.tasks.len() > limit {
                let cap = pattern.settings.max_tasks_per_phase;
                let why = if limit == cap { format!("max_tasks_per_phase = {cap}") } else { format!("the {limit} a phase with its roles may hold (max_tasks_per_phase = {cap}, or twice a role's own parallel)") };
                errs.push(format!("{pname}: {} tasks exceeds {why}", ph.tasks.len()));
            }
            for t in &ph.tasks {
                if t.id.trim().is_empty() {
                    errs.push(format!("{pname}: a task has no id"));
                } else if !ids.insert(t.id.clone()) {
                    errs.push(format!("duplicate task id '{}' (ids must be unique across the whole plan)", t.id));
                } else if !t.id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
                    errs.push(format!("task id '{}' may only contain letters, digits, - and _", t.id));
                }
                if super::run::is_fix_id(&t.id) {
                    errs.push(format!("task id '{}' is reserved for Mantra's fix workers — choose another id", t.id));
                }
                if !workers.contains(&t.role) {
                    errs.push(format!("task '{}': role '{}' is not a worker role (use one of: {})", t.id, t.role, workers.join(", ")));
                }
                if t.prompt.trim().len() < 10 {
                    errs.push(format!("task '{}': prompt is missing or too short", t.id));
                }
            }
            // parallel tasks must not claim overlapping scopes
            for (i, a) in ph.tasks.iter().enumerate() {
                for b in ph.tasks.iter().skip(i + 1) {
                    for sa in &a.scope {
                        for sb in &b.scope {
                            if scopes_overlap(sa, sb) {
                                errs.push(format!("{pname}: tasks '{}' and '{}' both claim '{}'/'{}' — parallel tasks need disjoint scopes (move one to a later phase)", a.id, b.id, sa, sb));
                            }
                        }
                    }
                }
            }
        }
        if errs.is_empty() {
            Ok(())
        } else {
            Err(errs)
        }
    }

    /// Fill in missing ids/names so the rest of the engine can rely on them.
    pub fn normalize(&mut self) {
        for (i, ph) in self.phases.iter_mut().enumerate() {
            if ph.id.is_empty() {
                ph.id = format!("p{}", i + 1);
            }
            if ph.name.is_empty() {
                ph.name = format!("Phase {}", i + 1);
            }
            for t in &mut ph.tasks {
                if t.title.is_empty() {
                    t.title = crate::util::trunc(t.prompt.lines().next().unwrap_or(&t.id), 40);
                }
            }
        }
    }

    pub fn to_markdown(&self) -> String {
        let mut s = format!("# {}\n\n{}\n\n", self.title, self.summary);
        for (i, ph) in self.phases.iter().enumerate() {
            s.push_str(&format!("## Phase {} — {}\n\n{}\n\n", i + 1, ph.name, ph.goal));
            for t in &ph.tasks {
                s.push_str(&format!("### `{}` {} ({})\n\n", t.id, t.title, t.role));
                if !t.scope.is_empty() {
                    s.push_str(&format!("Scope: `{}`\n\n", t.scope.join("`, `")));
                }
                s.push_str(&format!("{}\n\n", t.prompt.trim()));
                if !t.acceptance.is_empty() {
                    s.push_str(&format!("Acceptance: {}\n\n", t.acceptance));
                }
            }
            if ph.gate == Gate::default() {
                continue;
            }
            s.push_str("Gate:\n");
            for c in &ph.gate.checks {
                s.push_str(&format!("- `{c}`\n"));
            }
            if !ph.gate.criteria.is_empty() {
                s.push_str(&format!("- criteria: {}\n", ph.gate.criteria));
            }
            if !ph.gate.focus.is_empty() {
                s.push_str(&format!("- QA focus: {}\n", ph.gate.focus));
            }
            s.push('\n');
        }
        if !self.final_checks.is_empty() {
            s.push_str("## Final checks\n\n");
            for c in &self.final_checks {
                s.push_str(&format!("- `{c}`\n"));
            }
        }
        s
    }
}

pub(crate) fn scopes_overlap(a: &str, b: &str) -> bool {
    let root = |s: &str| -> String {
        s.split('/').take_while(|seg| !seg.contains('*') && !seg.contains('?')).collect::<Vec<_>>().join("/")
    };
    let (ra, rb) = (root(a), root(b));
    if a == b {
        return true;
    }
    // A recursive directory claim includes every more-specific rooted glob beneath it,
    // e.g. src/** and src/auth/**. Comparing only equal roots missed this overlap.
    let below = |parent: &str, child: &str| child == parent || child.strip_prefix(parent).is_some_and(|rest| rest.starts_with('/'));
    if a == "**" || b == "**" || a.strip_suffix("/**").is_some_and(|parent| below(parent, &rb)) || b.strip_suffix("/**").is_some_and(|parent| below(parent, &ra)) {
        return true;
    }
    // a concrete file claimed by both, or one glob fully inside the other's concrete root
    let a_concrete = !a.contains('*');
    let b_concrete = !b.contains('*');
    if a_concrete && crate::util::in_scope(&[b.to_string()], a) {
        return true;
    }
    if b_concrete && crate::util::in_scope(&[a.to_string()], b) {
        return true;
    }
    // two globs rooted at the same directory, e.g. src/** and src/**/*.rs
    !ra.is_empty() && ra == rb && (a.ends_with("**") || b.ends_with("**"))
}

/// `obj[key]` sent as JSON text is replaced by what it parses to, when that is the wanted kind;
/// anything else is left for serde's own error.
fn unstring(obj: &mut Value, key: &str, wanted: fn(&Value) -> bool) {
    let Some(slot) = obj.get_mut(key) else { return };
    let Value::String(text) = &*slot else { return };
    if let Some(parsed) = serde_json::from_str::<Value>(text).ok().filter(wanted) {
        *slot = parsed;
    }
}

/// Apply `ops` to a copy of `plan`. The first operation that cannot be applied stops it with a
/// message that names the operation's position and what is wrong.
pub fn apply_ops(plan: &Plan, ops: &Value) -> Result<Plan, String> {
    let ops = match ops {
        Value::String(s) => serde_json::from_str::<Value>(s).unwrap_or(Value::Null),
        v => v.clone(),
    };
    let Some(ops) = ops.as_array() else { return Err("`ops` must be a list of operations".into()) };
    if ops.is_empty() {
        return Err("`ops` is empty".into());
    }
    // overlay the given fields on a serialisable struct
    fn overlay<T: Serialize + serde::de::DeserializeOwned>(at: &str, target: &T, value: &Value, keys: &[&str]) -> Result<T, String> {
        let mut cur = serde_json::to_value(target).map_err(|e| e.to_string())?;
        for (k, v) in value.as_object().into_iter().flatten() {
            if !keys.contains(&k.as_str()) {
                if k == "id" {
                    return Err(format!("{at}: a task's id cannot change — remove it and add a new one"));
                }
                return Err(format!("{at}: unknown field '{k}' (allowed: {})", keys.join(", ")));
            }
            let mut v = v.clone();
            if let Value::String(s) = &v {
                if let Some(p) = serde_json::from_str::<Value>(s).ok().filter(Value::is_array) {
                    v = p;
                }
            }
            cur[k] = v;
        }
        serde_json::from_value(cur).map_err(|e| format!("{at}: {e}"))
    }
    let mut plan = plan.clone();
    for (i, op) in ops.iter().enumerate() {
        let name = op.get("op").and_then(Value::as_str).unwrap_or("").to_string();
        let at = format!("op {} ({name})", i + 1);
        let sget = |k: &str| op.get(k).and_then(Value::as_str).map(str::to_string).or_else(|| op.get(k).and_then(Value::as_i64).map(|n| n.to_string()));
        let value = match op.get("value") {
            Some(Value::String(s)) => match serde_json::from_str::<Value>(s) {
                Ok(v) if v.is_object() => v,
                _ => Value::String(s.clone()),
            },
            Some(v) => v.clone(),
            None => Value::Null,
        };
        let phase_idx = |plan: &Plan| -> Result<usize, String> {
            let x = sget("phase").ok_or_else(|| format!("{at}: `phase` is required"))?;
            plan.phases
                .iter()
                .position(|p| p.id == x)
                .or_else(|| x.parse::<usize>().ok().filter(|n| *n >= 1 && *n <= plan.phases.len()).map(|n| n - 1))
                .ok_or_else(|| format!("{at}: no phase '{x}' (phases: {})", plan.phases.iter().map(|p| p.id.clone()).collect::<Vec<_>>().join(", ")))
        };
        let task_pos = |plan: &Plan| -> Result<(usize, usize), String> {
            let x = sget("task").ok_or_else(|| format!("{at}: `task` is required"))?;
            plan.phases
                .iter()
                .enumerate()
                .find_map(|(pi, p)| p.tasks.iter().position(|t| t.id == x).map(|ti| (pi, ti)))
                .ok_or_else(|| format!("{at}: no task '{x}'"))
        };
        let check_idx = |plan: &Plan, pi: usize| -> Result<usize, String> {
            let n = plan.phases[pi].gate.checks.len();
            let idx = sget("index").and_then(|s| s.parse::<usize>().ok());
            match idx {
                Some(k) if k >= 1 && k <= n => Ok(k - 1),
                _ => Err(format!("{at}: phase '{}' has {n} check(s)", plan.phases[pi].id)),
            }
        };
        let need_obj = |keys: &[&str]| -> Result<Value, String> {
            if !value.is_object() {
                return Err(format!("{at}: value must be an object with any of {}", keys.join(", ")));
            }
            Ok(value.clone())
        };
        let need_str = || -> Result<String, String> { value.as_str().map(str::to_string).ok_or_else(|| format!("{at}: value must be a string")) };
        match name.as_str() {
            "set_task" => {
                let (pi, ti) = task_pos(&plan)?;
                let keys = ["title", "role", "prompt", "scope", "acceptance", "effort"];
                let v = need_obj(&keys)?;
                plan.phases[pi].tasks[ti] = overlay(&at, &plan.phases[pi].tasks[ti], &v, &keys)?;
            }
            "add_task" => {
                let pi = phase_idx(&plan)?;
                let mut v = need_obj(&["id", "role", "prompt"])?;
                if let Some(p) = v.get("scope").and_then(Value::as_str).and_then(|s| serde_json::from_str::<Value>(s).ok()).filter(Value::is_array) {
                    v["scope"] = p;
                }
                let t: Task = serde_json::from_value(v).map_err(|e| format!("{at}: value is not a task: {e}"))?;
                plan.phases[pi].tasks.push(t);
            }
            "remove_task" => {
                let (pi, ti) = task_pos(&plan)?;
                plan.phases[pi].tasks.remove(ti);
            }
            "set_gate" => {
                let pi = phase_idx(&plan)?;
                let keys = ["checks", "focus", "criteria"];
                let v = need_obj(&keys)?;
                plan.phases[pi].gate = overlay(&at, &plan.phases[pi].gate, &v, &keys)?;
            }
            "set_check" => {
                let pi = phase_idx(&plan)?;
                let k = check_idx(&plan, pi)?;
                plan.phases[pi].gate.checks[k] = need_str()?;
            }
            "add_check" => {
                let pi = phase_idx(&plan)?;
                plan.phases[pi].gate.checks.push(need_str()?);
            }
            "remove_check" => {
                let pi = phase_idx(&plan)?;
                let k = check_idx(&plan, pi)?;
                plan.phases[pi].gate.checks.remove(k);
            }
            "set_phase" => {
                let pi = phase_idx(&plan)?;
                let keys = ["name", "goal"];
                let v = need_obj(&keys)?;
                plan.phases[pi] = overlay(&at, &plan.phases[pi], &v, &keys)?;
            }
            "set_plan" => {
                let keys = ["title", "summary", "orchestrator_brief", "final_checks"];
                let v = need_obj(&keys)?;
                plan = overlay(&at, &plan, &v, &keys)?;
            }
            other => return Err(format!("{at}: unknown operation '{other}'")),
        }
    }
    plan.normalize();
    Ok(plan)
}

/// JSON schema given to the planner's tools.
pub fn plan_schema(worker_roles: &[String]) -> Value {
    json!({
        "type": "object",
        "properties": {
            "plan": {
                "type": "object",
                "description": "The full phased plan.",
                "properties": {
                    "title": {"type": "string"},
                    "summary": {"type": "string", "description": "2-4 sentences: what will be built and how."},
                    "orchestrator_brief": {"type": "string", "description": "Instructions for the orchestrator on supervising this project."},
                    "phases": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": {"type": "string"},
                                "name": {"type": "string"},
                                "goal": {"type": "string"},
                                "tasks": {
                                    "type": "array",
                                    "description": "Tasks in one phase run in parallel on isolated copies of the repo.",
                                    "items": {
                                        "type": "object",
                                        "properties": {
                                            "id": {"type": "string", "description": "unique across the plan, e.g. p1-auth"},
                                            "title": {"type": "string"},
                                            "role": {"type": "string", "enum": worker_roles},
                                            "prompt": {"type": "string", "description": "Self-contained instructions for the worker."},
                                            "scope": {"type": "array", "items": {"type": "string"}, "description": "Path globs this task may change, e.g. src/auth/**"},
                                            "acceptance": {"type": "string"},
                                            "effort": {"type": "string", "description": "optional reasoning effort override: low|medium|high|xhigh|max"}
                                        },
                                        "required": ["id", "title", "role", "prompt"]
                                    }
                                },
                                "gate": {
                                    "type": "object",
                                    "properties": {
                                        "checks": {"type": "array", "items": {"type": "string"}, "description": "Shell commands that must exit 0, e.g. cargo test"},
                                        "focus": {"type": "string"},
                                        "criteria": {"type": "string"}
                                    }
                                }
                            },
                            "required": ["name", "tasks"]
                        }
                    },
                    "final_checks": {"type": "array", "items": {"type": "string"}}
                },
                "required": ["title", "summary", "phases"]
            }
        },
        "required": ["plan"]
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sample() -> Plan {
        serde_json::from_value(json!({
            "title": "T", "summary": "S",
            "phases": [{"name": "A", "tasks": [
                {"id": "a1", "title": "x", "role": "worker-small", "prompt": "do the first thing well", "scope": ["src/auth/**"]},
                {"id": "a2", "title": "y", "role": "worker-big", "prompt": "do the second thing well", "scope": ["src/orders/**"]}
            ], "gate": {"checks": ["true"]}}]
        }))
        .unwrap()
    }
    #[test]
    fn recursive_directory_globs_conflict_with_nested_globs() {
        assert!(scopes_overlap("src/**", "src/auth/**"));
        assert!(scopes_overlap("src/auth/**", "src/**"));
        assert!(scopes_overlap("**", "src/auth/**"));
        assert!(!scopes_overlap("src/**", "src2/auth/**"));
        assert!(!scopes_overlap("src/auth/**", "src/orders/**"));
        let mut plan = sample();
        plan.phases[0].tasks[0].scope = vec!["src/**".into()];
        assert!(plan.validate(&Pattern::builtin()).is_err());
    }

    #[test]
    fn validates() {
        let p = Pattern::builtin();
        assert!(sample().validate(&p).is_ok());
        let mut bad = sample();
        bad.phases[0].tasks[1].scope = vec!["src/auth/login.rs".into()];
        assert!(bad.validate(&p).is_err());
        let mut bad2 = sample();
        bad2.phases[0].tasks[1].role = "qa".into();
        assert!(bad2.validate(&p).is_err());
    }
    /// `n` tasks of `role` in one phase, disjoint scopes.
    fn phase_of(n: usize, role: &str) -> Plan {
        let tasks: Vec<Value> = (0..n).map(|i| json!({"id": format!("t{i}"), "role": role, "prompt": "do one small thing well", "scope": [format!("src/m{i}/**")]})).collect();
        serde_json::from_value(json!({"title": "T", "phases": [{"name": "A", "tasks": tasks, "gate": {"checks": ["true"]}}]})).unwrap()
    }

    /// A phase of the shared pool's roles holds `max_tasks_per_phase` tasks; one that uses a class
    /// of its own holds twice that class's cap when that is more — a 32-task swarm phase fits the
    /// default pattern as it ships, a 33rd task does not, and mixing in a big task changes nothing.
    #[test]
    fn a_swarm_phase_may_hold_twice_its_class_cap() {
        let p = Pattern::builtin();
        assert!(phase_of(8, "worker-small").validate(&p).is_ok());
        let errs = phase_of(9, "worker-small").validate(&p).unwrap_err();
        assert_eq!(errs, vec!["A: 9 tasks exceeds max_tasks_per_phase = 8".to_string()]);
        assert!(phase_of(32, "worker-swarm").validate(&p).is_ok(), "{:?}", phase_of(32, "worker-swarm").validate(&p));
        let errs = phase_of(33, "worker-swarm").validate(&p).unwrap_err();
        assert!(errs[0].contains("33 tasks exceeds the 32 a phase with its roles may hold") && errs[0].contains("max_tasks_per_phase = 8"), "{errs:?}");
        let mut mixed = phase_of(31, "worker-swarm");
        mixed.phases[0].tasks[0].role = "worker-big".into();
        assert!(mixed.validate(&p).is_ok(), "{:?}", mixed.validate(&p));
    }

    /// `--parallel N` must be reachable: phases run one after another, so the per-phase task cap
    /// is the ceiling on what runs at once, and the run's copy raises it to N for the shared pool
    /// (and the swarm's own cap with it); a smaller N leaves the cap.
    #[test]
    fn a_runs_parallel_override_lets_a_phase_hold_that_many_tasks() {
        let mut p = Pattern::builtin();
        assert!(phase_of(64, "worker-small").validate(&p).is_err(), "the pattern's own cap is 8");
        p.set_parallel(64);
        assert_eq!((p.settings.max_parallel, p.settings.max_tasks_per_phase), (64, 64));
        assert!(phase_of(64, "worker-small").validate(&p).is_ok(), "{:?}", phase_of(64, "worker-small").validate(&p));
        assert!(phase_of(65, "worker-small").validate(&p).unwrap_err()[0].contains("exceeds max_tasks_per_phase = 64"));
        assert!(phase_of(128, "worker-swarm").validate(&p).is_ok(), "twice the swarm's 64");
        let mut few = Pattern::builtin();
        few.set_parallel(4);
        assert_eq!((few.settings.max_parallel, few.settings.max_tasks_per_phase), (4, 8), "fewer at once, same plan");
        assert!(phase_of(9, "worker-swarm").validate(&few).is_err(), "4 at once: twice that is below the setting's 8");
    }
    #[test]
    fn lenient_parse() {
        let v = json!({"plan": serde_json::to_string(&sample()).unwrap()});
        assert!(Plan::from_value(&v).is_ok());
    }
    /// A planner that stringified its plan and double-escaped the quotes inside a gate command
    /// (what a live run looped on for ten minutes) is told to send the object, not where the
    /// string broke.
    #[test]
    fn a_broken_plan_string_is_told_to_send_the_object() {
        let text = r#"{"title": "T", "phases": [{"gate": {"checks": ["python3 -c \\"import x\\""]}, "tasks": []}]}"#;
        let err = Plan::from_value(&json!({"plan": text})).expect_err("the inner quote ends the string early");
        assert!(err.starts_with("`plan` arrived as a string of JSON text"), "{err}");
        assert!(err.contains("Pass `plan` as a JSON object"), "{err}");
        let err = Plan::from_value(&json!({"plan": {"title": "T", "phases": "not json"}})).expect_err("prose");
        assert!(err.contains("does not match the schema") && err.contains("Pass `plan` as a JSON object"), "{err}");
    }
    #[test]
    fn nested_lists_sent_as_json_text_are_read() {
        let mut plan = serde_json::to_value(sample()).unwrap();
        let phase = &mut plan["phases"][0];
        phase["gate"]["checks"] = json!(phase["gate"]["checks"].to_string());
        phase["tasks"] = json!(phase["tasks"].to_string());
        plan["phases"] = json!(plan["phases"].to_string());
        plan["final_checks"] = json!("[\"cargo test\"]");
        let parsed = Plan::from_value(&json!({"plan": plan})).expect("stringified lists parse");
        assert_eq!(parsed.phases, sample().phases);
        assert_eq!(parsed.final_checks, vec!["cargo test".to_string()]);
        let err = Plan::from_value(&json!({"plan": {"title": "T", "phases": "not json"}})).expect_err("prose is still refused");
        assert!(err.contains("does not match the schema"), "{err}");
    }

    #[test]
    fn apply_ops_covers_every_operation() {
        let mut p = sample();
        p.normalize();
        let ops = json!([
            {"op": "set_task", "task": "a1", "value": {"prompt": "do the first thing better", "scope": ["src/x/**"]}},
            {"op": "add_task", "phase": "1", "value": {"id": "a3", "role": "worker-small", "prompt": "a third thing to do"}},
            {"op": "remove_task", "task": "a2"},
            {"op": "add_check", "phase": "p1", "value": "echo hi"},
            {"op": "set_check", "phase": "p1", "index": 1, "value": "false"},
            {"op": "set_gate", "phase": "p1", "value": {"focus": "look here"}},
            {"op": "set_phase", "phase": "p1", "value": "{\"goal\": \"g\"}"},
            {"op": "set_plan", "value": {"title": "New", "final_checks": ["true"]}},
        ]);
        let q = apply_ops(&p, &ops).unwrap();
        assert_eq!(q.title, "New");
        assert_eq!(q.final_checks, vec!["true"]);
        let ph = &q.phases[0];
        assert_eq!(ph.goal, "g");
        assert_eq!(ph.gate.focus, "look here");
        assert_eq!(ph.gate.checks, vec!["false", "echo hi"]);
        assert_eq!(ph.tasks.iter().map(|t| t.id.as_str()).collect::<Vec<_>>(), vec!["a1", "a3"]);
        assert_eq!(ph.tasks[0].scope, vec!["src/x/**"]);
        let q = apply_ops(&q, &json!([{"op": "remove_check", "phase": "p1", "index": 2}])).unwrap();
        assert_eq!(q.phases[0].gate.checks, vec!["false"]);
        assert_eq!(p.title, "T");
    }

    #[test]
    fn apply_ops_errors_name_the_position() {
        let mut p = sample();
        p.normalize();
        let e = apply_ops(&p, &json!([{"op": "add_check", "phase": "p1", "value": "x"}, {"op": "set_gate", "phase": "zz", "value": {}}])).unwrap_err();
        assert!(e.contains("op 2 (set_gate): no phase 'zz' (phases: p1)"), "{e}");
        let e = apply_ops(&p, &json!([{"op": "remove_task", "task": "nope"}])).unwrap_err();
        assert!(e.contains("op 1 (remove_task): no task 'nope'"), "{e}");
        let e = apply_ops(&p, &json!([{"op": "set_check", "phase": "p1", "index": 3, "value": "x"}])).unwrap_err();
        assert!(e.contains("phase 'p1' has 1 check(s)"), "{e}");
        let e = apply_ops(&p, &json!([{"op": "set_check", "phase": "p1", "index": 1, "value": {"a": 1}}])).unwrap_err();
        assert!(e.contains("value must be a string"), "{e}");
        let e = apply_ops(&p, &json!([{"op": "set_task", "task": "a1", "value": {"id": "new"}}])).unwrap_err();
        assert!(e.contains("a task's id cannot change"), "{e}");
    }
}
