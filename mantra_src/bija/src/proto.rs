//! The stdio protocol (SPEC §4): `Op` lines come in on stdin (or a control client), `Event` lines
//! go out on stdout (and to every control client). Mantra's `hub/bija.rs` speaks exactly these
//! shapes, so a change here is a change to both sides.

use crate::types::{ApiError, FileChange};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One instruction to a session (SPEC §4.1).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    Prompt { text: String },
    Steer { text: String },
    Interrupt,
    Compact,
    Set {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effort: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
    Shell { command: String },
    ToolResult {
        id: String,
        #[serde(default = "default_true")]
        ok: bool,
        #[serde(default)]
        text: String,
    },
    Status,
    Shutdown,
}

fn default_true() -> bool {
    true
}

impl Op {
    /// Parses one stdin line. `Err` carries a message for `Event::Error`.
    pub fn parse(line: &str) -> Result<Op, String> {
        serde_json::from_str(line.trim()).map_err(|e| format!("bad op: {e}"))
    }
}

/// How a turn ended: `turn_done.status`, and the `status` of the transcript's `turn` record.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    Completed,
    Failed,
    Interrupted,
}

impl TurnStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            TurnStatus::Completed => "completed",
            TurnStatus::Failed => "failed",
            TurnStatus::Interrupted => "interrupted",
        }
    }
}

/// One thing that happened in a session (SPEC §4.2).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "ev", rename_all = "snake_case")]
pub enum Event {
    Ready { session: String, dir: String, model: String, wire: String, resumed: bool, pid: u32, pending: bool },
    TurnStarted { turn: String },
    TextDelta { turn: String, id: String, text: String },
    ReasoningDelta { turn: String, id: String, text: String },
    Message { turn: String, id: String, text: String },
    ToolStarted { turn: String, id: String, name: String, args: Value },
    ToolOutput { turn: String, id: String, text: String },
    ToolDone {
        turn: String,
        id: String,
        name: String,
        ok: bool,
        result: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        exit_code: Option<i32>,
        duration_ms: u64,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        changes: Vec<FileChange>,
    },
    ToolCall { turn: String, id: String, name: String, args: Value },
    Usage { turn: String, last_in: u64, last_out: u64, total: u64, cached: u64, window: u64, estimated: bool },
    Retry { turn: String, attempt: u32, max: u32, error: String, wait_ms: u64 },
    Compacted {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn: Option<String>,
        from: u64,
        to: u64,
    },
    TurnDone { turn: String, status: TurnStatus, error: Option<ApiError> },
    Warning { message: String },
    Status {
        session: String,
        busy: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn: Option<String>,
        turns: u64,
        model: String,
        effort: String,
        messages: usize,
        last_in: u64,
        total: u64,
        cached: u64,
        window: u64,
        queued: usize,
        jobs: usize,
    },
    Error { message: String },
    Exit { code: i32 },
}

impl Event {
    /// The one line this event is written as (no trailing newline).
    pub fn line(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|e| format!("{{\"ev\":\"error\",\"message\":\"serialize: {e}\"}}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ops_parse_the_documented_lines() {
        assert_eq!(Op::parse(r#"{"op":"prompt","text":"hi"}"#).unwrap(), Op::Prompt { text: "hi".into() });
        assert_eq!(Op::parse(r#"{"op":"interrupt"}"#).unwrap(), Op::Interrupt);
        assert_eq!(Op::parse(r#"{"op":"set","effort":"high"}"#).unwrap(), Op::Set { model: None, effort: Some("high".into()), name: None });
        assert_eq!(Op::parse(r#"{"op":"tool_result","id":"c1","text":"done"}"#).unwrap(), Op::ToolResult { id: "c1".into(), ok: true, text: "done".into() });
        assert!(Op::parse("nope").unwrap_err().starts_with("bad op"));
        assert!(Op::parse(r#"{"op":"dance"}"#).is_err());
    }

    #[test]
    fn events_serialize_with_the_ev_tag_and_snake_case() {
        let e = Event::TurnDone { turn: "t1".into(), status: TurnStatus::Completed, error: None };
        assert_eq!(e.line(), r#"{"ev":"turn_done","turn":"t1","status":"completed","error":null}"#);
        let e = Event::Compacted { turn: None, from: 10, to: 2 };
        assert_eq!(e.line(), r#"{"ev":"compacted","from":10,"to":2}"#);
        let e = Event::ToolDone { turn: "t1".into(), id: "c".into(), name: "bash".into(), ok: true, result: "x".into(), exit_code: Some(0), duration_ms: 5, changes: vec![] };
        assert!(e.line().contains("\"exit_code\":0") && !e.line().contains("changes"));
        for s in [TurnStatus::Completed, TurnStatus::Failed, TurnStatus::Interrupted] {
            assert_eq!(serde_json::to_value(&s).unwrap(), s.as_str(), "the record and the event spell it alike");
        }
    }
}
