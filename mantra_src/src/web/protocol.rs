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

/// A secret on its way in (a pasted API key, a password, a setting that may be one): on the wire
/// the plain string, in `Debug` never — a command is data, and data ends up in test failures and
/// log lines.
#[derive(Serialize, Deserialize, Clone, PartialEq, Default)]
#[serde(transparent)]
pub struct Secret(pub String);

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("\"••••\"")
    }
}

/// Every command of design §6. Agent ids are the hub's `u32`s.
///
/// 0.8 added the configuration commands (`run_stop` to `add_backend` below) — additive, so a UI
/// bundle built for 0.7 keeps working on protocol 1. They go through the same APIs the TUI's
/// `/settings`, `/models` and `/studio` do (`web::ops`); no reply carries a key or a password.
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
        /// How many workers at once for this run only, in every worker class (`--parallel N`,
        /// `pattern::PARALLEL_RANGE`); absent = what the pattern says.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parallel: Option<usize>,
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
    SetRoleModel {
        role: String,
        alias: String,
        #[serde(default)]
        all: bool,
    },
    /// A role's fallback model (`Role.fallback`) in the next run's pattern, saved like
    /// `set_role_model`: another alias than the role's own model, or none (`alias` null/absent).
    SetRoleFallback {
        role: String,
        #[serde(default)]
        alias: Option<String>,
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
    /// Stop the active run for good (`App::stop_run`): saved as "stopped", openable from the runs
    /// list to land or delete, and the way clear for `start_run`. → `{}`.
    RunStop,
    /// Every pattern as a list shows it (`Pattern::summaries`). → `{patterns, current, default}`.
    PatternsList,
    /// One pattern to edit. → `{pattern (JSON), toml, tag, builtin, edited, yours, path?, options}`;
    /// `options` holds the values its fields take (kinds, sandboxes, permissions, models, …).
    PatternGet {
        name: String,
    },
    /// Validate and save `pattern` (the JSON `pattern_get` gave, edited) under `name`: its own name
    /// saves over it (a built-in gets an edited copy in ~/.mantra/patterns), another one saves a
    /// copy (`Pattern::save_as`). → `{name, path, tag}`; refused with every reason, one per line.
    PatternSave {
        name: String,
        pattern: Value,
    },
    /// Delete a pattern's file (`Pattern::delete_file`): yours is gone, an edited built-in is the
    /// built-in again. → `{path}`.
    PatternDelete {
        name: String,
    },
    /// The settings as `/settings` shows them (`Settings::fields`), the MCP servers and what the
    /// web layer is serving. Secrets read "set" / "not set". → `{fields, mcp, web, demo}`.
    SettingsGet,
    /// One setting (`App::set_setting`), the value as text. → `{note, settings}` (`note`: when it
    /// takes effect; `settings`: the `settings_get` reply after the change).
    SettingsSet {
        key: String,
        value: Secret,
    },
    /// Add an MCP server, or change one: `command` with `args` absent is a command line, split
    /// the way a shell would (`settings_ops::parse_command_line`); `env` replaces the server's;
    /// `enabled` alone switches it. Saved to settings.toml. → `{settings}`.
    McpSet {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        command: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        args: Option<Vec<String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        env: Option<BTreeMap<String, Secret>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        enabled: Option<bool>,
    },
    /// → `{settings}`.
    McpRemove {
        name: String,
    },
    /// Start the server, shake hands, list its tools (`mantra mcp test`); answered once it has.
    /// → `{report}`.
    McpTest {
        name: String,
    },
    /// The model registry: models, providers (`api_key` as `••••` + its last four), the Codex
    /// account as a row of its own, which add-on backends can be added. → `{models, providers,
    /// backends, options}`.
    ModelsGet,
    /// One cell of a model (`alias`, `provider`, `model`, `context`, `compact`, `effort`,
    /// `efforts`, `vision`, `note`), saved to models.toml. → `{alias, registry}` (the alias after
    /// the change; `registry` is the `models_get` reply).
    ModelSet {
        alias: String,
        field: String,
        value: String,
    },
    /// A new model on `provider`; without `model` (and `alias`) a placeholder row to fill in.
    /// → `{alias, registry}`.
    ModelAdd {
        provider: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        alias: Option<String>,
    },
    /// → `{registry}`.
    ModelDelete {
        alias: String,
    },
    /// One cell of a provider (`id`, `name`, `base_url`, `env_key`, `api_key`, `kind`, `auth`,
    /// `wire`); a new id carries its models along. → `{id, registry}`.
    ProviderSet {
        id: String,
        field: String,
        value: Secret,
    },
    /// A provider and every model on it; `openai` is the Codex account (all its models).
    /// → `{models, registry}` (`models`: how many went with it).
    ProviderDelete {
        id: String,
    },
    /// A provider from its API key (`provider_key::provider_from_key`). `probe: false` only says
    /// what the key looks like → `{draft}`; otherwise the provider is added (or the key put into
    /// the one at that URL) and its model list asked for off the App task, answered once it is
    /// back → `{provider, draft, candidates, probe_error?, registry}`. A failed probe keeps the
    /// provider, for models by hand.
    ProviderFromKey {
        key: Secret,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        base_url: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        probe: Option<bool>,
    },
    /// Add the chosen `candidates` (rows of a `provider_from_key` reply) to `provider`.
    /// → `{added, updated, registry}`.
    ModelsImport {
        provider: String,
        candidates: Vec<super::ops::CandidateView>,
    },
    /// The live test of `/models`: a throwaway agent on the alias asked for one word, answered
    /// once it has. → `{status}` ("ok 0.4s · …"); a failed test is an error with the reason.
    ModelTest {
        alias: String,
    },
    /// An add-on backend's default models, `kind` "codex" | "claude-code"
    /// (`provider_key::add_backend`). → `{added, registry}`.
    AddBackend {
        kind: String,
    },
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
        "set_role_model",
        "set_role_fallback",
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
        "run_stop",
        "patterns_list",
        "pattern_get",
        "pattern_save",
        "pattern_delete",
        "settings_get",
        "settings_set",
        "mcp_set",
        "mcp_remove",
        "mcp_test",
        "models_get",
        "model_set",
        "model_add",
        "model_delete",
        "provider_set",
        "provider_delete",
        "provider_from_key",
        "models_import",
        "model_test",
        "add_backend",
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
            Command::StartRun { goal: "g".into(), pattern: Some("p".into()), parallel: Some(8) },
            Command::StartRun { goal: "g".into(), pattern: None, parallel: None },
            Command::PlanApprove,
            Command::PauseResume,
            Command::Interrupt { agent: 2 },
            Command::Respawn { agent: 2 },
            Command::Compact { agent: 2 },
            Command::SetModel { agent: 2, alias: "sol".into() },
            Command::SetRoleModel { role: "worker".into(), alias: "sol".into(), all: false },
            Command::SetRoleFallback { role: "worker".into(), alias: Some("luna".into()) },
            Command::SetRoleFallback { role: "worker".into(), alias: None },
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
            Command::RunStop,
            Command::PatternsList,
            Command::PatternGet { name: "p".into() },
            Command::PatternSave { name: "p".into(), pattern: json!({"name": "p", "roles": {}}) },
            Command::PatternDelete { name: "p".into() },
            Command::SettingsGet,
            Command::SettingsSet { key: "fps".into(), value: Secret("30".into()) },
            Command::McpSet { name: "blender".into(), command: Some("uvx blender-mcp".into()), args: None, env: Some([("TOKEN".to_string(), Secret("t".into()))].into()), enabled: Some(true) },
            Command::McpSet { name: "blender".into(), command: None, args: None, env: None, enabled: Some(false) },
            Command::McpRemove { name: "blender".into() },
            Command::McpTest { name: "blender".into() },
            Command::ModelsGet,
            Command::ModelSet { alias: "sol".into(), field: "note".into(), value: "x".into() },
            Command::ModelAdd { provider: "bija".into(), model: Some("m".into()), alias: None },
            Command::ModelDelete { alias: "m".into() },
            Command::ProviderSet { id: "bija".into(), field: "api_key".into(), value: Secret("sk-x".into()) },
            Command::ProviderDelete { id: "gw".into() },
            Command::ProviderFromKey { key: Secret("sk-x".into()), base_url: None, probe: Some(false) },
            Command::ModelsImport { provider: "gw".into(), candidates: vec![crate::web::ops::CandidateView { model: "m".into(), context: Some(8000), ..Default::default() }] },
            Command::ModelTest { alias: "sol".into() },
            Command::AddBackend { kind: "codex".into() },
        ];
        assert_eq!(cmds.iter().map(|c| c.name()).collect::<std::collections::BTreeSet<_>>().len(), Command::NAMES.len(), "every command covered");
        for (i, cmd) in cmds.into_iter().enumerate() {
            round_client(ClientMsg::Cmd { req: i as u64, cmd });
        }
    }

    /// A key or password in a command never shows in its `Debug` (test failures, log lines), and
    /// still travels as the plain string; a 0.7 UI's `start_run` (no `parallel`) still parses.
    #[test]
    fn secrets_are_plain_on_the_wire_and_hidden_in_debug() {
        let cmd = Command::ProviderFromKey { key: Secret("sk-live-123".into()), base_url: None, probe: None };
        assert!(!format!("{cmd:?}").contains("sk-live-123"), "{cmd:?}");
        let wire = serde_json::to_string(&ClientMsg::Cmd { req: 1, cmd }).unwrap();
        assert!(wire.contains(r#""key":"sk-live-123""#), "{wire}");
        let set = ClientMsg::parse(r#"{"t":"cmd","req":2,"cmd":"settings_set","key":"web.password","value":"hunter2"}"#).unwrap();
        assert!(!format!("{set:?}").contains("hunter2"));
        let old = ClientMsg::parse(r#"{"t":"cmd","req":3,"cmd":"start_run","goal":"g"}"#).unwrap();
        assert_eq!(old, ClientMsg::Cmd { req: 3, cmd: Command::StartRun { goal: "g".into(), pattern: None, parallel: None } });
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
        round_server(ServerMsg::Hello(HelloMsg { protocol: 1, version: "0.6.2".into(), mode: "local".into(), conn: "3".into(), vapid: Some("BAAA".into()), tls: true, push: true }));
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
