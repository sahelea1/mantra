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
    /// Parse leniently: accepts the plan object directly or wrapped as {"plan": {...}} or as a JSON string.
    pub fn from_value(v: &Value) -> Result<Plan, String> {
        let v = match v.get("plan") {
            Some(p) => p.clone(),
            None => v.clone(),
        };
        let v = match &v {
            Value::String(s) => serde_json::from_str::<Value>(s).map_err(|e| format!("plan is not valid JSON: {e}"))?,
            _ => v,
        };
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
    #[test]
    fn lenient_parse() {
        let v = json!({"plan": serde_json::to_string(&sample()).unwrap()});
        assert!(Plan::from_value(&v).is_ok());
    }
}
