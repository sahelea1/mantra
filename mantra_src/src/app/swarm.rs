//! Scoped, bounded Akasha delegation. A group belongs to one live parent, and its children
//! share a small notebook without receiving delegation tools themselves.

use super::*;
use crate::engine::akasha::Block;

const MAX_MEMBERS: usize = 8;
const MAX_LIVE_MEMBERS: usize = 16;
const DEFAULT_CHARS: usize = 8_192;
const MAX_CHARS: usize = 16_384;
const MEMBER_PROTOCOL: &str = "\n## Shared Akasha notes\nYou and your parent share a bounded notebook. Read it with mantra_akasha_read at the start, at useful milestones, and before reporting. Add short facts, interfaces and decisions with mantra_akasha_note. The urgent flag marks priority in the notebook; peers see it when they next read notes or receive a follow-up. Your parent will review all results.\n";

pub(crate) struct SwarmGroup {
    pub owner: AgentId,
    pub name: String,
    pub notes: Block,
    pub children: Vec<AgentId>,
}

struct Task {
    text: String,
    model: String,
    effort: Option<String>,
}

impl App {
    /// Return true for any swarm call, including unauthorized or stale calls.
    pub(super) fn swarm_tool_call(&mut self, caller: AgentId, request: Value, tool: &str, args: &Value) -> bool {
        let is_swarm = matches!(tool, "mantra_swarm_start" | "mantra_swarm_status" | "mantra_swarm_wait" | "mantra_swarm_note" | "mantra_swarm_message" | "mantra_swarm_stop");
        let is_note = matches!(tool, "mantra_akasha_read" | "mantra_akasha_note" | "mantra_akasha_amend" | "mantra_akasha_forget");
        if !is_swarm && !is_note { return false; }
        if is_note && !self.swarm_owners.contains_key(&caller) && Some(caller) != self.solo {
            return false; // Existing pattern Akasha group remains the run engine's responsibility.
        }
        if tool == "mantra_swarm_wait" && self.swarm_authorized(caller) && self.swarm_pending(caller) {
            self.swarm_waiters.entry(caller).or_default().push(request);
            if let Some(a) = self.agents.get_mut(&caller) { a.swarm_waiting = true; a.activity = "waiting for Akasha swarm".into(); }
            return true;
        }
        let result = if is_note {
            self.swarm_note(caller, tool, args)
        } else if !self.swarm_authorized(caller) {
            ("Swarm spawning is available to Solo and pattern roles with can_spawn_swarm enabled. Swarm members cannot spawn another swarm.".into(), false)
        } else {
            match tool {
                "mantra_swarm_start" => self.swarm_start(caller, args),
                "mantra_swarm_status" => self.swarm_status(caller),
                "mantra_swarm_wait" => self.swarm_status(caller),
                "mantra_swarm_note" => self.swarm_note(caller, "mantra_akasha_note", args),
                "mantra_swarm_message" => self.swarm_message(caller, args),
                "mantra_swarm_stop" => self.swarm_stop(caller, args),
                _ => unreachable!(),
            }
        };
        self.respond_tool_text(caller, request, &result.0, result.1);
        true
    }

    fn swarm_authorized(&self, parent: AgentId) -> bool {
        if self.swarm_owners.contains_key(&parent) || !self.hub.has_agent(parent) || self.agents.get(&parent).is_none_or(|a| a.stopped_by_user || a.status == Status::Stopped) { return false; }
        if Some(parent) == self.solo { return true; }
        self.run.as_ref().filter(|r| r.is_active()).and_then(|r| r.role_name_of(parent).and_then(|name| r.pattern.role(&name).map(|role| role.can_spawn_swarm))).unwrap_or(false)
    }

    /// Starting and retrying members count too: a parent must never finish while their workspace
    /// can still be changed by a delegated process.
    pub(super) fn swarm_pending(&self, owner: AgentId) -> bool {
        self.swarms.get(&owner).is_some_and(|g| g.children.iter().any(|id| self.agents.get(id).is_some_and(|a| {
            self.hub.has_agent(*id) && (a.busy() || a.awaiting_start || matches!(a.status, Status::Starting | Status::Retrying(_)))
        })))
    }

    fn swarm_start(&mut self, owner: AgentId, args: &Value) -> (String, bool) {
        let Some(parent) = self.agents.get(&owner) else { return ("Parent agent is no longer available.".into(), false) };
        let Some(input) = args.get("tasks").and_then(Value::as_array) else { return ("Provide a tasks array of one to eight independent tasks.".into(), false) };
        let prior = self.swarms.get(&owner).map(|g| g.children.len()).unwrap_or(0);
        if input.is_empty() || prior + input.len() > MAX_MEMBERS {
            return (format!("A swarm holds one to {MAX_MEMBERS} members total; this group has {prior}. Stop the group to start a new one."), false);
        }
        let active = self.swarm_owners.keys().filter(|id| self.hub.has_agent(**id)).count();
        if active + input.len() > MAX_LIVE_MEMBERS {
            return (format!("At most {MAX_LIVE_MEMBERS} swarm members may have live processes at once; {active} are live."), false);
        }
        let cap = match args.get("shared_chars") {
            Some(v) => match v.as_u64() { Some(n) => n, None => return ("shared_chars must be an integer from 1024 to 16384.".into(), false) },
            None => DEFAULT_CHARS as u64,
        };
        if !(1_024..=MAX_CHARS as u64).contains(&cap) || (prior > 0 && args.get("shared_chars").is_some()) {
            return ("shared_chars must be 1024..16384, and can only be set when starting a group.".into(), false);
        }
        // Validate the entire batch before starting any process.
        let mut tasks = Vec::with_capacity(input.len());
        for (i, item) in input.iter().enumerate() {
            let task = item.get("task").and_then(Value::as_str).unwrap_or("").trim();
            let alias = item.get("model").and_then(Value::as_str).unwrap_or(&parent.model_alias).trim();
            if task.is_empty() || task.chars().count() > 12_000 {
                return (format!("Task {} needs a nonempty goal of at most 12000 characters; no members started.", i + 1), false);
            }
            if self.registry.get(alias).is_none() || (!self.demo && self.registry.alias_problem(alias).is_some()) {
                return (format!("Task {} uses unavailable model alias '{alias}'; no members started. Choose a configured /models alias.", i + 1), false);
            }
            let effort = item.get("effort").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
            tasks.push(Task { text: task.to_string(), model: alias.to_string(), effort });
        }
        let (cwd, approval, parent_model, sandbox) = (parent.cwd.clone(), parent.approval.clone(), parent.model_alias.clone(), parent.sandbox.clone());
        let in_run = Some(owner) != self.solo && self.in_run(owner);
        let role = self.run.as_ref().filter(|_| in_run).and_then(|r| r.role_name_of(owner).and_then(|name| r.pattern.role(&name).cloned()));
        let network = self.run.as_ref().filter(|_| in_run).map(|r| r.pattern.settings.network).unwrap_or(true);
        let writable_roots = self.run.as_ref().filter(|_| in_run).map(|r| r.pattern.settings.writable_roots.clone()).unwrap_or_default();
        let mcp_names = role.as_ref().map(|r| r.mcp.clone()).unwrap_or_else(|| self.solo_mcp.clone());
        let mcp = self.settings.mcp_for(&mcp_names);
        let group = self.swarms.entry(owner).or_insert_with(|| SwarmGroup {
            owner, name: format!("swarm-{owner}"), notes: Block::new(&format!("swarm-{owner}"), cap as usize), children: vec![],
        });
        let notes = group.notes.render();
        let mut ids = vec![];
        for task in tasks {
            let model = self.registry.resolve(&task.model);
            let backend = self.registry.backend_of(&model);
            let child = spawn_agent(&mut self.hub, &mut self.agents, &self.registry, AgentOpts {
                name: format!("akasha · {}", task.model), role: "swarm-member".into(), glyph: "◇".into(), color: "violet".into(),
                model_alias: task.model.clone(), effort: task.effort, cwd: cwd.clone(), approval: approval.clone(),
                sandbox: role_sandbox(&sandbox, backend),
                instructions: "You are one member of a bounded Akasha swarm. Complete your assigned task, coordinate through the shared notes, stay within the parent's working directory, and return a concise result. You cannot delegate or spawn other agents.\n".to_string() + MEMBER_PROTOCOL,
                tools: { let mut t = tools::swarm_member_note_tools(); if model.vision { t.extend(tools::vision_tools()); } t },
                extra_writable: vec![], network, writable_roots: writable_roots.clone(), context_override: None,
                resume_thread: None, thread_name: Some(format!("Akasha swarm · {}", task.model)), mcp: mcp.clone(),
            });
            self.swarms.get_mut(&owner).unwrap().children.push(child);
            self.swarm_owners.insert(child, owner);
            let brief = format!("Your task: {}\n\nShared Akasha notes (read again with mantra_akasha_read):\n{}", task.text, notes);
            prompt_agent(&self.hub, &mut self.agents, child, brief, true, Send::Auto);
            ids.push(format!("{child} ({})", task.model));
        }
        (format!("Akasha {} started {} member{}: {}. Use mantra_swarm_status for progress and shared notes, then mantra_swarm_wait before reporting completion. Parent model: {parent_model}.", self.swarms[&owner].name, ids.len(), if ids.len() == 1 { "" } else { "s" }, ids.join(", ")), true)
    }

    fn swarm_status(&self, owner: AgentId) -> (String, bool) {
        let Some(group) = self.swarms.get(&owner) else { return ("No Akasha swarm yet. Start one with mantra_swarm_start.".into(), true) };
        let mut lines = vec![format!("Akasha {} · {} members · {} notes", group.name, group.children.len(), group.notes.entries.len())];
        for id in &group.children {
            if let Some(a) = self.agents.get(id) {
                let answer = a.final_message.as_deref().unwrap_or("").trim();
                lines.push(format!("{id} · {} · {:?}{}", a.model_alias, a.status, if answer.is_empty() { String::new() } else { format!(" · {}", crate::util::trunc(answer, 500)) }));
            }
        }
        lines.push(group.notes.render());
        (lines.join("\n"), true)
    }

    fn swarm_message(&mut self, owner: AgentId, args: &Value) -> (String, bool) {
        let id = args.get("agent").and_then(Value::as_str).unwrap_or("").parse::<AgentId>().ok();
        let msg = args.get("message").and_then(Value::as_str).unwrap_or("").trim();
        let Some(id) = id.filter(|id| self.swarm_owners.get(id) == Some(&owner)) else { return ("Unknown member in your swarm.".into(), false) };
        if msg.is_empty() { return ("Provide a nonempty message.".into(), false) }
        if !self.hub.has_agent(id) { return (format!("Member {id} is stopped."), false) }
        let notes = self.swarms[&owner].notes.render();
        prompt_agent(&self.hub, &mut self.agents, id, format!("{msg}\n\nShared Akasha notes:\n{notes}"), true, Send::Auto);
        (format!("Message sent to member {id}."), true)
    }

    fn swarm_stop(&mut self, owner: AgentId, args: &Value) -> (String, bool) {
        if let Some(id) = args.get("agent").and_then(Value::as_str).filter(|s| !s.is_empty()) {
            let Some(child) = id.parse::<AgentId>().ok().filter(|id| self.swarm_owners.get(id) == Some(&owner)) else { return ("Unknown member in your swarm.".into(), false) };
            self.stop_swarm_member(child);
            (format!("Stopped swarm member {child}."), true)
        } else if self.swarms.contains_key(&owner) {
            self.stop_swarm_children(owner);
            ("Stopped the Akasha swarm.".into(), true)
        } else { ("No Akasha swarm to stop.".into(), false) }
    }

    fn swarm_note(&mut self, caller: AgentId, tool: &str, args: &Value) -> (String, bool) {
        let owner = self.swarm_owners.get(&caller).copied().unwrap_or(caller);
        let Some(group) = self.swarms.get_mut(&owner) else { return ("No Akasha swarm yet. Start one with mantra_swarm_start.".into(), false) };
        let notes = &mut group.notes;
        let author = if caller == owner { format!("owner-{owner}") } else { format!("member-{caller}") };
        let now = crate::util::unix_secs();
        let text = args.get("text").and_then(Value::as_str).unwrap_or("");
        let id = args.get("id").and_then(Value::as_u64).and_then(|n| u32::try_from(n).ok());
        match tool {
            "mantra_akasha_read" => (notes.render(), true),
            "mantra_akasha_note" => match notes.note(&author, text, args.get("urgent").and_then(Value::as_bool).unwrap_or(false), now) {
                Ok(id) => (format!("Noted #{id} ({}).", notes.usage()), true), Err(e) => (e, false),
            },
            "mantra_akasha_amend" => match id {
                Some(id) => match notes.amend(id, text, &author, now) { Ok(()) => (format!("Amended #{id}."), true), Err(e) => (e, false) },
                None => ("Provide a note id.".into(), false),
            },
            "mantra_akasha_forget" => match id {
                Some(id) => match notes.forget(id) { Ok(()) => (format!("Forgot #{id}."), true), Err(e) => (e, false) },
                None => ("Provide a note id.".into(), false),
            },
            _ => unreachable!(),
        }
    }

    pub(super) fn swarm_child_ended(&mut self, child: AgentId, status: &str, error: Option<&str>) {
        let Some(owner) = self.swarm_owners.get(&child).copied() else { return };
        let Some(group) = self.swarms.get_mut(&owner) else { return };
        let text = if status == "completed" {
            self.agents.get(&child).and_then(|a| a.final_message.as_deref()).unwrap_or("").trim().to_string()
        } else { error.unwrap_or(status).to_string() };
        let summary = format!("Member {child} {status}: {}", crate::util::trunc(&text, 450));
        let _ = group.notes.note("mantra", &summary, status != "completed", crate::util::unix_secs());
        let group_name = group.name.clone();
        let ready = !self.swarm_pending(owner);
        if ready { if let Some(a) = self.agents.get_mut(&owner) { a.swarm_waiting = false; } }
        let had_waiters = self.swarm_waiters.contains_key(&owner);
        if ready {
            if let Some(waiters) = self.swarm_waiters.remove(&owner) {
                let report = self.swarm_status(owner).0;
                for request in waiters { self.respond_tool_text(owner, request, &report, true); }
            }
        }
        let may_prompt = self.hub.has_agent(owner) && self.agents.get(&owner).is_some_and(|a| !a.stopped_by_user && a.status != Status::Stopped) && (Some(owner) == self.solo || self.run.as_ref().is_some_and(|r| r.is_active() && r.all_agents().contains(&owner)));
        if ready && may_prompt && self.swarm_deferred.remove(&owner) {
            prompt_agent(&self.hub, &mut self.agents, owner, format!("[Akasha {group_name}] All members finished. Review mantra_swarm_status and shared notes, verify their work, then report your result."), true, Send::Auto);
        } else if may_prompt && ready && !had_waiters {
            // Solo and long-lived supervisors get one concise completion wakeup. A worker that
            // used mantra_swarm_wait receives its result through that tool call instead.
            if Some(owner) == self.solo || !self.run.as_ref().is_some_and(|r| r.role_name_of(owner).and_then(|n| r.pattern.role(&n).map(|role| role.kind == "worker" || role.kind == "gate")).unwrap_or(false)) {
                prompt_agent(&self.hub, &mut self.agents, owner, format!("[Akasha {group_name}] {summary}"), true, Send::Auto);
            }
        }
    }

    fn stop_swarm_member(&mut self, child: AgentId) {
        let Some(owner) = self.swarm_owners.get(&child).copied() else { return };
        // The generic stop reports this member to the group *after* its process is gone, so a
        // pending mantra_swarm_wait can finish. Whole-group shutdown removes the group first.
        self.stop_agent_by_user(child);
        self.swarm_owners.remove(&child);
        if let Some(group) = self.swarms.get_mut(&owner) { group.children.retain(|id| *id != child); }
    }

    pub(crate) fn stop_swarm_children(&mut self, owner: AgentId) {
        if let Some(a) = self.agents.get_mut(&owner) { a.swarm_waiting = false; }
        if let Some(waiters) = self.swarm_waiters.remove(&owner) {
            for request in waiters { self.respond_tool_text(owner, request, "Akasha swarm stopped before its members finished.", false); }
        }
        self.swarm_deferred.remove(&owner);
        if let Some(group) = self.swarms.remove(&owner) {
            for child in group.children { self.stop_swarm_member(child); }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parent(app: &mut App, id: AgentId) {
        let mut a = Agent::new(id, "parent", "solo", PathBuf::from("."));
        a.model_alias = "sol".into();
        a.status = Status::Busy;
        app.agents.insert(id, a);
        let _ = app.hub.test_register(id);
    }

    #[test]
    fn only_solo_and_explicitly_enabled_roles_can_spawn() {
        let mut app = crate::web::snapshot::tests::test_app();
        parent(&mut app, 1);
        assert!(!app.swarm_authorized(1));
        app.solo = Some(1);
        assert!(app.swarm_authorized(1));
        app.agents.get_mut(&1).unwrap().stopped_by_user = true;
        assert!(!app.swarm_authorized(1));
        app.agents.get_mut(&1).unwrap().stopped_by_user = false;
        app.solo = None;
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        run.planner = Some(1);
        app.run = Some(run);
        assert!(!app.swarm_authorized(1));
        app.run.as_mut().unwrap().pattern.roles.get_mut("planner").unwrap().can_spawn_swarm = true;
        assert!(app.swarm_authorized(1));
        app.run.as_mut().unwrap().stage = Stage::Done;
        assert!(!app.swarm_authorized(1), "finished runs cannot spawn new processes");
        app.run.as_mut().unwrap().stage = Stage::Planning;
        app.swarm_owners.insert(1, 7);
        assert!(!app.swarm_authorized(1), "members cannot recursively delegate");
    }

    #[test]
    fn validation_is_atomic_and_other_owners_cannot_message_or_stop_members() {
        let mut app = crate::web::snapshot::tests::test_app();
        parent(&mut app, 1);
        parent(&mut app, 2);
        app.solo = Some(1);
        let (text, ok) = app.swarm_start(1, &json!({"tasks": [
            {"task": "good", "model": "sol"}, {"task": "bad", "model": "does-not-exist"}
        ]}));
        assert!(!ok && text.contains("no members started"));
        assert!(app.swarms.is_empty() && app.swarm_owners.is_empty());
        app.swarms.insert(1, SwarmGroup { owner: 1, name: "swarm-1".into(), notes: Block::new("swarm-1", 8_192), children: vec![3] });
        app.swarm_owners.insert(3, 1);
        assert!(!app.swarm_message(2, &json!({"agent": "3", "message": "hi"})).1);
        assert!(!app.swarm_stop(2, &json!({"agent": "3"})).1);
        assert_eq!(app.swarm_owners.get(&3), Some(&1));
    }

    #[test]
    fn owner_shutdown_cleans_children_and_rejects_late_access() {
        let mut app = crate::web::snapshot::tests::test_app();
        parent(&mut app, 1);
        let mut child = Agent::new(3, "member", "swarm-member", PathBuf::from("."));
        child.status = Status::Busy;
        app.agents.insert(3, child);
        let _ = app.hub.test_register(3);
        app.swarms.insert(1, SwarmGroup { owner: 1, name: "swarm-1".into(), notes: Block::new("swarm-1", 8_192), children: vec![3] });
        app.swarm_owners.insert(3, 1);
        app.stop_swarm_children(1);
        assert!(app.swarms.is_empty() && app.swarm_owners.is_empty());
        assert!(!app.hub.has_agent(3));
        assert_eq!(app.agents[&3].status, Status::Stopped);
        assert!(app.agents[&3].stopped_by_user);
    }

    #[test]
    fn worker_completion_waits_for_children_then_wakes_worker_to_review() {
        let mut app = crate::web::snapshot::tests::test_app();
        let mut owner = Agent::new(1, "worker", "worker-small", PathBuf::from("."));
        owner.model_alias = "sol".into();
        owner.status = Status::Idle;
        app.agents.insert(1, owner);
        let mut owner_rx = app.hub.test_register(1);
        let mut child = Agent::new(3, "member", "swarm-member", PathBuf::from("."));
        child.status = Status::Busy;
        child.turn_active = true;
        app.agents.insert(3, child);
        let _member_rx = app.hub.test_register(3);
        let mut run = Run::new(PathBuf::from("."), Pattern::builtin(), "test".into());
        run.workers.push(crate::web::snapshot::tests::worker("one", 1, Some(1), crate::engine::run::WState::Running));
        app.run = Some(run);
        app.swarms.insert(1, SwarmGroup { owner: 1, name: "swarm-1".into(), notes: Block::new("swarm-1", 8_192), children: vec![3] });
        app.swarm_owners.insert(3, 1);

        app.on_turn_done(1, "completed", None, None);
        assert!(app.swarm_deferred.contains(&1));
        assert!(app.hub.has_agent(3), "the writer keeps working until it finishes");
        assert!(app.swarms.contains_key(&1), "its shared notes remain for review");
        app.agents.get_mut(&3).unwrap().status = Status::Idle;
        app.agents.get_mut(&3).unwrap().turn_active = false;
        app.agents.get_mut(&3).unwrap().final_message = Some("done".into());
        app.on_turn_done(3, "completed", None, None);
        assert!(!app.swarm_deferred.contains(&1));
        assert!(matches!(owner_rx.try_recv(), Ok(Cmd::Turn { text }) if text.contains("All members finished")));
        assert!(app.hub.has_agent(3), "the owner still has time to verify before engine completion");
    }

    #[test]
    fn stopping_one_member_resolves_a_pending_wait() {
        let mut app = crate::web::snapshot::tests::test_app();
        let mut owner = Agent::new(1, "solo", "solo", PathBuf::from("."));
        owner.model_alias = "sol".into();
        owner.status = Status::Busy;
        app.agents.insert(1, owner);
        app.solo = Some(1);
        let mut owner_rx = app.hub.test_register(1);
        let mut member = Agent::new(3, "member", "swarm-member", PathBuf::from("."));
        member.status = Status::Busy;
        member.turn_active = true;
        app.agents.insert(3, member);
        let _member_rx = app.hub.test_register(3);
        app.swarms.insert(1, SwarmGroup { owner: 1, name: "swarm-1".into(), notes: Block::new("swarm-1", 8_192), children: vec![3] });
        app.swarm_owners.insert(3, 1);
        assert!(app.swarm_tool_call(1, json!("wait"), "mantra_swarm_wait", &json!({})));
        assert!(owner_rx.try_recv().is_err());
        app.stop_agent_by_user(3);
        match owner_rx.try_recv() {
            Ok(Cmd::Respond { id, result }) => {
                assert_eq!(id, json!("wait"));
                assert_eq!(result["success"], true);
                assert!(result.pointer("/contentItems/0/text").and_then(Value::as_str).unwrap().contains("Stopped"));
            }
            other => panic!("wait should resolve after remote stop: {other:?}"),
        }
    }
}
