//! `mantra mcp-bridge` — a tiny MCP (Model Context Protocol) stdio server that Claude Code
//! spawns per agent, exposing Mantra's dynamic tools (`mantra_submit_plan`, `mantra_spawn`, …)
//! to a headless `claude -p` process.
//!
//! It owns no logic: every `tools/list` and `tools/call` is forwarded, one JSON line each, over
//! a Unix socket to the running Mantra process, which answers from the same engine code that
//! serves Codex's `item/tool/call` requests.
//!
//! Wire protocol on the socket (newline-delimited JSON, bridge → Mantra, then Mantra → bridge):
//!
//! ```text
//! {"agent": 7, "hello": true}                                  // first line after connecting
//! {"agent": 7, "call": "1", "list": true}                       // tools/list
//!     → {"call": "1", "tools": [ {name, description, inputSchema}, … ]}
//! {"agent": 7, "call": "2", "tool": "mantra_submit_plan", "args": {…}}   // tools/call
//!     → {"call": "2", "ok": true, "text": "plan accepted"}      // or ok:false + text = error
//! ```
//!
//! Replies are correlated by `call`, so Claude's parallel tool calls are fine. If the socket
//! closes, the bridge exits non-zero: Claude reports the MCP server as failed and Mantra
//! restarts the agent with a clear reason.
//!
//! Usage: `mantra mcp-bridge --sock <path> --agent <id>`

use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

/// MCP protocol version we answer with when the client's is unknown to us.
const MCP_VERSION: &str = "2024-11-05";

/// Parse `--sock <path> --agent <id>` from argv (after the subcommand name).
fn parse(args: &[String]) -> Result<(String, u32), String> {
    let mut sock = None;
    let mut agent = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--sock" => {
                sock = args.get(i + 1).cloned();
                i += 2;
            }
            "--agent" => {
                agent = args.get(i + 1).and_then(|a| a.parse::<u32>().ok());
                i += 2;
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    match (sock, agent) {
        (Some(s), Some(a)) => Ok((s, a)),
        _ => Err("usage: mantra mcp-bridge --sock <path> --agent <id>".into()),
    }
}

/// Entry point for the subcommand. Returns the process exit code.
pub async fn run(args: Vec<String>) -> i32 {
    let (sock, agent) = match parse(&args) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("mcp-bridge: {e}");
            return 2;
        }
    };
    let stream = match tokio::net::UnixStream::connect(&sock).await {
        Ok(s) => s,
        Err(e) => {
            eprintln!("mcp-bridge: cannot connect to {sock}: {e}");
            return 1;
        }
    };
    match serve(tokio::io::stdin(), tokio::io::stdout(), stream, agent).await {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("mcp-bridge: {e}");
            1
        }
    }
}

/// Pending socket requests, keyed by call id.
type Pending = Arc<Mutex<HashMap<String, oneshot::Sender<Value>>>>;

/// The bridge proper: MCP on `stdin`/`stdout`, Mantra on `stream`. Generic so tests can drive it
/// with in-memory pipes.
pub async fn serve<I, O, S>(stdin: I, stdout: O, stream: S, agent: u32) -> Result<(), String>
where
    I: AsyncRead + Unpin + Send + 'static,
    O: AsyncWrite + Unpin + Send + 'static,
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (sock_r, mut sock_w) = tokio::io::split(stream);
    let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
    let next = Arc::new(AtomicU64::new(1));

    // One writer task per direction keeps line writes atomic across concurrent tool calls.
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
    let mut stdout = stdout;
    tokio::spawn(async move {
        while let Some(l) = out_rx.recv().await {
            if stdout.write_all(l.as_bytes()).await.is_err() || stdout.write_all(b"\n").await.is_err() {
                break;
            }
            let _ = stdout.flush().await;
        }
    });
    let (sock_tx, mut sock_rx) = mpsc::unbounded_channel::<String>();
    let sock_closed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let closed = sock_closed.clone();
        tokio::spawn(async move {
            while let Some(l) = sock_rx.recv().await {
                if sock_w.write_all(l.as_bytes()).await.is_err() || sock_w.write_all(b"\n").await.is_err() {
                    closed.store(true, Ordering::SeqCst);
                    break;
                }
                let _ = sock_w.flush().await;
            }
        });
    }
    sock_tx.send(json!({"agent": agent, "hello": true}).to_string()).map_err(|_| "socket closed".to_string())?;

    // Socket reader: route replies to whoever is waiting on that call id.
    {
        let pending = pending.clone();
        let closed = sock_closed.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(sock_r).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
                let Some(id) = v.get("call").and_then(|c| c.as_str()) else { continue };
                let tx = pending.lock().ok().and_then(|mut p| p.remove(id));
                if let Some(tx) = tx {
                    let _ = tx.send(v);
                }
            }
            closed.store(true, Ordering::SeqCst);
            // Fail everything still waiting so Claude gets an error instead of a hang.
            if let Ok(mut p) = pending.lock() {
                for (_, tx) in p.drain() {
                    let _ = tx.send(json!({"ok": false, "text": "mantra disconnected"}));
                }
            }
        });
    }

    let mut lines = BufReader::new(stdin).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if sock_closed.load(Ordering::SeqCst) {
            return Err("mantra disconnected".into());
        }
        let Ok(req) = serde_json::from_str::<Value>(&line) else { continue };
        let id = req.get("id").cloned();
        let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("").to_string();
        let params = req.get("params").cloned().unwrap_or(Value::Null);
        // Notifications (no id) need no answer.
        let Some(id) = id else { continue };
        match method.as_str() {
            "initialize" => {
                let ver = params.get("protocolVersion").and_then(|v| v.as_str()).unwrap_or(MCP_VERSION);
                let _ = out_tx.send(
                    json!({"jsonrpc": "2.0", "id": id, "result": {
                        "protocolVersion": ver,
                        "capabilities": {"tools": {}},
                        "serverInfo": {"name": "mantra", "version": env!("CARGO_PKG_VERSION")},
                    }})
                    .to_string(),
                );
            }
            "ping" => {
                let _ = out_tx.send(json!({"jsonrpc": "2.0", "id": id, "result": {}}).to_string());
            }
            "tools/list" | "tools/call" => {
                let call = next.fetch_add(1, Ordering::SeqCst).to_string();
                let msg = if method == "tools/list" {
                    json!({"agent": agent, "call": call, "list": true})
                } else {
                    json!({
                        "agent": agent,
                        "call": call,
                        "tool": params.get("name").cloned().unwrap_or(Value::Null),
                        "args": params.get("arguments").cloned().unwrap_or(json!({})),
                    })
                };
                let (tx, rx) = oneshot::channel();
                if let Ok(mut p) = pending.lock() {
                    p.insert(call.clone(), tx);
                }
                if sock_tx.send(msg.to_string()).is_err() {
                    return Err("mantra disconnected".into());
                }
                let out = out_tx.clone();
                let is_list = method == "tools/list";
                tokio::spawn(async move {
                    let reply = rx.await.unwrap_or_else(|_| json!({"ok": false, "text": "mantra disconnected"}));
                    let result = if is_list {
                        json!({"tools": reply.get("tools").cloned().unwrap_or_else(|| json!([]))})
                    } else {
                        let ok = reply.get("ok").and_then(|o| o.as_bool()).unwrap_or(false);
                        let text = reply.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string();
                        json!({"content": [{"type": "text", "text": text}], "isError": !ok})
                    };
                    let _ = out.send(json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string());
                });
            }
            _ => {
                let _ = out_tx.send(
                    json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": format!("method not found: {method}")}}).to_string(),
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_args() {
        let a = |s: &str| s.split(' ').map(String::from).collect::<Vec<_>>();
        assert_eq!(parse(&a("--sock /tmp/x.sock --agent 7")).unwrap(), ("/tmp/x.sock".into(), 7));
        assert!(parse(&a("--sock /tmp/x.sock")).is_err());
        assert!(parse(&a("--agent x --sock s")).is_err());
        assert!(parse(&a("--bogus")).is_err());
    }

    /// Drive the bridge with in-memory pipes: a fake Claude on one side, a fake Mantra on the other.
    #[tokio::test]
    async fn round_trips_list_and_call() {
        let (claude_side, bridge_stdin) = tokio::io::duplex(64 * 1024);
        let (bridge_stdout, claude_read) = tokio::io::duplex(64 * 1024);
        let (mantra_side, bridge_sock) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            let _ = serve(bridge_stdin, bridge_stdout, bridge_sock, 42).await;
        });

        // Fake Mantra: answer hello, list and call.
        let (m_r, mut m_w) = tokio::io::split(mantra_side);
        tokio::spawn(async move {
            let mut lines = BufReader::new(m_r).lines();
            while let Ok(Some(l)) = lines.next_line().await {
                let v: Value = serde_json::from_str(&l).unwrap();
                assert_eq!(v["agent"], 42);
                if v.get("hello").is_some() {
                    continue;
                }
                let call = v["call"].as_str().unwrap();
                let reply = if v.get("list").is_some() {
                    json!({"call": call, "tools": [{"name": "mantra_log", "description": "log", "inputSchema": {"type": "object"}}]})
                } else {
                    assert_eq!(v["tool"], "mantra_log");
                    assert_eq!(v["args"]["text"], "hi");
                    json!({"call": call, "ok": true, "text": "logged"})
                };
                m_w.write_all(format!("{reply}\n").as_bytes()).await.unwrap();
            }
        });

        // Fake Claude: initialize, list, call, then an unknown method.
        let (_c_r, mut c_w) = tokio::io::split(claude_side);
        let mut out = BufReader::new(claude_read).lines();
        for req in [
            json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {"protocolVersion": "2025-11-25"}}),
            json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
            json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
            json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "mantra_log", "arguments": {"text": "hi"}}}),
            json!({"jsonrpc": "2.0", "id": 3, "method": "resources/list"}),
        ] {
            c_w.write_all(format!("{req}\n").as_bytes()).await.unwrap();
        }
        let mut got: HashMap<i64, Value> = HashMap::new();
        while got.len() < 4 {
            let l = tokio::time::timeout(std::time::Duration::from_secs(5), out.next_line()).await.expect("bridge answered").unwrap().unwrap();
            let v: Value = serde_json::from_str(&l).unwrap();
            got.insert(v["id"].as_i64().unwrap(), v);
        }
        assert_eq!(got[&0]["result"]["protocolVersion"], "2025-11-25");
        assert_eq!(got[&0]["result"]["serverInfo"]["name"], "mantra");
        assert_eq!(got[&1]["result"]["tools"][0]["name"], "mantra_log");
        assert_eq!(got[&2]["result"]["content"][0]["text"], "logged");
        assert_eq!(got[&2]["result"]["isError"], false);
        assert_eq!(got[&3]["error"]["code"], -32601);
    }
}
