//! The API layer (SPEC §7): one [`Client`] per session, one [`Client::complete`] call per model
//! request, every retry inside it.
//!
//! The split: `http` moves bytes, `openai`/`anthropic` turn a wire-agnostic [`Request`] into a body
//! and a stream of events back into a [`Response`], and this file owns the policy — which failures
//! are worth another attempt (§7.2), how long to wait, and how errors are named. Classification
//! lives here, not in the wires, so both wires (and the proxy refusal in `http`) agree on it.
//!
//! Besides retries there is one more recovery: *adapting*. When a provider answers 400 to an
//! optional request feature — a reasoning control it does not know, `stream_options`, automatic
//! prompt caching — the wire drops that feature for the rest of the session and the request is
//! sent again at once. That is not a retry (nothing transient happened, so no wait and no
//! `on_retry`), and it is bounded: every adaptation switches a feature off for good.
//!
//! When the provider reports no usage, the response carries a `chars / 4` estimate flagged
//! `estimated` (SPEC §3), so the agent's bookkeeping never sees an unexplained zero.

pub mod anthropic;
pub mod http;
pub mod openai;

use crate::types::{clip, estimate_tokens, random_bytes, sent_chars, ApiError, Delta, ErrorKind, Message, Request, Response, RetryNote, ToolCall, Usage, Wire};
use serde_json::Value;
use std::time::Duration;

/// Error bodies are short; past this they are cut (the message is all we need from them).
const MAX_ERROR_BODY: usize = 64 * 1024;
/// A non-streamed completion holding this much is not a completion.
const MAX_BODY: usize = 32 * 1024 * 1024;
/// After a stream's terminal event, how long to wait for the body's end to keep the connection
/// (servers end it at once; one that holds it open costs this much per request, no more).
const DRAIN_GRACE: Duration = Duration::from_millis(250);
/// Backoff ceiling in backoff units (60 s), and the ceiling on a server's `Retry-After`.
const MAX_BACKOFF_UNITS: u32 = 60;
const MAX_RETRY_AFTER: Duration = Duration::from_secs(120);
/// Immediate resends per `complete` after an adaptation; each one turns a feature off for good,
/// so this only guards against a provider that rejects everything.
const MAX_ADAPTATIONS: u32 = 6;

#[derive(Debug, Clone)]
pub struct ClientConfig {
    pub base_url: String,
    pub wire: Wire,
    pub api_key: Option<String>,
    pub max_retries: u32,
    pub timeout: Duration,
    pub stream: bool,
    pub user_agent: String,
}

pub struct Client {
    cfg: ClientConfig,
    http: http::Pool,
    chat: openai::Quirks,
    messages: anthropic::Memory,
    /// One backoff step (1 s); tests shrink it so a retried stream does not really sleep.
    backoff_unit: Duration,
    /// xorshift state for the jitter; seeded per client so a swarm never retries in lockstep.
    rng: u64,
}

impl Client {
    pub fn new(cfg: ClientConfig) -> Client {
        let http = http::Pool::new(Duration::from_secs(20), cfg.timeout);
        Client { cfg, http, chat: openai::Quirks::default(), messages: anthropic::Memory::default(), backoff_unit: Duration::from_secs(1), rng: seed() }
    }

    /// One completion with retries (SPEC §7.1/§7.2). Cancel-safe: dropping the future aborts the
    /// request in flight and discards the connection.
    pub async fn complete(&mut self, req: &Request<'_>, on_delta: &mut dyn FnMut(Delta), on_retry: &mut dyn FnMut(&RetryNote)) -> Result<Response, ApiError> {
        let mut retries = 0;
        let mut adaptations = 0;
        loop {
            let err = match self.attempt(req, on_delta).await {
                Ok(resp) if !is_empty_completion(&resp) => return Ok(with_usage_estimate(resp, req)),
                Ok(_) => ApiError::new(ErrorKind::Transient, "the provider returned an empty completion (no text, no tool calls, no usage)"),
                Err(e) => e,
            };
            if adaptations < MAX_ADAPTATIONS {
                let adapted = match self.cfg.wire {
                    Wire::OpenaiChat => self.chat.adapt(&err),
                    Wire::Anthropic => self.messages.adapt(&err),
                };
                if let Some(what) = adapted {
                    adaptations += 1;
                    eprintln!("bija: {what}");
                    continue;
                }
            }
            if !err.kind.retryable() {
                return Err(err);
            }
            if retries >= self.cfg.max_retries {
                return Err(exhausted(err, retries + 1));
            }
            retries += 1;
            let wait = backoff(retries, err.retry_after_ms, self.backoff_unit, self.next_jitter());
            on_retry(&RetryNote { attempt: retries, max: self.cfg.max_retries, error: err, wait });
            tokio::time::sleep(wait).await;
        }
    }

    /// One request, one response, no retries.
    async fn attempt(&mut self, req: &Request<'_>, on_delta: &mut dyn FnMut(Delta)) -> Result<Response, ApiError> {
        let endpoint = match self.cfg.wire {
            Wire::OpenaiChat => openai::endpoint(&self.cfg.base_url),
            Wire::Anthropic => anthropic::endpoint(&self.cfg.base_url),
        };
        let url = http::Url::parse(&endpoint).map_err(|e| ApiError::new(ErrorKind::BadRequest, format!("invalid provider URL: {e}")))?;
        let body = match self.cfg.wire {
            Wire::OpenaiChat => openai::body(req, self.cfg.stream, &mut self.chat),
            Wire::Anthropic => anthropic::body(req, self.cfg.stream, &mut self.messages),
        };
        let body = serde_json::to_vec(&body).map_err(|e| ApiError::internal(format!("cannot encode the request: {e}")))?;
        let headers = self.headers()?;
        let mut resp = self.http.send(&http::HttpRequest { method: "POST", url: &url, headers: &headers, body: &body, streaming: self.cfg.stream }).await?;
        // The request body can be the size of the whole context; it is not needed while streaming.
        drop(body);
        if !(200..300).contains(&resp.status) {
            let status = resp.status;
            let retry_after = retry_after_ms(&resp);
            let reason = std::mem::take(&mut resp.reason);
            let detail = match resp.bytes(MAX_ERROR_BODY).await {
                Ok(bytes) if !bytes.is_empty() => error_detail(&bytes),
                _ => reason,
            };
            return Err(status_error(status, &detail, retry_after, &endpoint));
        }
        let event_stream = resp.header("content-type").map_or(true, |t| !t.to_ascii_lowercase().contains("json"));
        if !self.cfg.stream || !event_stream {
            // Also the answer of a server that ignored `stream: true` and sent plain JSON.
            let bytes = resp.bytes(MAX_BODY).await?;
            return match self.cfg.wire {
                Wire::OpenaiChat => openai::parse_response(&bytes),
                Wire::Anthropic => anthropic::parse_response(&bytes, &mut self.messages),
            };
        }
        let mut parser = match self.cfg.wire {
            Wire::OpenaiChat => Parser::Chat(openai::Stream::default()),
            Wire::Anthropic => Parser::Messages(anthropic::Stream::default()),
        };
        let mut splitter = http::SseSplitter::default();
        let mut events = Vec::new();
        loop {
            let piece = resp.chunk().await?;
            match &piece {
                Some(bytes) => splitter.feed(bytes, &mut events),
                None => splitter.finish(&mut events),
            }
            for event in events.drain(..) {
                if parser.on_event(&event, on_delta)? {
                    resp.finish_within(DRAIN_GRACE).await;
                    return parser.finish(false, &mut self.messages);
                }
            }
            if piece.is_none() {
                return parser.finish(true, &mut self.messages);
            }
        }
    }

    fn headers(&self) -> Result<Vec<(&'static str, String)>, ApiError> {
        let user_agent = if self.cfg.user_agent.trim().is_empty() { format!("bija/{}", env!("CARGO_PKG_VERSION")) } else { self.cfg.user_agent.clone() };
        let accept = if self.cfg.stream { "text/event-stream" } else { "application/json" };
        let mut headers = vec![("User-Agent", user_agent), ("Content-Type", "application/json".to_string()), ("Accept", accept.to_string())];
        // A key read from a file often ends in a newline; anything else unprintable is a mistake.
        let key = self.cfg.api_key.as_deref().map(str::trim).filter(|k| !k.is_empty());
        if key.is_some_and(|k| k.chars().any(char::is_control)) {
            return Err(ApiError::new(ErrorKind::Auth, "the API key contains control characters"));
        }
        match self.cfg.wire {
            Wire::OpenaiChat => headers.extend(key.map(|k| ("Authorization", format!("Bearer {k}")))),
            Wire::Anthropic => {
                headers.extend(key.map(|k| ("x-api-key", k.to_string())));
                headers.push(("anthropic-version", anthropic::API_VERSION.to_string()));
            }
        }
        Ok(headers)
    }

    /// A uniform draw in `[-1, 1)`.
    fn next_jitter(&mut self) -> f64 {
        let mut x = self.rng;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.rng = x;
        let r = x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11;
        r as f64 / (1u64 << 53) as f64 * 2.0 - 1.0
    }
}

fn seed() -> u64 {
    let mut b = [0u8; 8];
    random_bytes(&mut b);
    // xorshift never leaves zero
    u64::from_le_bytes(b) | 1
}

/// The two wires' stream parsers behind one face.
enum Parser {
    Chat(openai::Stream),
    Messages(anthropic::Stream),
}

impl Parser {
    /// `Ok(true)` when the event ended the stream.
    fn on_event(&mut self, event: &http::SseEvent, on_delta: &mut dyn FnMut(Delta)) -> Result<bool, ApiError> {
        match self {
            Parser::Chat(s) => s.on_event(&event.data, on_delta),
            Parser::Messages(s) => s.on_event(event, on_delta),
        }
    }

    fn finish(self, at_eof: bool, memory: &mut anthropic::Memory) -> Result<Response, ApiError> {
        match self {
            Parser::Chat(s) => s.finish(at_eof),
            Parser::Messages(s) => s.finish(at_eof, memory),
        }
    }
}

// ------------------------------------------------------------------------------- retry policy

/// The wait before retry `retry` (1-based): `min(60, 2^retry)` units ± 25 % (`jitter` ∈ [-1, 1]),
/// or the server's `Retry-After` as given, capped at 120 s.
fn backoff(retry: u32, retry_after_ms: Option<u64>, unit: Duration, jitter: f64) -> Duration {
    if let Some(ms) = retry_after_ms {
        return Duration::from_millis(ms).min(MAX_RETRY_AFTER);
    }
    let base = unit * 2u32.saturating_pow(retry).min(MAX_BACKOFF_UNITS);
    let jitter = if jitter.is_finite() { jitter.clamp(-1.0, 1.0) } else { 0.0 };
    base.mul_f64(1.0 + 0.25 * jitter)
}

fn exhausted(mut err: ApiError, attempts: u32) -> ApiError {
    let plural = if attempts == 1 { "" } else { "s" };
    err.message = format!("after {attempts} attempt{plural}: {}", err.message);
    err
}

/// `retry-after-ms` (milliseconds) or `retry-after` (seconds, possibly fractional). The HTTP-date
/// form is ignored: APIs send seconds, and the computed backoff is a fine fallback.
fn retry_after_ms(resp: &http::Response<'_>) -> Option<u64> {
    let number = |name: &str| resp.header(name).and_then(|v| v.trim().parse::<f64>().ok()).filter(|n| n.is_finite() && *n >= 0.0);
    number("retry-after-ms").map(|ms| ms as u64).or_else(|| number("retry-after").map(|s| (s * 1000.0) as u64))
}

fn is_empty_completion(resp: &Response) -> bool {
    let Message::Assistant { text, tool_calls, .. } = &resp.message else {
        return false;
    };
    text.trim().is_empty() && tool_calls.is_empty() && resp.usage.input == 0 && resp.usage.output == 0
}

/// The provider said nothing about usage: estimate it from the text sent (system prompt, tool
/// definitions, history) and received (the reply, its reasoning included — that was generated).
fn with_usage_estimate(mut resp: Response, req: &Request<'_>) -> Response {
    if resp.usage.input > 0 || resp.usage.output > 0 {
        return resp;
    }
    let tools: usize = req.tools.iter().map(|t| t.name.len() + t.description.len() + t.parameters.to_string().len()).sum();
    let input = req.system.len() + tools + sent_chars(req.messages);
    let thought = match &resp.message {
        Message::Assistant { reasoning, .. } => reasoning.len(),
        _ => 0,
    };
    resp.usage = Usage { input: estimate_tokens(input), output: estimate_tokens(resp.message.sent_chars() + thought), cached: 0, estimated: true };
    resp
}

// ----------------------------------------------------------------------------- classification

/// SPEC §7.2's status table. `message` refines 400 (context overflow, an exhausted balance) and
/// 429 (a quota is not a rate limit: waiting will not refill it).
pub(crate) fn kind_for_status(status: u16, message: &str) -> ErrorKind {
    let m = message.to_ascii_lowercase();
    match status {
        400 if looks_like_context(&m) => ErrorKind::Context,
        400 | 429 if looks_like_quota(&m) => ErrorKind::Usage,
        400 | 404 | 422 => ErrorKind::BadRequest,
        401 | 403 => ErrorKind::Auth,
        402 => ErrorKind::Usage,
        413 => ErrorKind::Context,
        429 => ErrorKind::RateLimit,
        408 | 409 | 425 => ErrorKind::Transient,
        501 | 505 => ErrorKind::BadRequest,
        500..=599 => ErrorKind::Transient,
        _ => ErrorKind::BadRequest,
    }
}

/// For errors that come without a status (a stream's `error` event): the same verdicts from words.
pub(crate) fn kind_for_message(message: &str) -> ErrorKind {
    const RATE_LIMIT: &[&str] = &["rate limit", "rate_limit", "ratelimit", "rate-limit", "too many requests"];
    const TRANSIENT: &[&str] = &["overloaded", "timeout", "timed out", "temporarily", "try again", "upstream", "unavailable", "server_error", "server error", "internal error", "api_error", "bad gateway", "capacity", "connection reset"];
    const AUTH: &[&str] = &["invalid api key", "invalid_api_key", "invalid x-api-key", "unauthorized", "authentication", "permission denied"];
    let m = message.to_ascii_lowercase();
    let any = |words: &[&str]| words.iter().any(|w| m.contains(w));
    if looks_like_context(&m) {
        ErrorKind::Context
    } else if looks_like_quota(&m) {
        ErrorKind::Usage
    } else if any(RATE_LIMIT) {
        ErrorKind::RateLimit
    } else if any(TRANSIENT) {
        ErrorKind::Transient
    } else if any(AUTH) {
        ErrorKind::Auth
    } else {
        ErrorKind::BadRequest
    }
}

/// "prompt is too long", "maximum context length is …", "exceeds the context window", "maximum of
/// N tokens" — but not "`max_tokens` exceeds the maximum output tokens", a setting, not the window.
fn looks_like_context(m: &str) -> bool {
    let about_output = m.contains("max_tokens") || m.contains("output tokens") || m.contains("max_completion_tokens");
    m.contains("too long")
        || m.contains("context_length")
        || m.contains("too many tokens")
        || (m.contains("context") && ["length", "window", "limit", "exceed", "size", "maximum", "too large"].iter().any(|w| m.contains(w)))
        || (m.contains("maximum") && m.contains("token") && !about_output)
}

fn looks_like_quota(m: &str) -> bool {
    ["quota", "billing", "credit balance", "insufficient credit", "insufficient_credit", "insufficient balance", "payment required"].iter().any(|w| m.contains(w))
}

/// A non-2xx answer as an [`ApiError`].
fn status_error(status: u16, detail: &str, retry_after_ms: Option<u64>, endpoint: &str) -> ApiError {
    let detail = if detail.is_empty() { "no details" } else { detail };
    let message = match status {
        404 => format!("model or endpoint not found at {endpoint}: {detail}"),
        _ => detail.to_string(),
    };
    let kind = kind_for_status(status, &message);
    ApiError { kind, status: Some(status), message, retry_after_ms: retry_after_ms.filter(|_| kind.retryable()) }
}

/// The human part of an error body: `{"error":{"message","type"}}` (both wires), `{"error":"…"}`,
/// `{"message"}`, `{"detail"}`, else the raw text, clipped.
pub(crate) fn error_detail(body: &[u8]) -> String {
    if let Ok(v) = serde_json::from_slice::<Value>(body) {
        if let Some(text) = describe_error(&v) {
            return text;
        }
    }
    clip(String::from_utf8_lossy(body).trim(), 500)
}

/// The message of an error object (`v` itself or its `error` field), with its type or code
/// appended when the message does not already say it.
pub(crate) fn describe_error(v: &Value) -> Option<String> {
    let err = v.get("error").unwrap_or(v);
    if let Some(s) = err.as_str() {
        return Some(clip(s, 500));
    }
    let message = match err.get("message").or_else(|| err.get("detail")) {
        Some(Value::String(s)) => Some(s.clone()),
        Some(other) if !other.is_null() => Some(other.to_string()),
        _ => None,
    };
    let label = ["type", "code"].iter().find_map(|k| err.get(*k).and_then(Value::as_str)).filter(|l| !l.is_empty());
    let text = match (message, label) {
        (Some(m), Some(l)) if !m.contains(l) => format!("{m} ({l})"),
        (Some(m), _) => m,
        (None, Some(l)) => l.to_string(),
        (None, None) => return None,
    };
    Some(clip(&text, 500))
}

/// An error object found inside a 200 answer (a stream event or a JSON body). Gateways often put
/// the upstream HTTP status in `code`/`status`; when they do it decides, otherwise the words do.
pub(crate) fn embedded_error(v: &Value) -> ApiError {
    let err = v.get("error").unwrap_or(v);
    let message = describe_error(v).unwrap_or_else(|| "the provider reported an error without a message".to_string());
    let status = ["code", "status"].iter().find_map(|k| err.get(*k).and_then(Value::as_u64)).filter(|s| (400..600).contains(s)).map(|s| s as u16);
    let kind = match status {
        Some(s) => kind_for_status(s, &message),
        None => kind_for_message(&message),
    };
    ApiError { kind, status, message, retry_after_ms: None }
}

/// A stream that broke off before its end: retried like any broken connection.
pub(crate) fn truncated_stream(what: &str) -> ApiError {
    ApiError::new(ErrorKind::Transient, format!("the stream ended before {what}"))
}

/// A body that is not the JSON it should be — what a proxy's error page or a torn stream looks like.
pub(crate) fn malformed(what: &str, e: impl std::fmt::Display) -> ApiError {
    ApiError::new(ErrorKind::Transient, format!("the provider sent malformed {what}: {e}"))
}

// ------------------------------------------------------------------------------ shared wiring

/// Tool calls and results as plain text, for a request that offers no tools (the compaction
/// call): providers refuse tool blocks in a history when the request defines no tools.
pub(crate) fn call_as_text(call: &ToolCall) -> String {
    let args = if call.args.is_null() && !call.raw.is_empty() { call.raw.clone() } else { call.args.to_string() };
    format!("[tool call {} {args}]", call.name)
}

pub(crate) fn result_as_text(name: &str, text: &str, is_error: bool) -> String {
    let failed = if is_error { " (failed)" } else { "" };
    format!("[tool result of {name}{failed}]\n{text}")
}

/// Tool call arguments as the model sent them: a JSON object, `{}` for nothing at all, the inner
/// object of a double-encoded string, or `Null` with the text kept in `raw` (the agent then
/// tells the model its arguments did not parse).
pub(crate) fn parse_args(text: &str) -> (Value, String) {
    let text = text.trim();
    if text.is_empty() {
        return (Value::Object(Default::default()), String::new());
    }
    match serde_json::from_str::<Value>(text) {
        Ok(Value::String(inner)) => match serde_json::from_str::<Value>(&inner) {
            Ok(v @ Value::Object(_)) => (v, String::new()),
            _ => (Value::Null, text.to_string()),
        },
        Ok(v) => (v, String::new()),
        Err(_) => (Value::Null, text.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::http::test_server::{self, After, Reply, Server};
    use super::*;
    use crate::types::{Finish, ToolDef};

    #[test]
    fn statuses_map_to_the_spec_table() {
        let table = [
            (400, ErrorKind::BadRequest),
            (401, ErrorKind::Auth),
            (402, ErrorKind::Usage),
            (403, ErrorKind::Auth),
            (404, ErrorKind::BadRequest),
            (408, ErrorKind::Transient),
            (409, ErrorKind::Transient),
            (413, ErrorKind::Context),
            (422, ErrorKind::BadRequest),
            (425, ErrorKind::Transient),
            (429, ErrorKind::RateLimit),
            (500, ErrorKind::Transient),
            (502, ErrorKind::Transient),
            (503, ErrorKind::Transient),
            (504, ErrorKind::Transient),
            (529, ErrorKind::Transient),
            (501, ErrorKind::BadRequest),
            (418, ErrorKind::BadRequest),
        ];
        for (status, kind) in table {
            assert_eq!(kind_for_status(status, "something went wrong"), kind, "HTTP {status}");
        }
        assert!(kind_for_status(503, "").retryable() && kind_for_status(429, "").retryable());
        assert!(!kind_for_status(401, "").retryable() && !kind_for_status(400, "").retryable());
    }

    #[test]
    fn a_400_about_the_window_is_context_and_one_about_output_tokens_is_not() {
        for m in [
            "prompt is too long: 210000 tokens > 200000 maximum",
            "This model's maximum context length is 128000 tokens. However, your messages resulted in 130000 tokens.",
            "input length and `max_tokens` exceed context limit: 190000 + 16384 > 200000",
            "Request exceeds the maximum of 32768 tokens",
            "context_length_exceeded",
        ] {
            assert_eq!(kind_for_status(400, m), ErrorKind::Context, "{m}");
        }
        for m in ["max_tokens: 64000 > 32000, which is the maximum allowed number of output tokens", "messages: roles must alternate", "Invalid value for reasoning_effort"] {
            assert_eq!(kind_for_status(400, m), ErrorKind::BadRequest, "{m}");
        }
    }

    #[test]
    fn an_exhausted_quota_is_usage_not_a_rate_limit() {
        assert_eq!(kind_for_status(429, "You exceeded your current quota, please check your plan and billing details."), ErrorKind::Usage);
        assert_eq!(kind_for_status(400, "Your credit balance is too low to access the API"), ErrorKind::Usage);
        assert_eq!(kind_for_status(429, "Rate limit reached for requests"), ErrorKind::RateLimit);
    }

    #[test]
    fn stream_error_messages_classify_by_their_words() {
        assert_eq!(kind_for_message("Overloaded (overloaded_error)"), ErrorKind::Transient);
        assert_eq!(kind_for_message("upstream connect error"), ErrorKind::Transient);
        assert_eq!(kind_for_message("The server had an error processing your request (server_error)"), ErrorKind::Transient);
        assert_eq!(kind_for_message("Please try again later"), ErrorKind::Transient);
        assert_eq!(kind_for_message("rate_limit_error: slow down"), ErrorKind::RateLimit);
        assert_eq!(kind_for_message("prompt is too long"), ErrorKind::Context);
        assert_eq!(kind_for_message("invalid x-api-key"), ErrorKind::Auth);
        assert_eq!(kind_for_message("Your request was flagged by our moderation system"), ErrorKind::BadRequest, "\"moderation\" is not a rate limit");
    }

    #[test]
    fn embedded_errors_prefer_a_status_code_when_the_gateway_gives_one() {
        let e = embedded_error(&serde_json::json!({"error": {"code": 502, "message": "Provider returned error"}}));
        assert_eq!((e.kind, e.status), (ErrorKind::Transient, Some(502)));
        let e = embedded_error(&serde_json::json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}}));
        assert_eq!((e.kind, e.message.as_str()), (ErrorKind::Transient, "Overloaded (overloaded_error)"));
        let e = embedded_error(&serde_json::json!({"error": {"code": "context_length_exceeded", "message": "too many tokens"}}));
        assert_eq!(e.kind, ErrorKind::Context);
    }

    #[test]
    fn error_bodies_yield_their_message_or_their_clipped_text() {
        assert_eq!(error_detail(br#"{"error":{"message":"bad key","type":"invalid_request_error"}}"#), "bad key (invalid_request_error)");
        assert_eq!(error_detail(br#"{"error":"nope"}"#), "nope");
        assert_eq!(error_detail(br#"{"detail":[{"loc":["body"]}]}"#), r#"[{"loc":["body"]}]"#);
        assert_eq!(error_detail(b"  <html>Bad Gateway</html> "), "<html>Bad Gateway</html>");
        assert_eq!(error_detail(&[b'x'; 600]).chars().count(), 501);
    }

    #[test]
    fn backoff_doubles_to_a_minute_with_bounded_jitter_and_honours_retry_after() {
        let s = Duration::from_secs(1);
        assert_eq!(backoff(1, None, s, 0.0), Duration::from_secs(2));
        assert_eq!(backoff(2, None, s, 0.0), Duration::from_secs(4));
        assert_eq!(backoff(5, None, s, 0.0), Duration::from_secs(32));
        assert_eq!(backoff(6, None, s, 0.0), Duration::from_secs(60));
        assert_eq!(backoff(40, None, s, 0.0), Duration::from_secs(60));
        assert_eq!(backoff(1, None, s, 1.0), Duration::from_millis(2500));
        assert_eq!(backoff(1, None, s, -1.0), Duration::from_millis(1500));
        assert_eq!(backoff(6, None, s, 5.0), Duration::from_secs(75), "jitter is clamped to ±25 %");
        assert_eq!(backoff(1, None, s, f64::NAN), Duration::from_secs(2));
        assert_eq!(backoff(3, Some(5000), s, 1.0), Duration::from_secs(5));
        assert_eq!(backoff(3, Some(10_000_000), s, 0.0), Duration::from_secs(120));
        assert_eq!(backoff(3, Some(0), s, 0.0), Duration::ZERO);
    }

    #[test]
    fn jitter_stays_in_range_and_varies() {
        let mut c = client("http://127.0.0.1:9/v1", Wire::OpenaiChat, 0);
        let draws: Vec<f64> = (0..1000).map(|_| c.next_jitter()).collect();
        assert!(draws.iter().all(|j| (-1.0..1.0).contains(j)));
        assert!(draws.iter().any(|j| *j < -0.5) && draws.iter().any(|j| *j > 0.5));
    }

    #[test]
    fn arguments_parse_leniently() {
        assert_eq!(parse_args(r#" {"a": 1} "#), (serde_json::json!({"a": 1}), String::new()));
        assert_eq!(parse_args(""), (serde_json::json!({}), String::new()));
        assert_eq!(parse_args(r#""{\"a\":1}""#), (serde_json::json!({"a": 1}), String::new()), "double-encoded");
        assert_eq!(parse_args(r#"{"a": 1"#), (Value::Null, r#"{"a": 1"#.to_string()));
    }

    // ----------------------------------------------------------------- against a scripted server

    fn client(base: &str, wire: Wire, max_retries: u32) -> Client {
        let mut c = Client::new(ClientConfig { base_url: base.into(), wire, api_key: Some("sk-test\n".into()), max_retries, timeout: Duration::from_secs(10), stream: true, user_agent: String::new() });
        c.http.set_proxy(http::ProxyEnv::default());
        c.backoff_unit = Duration::from_millis(1);
        c
    }

    const HELLO: &[&str] = &[r#"data: {"choices":[{"index":0,"delta":{"content":"Hel"}}]}"#, r#"data: {"choices":[{"index":0,"delta":{"content":"lo"},"finish_reason":"stop"}]}"#, r#"data: {"choices":[],"usage":{"prompt_tokens":5,"completion_tokens":2}}"#, "data: [DONE]"];

    fn hello() -> Reply {
        Reply { bytes: test_server::sse(HELLO, true), after: After::KeepOpen }
    }

    struct Run {
        result: Result<Response, ApiError>,
        deltas: Vec<Delta>,
        retries: Vec<RetryNote>,
    }

    async fn run(c: &mut Client) -> Run {
        let messages = [Message::User { text: "hi".into() }];
        let tools = [ToolDef { name: "bash".into(), description: "run".into(), parameters: serde_json::json!({"type": "object"}) }];
        let req = Request { model: "m1", effort: "", system: "be brief", messages: &messages, tools: &tools, max_output_tokens: None };
        let (mut deltas, mut retries) = (Vec::new(), Vec::new());
        let result = c.complete(&req, &mut |d| deltas.push(d), &mut |n| retries.push(n.clone())).await;
        Run { result, deltas, retries }
    }

    fn text_of(r: &Run) -> String {
        match &r.result {
            Ok(Response { message: Message::Assistant { text, .. }, .. }) => text.clone(),
            other => panic!("expected a completion, got {other:?}"),
        }
    }

    async fn openai_server(replies: Vec<Reply>) -> (Server, Client) {
        let server = test_server::start(replies).await;
        let c = client(&server.url("/v1"), Wire::OpenaiChat, 3);
        (server, c)
    }

    #[tokio::test]
    async fn a_503_then_a_200_retries_once_and_returns_the_completion() {
        let busy = Reply { bytes: test_server::response(503, "Retry-After: 0\r\n", r#"{"error":{"message":"busy"}}"#), after: After::KeepOpen };
        let (server, mut c) = openai_server(vec![busy, hello()]).await;
        let r = run(&mut c).await;
        assert_eq!(text_of(&r), "Hello");
        assert_eq!(r.retries.len(), 1);
        let note = &r.retries[0];
        assert_eq!((note.attempt, note.max, note.error.status, note.error.kind, note.wait), (1, 3, Some(503), ErrorKind::Transient, Duration::ZERO));
        assert_eq!(r.deltas, vec![Delta::Text("Hel".into()), Delta::Text("lo".into())]);
        let first = server.requests.lock().expect("log")[0].clone();
        assert!(first.starts_with("POST /v1/chat/completions HTTP/1.1"), "{first}");
        assert!(first.contains("Authorization: Bearer sk-test\r\n"), "the key is trimmed: {first}");
        assert!(first.contains(&format!("User-Agent: bija/{}", env!("CARGO_PKG_VERSION"))), "{first}");
        assert!(first.contains("Accept: text/event-stream"), "{first}");
        assert_eq!(server.connection_count(), 1, "the error body was read, so the connection was reused");
    }

    #[tokio::test]
    async fn a_stream_cut_after_its_first_chunk_is_retried() {
        let cut = Reply { bytes: test_server::sse(&HELLO[..1], false), after: After::Close };
        let (server, mut c) = openai_server(vec![cut, hello()]).await;
        let r = run(&mut c).await;
        assert_eq!(text_of(&r), "Hello");
        assert_eq!(r.retries.len(), 1);
        assert_eq!(r.retries[0].error.kind, ErrorKind::Transient);
        assert!(r.retries[0].wait <= Duration::from_millis(3), "one shrunken backoff unit: {:?}", r.retries[0].wait);
        assert_eq!(r.deltas.first(), Some(&Delta::Text("Hel".into())), "the broken attempt's delta was already streamed");
        assert_eq!(server.connection_count(), 2);
    }

    #[tokio::test]
    async fn a_401_is_auth_and_never_retried() {
        let denied = Reply { bytes: test_server::response(401, "", r#"{"error":{"message":"Incorrect API key provided","type":"invalid_request_error"}}"#), after: After::KeepOpen };
        let (server, mut c) = openai_server(vec![denied, hello()]).await;
        let r = run(&mut c).await;
        let err = r.result.expect_err("auth failure");
        assert_eq!((err.kind, err.status), (ErrorKind::Auth, Some(401)));
        assert!(err.message.contains("Incorrect API key"), "{}", err.message);
        assert!(r.retries.is_empty());
        assert_eq!(server.request_count(), 1);
    }

    #[tokio::test]
    async fn keep_alive_carries_two_completions_over_one_connection() {
        let (server, mut c) = openai_server(vec![hello(), hello()]).await;
        assert_eq!(text_of(&run(&mut c).await), "Hello");
        assert!(c.http.has_idle_connection(), "the stream's end was drained, so the connection went back");
        assert_eq!(text_of(&run(&mut c).await), "Hello");
        assert_eq!(server.connection_count(), 1);
        assert_eq!(server.request_count(), 2);
    }

    #[tokio::test]
    async fn a_server_that_dropped_the_idle_connection_costs_no_retry() {
        let first = Reply { bytes: test_server::sse(HELLO, true), after: After::Close };
        let (server, mut c) = openai_server(vec![first, hello()]).await;
        assert_eq!(text_of(&run(&mut c).await), "Hello");
        tokio::time::sleep(Duration::from_millis(20)).await;
        let r = run(&mut c).await;
        assert_eq!(text_of(&r), "Hello");
        assert!(r.retries.is_empty(), "a stale keep-alive connection is redialled silently");
        assert_eq!(server.connection_count(), 2);
    }

    #[tokio::test]
    async fn an_empty_completion_is_retried() {
        let empty = Reply { bytes: test_server::sse(&[r#"data: {"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#, "data: [DONE]"], true), after: After::KeepOpen };
        let (_server, mut c) = openai_server(vec![empty, hello()]).await;
        let r = run(&mut c).await;
        assert_eq!(text_of(&r), "Hello");
        assert_eq!(r.retries.len(), 1);
        assert!(r.retries[0].error.message.contains("empty completion"));
    }

    #[tokio::test]
    async fn the_last_failure_names_the_attempt_count() {
        let busy = || Reply { bytes: test_server::response(529, "Retry-After: 0\r\n", r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#), after: After::KeepOpen };
        let (_server, mut c) = openai_server(vec![busy(), busy(), busy(), busy()]).await;
        let r = run(&mut c).await;
        let err = r.result.expect_err("still overloaded");
        assert_eq!((err.kind, err.status), (ErrorKind::Transient, Some(529)));
        assert_eq!(err.message, "after 4 attempts: Overloaded (overloaded_error)");
        assert_eq!(r.retries.iter().map(|n| n.attempt).collect::<Vec<_>>(), vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn dropping_a_completion_in_flight_leaves_the_client_usable() {
        let stalled = Reply { bytes: test_server::sse(&HELLO[..1], false), after: After::Stall };
        let (server, mut c) = openai_server(vec![stalled, hello()]).await;
        let cancelled = tokio::time::timeout(Duration::from_millis(100), run(&mut c)).await;
        assert!(cancelled.is_err(), "the stalled stream is still waiting when it is dropped");
        assert!(!c.http.has_idle_connection());
        assert_eq!(text_of(&run(&mut c).await), "Hello");
        assert_eq!(server.connection_count(), 2);
    }

    #[tokio::test]
    async fn a_404_names_the_endpoint_and_a_bad_url_is_a_bad_request() {
        let missing = Reply { bytes: test_server::response(404, "", r#"{"error":{"message":"The model `m1` does not exist"}}"#), after: After::KeepOpen };
        let (server, mut c) = openai_server(vec![missing]).await;
        let err = run(&mut c).await.result.expect_err("not found");
        assert_eq!(err.kind, ErrorKind::BadRequest);
        assert!(err.message.contains(&server.url("/v1/chat/completions")) && err.message.contains("does not exist"), "{}", err.message);
        let mut c = client("127.0.0.1:1234", Wire::OpenaiChat, 3);
        assert_eq!(run(&mut c).await.result.expect_err("no scheme").kind, ErrorKind::BadRequest);
    }

    #[tokio::test]
    async fn a_plain_json_answer_to_a_stream_request_is_still_parsed() {
        let body = r#"{"choices":[{"message":{"role":"assistant","content":"plain"},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":1}}"#;
        let (_server, mut c) = openai_server(vec![Reply { bytes: test_server::response(200, "", body), after: After::KeepOpen }]).await;
        let r = run(&mut c).await;
        assert_eq!(text_of(&r), "plain");
        let resp = r.result.expect("ok");
        assert_eq!((resp.usage.input, resp.finish), (3, Finish::Stop));
    }

    #[tokio::test]
    async fn missing_usage_is_estimated_and_flagged() {
        let events = [r#"data: {"choices":[{"index":0,"delta":{"content":"12345678"},"finish_reason":"stop"}]}"#, "data: [DONE]"];
        let (_server, mut c) = openai_server(vec![Reply { bytes: test_server::sse(&events, true), after: After::KeepOpen }]).await;
        let usage = run(&mut c).await.result.expect("ok").usage;
        assert!(usage.estimated);
        assert_eq!(usage.output, 2);
        assert!(usage.input > 0);
    }

    #[tokio::test]
    async fn a_rejected_optional_feature_is_dropped_and_resent_without_a_retry() {
        let rejected = Reply { bytes: test_server::response(400, "", r#"{"error":{"message":"Unrecognized request argument supplied: stream_options"}}"#), after: After::KeepOpen };
        let (server, mut c) = openai_server(vec![rejected, hello(), hello()]).await;
        let r = run(&mut c).await;
        assert_eq!(text_of(&r), "Hello");
        assert!(r.retries.is_empty());
        assert_eq!(text_of(&run(&mut c).await), "Hello");
        let log = server.requests.lock().expect("log").clone();
        assert!(log[0].contains("stream_options") && !log[1].contains("stream_options") && !log[2].contains("stream_options"), "dropped for the rest of the session");
    }

    #[tokio::test]
    async fn the_messages_wire_streams_through_the_same_client() {
        let events = [
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":7,\"output_tokens\":1}}}",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi\"}}",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":2}}",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}",
        ];
        let server = test_server::start(vec![Reply { bytes: test_server::sse(&events, true), after: After::KeepOpen }]).await;
        let mut c = client(&server.url(""), Wire::Anthropic, 3);
        let r = run(&mut c).await;
        assert_eq!(text_of(&r), "Hi");
        assert_eq!(r.result.expect("ok").usage, Usage { input: 7, output: 2, cached: 0, estimated: false });
        let first = server.requests.lock().expect("log")[0].clone();
        assert!(first.starts_with("POST /v1/messages HTTP/1.1"), "{first}");
        assert!(first.contains("x-api-key: sk-test\r\n") && first.contains("anthropic-version: 2023-06-01"), "{first}");
        assert!(!first.contains("Authorization"), "{first}");
    }
}
