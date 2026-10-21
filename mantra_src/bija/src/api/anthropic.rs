//! Anthropic-compatible Messages wire (SPEC §7.4): `POST {base}/v1/messages` (`{base}/messages`
//! when the base already ends in `/v1`).
//!
//! Two things make this wire more than a format translation, and both live in [`Memory`]:
//!
//! **Thinking blocks are replayed.** A model that thinks before a tool call expects that thinking
//! back, unmodified and with its signature, when the conversation continues; dropping it
//! mid-history breaks the turn. `types::Message` has no place for a signature (and never will:
//! it is wire-agnostic), so the blocks are kept here, keyed by the assistant message they came
//! with, and put back in their original position when that message is sent again. Entries leave
//! with their message (compaction, a new session). After a restart the store is empty, so the
//! oldest messages go without their thinking — dropping from the front is the one omission the
//! API accepts. A 400 about a signature clears the store once (the documented recovery).
//!
//! **Reasoning controls differ by model generation.** Current models take
//! `thinking: {type: "adaptive"}` plus `output_config.effort` (and refuse a fixed budget); older
//! ones only a fixed `budget_tokens`; some gateways neither. So the wire starts with the adaptive
//! form and steps down a ladder — budget, effort alone, nothing — each time the provider answers
//! 400 naming a reasoning field (SPEC §7.4). Each step is permanent for the session, so the ladder
//! costs at most one rejected request per step. Automatic prompt caching (`cache_control` at the
//! top level) is dropped the same way where it is refused. It matters: an agent loop resends its
//! whole history on every request.

use super::http::SseEvent;
use super::{call_as_text, embedded_error, malformed, parse_args, refuses_images, result_as_text, truncated_stream, ImageFiles};
use crate::types::{str_at, ApiError, Delta, ErrorKind, Finish, Message, Request, Response, ToolCall, Usage};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};

/// The `anthropic-version` header value.
pub const API_VERSION: &str = "2023-06-01";
/// `max_tokens` when none is configured: the API requires one.
const DEFAULT_MAX_TOKENS: u32 = 16384;
/// Room left for the answer above a thinking budget (SPEC §7.4).
const ANSWER_HEADROOM: u32 = 4096;
/// The API's smallest thinking budget.
const MIN_BUDGET: u32 = 1024;
/// Words in a 400 that mean "the reasoning controls are the problem".
const REASONING_FIELDS: &[&str] = &["thinking", "budget_tokens", "adaptive", "output_config", "effort"];
/// The API's limit on one image, counted in base64 bytes: a bigger one is left out up front
/// rather than costing the session its images (`no_images`) on the 400 it would bring.
const MAX_IMAGE_B64: usize = 5 * 1024 * 1024;

pub fn endpoint(base: &str) -> String {
    let base = base.trim().trim_end_matches('/');
    if base.ends_with("/v1") {
        format!("{base}/messages")
    } else {
        format!("{base}/v1/messages")
    }
}

/// How effort is expressed; see the module docs for the ladder.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Style {
    #[default]
    Adaptive,
    Budget,
    EffortOnly,
    Off,
}

impl Style {
    fn next(self) -> Option<Style> {
        match self {
            Style::Adaptive => Some(Style::Budget),
            Style::Budget => Some(Style::EffortOnly),
            Style::EffortOnly => Some(Style::Off),
            Style::Off => None,
        }
    }

    fn describe(self) -> &'static str {
        match self {
            Style::Adaptive => "adaptive thinking",
            Style::Budget => "a fixed thinking budget",
            Style::EffortOnly => "an effort level alone",
            Style::Off => "no reasoning controls",
        }
    }
}

/// A thinking (or redacted thinking) block exactly as received, and how many tool calls came
/// before it in its message — enough to put it back where it was.
#[derive(Debug, Clone, PartialEq)]
struct Replay {
    after_calls: usize,
    block: Value,
}

/// What the last request carried, so a 400 can be pinned on one of its optional parts.
#[derive(Debug, Default, Clone, Copy)]
struct Sent {
    reasoning: bool,
    display: bool,
    cache_control: bool,
    replayed: bool,
    raised_max_tokens: bool,
    images: bool,
}

/// What this wire carries from one request to the next (module docs).
#[derive(Debug, Default)]
pub struct Memory {
    replays: HashMap<u64, Vec<Replay>>,
    style: Style,
    no_cache_control: bool,
    /// Keep `max_tokens` as configured even when a thinking budget would want more room.
    no_raise: bool,
    /// Adaptive thinking without `display: "summarized"` (reasoning then streams empty).
    no_display: bool,
    /// Tool results go as text only: the provider refused an image.
    no_images: bool,
    pub(crate) images: ImageFiles,
    sent: Sent,
}

impl Memory {
    /// After a failed request: if a 400 is about an optional part of it, change course and say how
    /// (the caller resends at once). `None` = nothing to adapt.
    pub fn adapt(&mut self, err: &ApiError) -> Option<String> {
        if err.kind != ErrorKind::BadRequest {
            return None;
        }
        let m = err.message.to_ascii_lowercase();
        if self.sent.replayed && m.contains("signature") {
            self.replays.clear();
            return Some("the provider refused a replayed thinking block; resending without the earlier thinking".into());
        }
        if refuses_images(err, self.sent.images) {
            self.no_images = true;
            return Some("the provider refused an image; tool results go as text only from now on".into());
        }
        if self.sent.cache_control && m.contains("cache_control") {
            self.no_cache_control = true;
            return Some("the provider rejects automatic prompt caching; continuing without it".into());
        }
        if self.sent.raised_max_tokens && m.contains("max_tokens") && !m.contains("budget_tokens") {
            self.no_raise = true;
            return Some("the provider caps max_tokens below the thinking headroom; keeping the configured max_tokens".into());
        }
        // Only the display option, not thinking itself: stepping down the ladder for it would
        // switch reasoning off on models that think only when asked to.
        if self.sent.display && m.contains("display") {
            self.no_display = true;
            return Some("the provider rejects `thinking.display`; asking for adaptive thinking without it".into());
        }
        if self.sent.reasoning && REASONING_FIELDS.iter().any(|w| m.contains(w)) {
            let from = self.style;
            self.style = from.next()?;
            return Some(format!("the provider rejected {}; trying {}", from.describe(), self.style.describe()));
        }
        None
    }

    fn remember(&mut self, message: &Message, replay: Vec<Replay>) {
        if let Message::Assistant { text, tool_calls, .. } = message {
            if !replay.is_empty() {
                self.replays.insert(replay_key(text, tool_calls), replay);
            }
        }
    }
}

/// Identifies an assistant message by what the agent never rewrites: its text and the
/// provider's call ids (arguments may be normalised on the way into the transcript).
fn replay_key(text: &str, calls: &[ToolCall]) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    for c in calls {
        c.id.hash(&mut h);
    }
    h.finish()
}

/// The request body. Without tools (the compaction call) tool calls and results go as plain text
/// and no thinking is replayed: tool blocks need tool definitions, and replayed thinking needs its
/// message unchanged.
pub fn body(req: &Request<'_>, stream: bool, memory: &mut Memory) -> Value {
    let plain = req.tools.is_empty();
    let (messages, replayed, images) = messages_json(req.messages, plain, memory);
    if !plain {
        // Entries whose message is gone (compacted away) are never needed again.
        memory.replays.retain(|key, _| replayed.contains(key));
    }
    let mut sent = Sent { replayed: !replayed.is_empty(), images, ..Sent::default() };
    let mut body = Map::new();
    body.insert("model".into(), json!(req.model));
    let configured = req.max_output_tokens.unwrap_or(DEFAULT_MAX_TOKENS).max(1);
    let mut max_tokens = configured;
    if let Some(effort) = normalized_effort(req.effort).filter(|_| memory.style != Style::Off) {
        let budget = budget_for(effort);
        if !memory.no_raise {
            max_tokens = max_tokens.max(budget + ANSWER_HEADROOM);
        }
        match memory.style {
            Style::Adaptive => {
                let thinking = if memory.no_display { json!({"type": "adaptive"}) } else { json!({"type": "adaptive", "display": "summarized"}) };
                body.insert("thinking".into(), thinking);
                body.insert("output_config".into(), json!({"effort": effort}));
                sent.display = !memory.no_display;
            }
            Style::Budget => {
                let budget = budget.min(max_tokens.saturating_sub(MIN_BUDGET));
                if budget >= MIN_BUDGET {
                    body.insert("thinking".into(), json!({"type": "enabled", "budget_tokens": budget}));
                }
            }
            Style::EffortOnly => {
                body.insert("output_config".into(), json!({"effort": effort}));
            }
            Style::Off => {}
        }
        sent.reasoning = body.contains_key("thinking") || body.contains_key("output_config");
        sent.raised_max_tokens = max_tokens > configured;
    }
    body.insert("max_tokens".into(), json!(max_tokens));
    if !req.system.is_empty() {
        body.insert("system".into(), json!(req.system));
    }
    body.insert("messages".into(), Value::Array(messages));
    if !plain {
        let tools = req.tools.iter().map(|t| json!({"name": t.name, "description": t.description, "input_schema": t.parameters})).collect();
        body.insert("tools".into(), Value::Array(tools));
    }
    if stream {
        body.insert("stream".into(), json!(true));
    }
    if !memory.no_cache_control {
        body.insert("cache_control".into(), json!({"type": "ephemeral"}));
        sent.cache_control = true;
    }
    memory.sent = sent;
    Value::Object(body)
}

/// Effort as this API spells it; `None` for no reasoning controls at all.
fn normalized_effort(effort: &str) -> Option<&str> {
    match effort.trim() {
        "" => None,
        e if e.eq_ignore_ascii_case("none") => None,
        e if e.eq_ignore_ascii_case("minimal") => Some("low"),
        e if e.eq_ignore_ascii_case("ultra") => Some("max"),
        e => Some(e),
    }
}

/// SPEC §7.4's budget table; an unknown level gets the medium budget.
fn budget_for(effort: &str) -> u32 {
    match effort.to_ascii_lowercase().as_str() {
        "low" => 2048,
        "high" => 16384,
        "xhigh" | "max" => 32768,
        _ => 8192,
    }
}

/// `(messages, keys of the assistant messages whose thinking was replayed, whether an image
/// went)`. Roles must alternate, so consecutive same-role messages merge (tool results and a steer
/// that followed them become one user message, results first); empty text is left out, since the
/// API refuses empty text blocks.
fn messages_json(messages: &[Message], plain: bool, memory: &Memory) -> (Vec<Value>, Vec<u64>, bool) {
    let mut turns: Vec<(&'static str, Vec<Value>)> = Vec::with_capacity(messages.len());
    let mut replayed = Vec::new();
    let mut sent_images = false;
    for m in messages {
        let (role, blocks): (&'static str, Vec<Value>) = match m {
            Message::User { text } => ("user", text_block(text).into_iter().collect()),
            Message::Assistant { text, tool_calls, .. } if plain => {
                let mut content = text.clone();
                for call in tool_calls {
                    if !content.is_empty() {
                        content.push_str("\n\n");
                    }
                    content.push_str(&call_as_text(call));
                }
                ("assistant", text_block(&content).into_iter().collect())
            }
            Message::Assistant { text, tool_calls, .. } => {
                let key = replay_key(text, tool_calls);
                let replay = memory.replays.get(&key).map(Vec::as_slice).unwrap_or(&[]);
                let blocks = assistant_blocks(text, tool_calls, replay);
                if !replay.is_empty() && !blocks.is_empty() {
                    replayed.push(key);
                }
                ("assistant", blocks)
            }
            Message::Tool { name, text, is_error, .. } if plain => ("user", text_block(&result_as_text(name, text, *is_error)).into_iter().collect()),
            Message::Tool { call_id, text, is_error, images, .. } => {
                let pictures: Vec<(&str, String)> = if memory.no_images { Vec::new() } else { images.iter().filter_map(|img| memory.images.load(img, Some(MAX_IMAGE_B64))).collect() };
                sent_images |= !pictures.is_empty();
                ("user", vec![tool_result(call_id, text, *is_error, &pictures)])
            }
        };
        if blocks.is_empty() {
            continue;
        }
        match turns.last_mut() {
            Some((last, content)) if *last == role => content.extend(blocks),
            _ => turns.push((role, blocks)),
        }
    }
    if turns.first().is_some_and(|(role, _)| *role == "assistant") {
        turns.insert(0, ("user", vec![json!({"type": "text", "text": "(continuing an earlier conversation)"})]));
    }
    let messages = turns.into_iter().map(|(role, content)| json!({"role": role, "content": content})).collect();
    (messages, replayed, sent_images)
}

fn text_block(text: &str) -> Option<Value> {
    (!text.trim().is_empty()).then(|| json!({"type": "text", "text": text}))
}

/// An assistant message's blocks, thinking put back between them as it came: blocks that arrived
/// after k tool calls go right before call k+1. An empty message (no text, no calls) yields none.
fn assistant_blocks(text: &str, calls: &[ToolCall], replay: &[Replay]) -> Vec<Value> {
    if text.trim().is_empty() && calls.is_empty() {
        return Vec::new();
    }
    let mut blocks = Vec::with_capacity(calls.len() + replay.len() + 1);
    for k in 0..=calls.len() {
        blocks.extend(replay.iter().filter(|r| r.after_calls == k).map(|r| r.block.clone()));
        if k == 0 {
            blocks.extend(text_block(text));
        }
        if let Some(c) = calls.get(k) {
            // Unparsable arguments go back as `{}`: `input` must be an object, and the tool
            // result already told the model what went wrong.
            let input = if c.args.is_object() { c.args.clone() } else { json!({}) };
            blocks.push(json!({"type": "tool_use", "id": c.id, "name": c.name, "input": input}));
        }
    }
    blocks
}

/// A result block: its text, and the images (media type, base64) beside it in its content.
fn tool_result(call_id: &str, text: &str, is_error: bool, images: &[(&str, String)]) -> Value {
    let text = if text.trim().is_empty() { "(no output)" } else { text };
    let content = if images.is_empty() {
        json!(text)
    } else {
        let mut parts = vec![json!({"type": "text", "text": text})];
        parts.extend(images.iter().map(|(mime, b64)| json!({"type": "image", "source": {"type": "base64", "media_type": mime, "data": b64}})));
        Value::Array(parts)
    };
    let mut block = Map::new();
    block.insert("type".into(), json!("tool_result"));
    block.insert("tool_use_id".into(), json!(call_id));
    block.insert("content".into(), content);
    if is_error {
        block.insert("is_error".into(), json!(true));
    }
    Value::Object(block)
}

/// One content block of the answer being streamed, by its stream index.
#[derive(Debug)]
enum Block {
    Text,
    Thinking { text: String, signature: String, after_calls: usize },
    Redacted { data: String, after_calls: usize },
    ToolUse { call: usize, id: String, name: String, json: String, input: Value },
    Other,
}

/// The streaming parser: feed it each SSE event, then [`Stream::finish`].
#[derive(Debug, Default)]
pub struct Stream {
    text: String,
    reasoning: String,
    blocks: Vec<(u64, Block)>,
    calls: usize,
    /// A new text (thinking) block began after earlier ones: separate it on its first delta.
    text_break: bool,
    reasoning_break: bool,
    fresh_input: u64,
    cache_read: u64,
    cache_write: u64,
    output: u64,
    saw_usage: bool,
    stop: Option<String>,
    done: bool,
}

impl Stream {
    /// One event. `Ok(true)` once `message_stop` arrives; an `error` event is an `Err`.
    pub fn on_event(&mut self, event: &SseEvent, on_delta: &mut dyn FnMut(Delta)) -> Result<bool, ApiError> {
        let data = event.data.trim();
        if data.is_empty() {
            return Ok(false);
        }
        if data == "[DONE]" {
            // not this API's, but gateways that translate from the other wire send it
            self.done = true;
            return Ok(true);
        }
        let v: Value = serde_json::from_str(data).map_err(|e| malformed("stream event", e))?;
        let kind = v.get("type").and_then(Value::as_str).unwrap_or(event.event.as_str());
        match kind {
            "message_start" => {
                if let Some(u) = v.pointer("/message/usage") {
                    self.note_usage(u);
                }
            }
            "content_block_start" => {
                let index = v.get("index").and_then(Value::as_u64).unwrap_or(self.blocks.len() as u64);
                self.start_block(index, v.get("content_block").unwrap_or(&Value::Null), on_delta);
            }
            "content_block_delta" => self.apply_delta(v.get("index").and_then(Value::as_u64), v.get("delta").unwrap_or(&Value::Null), on_delta),
            "message_delta" => {
                if let Some(reason) = v.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    self.stop = Some(reason.to_string());
                }
                if let Some(u) = v.get("usage") {
                    self.note_usage(u);
                }
            }
            "message_stop" => {
                self.done = true;
                return Ok(true);
            }
            "error" => return Err(embedded_error(&v)),
            // ping, content_block_stop, and whatever the API adds next
            _ => {}
        }
        Ok(false)
    }

    /// Usage fields arrive in `message_start` and again, cumulatively, in `message_delta`.
    fn note_usage(&mut self, u: &Value) {
        let slots = [("input_tokens", &mut self.fresh_input), ("cache_read_input_tokens", &mut self.cache_read), ("cache_creation_input_tokens", &mut self.cache_write), ("output_tokens", &mut self.output)];
        for (key, slot) in slots {
            if let Some(n) = u.get(key).and_then(Value::as_u64) {
                *slot = (*slot).max(n);
                self.saw_usage = true;
            }
        }
    }

    fn start_block(&mut self, index: u64, block: &Value, on_delta: &mut dyn FnMut(Delta)) {
        let field = |k: &str| str_at(block, k).to_string();
        let parsed = match str_at(block, "type") {
            "text" => {
                self.text_break = !self.text.is_empty();
                self.push_text(&field("text"), on_delta);
                Block::Text
            }
            "thinking" => {
                self.reasoning_break = !self.reasoning.is_empty();
                let text = field("thinking");
                self.push_reasoning(&text, on_delta);
                Block::Thinking { text, signature: field("signature"), after_calls: self.calls }
            }
            "redacted_thinking" => Block::Redacted { data: field("data"), after_calls: self.calls },
            "tool_use" => {
                let call = self.calls;
                self.calls += 1;
                let (id, name) = (field("id"), field("name"));
                on_delta(Delta::ToolCallStart { index: call, id: id.clone(), name: name.clone() });
                Block::ToolUse { call, id, name, json: String::new(), input: block.get("input").cloned().unwrap_or(Value::Null) }
            }
            _ => Block::Other,
        };
        self.blocks.push((index, parsed));
    }

    fn apply_delta(&mut self, index: Option<u64>, delta: &Value, on_delta: &mut dyn FnMut(Delta)) {
        let field = |k: &str| str_at(delta, k);
        match str_at(delta, "type") {
            "text_delta" => self.push_text(field("text"), on_delta),
            "thinking_delta" => {
                let piece = field("thinking");
                self.push_reasoning(piece, on_delta);
                if let Some(Block::Thinking { text, .. }) = self.block_mut(index) {
                    text.push_str(piece);
                }
            }
            "signature_delta" => {
                if let Some(Block::Thinking { signature, .. }) = self.block_mut(index) {
                    signature.push_str(field("signature"));
                }
            }
            "input_json_delta" => {
                let piece = field("partial_json");
                if let Some(Block::ToolUse { call, json, .. }) = self.block_mut(index) {
                    json.push_str(piece);
                    if !piece.is_empty() {
                        on_delta(Delta::ToolCallArgs { index: *call, chunk: piece.to_string() });
                    }
                }
            }
            _ => {}
        }
    }

    fn block_mut(&mut self, index: Option<u64>) -> Option<&mut Block> {
        let index = index?;
        self.blocks.iter_mut().find(|(i, _)| *i == index).map(|(_, b)| b)
    }

    fn push_text(&mut self, piece: &str, on_delta: &mut dyn FnMut(Delta)) {
        if piece.is_empty() {
            return;
        }
        if std::mem::take(&mut self.text_break) {
            self.text.push_str("\n\n");
            on_delta(Delta::Text("\n\n".into()));
        }
        self.text.push_str(piece);
        on_delta(Delta::Text(piece.to_string()));
    }

    fn push_reasoning(&mut self, piece: &str, on_delta: &mut dyn FnMut(Delta)) {
        if piece.is_empty() {
            return;
        }
        if std::mem::take(&mut self.reasoning_break) {
            self.reasoning.push_str("\n\n");
            on_delta(Delta::Reasoning("\n\n".into()));
        }
        self.reasoning.push_str(piece);
        on_delta(Delta::Reasoning(piece.to_string()));
    }

    /// The assembled response; its thinking goes into `memory` for replay. `at_eof`: the body
    /// ended without `message_stop` — complete only if the stop reason had arrived.
    pub fn finish(self, at_eof: bool, memory: &mut Memory) -> Result<Response, ApiError> {
        if at_eof && !self.done && self.stop.is_none() {
            return Err(truncated_stream("message_stop"));
        }
        let mut tool_calls = Vec::new();
        let mut replay = Vec::new();
        for (_, block) in self.blocks {
            match block {
                Block::ToolUse { id, name, json, input, .. } => {
                    // a non-streaming answer (or a terse gateway) puts the whole input in the start,
                    // some as the object's JSON text rather than the object
                    let (args, raw) = match input {
                        Value::Object(_) if json.trim().is_empty() => (input, String::new()),
                        Value::String(text) if json.trim().is_empty() => parse_args(&text),
                        _ => parse_args(&json),
                    };
                    tool_calls.push(ToolCall { id, name, args, raw });
                }
                // without a signature there is nothing the API could verify, so nothing to replay
                Block::Thinking { text, signature, after_calls } if !signature.is_empty() => replay.push(Replay { after_calls, block: json!({"type": "thinking", "thinking": text, "signature": signature}) }),
                Block::Redacted { data, after_calls } if !data.is_empty() => replay.push(Replay { after_calls, block: json!({"type": "redacted_thinking", "data": data}) }),
                _ => {}
            }
        }
        let usage = if self.saw_usage { Usage { input: self.fresh_input + self.cache_read + self.cache_write, output: self.output, cached: self.cache_read, estimated: false } } else { Usage::default() };
        let finish = match self.stop.as_deref() {
            Some("end_turn") | Some("stop_sequence") => Finish::Stop,
            Some("tool_use") => Finish::ToolCalls,
            Some("max_tokens") => Finish::Length,
            Some(other) => Finish::Other(other.to_string()),
            None if !tool_calls.is_empty() => Finish::ToolCalls,
            None => Finish::Stop,
        };
        let message = Message::Assistant { text: self.text, reasoning: self.reasoning, tool_calls };
        memory.remember(&message, replay);
        Ok(Response { message, usage, finish })
    }
}

/// A non-streaming answer: its content blocks go through the same block handling as a stream.
pub fn parse_response(body: &[u8], memory: &mut Memory) -> Result<Response, ApiError> {
    let v: Value = serde_json::from_slice(body).map_err(|e| malformed("JSON", e))?;
    if v.get("type").and_then(Value::as_str) == Some("error") || v.get("error").is_some_and(|e| !e.is_null()) {
        return Err(embedded_error(&v));
    }
    let content = v.get("content").and_then(Value::as_array).ok_or_else(|| malformed("a message", "it has no content"))?;
    let mut stream = Stream::default();
    let mut ignore = |_: Delta| {};
    for (i, block) in content.iter().enumerate() {
        stream.start_block(i as u64, block, &mut ignore);
    }
    if let Some(u) = v.get("usage") {
        stream.note_usage(u);
    }
    stream.stop = v.get("stop_reason").and_then(Value::as_str).map(str::to_string);
    stream.done = true;
    stream.finish(false, memory)
}

#[cfg(test)]
mod tests {
    use super::super::http::SseSplitter;
    use super::*;
    use crate::types::ToolDef;

    const FIXTURE: &str = include_str!("testdata/anthropic-stream.sse");

    fn feed(sse: &str, memory: &mut Memory) -> (Result<Response, ApiError>, Vec<Delta>) {
        let mut splitter = SseSplitter::default();
        let mut events = Vec::new();
        splitter.feed(sse.as_bytes(), &mut events);
        splitter.finish(&mut events);
        let mut stream = Stream::default();
        let mut deltas = Vec::new();
        let mut ended = false;
        for e in &events {
            match stream.on_event(e, &mut |d| deltas.push(d)) {
                Ok(true) => {
                    ended = true;
                    break;
                }
                Ok(false) => {}
                Err(err) => return (Err(err), deltas),
            }
        }
        (stream.finish(!ended, memory), deltas)
    }

    fn tools() -> Vec<ToolDef> {
        vec![ToolDef { name: "list_dir".into(), description: "list a directory".into(), parameters: json!({"type": "object", "properties": {"path": {"type": "string"}}}) }]
    }

    fn request<'a>(messages: &'a [Message], tools: &'a [ToolDef], effort: &'a str) -> Request<'a> {
        Request { model: "m1", effort, system: "be careful", messages, tools, max_output_tokens: None }
    }

    fn bad_request(message: &str) -> ApiError {
        ApiError { kind: ErrorKind::BadRequest, status: Some(400), message: message.into(), retry_after_ms: None }
    }

    #[test]
    fn the_recorded_stream_assembles_thinking_text_a_tool_call_and_cache_usage() {
        let mut memory = Memory::default();
        let (resp, deltas) = feed(FIXTURE, &mut memory);
        let resp = resp.expect("complete stream");
        let Message::Assistant { text, reasoning, tool_calls } = &resp.message else { panic!("not an assistant message") };
        assert_eq!((text.as_str(), reasoning.as_str()), ("Let me look around.", "Check the tree first."));
        assert_eq!(tool_calls, &[ToolCall { id: "toolu_1".into(), name: "list_dir".into(), args: json!({"path": "src", "depth": 2}), raw: String::new() }]);
        assert_eq!(resp.usage, Usage { input: 4312, output: 87, cached: 4000, estimated: false }, "input counts the cached and the cache-written tokens");
        assert_eq!(resp.finish, Finish::ToolCalls);
        assert_eq!(
            deltas,
            vec![
                Delta::Reasoning("Check the tree first.".into()),
                Delta::Text("Let me look ".into()),
                Delta::Text("around.".into()),
                Delta::ToolCallStart { index: 0, id: "toolu_1".into(), name: "list_dir".into() },
                Delta::ToolCallArgs { index: 0, chunk: "{\"path\": \"sr".into() },
                Delta::ToolCallArgs { index: 0, chunk: "c\", \"depth\": 2}".into() },
            ]
        );
        assert_eq!(memory.replays.len(), 1, "the signed thinking block is kept for replay");
    }

    #[test]
    fn thinking_is_replayed_in_place_and_forgotten_with_its_message() {
        let mut memory = Memory::default();
        let (resp, _) = feed(FIXTURE, &mut memory);
        let assistant = resp.expect("ok").message;
        let tools = tools();
        let history = vec![Message::User { text: "look around".into() }, assistant, Message::Tool { images: vec![], call_id: "toolu_1".into(), name: "list_dir".into(), text: "src/\nCargo.toml".into(), is_error: false }];
        let b = body(&request(&history, &tools, "high"), true, &mut memory);
        assert_eq!(
            b["messages"][1],
            json!({"role": "assistant", "content": [
                {"type": "thinking", "thinking": "Check the tree first.", "signature": "c2lnLTE="},
                {"type": "text", "text": "Let me look around."},
                {"type": "tool_use", "id": "toolu_1", "name": "list_dir", "input": {"path": "src", "depth": 2}},
            ]})
        );
        assert_eq!(b["messages"][2], json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_1", "content": "src/\nCargo.toml"}]}));
        let plain = body(&request(&history, &[], ""), false, &mut memory);
        assert!(!plain.to_string().contains("signature") && !plain.to_string().contains("tool_use"), "no replay, no tool blocks without tools: {plain}");
        assert_eq!(memory.replays.len(), 1, "a tool-less request (compaction) does not prune the store");
        let compacted = vec![Message::User { text: "[context summary]".into() }];
        body(&request(&compacted, &tools, "high"), true, &mut memory);
        assert!(memory.replays.is_empty(), "the message left the history, so its thinking did too");
    }

    #[test]
    fn interleaved_thinking_goes_back_between_the_calls_it_came_between() {
        let sse = [
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"a","signature":"s1"}}"#,
            r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"t1","name":"glob","input":{"pattern":"*"}}}"#,
            r#"data: {"type":"content_block_start","index":2,"content_block":{"type":"redacted_thinking","data":"opaque"}}"#,
            r#"data: {"type":"content_block_start","index":3,"content_block":{"type":"tool_use","id":"t2","name":"glob","input":{}}}"#,
            r#"data: {"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#,
            r#"data: {"type":"message_stop"}"#,
        ]
        .join("\n\n");
        let mut memory = Memory::default();
        let assistant = feed(&sse, &mut memory).0.expect("ok").message;
        let history = vec![Message::User { text: "go".into() }, assistant];
        let b = body(&request(&history, &tools(), ""), true, &mut memory);
        let types: Vec<&str> = b["messages"][1]["content"].as_array().expect("blocks").iter().filter_map(|x| x["type"].as_str()).collect();
        assert_eq!(types, ["thinking", "tool_use", "redacted_thinking", "tool_use"]);
    }

    #[test]
    fn the_body_alternates_roles_and_merges_results_with_a_following_note() {
        let history = vec![
            Message::User { text: "go".into() },
            Message::Assistant { text: String::new(), reasoning: String::new(), tool_calls: vec![ToolCall { id: "t1".into(), name: "list_dir".into(), args: Value::Null, raw: "{oops".into() }] },
            Message::Tool { images: vec![], call_id: "t1".into(), name: "list_dir".into(), text: "bad arguments".into(), is_error: true },
            Message::User { text: "[note] also check tests/".into() },
            Message::User { text: "  ".into() },
        ];
        let tools = tools();
        let b = body(&request(&history, &tools, ""), true, &mut Memory::default());
        assert_eq!(b["system"], "be careful");
        assert_eq!(b["max_tokens"], 16384);
        assert_eq!((b["stream"].clone(), b["cache_control"].clone()), (json!(true), json!({"type": "ephemeral"})));
        assert_eq!(b["tools"][0], json!({"name": "list_dir", "description": "list a directory", "input_schema": {"type": "object", "properties": {"path": {"type": "string"}}}}));
        assert!(b.get("thinking").is_none() && b.get("output_config").is_none());
        let messages = b["messages"].as_array().expect("messages");
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[1], json!({"role": "assistant", "content": [{"type": "tool_use", "id": "t1", "name": "list_dir", "input": {}}]}));
        assert_eq!(
            messages[2],
            json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": "bad arguments", "is_error": true},
                {"type": "text", "text": "[note] also check tests/"},
            ]})
        );
        let starts_with_assistant = [Message::Assistant { text: "earlier".into(), reasoning: String::new(), tool_calls: vec![] }, Message::User { text: "go on".into() }];
        let b = body(&request(&starts_with_assistant, &tools, ""), false, &mut Memory::default());
        assert_eq!(b["messages"][0]["role"], "user", "the first message must be the user's");
    }

    #[test]
    fn effort_maps_to_adaptive_thinking_with_room_to_answer() {
        let messages = [Message::User { text: "hi".into() }];
        let b = body(&request(&messages, &[], "high"), true, &mut Memory::default());
        assert_eq!(b["thinking"], json!({"type": "adaptive", "display": "summarized"}));
        assert_eq!(b["output_config"], json!({"effort": "high"}));
        assert_eq!(b["max_tokens"], 16384 + 4096);
        let b = body(&request(&messages, &[], "minimal"), true, &mut Memory::default());
        assert_eq!(b["output_config"], json!({"effort": "low"}));
        assert_eq!(b["max_tokens"], 16384, "the low budget already fits");
        let big = Request { max_output_tokens: Some(64000), ..request(&messages, &[], "max") };
        assert_eq!(body(&big, true, &mut Memory::default())["max_tokens"], 64000);
        assert!(body(&request(&messages, &[], "none"), true, &mut Memory::default()).get("thinking").is_none());
    }

    #[test]
    fn rejected_reasoning_controls_step_down_the_ladder() {
        let messages = [Message::User { text: "hi".into() }];
        let req = request(&messages, &[], "xhigh");
        let mut m = Memory::default();
        body(&req, true, &mut m);
        assert!(m.adapt(&ApiError::new(ErrorKind::Transient, "thinking")).is_none(), "only a 400 adapts");
        assert!(m.adapt(&bad_request("thinking.type: Input should be 'enabled' or 'disabled'")).is_some());
        let b = body(&req, true, &mut m);
        assert_eq!(b["thinking"], json!({"type": "enabled", "budget_tokens": 32768}));
        assert!(b.get("output_config").is_none());
        assert_eq!(b["max_tokens"], 32768 + 4096);
        assert!(m.adapt(&bad_request("thinking: budget_tokens is not supported for this model")).is_some());
        let b = body(&req, true, &mut m);
        assert_eq!((b.get("thinking"), b["output_config"].clone()), (None, json!({"effort": "xhigh"})));
        assert!(m.adapt(&bad_request("output_config: Extra inputs are not permitted")).is_some());
        let b = body(&req, true, &mut m);
        assert!(b.get("thinking").is_none() && b.get("output_config").is_none());
        assert_eq!(b["max_tokens"], 16384, "nothing to make room for");
        assert!(m.adapt(&bad_request("thinking is weird")).is_none(), "the ladder ends");
    }

    #[test]
    fn a_refused_display_option_keeps_adaptive_thinking() {
        let messages = [Message::User { text: "hi".into() }];
        let req = request(&messages, &[], "high");
        let mut m = Memory::default();
        body(&req, true, &mut m);
        assert!(m.adapt(&bad_request("thinking.adaptive.display: Extra inputs are not permitted")).is_some());
        let b = body(&req, true, &mut m);
        assert_eq!((b["thinking"].clone(), b["output_config"].clone()), (json!({"type": "adaptive"}), json!({"effort": "high"})));
        assert!(m.adapt(&bad_request("thinking.type: Input should be 'enabled' or 'disabled'")).is_some(), "a later refusal of thinking itself still steps down");
        assert_eq!(body(&req, true, &mut m)["thinking"], json!({"type": "enabled", "budget_tokens": 16384}));
    }

    #[test]
    fn a_refused_max_tokens_or_cache_control_is_dropped_and_a_bad_signature_clears_the_store() {
        let messages = [Message::User { text: "hi".into() }];
        let mut m = Memory::default();
        body(&request(&messages, &[], "high"), true, &mut m);
        assert!(m.adapt(&bad_request("max_tokens: 20480 > 8192, which is the maximum allowed number of output tokens")).is_some());
        assert!(m.adapt(&bad_request("cache_control: Extra inputs are not permitted")).is_some());
        let b = body(&request(&messages, &[], "high"), true, &mut m);
        assert_eq!(b["max_tokens"], 16384);
        assert!(b.get("cache_control").is_none());

        let (resp, _) = feed(FIXTURE, &mut m);
        let history = vec![Message::User { text: "look".into() }, resp.expect("ok").message, Message::Tool { images: vec![], call_id: "toolu_1".into(), name: "list_dir".into(), text: "x".into(), is_error: false }];
        let tools = tools();
        body(&request(&history, &tools, ""), true, &mut m);
        assert!(m.adapt(&bad_request("messages.1.content.0: Invalid `signature` in `thinking` block")).is_some());
        assert!(!body(&request(&history, &tools, ""), true, &mut m).to_string().contains("signature"));
        assert!(m.adapt(&bad_request("messages.1.content.0: Invalid `signature` in `thinking` block")).is_none(), "nothing replayed, nothing to clear");
    }

    #[test]
    fn error_events_and_cut_streams_are_classified() {
        let (err, _) = feed("event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n", &mut Memory::default());
        let err = err.expect_err("error event");
        assert_eq!(err.kind, ErrorKind::Transient);
        assert!(err.message.contains("Overloaded"));
        let cut = FIXTURE.split("event: message_delta").next().unwrap_or("");
        assert_eq!(feed(cut, &mut Memory::default()).0.expect_err("no stop reason").kind, ErrorKind::Transient);
        let no_stop_event = FIXTURE.split("event: message_stop").next().unwrap_or("");
        assert!(feed(no_stop_event, &mut Memory::default()).0.is_ok(), "the stop reason had arrived");
    }

    #[test]
    fn non_streaming_answers_parse_like_streams() {
        let body = br#"{"id":"msg_2","type":"message","role":"assistant","content":[{"type":"thinking","thinking":"hm","signature":"s"},{"type":"text","text":"Reading."},{"type":"tool_use","id":"t9","name":"read_file","input":{"path":"a.rs"}}],"stop_reason":"tool_use","usage":{"input_tokens":5,"cache_read_input_tokens":10,"output_tokens":3}}"#;
        let mut memory = Memory::default();
        let resp = parse_response(body, &mut memory).expect("parses");
        let Message::Assistant { text, reasoning, tool_calls } = &resp.message else { panic!("not an assistant message") };
        assert_eq!((text.as_str(), reasoning.as_str()), ("Reading.", "hm"));
        assert_eq!(tool_calls[0].args, json!({"path": "a.rs"}));
        assert_eq!(resp.usage, Usage { input: 15, output: 3, cached: 10, estimated: false });
        assert_eq!(memory.replays.len(), 1);
        let as_text = br#"{"id":"msg_3","type":"message","role":"assistant","content":[{"type":"tool_use","id":"t1","name":"read_file","input":"{\"path\": \"b.rs\"}"}],"stop_reason":"tool_use","usage":{"input_tokens":1,"output_tokens":1}}"#;
        let Message::Assistant { tool_calls, .. } = parse_response(as_text, &mut memory).expect("parses").message else { panic!("not an assistant message") };
        assert_eq!((&tool_calls[0].args, tool_calls[0].raw.as_str()), (&json!({"path": "b.rs"}), ""), "an input sent as its JSON text is unwrapped");
        let err = parse_response(br#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 300000 tokens > 200000 maximum"}}"#, &mut memory).expect_err("error body");
        assert_eq!(err.kind, ErrorKind::Context);
    }

    /// A result's images go inside its `tool_result` block, read from their files; one too big
    /// for the API is left out up front, and a refused image turns images off for the session.
    #[test]
    fn images_go_in_the_tool_result_block() {
        let dir = std::env::temp_dir().join(format!("bija-messages-img-{}", crate::types::uuid_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let small = dir.join("c1-1.png");
        let bytes = b"\x89PNG\r\n\x1a\nsmall".to_vec();
        std::fs::write(&small, &bytes).unwrap();
        let big = dir.join("c1-2.jpg");
        let mut huge = vec![0xFF, 0xD8, 0xFF];
        huge.resize(MAX_IMAGE_B64, 0);
        std::fs::write(&big, &huge).unwrap();
        let images = vec![crate::types::Image { mime: "image/png".into(), path: small.clone() }, crate::types::Image { mime: "image/jpeg".into(), path: big.clone() }];
        let assistant = Message::Assistant { text: String::new(), reasoning: String::new(), tool_calls: vec![ToolCall { id: "toolu_1".into(), name: "mcp__demo__render".into(), args: json!({}), raw: String::new() }] };
        let history = vec![Message::User { text: "render".into() }, assistant, Message::Tool { images, call_id: "toolu_1".into(), name: "mcp__demo__render".into(), text: "rendered".into(), is_error: false }];
        let tools = tools();
        let mut memory = Memory::default();
        let b = body(&request(&history, &tools, ""), true, &mut memory);
        assert_eq!(
            b["messages"][2],
            json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_1", "content": [
                {"type": "text", "text": "rendered"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": crate::types::base64_encode(&bytes)}},
            ]}]})
        );
        let warnings = memory.images.take_warnings();
        assert!(warnings.len() == 1 && warnings[0].contains("c1-2.jpg") && warnings[0].contains("more than this provider takes"), "{warnings:?}");
        // Without tools (a compaction) the result is plain text.
        assert!(!body(&request(&history, &[], ""), true, &mut memory).to_string().contains("\"image\""));

        body(&request(&history, &tools, ""), true, &mut memory);
        assert!(memory.adapt(&bad_request("messages.2.content.0.content.1.image.source.base64: image exceeds 5 MB maximum")).is_some());
        let b = body(&request(&history, &tools, ""), true, &mut memory);
        assert_eq!(b["messages"][2]["content"][0]["content"], "rendered", "text only from now on");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_endpoint_respects_a_base_that_already_has_the_version() {
        assert_eq!(endpoint("https://api.example.com"), "https://api.example.com/v1/messages");
        assert_eq!(endpoint("https://gw.example.com/anthropic/v1/"), "https://gw.example.com/anthropic/v1/messages");
    }
}
