//! The WebSocket protocol (design §5): one JSON message per text frame, `"t"`-tagged. The same
//! messages travel inside the end-to-end encrypted relay channel (`remote.rs`), so nothing here
//! knows about transports.

use super::snapshot::{AgentView, AppInfo, ApprovalView, ItemDelta, ItemView, PlanView, PulseView, RemoteInfo, RunView, ToastView};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub const PROTOCOL: u32 = 1;

/// Deserialize a present-but-`null` field as `Some(None)` (a delta's "this section is gone"),
/// while a missing field stays `None` via `#[serde(default)]`.
fn double_option<'de, T: Deserialize<'de>, D: Deserializer<'de>>(d: D) -> Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(d).map(Some)
}

// ───────────────────────────── client → server ─────────────────────────────

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ClientMsg {
    Hello {
        protocol: u32,
        #[serde(default)]
        client: String,
        #[serde(default)]
        ua: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        since: Option<u64>,
    },
    Cmd {
        req: u64,
        #[serde(flatten)]
        cmd: Command,
    },
    Ping,
    /// Relay mode: the answer to the host's JSON `ping` (WebSocket ping frames can't cross the
    /// blind relay as ours).
    Pong,
}

/// Why a client message was refused before it reached the App: `req` is echoed when the frame
/// carried one, so the client's pending promise settles.
#[derive(Debug, Clone, PartialEq)]
pub struct ParseError {
    pub req: Option<u64>,
    pub error: String,
}

impl ClientMsg {
    /// Parse one frame. Unknown commands and bad arguments are errors that still carry the `req`
    /// (a strict one-shot serde parse could not say which request failed).
    pub fn parse(text: &str) -> Result<ClientMsg, ParseError> {
        let v: Value = serde_json::from_str(text).map_err(|e| ParseError { req: None, error: format!("bad message: {e}") })?;
        let t = v.get("t").and_then(|t| t.as_str()).unwrap_or("");
        if t != "cmd" {
            return serde_json::from_value(v).map_err(|e| ParseError { req: None, error: format!("bad message: {e}") });
        }
        let req = v.get("req").and_then(|r| r.as_u64());
        let Some(req) = req else {
            return Err(ParseError { req: None, error: "cmd without req".into() });
        };
        let name = v.get("cmd").and_then(|c| c.as_str()).unwrap_or("").to_string();
        if !Command::NAMES.contains(&name.as_str()) {
            return Err(ParseError { req: Some(req), error: format!("unknown command {name}") });
        }
        match serde_json::from_value::<Command>(v) {
            Ok(cmd) => Ok(ClientMsg::Cmd { req, cmd }),
            Err(e) => Err(ParseError { req: Some(req), error: format!("bad arguments for {name}: {e}") }),
        }
    }
}

/// `push_subscribe`'s subscription, exactly as the browser's `PushSubscription.toJSON()` has it.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct PushSubscriptionJson {
    pub endpoint: String,
    pub keys: PushKeys,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct PushKeys {
    pub p256dh: String,
    pub auth: String,
}

/// Every command of design §6. Agent ids are the hub's `u32`s.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Command {
    Send {
        agent: u32,
        #[serde(default)]
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        force: Option<bool>,
    },
    RunInput {
        text: String,
    },
    StartRun {
        goal: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pattern: Option<String>,
    },
    PlanApprove,
    PauseResume,
    Interrupt {
        agent: u32,
    },
    Respawn {
        agent: u32,
    },
    Compact {
        agent: u32,
    },
    SetModel {
        agent: u32,
        alias: String,
    },
    SetEffort {
        agent: u32,
        effort: String,
    },
    StepEffort {
        agent: u32,
        delta: i32,
    },
    Approve {
        key: String,
        decision: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        answer: Option<String>,
    },
    ApprovalMode {
        mode: String,
    },
    DiscardQueue {
        agent: u32,
    },
    PopQueued {
        agent: u32,
    },
    Land,
    RunsList,
    RunResume {
        id: String,
    },
    RunDelete {
        id: String,
    },
    NewSolo,
    SetPattern {
        name: String,
    },
    Diff {
        agent: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
    },
    PlanMarkdown,
    FetchItems {
        agent: u32,
        before: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        count: Option<u32>,
    },
    PushSubscribe {
        subscription: PushSubscriptionJson,
        #[serde(default)]
        device: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prefs: Option<super::push::Prefs>,
    },
    PushUnsubscribe {
        endpoint: String,
    },
    PushPrefs {
        endpoint: String,
        prefs: super::push::Prefs,
    },
    PushTest {
        endpoint: String,
    },
    RemoteRotate,
}

impl Command {
    /// The wire names, for telling "unknown command" from "bad arguments".
    pub const NAMES: &'static [&'static str] = &[
        "send",
        "run_input",
        "start_run",
        "plan_approve",
        "pause_resume",
        "interrupt",
        "respawn",
        "compact",
        "set_model",
        "set_effort",
        "step_effort",
        "approve",
        "approval_mode",
        "discard_queue",
        "pop_queued",
        "land",
        "runs_list",
        "run_resume",
        "run_delete",
        "new_solo",
        "set_pattern",
        "diff",
        "plan_markdown",
        "fetch_items",
        "push_subscribe",
        "push_unsubscribe",
        "push_prefs",
        "push_test",
        "remote_rotate",
    ];

    /// The wire name of this command (for logs and error texts).
    pub fn name(&self) -> String {
        serde_json::to_value(self).ok().and_then(|v| v.get("cmd").and_then(|c| c.as_str()).map(|s| s.to_string())).unwrap_or_default()
    }
}

// ───────────────────────────── server → client ─────────────────────────────

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ServerMsg {
    Hello(HelloMsg),
    Snapshot(Box<SnapshotMsg>),
    Delta(Box<DeltaMsg>),
    Items(ItemsMsg),
    Result(ResultMsg),
    Note(NoteMsg),
    /// Relay mode keep-alive (the client answers `pong`).
    Ping,
    Pong,
    Bye {
        reason: String,
    },
}

impl ServerMsg {
    pub fn to_json(&self) -> String {
        // Serializing these types cannot fail (no non-string map keys, no custom errors); an empty
        // string would only ever be dropped by the client.
        serde_json::to_string(self).unwrap_or_default()
    }

    pub fn ok(req: u64, data: Value) -> ServerMsg {
        ServerMsg::Result(ResultMsg { req, ok: true, data: Some(data), error: None })
    }

    pub fn err(req: u64, error: impl Into<String>) -> ServerMsg {
        ServerMsg::Result(ResultMsg { req, ok: false, data: None, error: Some(error.into()) })
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct HelloMsg {
    pub protocol: u32,
    pub version: String,
    /// "local" | "relay"
    pub mode: String,
    pub conn: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vapid: Option<String>,
    pub tls: bool,
    pub push: bool,
}

/// An agent with its most recent transcript items (only in a full snapshot).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct AgentWithItems {
    #[serde(flatten)]
    pub agent: AgentView,
    pub items: Vec<ItemView>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct SnapshotMsg {
    pub seq: u64,
    pub app: AppInfo,
    pub agents: Vec<AgentWithItems>,
    pub run: Option<RunView>,
    pub plan: Option<PlanView>,
    pub approvals: Vec<ApprovalView>,
    pub toast: Option<ToastView>,
    pub remote: Option<RemoteInfo>,
    pub pulse: Vec<PulseView>,
}

/// Only the sections that changed since `seq - 1`. `run`/`plan`/`toast`/`remote` distinguish
/// "unchanged" (absent) from "gone" (`null`).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Default)]
pub struct DeltaMsg {
    pub seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<AppInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents: Option<Vec<AgentView>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents_removed: Option<Vec<u32>>,
    /// Keyed by the agent id as a string (JSON object keys).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub items: Option<BTreeMap<String, Vec<ItemDelta>>>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "double_option")]
    pub run: Option<Option<RunView>>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "double_option")]
    pub plan: Option<Option<PlanView>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pulse: Option<Vec<PulseView>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approvals: Option<Vec<ApprovalView>>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "double_option")]
    pub toast: Option<Option<ToastView>>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "double_option")]
    pub remote: Option<Option<RemoteInfo>>,
}

impl DeltaMsg {
    /// Nothing but the sequence number: no section changed.
    pub fn is_empty(&self) -> bool {
        self.app.is_none()
            && self.agents.is_none()
            && self.agents_removed.is_none()
            && self.items.is_none()
            && self.run.is_none()
            && self.plan.is_none()
            && self.pulse.is_none()
            && self.approvals.is_none()
            && self.toast.is_none()
            && self.remote.is_none()
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct ItemsMsg {
    pub req: u64,
    pub agent: u32,
    pub items: Vec<ItemView>,
    pub first: u64,
    pub total: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct ResultMsg {
    pub req: u64,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct NoteMsg {
    /// "halt" | "question" | "approval" | "review" | "done" | "failed" | "turn" | "info"
    pub kind: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<u32>,
    pub at: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn round_client(m: ClientMsg) {
        let s = serde_json::to_string(&m).unwrap();
        assert_eq!(ClientMsg::parse(&s).unwrap(), m, "{s}");
    }

    fn round_server(m: ServerMsg) {
        let s = m.to_json();
        let back: ServerMsg = serde_json::from_str(&s).unwrap_or_else(|e| panic!("{e}: {s}"));
        assert_eq!(back, m, "{s}");
    }

    #[test]
    fn every_client_message_round_trips() {
        round_client(ClientMsg::Hello { protocol: 1, client: "pwa".into(), ua: "x".into(), since: Some(4) });
        round_client(ClientMsg::Ping);
        round_client(ClientMsg::Pong);
        let prefs = crate::web::push::Prefs::default();
        let sub = PushSubscriptionJson { endpoint: "https://push.example/abc".into(), keys: PushKeys { p256dh: "BA".into(), auth: "AQ".into() } };
        let cmds = vec![
            Command::Send { agent: 1, text: "hi".into(), force: Some(true) },
            Command::Send { agent: 1, text: "hi".into(), force: None },
            Command::RunInput { text: "yes".into() },
            Command::StartRun { goal: "g".into(), pattern: Some("p".into()) },
            Command::PlanApprove,
            Command::PauseResume,
            Command::Interrupt { agent: 2 },
            Command::Respawn { agent: 2 },
            Command::Compact { agent: 2 },
            Command::SetModel { agent: 2, alias: "sol".into() },
            Command::SetEffort { agent: 2, effort: "high".into() },
            Command::StepEffort { agent: 2, delta: -1 },
            Command::Approve { key: "1:5".into(), decision: "yes".into(), answer: Some("a".into()) },
            Command::ApprovalMode { mode: "never".into() },
            Command::DiscardQueue { agent: 1 },
            Command::PopQueued { agent: 1 },
            Command::Land,
            Command::RunsList,
            Command::RunResume { id: "r".into() },
            Command::RunDelete { id: "r".into() },
            Command::NewSolo,
            Command::SetPattern { name: "p".into() },
            Command::Diff { agent: 1, path: Some("a.rs".into()) },
            Command::PlanMarkdown,
            Command::FetchItems { agent: 1, before: 10, count: Some(5) },
            Command::PushSubscribe { subscription: sub.clone(), device: "phone".into(), prefs: Some(prefs.clone()) },
            Command::PushUnsubscribe { endpoint: "e".into() },
            Command::PushPrefs { endpoint: "e".into(), prefs },
            Command::PushTest { endpoint: "e".into() },
            Command::RemoteRotate,
        ];
        assert_eq!(cmds.iter().map(|c| c.name()).collect::<std::collections::BTreeSet<_>>().len(), Command::NAMES.len(), "every command covered");
        for (i, cmd) in cmds.into_iter().enumerate() {
            round_client(ClientMsg::Cmd { req: i as u64, cmd });
        }
    }

    #[test]
    fn unknown_commands_and_bad_arguments_keep_the_req() {
        let e = ClientMsg::parse(r#"{"t":"cmd","req":7,"cmd":"quit"}"#).unwrap_err();
        assert_eq!(e, ParseError { req: Some(7), error: "unknown command quit".into() });
        let e = ClientMsg::parse(r#"{"t":"cmd","req":8,"cmd":"send","agent":"x"}"#).unwrap_err();
        assert_eq!(e.req, Some(8));
        assert!(e.error.starts_with("bad arguments for send"), "{}", e.error);
        assert!(ClientMsg::parse("not json").unwrap_err().req.is_none());
        // the documented wire shape parses as-is
        let m = ClientMsg::parse(r#"{"t":"cmd","req":1,"cmd":"send","agent":3,"text":"hello"}"#).unwrap();
        assert_eq!(m, ClientMsg::Cmd { req: 1, cmd: Command::Send { agent: 3, text: "hello".into(), force: None } });
    }

    #[test]
    fn every_server_message_round_trips() {
        round_server(ServerMsg::Hello(HelloMsg { protocol: 1, version: "0.5.0".into(), mode: "local".into(), conn: "3".into(), vapid: Some("BAAA".into()), tls: true, push: true }));
        round_server(ServerMsg::Items(ItemsMsg { req: 1, agent: 2, items: vec![crate::web::snapshot::tests::item(4)], first: 0, total: 9 }));
        round_server(ServerMsg::ok(3, json!({"a": 1})));
        round_server(ServerMsg::err(3, "no such agent"));
        round_server(ServerMsg::Note(NoteMsg { kind: "halt".into(), text: "t".into(), agent: Some(1), at: 5 }));
        round_server(ServerMsg::Ping);
        round_server(ServerMsg::Pong);
        round_server(ServerMsg::Bye { reason: "quit".into() });
        let (snap, delta) = crate::web::snapshot::tests::sample_messages();
        round_server(ServerMsg::Snapshot(Box::new(snap)));
        round_server(ServerMsg::Delta(Box::new(delta)));
        // "gone" survives the trip as Some(None), "unchanged" as None
        let d = DeltaMsg { seq: 2, run: Some(None), ..Default::default() };
        let s = ServerMsg::Delta(Box::new(d)).to_json();
        assert!(s.contains("\"run\":null") && !s.contains("plan"), "{s}");
        round_server(serde_json::from_str(&s).unwrap());
    }
}
