//! OpenAI-compatible Chat Completions wire (SPEC §7.3): `POST {base}/chat/completions`.
//!
//! This is the dialect nearly every provider, gateway and local server speaks, so the parser is
//! deliberately forgiving about the ways implementations differ: tool-call deltas without an
//! `index`, ids repeated on every chunk, arguments sent whole instead of in pieces (or as a
//! double-encoded string), reasoning under `reasoning_content` or `reasoning`, usage in any chunk
//! or none, a final chunk without `[DONE]`. What it never does is guess at text: content is
//! passed through exactly as streamed.
//!
//! The request side is strict the other way: it sends only what SPEC §7.3 lists, and the few
//! optional fields (`stream_options`, `reasoning_effort`, `max_tokens`, images) are dropped or
//! renamed for the session when a provider rejects them ([`Quirks`]).
//!
//! Images are the one thing Chat Completions cannot put where they belong: a `tool` message is
//! text only. So a result's images ride in a `user` message after the results — after *all* of
//! them, because every call of an assistant turn must be answered before anything else is said.

use super::{call_as_text, embedded_error, malformed, parse_args, refuses_images, result_as_text, truncated_stream, ImageFiles};
use crate::types::{str_at, ApiError, Delta, ErrorKind, Finish, Message, Request, Response, ToolCall, Usage};
use serde_json::{json, Map, Value};

pub fn endpoint(base: &str) -> String {
    format!("{}/chat/completions", base.trim().trim_end_matches('/'))
}

/// The optional request fields this provider turned out to reject, and what the last request sent.
#[derive(Debug, Default)]
pub struct Quirks {
    no_stream_options: bool,
    no_effort: bool,
    max_completion_tokens: bool,
    /// `""` instead of `null` as the content of an assistant turn that only calls tools.
    empty_content: bool,
    /// Tool results go as text only: the provider refused an image.
    no_images: bool,
    pub(crate) images: ImageFiles,
    sent: Sent,
}

#[derive(Debug, Default, Clone, Copy)]
struct Sent {
    stream_options: bool,
    effort: bool,
    max_tokens: bool,
    null_content: bool,
    images: bool,
}

impl Quirks {
    /// After a failed request: if a 400 names an optional field the request carried, stop sending
    /// it and say what changed (the caller resends at once). `None` = nothing to adapt.
    pub fn adapt(&mut self, err: &ApiError) -> Option<String> {
        if err.kind != ErrorKind::BadRequest {
            return None;
        }
        let m = err.message.to_ascii_lowercase();
        if self.sent.stream_options && m.contains("stream_options") {
            self.no_stream_options = true;
            return Some("the provider rejects `stream_options`; streaming without usage reports".into());
        }
        if refuses_images(err, self.sent.images) {
            self.no_images = true;
            return Some("the provider refused an image; tool results go as text only from now on".into());
        }
        if self.sent.max_tokens && !self.max_completion_tokens && m.contains("max_completion_tokens") {
            self.max_completion_tokens = true;
            return Some("the provider wants `max_completion_tokens` instead of `max_tokens`".into());
        }
        if self.sent.effort && m.contains("reasoning") {
            self.no_effort = true;
            return Some("the provider rejects `reasoning_effort`; continuing without one".into());
        }
        if self.sent.null_content && m.contains("content") && ["null", "none", "string"].iter().any(|w| m.contains(w)) {
            self.empty_content = true;
            return Some("the provider rejects `content: null` on tool-call turns; sending empty strings".into());
        }
        None
    }
}

/// The request body. Without tools (the compaction call) the history's tool calls and results go
/// as plain text: a tool message is only valid when the request defines tools.
pub fn body(req: &Request<'_>, stream: bool, quirks: &mut Quirks) -> Value {
    let plain = req.tools.is_empty();
    let mut messages = Vec::with_capacity(req.messages.len() + 1);
    if !req.system.is_empty() {
        messages.push(json!({"role": "system", "content": req.system}));
    }
    let null_content = if quirks.empty_content { json!("") } else { Value::Null };
    // The image parts of the run of tool results being written, sent once the run ends.
    let mut pictures: Vec<Value> = Vec::new();
    let mut sent_images = false;
    for m in req.messages {
        if !matches!(m, Message::Tool { .. }) && !pictures.is_empty() {
            messages.push(json!({"role": "user", "content": std::mem::take(&mut pictures)}));
        }
        messages.push(message_json(m, plain, &null_content));
        match m {
            Message::Tool { name, images, .. } if !plain && !quirks.no_images => {
                for img in images {
                    if let Some((mime, b64)) = quirks.images.load(img, None) {
                        pictures.push(json!({"type": "text", "text": format!("image from {name}: {}", img.path.display())}));
                        pictures.push(json!({"type": "image_url", "image_url": {"url": format!("data:{mime};base64,{b64}")}}));
                        sent_images = true;
                    }
                }
            }
            _ => {}
        }
    }
    if !pictures.is_empty() {
        messages.push(json!({"role": "user", "content": pictures}));
    }
    let mut body = Map::new();
    body.insert("model".into(), json!(req.model));
    body.insert("messages".into(), Value::Array(messages));
    if !plain {
        let tools = req.tools.iter().map(|t| json!({"type": "function", "function": {"name": t.name, "description": t.description, "parameters": t.parameters}})).collect();
        body.insert("tools".into(), Value::Array(tools));
    }
    let mut sent = Sent { null_content: !plain && !quirks.empty_content && req.messages.iter().any(only_calls), images: sent_images, ..Sent::default() };
    if stream {
        body.insert("stream".into(), json!(true));
        if !quirks.no_stream_options {
            body.insert("stream_options".into(), json!({"include_usage": true}));
            sent.stream_options = true;
        }
    }
    let effort = req.effort.trim();
    if !effort.is_empty() && !effort.eq_ignore_ascii_case("none") && !quirks.no_effort {
        body.insert("reasoning_effort".into(), json!(effort));
        sent.effort = true;
    }
    if let Some(n) = req.max_output_tokens {
        let field = if quirks.max_completion_tokens { "max_completion_tokens" } else { "max_tokens" };
        body.insert(field.into(), json!(n));
        sent.max_tokens = true;
    }
    quirks.sent = sent;
    Value::Object(body)
}

/// An assistant turn with tool calls and no text: its `content` is `null` in the canonical shape.
fn only_calls(m: &Message) -> bool {
    matches!(m, Message::Assistant { text, tool_calls, .. } if text.is_empty() && !tool_calls.is_empty())
}

fn message_json(m: &Message, plain: bool, no_text: &Value) -> Value {
    match m {
        Message::User { text } => json!({"role": "user", "content": text}),
        Message::Assistant { text, tool_calls, .. } if plain || tool_calls.is_empty() => {
            let mut content = text.clone();
            for call in tool_calls {
                if !content.is_empty() {
                    content.push_str("\n\n");
                }
                content.push_str(&call_as_text(call));
            }
            json!({"role": "assistant", "content": content})
        }
        Message::Assistant { text, tool_calls, .. } => {
            // Unparsable arguments go back as `{}`: the tool result already told the model, and a
            // gateway that re-parses `arguments` would refuse the whole history over them.
            let calls: Vec<Value> = tool_calls
                .iter()
                .map(|c| {
                    let arguments = if c.args.is_null() { "{}".to_string() } else { c.args.to_string() };
                    json!({"id": c.id, "type": "function", "function": {"name": c.name, "arguments": arguments}})
                })
                .collect();
            let content = if text.is_empty() { no_text.clone() } else { json!(text) };
            json!({"role": "assistant", "content": content, "tool_calls": calls})
        }
        Message::Tool { name, text, is_error, .. } if plain => json!({"role": "user", "content": result_as_text(name, text, *is_error)}),
        Message::Tool { call_id, text, .. } => json!({"role": "tool", "tool_call_id": call_id, "content": text}),
    }
}

/// A tool call being assembled from deltas. `index` is the provider's, when it sent one.
#[derive(Debug, Default)]
struct PartialCall {
    index: Option<u64>,
    id: String,
    name: String,
    args: String,
    announced: bool,
}

/// The streaming parser: feed it each SSE event's data, then [`Stream::finish`].
#[derive(Debug, Default)]
pub struct Stream {
    text: String,
    reasoning: String,
    calls: Vec<PartialCall>,
    usage: Option<Usage>,
    finish: Option<String>,
    done: bool,
}

impl Stream {
    /// One event's `data`. `Ok(true)` once `[DONE]` arrives; an `error` object is an `Err`.
    pub fn on_event(&mut self, data: &str, on_delta: &mut dyn FnMut(Delta)) -> Result<bool, ApiError> {
        let data = data.trim();
        if data.is_empty() {
            return Ok(false);
        }
        if data == "[DONE]" {
            self.done = true;
            return Ok(true);
        }
        let v: Value = serde_json::from_str(data).map_err(|e| malformed("stream event", e))?;
        if v.get("error").is_some_and(|e| !e.is_null()) {
            return Err(embedded_error(&v));
        }
        if let Some(u) = v.get("usage").filter(|u| u.is_object()) {
            self.usage = Some(parse_usage(u));
        }
        let first_choice = v.get("choices").and_then(Value::as_array).and_then(|cs| cs.iter().find(|c| c.get("index").and_then(Value::as_u64).unwrap_or(0) == 0));
        let Some(choice) = first_choice else {
            return Ok(false);
        };
        if let Some(delta) = choice.get("delta") {
            if let Some(text) = delta.get("content").and_then(Value::as_str).filter(|t| !t.is_empty()) {
                self.text.push_str(text);
                on_delta(Delta::Text(text.to_string()));
            }
            if let Some(thought) = reasoning_of(delta) {
                self.reasoning.push_str(thought);
                on_delta(Delta::Reasoning(thought.to_string()));
            }
            for call in delta.get("tool_calls").and_then(Value::as_array).into_iter().flatten() {
                self.on_tool_delta(call, on_delta);
            }
        }
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish = Some(reason.to_string());
        }
        Ok(false)
    }

    fn on_tool_delta(&mut self, delta: &Value, on_delta: &mut dyn FnMut(Delta)) {
        let id = str_at(delta, "id");
        let at = self.slot_for(delta.get("index").and_then(Value::as_u64), id);
        let call = &mut self.calls[at];
        if call.id.is_empty() {
            call.id = id.to_string();
        }
        let function = delta.get("function");
        // Some servers repeat the name on every chunk; the first one is the name.
        if let Some(name) = function.and_then(|f| f.get("name")).and_then(Value::as_str) {
            if call.name.is_empty() {
                call.name = name.to_string();
            }
        }
        let chunk = arguments_text(function);
        if !call.announced && !call.name.is_empty() {
            call.announced = true;
            on_delta(Delta::ToolCallStart { index: at, id: call.id.clone(), name: call.name.clone() });
            if !call.args.is_empty() {
                on_delta(Delta::ToolCallArgs { index: at, chunk: call.args.clone() });
            }
        }
        if !chunk.is_empty() {
            call.args.push_str(&chunk);
            if call.announced {
                on_delta(Delta::ToolCallArgs { index: at, chunk });
            }
        }
    }

    /// Which call a delta continues: by the provider's index, else by id, else the latest one.
    fn slot_for(&mut self, index: Option<u64>, id: &str) -> usize {
        let found = match index {
            Some(i) => self.calls.iter().position(|c| c.index == Some(i)),
            None if !id.is_empty() => self.calls.iter().position(|c| c.id == id),
            None => self.calls.len().checked_sub(1),
        };
        found.unwrap_or_else(|| {
            self.calls.push(PartialCall { index, ..PartialCall::default() });
            self.calls.len() - 1
        })
    }

    /// The assembled response. `at_eof`: the body ended without `[DONE]`, which is still a
    /// complete answer when a `finish_reason` arrived (some servers just close), else a cut stream.
    pub fn finish(self, at_eof: bool) -> Result<Response, ApiError> {
        if at_eof && !self.done && self.finish.is_none() {
            return Err(truncated_stream("its final chunk"));
        }
        let tool_calls: Vec<ToolCall> = self
            .calls
            .into_iter()
            .map(|c| {
                let (args, raw) = parse_args(&c.args);
                ToolCall { id: c.id, name: c.name, args, raw }
            })
            .collect();
        let finish = finish_of(self.finish.as_deref(), !tool_calls.is_empty());
        Ok(Response { message: Message::Assistant { text: self.text, reasoning: self.reasoning, tool_calls }, usage: self.usage.unwrap_or_default(), finish })
    }
}

/// A non-streaming answer (`--no-stream`, or a server that ignored `stream: true`).
pub fn parse_response(body: &[u8]) -> Result<Response, ApiError> {
    let v: Value = serde_json::from_slice(body).map_err(|e| malformed("JSON", e))?;
    if v.get("error").is_some_and(|e| !e.is_null()) {
        return Err(embedded_error(&v));
    }
    let choice = v.get("choices").and_then(Value::as_array).and_then(|cs| cs.first()).ok_or_else(|| malformed("a completion", "it has no choices"))?;
    let message = choice.get("message").unwrap_or(&Value::Null);
    let text = match message.get("content") {
        Some(Value::String(s)) => s.clone(),
        // content parts: [{"type":"text","text":"…"}, …]
        Some(Value::Array(parts)) => parts.iter().filter_map(|p| p.get("text").and_then(Value::as_str)).collect(),
        _ => String::new(),
    };
    let reasoning = reasoning_of(message).unwrap_or("").to_string();
    let tool_calls: Vec<ToolCall> = message
        .get("tool_calls")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|c| {
            let function = c.get("function");
            let (args, raw) = parse_args(&arguments_text(function));
            ToolCall { id: str_at(c, "id").to_string(), name: function.map_or("", |f| str_at(f, "name")).to_string(), args, raw }
        })
        .collect();
    let usage = v.get("usage").filter(|u| u.is_object()).map(parse_usage).unwrap_or_default();
    let finish = finish_of(choice.get("finish_reason").and_then(Value::as_str), !tool_calls.is_empty());
    Ok(Response { message: Message::Assistant { text, reasoning, tool_calls }, usage, finish })
}

fn reasoning_of(v: &Value) -> Option<&str> {
    ["reasoning_content", "reasoning"].iter().find_map(|k| v.get(*k).and_then(Value::as_str)).filter(|r| !r.is_empty())
}

/// `function.arguments`: normally a string; an object sent whole is taken as its JSON text.
fn arguments_text(function: Option<&Value>) -> String {
    match function.and_then(|f| f.get("arguments")) {
        Some(Value::String(s)) => s.clone(),
        Some(v) if !v.is_null() => v.to_string(),
        _ => String::new(),
    }
}

/// `prompt_tokens` already includes the cached part; aliases cover servers that use other names.
fn parse_usage(u: &Value) -> Usage {
    let n = |path: &str| u.pointer(path).and_then(Value::as_u64);
    let input = n("/prompt_tokens").or_else(|| n("/input_tokens")).unwrap_or(0);
    let output = n("/completion_tokens").or_else(|| n("/output_tokens")).unwrap_or(0);
    let cached = n("/prompt_tokens_details/cached_tokens").or_else(|| n("/prompt_cache_hit_tokens")).or_else(|| n("/input_tokens_details/cached_tokens")).unwrap_or(0);
    Usage { input, output, cached, estimated: false }
}

fn finish_of(reason: Option<&str>, has_calls: bool) -> Finish {
    match reason {
        Some("stop") => Finish::Stop,
        Some("tool_calls") | Some("function_call") => Finish::ToolCalls,
        Some("length") => Finish::Length,
        Some(other) => Finish::Other(other.to_string()),
        None if has_calls => Finish::ToolCalls,
        None => Finish::Stop,
    }
}

#[cfg(test)]
mod tests {
    use super::super::http::SseSplitter;
    use super::*;
    use crate::types::ToolDef;

    const FIXTURE: &str = include_str!("testdata/openai-stream.sse");

    fn feed(sse: &str) -> (Result<Response, ApiError>, Vec<Delta>) {
        let mut splitter = SseSplitter::default();
        let mut events = Vec::new();
        splitter.feed(sse.as_bytes(), &mut events);
        splitter.finish(&mut events);
        let mut stream = Stream::default();
        let mut deltas = Vec::new();
        let mut ended = false;
        for e in &events {
            match stream.on_event(&e.data, &mut |d| deltas.push(d)) {
                Ok(true) => {
                    ended = true;
                    break;
                }
                Ok(false) => {}
                Err(err) => return (Err(err), deltas),
            }
        }
        (stream.finish(!ended), deltas)
    }

    fn assistant(r: &Response) -> (&str, &str, &[ToolCall]) {
        match &r.message {
            Message::Assistant { text, reasoning, tool_calls } => (text, reasoning, tool_calls),
            other => panic!("not an assistant message: {other:?}"),
        }
    }

    #[test]
    fn the_recorded_stream_assembles_text_reasoning_a_tool_call_and_usage() {
        let (resp, deltas) = feed(FIXTURE);
        let resp = resp.expect("complete stream");
        let (text, reasoning, calls) = assistant(&resp);
        assert_eq!(text, "Listing the files.");
        assert_eq!(reasoning, "The user wants a listing.");
        assert_eq!(calls, [ToolCall { id: "call_abc".into(), name: "bash".into(), args: json!({"command": "ls -la"}), raw: String::new() }]);
        assert_eq!(resp.usage, Usage { input: 1200, output: 48, cached: 1024, estimated: false });
        assert_eq!(resp.finish, Finish::ToolCalls);
        assert_eq!(
            deltas,
            vec![
                Delta::Reasoning("The user wants ".into()),
                Delta::Reasoning("a listing.".into()),
                Delta::Text("Listing ".into()),
                Delta::Text("the files.".into()),
                Delta::ToolCallStart { index: 0, id: "call_abc".into(), name: "bash".into() },
                Delta::ToolCallArgs { index: 0, chunk: "{\"comm".into() },
                Delta::ToolCallArgs { index: 0, chunk: "and\": \"ls".into() },
                Delta::ToolCallArgs { index: 0, chunk: " -la\"}".into() },
            ]
        );
    }

    #[test]
    fn the_recorded_stream_parses_the_same_with_crlf_line_ends() {
        let (lf, _) = feed(FIXTURE);
        let (crlf, _) = feed(&FIXTURE.replace('\n', "\r\n"));
        assert_eq!(crlf.expect("crlf"), lf.expect("lf"));
    }

    #[test]
    fn parallel_calls_are_kept_apart_and_index_less_continuations_join_the_latest() {
        let events = [
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"a","function":{"name":"read_file","arguments":"{\"path\":"}},{"index":1,"id":"b","function":{"name":"glob","arguments":""}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"function":{"arguments":"{\"pattern\":\"*.rs\"}"}},{"index":0,"id":"a","function":{"name":"read_file","arguments":"\"x\"}"}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"id":"c","function":{"name":"list_dir","arguments":"{"}}]}}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"function":{"arguments":"}"}}]},"finish_reason":"tool_calls"}]}"#,
        ];
        let mut s = Stream::default();
        for e in events {
            s.on_event(e, &mut |_| {}).expect("event");
        }
        let resp = s.finish(true).expect("finish_reason without [DONE] is a complete answer");
        let (_, _, calls) = assistant(&resp);
        let summary: Vec<(&str, &str, Value)> = calls.iter().map(|c| (c.id.as_str(), c.name.as_str(), c.args.clone())).collect();
        assert_eq!(summary, vec![("a", "read_file", json!({"path": "x"})), ("b", "glob", json!({"pattern": "*.rs"})), ("c", "list_dir", json!({}))]);
    }

    #[test]
    fn unparsable_arguments_become_null_with_the_raw_text_kept() {
        let mut s = Stream::default();
        s.on_event(r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"x","function":{"name":"bash","arguments":"{\"command\": \"ls"}}]},"finish_reason":"tool_calls"}]}"#, &mut |_| {}).expect("event");
        let resp = s.finish(false).expect("ok");
        let (_, _, calls) = assistant(&resp);
        assert_eq!((calls[0].args.clone(), calls[0].raw.as_str()), (Value::Null, "{\"command\": \"ls"));
    }

    #[test]
    fn an_error_chunk_is_classified() {
        let mut s = Stream::default();
        let err = s.on_event(r#"{"error":{"message":"Rate limit exceeded","code":429}}"#, &mut |_| {}).expect_err("error chunk");
        assert_eq!((err.kind, err.status), (ErrorKind::RateLimit, Some(429)));
        let err = Stream::default().on_event(r#"{"error":{"message":"The server is overloaded"}}"#, &mut |_| {}).expect_err("error chunk");
        assert_eq!(err.kind, ErrorKind::Transient);
        let err = Stream::default().on_event("{not json", &mut |_| {}).expect_err("torn event");
        assert_eq!(err.kind, ErrorKind::Transient);
    }

    #[test]
    fn a_stream_cut_before_its_end_is_transient() {
        let mut s = Stream::default();
        s.on_event(r#"{"choices":[{"index":0,"delta":{"content":"half"}}]}"#, &mut |_| {}).expect("event");
        let err = s.finish(true).expect_err("no finish_reason, no [DONE]");
        assert_eq!(err.kind, ErrorKind::Transient);
    }

    #[test]
    fn non_streaming_answers_parse_content_parts_and_tool_calls() {
        let body = br#"{"choices":[{"message":{"role":"assistant","content":[{"type":"text","text":"a"},{"type":"text","text":"b"}],"reasoning":"why","tool_calls":[{"id":"t1","type":"function","function":{"name":"bash","arguments":"{\"command\":\"pwd\"}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":9,"completion_tokens":4,"prompt_cache_hit_tokens":6}}"#;
        let resp = parse_response(body).expect("parses");
        let (text, reasoning, calls) = assistant(&resp);
        assert_eq!((text, reasoning), ("ab", "why"));
        assert_eq!(calls[0].args, json!({"command": "pwd"}));
        assert_eq!(resp.usage, Usage { input: 9, output: 4, cached: 6, estimated: false });
        assert_eq!(parse_response(br#"{"error":{"message":"bad","type":"invalid_request_error"}}"#).expect_err("error body").kind, ErrorKind::BadRequest);
        assert_eq!(parse_response(b"<html>").expect_err("not json").kind, ErrorKind::Transient);
    }

    fn history() -> Vec<Message> {
        vec![
            Message::User { text: "list".into() },
            Message::Assistant { text: String::new(), reasoning: "secret".into(), tool_calls: vec![ToolCall { id: "c1".into(), name: "bash".into(), args: json!({"command": "ls"}), raw: String::new() }, ToolCall { id: "c2".into(), name: "bash".into(), args: Value::Null, raw: "{bad".into() }] },
            Message::Tool { images: vec![], call_id: "c1".into(), name: "bash".into(), text: "a.rs".into(), is_error: false },
            Message::Tool { images: vec![], call_id: "c2".into(), name: "bash".into(), text: "bad arguments".into(), is_error: true },
            Message::Assistant { text: "done".into(), reasoning: String::new(), tool_calls: vec![] },
        ]
    }

    fn tools() -> Vec<ToolDef> {
        vec![ToolDef { name: "bash".into(), description: "run a command".into(), parameters: json!({"type": "object", "properties": {"command": {"type": "string"}}}) }]
    }

    #[test]
    fn the_body_carries_system_history_tools_and_streaming_options() {
        let (messages, tools) = (history(), tools());
        let req = Request { model: "m1", effort: "high", system: "be careful", messages: &messages, tools: &tools, max_output_tokens: None };
        let b = body(&req, true, &mut Quirks::default());
        assert_eq!(b["model"], "m1");
        assert_eq!(b["messages"][0], json!({"role": "system", "content": "be careful"}));
        assert_eq!(b["messages"][1], json!({"role": "user", "content": "list"}));
        assert_eq!(
            b["messages"][2],
            json!({"role": "assistant", "content": null, "tool_calls": [
                {"id": "c1", "type": "function", "function": {"name": "bash", "arguments": "{\"command\":\"ls\"}"}},
                {"id": "c2", "type": "function", "function": {"name": "bash", "arguments": "{}"}},
            ]})
        );
        assert_eq!(b["messages"][3], json!({"role": "tool", "tool_call_id": "c1", "content": "a.rs"}));
        assert_eq!(b["messages"][5], json!({"role": "assistant", "content": "done"}));
        assert!(!b.to_string().contains("secret"), "reasoning is never sent back");
        assert_eq!(b["tools"][0]["type"], "function");
        assert_eq!(b["tools"][0]["function"]["parameters"]["properties"]["command"]["type"], "string");
        assert_eq!((b["stream"].clone(), b["stream_options"].clone()), (json!(true), json!({"include_usage": true})));
        assert_eq!(b["reasoning_effort"], "high");
        assert!(b.get("max_tokens").is_none(), "only sent when configured");
    }

    #[test]
    fn optional_fields_follow_the_request() {
        let messages = [Message::User { text: "hi".into() }];
        for effort in ["", "none", "None"] {
            let req = Request { model: "m1", effort, system: "", messages: &messages, tools: &[], max_output_tokens: Some(512) };
            let b = body(&req, false, &mut Quirks::default());
            assert!(b.get("reasoning_effort").is_none() && b.get("stream").is_none() && b.get("tools").is_none());
            assert_eq!(b["messages"][0]["role"], "user", "no system message when there is no system prompt");
            assert_eq!(b["max_tokens"], 512);
        }
    }

    #[test]
    fn without_tools_the_history_goes_as_plain_text() {
        let messages = history();
        let req = Request { model: "m1", effort: "", system: "", messages: &messages, tools: &[], max_output_tokens: None };
        let b = body(&req, true, &mut Quirks::default());
        assert!(b.get("tools").is_none());
        assert!(!b.to_string().contains("tool_call"), "{b}");
        assert_eq!(b["messages"][1]["content"], "[tool call bash {\"command\":\"ls\"}]\n\n[tool call bash {bad]");
        assert_eq!(b["messages"][2], json!({"role": "user", "content": "[tool result of bash]\na.rs"}));
        assert_eq!(b["messages"][3]["content"], "[tool result of bash (failed)]\nbad arguments");
    }

    #[test]
    fn rejected_optional_fields_are_dropped_or_renamed_once() {
        let messages = [Message::User { text: "hi".into() }];
        let req = Request { model: "m1", effort: "low", system: "", messages: &messages, tools: &[], max_output_tokens: Some(100) };
        let mut q = Quirks::default();
        let bad = |m: &str| ApiError { kind: ErrorKind::BadRequest, status: Some(400), message: m.into(), retry_after_ms: None };
        body(&req, true, &mut q);
        assert!(q.adapt(&ApiError::new(ErrorKind::Transient, "stream_options")).is_none(), "only a 400 adapts");
        assert!(q.adapt(&bad("Unrecognized request argument supplied: stream_options")).is_some());
        assert!(q.adapt(&bad("Unsupported parameter: 'max_tokens' is not supported with this model. Use 'max_completion_tokens' instead.")).is_some());
        assert!(q.adapt(&bad("reasoning_effort is not supported by this model")).is_some());
        let b = body(&req, true, &mut q);
        assert!(b.get("stream_options").is_none() && b.get("reasoning_effort").is_none() && b.get("max_tokens").is_none());
        assert_eq!(b["max_completion_tokens"], 100);
        assert!(q.adapt(&bad("Unrecognized request argument supplied: stream_options")).is_none(), "nothing left to drop");
        assert!(q.adapt(&bad("messages: invalid role")).is_none());
    }

    #[test]
    fn a_provider_that_refuses_null_content_gets_empty_strings() {
        let (messages, tools) = (history(), tools());
        let req = Request { model: "m1", effort: "", system: "", messages: &messages, tools: &tools, max_output_tokens: None };
        let bad = ApiError { kind: ErrorKind::BadRequest, status: Some(400), message: "Invalid type for 'messages[1].content': expected a string, but got null".into(), retry_after_ms: None };
        let mut q = Quirks::default();
        assert_eq!(body(&req, true, &mut q)["messages"][1]["content"], Value::Null);
        assert!(q.adapt(&bad).is_some());
        assert_eq!(body(&req, true, &mut q)["messages"][1]["content"], "");
        assert!(q.adapt(&bad).is_none());
        let plain_history = [Message::User { text: "hi".into() }];
        let mut q = Quirks::default();
        body(&Request { messages: &plain_history, ..req }, true, &mut q);
        assert!(q.adapt(&bad).is_none(), "nothing null was sent");
    }

    /// A PNG on disk (only its signature matters to the wire) and the image record pointing at it.
    fn png_file(tag: &str) -> (crate::types::Image, Vec<u8>) {
        let path = std::env::temp_dir().join(format!("bija-chat-{tag}-{}.png", crate::types::uuid_v4()));
        let bytes = b"\x89PNG\r\n\x1a\nnot really pixels".to_vec();
        std::fs::write(&path, &bytes).unwrap();
        (crate::types::Image { mime: "image/png".into(), path }, bytes)
    }

    /// The tool message stays text; the image follows in a user message once every call of the
    /// turn is answered — before a steer that came after the results, never between two results.
    #[test]
    fn images_follow_the_tool_results_in_a_user_message() {
        let (img, bytes) = png_file("img");
        let mut messages = history();
        if let Message::Tool { images, .. } = &mut messages[2] {
            images.push(img.clone());
        }
        messages.insert(4, Message::User { text: "also look at b.rs".into() });
        let tools = tools();
        let req = Request { model: "m1", effort: "", system: "", messages: &messages, tools: &tools, max_output_tokens: None };
        let mut q = Quirks::default();
        let b = body(&req, true, &mut q);
        let roles: Vec<&str> = b["messages"].as_array().unwrap().iter().map(|m| m["role"].as_str().unwrap()).collect();
        assert_eq!(roles, ["user", "assistant", "tool", "tool", "user", "user", "assistant"]);
        assert_eq!(b["messages"][2], json!({"role": "tool", "tool_call_id": "c1", "content": "a.rs"}));
        let url = format!("data:image/png;base64,{}", crate::types::base64_encode(&bytes));
        assert_eq!(
            b["messages"][4],
            json!({"role": "user", "content": [
                {"type": "text", "text": format!("image from bash: {}", img.path.display())},
                {"type": "image_url", "image_url": {"url": url}},
            ]})
        );
        assert_eq!(b["messages"][5], json!({"role": "user", "content": "also look at b.rs"}));
        // The compaction request (no tools) carries no images.
        let plain = body(&Request { tools: &[], ..req }, true, &mut q);
        assert!(!plain.to_string().contains("image_url"));

        // A provider that takes no images: dropped for good, the text stays.
        let bad = ApiError { kind: ErrorKind::BadRequest, status: Some(400), message: "Invalid content type. image_url is only supported by certain models.".into(), retry_after_ms: None };
        body(&req, true, &mut q);
        assert!(q.adapt(&bad).is_some());
        let b = body(&req, true, &mut q);
        assert!(!b.to_string().contains("image_url") && b["messages"][2]["content"] == "a.rs");
        assert!(q.adapt(&bad).is_none(), "nothing left to drop");

        // A file gone since: left out, reported once, the text still goes.
        std::fs::remove_file(&img.path).unwrap();
        let mut q = Quirks::default();
        let b = body(&req, true, &mut q);
        body(&req, true, &mut q);
        assert!(!b.to_string().contains("image_url"));
        let warnings = q.images.take_warnings();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains(&img.path.display().to_string()), "{warnings:?}");
        assert!(q.adapt(&bad).is_none(), "no image went, so a 400 is not about one");
    }

    #[test]
    fn the_endpoint_appends_to_the_base() {
        assert_eq!(endpoint("https://api.example.com/v1/"), "https://api.example.com/v1/chat/completions");
        assert_eq!(endpoint("http://127.0.0.1:11434/v1"), "http://127.0.0.1:11434/v1/chat/completions");
    }
}
