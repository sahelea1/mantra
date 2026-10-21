//! A small stdio MCP client: what `mantra mcp test` checks a configured server with, and what
//! `mock_claude.rs` drives every server of its `--mcp-config` through, as `claude` would. JSON-RPC 2.0 over NDJSON on the server's stdin/stdout: `initialize` (asking for the newest
//! protocol, taking whatever the server answers), `notifications/initialized`, `tools/list` page by
//! page, `tools/call`. One request in flight at a time; the server's own requests in between are
//! answered (`ping`) or refused, its notifications and any stdout line that is not JSON (a banner,
//! a stray print) skipped. No timeouts in here — the caller puts one around what it awaits.

use crate::config::McpServer;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};

/// What `initialize` asks for; a server that speaks an older one says so, and that is used.
pub const PROTOCOL: &str = "2025-06-18";

/// How many of the server's last stderr lines are kept for an error message.
const STDERR_LINES: usize = 12;

/// `tools/list` pages followed before giving up on a server whose cursor never ends.
const MAX_PAGES: usize = 100;

/// One `tools/call`'s outcome: the text content joined by newlines, each image as
/// `(mime, base64)`, and whether the server called it an error. (`mantra mcp test` only lists
/// tools — it never calls one of a real server's, which could change a scene — so calls are made
/// by the tests and by `mock_claude.rs`.)
#[derive(Debug, Clone, PartialEq)]
pub struct ToolResult {
    pub is_error: bool,
    pub text: String,
    pub images: Vec<(String, String)>,
}

pub struct Client {
    write: Box<dyn AsyncWrite + Unpin + Send>,
    read: BufReader<Box<dyn AsyncRead + Unpin + Send>>,
    /// The server process, killed when the client is dropped; `None` over an in-memory pipe.
    _child: Option<Child>,
    stderr: Arc<Mutex<VecDeque<String>>>,
    /// The task filling `stderr`, done once the server closes it.
    stderr_reader: Option<tokio::task::JoinHandle<()>>,
    next_id: i64,
    /// The protocol version the server answered `initialize` with.
    pub protocol: String,
    /// `<name> <version>` from the server's `serverInfo`.
    pub server: String,
}

impl Client {
    /// Starts the server — `command` with `args`, `env` added to Mantra's own environment — and
    /// returns before the handshake (`initialize`), so a caller's timeout around that can still
    /// read the server's stderr when it never answers.
    pub fn launch(server: &McpServer) -> Result<Client, String> {
        let mut child = Command::new(&server.command)
            .args(&server.args)
            .envs(&server.env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("could not start `{}`: {e}", server.command))?;
        let (Some(stdin), Some(stdout), Some(stderr)) = (child.stdin.take(), child.stdout.take(), child.stderr.take()) else {
            return Err("the server's stdio pipes are missing".into());
        };
        let tail = Arc::new(Mutex::new(VecDeque::new()));
        let keep = tail.clone();
        let reader = tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if line.trim().is_empty() {
                    continue;
                }
                let mut t = keep.lock().unwrap_or_else(|e| e.into_inner());
                if t.len() == STDERR_LINES {
                    t.pop_front();
                }
                t.push_back(line);
            }
        });
        let mut c = Client::over(stdout, stdin);
        c._child = Some(child);
        c.stderr = tail;
        c.stderr_reader = Some(reader);
        Ok(c)
    }

    /// A client over any pair of streams — an in-memory pipe in the tests.
    pub fn over<R: AsyncRead + Unpin + Send + 'static, W: AsyncWrite + Unpin + Send + 'static>(read: R, write: W) -> Client {
        Client { write: Box::new(write), read: BufReader::new(Box::new(read)), _child: None, stderr: Arc::default(), stderr_reader: None, next_id: 0, protocol: String::new(), server: String::new() }
    }

    /// The server's last stderr lines, oldest first ("" when it said nothing).
    pub fn stderr_tail(&self) -> String {
        self.stderr.lock().map(|t| t.iter().cloned().collect::<Vec<_>>().join("\n")).unwrap_or_default()
    }

    /// `stderr_tail` once the server has said all it will — it closed stderr, as one that exits
    /// does — or after a second: what a failure quotes. A server that dies can close stdout
    /// before its last stderr line has been read.
    pub async fn last_words(&mut self) -> String {
        if let Some(reader) = self.stderr_reader.take() {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(1), reader).await;
        }
        self.stderr_tail()
    }

    /// The handshake: `initialize`, then `notifications/initialized`.
    pub async fn initialize(&mut self) -> Result<(), String> {
        let params = json!({"protocolVersion": PROTOCOL, "capabilities": {}, "clientInfo": {"name": "mantra", "version": env!("CARGO_PKG_VERSION")}});
        let r = self.request("initialize", params).await?;
        self.protocol = r.get("protocolVersion").and_then(|v| v.as_str()).unwrap_or(PROTOCOL).to_string();
        let info = |k: &str| r.pointer(&format!("/serverInfo/{k}")).and_then(|v| v.as_str()).unwrap_or("").to_string();
        self.server = format!("{} {}", info("name"), info("version")).trim().to_string();
        self.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).await
    }

    /// Every tool the server offers (`{name, description, inputSchema}`), all pages.
    pub async fn list_tools(&mut self) -> Result<Vec<Value>, String> {
        let mut tools = vec![];
        let mut cursor: Option<String> = None;
        let mut seen = std::collections::HashSet::new();
        for _ in 0..MAX_PAGES {
            let params = match &cursor {
                Some(c) => json!({"cursor": c}),
                None => json!({}),
            };
            let page = self.request("tools/list", params).await?;
            tools.extend(page.get("tools").and_then(|t| t.as_array()).cloned().unwrap_or_default());
            match page.get("nextCursor").and_then(|c| c.as_str()).filter(|c| !c.is_empty()) {
                // A cursor seen before would page forever.
                Some(c) if seen.insert(c.to_string()) => cursor = Some(c.to_string()),
                _ => return Ok(tools),
            }
        }
        Ok(tools)
    }

    pub async fn call_tool(&mut self, name: &str, args: Value) -> Result<ToolResult, String> {
        let r = self.request("tools/call", json!({"name": name, "arguments": args})).await?;
        let mut text = vec![];
        let mut images = vec![];
        for c in r.get("content").and_then(|c| c.as_array()).into_iter().flatten() {
            match c.get("type").and_then(|t| t.as_str()) {
                Some("text") => text.extend(c.get("text").and_then(|t| t.as_str()).map(str::to_string)),
                Some("image") => {
                    if let (Some(mime), Some(data)) = (c.get("mimeType").and_then(|m| m.as_str()), c.get("data").and_then(|d| d.as_str())) {
                        images.push((mime.to_string(), data.to_string()));
                    }
                }
                // An embedded text resource reads as text too.
                Some("resource") => text.extend(c.pointer("/resource/text").and_then(|t| t.as_str()).map(str::to_string)),
                _ => {}
            }
        }
        Ok(ToolResult { is_error: r.get("isError").and_then(|e| e.as_bool()).unwrap_or(false), text: text.join("\n"), images })
    }

    async fn send(&mut self, v: &Value) -> Result<(), String> {
        let mut line = v.to_string();
        line.push('\n');
        self.write.write_all(line.as_bytes()).await.map_err(|e| format!("the server's stdin closed: {e}"))?;
        self.write.flush().await.map_err(|e| format!("the server's stdin closed: {e}"))
    }

    /// One request, and its answer's `result` (an `error` answer is `Err` with its message).
    async fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})).await?;
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match self.read.read_until(b'\n', &mut buf).await {
                Ok(0) => return Err(format!("the server closed its output before answering {method}")),
                Ok(_) => {}
                Err(e) => return Err(format!("reading the server's output: {e}")),
            }
            let Ok(msg) = serde_json::from_slice::<Value>(&buf) else { continue };
            match (msg.get("method").and_then(|m| m.as_str()), msg.get("id")) {
                // The server asking something of us: a ping is answered, anything else refused.
                (Some(m), Some(rid)) => {
                    let reply = if m == "ping" {
                        json!({"jsonrpc": "2.0", "id": rid, "result": {}})
                    } else {
                        json!({"jsonrpc": "2.0", "id": rid, "error": {"code": -32601, "message": format!("mantra does not support {m}")}})
                    };
                    self.send(&reply).await?;
                }
                (Some(_), None) => {} // a notification (a log line, progress)
                (None, Some(rid)) if rid.as_i64() == Some(id) => {
                    if let Some(e) = msg.get("error") {
                        let text = e.get("message").and_then(|m| m.as_str()).unwrap_or("error").to_string();
                        return Err(format!("{method}: {text}"));
                    }
                    return Ok(msg.get("result").cloned().unwrap_or_else(|| json!({})));
                }
                _ => {} // an answer to something else (nothing else is ever in flight)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scripted server on the other end of a pipe: answers each request with `reply(request)`,
    /// first sending whatever `before(request)` says (its own requests, notifications, junk).
    async fn scripted(mut rd: impl tokio::io::AsyncBufRead + Unpin, mut wr: impl AsyncWrite + Unpin, script: fn(&Value) -> (Vec<String>, Option<Value>)) {
        let mut line = String::new();
        loop {
            line.clear();
            if rd.read_line(&mut line).await.unwrap_or(0) == 0 {
                return;
            }
            let Ok(req) = serde_json::from_str::<Value>(&line) else { continue };
            let (before, reply) = script(&req);
            for b in before {
                let _ = wr.write_all(format!("{b}\n").as_bytes()).await;
            }
            if let Some(r) = reply {
                let _ = wr.write_all(format!("{r}\n").as_bytes()).await;
            }
            let _ = wr.flush().await;
        }
    }

    fn pipe(script: fn(&Value) -> (Vec<String>, Option<Value>)) -> (Client, tokio::task::JoinHandle<()>) {
        let (a, b) = tokio::io::duplex(1 << 16);
        let (srd, swr) = tokio::io::split(b);
        let server = tokio::spawn(scripted(BufReader::new(srd), swr, script));
        let (crd, cwr) = tokio::io::split(a);
        (Client::over(crd, cwr), server)
    }

    /// An older server's protocol is taken; tools come from every page; the server's ping in the
    /// middle is answered, its notification and a banner line skipped; an error answer is `Err`.
    #[tokio::test]
    async fn handshake_pages_and_the_servers_own_messages() {
        let (mut c, _server) = pipe(|req| {
            let id = req["id"].clone();
            match req["method"].as_str().unwrap_or("") {
                "initialize" => (vec!["Starting server...".into()], Some(json!({"jsonrpc": "2.0", "id": id, "result": {"protocolVersion": "2024-11-05", "serverInfo": {"name": "old", "version": "1.0"}}}))),
                "tools/list" if req.pointer("/params/cursor").is_none() => (
                    vec![json!({"jsonrpc": "2.0", "id": "s1", "method": "ping"}).to_string(), json!({"jsonrpc": "2.0", "method": "notifications/message", "params": {}}).to_string()],
                    Some(json!({"jsonrpc": "2.0", "id": id, "result": {"tools": [{"name": "a"}], "nextCursor": "p2"}})),
                ),
                "tools/list" => (vec![], Some(json!({"jsonrpc": "2.0", "id": id, "result": {"tools": [{"name": "b"}], "nextCursor": "p2"}}))),
                "tools/call" => (vec![], Some(json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32602, "message": "no such tool"}}))),
                _ => (vec![], None), // the initialized notification, and our pong
            }
        });
        c.initialize().await.unwrap();
        assert_eq!((c.protocol.as_str(), c.server.as_str()), ("2024-11-05", "old 1.0"));
        let names: Vec<String> = c.list_tools().await.unwrap().iter().map(|t| t["name"].as_str().unwrap_or("").to_string()).collect();
        assert_eq!(names, vec!["a", "b"], "the second page's repeated cursor ends it");
        let e = c.call_tool("x", json!({})).await.unwrap_err();
        assert!(e.contains("no such tool"), "{e}");
    }

    /// A tool result's text blocks (and an embedded text resource) join with newlines; image
    /// blocks come back as (mime, base64); `isError` is kept.
    #[tokio::test]
    async fn a_tool_result_splits_into_text_and_images() {
        let (mut c, _server) = pipe(|req| {
            let id = req["id"].clone();
            let result = json!({"isError": true, "content": [
                {"type": "text", "text": "line one"},
                {"type": "image", "data": "iVBO", "mimeType": "image/png"},
                {"type": "resource", "resource": {"uri": "file:///a", "text": "line two"}},
                {"type": "audio", "data": "x", "mimeType": "audio/wav"},
            ]});
            (vec![], req.get("id").map(|_| json!({"jsonrpc": "2.0", "id": id, "result": result})))
        });
        let r = c.call_tool("render", json!({})).await.unwrap();
        assert_eq!(r, ToolResult { is_error: true, text: "line one\nline two".into(), images: vec![("image/png".into(), "iVBO".into())] });
    }

    /// A server that dies says so instead of hanging, and what it printed on stderr is kept.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_server_that_exits_is_an_error_with_its_stderr() {
        let s = McpServer { command: "sh".into(), args: vec!["-c".into(), "echo 'uvx: blender-mcp not found' >&2; exit 1".into()], ..Default::default() };
        let mut c = Client::launch(&s).unwrap();
        let e = c.initialize().await.unwrap_err();
        assert!(e.contains("closed"), "stdin or stdout, whichever it noticed first: {e}");
        assert_eq!(c.last_words().await, "uvx: blender-mcp not found");
        let missing = McpServer { command: "/nonexistent/mcp-server".into(), ..Default::default() };
        assert!(Client::launch(&missing).err().is_some_and(|e| e.contains("could not start")));
    }
}
