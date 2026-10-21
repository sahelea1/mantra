//! End to end: the real `bija` binary against a scripted mock Chat Completions server.
//!
//! Every test starts its own mock on 127.0.0.1 (plain HTTP, the provider's real streaming shape:
//! content and tool-call deltas split across chunks, a usage chunk, `[DONE]`), lets a closure decide
//! each answer from the request's messages, and records every request so the test can check what
//! bija actually sent. bija runs with `--home` in a fresh temp dir, with every proxy and `BIJA_*`
//! variable removed from its environment, and with `--no-bwrap` — except in the sandbox tests,
//! which run the real bubblewrap and skip, saying why, where it is missing or cannot work.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const BIJA: &str = env!("CARGO_BIN_EXE_bija");
const WAIT: Duration = Duration::from_secs(20);
const RESTART_RESULT: &str = "interrupted: the process was restarted before this tool finished";

// ---------------------------------------------------------------------------------------------
// Scratch dirs

/// `<tmp>/bija-e2e-<pid>-<n>/` with a `work/` inside; removed when dropped. Short on purpose: the
/// control socket's path must fit in `sun_path`.
struct Scratch {
    home: PathBuf,
    work: PathBuf,
}

impl Scratch {
    fn new() -> Scratch {
        let home = Scratch::fresh(&std::env::temp_dir());
        let work = home.join("work");
        std::fs::create_dir_all(&work).unwrap();
        Scratch { home, work }
    }

    /// A scratch whose `work/` lies outside `/tmp` (which the sandbox always leaves writable), so a
    /// test can tell the workspace's own bind from the scratch dirs'.
    fn outside_tmp() -> Scratch {
        let home = Scratch::fresh(&std::env::temp_dir());
        let work = Scratch::fresh(Path::new(env!("CARGO_TARGET_TMPDIR")));
        for dir in [&home, &work] {
            std::fs::create_dir_all(dir).unwrap();
        }
        Scratch { home, work }
    }

    fn fresh(parent: &Path) -> PathBuf {
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = parent.join(format!("bija-e2e-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst)));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// The flags every serve/run invocation here shares.
    fn args(&self, mock: &Mock, extra: &[&str]) -> Vec<String> {
        let mut a = self.sandboxed_args(mock, extra);
        a.push("--no-bwrap".into());
        a
    }

    /// The same, leaving bwrap to bija's own detection.
    fn sandboxed_args(&self, mock: &Mock, extra: &[&str]) -> Vec<String> {
        let mut a: Vec<String> = ["--home", path(&self.home), "--provider-url", &mock.url(), "--model", "mock-model", "--cwd", path(&self.work), "--api-key-env", "BIJA_TEST_KEY"].iter().map(|s| s.to_string()).collect();
        a.extend(extra.iter().map(|s| s.to_string()));
        a
    }

    fn transcript(&self, id: &str) -> Vec<Value> {
        let text = std::fs::read_to_string(self.home.join("sessions").join(id).join("transcript.jsonl")).unwrap();
        text.lines().map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("transcript line {l:?}: {e}"))).collect()
    }

    fn meta(&self, id: &str) -> Value {
        serde_json::from_str(&std::fs::read_to_string(self.home.join("sessions").join(id).join("session.json")).unwrap()).unwrap()
    }

    fn tools_file(&self) -> PathBuf {
        let p = self.home.join("tools.json");
        let tools = json!([{
            "type": "function",
            "name": "mantra_ping",
            "description": "Ping the orchestrator.",
            "inputSchema": {"type": "object", "properties": {"word": {"type": "string"}}, "required": ["word"]}
        }]);
        std::fs::write(&p, tools.to_string()).unwrap();
        p
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.home);
        let _ = std::fs::remove_dir_all(&self.work);
    }
}

fn path(p: &Path) -> &str {
    p.to_str().unwrap()
}

// ---------------------------------------------------------------------------------------------
// The mock provider

enum Reply {
    /// An SSE stream of these chunk objects, then `[DONE]`.
    Stream(Vec<Value>),
    /// The start of a stream: these chunks, then the connection closes mid-body (no `[DONE]`, no
    /// final chunk) — a stream that broke off.
    Cut(Vec<Value>),
    /// A JSON error body with this status.
    Status { code: u16, body: Value, retry_after: Option<u64> },
}

#[derive(Clone, Debug)]
struct Req {
    /// Request line and headers, as received.
    head: String,
    body: Value,
}

type Script = dyn Fn(&Value, usize) -> Reply + Send + Sync;

struct Mock {
    port: u16,
    requests: Arc<Mutex<Vec<Req>>>,
}

impl Mock {
    /// Serves until the test process ends; `script(body, index)` answers request number `index`.
    fn start(script: impl Fn(&Value, usize) -> Reply + Send + Sync + 'static) -> Mock {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests: Arc<Mutex<Vec<Req>>> = Arc::default();
        let script: Arc<Script> = Arc::new(script);
        let reqs = requests.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (script, reqs) = (script.clone(), reqs.clone());
                std::thread::spawn(move || answer(stream, &*script, &reqs));
            }
        });
        Mock { port, requests }
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}/v1", self.port)
    }

    fn requests(&self) -> Vec<Req> {
        self.requests.lock().unwrap().clone()
    }
}

/// One request per connection, answered with `Connection: close` — no keep-alive state to get wrong.
fn answer(stream: TcpStream, script: &Script, reqs: &Mutex<Vec<Req>>) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut head = String::new();
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                content_length = v.trim().parse().unwrap_or(0);
            }
        }
        head.push_str(&line);
    }
    let mut body = vec![0u8; content_length];
    if reader.read_exact(&mut body).is_err() {
        return;
    }
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let index = {
        let mut r = reqs.lock().unwrap();
        r.push(Req { head, body: body.clone() });
        r.len() - 1
    };
    let mut stream = stream;
    match script(&body, index) {
        Reply::Stream(chunks) => {
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n");
            for c in chunks {
                write_chunk(&mut stream, &format!("data: {c}\n\n"));
            }
            write_chunk(&mut stream, "data: [DONE]\n\n");
            let _ = stream.write_all(b"0\r\n\r\n");
        }
        Reply::Cut(chunks) => {
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n");
            for c in chunks {
                write_chunk(&mut stream, &format!("data: {c}\n\n"));
            }
        }
        Reply::Status { code, body, retry_after } => {
            let body = body.to_string();
            let retry = retry_after.map(|s| format!("Retry-After: {s}\r\n")).unwrap_or_default();
            let _ = stream.write_all(format!("HTTP/1.1 {code} Mock\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{retry}Connection: close\r\n\r\n{body}", body.len()).as_bytes());
        }
    }
    let _ = stream.flush();
    let _ = stream.shutdown(Shutdown::Both);
}

fn write_chunk(stream: &mut TcpStream, data: &str) {
    let _ = stream.write_all(format!("{:x}\r\n{data}\r\n", data.len()).as_bytes());
}

fn usage_chunk(prompt_tokens: u64) -> Value {
    json!({"id": "c", "object": "chat.completion.chunk", "choices": [], "usage": {"prompt_tokens": prompt_tokens, "completion_tokens": 7, "total_tokens": prompt_tokens + 7, "prompt_tokens_details": {"cached_tokens": 0}}})
}

fn chunk(delta: Value, finish: Value) -> Value {
    json!({"id": "c", "object": "chat.completion.chunk", "model": "mock-model", "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]})
}

/// A final text answer, streamed in two pieces.
fn text_reply(text: &str, prompt_tokens: u64) -> Reply {
    let mid = text.char_indices().nth(text.chars().count() / 2).map(|(i, _)| i).unwrap_or(0);
    Reply::Stream(vec![
        chunk(json!({"role": "assistant", "content": ""}), Value::Null),
        chunk(json!({"content": &text[..mid]}), Value::Null),
        chunk(json!({"content": &text[mid..]}), Value::Null),
        chunk(json!({}), json!("stop")),
        usage_chunk(prompt_tokens),
    ])
}

/// A final text answer from a provider that reports no usage at all.
fn text_reply_without_usage(text: &str) -> Reply {
    Reply::Stream(vec![chunk(json!({"role": "assistant", "content": text}), Value::Null), chunk(json!({}), json!("stop"))])
}

/// Tool calls, each announced with id and name and its arguments split across two chunks.
fn tool_reply(calls: &[(&str, &str, Value)], prompt_tokens: u64) -> Reply {
    let mut chunks = vec![chunk(json!({"role": "assistant", "content": null}), Value::Null)];
    for (i, (id, name, args)) in calls.iter().enumerate() {
        let args = args.to_string();
        let (a, b) = args.split_at(args.len() / 2);
        chunks.push(chunk(json!({"tool_calls": [{"index": i, "id": id, "type": "function", "function": {"name": name, "arguments": ""}}]}), Value::Null));
        chunks.push(chunk(json!({"tool_calls": [{"index": i, "function": {"arguments": a}}]}), Value::Null));
        chunks.push(chunk(json!({"tool_calls": [{"index": i, "function": {"arguments": b}}]}), Value::Null));
    }
    chunks.push(chunk(json!({}), json!("tool_calls")));
    chunks.push(usage_chunk(prompt_tokens));
    Reply::Stream(chunks)
}

fn messages(req: &Value) -> Vec<Value> {
    req["messages"].as_array().cloned().unwrap_or_default()
}

/// A message's text, whether `content` is a string or a list of parts.
fn content(m: &Value) -> String {
    match &m["content"] {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join(""),
        _ => String::new(),
    }
}

fn tool_results(req: &Value) -> Vec<Value> {
    messages(req).into_iter().filter(|m| m["role"] == "tool").collect()
}

fn is_compaction(req: &Value) -> bool {
    messages(req).last().is_some_and(|m| m["role"] == "user" && content(m).contains("hand-off summary"))
}

fn tool_names(req: &Value) -> Vec<String> {
    req["tools"].as_array().map(|ts| ts.iter().filter_map(|t| t["function"]["name"].as_str().map(String::from)).collect()).unwrap_or_default()
}

// ---------------------------------------------------------------------------------------------
// The binary

/// A serving bija: ops in through stdin, events collected from stdout.
struct Bija {
    child: Child,
    stdin: Option<ChildStdin>,
    events: Receiver<Value>,
    seen: Vec<Value>,
    stderr: Arc<Mutex<String>>,
}

fn clean_env(cmd: &mut Command) {
    for k in ["HTTPS_PROXY", "HTTP_PROXY", "ALL_PROXY", "https_proxy", "http_proxy", "all_proxy", "NO_PROXY", "no_proxy", "BIJA_HOME", "BIJA_MODEL", "BIJA_PROVIDER_URL", "BIJA_API_KEY"] {
        cmd.env_remove(k);
    }
    cmd.env("BIJA_TEST_KEY", "test-key");
}

impl Bija {
    fn spawn(args: &[String]) -> Bija {
        let mut cmd = Command::new(BIJA);
        cmd.args(args).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        clean_env(&mut cmd);
        let mut child = cmd.spawn().unwrap();
        let stdout = child.stdout.take().unwrap();
        let (tx, events) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let ev = serde_json::from_str(&line).unwrap_or_else(|_| json!({"ev": "__not_json__", "line": line}));
                if tx.send(ev).is_err() {
                    break;
                }
            }
        });
        let stderr = Arc::new(Mutex::new(String::new()));
        let (err_pipe, sink) = (child.stderr.take().unwrap(), stderr.clone());
        std::thread::spawn(move || {
            for line in BufReader::new(err_pipe).lines().map_while(Result::ok) {
                let mut s = sink.lock().unwrap();
                s.push_str(&line);
                s.push('\n');
            }
        });
        let stdin = child.stdin.take();
        Bija { child, stdin, events, seen: Vec::new(), stderr }
    }

    fn send(&mut self, op: Value) {
        self.send_line(&op.to_string());
    }

    fn send_line(&mut self, line: &str) {
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{line}").unwrap();
        stdin.flush().unwrap();
    }

    fn dump(&self) -> String {
        let evs: Vec<String> = self.seen.iter().map(|e| e.to_string()).collect();
        format!("events:\n{}\nstderr:\n{}", evs.join("\n"), self.stderr.lock().unwrap())
    }

    /// The next event matching `pred` (everything before it is kept in `seen`).
    fn wait_for(&mut self, what: &str, pred: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + WAIT;
        loop {
            match self.events.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(ev) => {
                    assert_ne!(ev["ev"], "__not_json__", "stdout must carry NDJSON events only\n{}", self.dump());
                    self.seen.push(ev.clone());
                    if pred(&ev) {
                        return ev;
                    }
                }
                Err(_) => panic!("timed out waiting for {what}\n{}", self.dump()),
            }
        }
    }

    fn ev(&mut self, name: &str) -> Value {
        self.wait_for(name, |e| e["ev"] == name)
    }

    fn seen(&self, name: &str) -> Vec<Value> {
        self.seen.iter().filter(|e| e["ev"] == name).cloned().collect()
    }

    /// `shutdown`, then everything up to the end of stdout; returns the exit code.
    fn shutdown(mut self) -> (Vec<Value>, i32) {
        self.send(json!({"op": "shutdown"}));
        let deadline = Instant::now() + WAIT;
        while let Ok(ev) = self.events.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            self.seen.push(ev);
        }
        let code = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status.code().unwrap_or(-1);
            }
            assert!(Instant::now() < deadline, "bija did not exit after shutdown\n{}", self.dump());
            std::thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(self.seen.last().map(|e| e["ev"].clone()), Some(json!("exit")), "exit is the last line\n{}", self.dump());
        (std::mem::take(&mut self.seen), code)
    }

    /// Sends `sig` (what a `kill` from outside does), then everything up to the end of stdout;
    /// returns the events and the exit code.
    fn signal(mut self, sig: i32) -> (Vec<Value>, i32) {
        // SAFETY: kill(2) on our own child, which has not been reaped yet.
        assert_eq!(unsafe { libc::kill(self.child.id() as i32, sig) }, 0);
        let deadline = Instant::now() + WAIT;
        while let Ok(ev) = self.events.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            self.seen.push(ev);
        }
        let code = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status.code().unwrap_or(-1);
            }
            assert!(Instant::now() < deadline, "bija did not exit after signal {sig}\n{}", self.dump());
            std::thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(self.seen.last().map(|e| e["ev"].clone()), Some(json!("exit")), "exit is the last line\n{}", self.dump());
        (std::mem::take(&mut self.seen), code)
    }

    fn kill9(&mut self) {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
    }
}

impl Drop for Bija {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn kinds(transcript: &[Value]) -> Vec<String> {
    transcript.iter().map(|r| if r["t"] == "msg" { format!("msg:{}", r["role"].as_str().unwrap_or("?")) } else { r["t"].as_str().unwrap_or("?").to_string() }).collect()
}

/// Every process whose command line contains `needle`.
fn processes_with(needle: &str) -> Vec<String> {
    let Ok(dir) = std::fs::read_dir("/proc") else { return Vec::new() };
    dir.flatten()
        .filter(|e| e.file_name().to_string_lossy().chars().all(|c| c.is_ascii_digit()))
        .filter_map(|e| std::fs::read(e.path().join("cmdline")).ok())
        .map(|raw| String::from_utf8_lossy(&raw).replace('\0', " "))
        .filter(|cmd| cmd.contains(needle))
        .collect()
}

/// SIGKILLs every process whose command line contains `needle` (what a killed bija left behind).
fn kill_processes_with(needle: &str) {
    let Ok(dir) = std::fs::read_dir("/proc") else { return };
    for e in dir.flatten() {
        let Ok(pid) = e.file_name().to_string_lossy().parse::<i32>() else { continue };
        let cmd = std::fs::read(e.path().join("cmdline")).map(|raw| String::from_utf8_lossy(&raw).replace('\0', " ")).unwrap_or_default();
        if cmd.contains(needle) && pid != std::process::id() as i32 {
            // SAFETY: kill(2) on a pid read from /proc; at worst it is gone already.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
}

/// The CPU time `pid` itself has used so far (user + system, all threads; not its children's),
/// from `/proc/<pid>/stat`.
fn cpu_time(pid: u32) -> Duration {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    // The fields after the command name, which is in parentheses and may contain anything.
    let fields: Vec<&str> = stat.rsplit_once(')').unwrap().1.split_whitespace().collect();
    let ticks: u64 = fields[11].parse::<u64>().unwrap() + fields[12].parse::<u64>().unwrap();
    // SAFETY: sysconf only reads a system constant.
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as u64;
    Duration::from_millis(ticks * 1000 / hz)
}

/// SPEC §8's bound on what bija may spend while it waits on something else.
const IDLE_CPU: Duration = Duration::from_millis(50);

fn wait_until(what: &str, timeout: Duration, mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !ok() {
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

// ---------------------------------------------------------------------------------------------
// The tests

#[test]
fn a_prompt_runs_a_builtin_tool_and_completes() {
    let mock = Mock::start(|req, _| {
        if tool_results(req).is_empty() {
            tool_reply(&[("call_w", "write_file", json!({"path": "hello.txt", "content": "hi from bija\n"}))], 120)
        } else {
            text_reply("Wrote hello.txt.", 160)
        }
    });
    let s = Scratch::new();
    let mut b = Bija::spawn(&s.args(&mock, &[]));
    let ready = b.ev("ready");
    assert_eq!((ready["resumed"].as_bool(), ready["pending"].as_bool(), ready["model"].as_str(), ready["wire"].as_str()), (Some(false), Some(false), Some("mock-model"), Some("openai_chat")));
    let id = ready["session"].as_str().unwrap().to_string();

    b.send_line("this is not an op");
    assert!(b.ev("error")["message"].as_str().unwrap().starts_with("bad op"));

    b.send(json!({"op": "prompt", "text": "create hello.txt"}));
    assert_eq!(b.ev("turn_started")["turn"], "t1");
    let started = b.ev("tool_started");
    assert_eq!((started["id"].as_str(), started["name"].as_str(), started["args"]["path"].as_str()), (Some("call_w"), Some("write_file"), Some("hello.txt")));
    let done = b.ev("tool_done");
    assert_eq!((done["id"].as_str(), done["ok"].as_bool()), (Some("call_w"), Some(true)));
    assert_eq!(done["changes"][0]["kind"], "add");
    let message = b.ev("message");
    assert_eq!((message["id"].as_str(), message["text"].as_str()), (Some("t1-m2"), Some("Wrote hello.txt.")));
    let turn_done = b.ev("turn_done");
    assert_eq!((turn_done["turn"].as_str(), turn_done["status"].as_str(), &turn_done["error"]), (Some("t1"), Some("completed"), &Value::Null));

    let deltas: String = b.seen("text_delta").iter().filter(|e| e["id"] == "t1-m2").map(|e| e["text"].as_str().unwrap().to_string()).collect();
    assert_eq!(deltas, "Wrote hello.txt.");
    let usage = b.seen("usage");
    assert_eq!(usage.len(), 2, "one usage per model call\n{}", b.dump());
    assert_eq!((usage[1]["last_in"].as_u64(), usage[1]["total"].as_u64(), usage[1]["estimated"].as_bool()), (Some(160), Some(120 + 7 + 160 + 7), Some(false)));
    assert_eq!(std::fs::read_to_string(s.work.join("hello.txt")).unwrap(), "hi from bija\n");

    let reqs = mock.requests();
    assert_eq!(reqs.len(), 2);
    assert!(reqs[0].head.starts_with("POST /v1/chat/completions "), "{}", reqs[0].head);
    assert!(reqs[0].head.to_ascii_lowercase().contains("authorization: bearer test-key"), "{}", reqs[0].head);
    let first = &reqs[0].body;
    assert_eq!((first["model"].as_str(), first["stream"].as_bool()), (Some("mock-model"), Some(true)));
    let msgs = messages(first);
    assert_eq!(msgs[0]["role"], "system");
    assert!(content(&msgs[0]).contains("running in bija"), "{}", content(&msgs[0]));
    assert_eq!((msgs[1]["role"].as_str(), content(&msgs[1]).as_str()), (Some("user"), "create hello.txt"));
    let names = tool_names(first);
    assert!(["read_file", "write_file", "edit_file", "bash", "job"].iter().all(|n| names.iter().any(|m| m == n)), "{names:?}");
    let second = messages(&reqs[1].body);
    assert_eq!(second.len(), 4, "{second:?}");
    assert_eq!(second[2]["tool_calls"][0]["id"], "call_w");
    assert_eq!((second[3]["role"].as_str(), second[3]["tool_call_id"].as_str()), (Some("tool"), Some("call_w")));

    let (_, code) = b.shutdown();
    assert_eq!(code, 0);
    let t = s.transcript(&id);
    assert_eq!(kinds(&t), vec!["msg:user", "msg:assistant", "msg:tool", "msg:assistant", "turn"]);
    assert_eq!(t[1]["tool_calls"][0], json!({"id": "call_w", "name": "write_file", "args": {"path": "hello.txt", "content": "hi from bija\n"}}));
    assert_eq!((t[2]["call_id"].as_str(), t[2]["is_error"].as_bool()), (Some("call_w"), Some(false)));
    assert_eq!(t[3]["text"], "Wrote hello.txt.");
    assert_eq!((t[4]["n"].as_u64(), t[4]["status"].as_str()), (Some(1), Some("completed")));
    assert!(t.iter().all(|r| r["at"].as_u64().is_some_and(|at| at > 0)));
    let meta = s.meta(&id);
    assert_eq!((meta["turns"].as_u64(), meta["last_in"].as_u64(), meta["usage_total"]["input"].as_u64()), (Some(1), Some(160), Some(280)));
}

#[test]
fn an_external_tool_round_trips_over_the_protocol() {
    let mock = Mock::start(|req, _| {
        let results = tool_results(req);
        match results.last() {
            None => tool_reply(&[("call_x", "mantra_ping", json!({"word": "hello"}))], 100),
            Some(r) => text_reply(&format!("The orchestrator said: {}", content(r)), 120),
        }
    });
    let s = Scratch::new();
    let tools = s.tools_file();
    let mut b = Bija::spawn(&s.args(&mock, &["--tools-file", path(&tools)]));
    b.ev("ready");
    b.send(json!({"op": "prompt", "text": "ping the orchestrator"}));
    let call = b.ev("tool_call");
    assert_eq!((call["id"].as_str(), call["name"].as_str(), &call["args"]), (Some("call_x"), Some("mantra_ping"), &json!({"word": "hello"})));
    assert!(b.seen("tool_started").is_empty(), "an external call is a tool_call, not a tool_started");

    b.send(json!({"op": "tool_result", "id": "nope", "ok": true, "text": "stray"}));
    assert!(b.ev("warning")["message"].as_str().unwrap().contains("nope"));
    b.send(json!({"op": "status"}));
    let status = b.ev("status");
    assert_eq!((status["busy"].as_bool(), status["turn"].as_str()), (Some(true), Some("t1")), "ops are answered while the call waits");

    b.send(json!({"op": "tool_result", "id": "call_x", "ok": true, "text": "pong: hello"}));
    let done = b.ev("tool_done");
    assert_eq!((done["id"].as_str(), done["ok"].as_bool(), done["result"].as_str()), (Some("call_x"), Some(true), Some("pong: hello")));
    assert_eq!(b.ev("message")["text"], "The orchestrator said: pong: hello");
    assert_eq!(b.ev("turn_done")["status"], "completed");

    let reqs = mock.requests();
    let offered = &reqs[0].body["tools"];
    let ping = offered.as_array().unwrap().iter().find(|t| t["function"]["name"] == "mantra_ping").expect("mantra_ping offered");
    assert_eq!(ping["function"]["parameters"]["required"], json!(["word"]));
    assert_eq!(content(&tool_results(&reqs[1].body)[0]), "pong: hello");
    assert_eq!(b.shutdown().1, 0);
}

/// A model that sends a nested array as a JSON string (a common quirk): the external tool gets the
/// array its schema asks for, while the transcript and the history sent back keep what was sent.
#[test]
fn an_external_tool_gets_arguments_in_its_schemas_types() {
    let stringified = r#"[{"name": "A", "tasks": [], "gate": {"checks": ["true"]}}]"#;
    let mock = Mock::start(move |req, _| match tool_results(req).last() {
        None => tool_reply(&[("call_p", "mantra_submit_plan", json!({"plan": {"title": "T", "phases": stringified}}))], 100),
        Some(r) => text_reply(&format!("Mantra said: {}", content(r)), 120),
    });
    let s = Scratch::new();
    let tools = s.home.join("plan-tools.json");
    let schema = json!({"type": "object", "properties": {"plan": {"type": "object", "properties": {
        "title": {"type": "string"},
        "phases": {"type": "array", "items": {"type": "object", "properties": {"name": {"type": "string"}, "tasks": {"type": "array"}}}}
    }}}, "required": ["plan"]});
    std::fs::write(&tools, json!([{"type": "function", "name": "mantra_submit_plan", "description": "Submit the plan.", "inputSchema": schema}]).to_string()).unwrap();
    let mut b = Bija::spawn(&s.args(&mock, &["--tools-file", path(&tools)]));
    let id = b.ev("ready")["session"].as_str().unwrap().to_string();
    b.send(json!({"op": "prompt", "text": "plan it"}));
    let call = b.ev("tool_call");
    let phases = json!([{"name": "A", "tasks": [], "gate": {"checks": ["true"]}}]);
    assert_eq!((call["id"].as_str(), &call["args"]), (Some("call_p"), &json!({"plan": {"title": "T", "phases": phases}})), "{}", b.dump());
    b.send(json!({"op": "tool_result", "id": "call_p", "ok": true, "text": "plan accepted"}));
    assert_eq!(b.ev("turn_done")["status"], "completed");
    assert_eq!(b.shutdown().1, 0);

    let reqs = mock.requests();
    let sent: Value = serde_json::from_str(messages(&reqs[1].body)[2]["tool_calls"][0]["function"]["arguments"].as_str().unwrap()).unwrap();
    assert_eq!(sent["plan"]["phases"], stringified, "the history sent back is what the model wrote");
    let t = s.transcript(&id);
    assert_eq!(t[1]["tool_calls"][0]["args"]["plan"]["phases"], stringified, "so is the transcript");
}

#[test]
fn a_killed_session_resumes_its_open_turn() {
    let mock = Mock::start(|req, _| {
        let results = tool_results(req);
        match results.last() {
            None => tool_reply(&[("call_k", "mantra_ping", json!({"word": "x"}))], 100),
            Some(r) => text_reply(&format!("Recovered after: {}", content(r)), 120),
        }
    });
    let s = Scratch::new();
    let tools = s.tools_file();
    let mut a = Bija::spawn(&s.args(&mock, &["--tools-file", path(&tools)]));
    let id = a.ev("ready")["session"].as_str().unwrap().to_string();
    a.send(json!({"op": "prompt", "text": "start something"}));
    a.ev("tool_call");
    a.kill9();
    assert_eq!(kinds(&s.transcript(&id)), vec!["msg:user", "msg:assistant"], "the crash left the call open");

    // Same flags (the tools file comes back from meta), no prompt: the turn carries on by itself.
    let mut b = Bija::spawn(&s.args(&mock, &["--resume", &id]));
    let ready = b.ev("ready");
    assert_eq!((ready["resumed"].as_bool(), ready["pending"].as_bool(), ready["session"].as_str()), (Some(true), Some(true), Some(id.as_str())));
    assert_eq!(b.ev("turn_started")["turn"], "t1");
    let message = b.ev("message");
    assert_eq!(message["id"], "t1-m2", "the resumed turn numbers past the message emitted before the crash");
    assert!(message["text"].as_str().unwrap().contains(RESTART_RESULT));
    assert_eq!(b.ev("turn_done")["status"], "completed");

    let reqs = mock.requests();
    assert_eq!(reqs.len(), 2);
    let results = tool_results(&reqs[1].body);
    assert_eq!((results[0]["tool_call_id"].as_str(), content(&results[0]).as_str()), (Some("call_k"), RESTART_RESULT));
    assert_eq!(b.shutdown().1, 0);
    let t = s.transcript(&id);
    assert_eq!(kinds(&t), vec!["msg:user", "msg:assistant", "msg:tool", "msg:assistant", "turn"]);
    assert_eq!((t[2]["text"].as_str(), t[2]["is_error"].as_bool()), (Some(RESTART_RESULT), Some(true)));
}

/// A stream that broke after some text is retried; that text was not the reply. The retry streams
/// under a fresh id, the abandoned one never gets a `message`, and neither the transcript nor the
/// next request carries the partial text.
#[test]
fn a_retry_after_streamed_text_starts_a_fresh_message() {
    let mock = Mock::start(|_, i| match i {
        0 => Reply::Cut(vec![chunk(json!({"role": "assistant", "content": ""}), Value::Null), chunk(json!({"content": "Let me look"}), Value::Null)]),
        1 => tool_reply(&[("call_r", "bash", json!({"command": "echo retried"}))], 90),
        _ => text_reply("All done.", 120),
    });
    let s = Scratch::new();
    let mut b = Bija::spawn(&s.args(&mock, &[]));
    let id = b.ev("ready")["session"].as_str().unwrap().to_string();
    b.send(json!({"op": "prompt", "text": "look around"}));
    let partial = b.ev("text_delta");
    assert_eq!((partial["id"].as_str(), partial["text"].as_str()), (Some("t1-m1"), Some("Let me look")));
    let retry = b.ev("retry");
    assert_eq!((retry["turn"].as_str(), retry["attempt"].as_u64()), (Some("t1"), Some(1)));
    assert_eq!(b.ev("tool_done")["id"], "call_r");
    let message = b.ev("message");
    assert_eq!((message["id"].as_str(), message["text"].as_str()), (Some("t1-m3"), Some("All done.")), "m1 was given up, m2 is the tool call\n{}", b.dump());
    assert_eq!(b.ev("turn_done")["status"], "completed");

    assert_eq!(b.seen("message").len(), 1, "no message for the abandoned text\n{}", b.dump());
    let ids: Vec<&str> = b.seen.iter().filter(|e| e["ev"] == "text_delta").map(|e| e["id"].as_str().unwrap_or("")).collect();
    assert_eq!(ids.first(), Some(&"t1-m1"));
    assert!(ids[1..].iter().all(|i| *i == "t1-m3"), "after the retry, deltas only for the final message: {ids:?}");
    let final_text: String = b.seen("text_delta").iter().filter(|e| e["id"] == "t1-m3").map(|e| e["text"].as_str().unwrap().to_string()).collect();
    assert_eq!(final_text, "All done.");

    let reqs = mock.requests();
    assert_eq!(reqs.len(), 3);
    assert_eq!(messages(&reqs[1].body), messages(&reqs[0].body), "the retry resends the same history");
    assert!(!reqs[2].body.to_string().contains("Let me look"), "the partial text is not history");
    assert_eq!(b.shutdown().1, 0);
    let t = s.transcript(&id);
    assert_eq!(kinds(&t), vec!["msg:user", "msg:assistant", "msg:tool", "msg:assistant", "turn"]);
    assert_eq!((t[1]["text"].as_str(), t[1]["tool_calls"][0]["id"].as_str()), (Some(""), Some("call_r")), "the tool-call reply carries none of the cut text");
    assert_eq!(t[3]["text"], "All done.");
}

#[test]
fn transient_errors_are_retried_with_retry_events() {
    let mock = Mock::start(|_, i| {
        if i < 2 {
            Reply::Status { code: 503, body: json!({"error": {"message": "upstream overloaded, try again", "type": "server_error"}}), retry_after: Some(1) }
        } else {
            text_reply("Third time lucky.", 50)
        }
    });
    let s = Scratch::new();
    let mut b = Bija::spawn(&s.args(&mock, &[]));
    b.ev("ready");
    b.send(json!({"op": "prompt", "text": "hello?"}));
    assert_eq!(b.ev("message")["text"], "Third time lucky.");
    assert_eq!(b.ev("turn_done")["status"], "completed");
    let retries = b.seen("retry");
    assert_eq!(retries.len(), 2, "{}", b.dump());
    assert!(retries[0]["attempt"].as_u64() < retries[1]["attempt"].as_u64());
    assert!(retries.iter().all(|r| r["max"] == 8 && r["turn"] == "t1" && r["error"].as_str().unwrap().contains("503")), "{retries:?}");
    assert_eq!(mock.requests().len(), 3);
    assert_eq!(b.shutdown().1, 0);
}

#[test]
fn an_interrupt_kills_the_running_command_and_ends_the_turn() {
    const MARK: &str = "sleep 23.4567";
    let mock = Mock::start(|req, _| match tool_results(req).is_empty() {
        true => tool_reply(&[("call_s", "bash", json!({"command": MARK}))], 80),
        false => text_reply("this request should never be made", 90),
    });
    let s = Scratch::new();
    let mut b = Bija::spawn(&s.args(&mock, &[]));
    let id = b.ev("ready")["session"].as_str().unwrap().to_string();
    b.send(json!({"op": "prompt", "text": "wait a while"}));
    b.ev("tool_started");
    wait_until("the command runs", Duration::from_secs(10), || !processes_with(MARK).is_empty());

    let asked = Instant::now();
    b.send(json!({"op": "interrupt"}));
    let done = b.ev("turn_done");
    assert!(asked.elapsed() < Duration::from_secs(3), "interrupt took {:?}", asked.elapsed());
    assert_eq!((done["status"].as_str(), &done["error"]), (Some("interrupted"), &Value::Null));
    let tool_done = b.seen("tool_done");
    assert_eq!((tool_done[0]["id"].as_str(), tool_done[0]["ok"].as_bool(), tool_done[0]["result"].as_str()), (Some("call_s"), Some(false), Some("interrupted")));
    wait_until("the command is gone", Duration::from_secs(3), || processes_with(MARK).is_empty());

    b.send(json!({"op": "status"}));
    assert_eq!(b.ev("status")["busy"], false);
    assert_eq!(mock.requests().len(), 1, "an interrupted turn makes no further model call");
    assert_eq!(b.shutdown().1, 0);
    let t = s.transcript(&id);
    assert_eq!(kinds(&t), vec!["msg:user", "msg:assistant", "msg:tool", "note", "turn"]);
    assert_eq!((t[2]["text"].as_str(), t[2]["is_error"].as_bool()), (Some("interrupted"), Some(true)));
    assert_eq!((t[3]["text"].as_str(), t[4]["status"].as_str()), (Some("turn interrupted"), Some("interrupted")));
}

/// Mantra's quit path SIGKILLs bija a few hundred ms after `shutdown`. Whatever bija still had to
/// do then (a job that ignores SIGTERM keeps it in its grace), the transcript is already complete:
/// the interrupted turn is closed and repaired, so a resume has nothing pending.
#[test]
fn a_shutdown_cut_short_by_sigkill_leaves_a_complete_transcript() {
    const JOB: &str = "sleep 31.4159";
    const COMMAND: &str = "sleep 27.1828";
    let mock = Mock::start(|_, i| match i {
        0 => tool_reply(&[("call_job", "bash", json!({"command": format!("trap '' TERM; {JOB}"), "background": true})), ("call_fg", "bash", json!({"command": COMMAND}))], 80),
        _ => text_reply("Nothing was pending.", 90),
    });
    let s = Scratch::new();
    let mut a = Bija::spawn(&s.args(&mock, &[]));
    let id = a.ev("ready")["session"].as_str().unwrap().to_string();
    a.send(json!({"op": "prompt", "text": "start a job, then wait"}));
    assert_eq!(a.ev("tool_done")["id"], "call_job");
    a.wait_for("the foreground command", |e| e["ev"] == "tool_started" && e["id"] == "call_fg");
    wait_until("both commands run", Duration::from_secs(10), || !processes_with(JOB).is_empty() && !processes_with(COMMAND).is_empty());
    a.send(json!({"op": "shutdown"}));
    std::thread::sleep(Duration::from_millis(100));
    a.kill9();
    kill_processes_with(JOB);

    let t = s.transcript(&id);
    assert_eq!(kinds(&t), vec!["msg:user", "msg:assistant", "msg:tool", "msg:tool", "note", "turn"], "{t:?}");
    assert_eq!((t[3]["call_id"].as_str(), t[3]["text"].as_str(), t[3]["is_error"].as_bool()), (Some("call_fg"), Some("interrupted"), Some(true)));
    assert_eq!((t[5]["n"].as_u64(), t[5]["status"].as_str()), (Some(1), Some("interrupted")));
    assert_eq!(s.meta(&id)["turns"], 1);

    let mut b = Bija::spawn(&s.args(&mock, &["--resume", &id]));
    let ready = b.ev("ready");
    assert_eq!((ready["resumed"].as_bool(), ready["pending"].as_bool()), (Some(true), Some(false)), "{}", b.dump());
    assert_eq!(b.shutdown().1, 0);
    assert_eq!(s.transcript(&id).len(), t.len(), "the resume had nothing to repair");
    assert_eq!(mock.requests().len(), 1, "and nothing to continue");
}

/// A SIGTERM from outside (a `kill`, a supervisor, a container stop) is not Mantra's `shutdown`:
/// the process goes, the work does not. Command and job are ended, but the turn is left exactly as
/// a SIGKILL would leave it — no repair, no note, no `turn` record — and the exit says 143, so
/// whoever relaunches with `--resume` gets `pending` and the turn finishes.
#[test]
fn a_sigterm_mid_command_leaves_the_turn_for_the_resume() {
    const JOB: &str = "sleep 41.4213";
    const COMMAND: &str = "sleep 37.3205";
    let mock = Mock::start(|req, _| match tool_results(req).len() {
        0 => tool_reply(&[("call_job", "bash", json!({"command": JOB, "background": true})), ("call_fg", "bash", json!({"command": COMMAND}))], 80),
        _ => text_reply("Carried on after the restart.", 90),
    });
    let s = Scratch::new();
    let mut a = Bija::spawn(&s.args(&mock, &[]));
    let id = a.ev("ready")["session"].as_str().unwrap().to_string();
    a.send(json!({"op": "prompt", "text": "start a job, then wait"}));
    assert_eq!(a.ev("tool_done")["id"], "call_job");
    a.wait_for("the foreground command", |e| e["ev"] == "tool_started" && e["id"] == "call_fg");
    wait_until("both commands run", Duration::from_secs(10), || !processes_with(JOB).is_empty() && !processes_with(COMMAND).is_empty());

    let (events, code) = a.signal(libc::SIGTERM);
    assert_eq!(code, 143);
    assert_eq!(events.last().map(|e| e["code"].clone()), Some(json!(143)));
    assert!(!events.iter().any(|e| e["ev"] == "turn_done"), "the turn did not end: {events:?}");
    assert!(!events.iter().any(|e| e["ev"] == "tool_done" && e["id"] == "call_fg"), "the cut-off call has no result: {events:?}");
    let left = || [JOB, COMMAND].iter().flat_map(|m| processes_with(m)).collect::<Vec<_>>();
    let gone = (0..150).any(|_| left().is_empty() || {
        std::thread::sleep(Duration::from_millis(20));
        false
    });
    assert!(gone, "the command and the job outlived bija: {:?}", left());
    let t = s.transcript(&id);
    assert_eq!(kinds(&t), vec!["msg:user", "msg:assistant", "msg:tool"], "the transcript ends inside the turn: {t:?}");

    let mut b = Bija::spawn(&s.args(&mock, &["--resume", &id]));
    let ready = b.ev("ready");
    assert_eq!((ready["resumed"].as_bool(), ready["pending"].as_bool()), (Some(true), Some(true)), "{}", b.dump());
    assert_eq!(b.ev("turn_started")["turn"], "t1");
    assert_eq!(b.ev("turn_done")["status"], "completed");
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 2);
    let results = tool_results(&reqs[1].body);
    assert_eq!((results[1]["tool_call_id"].as_str(), content(&results[1]).as_str()), (Some("call_fg"), RESTART_RESULT));
    assert_eq!(b.shutdown().1, 0);
    let t = s.transcript(&id);
    assert_eq!(kinds(&t), vec!["msg:user", "msg:assistant", "msg:tool", "msg:tool", "msg:assistant", "turn"]);
    assert_eq!((t[5]["n"].as_u64(), t[5]["status"].as_str()), (Some(1), Some("completed")));
}

/// Between turns a signal has nothing to leave open: it exits 128 + signal, and the resume that
/// follows finds nothing pending.
#[test]
fn a_signal_between_turns_exits_non_zero_with_nothing_pending() {
    let mock = Mock::start(|_, _| text_reply("Done.", 30));
    let s = Scratch::new();
    let mut a = Bija::spawn(&s.args(&mock, &[]));
    let id = a.ev("ready")["session"].as_str().unwrap().to_string();
    a.send(json!({"op": "prompt", "text": "say done"}));
    assert_eq!(a.ev("turn_done")["status"], "completed");
    let (events, code) = a.signal(libc::SIGTERM);
    assert_eq!((code, events.last().map(|e| e["code"].clone())), (143, Some(json!(143))));
    let t = s.transcript(&id);
    assert_eq!(kinds(&t), vec!["msg:user", "msg:assistant", "turn"]);

    // SIGHUP (a closed terminal, a supervisor's reload) is the same stop.
    let mut b = Bija::spawn(&s.args(&mock, &["--resume", &id]));
    let ready = b.ev("ready");
    assert_eq!((ready["resumed"].as_bool(), ready["pending"].as_bool()), (Some(true), Some(false)), "{}", b.dump());
    assert_eq!(b.signal(libc::SIGHUP).1, 129);
    assert_eq!(s.transcript(&id).len(), t.len(), "nothing was written");
    assert_eq!(mock.requests().len(), 1, "and nothing continued");
}

/// SPEC §8: while a command runs, bija is parked in the reactor — no polling while it waits.
#[test]
fn bija_idles_while_a_command_runs() {
    let mock = Mock::start(|req, _| match tool_results(req).is_empty() {
        true => tool_reply(&[("call_sleep", "bash", json!({"command": "sleep 3"}))], 60),
        false => text_reply("Slept.", 70),
    });
    let s = Scratch::new();
    let mut b = Bija::spawn(&s.args(&mock, &[]));
    let pid = b.ev("ready")["pid"].as_u64().unwrap() as u32;
    b.send(json!({"op": "prompt", "text": "sleep a while"}));
    b.ev("tool_started");
    // The command line of the sleep itself (or of the shell that runs it) ends in exactly this.
    wait_until("the sleep runs", Duration::from_secs(5), || processes_with("sleep 3 ").iter().any(|c| c.ends_with("sleep 3 ")));
    let before = cpu_time(pid);
    std::thread::sleep(Duration::from_secs(2));
    let spent = cpu_time(pid) - before;
    assert!(b.seen("tool_done").is_empty(), "the window must lie inside the sleep\n{}", b.dump());
    assert!(spent < IDLE_CPU, "bija used {spent:?} of CPU while its command slept");
    assert_eq!(b.ev("turn_done")["status"], "completed");
    assert_eq!(b.shutdown().1, 0);
}

/// SPEC §8: between turns, bija waits on stdin in the reactor and uses no CPU at all.
#[test]
fn bija_idles_between_turns() {
    let mock = Mock::start(|_, _| text_reply("unused", 10));
    let s = Scratch::new();
    let mut b = Bija::spawn(&s.args(&mock, &[]));
    let pid = b.ev("ready")["pid"].as_u64().unwrap() as u32;
    let before = cpu_time(pid);
    std::thread::sleep(Duration::from_secs(2));
    let spent = cpu_time(pid) - before;
    assert!(spent < IDLE_CPU, "bija used {spent:?} of CPU waiting for a prompt");
    assert_eq!(b.shutdown().1, 0);
    assert!(mock.requests().is_empty());
}

#[test]
fn a_steer_reaches_the_next_model_call() {
    let mock = Mock::start(|_, i| match i {
        0 => tool_reply(&[("call_1", "bash", json!({"command": "sleep 1"}))], 80),
        _ => text_reply_without_usage("Steered."),
    });
    let s = Scratch::new();
    let mut b = Bija::spawn(&s.args(&mock, &[]));
    b.ev("ready");
    b.send(json!({"op": "prompt", "text": "do the thing"}));
    b.ev("tool_started");
    b.send(json!({"op": "steer", "text": "also mention the weather"}));
    assert_eq!(b.ev("turn_done")["status"], "completed");
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 2);
    let msgs = messages(&reqs[1].body);
    let last = msgs.last().unwrap();
    assert_eq!((last["role"].as_str(), content(last).as_str()), (Some("user"), "also mention the weather"));
    assert_eq!(msgs[msgs.len() - 2]["role"], "tool", "the steer comes after the tool result, at the tool boundary");
    assert!(b.seen("message").iter().all(|m| m["text"] != "also mention the weather"), "a steer is not echoed");
    let usage = b.seen("usage");
    assert_eq!((usage[0]["estimated"].as_bool(), usage[0]["last_in"].as_u64()), (Some(false), Some(80)));
    assert_eq!(usage[1]["estimated"], true, "no usage from the provider: estimated from the text");
    assert!(usage[1]["last_in"].as_u64().unwrap() > 0 && usage[1]["total"].as_u64().unwrap() > 87, "{usage:?}");
    assert_eq!(b.shutdown().1, 0);
}

#[test]
fn set_changes_the_model_of_the_next_call() {
    let mock = Mock::start(|_, i| match i {
        0 => tool_reply(&[("call_1", "bash", json!({"command": "sleep 1"}))], 80),
        _ => text_reply("Switched.", 90),
    });
    let s = Scratch::new();
    let mut b = Bija::spawn(&s.args(&mock, &["--effort", "low"]));
    let id = b.ev("ready")["session"].as_str().unwrap().to_string();
    b.send(json!({"op": "prompt", "text": "go"}));
    b.ev("tool_started");
    b.send(json!({"op": "set", "model": "mock-model-2", "effort": "high"}));
    b.send(json!({"op": "prompt", "text": "and then this"}));
    b.send(json!({"op": "status"}));
    let status = b.ev("status");
    assert_eq!((status["model"].as_str(), status["queued"].as_u64()), (Some("mock-model-2"), Some(1)), "set applies at once; the prompt waits");
    assert_eq!(b.ev("turn_done")["turn"], "t1");
    assert_eq!(b.ev("turn_started")["turn"], "t2", "the queued prompt runs next");
    assert_eq!(b.ev("turn_done")["status"], "completed");
    let reqs = mock.requests();
    assert_eq!(reqs.len(), 3);
    assert_eq!(content(messages(&reqs[2].body).last().unwrap()), "and then this");
    assert_eq!((reqs[0].body["model"].as_str(), reqs[0].body["reasoning_effort"].as_str()), (Some("mock-model"), Some("low")));
    assert_eq!((reqs[1].body["model"].as_str(), reqs[1].body["reasoning_effort"].as_str()), (Some("mock-model-2"), Some("high")));
    assert_eq!(b.shutdown().1, 0);
    assert_eq!((s.meta(&id)["model"].as_str(), s.meta(&id)["effort"].as_str()), (Some("mock-model-2"), Some("high")), "persisted in session.json");
}

#[test]
fn a_full_context_is_compacted_mid_turn() {
    let mock = Mock::start(|req, _| {
        let msgs = messages(req);
        let resumed_from_summary = msgs.iter().any(|m| m["role"] == "user" && content(m).starts_with("[context summary"));
        if is_compaction(req) {
            text_reply("SUMMARY: ran echo hi; next, report.", 950)
        } else if resumed_from_summary {
            text_reply("Done after compaction.", 100)
        } else {
            // 900 of a 1000-token window: above the 85 % threshold.
            tool_reply(&[("call_e", "bash", json!({"command": "echo hi"}))], 900)
        }
    });
    let s = Scratch::new();
    let mut b = Bija::spawn(&s.args(&mock, &["--context-window", "1000"]));
    let id = b.ev("ready")["session"].as_str().unwrap().to_string();
    b.send(json!({"op": "prompt", "text": "fill the context"}));
    let compacted = b.ev("compacted");
    assert_eq!(compacted["turn"], "t1");
    assert!(compacted["from"].as_u64().unwrap() >= 900 && compacted["to"].as_u64().unwrap() < 100, "{compacted}");
    assert_eq!(b.ev("message")["text"], "Done after compaction.");
    assert_eq!(b.ev("turn_done")["status"], "completed");

    let reqs = mock.requests();
    assert_eq!(reqs.len(), 3, "call, compaction, call");
    assert!(tool_names(&reqs[1].body).is_empty(), "the compaction call offers no tools");
    let after = messages(&reqs[2].body);
    assert_eq!(after.len(), 2, "system + the summary only: {after:?}");
    assert_eq!(after[0]["role"], "system");
    let summary = content(&after[1]);
    assert!(summary.starts_with("[context summary — the conversation so far was compacted]") && summary.contains("SUMMARY: ran echo hi"), "{summary}");
    assert_eq!(s.meta(&id)["last_in"], 100);

    // Asked for while idle: compacted at once, outside any turn.
    b.send(json!({"op": "compact"}));
    let idle = b.ev("compacted");
    assert!(idle.get("turn").is_none(), "{idle}");
    assert_eq!(mock.requests().len(), 4);
    assert!(is_compaction(&mock.requests()[3].body));
    assert_eq!(b.shutdown().1, 0);
    let t = s.transcript(&id);
    assert_eq!(kinds(&t).iter().filter(|k| *k == "compact").count(), 2, "{t:?}");
}

#[test]
fn run_prints_the_final_text_and_its_exit_code_says_how_it_went() {
    let mock = Mock::start(|_, _| text_reply("Hello from run.", 40));
    let s = Scratch::new();
    let mut cmd = Command::new(BIJA);
    cmd.arg("run").args(s.args(&mock, &[])).arg("say hello");
    clean_env(&mut cmd);
    let out = cmd.output().unwrap();
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "Hello from run.\n");

    let refusing = Mock::start(|_, _| Reply::Status { code: 401, body: json!({"error": {"message": "invalid api key"}}), retry_after: None });
    let mut cmd = Command::new(BIJA);
    cmd.arg("run").args(s.args(&refusing, &[])).arg("say hello");
    clean_env(&mut cmd);
    let out = cmd.output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).starts_with("bija: turn failed"), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(refusing.requests().len(), 1, "an auth error is not retried");

    let mut cmd = Command::new(BIJA);
    cmd.args(["run", "--model", "m", "hi"]).args(["--home", path(&s.home)]);
    clean_env(&mut cmd);
    let out = cmd.output().unwrap();
    assert_eq!(out.status.code(), Some(2), "a missing --provider-url is a usage error");
}

#[test]
fn the_control_socket_takes_prompts_from_bija_send() {
    let mock = Mock::start(|_, _| text_reply("pong over the socket", 30));
    let s = Scratch::new();
    let mut b = Bija::spawn(&s.args(&mock, &["--control", "--name", "socket test"]));
    let ready = b.ev("ready");
    let id = ready["session"].as_str().unwrap().to_string();
    let sock = Path::new(ready["dir"].as_str().unwrap()).join("control.sock");
    assert!(sock.exists());

    let mut cmd = Command::new(BIJA);
    cmd.args(["send", &id, "ping", "--home", path(&s.home)]);
    clean_env(&mut cmd);
    let out = cmd.output().unwrap();
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let printed: Vec<Value> = String::from_utf8_lossy(&out.stdout).lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert!(printed.iter().any(|e| e["ev"] == "message" && e["text"] == "pong over the socket"), "{printed:?}");
    assert_eq!(printed.last().map(|e| e["ev"].clone()), Some(json!("turn_done")), "send stops at its turn's turn_done");
    assert_eq!(b.ev("turn_done")["status"], "completed", "stdout sees the same turn");

    let mut cmd = Command::new(BIJA);
    cmd.args(["send", &id, "--home", path(&s.home), "--json", r#"{"op":"status"}"#]);
    clean_env(&mut cmd);
    let out = cmd.output().unwrap();
    let status: Value = serde_json::from_str(String::from_utf8_lossy(&out.stdout).trim()).unwrap();
    assert_eq!((status["ev"].as_str(), status["turns"].as_u64()), (Some("status"), Some(1)));

    let mut cmd = Command::new(BIJA);
    cmd.args(["sessions", "--home", path(&s.home)]);
    let listing = String::from_utf8_lossy(&cmd.output().unwrap().stdout).to_string();
    assert!(listing.contains(&id) && listing.contains("socket test"), "{listing}");
    let mut cmd = Command::new(BIJA);
    cmd.args(["show", &id, "--home", path(&s.home)]);
    let shown = String::from_utf8_lossy(&cmd.output().unwrap().stdout).to_string();
    assert!(shown.contains("> ping") && shown.contains("pong over the socket") && shown.contains("[turn 1 completed]"), "{shown}");

    assert_eq!(b.shutdown().1, 0);
    assert!(!sock.exists(), "the socket file is removed at exit");
}

#[test]
fn a_shell_op_runs_while_idle_and_the_model_sees_it() {
    let mock = Mock::start(|_, _| text_reply("Saw it.", 30));
    let s = Scratch::new();
    let mut b = Bija::spawn(&s.args(&mock, &[]));
    b.ev("ready");
    b.send(json!({"op": "shell", "command": "echo shell-says-hi"}));
    let started = b.ev("tool_started");
    assert_eq!((started["id"].as_str(), started["name"].as_str(), started["turn"].as_str()), (Some("shell-1"), Some("bash"), Some("")));
    let done = b.ev("tool_done");
    assert_eq!((done["id"].as_str(), done["ok"].as_bool(), done["exit_code"].as_i64()), (Some("shell-1"), Some(true), Some(0)));
    assert!(done["result"].as_str().unwrap().contains("shell-says-hi"));
    assert!(b.seen("turn_started").is_empty(), "a shell command is not a turn");

    b.send(json!({"op": "prompt", "text": "what did I run?"}));
    assert_eq!(b.ev("turn_done")["status"], "completed");
    let msgs = messages(&mock.requests()[0].body);
    let note = content(&msgs[1]);
    assert!(note.starts_with("[note] $ echo shell-says-hi\n") && note.contains("shell-says-hi"), "{note}");
    assert_eq!(content(&msgs[2]), "what did I run?");
    assert_eq!(b.shutdown().1, 0);
}

#[test]
fn a_model_stuck_calling_tools_hits_the_round_cap() {
    let mock = Mock::start(|_, i| tool_reply(&[(&format!("call_{i}"), "list_dir", json!({"path": "."}))], 50));
    let s = Scratch::new();
    let mut b = Bija::spawn(&s.args(&mock, &[]));
    let id = b.ev("ready")["session"].as_str().unwrap().to_string();
    b.send(json!({"op": "prompt", "text": "loop forever"}));
    let done = b.wait_for("turn_done", |e| e["ev"] == "turn_done");
    assert_eq!((done["status"].as_str(), done["error"]["kind"].as_str()), (Some("failed"), Some("internal")));
    assert!(done["error"]["message"].as_str().unwrap().contains("round cap"), "{done}");
    assert_eq!(mock.requests().len(), 201, "200 rounds run, the 201st reply is refused");
    assert_eq!(b.shutdown().1, 0);
    let t = s.transcript(&id);
    let last_tool = t.iter().rev().find(|r| r["role"] == "tool").unwrap();
    assert_eq!((last_tool["call_id"].as_str(), last_tool["is_error"].as_bool()), (Some("call_200"), Some(true)), "the refused call still gets a result");
    assert_eq!((t.last().unwrap()["t"].as_str(), t.last().unwrap()["status"].as_str()), (Some("turn"), Some("failed")));
}

#[test]
fn commands_never_see_the_api_key() {
    let mock = Mock::start(|_, i| match i {
        0 => tool_reply(
            &[
                ("call_f", "bash", json!({"command": "echo \"key=[$BIJA_TEST_KEY]\"; env | grep -c test-key || true"})),
                ("call_b", "bash", json!({"command": "echo \"job=[$BIJA_TEST_KEY]\"", "background": true})),
            ],
            50,
        ),
        1 => tool_reply(&[("call_w", "job", json!({"action": "wait", "id": 1, "timeout_secs": 10}))], 60),
        _ => text_reply("Checked.", 70),
    });
    let s = Scratch::new();
    let mut b = Bija::spawn(&s.args(&mock, &[]));
    b.ev("ready");
    b.send(json!({"op": "prompt", "text": "print the key"}));
    assert_eq!(b.ev("turn_done")["status"], "completed", "{}", b.dump());
    let results: Vec<String> = b.seen("tool_done").iter().map(|d| d["result"].as_str().unwrap_or("").to_string()).collect();
    assert!(results[0].starts_with("key=[]\n0\n"), "the foreground command's environment: {results:?}");
    assert!(results[2].contains("job=[]"), "the background job's environment: {results:?}");
    assert!(results.iter().all(|r| !r.contains("test-key")), "{results:?}");
    // bija itself still has it: the requests carried it.
    assert!(mock.requests().iter().all(|r| r.head.to_ascii_lowercase().contains("authorization: bearer test-key")));
    assert_eq!(b.shutdown().1, 0);
}

/// One round of big tool outputs pushes the history past the provider's window. The compaction
/// that follows must not send that same history again: it is cut down to fit, the turn goes on
/// from the summary and completes.
#[test]
fn a_round_that_overflows_the_window_is_compacted_and_the_turn_goes_on() {
    // The mock's "window", in request bytes; one 48 KiB output fits, two do not.
    const LIMIT: usize = 80_000;
    const BIG: &str = "head -c 60000 /dev/zero | tr '\\0' a";
    let mock = Mock::start(|req, _| {
        if req.to_string().len() > LIMIT {
            let error = json!({"error": {"message": "This model's maximum context length is 20000 tokens. However, your messages resulted in 26000 tokens.", "type": "invalid_request_error", "code": "context_length_exceeded"}});
            return Reply::Status { code: 400, body: error, retry_after: None };
        }
        if is_compaction(req) {
            return text_reply("SUMMARY: two big outputs were read; report.", 900);
        }
        if messages(req).iter().any(|m| content(m).starts_with("[context summary")) {
            return text_reply("Done after the overflow.", 300);
        }
        tool_reply(&[("call_a", "bash", json!({"command": BIG})), ("call_b", "bash", json!({"command": BIG}))], 1000)
    });
    let s = Scratch::new();
    let mut b = Bija::spawn(&s.args(&mock, &["--context-window", "100000"]));
    b.ev("ready");
    b.send(json!({"op": "prompt", "text": "read two big things"}));
    let compacted = b.ev("compacted");
    assert_eq!(compacted["turn"], "t1");
    assert_eq!(b.ev("message")["text"], "Done after the overflow.", "{}", b.dump());
    assert_eq!(b.ev("turn_done")["status"], "completed");
    let reqs = mock.requests();
    let sizes: Vec<usize> = reqs.iter().map(|r| r.body.to_string().len()).collect();
    assert_eq!(reqs.len(), 4, "call, overflow, compaction, call: {sizes:?}");
    assert!(sizes[1] > LIMIT && sizes[2] < LIMIT, "{sizes:?}");
    assert!(is_compaction(&reqs[2].body));
    assert!(messages(&reqs[2].body).iter().any(|m| content(m).contains("bytes left out")), "the outputs were clipped for the summary");
    assert!(b.seen("warning").is_empty(), "{:?}", b.seen("warning"));
    assert_eq!(b.shutdown().1, 0);
}

/// An idle compaction cut short by an interrupt says so: whoever asked for it shows "compacting"
/// until it hears how it ended.
#[test]
fn an_interrupted_idle_compaction_is_reported() {
    let mock = Mock::start(|req, _| {
        if is_compaction(req) {
            std::thread::sleep(Duration::from_secs(5));
            return text_reply("too late", 10);
        }
        text_reply("Hello.", 50)
    });
    let s = Scratch::new();
    let mut b = Bija::spawn(&s.args(&mock, &[]));
    b.ev("ready");
    b.send(json!({"op": "prompt", "text": "hi"}));
    b.ev("turn_done");
    b.send(json!({"op": "compact"}));
    wait_until("the compaction request is made", Duration::from_secs(10), || mock.requests().len() == 2);
    b.send(json!({"op": "interrupt"}));
    let warning = b.ev("warning");
    assert_eq!(warning["message"], "compaction interrupted");
    b.send(json!({"op": "status"}));
    assert_eq!(b.ev("status")["busy"], false);
    let (seen, code) = b.shutdown();
    assert_eq!(code, 0);
    assert!(!seen.iter().any(|e| e["ev"] == "compacted"), "nothing was compacted");
}

// ---------------------------------------------------------------------------------------------
// The sandbox, with the real bubblewrap

/// Whether bubblewrap is installed and can create its namespaces here; when not, says why (the
/// sandbox tests then skip: bija runs commands plainly there, which the unit tests cover).
fn bwrap_usable() -> bool {
    let probe = Command::new("bwrap").args(["--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc", "--unshare-pid", "true"]).stdout(Stdio::null()).stderr(Stdio::null()).status();
    match probe {
        Ok(status) if status.success() => true,
        Ok(_) => {
            eprintln!("skipped: bwrap is installed but cannot create its namespaces here");
            false
        }
        Err(_) => {
            eprintln!("skipped: bwrap is not installed");
            false
        }
    }
}

/// A bash call per check: a write into the workspace, one into /tmp, one outside every root, and
/// a connection to a port the test listens on in the host's network namespace.
fn sandbox_checks(port: u16, tmp_file: &str) -> Reply {
    tool_reply(
        &[
            ("call_cwd", "bash", json!({"command": "touch inside.txt && echo wrote-inside"})),
            ("call_tmp", "bash", json!({"command": format!("touch {tmp_file} && echo wrote-tmp")})),
            ("call_usr", "bash", json!({"command": "touch /usr/bija-e2e-refused"})),
            ("call_net", "bash", json!({"command": format!("exec 3<>/dev/tcp/127.0.0.1/{port} && echo connected")})),
        ],
        80,
    )
}

/// The checks' results by call id, from one session in `mode` (`extra` flags), and the system
/// prompt it sent.
fn run_sandbox_checks(s: &Scratch, mock: &Mock, extra: &[&str]) -> (std::collections::HashMap<String, (bool, String)>, String) {
    let first_request = mock.requests().len();
    let mut b = Bija::spawn(&s.sandboxed_args(mock, extra));
    b.ev("ready");
    b.send(json!({"op": "prompt", "text": "check the sandbox"}));
    assert_eq!(b.ev("turn_done")["status"], "completed", "{}", b.dump());
    assert!(b.seen("warning").is_empty(), "bwrap works here, so nothing to warn about\n{}", b.dump());
    let results = b.seen("tool_done").iter().map(|d| (d["id"].as_str().unwrap().to_string(), (d["ok"].as_bool().unwrap(), d["result"].as_str().unwrap().to_string()))).collect();
    assert_eq!(b.shutdown().1, 0);
    let system = content(&messages(&mock.requests()[first_request].body)[0]);
    (results, system)
}

#[test]
fn under_bwrap_writes_stay_in_the_roots_and_the_network_is_opt_in() {
    if !bwrap_usable() {
        return;
    }
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let tmp_file = std::env::temp_dir().join(format!("bija-e2e-bwrap-{}", std::process::id())).display().to_string();
    let checks = tmp_file.clone();
    let mock = Mock::start(move |req, _| if tool_results(req).is_empty() { sandbox_checks(port, &checks) } else { text_reply("Checked.", 90) });
    let s = Scratch::outside_tmp();

    let (r, system) = run_sandbox_checks(&s, &mock, &[]);
    assert!(system.contains("bubblewrap sandbox (no network)"), "{system}");
    assert!(r["call_cwd"].0 && r["call_cwd"].1.contains("wrote-inside") && s.work.join("inside.txt").exists(), "{r:?}");
    assert!(r["call_tmp"].0 && r["call_tmp"].1.contains("wrote-tmp"), "{r:?}");
    assert!(!r["call_usr"].0 && r["call_usr"].1.contains("Read-only file system") && !Path::new("/usr/bija-e2e-refused").exists(), "{r:?}");
    assert!(!r["call_net"].0 && !r["call_net"].1.contains("connected"), "no network by default: {r:?}");

    let (r, system) = run_sandbox_checks(&s, &mock, &["--network"]);
    assert!(system.contains("bubblewrap sandbox (network allowed)"), "{system}");
    assert!(r["call_net"].0 && r["call_net"].1.contains("connected"), "--network shares the host's network: {r:?}");

    std::fs::remove_file(s.work.join("inside.txt")).unwrap();
    let _ = std::fs::remove_file(&tmp_file);
    let (r, _) = run_sandbox_checks(&s, &mock, &["--sandbox", "read-only"]);
    assert!(!r["call_cwd"].0 && r["call_cwd"].1.contains("Read-only file system") && !s.work.join("inside.txt").exists(), "read-only: the workspace too: {r:?}");
    assert!(r["call_tmp"].0, "read-only still leaves /tmp writable: {r:?}");
    let _ = std::fs::remove_file(&tmp_file);
    drop(listener);
}

/// Inside bwrap a command has a session and a pid namespace of its own, out of reach of a
/// `killpg`. An interrupt, and bija dying outright, must still end everything it started.
#[test]
fn under_bwrap_an_interrupt_or_a_dead_bija_ends_every_sandboxed_process() {
    if !bwrap_usable() {
        return;
    }
    const FIRST: &str = "sleep 71.11";
    const SECOND: &str = "sleep 72.22";
    let mock = Mock::start(|req, _| {
        let last = messages(req).last().map(content).unwrap_or_default();
        match last.as_str() {
            "first" => tool_reply(&[("call_1", "bash", json!({"command": format!("{FIRST} & {FIRST}; wait")}))], 60),
            "second" => tool_reply(&[("call_2", "bash", json!({"command": format!("{SECOND} & {SECOND}; wait")}))], 70),
            _ => text_reply("unexpected", 80),
        }
    });
    let s = Scratch::outside_tmp();
    let mut b = Bija::spawn(&s.sandboxed_args(&mock, &[]));
    b.ev("ready");
    b.send(json!({"op": "prompt", "text": "first"}));
    b.ev("tool_started");
    wait_until("the first command runs", Duration::from_secs(10), || processes_with(FIRST).len() >= 2);
    b.send(json!({"op": "interrupt"}));
    assert_eq!(b.ev("turn_done")["status"], "interrupted");
    wait_until("the interrupted command is gone", Duration::from_secs(5), || processes_with(FIRST).is_empty());

    b.send(json!({"op": "prompt", "text": "second"}));
    b.ev("tool_started");
    wait_until("the second command runs", Duration::from_secs(10), || processes_with(SECOND).len() >= 2);
    b.kill9();
    wait_until("the sandbox died with bija", Duration::from_secs(5), || processes_with(SECOND).is_empty());
}
