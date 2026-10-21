//! The shared vocabulary of the harness (api ↔ agent ↔ tools ↔ session), and the small helpers
//! more than one module needs (text clipping, the `chars / 4` estimate, ids, time). The shapes are
//! the contract between the modules — `Message` is also the transcript's record format — so they
//! stay as they are; helpers are added here rather than copied into a second module.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// One message of the conversation as the model sees it (wire-agnostic). The system prompt is not a
/// message: it lives in the session meta and each wire sends it its own way.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum Message {
    User {
        text: String,
    },
    Assistant {
        #[serde(default)]
        text: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        reasoning: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ToolCall>,
    },
    Tool {
        call_id: String,
        name: String,
        text: String,
        #[serde(default)]
        is_error: bool,
    },
}

/// A tool call the model made. `args` is the parsed JSON object, or `Value::Null` when the model's
/// argument text was not valid JSON (then `raw` holds it, so the agent can tell the model).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub args: Value,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub raw: String,
}

/// A tool the model may call (OpenAI function shape, wire-agnostic; `parameters` is a JSON schema).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

/// Token counts of one API call (`input` includes cached and cache-creation tokens, so it is the
/// context the model saw), or a running total. `estimated` when derived from text length.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cached: u64,
    #[serde(default)]
    pub estimated: bool,
}

/// The API dialect a provider speaks (SPEC §7.3, §7.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Wire {
    #[default]
    OpenaiChat,
    Anthropic,
}

impl Wire {
    pub fn parse(s: &str) -> Option<Wire> {
        match s.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "" | "openai" | "openai_chat" | "chat" => Some(Wire::OpenaiChat),
            "anthropic" | "messages" => Some(Wire::Anthropic),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Wire::OpenaiChat => "openai_chat",
            Wire::Anthropic => "anthropic",
        }
    }
}

/// Why an API call (or a turn) failed. Mirrors SPEC §4.2 `error.kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    Transient,
    RateLimit,
    Auth,
    Context,
    BadRequest,
    Usage,
    Interrupted,
    Internal,
}

impl ErrorKind {
    pub fn retryable(self) -> bool {
        matches!(self, ErrorKind::Transient | ErrorKind::RateLimit)
    }
}

/// A failed API call (or turn), classified; `turn_done.error` carries it as is.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ApiError {
    pub kind: ErrorKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u64>,
}

impl ApiError {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> ApiError {
        ApiError { kind, status: None, message: message.into(), retry_after_ms: None }
    }
    pub fn internal(message: impl Into<String>) -> ApiError {
        ApiError::new(ErrorKind::Internal, message)
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.status {
            Some(s) => write!(f, "HTTP {s}: {}", self.message),
            None => f.write_str(&self.message),
        }
    }
}

/// What the api layer streams back while a completion runs.
#[derive(Debug, Clone, PartialEq)]
pub enum Delta {
    Text(String),
    Reasoning(String),
    ToolCallStart { index: usize, id: String, name: String },
    ToolCallArgs { index: usize, chunk: String },
}

/// Told to the agent before each retry wait (SPEC §7.2).
#[derive(Debug, Clone, PartialEq)]
pub struct RetryNote {
    pub attempt: u32,
    pub max: u32,
    pub error: ApiError,
    pub wait: std::time::Duration,
}

/// One completion request. `effort` empty = send no reasoning field.
pub struct Request<'a> {
    pub model: &'a str,
    pub effort: &'a str,
    pub system: &'a str,
    pub messages: &'a [Message],
    pub tools: &'a [ToolDef],
    /// `None` = the provider's default (openai-chat) / the configured default (anthropic requires one).
    pub max_output_tokens: Option<u32>,
}

/// Why the model stopped (`finish_reason` / `stop_reason`, normalized across wires).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Finish {
    Stop,
    ToolCalls,
    Length,
    Other(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Response {
    /// Always `Message::Assistant`.
    pub message: Message,
    pub usage: Usage,
    pub finish: Finish,
}

/// A file touched by a tool (SPEC §6.2).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FileChange {
    pub path: String,
    /// `add` | `modified` | `delete`
    pub kind: String,
    #[serde(default)]
    pub diff: String,
}

/// The result of one tool call (SPEC §6.2).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ToolOutcome {
    pub ok: bool,
    /// Exactly the text the model gets as the tool result.
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changes: Vec<FileChange>,
}

impl ToolOutcome {
    pub fn err(text: impl Into<String>) -> ToolOutcome {
        ToolOutcome { ok: false, text: text.into(), ..Default::default() }
    }
    pub fn ok(text: impl Into<String>) -> ToolOutcome {
        ToolOutcome { ok: true, text: text.into(), ..Default::default() }
    }
}

impl Message {
    /// The characters of this message a provider is sent: text, each call's name and arguments, a
    /// result's tool name and text — never the reasoning, which no wire sends back. What every
    /// `chars / 4` estimate counts.
    pub fn sent_chars(&self) -> usize {
        match self {
            Message::User { text } => text.len(),
            Message::Assistant { text, tool_calls, .. } => text.len() + tool_calls.iter().map(|c| c.name.len() + if c.raw.is_empty() { c.args.to_string().len() } else { c.raw.len() }).sum::<usize>(),
            Message::Tool { name, text, .. } => name.len() + text.len(),
        }
    }
}

/// `Message::sent_chars` over a history.
pub fn sent_chars(messages: &[Message]) -> usize {
    messages.iter().map(Message::sent_chars).sum()
}

/// Rough token estimate when the API reported none: 4 chars per token.
pub fn estimate_tokens(text_len: usize) -> u64 {
    (text_len as u64).div_ceil(4)
}

/// `s` cut to at most `max` characters, with `…` appended when anything was cut: for text shown
/// on one line (an error detail, a listing column, a log line).
pub fn clip(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

/// The longest start of `s` that is at most `max` bytes and ends on a char boundary.
pub fn prefix(s: &str, max: usize) -> &str {
    let mut end = max.min(s.len());
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// The longest end of `s` that is at most `max` bytes and starts on a char boundary.
pub fn suffix(s: &str, max: usize) -> &str {
    let mut start = s.len().saturating_sub(max);
    while !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

/// A JSON object's string field, `""` when it is missing or not a string — for reading what a
/// provider or a tools file sent, where an absent field is as good as an empty one.
pub fn str_at<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}

/// `path`, made absolute against the current directory when it is relative.
pub fn absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    std::env::current_dir().map(|d| d.join(path)).unwrap_or_else(|_| path.to_path_buf())
}

/// Seconds since the Unix epoch, for transcript records and session meta.
pub fn now_unix() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Fills `buf` from `ring`'s system RNG, or — should that ever fail — from a time/pid mix spread
/// over every byte (splitmix64). Good enough for what it feeds: ids and a retry jitter's seed.
pub fn random_bytes(buf: &mut [u8]) {
    use ring::rand::SecureRandom;
    if ring::rand::SystemRandom::new().fill(buf).is_ok() {
        return;
    }
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(1);
    let mut x = nanos ^ u64::from(std::process::id()).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    for chunk in buf.chunks_mut(8) {
        x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = x;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        chunk.copy_from_slice(&z.to_le_bytes()[..chunk.len()]);
    }
}

/// A random uuid v4.
pub fn uuid_v4() -> String {
    let mut b = [0u8; 16];
    random_bytes(&mut b);
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_round_trips_through_json_with_role_tag() {
        let m = Message::Assistant { text: "hi".into(), reasoning: String::new(), tool_calls: vec![ToolCall { id: "c1".into(), name: "bash".into(), args: serde_json::json!({"command": "ls"}), raw: String::new() }] };
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains("\"role\":\"assistant\""), "{s}");
        assert!(!s.contains("reasoning"), "empty reasoning is skipped: {s}");
        let back: Message = serde_json::from_str(&s).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn uuid_v4_has_the_right_shape() {
        let u = uuid_v4();
        let parts: Vec<&str> = u.split('-').collect();
        assert_eq!(parts.iter().map(|p| p.len()).collect::<Vec<_>>(), vec![8, 4, 4, 4, 12]);
        assert!(parts[2].starts_with('4'));
        assert_ne!(u, uuid_v4());
    }

    #[test]
    fn the_estimate_counts_what_the_provider_is_sent() {
        let m = vec![
            Message::User { text: "abcd".into() },
            Message::Assistant { text: "ef".into(), reasoning: "not sent back".into(), tool_calls: vec![ToolCall { id: "c".into(), name: "bash".into(), args: serde_json::json!({"a": 1}), raw: String::new() }] },
            Message::Tool { call_id: "c".into(), name: "bash".into(), text: "xyz".into(), is_error: false },
        ];
        assert_eq!(sent_chars(&m), 4 + (2 + 4 + 7) + (4 + 3));
        let unparsed = ToolCall { id: "c".into(), name: "x".into(), args: Value::Null, raw: "{oops".into() };
        assert_eq!(Message::Assistant { text: String::new(), reasoning: String::new(), tool_calls: vec![unparsed] }.sent_chars(), 1 + 5, "unparsed arguments count as sent: raw");
        assert_eq!((estimate_tokens(0), estimate_tokens(1), estimate_tokens(8)), (0, 1, 2));
    }

    #[test]
    fn text_is_cut_on_char_boundaries() {
        assert_eq!(clip("abcdef", 3), "abc…");
        assert_eq!(clip("äbc", 3), "äbc");
        assert_eq!(clip("", 0), "");
        assert_eq!(prefix("héllo", 2), "h", "never half a character");
        assert_eq!(prefix("abc", 10), "abc");
        assert_eq!(suffix("héllo", 4), "llo");
        assert_eq!(suffix("abc", 10), "abc");
    }

    #[test]
    fn json_fields_and_paths() {
        let v = serde_json::json!({"a": "x", "n": 1});
        assert_eq!((str_at(&v, "a"), str_at(&v, "n"), str_at(&v, "missing")), ("x", "", ""));
        assert_eq!(absolute(Path::new("/abs")), PathBuf::from("/abs"));
        assert!(absolute(Path::new("rel")).is_absolute());
    }

    #[test]
    fn random_bytes_fill_the_whole_buffer() {
        let mut a = [0u8; 13];
        let mut b = [0u8; 13];
        random_bytes(&mut a);
        random_bytes(&mut b);
        assert_ne!(a, b);
    }

    #[test]
    fn wire_parses_the_documented_spellings() {
        assert_eq!(Wire::parse("openai-chat"), Some(Wire::OpenaiChat));
        assert_eq!(Wire::parse(""), Some(Wire::OpenaiChat));
        assert_eq!(Wire::parse("Anthropic"), Some(Wire::Anthropic));
        assert_eq!(Wire::parse("grpc"), None);
    }
}
