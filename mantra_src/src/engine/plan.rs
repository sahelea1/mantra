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
        let mut v = match &v {
            Value::String(s) => serde_json::from_str::<Value>(s).map_err(|e| format!("plan is not valid JSON: {e}"))?,
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
        serde_json::from_value::<Plan>(v).map_err(|e| format!("plan does not match the schema: {e}"))
    }

    pub fn validate(&self, pattern: &Pattern) -> Result<(), Vec<String>> {
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
            if ph.tasks.len() > pattern.settings.max_tasks_per_phase {
                errs.push(format!("{pname}: {} tasks exceeds max_tasks_per_phase = {}", ph.tasks.len(), pattern.settings.max_tasks_per_phase));
            }
            for t in &ph.tasks {
                if t.id.trim().is_empty() {
                    errs.push(format!("{pname}: a task has no id"));
                } else if !ids.insert(t.id.clone()) {
                    errs.push(format!("duplicate task id '{}' (ids must be unique across the whole plan)", t.id));
                } else if !t.id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
                    errs.push(format!("task id '{}' may only contain letters, digits, - and _", t.id));
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

fn scopes_overlap(a: &str, b: &str) -> bool {
    let root = |s: &str| -> String {
        s.split('/').take_while(|seg| !seg.contains('*') && !seg.contains('?')).collect::<Vec<_>>().join("/")
    };
    let (ra, rb) = (root(a), root(b));
    if a == b {
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
    if let Some(parsed) = serde_json::from_str::<Value>(text).ok().filter(|p| wanted(p)) {
        *slot = parsed;
    }
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
    /// `--swarm N` / `--parallel N` must be reachable: phases run one after another, so the per
    /// phase task cap is the ceiling on what runs at once, and the run's copy raises it to N. A
    /// `--swarm 64` planner that cuts 64 tasks into a phase is accepted; a smaller N leaves the cap.
    #[test]
    fn a_runs_parallel_override_lets_a_phase_hold_that_many_tasks() {
        let phase_of = |n: usize| -> Plan {
            let tasks: Vec<Value> = (0..n).map(|i| json!({"id": format!("t{i}"), "role": "worker-small", "prompt": "do one small thing well", "scope": [format!("src/m{i}/**")]})).collect();
            serde_json::from_value(json!({"title": "T", "phases": [{"name": "A", "tasks": tasks, "gate": {"checks": ["true"]}}]})).unwrap()
        };
        let mut swarm = Pattern::builtin_swarm();
        assert!(phase_of(64).validate(&swarm).is_err(), "the pattern's own cap is 32");
        swarm.set_parallel(64);
        assert_eq!((swarm.settings.max_parallel, swarm.settings.max_tasks_per_phase), (64, 64));
        assert!(phase_of(64).validate(&swarm).is_ok(), "{:?}", phase_of(64).validate(&swarm));
        assert!(phase_of(65).validate(&swarm).unwrap_err()[0].contains("exceeds max_tasks_per_phase = 64"));
        let mut few = Pattern::builtin_swarm();
        few.set_parallel(4);
        assert_eq!((few.settings.max_parallel, few.settings.max_tasks_per_phase), (4, 32), "fewer at once, same plan");
        let mut default = Pattern::builtin();
        default.set_parallel(16);
        assert_eq!((default.settings.max_parallel, default.settings.max_tasks_per_phase), (16, 16));
    }
    #[test]
    fn lenient_parse() {
        let v = json!({"plan": serde_json::to_string(&sample()).unwrap()});
        assert!(Plan::from_value(&v).is_ok());
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
}
