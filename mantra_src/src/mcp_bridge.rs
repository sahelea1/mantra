//! `mantra mcp-bridge --sock <path> --agent <id>` (WP10.4).
//!
//! `claude` spawns this as an ordinary stdio MCP server per `--mcp-config` (`hub::claude::
//! mcp_config_json`). It speaks real MCP on stdin/stdout to `claude`, and a tiny line-delimited
//! JSON protocol over a Unix socket to the running Mantra process: it connects to `--sock`, sends
//! `{"agent":<id>,"hello":true}`, then for every `tools/list` sends `{"agent":<id>,"list":true}`
//! and answers with whatever `{"tools":[...]}` comes back, and for every `tools/call` sends
//! `{"agent":<id>,"call":"<n>","tool":"<name>","args":{...}}` and answers with the matching
//! `{"call":"<n>","ok":bool,"text":"...","items":[…]?}` (`hub::claude::run_claude_process` on the
//! other end owns that socket once `Hub`'s accept loop hands the connection to the right agent —
//! see `hub.rs` and `hub/claude.rs`). `items` comes only with an answer that has an image in it (a
//! screenshot): the engine's `contentItems`, whose `inputImage` this turns into MCP image content.
//! `claude` only ever has one request in flight on this connection at a time (it awaits each tool
//! call before issuing the next), so this reads one line, blocks for one line back, and repeats —
//! no request/response correlation needed on either side.
//!
//! The stdio side is `serve`, shared with `mantra mock-mcp` (`mock_mcp.rs`): the JSON-RPC framing
//! and the handshake are the same for any server Mantra itself runs; only the tools differ.

use serde_json::{json, Value};
use std::path::PathBuf;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader, Lines};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;

fn parse_args(args: &[String]) -> Option<(PathBuf, u64)> {
    let get = |flag: &str| args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned();
    Some((PathBuf::from(get("--sock")?), get("--agent")?.parse().ok()?))
}

pub(crate) async fn write_line<W: AsyncWrite + Unpin>(w: &mut W, v: &Value) -> std::io::Result<()> {
    w.write_all(v.to_string().as_bytes()).await?;
    w.write_all(b"\n").await?;
    w.flush().await
}

/// The tools of a stdio MCP server built on `serve`.
pub(crate) trait ToolServer {
    /// The `tools` array `tools/list` answers with.
    async fn list(&mut self) -> Value;
    /// One `tools/call`'s result: `{"content":[…],"isError":bool}`.
    async fn call(&mut self, name: &str, args: Value) -> Value;
}

/// The protocol versions a server on `serve` can speak: it only has tools, the same at each.
const PROTOCOLS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

/// What `initialize` is answered with: the version the client asked for when `negotiate` and it is
/// one of `PROTOCOLS`, else the newest of them — or, without `negotiate`, `2024-11-05` always (the
/// bridge has answered so since WP10.4, and `claude` takes it).
fn protocol_for(requested: Option<&str>, negotiate: bool) -> &'static str {
    if !negotiate {
        return "2024-11-05";
    }
    requested.and_then(|r| PROTOCOLS.iter().find(|p| **p == r)).copied().unwrap_or(PROTOCOLS[0])
}

/// Serves MCP over NDJSON JSON-RPC 2.0 until the client closes `rd` (that is how `claude` and
/// bija shut a stdio server down): the handshake, `ping`, and `tools/list`/`tools/call` through
/// `tools`. One request at a time, answered in order.
pub(crate) async fn serve<R: AsyncBufRead + Unpin, W: AsyncWrite + Unpin, T: ToolServer>(rd: R, wr: &mut W, name: &str, negotiate: bool, tools: &mut T) {
    let mut lines = rd.lines();
    loop {
        let line = match lines.next_line().await {
            Ok(Some(l)) => l,
            Ok(None) | Err(_) => break, // the client closed our stdin: it's shutting this server down
        };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(req) = serde_json::from_str::<Value>(&line) else { continue };
        let id = req.get("id").cloned();
        let result = match req.get("method").and_then(|m| m.as_str()).unwrap_or("") {
            "initialize" => json!({
                "protocolVersion": protocol_for(req.pointer("/params/protocolVersion").and_then(|v| v.as_str()), negotiate),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": name, "version": env!("CARGO_PKG_VERSION")},
            }),
            "notifications/initialized" | "notifications/cancelled" => continue, // no response for notifications
            "ping" => json!({}),
            "tools/list" if id.is_some() => json!({"tools": tools.list().await}),
            "tools/call" if id.is_some() => {
                let tool = req.pointer("/params/name").and_then(|n| n.as_str()).unwrap_or("").to_string();
                let arguments = req.pointer("/params/arguments").cloned().unwrap_or_else(|| json!({}));
                tools.call(&tool, arguments).await
            }
            // Unknown request: answer an empty result rather than nothing, so a client that
            // insists on a reply (rare — most send only what §10.4 lists) doesn't hang forever.
            _ => json!({}),
        };
        let Some(id) = id else { continue };
        let _ = write_line(wr, &json!({"jsonrpc": "2.0", "id": id, "result": result})).await;
    }
}

/// An MCP text content block.
pub(crate) fn text_content(text: &str) -> Value {
    json!({"type": "text", "text": text})
}

/// The MCP content of the Hub's answer: its `text` — or, when it handed over the engine's `items`
/// because one of them is an image, each item: `inputText` as text, an `inputImage` data URL as
/// MCP image content (`{"type":"image","data","mimeType"}`), so `claude` shows the model the
/// picture. The text always leads, as the engine always puts the path in it.
fn call_content(answer: &Value) -> Vec<Value> {
    let text = answer.get("text").and_then(|t| t.as_str()).unwrap_or("");
    let Some(items) = answer.get("items").and_then(|i| i.as_array()) else { return vec![text_content(text)] };
    let mut out = vec![];
    for it in items {
        match it.get("type").and_then(|t| t.as_str()) {
            Some("inputText" | "text") => {
                if let Some(t) = it.get("text").and_then(|t| t.as_str()) {
                    out.push(text_content(t));
                }
            }
            Some("inputImage") => {
                if let Some((mime, data)) = it.get("imageUrl").and_then(|u| u.as_str()).and_then(crate::hub::data_url_parts) {
                    out.push(json!({"type": "image", "data": data, "mimeType": mime}));
                }
            }
            _ => {}
        }
    }
    if !out.iter().any(|c| c["type"] == "text") {
        out.insert(0, text_content(text));
    }
    out
}

/// The bridge's tools are the agent's role tools, asked for and answered over the Hub's socket.
struct Bridge {
    agent: u64,
    wr: OwnedWriteHalf,
    sock_lines: Lines<BufReader<OwnedReadHalf>>,
    call_n: u64,
}

impl ToolServer for Bridge {
    async fn list(&mut self) -> Value {
        let _ = write_line(&mut self.wr, &json!({"agent": self.agent, "list": true})).await;
        match self.sock_lines.next_line().await {
            Ok(Some(l)) => serde_json::from_str::<Value>(&l).ok().and_then(|v| v.get("tools").cloned()).unwrap_or_else(|| json!([])),
            _ => json!([]), // Mantra's end of the bridge is gone; answer empty rather than hang
        }
    }

    async fn call(&mut self, name: &str, args: Value) -> Value {
        self.call_n += 1;
        let call_id = self.call_n.to_string();
        let _ = write_line(&mut self.wr, &json!({"agent": self.agent, "call": call_id, "tool": name, "args": args})).await;
        let (ok, content) = match self.sock_lines.next_line().await {
            Ok(Some(l)) => {
                let v: Value = serde_json::from_str(&l).unwrap_or_else(|_| json!({}));
                (v.get("ok").and_then(|o| o.as_bool()).unwrap_or(false), call_content(&v))
            }
            _ => (false, vec![text_content("mantra mcp-bridge: lost the connection to Mantra")]),
        };
        json!({"content": content, "isError": !ok})
    }
}

pub async fn run() {
    let args: Vec<String> = std::env::args().skip(2).collect();
    let Some((sock, agent)) = parse_args(&args) else {
        eprintln!("mantra mcp-bridge: usage: mantra mcp-bridge --sock <path> --agent <id>");
        std::process::exit(2);
    };
    let stream = match UnixStream::connect(&sock).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("mantra mcp-bridge: couldn't connect to {}: {e}", sock.display());
            std::process::exit(1);
        }
    };
    let (rd, mut wr) = stream.into_split();
    if write_line(&mut wr, &json!({"agent": agent, "hello": true})).await.is_err() {
        eprintln!("mantra mcp-bridge: hello failed");
        std::process::exit(1);
    }
    let mut bridge = Bridge { agent, wr, sock_lines: BufReader::new(rd).lines(), call_n: 0 };
    serve(BufReader::new(tokio::io::stdin()), &mut tokio::io::stdout(), "mantra", false, &mut bridge).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_sock_and_agent_in_either_order() {
        let a = vec!["--sock".to_string(), "/tmp/x.sock".to_string(), "--agent".to_string(), "3".to_string()];
        assert_eq!(parse_args(&a), Some((PathBuf::from("/tmp/x.sock"), 3)));
        let b = vec!["--agent".to_string(), "9".to_string(), "--sock".to_string(), "/tmp/y.sock".to_string()];
        assert_eq!(parse_args(&b), Some((PathBuf::from("/tmp/y.sock"), 9)));
        assert_eq!(parse_args(&["--sock".to_string(), "/tmp/x.sock".to_string()]), None, "missing --agent");
        assert_eq!(parse_args(&["--sock".to_string(), "/tmp/x.sock".to_string(), "--agent".to_string(), "nope".to_string()]), None, "--agent must be numeric — the Hub's hello parser reads it as a JSON number");
    }

    /// The Hub's plain answer is one text block, as it always was; one that hands over items
    /// because of a screenshot becomes text plus MCP image content.
    #[test]
    fn an_answer_with_an_image_becomes_mcp_image_content() {
        assert_eq!(call_content(&json!({"call": "1", "ok": true, "text": "fine"})), vec![json!({"type": "text", "text": "fine"})]);
        let shot = json!({"call": "2", "ok": true, "text": "saved /r/s/w-1.png", "items": [
            {"type": "inputText", "text": "saved /r/s/w-1.png"},
            {"type": "inputImage", "imageUrl": "data:image/png;base64,iVBORw0KGgo="},
        ]});
        assert_eq!(call_content(&shot), vec![json!({"type": "text", "text": "saved /r/s/w-1.png"}), json!({"type": "image", "data": "iVBORw0KGgo=", "mimeType": "image/png"})]);
        // items with no text item still lead with the answer's text; a non-data URL is dropped
        let bare = json!({"text": "saved", "items": [{"type": "inputImage", "imageUrl": "data:image/jpeg;base64,/9j/"}, {"type": "inputImage", "imageUrl": "https://x/y.png"}]});
        assert_eq!(call_content(&bare), vec![json!({"type": "text", "text": "saved"}), json!({"type": "image", "data": "/9j/", "mimeType": "image/jpeg"})]);
    }

    struct Echo;
    impl ToolServer for Echo {
        async fn list(&mut self) -> Value {
            json!([{"name": "echo", "inputSchema": {"type": "object"}}])
        }
        async fn call(&mut self, name: &str, args: Value) -> Value {
            json!({"content": [text_content(&format!("{name} {args}"))], "isError": false})
        }
    }

    /// `serve`'s JSON-RPC: every request answered in order under its id, notifications never, an
    /// unknown method with an empty result; the bridge keeps answering `2024-11-05`, a negotiating
    /// server echoes a version it knows and offers its newest for one it does not.
    #[tokio::test]
    async fn serve_answers_requests_in_order_and_negotiates_only_when_asked() {
        for (negotiate, asked, answered) in [(false, "2025-06-18", "2024-11-05"), (true, "2025-03-26", "2025-03-26"), (true, "2099-01-01", "2025-06-18")] {
            let input = [
                json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": asked}}),
                json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
                json!({"jsonrpc": "2.0", "id": "p", "method": "ping"}),
                json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
                json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "echo", "arguments": {"a": 1}}}),
                json!({"jsonrpc": "2.0", "id": 4, "method": "resources/list"}),
            ]
            .iter()
            .map(|v| format!("{v}\nnot json\n"))
            .collect::<String>();
            let mut out: Vec<u8> = vec![];
            serve(input.as_bytes(), &mut out, "t", negotiate, &mut Echo).await;
            let replies: Vec<Value> = String::from_utf8(out).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
            assert_eq!(replies.iter().map(|r| r["id"].clone()).collect::<Vec<_>>(), vec![json!(1), json!("p"), json!(2), json!(3), json!(4)]);
            assert_eq!(replies[0]["result"]["protocolVersion"], answered, "negotiate={negotiate} asked={asked}");
            assert_eq!(replies[0]["result"]["serverInfo"]["name"], "t");
            assert_eq!(replies[2]["result"]["tools"][0]["name"], "echo");
            assert_eq!(replies[3]["result"]["content"][0]["text"], "echo {\"a\":1}");
            assert_eq!(replies[4]["result"], json!({}));
        }
    }
}
