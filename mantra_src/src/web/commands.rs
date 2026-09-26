//! The web command table (design §6): each command is validated and executed on the App task,
//! through the same App methods the keyboard uses, and answered with a `result` (or `items`).

use super::protocol::{Command, ItemsMsg, ServerMsg};
use super::{ConnKind, Inbound};
use crate::agent::Level;
use crate::app::{prompt_agent, App};
use crate::engine::pattern::Pattern;
use crate::engine::run::{Send, Stage};
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
                if !self.hub.has_agent(agent) {
                    return Err("that agent's process has ended (respawn it, or start a new session)".into());
                }
                let mode = if force.unwrap_or(false) { Send::Force } else { Send::Queue };
                let text = text.trim_end().to_string();
                if text.trim().is_empty() && mode == Send::Queue {
                    return Err("empty message".into());
                }
                if Some(agent) == self.solo {
                    if let Some(sh) = text.strip_prefix('!') {
                        if let Some(a) = self.agents.get_mut(&agent) {
                            a.push_user(&text);
                        }
                        self.hub.send(agent, Cmd::Shell { command: sh.trim().to_string() });
                        return ok();
                    }
                }
                if text.trim().is_empty() && self.agents.get(&agent).map(|a| a.queued.is_empty()).unwrap_or(true) {
                    return Err("nothing queued to send".into());
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
            Command::StartRun { goal, pattern } => {
                if self.run.as_ref().map(|r| r.is_active()).unwrap_or(false) {
                    return Err("a run is already open".into());
                }
                if goal.trim().is_empty() {
                    return Err("describe the goal".into());
                }
                if let Some(p) = pattern.filter(|p| !p.is_empty()) {
                    if !Pattern::list(&self.project).contains(&p) {
                        return Err("unknown pattern".into());
                    }
                    self.pattern_name = p;
                }
                let before = self.run.as_ref().map(|r| r.id.clone());
                self.start_run(goal.trim());
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
                    Some(Ok(())) => ok(),
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
                if self.registry.get(&alias).is_none() {
                    return Err("unknown model".into());
                }
                let in_run = self.in_run(agent);
                self.set_model(agent, &alias);
                if in_run {
                    self.model_switched_for_run_agent(agent);
                }
                ok()
            }
            Command::SetEffort { agent, effort } => {
                self.agent_ok(agent)?;
                let alias = self.agents.get(&agent).map(|a| a.model_alias.clone()).unwrap_or_default();
                let efforts = self.registry.resolve(&alias).efforts();
                if efforts.is_empty() {
                    return Err("this model has no effort levels".into());
                }
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
                let runs: Vec<Value> = all
                    .iter()
                    .filter(|r| r.project_key == key)
                    .map(|r| {
                        json!({
                            "id": r.id,
                            "stage": r.stage_label(),
                            "updated_at": r.updated_unix,
                            "ago": crate::engine::state::fmt_ago(r.updated_unix),
                            "brief": r.brief(),
                            "unfinished": r.unfinished(),
                            "error": r.state.as_ref().err(),
                            "open": open.as_deref() == Some(r.id.as_str()),
                        })
                    })
                    .collect();
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
                    Err("that run is open".into())
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
            Command::FetchItems { .. } | Command::PushTest { .. } => Err("internal: handled above".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::Approval;
    use crate::web::snapshot::tests::{add_agent, test_app};
    use crate::web::{Link, LinkInfo, WebRegistryHandle};
    use std::time::Instant;

    /// An App wired to a registry with one local connection; returns the replies it gets.
    fn wired() -> (App, crate::web::ConnHandle, tokio::sync::mpsc::UnboundedReceiver<crate::hub::Cmd>) {
        let mut app = test_app();
        let (ctl, _c) = tokio::sync::mpsc::unbounded_channel();
        std::mem::forget(_c);
        let reg = WebRegistryHandle::new(ctl);
        let h = reg.register(ConnKind::Local);
        app.web = Some(Link { reg, remote: None, push: None, info: LinkInfo::default() });
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
    fn start_run_refuses_while_one_is_active() {
        let (mut app, mut h, _rx) = wired();
        let mut run_ = crate::engine::run::Run::new(std::path::PathBuf::from("."), Pattern::builtin(), "goal".into());
        run_.stage = Stage::Planning;
        app.run = Some(run_);
        let r = run(&mut app, &mut h, 1, Command::StartRun { goal: "another".into(), pattern: None });
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
}
