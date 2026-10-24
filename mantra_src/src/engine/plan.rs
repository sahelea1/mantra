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
    if let Some(parsed) = serde_json::from_str::<Value>(text).ok().filter(wanted) {
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
}
