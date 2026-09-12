//! Newline-delimited JSON-RPC (Codex app-server flavour: no "jsonrpc" field) over a child's stdio.
//!
//! Parsing is deliberately lenient: unknown methods/fields are forwarded untouched so newer
//! Codex versions never crash Mantra.

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot};

#[derive(Debug)]
pub enum Incoming {
    Notification { method: String, params: Value },
    Request { id: Value, method: String, params: Value },
    Closed { stderr_tail: String },
}

#[derive(Debug, Clone)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}

type Pending = Arc<Mutex<HashMap<i64, oneshot::Sender<std::result::Result<Value, RpcError>>>>>;
/// Last few stderr lines, ANSI-stripped, kept around so a crash reason (even one raised before the
/// process has actually exited, e.g. a `thread/start` RPC failure) can show *something* useful.
type TailBuf = Arc<Mutex<VecDeque<String>>>;

#[derive(Clone)]
pub struct Conn {
    out: mpsc::UnboundedSender<String>,
    pending: Pending,
    next: Arc<AtomicI64>,
    stderr_tail: TailBuf,
}

impl Conn {
    pub async fn request(&self, method: &str, params: Value) -> std::result::Result<Value, RpcError> {
        self.request_timeout(method, params, Duration::from_secs(90)).await
    }

    pub async fn request_timeout(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> std::result::Result<Value, RpcError> {
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        if let Ok(mut p) = self.pending.lock() {
            p.insert(id, tx);
        }
        let msg = json!({ "method": method, "id": id, "params": params });
        if self.out.send(msg.to_string()).is_err() {
            return Err(RpcError { code: -1, message: "connection closed".into() });
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err(RpcError { code: -1, message: "connection closed".into() }),
            Err(_) => {
                if let Ok(mut p) = self.pending.lock() {
                    p.remove(&id);
                }
                Err(RpcError { code: -2, message: format!("{method} timed out") })
            }
        }
    }

    pub fn notify(&self, method: &str, params: Value) {
        let _ = self.out.send(json!({ "method": method, "params": params }).to_string());
    }

    pub fn respond(&self, id: Value, result: Value) {
        let _ = self.out.send(json!({ "id": id, "result": result }).to_string());
    }

    pub fn respond_err(&self, id: Value, code: i64, message: &str) {
        let _ = self.out.send(json!({ "id": id, "error": { "code": code, "message": message } }).to_string());
    }

    /// The last few stderr lines seen so far, newline-joined. Usable even before the process exits
    /// (e.g. to explain a `thread/start` RPC failure while the process is still alive).
    pub fn stderr_tail(&self) -> String {
        self.stderr_tail.lock().map(|t| t.iter().cloned().collect::<Vec<_>>().join("\n")).unwrap_or_default()
    }
}

/// Spawn `cmd` and wire up reader/writer tasks.
pub fn spawn(cmd: &[String], extra_args: &[String], cwd: &std::path::Path) -> Result<(Conn, mpsc::UnboundedReceiver<Incoming>, Child)> {
    let (prog, args) = cmd.split_first().ok_or_else(|| anyhow!("empty codex command"))?;
    let mut c = Command::new(prog);
    c.args(args)
        .args(extra_args)
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = c.spawn().map_err(|e| anyhow!("failed to start `{}`: {e}", cmd.join(" ")))?;
    let stdin = child.stdin.take().ok_or_else(|| anyhow!("no stdin"))?;
    let stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
    let stderr = child.stderr.take().ok_or_else(|| anyhow!("no stderr"))?;

    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
    let (in_tx, in_rx) = mpsc::unbounded_channel::<Incoming>();
    let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
    let stderr_tail: TailBuf = Arc::new(Mutex::new(VecDeque::new()));
    const TAIL_LINES: usize = 5;

    // writer
    tokio::spawn(async move {
        let mut stdin = stdin;
        while let Some(line) = out_rx.recv().await {
            if stdin.write_all(line.as_bytes()).await.is_err() || stdin.write_all(b"\n").await.is_err() {
                break;
            }
            let _ = stdin.flush().await;
        }
    });

    // stderr → tail buffer (ANSI-stripped, last N lines) + log (raw, for on-disk debugging)
    {
        let tail = stderr_tail.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            // Codex can emit the same diagnostic thousands of times per turn (`OutputTextDelta
            // without active item`); collapse repeats and drop known noise so the log stays useful.
            let mut last: Option<String> = None;
            let mut repeats: u32 = 0;
            let mut dropped: u32 = 0;
            while let Ok(Some(l)) = lines.next_line().await {
                let clean = crate::util::strip_ansi(&l);
                if is_stderr_noise(&clean) {
                    dropped += 1;
                    continue;
                }
                if last.as_deref() == Some(clean.as_str()) {
                    repeats += 1;
                    continue;
                }
                if repeats > 0 {
                    crate::mlog!("[codex stderr] … previous line repeated {repeats}×");
                    repeats = 0;
                }
                crate::mlog!("[codex stderr] {}", crate::util::trunc(&clean, 400));
                last = Some(clean.clone());
                if !clean.trim().is_empty() {
                    if let Ok(mut t) = tail.lock() {
                        t.push_back(clean);
                        while t.len() > TAIL_LINES {
                            t.pop_front();
                        }
                    }
                }
            }
            if repeats > 0 {
                crate::mlog!("[codex stderr] … previous line repeated {repeats}×");
            }
            if dropped > 0 {
                crate::mlog!("[codex stderr] {dropped} known-noise line(s) dropped");
            }
        });
    }

    // reader
    {
        let pending = pending.clone();
        let tail = stderr_tail.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::with_capacity(1 << 16, stdout).lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        let line = line.trim();
                        if line.is_empty() {
                            continue;
                        }
                        let v: Value = match serde_json::from_str(line) {
                            Ok(v) => v,
                            Err(e) => {
                                crate::mlog!("bad json from codex: {e}: {}", crate::util::trunc(line, 200));
                                continue;
                            }
                        };
                        route(v, &pending, &in_tx);
                    }
                    Ok(None) | Err(_) => break,
                }
            }
            // fail all pending requests
            if let Ok(mut p) = pending.lock() {
                for (_, tx) in p.drain() {
                    let _ = tx.send(Err(RpcError { code: -1, message: "codex process exited".into() }));
                }
            }
            let t = tail.lock().map(|t| t.iter().cloned().collect::<Vec<_>>().join("\n")).unwrap_or_default();
            let _ = in_tx.send(Incoming::Closed { stderr_tail: t });
        });
    }

    Ok((Conn { out: out_tx, pending, next: Arc::new(AtomicI64::new(1)), stderr_tail }, in_rx, child))
}

fn route(v: Value, pending: &Pending, in_tx: &mpsc::UnboundedSender<Incoming>) {
    let method = v.get("method").and_then(|m| m.as_str()).map(|s| s.to_string());
    let id = v.get("id").cloned();
    match (method, id) {
        (Some(method), Some(id)) if !id.is_null() => {
            let params = v.get("params").cloned().unwrap_or(Value::Null);
            let _ = in_tx.send(Incoming::Request { id, method, params });
        }
        (Some(method), _) => {
            let params = v.get("params").cloned().unwrap_or(Value::Null);
            let _ = in_tx.send(Incoming::Notification { method, params });
        }
        (None, Some(id)) => {
            let Some(id) = id.as_i64() else { return };
            let tx = pending.lock().ok().and_then(|mut p| p.remove(&id));
            if let Some(tx) = tx {
                if let Some(err) = v.get("error") {
                    let _ = tx.send(Err(RpcError {
                        code: err.get("code").and_then(|c| c.as_i64()).unwrap_or(0),
                        message: err.get("message").and_then(|m| m.as_str()).unwrap_or("error").to_string(),
                    }));
                } else {
                    let _ = tx.send(Ok(v.get("result").cloned().unwrap_or(Value::Null)));
                }
            }
        }
        _ => {}
    }
}

/// `initialize` + `initialized` handshake.
pub async fn handshake(conn: &Conn) -> std::result::Result<Value, RpcError> {
    let r = conn
        .request_timeout(
            "initialize",
            json!({
                "clientInfo": { "name": "mantra", "title": "Mantra", "version": env!("CARGO_PKG_VERSION") },
                "capabilities": {
                    "experimentalApi": true,
                    "requestAttestation": false,
                    "optOutNotificationMethods": [
                        "rawResponseItem/completed", "rawResponse/completed",
                        "item/reasoning/textDelta", "item/fileChange/outputDelta"
                    ]
                }
            }),
            Duration::from_secs(30),
        )
        .await?;
    conn.notify("initialized", json!({}));
    Ok(r)
}

/// Codex diagnostics that carry no information for Mantra users and repeat in bulk.
fn is_stderr_noise(line: &str) -> bool {
    const NOISE: &[&str] = &[
        "OutputTextDelta without active item",
        "unsupported call: multi_agent_v1",
        "cannot update goal because this thread has no goal",
        "resources/read failed for `codex_apps`",
    ];
    NOISE.iter().any(|n| line.contains(n))
}

#[cfg(test)]
mod stderr_tests {
    #[test]
    fn noise_filter() {
        assert!(super::is_stderr_noise("2026-09-11T21:35:40Z ERROR codex_core::util: OutputTextDelta without active item"));
        assert!(super::is_stderr_noise("ERROR codex_core::tools::router: error=unsupported call: multi_agent_v1"));
        assert!(!super::is_stderr_noise("ERROR codex_app_server: Codex's Linux sandbox uses bubblewrap and needs access to create user namespaces."));
    }
}
