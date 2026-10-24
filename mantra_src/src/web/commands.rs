//! The web command table (design §6): each command is validated and executed on the App task,
//! through the same App methods the keyboard uses, and answered with a `result` (or `items`).
//! The configuration commands 0.8 added (settings, models, providers, patterns) are in `ops`.

use super::protocol::{Command, ItemsMsg, ServerMsg};
use super::{ConnKind, Inbound};
use crate::agent::Level;
use crate::app::{prompt_agent, App};
use crate::engine::pattern::Pattern;
use crate::engine::run::{Send, Stage, WState};
use crate::hub::{AgentId, Cmd};
use serde_json::{json, Value};

/// Total diff text per `diff` answer, and per file.
const DIFF_TOTAL: usize = 400_000;
const DIFF_FILE: usize = 100_000;

type Res = Result<Value, String>;

fn ok() -> Res {
    Ok(json!({}))
}

/// Cap a diff at `max` bytes, keeping the start (hunks read top-down) and saying so.
fn cap(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n… truncated ({} more bytes)", &s[..end], s.len() - end)
}

impl App {
    pub(crate) fn web_command(&mut self, i: Inbound) {
        let Inbound { conn, req, cmd } = i;
        let name = cmd.name();
        if let Command::FetchItems { agent, before, count } = cmd {
            let msg = match self.agents.get(&agent) {
                Some(a) => ServerMsg::Items(ItemsMsg {
                    req,
                    agent,
                    items: super::snapshot::items_before(a, before, count.unwrap_or(200).clamp(1, 500) as usize),
                    first: a.items_first_ord(),
                    total: a.items_total_ord(),
                }),
                None => ServerMsg::err(req, "no such agent"),
            };
            if let Some(l) = &self.web {
                l.reply(conn, &msg);
            }
            return;
        }
        if let Command::PushTest { endpoint } = &cmd {
            // Answered from a task once the push service has spoken, never blocking the App.
            let (Some(l), Some(p)) = (self.web.clone(), self.web.as_ref().and_then(|l| l.push.clone())) else {
                if let Some(l) = &self.web {
                    l.reply(conn, &ServerMsg::err(req, "push is off (settings.toml [web] push = false)"));
                }
                return;
            };
            let rx = p.sender.test(endpoint);
            tokio::spawn(async move {
                let msg = match rx.await {
                    Ok(Ok(())) => ServerMsg::ok(req, json!({})),
                    Ok(Err(e)) => ServerMsg::err(req, e),
                    Err(_) => ServerMsg::err(req, "push sender stopped"),
                };
                l.reply(conn, &msg);
            });
            return;
        }
        // A probe of a provider, an MCP server or a model: answered once it is done.
        if self.web_config_later(conn, req, &cmd) {
            return;
        }
        let from_relay = self.web.as_ref().and_then(|l| l.reg.kind(conn)) == Some(ConnKind::Relay);
        let res = self.run_web_command(cmd, from_relay);
        crate::mlog!("web: {name} → {}", if res.is_ok() { "ok" } else { "error" });
        let msg = match res {
            Ok(data) => ServerMsg::ok(req, data),
            Err(e) => ServerMsg::err(req, e),
        };
        if let Some(l) = &self.web {
            l.reply(conn, &msg);
        }
    }

    fn agent_ok(&self, a: AgentId) -> Result<(), String> {
        if self.agents.contains_key(&a) {
            Ok(())
        } else {
            Err("no such agent".into())
        }
    }

    fn run_web_command(&mut self, cmd: Command, from_relay: bool) -> Res {
        match cmd {
            Command::Send { agent, text, force } => {
                self.agent_ok(agent)?;
                // One rule for both modes: an empty send is only a no-op flush of whatever is
                // already queued — with nothing typed and nothing queued, there is nothing to do.
                if text.trim().is_empty() && self.agents.get(&agent).map(|a| a.queued.is_empty()).unwrap_or(true) {
                    return Err("nothing queued to send".into());
                }
                if !self.hub.has_agent(agent) {
                    return Err("that agent's process has ended (respawn it, or start a new session)".into());
                }
                let mode = if force.unwrap_or(false) { Send::Force } else { Send::Queue };
                let text = text.trim_end().to_string();
                if Some(agent) == self.solo {
                    if let Some(sh) = text.strip_prefix('!') {
                        if let Some(a) = self.agents.get_mut(&agent) {
                            a.push_user(&text);
                        }
                        self.hub.send(agent, Cmd::Shell { command: sh.trim().to_string() });
                        return ok();
                    }
                }
                if self.in_run(agent) {
                    let name = self.run.as_ref().map(|r| r.name_of(agent)).unwrap_or_default();
                    self.with_run(|r, c| r.direct(c, &name, &text, mode));
                } else {
                    let echo = !text.trim().is_empty();
                    prompt_agent(&self.hub, &mut self.agents, agent, text, echo, mode);
                }
                ok()
            }
            Command::RunInput { text } => {
                if self.run.is_none() {
                    return Err("no run".into());
                }
                if text.trim().is_empty() {
                    return Err("empty message".into());
                }
                self.with_run(|r, c| r.user_input(c, text.trim_end()));
                ok()
            }
            Command::StartRun { goal, pattern, parallel } => {
                if self.run.as_ref().map(|r| r.is_active()).unwrap_or(false) {
                    return Err("a run is already open".into());
                }
                if goal.trim().is_empty() {
                    return Err("describe the goal".into());
                }
                if let Some(n) = parallel {
                    crate::engine::pattern::parse_parallel(&n.to_string())?;
                }
                if let Some(p) = pattern.filter(|p| !p.is_empty()) {
                    if !Pattern::list(&self.project).contains(&p) {
                        return Err("unknown pattern".into());
                    }
                    self.pattern_name = p;
                }
                let before = self.run.as_ref().map(|r| r.id.clone());
                self.start_run_with(goal.trim(), None, parallel);
                match &self.run {
                    Some(r) if Some(&r.id) != before.as_ref() => Ok(json!({"run": r.id})),
                    _ => Err(self.toast.as_ref().map(|t| t.0.clone()).unwrap_or_else(|| "the run could not start".into())),
                }
            }
            Command::PlanApprove => {
                if self.run.as_ref().map(|r| r.stage != Stage::Review).unwrap_or(true) {
                    return Err("no plan to approve".into());
                }
                self.with_run(|r, c| r.approve_plan(c));
                // The TUI's plan overlay is now stale; the web user approved it for them.
                self.overlays.retain(|o| !matches!(o, crate::app::Overlay::Plan { .. }));
                self.land_on_overview();
                ok()
            }
            Command::PauseResume => {
                if self.run.is_none() {
                    return Err("no run".into());
                }
                self.with_run(|r, c| r.toggle_pause(c));
                Ok(json!({"halted": self.run.as_ref().map(|r| r.halted()).unwrap_or(false)}))
            }
            Command::Interrupt { agent } => {
                self.agent_ok(agent)?;
                Ok(json!({"interrupted": self.interrupt_by_user(agent)}))
            }
            Command::Respawn { agent } => {
                self.agent_ok(agent)?;
                let crashed = self.agents.get(&agent).map(|a| matches!(a.status, crate::agent::Status::Crashed(_))).unwrap_or(false);
                if crashed && !self.in_run(agent) {
                    self.hub.send(agent, Cmd::Restart);
                    return ok();
                }
                if !self.in_run(agent) {
                    return Err("only run agents can be respawned (use New session for Solo)".into());
                }
                match self.with_run(|r, c| r.respawn(c, agent, None)) {
                    Some(Ok(_)) => ok(),
                    Some(Err(e)) => Err(e),
                    None => Err("no run".into()),
                }
            }
            Command::Compact { agent } => {
                self.agent_ok(agent)?;
                self.compact_agent(agent);
                Ok(json!({"pending": self.agents.get(&agent).map(|a| a.compact_pending).unwrap_or(false)}))
            }
            Command::SetModel { agent, alias } => {
                self.agent_ok(agent)?;
                let in_run = self.in_run(agent);
                self.set_model(agent, &alias)?;
                if in_run {
                    self.model_switched_for_run_agent(agent);
                }
                ok()
            }
            Command::SetRoleModel { role, alias, all } => {
                let mut pattern = Pattern::load(&self.pattern_name, &self.project).map_err(|e| e.to_string())?;
                if self.registry.get(&alias).is_none() {
                    return Err("unknown model".into());
                }
                if let Some(p) = self.registry.alias_problem(&alias) {
                    return Err(p);
                }
                let targets: Vec<String> = if all {
                    pattern.roles.keys().cloned().collect()
                } else {
                    if !pattern.roles.contains_key(&role) {
                        return Err("no such role".into());
                    }
                    vec![role]
                };
                for name in &targets {
                    if let Some(r) = pattern.roles.get_mut(name) {
                        r.model = alias.clone();
                        // a named effort is re-clamped to the new model; "auto" stays "auto"
                        if r.effort != crate::engine::pattern::EFFORT_AUTO {
                            r.effort = self.registry.resolve(&alias).resolve_effort(&r.effort);
                        }
                    }
                }
                let path = pattern.save().map_err(|e| e.to_string())?;
                // `self.pattern_name` is the pattern for the *next* run and can differ from the
                // active run's own (already-loaded) one — only patch live state when they're the
                // same pattern, or a same-named role in an unrelated pattern gets silently reassigned.
                if let Some(run) = self.run.as_mut().filter(|r| r.pattern.name == pattern.name) {
                    for name in &targets {
                        if let Some(r) = run.pattern.roles.get_mut(name) {
                            r.model = alias.clone();
                            if r.effort != crate::engine::pattern::EFFORT_AUTO {
                                r.effort = self.registry.resolve(&alias).resolve_effort(&r.effort);
                            }
                        }
                    }
                }
                // An active run on a *different* pattern keeps its own agents untouched too — only
                // its role names happened to collide with this pattern's.
                let run_mismatch = self.run.as_ref().map(|r| r.pattern.name != pattern.name).unwrap_or(false);
                let new_backend = self.registry.backend_of(&self.registry.resolve(&alias));
                // Only agents that will run again are switched. An archived one, or a worker
                // whose attempt is over (done, failed, cancelled), is kept for its card and its
                // log: it ran on the model it ran on, and rewriting its alias would make a
                // finished card claim the role's new model.
                let over = |id: AgentId, a: &crate::agent::Agent| {
                    a.status == crate::agent::Status::Stopped
                        || self.run.as_ref().map(|r| r.workers.iter().any(|w| w.agent == Some(id) && matches!(w.state, WState::Done | WState::Failed(_) | WState::Cancelled))).unwrap_or(false)
                };
                let live: Vec<AgentId> = self.agents.iter().filter(|(id, a)| targets.contains(&a.role) && !over(**id, a)).map(|(id, _)| *id).collect();
                for id in live {
                    if run_mismatch && self.in_run(id) {
                        continue;
                    }
                    let same_backend = self.agents.get(&id).map(|a| a.backend == new_backend).unwrap_or(false);
                    if same_backend {
                        let in_run = self.in_run(id);
                        if self.set_model(id, &alias).is_ok() && in_run {
                            self.model_switched_for_run_agent(id);
                        }
                    } else if let Some(a) = self.agents.get_mut(&id) {
                        let name = a.name.clone();
                        let old = a.model_alias.clone();
                        a.notice(Level::Warn, format!("{name} keeps {old} until respawned (other backend)"));
                    }
                }
                Ok(json!({"changed": targets, "path": path.display().to_string()}))
            }
            Command::SetRoleFallback { role, alias } => {
                let mut pattern = Pattern::load(&self.pattern_name, &self.project).map_err(|e| e.to_string())?;
                let Some(r) = pattern.roles.get_mut(&role) else { return Err("no such role".into()) };
                if let Some(a) = &alias {
                    if self.registry.get(a).is_none() {
                        return Err("unknown model".into());
                    }
                    if let Some(p) = self.registry.alias_problem(a) {
                        return Err(p);
                    }
                    if *a == r.model {
                        return Err("a fallback must be another model than the role's own".into());
                    }
                }
                r.fallback = alias.clone();
                let path = pattern.save().map_err(|e| e.to_string())?;
                // the active run picks it up only when it runs this very pattern (as set_role_model)
                if let Some(r) = self.run.as_mut().filter(|r| r.pattern.name == pattern.name).and_then(|r| r.pattern.roles.get_mut(&role)) {
                    r.fallback = alias;
                }
                Ok(json!({"path": path.display().to_string()}))
            }
            Command::SetEffort { agent, effort } => {
                self.agent_ok(agent)?;
                let alias = self.agents.get(&agent).map(|a| a.model_alias.clone()).unwrap_or_default();
                let m = self.registry.resolve(&alias);
                let efforts = m.efforts();
                if efforts.is_empty() {
                    return Err("this model has no effort levels".into());
                }
                // "auto" is the role's own choice: resolved for this agent before the check
                let effort = if effort == crate::engine::pattern::EFFORT_AUTO { m.resolve_effort(self.auto_effort_for(agent)) } else { effort };
                if !efforts.contains(&effort) {
                    return Err("unknown effort".into());
                }
                self.set_effort(agent, &effort);
                ok()
            }
            Command::StepEffort { agent, delta } => {
                self.agent_ok(agent)?;
                self.step_effort(agent, delta.clamp(-10, 10));
                Ok(json!({"effort": self.agents.get(&agent).map(|a| a.effort.clone()).unwrap_or_default()}))
            }
            Command::Approve { key, decision, answer } => {
                let idx = self.approvals.iter().position(|a| super::snapshot::approval_key(a) == key).ok_or("approval is gone")?;
                let d = match decision.as_str() {
                    "yes" => 0,
                    "session" => 1,
                    "no" => 2,
                    "cancel" => 3,
                    _ => return Err("decision is yes, session, no or cancel".into()),
                };
                self.resolve_approval(idx, d, answer);
                ok()
            }
            Command::ApprovalMode { mode } => {
                if !["untrusted", "on-request", "never"].contains(&mode.as_str()) {
                    return Err("mode is untrusted, on-request or never".into());
                }
                self.set_approval_mode(&mode);
                ok()
            }
            Command::DiscardQueue { agent } => {
                self.agent_ok(agent)?;
                if let Some(a) = self.agents.get_mut(&agent) {
                    a.queued.clear();
                }
                ok()
            }
            Command::PopQueued { agent } => {
                self.agent_ok(agent)?;
                let t = self.agents.get_mut(&agent).and_then(|a| a.queued.pop()).ok_or("nothing queued")?;
                Ok(json!({"text": t}))
            }
            Command::Land => {
                if self.run.as_ref().map(|r| r.stage != Stage::Done).unwrap_or(true) {
                    return Err("no finished run to land".into());
                }
                self.with_run(|r, c| r.land(c));
                ok()
            }
            Command::RunsList => {
                let key = crate::engine::state::project_key(&self.project);
                let all = crate::engine::state::list_all();
                let others = all.iter().filter(|r| r.project_key != key).count();
                let open = self.run.as_ref().map(|r| r.id.clone());
                let runs: Vec<Value> = all.iter().filter(|r| r.project_key == key).map(|r| run_row(r, open.as_deref())).collect();
                Ok(json!({"runs": runs, "others": others}))
            }
            Command::RunResume { id } => {
                if self.run.as_ref().map(|r| r.is_active()).unwrap_or(false) {
                    return Err("a run is already open".into());
                }
                let r = crate::engine::state::find(&id)?;
                let rid = r.id.clone();
                self.resume_run(r);
                match &self.run {
                    Some(x) if x.id == rid => Ok(json!({"run": rid})),
                    _ => Err(self.toast.as_ref().map(|t| t.0.clone()).unwrap_or_else(|| "could not resume".into())),
                }
            }
            Command::RunDelete { id } => {
                let r = crate::engine::state::find(&id)?;
                if self.delete_run(&r) {
                    ok()
                } else {
                    // still active, or open in another Mantra: `delete_run` said which
                    Err(self.toast.as_ref().map(|t| t.0.clone()).unwrap_or_else(|| "that run is open".into()))
                }
            }
            Command::NewSolo => {
                self.start_solo();
                self.toast("fresh session (from the web)", Level::Ok);
                Ok(json!({"agent": self.solo}))
            }
            Command::SetPattern { name } => {
                if !Pattern::list(&self.project).contains(&name) {
                    return Err("unknown pattern".into());
                }
                self.pattern_name = name;
                ok()
            }
            Command::Diff { agent, path } => {
                self.agent_ok(agent)?;
                let a = &self.agents[&agent];
                let mut total = 0usize;
                let mut files = vec![];
                for (p, f) in a.files.iter().filter(|(p, _)| path.as_ref().map(|x| x == *p).unwrap_or(true)) {
                    let room = DIFF_TOTAL.saturating_sub(total);
                    let diff = cap(&f.diff, DIFF_FILE.min(room));
                    total = total.saturating_add(diff.len());
                    files.push(json!({"path": p, "kind": f.kind, "adds": f.adds, "dels": f.dels, "diff": diff}));
                }
                if path.is_some() && files.is_empty() {
                    return Err("no such file".into());
                }
                Ok(json!({"files": files}))
            }
            Command::PlanMarkdown => {
                let md = self.run.as_ref().and_then(|r| r.plan.as_ref()).map(|p| p.to_markdown()).ok_or("no plan yet")?;
                Ok(json!({"markdown": md}))
            }
            Command::PushSubscribe { subscription, device, prefs } => {
                let p = self.web.as_ref().and_then(|l| l.push.clone()).ok_or("push is off (settings.toml [web] push = false)")?;
                // Cheap and synchronous (scheme + IP-literal classification, no DNS) so this never
                // blocks the App task; a hostname that turns out to resolve privately is caught
                // and dropped by the Sender task below (SSRF guard, design §9.2 point 4).
                super::push::check_endpoint_syntax(&subscription.endpoint)?;
                let valid_keys = super::crypto::b64url_decode(&subscription.keys.p256dh).map(|k| k.len() == 65).unwrap_or(false) && super::crypto::b64url_decode(&subscription.keys.auth).map(|k| k.len() == 16).unwrap_or(false);
                if !valid_keys {
                    return Err("bad subscription keys".into());
                }
                let endpoint = subscription.endpoint;
                let sub = super::push::Subscription {
                    endpoint: endpoint.clone(),
                    p256dh: subscription.keys.p256dh,
                    auth: subscription.keys.auth,
                    device: crate::util::trunc(&device, 80),
                    created_unix: crate::util::unix_secs(),
                    prefs: prefs.unwrap_or_default(),
                    failures: 0,
                };
                {
                    let mut store = p.store.lock().unwrap_or_else(|e| e.into_inner());
                    store.upsert(sub);
                    store.save();
                }
                p.sender.validate(&endpoint);
                ok()
            }
            Command::PushUnsubscribe { endpoint } => {
                let p = self.web.as_ref().and_then(|l| l.push.clone()).ok_or("push is off")?;
                let mut store = p.store.lock().unwrap_or_else(|e| e.into_inner());
                let removed = store.remove(&endpoint);
                store.save();
                Ok(json!({"removed": removed}))
            }
            Command::PushPrefs { endpoint, prefs } => {
                let p = self.web.as_ref().and_then(|l| l.push.clone()).ok_or("push is off")?;
                let mut store = p.store.lock().unwrap_or_else(|e| e.into_inner());
                if !store.set_prefs(&endpoint, prefs) {
                    return Err("unknown subscription".into());
                }
                store.save();
                ok()
            }
            Command::RemoteRotate => {
                if from_relay {
                    return Err("not available from a remote session".into());
                }
                let r = self.web.as_ref().and_then(|l| l.remote.clone()).ok_or("remote access is off (start with --remote)")?;
                r.rotate();
                serde_json::to_value(r.info()).map_err(|e| e.to_string())
            }
            cmd @ (Command::RunStop
            | Command::PatternsList
            | Command::PatternGet { .. }
            | Command::PatternSave { .. }
            | Command::PatternDelete { .. }
            | Command::SettingsGet
            | Command::SettingsSet { .. }
            | Command::McpSet { .. }
            | Command::McpRemove { .. }
            | Command::ModelsGet
            | Command::ModelSet { .. }
            | Command::ModelAdd { .. }
            | Command::ModelDelete { .. }
            | Command::ProviderSet { .. }
            | Command::ProviderDelete { .. }
            | Command::ModelsImport { .. }
            | Command::AddBackend { .. }) => self.web_config(cmd, from_relay),
            Command::FetchItems { .. } | Command::PushTest { .. } | Command::ProviderFromKey { .. } | Command::McpTest { .. } | Command::ModelTest { .. } => Err("internal: handled above".into()),
        }
    }
}

/// One row of the `runs_list` reply: the run's title first (the id is the key, shown second).
fn run_row(r: &crate::engine::state::RunSummary, open: Option<&str>) -> Value {
    json!({
        "id": r.id,
        "title": r.title(),
        "stage": r.stage_label(),
        "updated_at": r.updated_unix,
        "ago": crate::engine::state::fmt_ago(r.updated_unix),
        "brief": r.brief(),
        "unfinished": r.unfinished(),
        "error": r.state.as_ref().err(),
        "open": open == Some(r.id.as_str()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Approval;
    use crate::web::snapshot::tests::{add_agent, test_app, with_mantra_home};
    use crate::web::{Link, LinkInfo, WebRegistryHandle};
    use std::time::Instant;

    /// An App wired to a registry with one local connection; returns the replies it gets.
    fn wired() -> (App, crate::web::ConnHandle, tokio::sync::mpsc::UnboundedReceiver<crate::hub::Cmd>) {
        let mut app = test_app();
        let (ctl, _c) = tokio::sync::mpsc::unbounded_channel();
        std::mem::forget(_c);
        let reg = WebRegistryHandle::new(ctl);
        let h = reg.register(ConnKind::Local);
        app.web = Some(Link { reg, remote: None, push: None, info: LinkInfo::default(), shared: Default::default() });
        add_agent(&mut app, 1, "solo");
        app.solo = Some(1);
        let rx = app.hub.test_register(1);
        (app, h, rx)
    }

    fn reply(h: &mut crate::web::ConnHandle) -> ServerMsg {
        match h.rx.try_recv() {
            Ok(crate::web::Outbound::Text(t)) => serde_json::from_str(&t).unwrap(),
            other => panic!("no reply: {other:?}"),
        }
    }

    fn run(app: &mut App, h: &mut crate::web::ConnHandle, req: u64, cmd: Command) -> ServerMsg {
        app.web_command(Inbound { conn: h.id, req, cmd });
        reply(h)
    }

    #[test]
    fn send_queues_behind_a_busy_turn_and_force_steers() {
        let (mut app, mut h, mut rx) = wired();
        {
            let a = app.agents.get_mut(&1).unwrap();
            a.turn_active = true;
            a.status = crate::agent::Status::Busy;
        }
        let r = run(&mut app, &mut h, 1, Command::Send { agent: 1, text: "later".into(), force: None });
        assert!(matches!(r, ServerMsg::Result(ref x) if x.ok && x.req == 1), "{r:?}");
        assert_eq!(app.agents[&1].queued, vec!["later".to_string()]);
        assert!(rx.try_recv().is_err());
        let r = run(&mut app, &mut h, 2, Command::Send { agent: 1, text: "now".into(), force: Some(true) });
        assert!(matches!(r, ServerMsg::Result(ref x) if x.ok));
        assert!(matches!(rx.try_recv(), Ok(Cmd::Steer { text }) if text == "later\n\nnow"));
        let r = run(&mut app, &mut h, 3, Command::Send { agent: 99, text: "x".into(), force: None });
        assert!(matches!(r, ServerMsg::Result(ref x) if !x.ok && x.error.as_deref() == Some("no such agent")));
    }

    #[test]
    fn approvals_resolve_by_key() {
        let (mut app, mut h, mut rx) = wired();
        app.approvals.push(Approval { agent: 1, id: json!(42), method: "item/commandExecution/requestApproval".into(), title: "Run this command?".into(), detail: "$ ls".into(), params: json!({}), at: Instant::now() });
        let key = super::super::snapshot::approval_key(&app.approvals[0]);
        assert_eq!(key, "1:42");
        let r = run(&mut app, &mut h, 1, Command::Approve { key: key.clone(), decision: "yes".into(), answer: None });
        assert!(matches!(r, ServerMsg::Result(ref x) if x.ok));
        assert!(app.approvals.is_empty());
        assert!(matches!(rx.try_recv(), Ok(Cmd::Respond { result, .. }) if result["decision"] == "accept"));
        let r = run(&mut app, &mut h, 2, Command::Approve { key, decision: "yes".into(), answer: None });
        assert!(matches!(r, ServerMsg::Result(ref x) if x.error.as_deref() == Some("approval is gone")));
    }

    #[test]
    fn set_effort_accepts_auto_and_resolves_it_for_the_agents_role() {
        let (mut app, mut h, _rx) = wired();
        let mut run_ = crate::engine::run::Run::new(std::path::PathBuf::from("."), Pattern::builtin(), "goal".into());
        run_.workers.push(crate::engine::run::Worker {
            task: crate::engine::plan::Task { id: "t1".into(), role: "worker-small".into(), ..Default::default() },
            prompt: String::new(),
            effort: None,
            agent: Some(1),
            attempt: 1,
            state: crate::engine::run::WState::Running,
            report: String::new(),
            wt: None,
            branch: String::new(),
            spawned: std::time::Instant::now(),
            finished: None,
            tripwires: vec![],
            adhoc: false,
            paused: false,
            stall_flagged: false,
            budget_flagged: false,
            context_override: None,
            idle_prompts: vec![],
            transient: false,
            model: None,
        });
        app.run = Some(run_);
        assert_eq!(app.auto_effort_for(1), "low", "a small worker");
        let r = run(&mut app, &mut h, 1, Command::SetEffort { agent: 1, effort: "auto".into() });
        assert!(matches!(r, ServerMsg::Result(ref x) if x.ok), "{r:?}");
        assert_eq!(app.agents[&1].effort, "low");
        let r = run(&mut app, &mut h, 2, Command::SetEffort { agent: 1, effort: "sideways".into() });
        assert!(matches!(r, ServerMsg::Result(ref x) if x.error.as_deref() == Some("unknown effort")), "{r:?}");
        app.run = None;
        assert_eq!(app.auto_effort_for(1), "medium", "outside a run auto is the middle of the road");
    }

    #[test]
    fn a_runs_list_row_leads_with_the_title() {
        use crate::engine::state::{RunState, RunSummary};
        let st = RunState { id: "760583-ich-benutze-schon-seit-l".into(), title: "Notes sync".into(), brief: "Ich benutze schon seit langer Zeit ein Tool".into(), stage: Stage::Done, updated_unix: 5, ..Default::default() };
        let row = run_row(&RunSummary { id: st.id.clone(), dir: std::path::PathBuf::from("."), project_key: "p".into(), state: Ok(st), updated_unix: 5 }, Some("760583-ich-benutze-schon-seit-l"));
        assert_eq!(row["title"], "Notes sync");
        assert_eq!(row["id"], "760583-ich-benutze-schon-seit-l", "the id stays, as the key");
        assert_eq!(row["open"], true);
        assert_eq!(row["unfinished"], false);
        let old = run_row(&RunSummary { id: "1-x".into(), dir: std::path::PathBuf::from("."), project_key: "p".into(), state: Ok(RunState { brief: "add a login page to the app now".into(), ..Default::default() }), updated_unix: 0 }, None);
        assert_eq!(old["title"], "add a login page to the", "an older state names itself from the goal");
        assert_eq!(old["open"], false);
    }

    #[test]
    fn start_run_refuses_while_one_is_active() {
        let (mut app, mut h, _rx) = wired();
        let mut run_ = crate::engine::run::Run::new(std::path::PathBuf::from("."), Pattern::builtin(), "goal".into());
        run_.stage = Stage::Planning;
        app.run = Some(run_);
        let r = run(&mut app, &mut h, 1, Command::StartRun { goal: "another".into(), pattern: None, parallel: None });
        assert!(matches!(r, ServerMsg::Result(ref x) if x.error.as_deref() == Some("a run is already open")), "{r:?}");
        let r = run(&mut app, &mut h, 2, Command::PlanApprove);
        assert!(matches!(r, ServerMsg::Result(ref x) if x.error.as_deref() == Some("no plan to approve")));
        app.run = None;
        let r = run(&mut app, &mut h, 3, Command::RunInput { text: "hi".into() });
        assert!(matches!(r, ServerMsg::Result(ref x) if x.error.as_deref() == Some("no run")));
    }

    #[test]
    fn fetch_items_windows_older_items() {
        let (mut app, mut h, _rx) = wired();
        for i in 0..10 {
            app.agents.get_mut(&1).unwrap().push_user(&format!("m{i}"));
        }
        app.web_command(Inbound { conn: h.id, req: 5, cmd: Command::FetchItems { agent: 1, before: 8, count: Some(3) } });
        match reply(&mut h) {
            ServerMsg::Items(m) => {
                assert_eq!((m.req, m.agent, m.first, m.total), (5, 1, 0, 10));
                assert_eq!(m.items.iter().map(|i| i.text.as_str()).collect::<Vec<_>>(), vec!["m5", "m6", "m7"]);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn queue_edits_and_validation() {
        let (mut app, mut h, _rx) = wired();
        app.agents.get_mut(&1).unwrap().queued = vec!["a".into(), "b".into()];
        let r = run(&mut app, &mut h, 1, Command::PopQueued { agent: 1 });
        assert!(matches!(r, ServerMsg::Result(ref x) if x.data.as_ref().unwrap()["text"] == "b"));
        let _ = run(&mut app, &mut h, 2, Command::DiscardQueue { agent: 1 });
        assert!(app.agents[&1].queued.is_empty());
        let r = run(&mut app, &mut h, 3, Command::SetModel { agent: 1, alias: "no-such-model".into() });
        assert!(matches!(r, ServerMsg::Result(ref x) if x.error.as_deref() == Some("unknown model")));
        let r = run(&mut app, &mut h, 4, Command::ApprovalMode { mode: "sometimes".into() });
        assert!(matches!(r, ServerMsg::Result(ref x) if !x.ok));
        let r = run(&mut app, &mut h, 5, Command::RemoteRotate);
        assert!(matches!(r, ServerMsg::Result(ref x) if x.error.as_deref() == Some("remote access is off (start with --remote)")));
        assert_eq!(cap("abcdef", 3), "abc\n… truncated (3 more bytes)");
    }

    #[test]
    fn send_rejects_whitespace_with_nothing_queued_in_both_modes() {
        let (mut app, mut h, _rx) = wired();
        let r = run(&mut app, &mut h, 1, Command::Send { agent: 1, text: "   ".into(), force: None });
        assert!(matches!(r, ServerMsg::Result(ref x) if x.error.as_deref() == Some("nothing queued to send")), "{r:?}");
        let r = run(&mut app, &mut h, 2, Command::Send { agent: 1, text: "\t\n".into(), force: Some(true) });
        assert!(matches!(r, ServerMsg::Result(ref x) if x.error.as_deref() == Some("nothing queued to send")), "{r:?}");
        // something queued: an empty force flushes it instead of being refused
        app.agents.get_mut(&1).unwrap().queued = vec!["a".into()];
        let r = run(&mut app, &mut h, 3, Command::Send { agent: 1, text: "".into(), force: Some(true) });
        assert!(matches!(r, ServerMsg::Result(ref x) if x.ok), "{r:?}");
    }

    #[test]
    fn set_role_model_all_writes_every_role_and_reclamps_effort() {
        with_mantra_home(|| {
            let (mut app, mut h, _rx) = wired();
            app.registry.models.push(crate::config::ModelEntry { alias: "capped".into(), model: "capped-model".into(), efforts: vec!["low".into(), "medium".into()], default_effort: "low".into(), ..Default::default() });
            let mut pattern = Pattern::builtin();
            pattern.name = "set-role-model-test".into();
            for role in pattern.roles.values_mut() {
                role.effort = "high".into();
            }
            pattern.save().unwrap();
            app.pattern_name = pattern.name.clone();

            let r = run(&mut app, &mut h, 1, Command::SetRoleModel { role: String::new(), alias: "capped".into(), all: true });
            match &r {
                ServerMsg::Result(x) => assert!(x.ok, "{x:?}"),
                other => panic!("{other:?}"),
            }
            let saved = Pattern::load(&app.pattern_name, &app.project).unwrap();
            assert!(!saved.roles.is_empty());
            for (name, role) in &saved.roles {
                assert_eq!(role.model, "capped", "role '{name}'");
                assert_eq!(role.effort, "medium", "role '{name}': high must clamp to this model's ceiling");
            }
        });
    }

    /// A project-local pattern (`<project>/.mantra/patterns/`) always wins on load — so a save
    /// must land there too, not in `$MANTRA_HOME`, or the change is invisible next load.
    #[test]
    fn set_role_model_keeps_an_auto_effort_auto() {
        with_mantra_home(|| {
            let (mut app, mut h, _rx) = wired();
            app.registry.models.push(crate::config::ModelEntry { alias: "capped".into(), model: "capped-model".into(), efforts: vec!["low".into(), "medium".into()], default_effort: "low".into(), ..Default::default() });
            let mut pattern = Pattern::builtin();
            pattern.name = "set-role-model-auto-test".into();
            pattern.roles.get_mut("planner").unwrap().effort = "high".into();
            pattern.save().unwrap();
            app.pattern_name = pattern.name.clone();
            let r = run(&mut app, &mut h, 1, Command::SetRoleModel { role: String::new(), alias: "capped".into(), all: true });
            assert!(matches!(r, ServerMsg::Result(ref x) if x.ok), "{r:?}");
            let saved = Pattern::load(&app.pattern_name, &app.project).unwrap();
            assert_eq!(saved.roles["planner"].effort, "medium", "a named effort is clamped to the new model");
            assert_eq!(saved.roles["worker-big"].effort, "auto", "auto is not baked into a level by a model change");
        });
    }

    #[test]
    fn set_role_fallback_takes_another_known_alias_or_none() {
        with_mantra_home(|| {
            let (mut app, mut h, _rx) = wired();
            app.registry.models.push(crate::config::ModelEntry { alias: "backup".into(), model: "backup-model".into(), ..Default::default() });
            let mut pattern = Pattern::builtin();
            pattern.name = "set-role-fallback-test".into();
            pattern.save().unwrap();
            app.pattern_name = pattern.name.clone();
            let own = pattern.roles["planner"].model.clone();
            let mut set = |app: &mut App, role: &str, alias: Option<&str>| run(app, &mut h, 1, Command::SetRoleFallback { role: role.into(), alias: alias.map(|a| a.into()) });
            let refused = |r: ServerMsg| matches!(r, ServerMsg::Result(ref x) if !x.ok);
            assert!(refused(set(&mut app, "planner", Some("no-such-alias"))), "an unknown alias");
            assert!(refused(set(&mut app, "planner", Some(own.as_str()))), "the role's own model");
            assert!(refused(set(&mut app, "no-such-role", Some("backup"))), "an unknown role");
            let saved = || Pattern::load("set-role-fallback-test", std::path::Path::new(".")).unwrap().roles["planner"].fallback.clone();
            assert_eq!(saved(), None, "nothing refused was saved");
            let r = set(&mut app, "planner", Some("backup"));
            assert!(matches!(r, ServerMsg::Result(ref x) if x.ok), "{r:?}");
            assert_eq!(saved().as_deref(), Some("backup"));
            let r = set(&mut app, "planner", None);
            assert!(matches!(r, ServerMsg::Result(ref x) if x.ok), "{r:?}");
            assert_eq!(saved(), None, "none clears it");
        });
    }

    #[test]
    fn set_role_model_saves_back_to_the_project_local_pattern() {
        with_mantra_home(|| {
            let (mut app, mut h, _rx) = wired();
            app.registry.models.push(crate::config::ModelEntry { alias: "capped".into(), model: "capped-model".into(), efforts: vec!["low".into(), "medium".into()], default_effort: "low".into(), ..Default::default() });
            let project = std::env::temp_dir().join(format!("mantra-test-project-{}-{}", std::process::id(), crate::web::snapshot::now_ms()));
            let patterns_dir = project.join(".mantra").join("patterns");
            std::fs::create_dir_all(&patterns_dir).unwrap();
            let mut pattern = Pattern::builtin();
            pattern.name = "set-role-model-test".into();
            std::fs::write(patterns_dir.join("set-role-model-test.toml"), pattern.to_toml()).unwrap();
            app.project = project.clone();
            app.pattern_name = pattern.name.clone();

            let r = run(&mut app, &mut h, 1, Command::SetRoleModel { role: String::new(), alias: "capped".into(), all: true });
            match &r {
                ServerMsg::Result(x) => assert!(x.ok, "{x:?}"),
                other => panic!("{other:?}"),
            }
            let saved = Pattern::load(&app.pattern_name, &app.project).unwrap();
            for (name, role) in &saved.roles {
                assert_eq!(role.model, "capped", "role '{name}': project-local load must see the change");
            }
            assert!(!crate::config::patterns_dir().join("set-role-model-test.toml").exists(), "must not have been written to $MANTRA_HOME instead");
            let _ = std::fs::remove_dir_all(&project);
        });
    }

    /// `self.pattern_name` (the pattern for the *next* run) can differ from the active run's own,
    /// already-loaded pattern — a same-named role must not leak the change into that run.
    #[test]
    fn set_role_model_does_not_touch_a_run_on_a_different_pattern() {
        with_mantra_home(|| {
            let (mut app, mut h, _rx) = wired();
            app.registry.models.push(crate::config::ModelEntry { alias: "capped".into(), model: "capped-model".into(), efforts: vec!["low".into(), "medium".into()], default_effort: "low".into(), ..Default::default() });
            let mut run_ = crate::engine::run::Run::new(std::path::PathBuf::from("."), Pattern::builtin(), "goal".into());
            let planner_model_before = run_.pattern.roles.get("planner").unwrap().model.clone();
            let planner_agent: AgentId = 2;
            add_agent(&mut app, planner_agent, "planner");
            run_.planner = Some(planner_agent);
            app.run = Some(run_);
            // Same role names as the run's pattern, but a different pattern — this is the one
            // `self.pattern_name` names, so it's the one `SetRoleModel` edits.
            let mut other = Pattern::builtin();
            other.name = "unrelated".into();
            other.save().unwrap();
            app.pattern_name = other.name.clone();

            let r = run(&mut app, &mut h, 1, Command::SetRoleModel { role: String::new(), alias: "capped".into(), all: true });
            assert!(matches!(r, ServerMsg::Result(ref x) if x.ok), "{r:?}");
            assert_eq!(app.run.as_ref().unwrap().pattern.roles["planner"].model, planner_model_before, "the active run's own pattern must be untouched");
            assert_ne!(app.agents[&planner_agent].model_alias, "capped", "the run's own live agent must be untouched");
        });
    }

    /// The reported bug: a role model change rewrote the alias of every agent with that role,
    /// finished workers included, so a done card read the role's new model although its attempt
    /// ran on another one. An attempt that is over keeps the alias it ran with; only agents that
    /// will run again are switched, and the role itself takes the new model for the next spawn.
    #[test]
    fn set_role_model_leaves_finished_attempts_on_the_model_they_ran_with() {
        with_mantra_home(|| {
            let (mut app, mut h, _rx) = wired();
            let mut pattern = Pattern::builtin();
            pattern.name = "done-alias-test".into();
            pattern.save().unwrap();
            app.pattern_name = pattern.name.clone();
            let mut run_ = crate::engine::run::Run::new(std::path::PathBuf::from("."), pattern.clone(), "goal".into());
            let worker = |id: &str, agent: AgentId, state: WState| crate::web::snapshot::tests::worker(id, 1, Some(agent), state);
            for (id, name) in [(2, "p1-done"), (3, "p1-live"), (4, "p1-old")] {
                add_agent(&mut app, id, name);
                let a = app.agents.get_mut(&id).unwrap();
                a.role = "worker-small".into();
                a.model_alias = "sol".into();
                a.backend = crate::config::ProviderKind::Bija; // where the default models run
            }
            app.agents.get_mut(&4).unwrap().status = crate::agent::Status::Stopped;
            run_.workers.push(worker("p1-done", 2, WState::Done));
            run_.workers.push(worker("p1-live", 3, WState::Running));
            run_.workers.push(worker("p1-old", 4, WState::Cancelled));
            app.run = Some(run_);

            let r = run(&mut app, &mut h, 1, Command::SetRoleModel { role: "worker-small".into(), alias: "luna".into(), all: false });
            assert!(matches!(r, ServerMsg::Result(ref x) if x.ok), "{r:?}");
            assert_eq!(app.agents[&3].model_alias, "luna", "a running worker switches");
            assert_eq!(app.agents[&2].model_alias, "sol", "a done attempt ran on sol, and its card keeps saying so");
            assert_eq!(app.agents[&4].model_alias, "sol", "an archived attempt too");
            assert_eq!(app.run.as_ref().unwrap().pattern.roles["worker-small"].model, "luna", "the next spawn of the role takes the new model");
        });
    }

    /// Changing the model of the role that halted the run applies the change but never resumes
    /// the run behind the user's back — they resume it themselves, and the toast says how.
    #[test]
    fn set_role_model_on_a_halted_run_applies_the_change_and_leaves_it_halted() {
        with_mantra_home(|| {
            let (mut app, mut h, _rx) = wired();
            let mut pattern = Pattern::builtin();
            pattern.name = "halted-switch-test".into();
            pattern.save().unwrap();
            app.pattern_name = pattern.name.clone();
            let mut run_ = crate::engine::run::Run::new(std::path::PathBuf::from("."), pattern.clone(), "goal".into());
            let planner: AgentId = 2;
            add_agent(&mut app, planner, "planner");
            app.agents.get_mut(&planner).unwrap().role = "planner".into();
            app.agents.get_mut(&planner).unwrap().backend = crate::config::ProviderKind::Bija; // where the default models run
            run_.planner = Some(planner);
            let msg = "planner (astra via zai): Unexpected message role.".to_string();
            run_.halt = Some(crate::engine::run::Halt { reason: crate::engine::run::HaltReason::ProviderRejected, agent: Some(planner), message: msg.clone(), full: msg, since: Instant::now() });
            app.run = Some(run_);
            assert_ne!(pattern.roles["planner"].model, "luna");

            let r = run(&mut app, &mut h, 1, Command::SetRoleModel { role: "planner".into(), alias: "luna".into(), all: false });
            assert!(matches!(r, ServerMsg::Result(ref x) if x.ok), "{r:?}");
            let run_ = app.run.as_ref().unwrap();
            assert!(run_.halted(), "the model change must not resume the run");
            assert_eq!(run_.pattern.roles["planner"].model, "luna", "but the change is applied");
            assert_eq!(app.agents[&planner].model_alias, "luna");
            assert_eq!(app.toast.as_ref().map(|t| t.0.as_str()), Some("model changed — resume the run to continue (space / Resume)"));
            assert!(!run_.pulse.iter().any(|p| p.text.contains("run resumed")), "no resume line in the journal");

            // an explicit resume still works, and a model change on a running run says nothing
            app.toast = None;
            let r = run(&mut app, &mut h, 2, Command::PauseResume);
            assert!(matches!(r, ServerMsg::Result(ref x) if x.ok), "{r:?}");
            assert!(!app.run.as_ref().unwrap().halted());
            let r = run(&mut app, &mut h, 3, Command::SetRoleModel { role: "planner".into(), alias: "sol".into(), all: false });
            assert!(matches!(r, ServerMsg::Result(ref x) if x.ok), "{r:?}");
            assert!(app.toast.is_none(), "{:?}", app.toast);
        });
    }

    // ─── the configuration commands (`web::ops`) ───

    /// A command's answer as a test reads it: the data of a success, or the error text.
    fn ask(app: &mut App, h: &mut crate::web::ConnHandle, cmd: Command) -> Result<Value, String> {
        match run(app, h, 1, cmd) {
            ServerMsg::Result(r) if r.ok => Ok(r.data.unwrap_or_default()),
            ServerMsg::Result(r) => Err(r.error.unwrap_or_default()),
            other => panic!("{other:?}"),
        }
    }

    /// The same for a command answered later, from a task: waits for it.
    async fn ask_later(app: &mut App, h: &mut crate::web::ConnHandle, cmd: Command) -> Result<Value, String> {
        app.web_command(Inbound { conn: h.id, req: 7, cmd });
        let m = tokio::time::timeout(std::time::Duration::from_secs(20), h.recv()).await.expect("an answer in time").expect("the connection is open");
        match m {
            crate::web::Outbound::Text(t) => match serde_json::from_str::<ServerMsg>(&t).unwrap() {
                ServerMsg::Result(r) if r.ok => Ok(r.data.unwrap_or_default()),
                ServerMsg::Result(r) => Err(r.error.unwrap_or_default()),
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        }
    }

    fn secret(s: &str) -> crate::web::protocol::Secret {
        crate::web::protocol::Secret(s.into())
    }

    /// `run_stop` ends the active run as "stopped" (the stage says so, not "failed"), and says
    /// when there is none; `start_run` takes a width for this run within the range.
    #[test]
    fn run_stop_stops_the_active_run_and_start_run_checks_its_width() {
        with_mantra_home(|| {
            let (mut app, mut h, _rx) = wired();
            assert_eq!(ask(&mut app, &mut h, Command::RunStop), Err("no active run to stop".into()));
            let mut run_ = crate::engine::run::Run::new(std::path::PathBuf::from("."), Pattern::builtin(), "goal".into());
            run_.stage = Stage::Planning;
            app.run = Some(run_);
            let id = app.run.as_ref().unwrap().id.clone();
            assert_eq!(ask(&mut app, &mut h, Command::RunStop).map(|d| d["run"].as_str().map(String::from)), Ok(Some(id)));
            let r = app.run.as_ref().unwrap();
            assert!(!r.is_active());
            let view = crate::web::snapshot::run_view(&app, r);
            assert!(view.stage.stopped && view.stage.kind == "failed" && view.stage.label == "stopped", "{:?}", view.stage);
            assert_eq!(ask(&mut app, &mut h, Command::RunStop), Err("no active run to stop".into()), "stopping twice");
            for n in [0, 129] {
                let e = ask(&mut app, &mut h, Command::StartRun { goal: "next".into(), pattern: None, parallel: Some(n) }).unwrap_err();
                assert!(e.contains("from 1 to 128"), "{n}: {e}");
            }
            app.run = None;
        });
    }

    /// The patterns API end to end: the list, one to edit, a copy saved and refused, deleted —
    /// and a deleted pattern that new runs used makes way for the default.
    #[test]
    fn patterns_are_listed_edited_copied_and_deleted() {
        with_mantra_home(|| {
            let (mut app, mut h, _rx) = wired();
            let list = ask(&mut app, &mut h, Command::PatternsList).unwrap();
            let names: Vec<&str> = list["patterns"].as_array().unwrap().iter().filter_map(|p| p["name"].as_str()).collect();
            assert_eq!(names, Pattern::names());
            assert_eq!(list["current"], app.pattern_name.as_str());
            assert!(ask(&mut app, &mut h, Command::PatternGet { name: "nope".into() }).unwrap_err().contains("not found"));
            let got = ask(&mut app, &mut h, Command::PatternGet { name: "mantra-default".into() }).unwrap();
            assert_eq!((got["tag"].as_str(), got["builtin"].as_bool(), got["yours"].as_bool()), (Some("default"), Some(true), Some(false)));
            assert_eq!(got["pattern"]["roles"]["worker-swarm"]["parallel"], 16);
            assert!(got["toml"].as_str().unwrap().contains("[roles.worker-swarm]"));
            assert!(got["options"]["kinds"].as_array().unwrap().iter().any(|k| k == "worker"));

            // a copy under a new name: yours, and the start-run picker hears of it
            let mut p = got["pattern"].clone();
            p["description"] = json!("my take");
            let saved = ask(&mut app, &mut h, Command::PatternSave { name: "My Copy".into(), pattern: p.clone() }).unwrap();
            assert_eq!((saved["name"].as_str(), saved["tag"].as_str()), (Some("my-copy"), Some("yours")));
            assert_eq!(Pattern::load("my-copy", &app.project).unwrap().description, "my take");
            assert!(app.web.as_ref().unwrap().shared.patterns_gen() > 0, "the cached pattern list is stale now");
            // refused (the copy edited, so its JSON names it): every reason, one per line
            let mut bad = p.clone();
            bad["name"] = json!("my-copy");
            bad["roles"]["planner"]["kind"] = json!("boss");
            bad["settings"]["max_parallel"] = json!(0);
            let e = ask(&mut app, &mut h, Command::PatternSave { name: "my-copy".into(), pattern: bad }).unwrap_err();
            assert!(e.lines().count() >= 2 && e.contains("kind must be one of") && e.contains("max_parallel"), "{e}");
            assert!(ask(&mut app, &mut h, Command::PatternSave { name: "mantra-lean".into(), pattern: p.clone() }).unwrap_err().contains("built-in"));
            assert!(ask(&mut app, &mut h, Command::PatternSave { name: "x".into(), pattern: json!({"roles": 3}) }).unwrap_err().starts_with("not a pattern"));
            // the built-in edited in place: a file of its own, "edited"; deleted, it is the built-in again
            let edited = ask(&mut app, &mut h, Command::PatternSave { name: "mantra-default".into(), pattern: p }).unwrap();
            assert_eq!(edited["tag"], "edited");
            assert!(ask(&mut app, &mut h, Command::PatternDelete { name: "mantra-default".into() }).is_ok());
            assert!(ask(&mut app, &mut h, Command::PatternDelete { name: "mantra-default".into() }).unwrap_err().contains("built in"));

            // deleting the pattern new runs use: both fall back to the default
            app.settings.default_pattern = "my-copy".into();
            app.pattern_name = "my-copy".into();
            assert!(ask(&mut app, &mut h, Command::PatternDelete { name: "my-copy".into() }).is_ok());
            assert_eq!((app.settings.default_pattern.as_str(), app.pattern_name.as_str()), ("mantra-default", "mantra-default"));
            assert!(ask(&mut app, &mut h, Command::PatternDelete { name: "my-copy".into() }).unwrap_err().contains("no pattern"));
        });
    }

    /// `settings_get` never says the password, `settings_set` refuses what `set_field` refuses,
    /// and a remote session may not change the password that guards it.
    #[test]
    fn settings_hide_the_password_and_validate_what_is_set() {
        let (mut app, mut h, _rx) = wired();
        app.settings.web.password = "hunter2-secret".into();
        let got = ask(&mut app, &mut h, Command::SettingsGet).unwrap();
        assert!(!got.to_string().contains("hunter2"), "{got}");
        let field = |v: &Value, key: &str| v["fields"].as_array().unwrap().iter().find(|f| f["key"] == key).cloned().unwrap();
        assert_eq!((field(&got, "web.password")["kind"].as_str(), field(&got, "web.password")["value"].as_str()), (Some("secret"), Some("set")));
        assert_eq!(field(&got, "fps")["max"], 60);
        assert!(field(&got, "default_model")["options"].as_array().unwrap().iter().any(|o| o == "luna"));
        assert!(ask(&mut app, &mut h, Command::SettingsSet { key: "fps".into(), value: secret("1000") }).unwrap_err().contains("from 4 to 60"));
        assert!(ask(&mut app, &mut h, Command::SettingsSet { key: "default_model".into(), value: secret("nope") }).unwrap_err().contains("not in /models"));
        let set = ask(&mut app, &mut h, Command::SettingsSet { key: "default_model".into(), value: secret("luna") }).unwrap();
        assert_eq!(set["note"], "Solo's next session (/new) uses it");
        assert_eq!(field(&set["settings"], "default_model")["value"], "luna");
        let set = ask(&mut app, &mut h, Command::SettingsSet { key: "approval_mode".into(), value: secret("never") }).unwrap();
        assert_eq!((set["note"].as_str(), app.settings.approval_mode.as_str()), (Some(""), "never"));
        let mut remote = app.web.as_ref().unwrap().reg.register(ConnKind::Relay);
        assert!(ask(&mut app, &mut remote, Command::SettingsSet { key: "web.password".into(), value: secret("x") }).unwrap_err().contains("not from a remote session"));
        assert_eq!(app.settings.web.password, "hunter2-secret");
        assert!(ask(&mut app, &mut remote, Command::SettingsSet { key: "notify".into(), value: secret("off") }).is_ok(), "anything else is fine from there");
    }

    /// MCP servers from the web: added from a command line, switched, removed — saved to
    /// settings.toml as `mantra mcp` saves them; an env value never comes back.
    #[test]
    fn mcp_servers_are_added_switched_and_removed_on_disk() {
        with_mantra_home(|| {
            let (mut app, mut h, _rx) = wired();
            app.demo = false;
            let env = Some([("TOKEN".to_string(), secret("tok-123456"))].into());
            let got = ask(&mut app, &mut h, Command::McpSet { name: "blender".into(), command: Some("uvx blender-mcp".into()), args: None, env, enabled: None }).unwrap();
            assert!(!got.to_string().contains("tok-123456"), "{got}");
            let row = got["settings"]["mcp"].as_array().unwrap().iter().find(|m| m["name"] == "blender").cloned().unwrap();
            assert_eq!((row["line"].as_str(), row["env"].clone(), row["enabled"].as_bool()), (Some("uvx blender-mcp"), json!(["TOKEN"]), Some(true)));
            let disk = || crate::config::Settings::load_on_disk().unwrap().mcp_servers;
            assert_eq!(disk()["blender"].args, vec!["blender-mcp".to_string()]);
            assert!(app.settings.mcp_servers.contains_key("blender"), "the session has it too");
            ask(&mut app, &mut h, Command::McpSet { name: "blender".into(), command: None, args: None, env: None, enabled: Some(false) }).unwrap();
            assert!(!disk()["blender"].enabled && disk()["blender"].env.contains_key("TOKEN"));
            assert!(ask(&mut app, &mut h, Command::McpSet { name: "a/b".into(), command: Some("uvx".into()), args: None, env: None, enabled: None }).unwrap_err().contains("letters"));
            assert!(ask(&mut app, &mut h, Command::McpSet { name: "ghost".into(), command: None, args: None, env: None, enabled: Some(true) }).unwrap_err().contains("no MCP server named ghost"));
            ask(&mut app, &mut h, Command::McpRemove { name: "blender".into() }).unwrap();
            assert!(disk().is_empty() && app.settings.mcp_servers.is_empty());
            assert!(ask(&mut app, &mut h, Command::McpRemove { name: "blender".into() }).unwrap_err().contains("none configured"));
        });
    }

    /// `mcp_test` is answered from a task once the server has spoken — here, failed to start.
    #[tokio::test]
    async fn mcp_test_is_answered_once_the_server_has_spoken() {
        let (mut app, mut h, _rx) = wired();
        assert!(ask_later(&mut app, &mut h, Command::McpTest { name: "nope".into() }).await.unwrap_err().contains("no MCP server named nope"));
        app.settings.mcp_servers.insert("ghost".into(), crate::config::McpServer { command: "/nonexistent/mantra-test-mcp".into(), ..Default::default() });
        let e = ask_later(&mut app, &mut h, Command::McpTest { name: "ghost".into() }).await.unwrap_err();
        assert!(e.contains("could not start"), "{e}");
    }

    /// The registry as the web sees it: keys as dots and their last four, the Codex account as a
    /// row of its own, whether each add-on backend can be added.
    #[test]
    fn models_get_masks_keys_and_shows_the_codex_account() {
        let (mut app, mut h, _rx) = wired();
        app.registry.providers[0].api_key = Some("sk-proj-abcdefgh1234".into());
        crate::provider_key::add_backend_with(&mut app.registry, crate::provider_key::Backend::Codex, true).unwrap();
        let got = ask(&mut app, &mut h, Command::ModelsGet).unwrap();
        assert!(!got.to_string().contains("abcdefgh"), "{got}");
        let providers = got["providers"].as_array().unwrap();
        assert_eq!((providers[0]["id"].as_str(), providers[0]["account"].as_bool(), providers[0]["models"].as_u64()), (Some("openai"), Some(true), Some(4)));
        let bija = providers.iter().find(|p| p["id"] == "bija").unwrap();
        assert_eq!((bija["api_key"].as_str(), bija["key_set"].as_bool(), bija["wire"].as_str()), (Some("••••1234"), Some(true), Some("openai")));
        let sol = got["models"].as_array().unwrap().iter().find(|m| m["alias"] == "sol").unwrap();
        assert_eq!((sol["provider"].as_str(), sol["backend"].as_str(), sol["default"].as_bool()), (Some("bija"), Some("bija"), Some(true)));
        assert_eq!(got["backends"]["codex"]["added"], true);
    }

    /// A model cell from the web lands in models.toml; a refused one changes nothing, and Solo's
    /// default model follows a new alias.
    #[test]
    fn model_set_round_trips_through_models_toml() {
        with_mantra_home(|| {
            let (mut app, mut h, _rx) = wired();
            app.demo = false;
            assert!(ask(&mut app, &mut h, Command::ModelSet { alias: "luna".into(), field: "context".into(), value: "lots".into() }).unwrap_err().contains("200k"));
            let got = ask(&mut app, &mut h, Command::ModelSet { alias: "luna".into(), field: "note".into(), value: "quick one".into() }).unwrap();
            assert_eq!(got["alias"], "luna");
            assert_eq!(crate::config::Registry::load().get("luna").map(|m| m.note.clone()).as_deref(), Some("quick one"));
            ask(&mut app, &mut h, Command::ModelSet { alias: "luna".into(), field: "context".into(), value: "128k".into() }).unwrap();
            assert_eq!(crate::config::Registry::load().get("luna").and_then(|m| m.context_window), Some(128_000));
            let got = ask(&mut app, &mut h, Command::ModelSet { alias: "sol".into(), field: "alias".into(), value: "sun".into() }).unwrap();
            assert_eq!((got["alias"].as_str(), app.settings.default_model.as_str()), (Some("sun"), "sun"), "the default follows");
            assert!(crate::config::Registry::load().get("sun").is_some());
            assert!(ask(&mut app, &mut h, Command::SettingsSet { key: "notify".into(), value: secret("off") }).is_ok(), "and later settings still save");
        });
    }

    /// Models added and deleted: the last one and Solo's default stay.
    #[test]
    fn model_add_and_delete_keep_the_default() {
        let (mut app, mut h, _rx) = wired();
        let got = ask(&mut app, &mut h, Command::ModelAdd { provider: "bija".into(), model: Some("mx-5.2".into()), alias: None }).unwrap();
        assert_eq!(got["alias"], "mx-5.2");
        assert!(got["registry"]["models"].as_array().unwrap().iter().any(|m| m["alias"] == "mx-5.2"));
        assert!(ask(&mut app, &mut h, Command::ModelAdd { provider: "nowhere".into(), model: None, alias: None }).unwrap_err().contains("no provider named nowhere"));
        assert!(ask(&mut app, &mut h, Command::ModelDelete { alias: "mx-5.2".into() }).is_ok());
        assert!(ask(&mut app, &mut h, Command::ModelDelete { alias: "mx-5.2".into() }).unwrap_err().contains("no model named"));
        assert!(ask(&mut app, &mut h, Command::ModelDelete { alias: "sol".into() }).unwrap_err().contains("Solo's default model"));
        app.registry.models.retain(|m| m.alias == "sol");
        assert!(ask(&mut app, &mut h, Command::ModelDelete { alias: "sol".into() }).unwrap_err().contains("last model"));
    }

    /// A provider cell from the web lands in models.toml — a pasted key as well, which no reply
    /// shows — and a new id takes its models along.
    #[test]
    fn provider_set_round_trips_and_never_echoes_the_key() {
        with_mantra_home(|| {
            let (mut app, mut h, _rx) = wired();
            app.demo = false;
            let got = ask(&mut app, &mut h, Command::ProviderSet { id: "bija".into(), field: "api_key".into(), value: secret("sk-test-0123456789") }).unwrap();
            assert!(!got.to_string().contains("0123456789") && got.to_string().contains("••••6789"), "{got}");
            let reg = crate::config::Registry::load();
            assert_eq!(reg.providers.iter().find(|p| p.id == "bija").and_then(|p| p.api_key.as_deref()), Some("sk-test-0123456789"));
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(crate::config::Registry::path()).unwrap().permissions().mode() & 0o777;
                assert_eq!(mode, 0o600, "a key in models.toml makes it yours only");
            }
            assert!(ask(&mut app, &mut h, Command::ProviderSet { id: "bija".into(), field: "wire".into(), value: secret("grpc") }).unwrap_err().contains("openai"));
            assert!(ask(&mut app, &mut h, Command::ProviderSet { id: "openai".into(), field: "name".into(), value: secret("x") }).unwrap_err().contains("Codex account"));
            let got = ask(&mut app, &mut h, Command::ProviderSet { id: "bija".into(), field: "id".into(), value: secret("main") }).unwrap();
            assert_eq!(got["id"], "main");
            let reg = crate::config::Registry::load();
            assert!(reg.providers.iter().any(|p| p.id == "main") && reg.models.iter().all(|m| m.provider == "main"));
        });
    }

    /// A provider goes with its models; the Codex row with the whole account; never Solo's default.
    #[test]
    fn provider_delete_takes_its_models_and_keeps_the_default() {
        let (mut app, mut h, _rx) = wired();
        let gw = crate::provider_key::add_provider(&mut app.registry, &crate::provider_key::provider_from_key("k", Some("https://inference.test-gateway.dev/v1"))).unwrap();
        app.registry.models.push(crate::config::ModelEntry { alias: "fast".into(), provider: gw.clone(), model: "fast-1".into(), ..Default::default() });
        assert_eq!(ask(&mut app, &mut h, Command::ProviderDelete { id: gw }).unwrap()["models"], 1);
        assert!(app.registry.get("fast").is_none());
        assert!(ask(&mut app, &mut h, Command::ProviderDelete { id: "bija".into() }).unwrap_err().contains("Solo's default model"));
        assert!(ask(&mut app, &mut h, Command::ProviderDelete { id: "openai".into() }).unwrap_err().contains("no Codex models"));
        crate::provider_key::add_backend_with(&mut app.registry, crate::provider_key::Backend::Codex, true).unwrap();
        assert_eq!(ask(&mut app, &mut h, Command::ProviderDelete { id: "openai".into() }).unwrap()["models"], 4);
        assert!(app.registry.codex_account_models().is_empty());
        assert!(ask(&mut app, &mut h, Command::ProviderDelete { id: "nope".into() }).unwrap_err().contains("no provider named nope"));
    }

    /// The key flow from the web: what a key looks like without adding anything, then the
    /// provider added and its list asked for off the App task — a refused key keeps it.
    #[tokio::test]
    async fn provider_from_key_drafts_then_probes_off_the_app_task() {
        use axum::routing::get;
        let (mut app, mut h, _rx) = wired();
        let draft = ask_later(&mut app, &mut h, Command::ProviderFromKey { key: secret("sk-abc-123"), base_url: None, probe: Some(false) }).await.unwrap();
        let guess = crate::config::bija_default_provider().base_url;
        assert_eq!((draft["draft"]["base_url"].as_str(), draft["draft"]["guessed"].as_bool()), (Some(guess.as_str()), Some(true)), "the key's prefix names the API");
        assert!(!draft.to_string().contains("sk-abc-123"));
        assert_eq!(draft["draft"]["existing"], "bija", "a fresh install's provider is at that URL: the key would go into it");
        assert_eq!(app.registry.providers.len(), 1, "a draft adds nothing");
        let unknown = ask_later(&mut app, &mut h, Command::ProviderFromKey { key: secret("ip_x"), base_url: None, probe: Some(false) }).await.unwrap();
        assert_eq!((unknown["draft"]["needs_url"].as_bool(), unknown["draft"]["existing"].is_null()), (Some(true), true));
        assert!(ask_later(&mut app, &mut h, Command::ProviderFromKey { key: secret("ip_x"), base_url: None, probe: None }).await.unwrap_err().contains("base URL"));

        let server = axum::Router::new().route(
            "/v1/models",
            get(|hd: axum::http::HeaderMap| async move {
                if hd.get("authorization").and_then(|v| v.to_str().ok()) == Some("Bearer good-key") {
                    (axum::http::StatusCode::OK, r#"{"data":[{"id":"alpha","context_length":64000},{"id":"beta"}]}"#)
                } else {
                    (axum::http::StatusCode::UNAUTHORIZED, r#"{"error":{"message":"bad key"}}"#)
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://127.0.0.1:{}/v1", listener.local_addr().unwrap().port());
        tokio::spawn(async move { axum::serve(listener, server).await.ok() });
        let got = ask_later(&mut app, &mut h, Command::ProviderFromKey { key: secret("good-key"), base_url: Some(url.clone()), probe: None }).await.unwrap();
        assert_eq!(got["provider"], "local");
        let models: Vec<&str> = got["candidates"].as_array().unwrap().iter().filter_map(|c| c["model"].as_str()).collect();
        assert_eq!(models, ["alpha", "beta"]);
        assert!(got["candidates"][0]["selected"].as_bool().unwrap() && got["probe_error"].is_null());
        assert!(!got.to_string().contains("good-key"), "{got}");
        let refused = ask_later(&mut app, &mut h, Command::ProviderFromKey { key: secret("bad-key"), base_url: Some(url), probe: None }).await.unwrap();
        assert!(refused["probe_error"].as_str().unwrap().starts_with("the key was refused"), "{refused}");
        assert_eq!(app.registry.providers.iter().filter(|p| p.id == "local").count(), 1, "the same URL is the same provider");

        // the chosen rows, imported once
        let pick = vec![crate::web::ops::CandidateView { model: "alpha".into(), context: Some(64_000), context_known: true, ..Default::default() }];
        let got = ask(&mut app, &mut h, Command::ModelsImport { provider: "local".into(), candidates: pick.clone() }).unwrap();
        assert_eq!((got["added"].as_u64(), app.registry.get("alpha").map(|m| m.provider.as_str())), (Some(1), Some("local")));
        assert!(ask(&mut app, &mut h, Command::ModelsImport { provider: "local".into(), candidates: pick }).unwrap_err().contains("there already"));
        assert!(ask(&mut app, &mut h, Command::ModelsImport { provider: "local".into(), candidates: vec![] }).unwrap_err().contains("nothing selected"));
        assert!(ask(&mut app, &mut h, Command::ModelsImport { provider: "nowhere".into(), candidates: vec![Default::default()] }).unwrap_err().contains("no provider"));
    }

    /// `model_test` waits for its probe agent: answered with the row's result once the agent's
    /// turn is over (`on_turn_done` takes it out of `probes`).
    #[tokio::test]
    async fn model_test_is_answered_when_its_probe_is_done() {
        let (mut app, mut h, _rx) = wired();
        assert_eq!(ask(&mut app, &mut h, Command::ModelTest { alias: "nope".into() }), Err("unknown model".into()));
        app.web_command(Inbound { conn: h.id, req: 9, cmd: Command::ModelTest { alias: "luna".into() } });
        assert!(h.rx.try_recv().is_err(), "not answered while the probe runs");
        let (probe, _) = app.probes.iter().find(|(_, (a, _))| a == "luna").map(|(id, x)| (*id, x.clone())).expect("a probe agent");
        let link = app.web.clone().unwrap();
        crate::web::ops::settle(&link.shared, &link.reg, &app);
        assert!(h.rx.try_recv().is_err(), "still running");
        app.probes.remove(&probe);
        app.models_ui.status.insert("luna".into(), "ok 0.4s · \"OK\"".into());
        crate::web::ops::settle(&link.shared, &link.reg, &app);
        match reply(&mut h) {
            ServerMsg::Result(r) => assert_eq!((r.req, r.ok, r.data.unwrap()["status"].as_str().map(String::from)), (9, true, Some("ok 0.4s · \"OK\"".into()))),
            other => panic!("{other:?}"),
        }
    }

    /// The add-on backends: the demo's are mocks, added whatever is installed, once.
    #[test]
    fn add_backend_adds_once_and_knows_its_kinds() {
        let (mut app, mut h, _rx) = wired();
        assert_eq!(ask(&mut app, &mut h, Command::AddBackend { kind: "codex".into() }).unwrap()["added"], 4);
        assert_eq!(ask(&mut app, &mut h, Command::AddBackend { kind: "codex".into() }).unwrap()["added"], 0);
        assert!(ask(&mut app, &mut h, Command::AddBackend { kind: "claude-code".into() }).unwrap()["added"].as_u64().unwrap() > 0);
        assert!(ask(&mut app, &mut h, Command::AddBackend { kind: "vim".into() }).unwrap_err().contains("codex or claude-code"));
    }
}
