//! `mantra mcp-bridge --sock <path> --agent <id>` (WP10.4).
//!
//! `claude` spawns this as an ordinary stdio MCP server per `--mcp-config` (`hub::claude::
//! mcp_config_json`). It speaks real MCP on stdin/stdout to `claude`, and a tiny line-delimited
//! JSON protocol over a Unix socket to the running Mantra process: it connects to `--sock`, sends
//! `{"agent":<id>,"hello":true}`, then for every `tools/list` sends `{"agent":<id>,"list":true}`
//! and answers with whatever `{"tools":[...]}` comes back, and for every `tools/call` sends
//! `{"agent":<id>,"call":"<n>","tool":"<name>","args":{...}}` and answers with the matching
//! `{"call":"<n>","ok":bool,"text":"..."}` (`hub::claude::run_claude_process` on the other end owns
//! that socket once `Hub`'s accept loop hands the connection to the right agent — see `hub.rs` and
//! `hub/claude.rs`). `claude` only ever has one request in flight on this connection at a time (it
//! awaits each tool call before issuing the next), so this reads one line, blocks for one line back,
//! and repeats — no request/response correlation needed on either side.

use serde_json::{json, Value};
use std::path::PathBuf;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

fn parse_args(args: &[String]) -> Option<(PathBuf, u64)> {
    let get = |flag: &str| args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned();
    Some((PathBuf::from(get("--sock")?), get("--agent")?.parse().ok()?))
}

async fn write_line<W: AsyncWriteExt + Unpin>(w: &mut W, v: &Value) -> std::io::Result<()> {
    w.write_all(v.to_string().as_bytes()).await?;
    w.write_all(b"\n").await?;
    w.flush().await
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
    let mut sock_lines = BufReader::new(rd).lines();
    if write_line(&mut wr, &json!({"agent": agent, "hello": true})).await.is_err() {
        eprintln!("mantra mcp-bridge: hello failed");
        std::process::exit(1);
    }

    let stdin = tokio::io::stdin();
    let mut in_lines = BufReader::new(stdin).lines();
    let mut stdout = tokio::io::stdout();
    let mut call_n: u64 = 0;

    loop {
        let line = match in_lines.next_line().await {
            Ok(Some(l)) => l,
            Ok(None) | Err(_) => break, // claude closed our stdin: it's shutting this server down
        };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(req) = serde_json::from_str::<Value>(&line) else { continue };
        let id = req.get("id").cloned();
        match req.get("method").and_then(|m| m.as_str()).unwrap_or("") {
            "initialize" => {
                let Some(id) = id else { continue };
                let resp = json!({"jsonrpc": "2.0", "id": id, "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "mantra", "version": env!("CARGO_PKG_VERSION")},
                }});
                let _ = write_line(&mut stdout, &resp).await;
            }
            "notifications/initialized" | "notifications/cancelled" => {} // no response for notifications
            "ping" => {
                if let Some(id) = id {
                    let _ = write_line(&mut stdout, &json!({"jsonrpc": "2.0", "id": id, "result": {}})).await;
                }
            }
            "tools/list" => {
                let Some(id) = id else { continue };
                let _ = write_line(&mut wr, &json!({"agent": agent, "list": true})).await;
                let tools = match sock_lines.next_line().await {
                    Ok(Some(l)) => serde_json::from_str::<Value>(&l).ok().and_then(|v| v.get("tools").cloned()).unwrap_or_else(|| json!([])),
                    _ => json!([]), // Mantra's end of the bridge is gone; answer empty rather than hang
                };
                let _ = write_line(&mut stdout, &json!({"jsonrpc": "2.0", "id": id, "result": {"tools": tools}})).await;
            }
            "tools/call" => {
                let Some(id) = id else { continue };
                call_n += 1;
                let call_id = call_n.to_string();
                let name = req.pointer("/params/name").and_then(|n| n.as_str()).unwrap_or("").to_string();
                let arguments = req.pointer("/params/arguments").cloned().unwrap_or_else(|| json!({}));
                let _ = write_line(&mut wr, &json!({"agent": agent, "call": call_id, "tool": name, "args": arguments})).await;
                let (ok, text) = match sock_lines.next_line().await {
                    Ok(Some(l)) => {
                        let v: Value = serde_json::from_str(&l).unwrap_or_else(|_| json!({}));
                        (v.get("ok").and_then(|o| o.as_bool()).unwrap_or(false), v.get("text").and_then(|t| t.as_str()).unwrap_or("").to_string())
                    }
                    _ => (false, "mantra mcp-bridge: lost the connection to Mantra".into()),
                };
                let resp = json!({"jsonrpc": "2.0", "id": id, "result": {"content": [{"type": "text", "text": text}], "isError": !ok}});
                let _ = write_line(&mut stdout, &resp).await;
            }
            _ => {
                // Unknown request: answer an empty result rather than nothing, so a client that
                // insists on a reply (rare — most send only what §10.4 lists) doesn't hang forever.
                if let Some(id) = id {
                    let _ = write_line(&mut stdout, &json!({"jsonrpc": "2.0", "id": id, "result": {}})).await;
                }
            }
        }
    }
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
}
