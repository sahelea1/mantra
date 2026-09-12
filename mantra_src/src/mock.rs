//! `mantra mock-codex`: a fake Codex app-server speaking the real protocol.
//!
//! Powers `mantra --demo` (explore the UI without spending tokens) and end-to-end tests.
//! Agents behave according to the `mantra-role:` marker in their developer instructions.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};

struct Thread {
    cwd: PathBuf,
    role: String,
    dev: String,
    approval: String,
    /// Policy captured when the current turn started (like Codex: fixed for the turn).
    turn_approval: String,
    model: String,
    turns: u32,
    active: Option<(String, Arc<AtomicBool>, Arc<Mutex<Vec<String>>>)>,
    tokens: u64,
    ctx: u64,
}

struct St {
    out: mpsc::UnboundedSender<String>,
    pending: Mutex<HashMap<i64, oneshot::Sender<Value>>>,
    next: AtomicI64,
    ids: AtomicU64,
    threads: Mutex<HashMap<String, Thread>>,
    speed: f64,
}

impl St {
    fn send(&self, v: Value) {
        let _ = self.out.send(v.to_string());
    }
    fn notify(&self, method: &str, params: Value) {
        self.send(json!({"method": method, "params": params}));
    }
    fn uid(&self, p: &str) -> String {
        format!("{p}-{:06x}", self.ids.fetch_add(1, Ordering::SeqCst) + 0x1a2b)
    }
    async fn request(&self, method: &str, params: Value) -> Value {
        let id = self.next.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        self.send(json!({"method": method, "id": id, "params": params}));
        match tokio::time::timeout(Duration::from_secs(3600), rx).await {
            Ok(Ok(v)) => v,
            _ => json!({"error": "no response"}),
        }
    }
}

pub async fn run() {
    let speed = std::env::var("MANTRA_MOCK_SPEED").ok().and_then(|s| s.parse::<f64>().ok()).unwrap_or(1.0).max(0.05);
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(l) = out_rx.recv().await {
            if stdout.write_all(l.as_bytes()).await.is_err() || stdout.write_all(b"\n").await.is_err() {
                break;
            }
            let _ = stdout.flush().await;
        }
    });
    let st = Arc::new(St { out: out_tx, pending: Mutex::new(HashMap::new()), next: AtomicI64::new(9000), ids: AtomicU64::new(0), threads: Mutex::new(HashMap::new()), speed });
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        let method = v.get("method").and_then(|m| m.as_str()).map(|s| s.to_string());
        let id = v.get("id").cloned();
        match (method, id) {
            (Some(m), Some(id)) => handle(&st, &m, id, v.get("params").cloned().unwrap_or(Value::Null)),
            (None, Some(id)) => {
                if let Some(i) = id.as_i64() {
                    if let Some(tx) = st.pending.lock().unwrap().remove(&i) {
                        let _ = tx.send(v.get("result").cloned().unwrap_or_else(|| json!({"error": v.get("error")})));
                    }
                }
            }
            _ => {}
        }
    }
}

fn thread_json(id: &str, cwd: &str, model: &str) -> Value {
    json!({"id": id, "sessionId": id, "preview": "", "ephemeral": false, "modelProvider": "openai", "model": model,
           "createdAt": crate::util::unix_secs(), "updatedAt": crate::util::unix_secs(), "status": {"type": "idle"},
           "cwd": cwd, "cliVersion": "0.154.0-mock", "source": "vscode", "turns": []})
}

fn handle(st: &Arc<St>, method: &str, id: Value, p: Value) {
    let ok = |r: Value| st.send(json!({"id": id, "result": r}));
    match method {
        "initialize" => ok(json!({"userAgent": "mantra-mock/0.154.0", "codexHome": "/tmp/mock-codex", "platformFamily": "unix", "platformOs": std::env::consts::OS})),
        "model/list" => {
            let m = |id: &str, desc: &str, eff: &[&str]| json!({"id": id, "model": id, "displayName": id, "description": desc, "hidden": false,
                "supportedReasoningEfforts": eff.iter().map(|e| json!({"reasoningEffort": e, "description": ""})).collect::<Vec<_>>(),
                "defaultReasoningEffort": "medium", "inputModalities": ["text"], "isDefault": id == "gpt-6-astra"});
            let all = ["low", "medium", "high", "xhigh", "max", "ultra"];
            ok(json!({"data": [m("gpt-6-astra", "Our most capable model", &all), m("gpt-5.6-sol", "Frontier agentic coding", &all),
                m("gpt-5.6-terra", "Balanced", &all), m("gpt-5.6-luna", "Fast and affordable", &all[..5]), m("gpt-5.5", "Frontier", &all[..4])], "nextCursor": null}))
        }
        "thread/start" | "thread/resume" => {
            let tid = p.get("threadId").and_then(|t| t.as_str()).map(|s| s.to_string()).unwrap_or_else(|| st.uid("thr"));
            let cwd = p.get("cwd").and_then(|t| t.as_str()).unwrap_or(".").to_string();
            let dev = p.get("developerInstructions").and_then(|t| t.as_str()).unwrap_or("").to_string();
            let model = p.get("model").and_then(|t| t.as_str()).unwrap_or("gpt-6-astra").to_string();
            let role = dev.lines().find_map(|l| l.trim().strip_prefix("mantra-role:")).map(|r| r.trim().to_string()).unwrap_or_else(|| "solo".into());
            let approval = p.get("approvalPolicy").and_then(|t| t.as_str()).unwrap_or("never").to_string();
            let resumed = method == "thread/resume";
            {
                let mut th = st.threads.lock().unwrap();
                let e = th.entry(tid.clone()).or_insert(Thread { cwd: PathBuf::from(&cwd), role: role.clone(), dev: dev.clone(), approval: approval.clone(), turn_approval: approval.clone(), model: model.clone(), turns: 0, active: None, tokens: 0, ctx: 9000 });
                if !dev.is_empty() {
                    e.role = role;
                    e.dev = dev;
                }
                if resumed {
                    e.turns = e.turns.max(1);
                }
            }
            ok(json!({"thread": thread_json(&tid, &cwd, &model), "model": model, "modelProvider": "openai", "cwd": cwd, "approvalPolicy": approval, "reasoningEffort": null}));
            st.notify("thread/started", json!({"thread": thread_json(&tid, &cwd, &model)}));
        }
        "turn/start" => {
            let tid = p.get("threadId").and_then(|t| t.as_str()).unwrap_or("").to_string();
            let text = p.pointer("/input/0/text").and_then(|t| t.as_str()).unwrap_or("").to_string();
            let turn_id = st.uid("turn");
            let cancel = Arc::new(AtomicBool::new(false));
            let steer = Arc::new(Mutex::new(vec![]));
            let exists = {
                let mut th = st.threads.lock().unwrap();
                match th.get_mut(&tid) {
                    Some(t) => {
                        if let Some(a) = p.get("approvalPolicy").and_then(|a| a.as_str()) {
                            t.approval = a.to_string();
                        }
                        t.turn_approval = t.approval.clone();
                        t.turns += 1;
                        t.active = Some((turn_id.clone(), cancel.clone(), steer.clone()));
                        true
                    }
                    None => false,
                }
            };
            if !exists {
                st.send(json!({"id": id, "error": {"code": -32600, "message": "thread not found"}}));
                return;
            }
            ok(json!({"turn": {"id": turn_id, "items": [], "itemsView": "notLoaded", "status": "inProgress", "error": null}}));
            let st2 = st.clone();
            tokio::spawn(async move {
                let e = Em { st: st2.clone(), tid: tid.clone(), turn: turn_id.clone(), cancel, steer, diff: Arc::new(Mutex::new(String::new())) };
                e.notify("thread/status/changed", json!({"threadId": tid, "status": {"type": "active", "activeFlags": []}}));
                e.notify("turn/started", json!({"threadId": tid, "turn": {"id": turn_id, "items": [], "status": "inProgress"}}));
                let uid = st2.uid("msg");
                let user = json!({"type": "userMessage", "id": uid, "clientId": null, "content": [{"type": "text", "text": text, "text_elements": []}]});
                e.item(&user, false);
                e.item(&user, true);
                let mut outcome = script(&e, &text).await;
                // Like Codex: input steered in near the end of a turn is still handled in that turn.
                if matches!(outcome, Outcome::Done) && !e.steer.lock().unwrap().is_empty() {
                    e.absorb_steer().await;
                    if !e.say("Got it — I'll take that into account.").await {
                        outcome = Outcome::Interrupted;
                    }
                }
                let (status, err) = match outcome {
                    Outcome::Done => ("completed", Value::Null),
                    Outcome::Interrupted => ("interrupted", Value::Null),
                    Outcome::Failed(info, msg) => ("failed", json!({"message": msg, "codexErrorInfo": info, "additionalDetails": null})),
                };
                if let Some(t) = st2.threads.lock().unwrap().get_mut(&tid) {
                    t.active = None;
                }
                e.notify("turn/completed", json!({"threadId": tid, "turn": {"id": turn_id, "items": [], "status": status, "error": err}}));
                e.notify("thread/status/changed", json!({"threadId": tid, "status": {"type": "idle"}}));
            });
        }
        "turn/steer" => {
            let tid = p.get("threadId").and_then(|t| t.as_str()).unwrap_or("");
            let text = p.pointer("/input/0/text").and_then(|t| t.as_str()).unwrap_or("").to_string();
            let th = st.threads.lock().unwrap();
            match th.get(tid).and_then(|t| t.active.as_ref()) {
                Some((turn, _, q)) => {
                    q.lock().unwrap().push(text);
                    st.send(json!({"id": id, "result": {"turnId": turn}}));
                }
                None => st.send(json!({"id": id, "error": {"code": -32600, "message": "no active turn"}})),
            }
        }
        "turn/interrupt" => {
            let tid = p.get("threadId").and_then(|t| t.as_str()).unwrap_or("");
            if let Some((_, c, _)) = st.threads.lock().unwrap().get(tid).and_then(|t| t.active.as_ref()) {
                c.store(true, Ordering::SeqCst);
            }
            ok(json!({}));
        }
        "thread/compact/start" => {
            ok(json!({}));
            let tid = p.get("threadId").and_then(|t| t.as_str()).unwrap_or("").to_string();
            let st2 = st.clone();
            tokio::spawn(async move {
                let turn = st2.uid("turn");
                let e = Em { st: st2.clone(), tid: tid.clone(), turn: turn.clone(), cancel: Arc::new(AtomicBool::new(false)), steer: Arc::new(Mutex::new(vec![])), diff: Arc::new(Mutex::new(String::new())) };
                e.notify("turn/started", json!({"threadId": tid, "turn": {"id": turn, "items": [], "status": "inProgress"}}));
                let id = st2.uid("cc");
                e.item(&json!({"type": "contextCompaction", "id": id}), false);
                e.sleep(1400).await;
                e.th(|t| t.ctx = t.ctx / 6 + 4000);
                e.item(&json!({"type": "contextCompaction", "id": id}), true);
                e.tokens(0);
                e.notify("thread/compacted", json!({"threadId": tid, "turnId": turn}));
                e.notify("turn/completed", json!({"threadId": tid, "turn": {"id": turn, "items": [], "status": "completed", "error": null}}));
            });
        }
        "thread/archive" => ok(json!({})),
        "thread/settings/update" => {
            let tid = p.get("threadId").and_then(|t| t.as_str()).unwrap_or("");
            if let (Some(t), Some(a)) = (st.threads.lock().unwrap().get_mut(tid), p.get("approvalPolicy").and_then(|a| a.as_str())) {
                t.approval = a.to_string(); // subsequent turns only — the running turn keeps its policy
            }
            ok(json!({}));
        }
        "thread/shellCommand" => {
            ok(json!({}));
            let tid = p.get("threadId").and_then(|t| t.as_str()).unwrap_or("").to_string();
            let cmd = p.get("command").and_then(|t| t.as_str()).unwrap_or("").to_string();
            let st2 = st.clone();
            tokio::spawn(async move {
                let e = Em { st: st2, tid, turn: "shell".into(), cancel: Arc::new(AtomicBool::new(false)), steer: Arc::new(Mutex::new(vec![])), diff: Arc::new(Mutex::new(String::new())) };
                let out = std::process::Command::new("sh").arg("-c").arg(&cmd).current_dir(e.cwd()).output().map(|o| String::from_utf8_lossy(&o.stdout).to_string() + &String::from_utf8_lossy(&o.stderr)).unwrap_or_else(|x| x.to_string());
                e.command(&cmd, &out, 0).await;
            });
        }
        _ => st.send(json!({"id": id, "error": {"code": -32601, "message": format!("mock: {method} not supported")}})),
    }
}

enum Outcome {
    Done,
    Interrupted,
    Failed(Value, String),
}

struct Em {
    st: Arc<St>,
    tid: String,
    turn: String,
    cancel: Arc<AtomicBool>,
    steer: Arc<Mutex<Vec<String>>>,
    diff: Arc<Mutex<String>>,
}

impl Em {
    fn notify(&self, m: &str, p: Value) {
        self.st.notify(m, p);
    }
    fn item(&self, item: &Value, done: bool) {
        let m = if done { "item/completed" } else { "item/started" };
        self.notify(m, json!({"item": item, "threadId": self.tid, "turnId": self.turn, "startedAtMs": 0, "completedAtMs": 0}));
    }
    fn th<R>(&self, f: impl FnOnce(&mut Thread) -> R) -> Option<R> {
        self.st.threads.lock().unwrap().get_mut(&self.tid).map(f)
    }
    fn cwd(&self) -> PathBuf {
        self.th(|t| t.cwd.clone()).unwrap_or_else(|| PathBuf::from("."))
    }
    fn role(&self) -> String {
        self.th(|t| t.role.clone()).unwrap_or_default()
    }
    fn dev(&self) -> String {
        self.th(|t| t.dev.clone()).unwrap_or_default()
    }
    fn turns(&self) -> u32 {
        self.th(|t| t.turns).unwrap_or(1)
    }
    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }
    async fn sleep(&self, ms: u64) -> bool {
        let total = (ms as f64 / self.st.speed) as u64;
        let mut left = total;
        while left > 0 {
            let step = left.min(50);
            tokio::time::sleep(Duration::from_millis(step)).await;
            left -= step;
            if self.cancelled() {
                return false;
            }
        }
        !self.cancelled()
    }
    fn tokens(&self, add: u64) {
        let (total, mut ctx, model) = self.th(|t| {
            t.tokens += add;
            t.ctx += add * 2;
            (t.tokens, t.ctx, t.model.clone())
        }).unwrap_or((add, add, String::new()));
        let window: u64 = if model.contains("astra") { 400_000 } else { 272_000 };
        // Codex-style automatic compaction mid-turn when the context gets full.
        if ctx > window * 7 / 10 {
            let id = self.st.uid("cc");
            self.item(&json!({"type": "contextCompaction", "id": id}), false);
            ctx = self.th(|t| {
                t.ctx = t.ctx / 6 + 4000;
                t.ctx
            }).unwrap_or(ctx);
            self.item(&json!({"type": "contextCompaction", "id": id}), true);
        }
        let ctx = ctx.min(window - 1000);
        self.notify("thread/tokenUsage/updated", json!({"threadId": self.tid, "turnId": self.turn, "tokenUsage": {
            "total": {"totalTokens": total, "inputTokens": total * 4 / 5, "cachedInputTokens": total / 2, "cacheWriteInputTokens": 0, "outputTokens": total / 5, "reasoningOutputTokens": total / 10},
            "last": {"totalTokens": ctx, "inputTokens": ctx.saturating_sub(800), "cachedInputTokens": 0, "cacheWriteInputTokens": 0, "outputTokens": 800, "reasoningOutputTokens": 300},
            "modelContextWindow": window}}));
    }
    async fn stream(&self, kind: &str, text: &str, per_word_ms: u64) -> bool {
        let id = self.st.uid(if kind == "reasoning" { "rs" } else { "am" });
        let (item0, delta_m) = if kind == "reasoning" {
            (json!({"type": "reasoning", "id": id, "summary": [], "content": []}), "item/reasoning/summaryTextDelta")
        } else {
            (json!({"type": "agentMessage", "id": id, "text": "", "phase": null}), "item/agentMessage/delta")
        };
        self.item(&item0, false);
        let mut ok = true;
        let words: Vec<&str> = text.split_inclusive(' ').collect();
        for chunk in words.chunks(2) {
            let d: String = chunk.concat();
            self.notify(delta_m, json!({"threadId": self.tid, "turnId": self.turn, "itemId": id, "delta": d, "summaryIndex": 0}));
            if !self.sleep(per_word_ms * 2).await {
                ok = false;
                break;
            }
        }
        let done = if kind == "reasoning" {
            json!({"type": "reasoning", "id": id, "summary": [text], "content": []})
        } else {
            json!({"type": "agentMessage", "id": id, "text": text, "phase": null})
        };
        self.item(&done, true);
        self.tokens(text.len() as u64 * 3 + 400);
        ok
    }
    async fn think(&self, t: &str) -> bool {
        self.stream("reasoning", t, 45).await
    }
    async fn say(&self, t: &str) -> bool {
        self.stream("message", t, 30).await
    }
    async fn command(&self, cmd: &str, output: &str, exit: i64) -> bool {
        let id = self.st.uid("cmd");
        let cwd = self.cwd().to_string_lossy().to_string();
        self.item(&json!({"type": "commandExecution", "id": id, "command": cmd, "cwd": cwd, "status": "inProgress", "commandActions": [], "aggregatedOutput": null, "exitCode": null, "durationMs": null}), false);
        let mut ok = true;
        for l in output.lines().take(40) {
            self.notify("item/commandExecution/outputDelta", json!({"threadId": self.tid, "turnId": self.turn, "itemId": id, "delta": format!("{l}\n")}));
            if !self.sleep(60).await {
                ok = false;
                break;
            }
        }
        self.item(&json!({"type": "commandExecution", "id": id, "command": cmd, "cwd": cwd, "status": if exit == 0 { "completed" } else { "failed" }, "commandActions": [], "aggregatedOutput": output, "exitCode": exit, "durationMs": 400 + output.len() as u64}), true);
        self.tokens(600 + output.len() as u64);
        ok
    }
    async fn write(&self, rel: &str, content: &str) -> bool {
        let path = self.cwd().join(rel);
        let old = std::fs::read_to_string(&path).ok();
        if let Some(d) = path.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let _ = std::fs::write(&path, content);
        let id = self.st.uid("fc");
        let (kind, diff) = match &old {
            None => (json!({"type": "add"}), content.to_string()),
            Some(o) if content.starts_with(o.as_str()) && content.len() > o.len() => {
                // append: a proper minimal hunk
                let n = o.lines().count();
                let added: Vec<&str> = content[o.len()..].lines().collect();
                let mut d = format!("@@ -{n},0 +{},{} @@\n", n + 1, added.len());
                for l in added {
                    d.push_str(&format!("+{l}\n"));
                }
                (json!({"type": "update", "move_path": null}), d)
            }
            Some(o) => {
                let mut d = format!("@@ -1,{} +1,{} @@\n", o.lines().count(), content.lines().count());
                for l in o.lines() {
                    d.push_str(&format!("-{l}\n"));
                }
                for l in content.lines() {
                    d.push_str(&format!("+{l}\n"));
                }
                (json!({"type": "update", "move_path": null}), d)
            }
        };
        let abs = path.to_string_lossy().to_string();
        let ch = json!([{"path": abs, "kind": kind, "diff": diff}]);
        self.item(&json!({"type": "fileChange", "id": id, "changes": ch, "status": "inProgress"}), false);
        let ok = self.sleep(250).await;
        self.item(&json!({"type": "fileChange", "id": id, "changes": ch, "status": "completed"}), true);
        let mut agg = self.diff.lock().unwrap();
        agg.push_str(&format!("diff --git a/{rel} b/{rel}\n"));
        if old.is_none() {
            agg.push_str(&format!("new file mode 100644\n--- /dev/null\n+++ b/{rel}\n@@ -0,0 +1,{} @@\n", content.lines().count()));
            for l in content.lines() {
                agg.push_str(&format!("+{l}\n"));
            }
        } else {
            agg.push_str(&format!("--- a/{rel}\n+++ b/{rel}\n{diff}"));
        }
        let d = agg.clone();
        drop(agg);
        self.notify("turn/diff/updated", json!({"threadId": self.tid, "turnId": self.turn, "diff": d}));
        self.tokens(900 + content.len() as u64);
        ok
    }
    fn plan(&self, steps: &[(&str, &str)]) {
        let p: Vec<Value> = steps.iter().map(|(s, st)| json!({"step": s, "status": st})).collect();
        self.notify("turn/plan/updated", json!({"threadId": self.tid, "turnId": self.turn, "explanation": null, "plan": p}));
    }
    async fn tool(&self, name: &str, args: Value) -> String {
        let id = self.st.uid("tc");
        self.item(&json!({"type": "dynamicToolCall", "id": id, "namespace": null, "tool": name, "arguments": args, "status": "inProgress", "contentItems": null, "success": null, "durationMs": null}), false);
        let r = self.st.request("item/tool/call", json!({"threadId": self.tid, "turnId": self.turn, "callId": id, "namespace": null, "tool": name, "arguments": args})).await;
        let text = r.get("contentItems").and_then(|c| c.as_array()).map(|a| a.iter().filter_map(|x| x.get("text").and_then(|t| t.as_str())).collect::<Vec<_>>().join("\n")).unwrap_or_default();
        let success = r.get("success").and_then(|s| s.as_bool()).unwrap_or(false);
        self.item(&json!({"type": "dynamicToolCall", "id": id, "namespace": null, "tool": name, "arguments": args, "status": if success { "completed" } else { "failed" }, "contentItems": [{"type": "inputText", "text": text}], "success": success, "durationMs": 20}), true);
        self.tokens(500 + text.len() as u64);
        text
    }
    async fn approve(&self, cmd: &str) -> bool {
        let id = self.st.uid("cmd");
        self.notify("thread/status/changed", json!({"threadId": self.tid, "status": {"type": "active", "activeFlags": ["waitingOnApproval"]}}));
        let r = self.st.request("item/commandExecution/requestApproval", json!({"kind": "command", "threadId": self.tid, "turnId": self.turn, "itemId": id, "startedAtMs": 0, "command": cmd, "cwd": self.cwd(), "reason": "run the test suite", "availableDecisions": ["accept", "acceptForSession", "decline", "cancel"]})).await;
        self.notify("thread/status/changed", json!({"threadId": self.tid, "status": {"type": "active", "activeFlags": []}}));
        matches!(r.get("decision").and_then(|d| d.as_str()), Some("accept") | Some("acceptForSession"))
    }
    async fn absorb_steer(&self) {
        let msgs: Vec<String> = std::mem::take(&mut *self.steer.lock().unwrap());
        for m in msgs {
            let id = self.st.uid("msg");
            let u = json!({"type": "userMessage", "id": id, "clientId": null, "content": [{"type": "text", "text": m, "text_elements": []}]});
            self.item(&u, true);
            self.think("**Adjusting course**\n\nNoted the new instruction; I'll fold it into what I'm doing.").await;
        }
    }
}

macro_rules! step {
    ($e:expr) => {
        if !$e {
            return Outcome::Interrupted;
        }
    };
}

async fn script(e: &Em, text: &str) -> Outcome {
    match e.role().as_str() {
        "planner" => planner(e, text).await,
        "orchestrator" => orchestrator(e, text).await,
        "worker" => worker(e, text).await,
        "gate" => gate(e, text).await,
        "architect" => architect(e, text).await,
        _ => solo(e, text).await,
    }
}

async fn solo(e: &Em, text: &str) -> Outcome {
    let t = crate::util::trunc(text.lines().next().unwrap_or(""), 60);
    step!(e.think(&format!("**Understanding the request**\n\nThe user wants: \"{t}\". I'll look at how the project is laid out before changing anything.")).await);
    e.plan(&[("Explore the codebase", "inProgress"), ("Implement the change", "pending"), ("Run the tests", "pending")]);
    let listing: String = std::fs::read_dir(e.cwd()).map(|rd| rd.flatten().map(|d| d.file_name().to_string_lossy().to_string()).filter(|n| !n.starts_with('.')).collect::<Vec<_>>().join("\n")).unwrap_or_default();
    step!(e.command("ls", &listing, 0).await);
    step!(e.command("rg -n \"pub fn\" src | head -8", "src/lib.rs:3:pub fn greet(name: &str) -> String {\nsrc/lib.rs:9:pub fn parse_args() -> Args {\nsrc/store.rs:12:pub fn open(path: &Path) -> Result<Store> {", 0).await);
    if text.to_lowercase().contains("fail") {
        step!(e.sleep(600).await);
        return Outcome::Failed(json!("serverOverloaded"), "The server is overloaded, please try again later.".into());
    }
    e.absorb_steer().await;
    step!(e.think("**Planning the change**\n\nThe cleanest place is a small new module wired from `lib.rs`. I'll add it with a unit test.").await);
    e.plan(&[("Explore the codebase", "completed"), ("Implement the change", "inProgress"), ("Run the tests", "pending")]);
    let n = e.turns();
    let file = format!("src/feature_{n}.rs");
    step!(e.write(&file, &format!("//! {t}\n\n/// Added by the agent for: {t}\npub fn feature_{n}(input: &str) -> String {{\n    input.trim().to_uppercase()\n}}\n\n#[cfg(test)]\nmod tests {{\n    #[test]\n    fn works() {{\n        assert_eq!(super::feature_{n}(\" hi \"), \"HI\");\n    }}\n}}\n")).await);
    let lib = std::fs::read_to_string(e.cwd().join("src/lib.rs")).unwrap_or_default();
    step!(e.write("src/lib.rs", &format!("{lib}pub mod feature_{n};\n")).await);
    e.plan(&[("Explore the codebase", "completed"), ("Implement the change", "completed"), ("Run the tests", "inProgress")]);
    let approval = e.th(|t| t.turn_approval.clone()).unwrap_or_default();
    let run_tests = if approval == "never" { true } else { e.approve("cargo test").await };
    if e.cancelled() {
        return Outcome::Interrupted;
    }
    if run_tests {
        step!(e.command("cargo test", "   Compiling demo v0.1.0\n    Finished test profile in 1.92s\n     Running unittests src/lib.rs\n\nrunning 7 tests\ntest feature::works ... ok\ntest store::roundtrip ... ok\ntest greet::basic ... ok\n\ntest result: ok. 7 passed; 0 failed; 0 ignored", 0).await);
    }
    e.plan(&[("Explore the codebase", "completed"), ("Implement the change", "completed"), ("Run the tests", if run_tests { "completed" } else { "pending" })]);
    step!(e.say(&format!("Done. I added **`feature_{n}`** in `{file}` and exported it from `src/lib.rs`.\n\n- trims and upper-cases its input\n- has a unit test (`feature_{n}::works`)\n- {}\n\nWant me to wire it into the CLI next?", if run_tests { "`cargo test` passes (7 tests)" } else { "tests were **not run** (you declined)" })).await);
    Outcome::Done
}

/// Shared with `mock_claude.rs` (WP10.6): the same demo plan, so a phase spawned by a Claude-backed
/// planner looks identical to one spawned by a Codex-backed planner.
pub(crate) fn mock_plan(first: bool) -> Value {
    let t = |id: &str, title: &str, role: &str, scope: &str, prompt: &str| json!({"id": id, "title": title, "role": role, "scope": [scope], "prompt": prompt, "acceptance": "builds, tests pass"});
    let mut p1_tasks = vec![
        t("p1-models", "Domain models", "worker-small", "src/models/**", "Create the core domain model types with serde derives and constructors."),
        t("p1-config", "Config loader", "worker-small", if first { "src/models/**" } else { "src/config/**" }, "Add a typed configuration loader with defaults and env overrides."),
    ];
    // WP6 test scenario: a task whose model the provider rejects outright (ProviderRejected halt).
    if std::env::var("MANTRA_MOCK_BADMODEL").is_ok() {
        p1_tasks.push(t("p1-badmodel", "Feature flags", "worker-small", "src/flags/**", "Add a simple feature-flag lookup."));
    }
    json!({"plan": {
        "title": "Build the requested feature set",
        "summary": "Three phases: shared foundations first, then the features in parallel, then integration and docs.",
        "orchestrator_brief": "Workers are independent inside a phase. Watch p2-auth closely (security-sensitive).",
        "phases": [
            {"id": "p1", "name": "Foundations", "goal": "Shared models and configuration", "tasks": p1_tasks,
             "gate": {"checks": ["test -d src", "echo gate-ok"], "focus": "consistent naming", "criteria": "models and config compile together"}},
            {"id": "p2", "name": "Features", "goal": "The main features, in parallel", "tasks": [
                t("p2-api", "HTTP API", "worker-big", "src/api/**", "Implement the REST handlers for the domain models with validation."),
                t("p2-auth", "Auth", "worker-big", "src/auth/**", "Implement token auth middleware with tests."),
                t("p2-ui", "Web UI", "worker-small", "web/**", "Build the minimal web UI pages for the API.")],
             "gate": {"checks": ["test -d src/api", "echo gate-ok"], "focus": "API ↔ auth integration", "criteria": "all features wired, tests green"}},
            {"id": "p3", "name": "Integration", "goal": "Wire everything and document it", "tasks": [
                t("p3-wire", "Wire-up", "worker-big", "src/app/**", "Wire API, auth and config into the app entrypoint."),
                t("p3-docs", "Docs", "worker-small", "docs/**", "Write the README section and API docs.")],
             "gate": {"checks": ["echo gate-ok"], "focus": "end-to-end flow", "criteria": "app starts, docs accurate"}}
        ],
        "final_checks": ["echo final-ok"]
    }})
}

async fn planner(e: &Em, text: &str) -> Outcome {
    if text.contains("[mantra:plan]") || text.contains("[mantra:revise]") || text.contains("[mantra] You haven't") {
        step!(e.think("**Exploring the project**\n\nBefore planning I want to see the layout, the build system and existing tests.").await);
        step!(e.command("ls -R | head -30", "Cargo.toml\nREADME.md\nsrc\nsrc/lib.rs\nsrc/main.rs\ntests", 0).await);
        step!(e.command("cat README.md", "# demo\nA small demo project for Mantra.", 0).await);
        step!(e.think("**Designing phases**\n\nFoundations must land before features. The three features are independent → one parallel phase with disjoint scopes. Integration and docs last.").await);
        let first = text.contains("[mantra:plan]");
        let r = e.tool("mantra_submit_plan", mock_plan(first)).await;
        if r.starts_with("REJECTED") {
            step!(e.think("**Fixing the plan**\n\nTwo parallel tasks claimed the same scope. Giving the config loader its own directory.").await);
            let _ = e.tool("mantra_submit_plan", mock_plan(false)).await;
        }
        step!(e.say("Plan submitted: **3 phases**, 7 tasks.\n\n1. Foundations — models + config (parallel)\n2. Features — API, auth, web UI (parallel)\n3. Integration — wiring + docs").await);
        return Outcome::Done;
    }
    if text.contains("[mantra:reprompt]") {
        step!(e.think("**Re-prompt from the user**\n\nChecking current state before deciding whether anything needs to pause.").await);
        let _ = e.tool("mantra_status", json!({})).await;
        let ask = text.lines().nth(1).unwrap_or("").to_string();
        let _ = e.tool("mantra_brief_orchestrator", json!({"message": format!("User request mid-run: \"{}\". Make sure the running workers account for it; steer them if needed.", crate::util::trunc(&ask, 120))})).await;
        step!(e.say("Handled the re-prompt: no plan change needed, the orchestrator has been briefed to steer the running workers.").await);
        return Outcome::Done;
    }
    if text.contains("[mantra:finale]") {
        step!(e.think("**Final verification**\n\nComparing the plan against what was built. Running the app and the checks.").await);
        step!(e.command("cargo run -- --help", "demo 0.1.0\nUSAGE: demo [OPTIONS]", 0).await);
        let _ = e.tool("mantra_spawn_adhoc", json!({"title": "Polish CLI help text", "role": "worker-small", "prompt": "Improve the --help output wording and add examples.", "scope": ["src/cli/**"]})).await;
        let _ = e.tool("mantra_wait", json!({})).await;
        return Outcome::Done;
    }
    if text.contains("[mantra:event]") {
        step!(e.think("**Re-verifying after fixes**").await);
        let _ = e.tool("mantra_gate_report", json!({"pass": true, "summary": "All plan items verified; CLI polish landed."})).await;
        step!(e.say("Verification complete: everything in the plan is implemented and checked.").await);
        return Outcome::Done;
    }
    step!(e.say("Noted.").await);
    Outcome::Done
}

async fn orchestrator(e: &Em, text: &str) -> Outcome {
    // WP7.2 demo: the watchdog nudged this (real, running) agent directly — acknowledge it and go
    // back to `mantra_wait`; there is nothing else queued to react to.
    if text.contains("[mantra:watchdog]") {
        step!(e.think("**Watchdog nudge**\n\nNothing new since my last check — logging and waiting again.").await);
        let _ = e.tool("mantra_wait", json!({})).await;
        return Outcome::Done;
    }
    if text.contains("[mantra:phase]") {
        let json_block = text.split("```json").nth(1).and_then(|s| s.split("```").next()).unwrap_or("{}");
        let phase: Value = serde_json::from_str(json_block).unwrap_or(json!({}));
        let ids: Vec<String> = phase.get("tasks").and_then(|t| t.as_array()).map(|a| a.iter().filter_map(|t| t.get("id").and_then(|i| i.as_str()).map(|s| s.to_string())).collect()).unwrap_or_default();
        step!(e.think(&format!("**Starting the phase**\n\n{} independent tasks — spawning them all in parallel.", ids.len())).await);
        for id in &ids {
            let _ = e.tool("mantra_spawn", json!({"task_id": id})).await;
            step!(e.sleep(250).await);
        }
        let _ = e.tool("mantra_wait", json!({})).await;
        return Outcome::Done;
    }
    if text.contains("[mantra:handoff]") {
        step!(e.say("Handoff: phase complete. Interfaces are in place; keep naming consistent with `models::*`. No open risks.").await);
        return Outcome::Done;
    }
    if text.contains("TRIPWIRE") {
        let who = text.split("TRIPWIRE: ").nth(1).and_then(|s| s.split_whitespace().next()).unwrap_or("").to_string();
        step!(e.think(&format!("**Tripwire on {who}**\n\nIt touched files outside its scope. I'll tell it to revert those and stay in its lane.")).await);
        let _ = e.tool("mantra_prompt", json!({"agent": who, "message": "You edited files outside your scope. Revert those edits and keep changes inside your scope."})).await;
    } else if let Some(pos) = text.find(" FAILED") {
        let who = text[..pos].split_whitespace().last().unwrap_or("").to_string();
        step!(e.think(&format!("**{who} failed**\n\nRespawning it with a tighter prompt.")).await);
        let _ = e.tool("mantra_retry", json!({"task_id": who, "prompt": "Retry the task; keep it minimal and verify with tests."})).await;
    } else {
        step!(e.think("**Progress update**\n\nAll good — nothing to correct.").await);
    }
    let _ = e.tool("mantra_wait", json!({})).await;
    Outcome::Done
}

/// Shared with `mock_claude.rs` (WP10.6).
pub(crate) fn task_info(dev: &str) -> (String, String, Vec<String>) {
    let line = dev.lines().find(|l| l.starts_with("## Your task:")).unwrap_or("## Your task: task — work");
    let rest = line.trim_start_matches("## Your task:").trim();
    let mut parts = rest.splitn(2, " — ");
    let id = parts.next().unwrap_or("task").trim().to_string();
    let title = parts.next().unwrap_or("work").trim().to_string();
    let scope = dev.lines().find_map(|l| l.strip_prefix("Scope: ")).map(|s| s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.starts_with('(')).collect()).unwrap_or_default();
    (id, title, scope)
}

async fn worker(e: &Em, text: &str) -> Outcome {
    let (id, title, scope) = task_info(&e.dev());
    let dir = scope.first().map(|s| s.trim_end_matches("/**").trim_end_matches("/*").to_string()).unwrap_or_else(|| "src".into());
    let h: u64 = id.bytes().map(|b| b as u64).sum();
    let steps = [("Read the relevant code", "inProgress"), ("Implement", "pending"), ("Verify", "pending")];
    e.plan(&steps);
    // WP7.2/7.6 demo (`MANTRA_MOCK_LAZY_ORCH=1`): this worker's process vanishes mid-task on its
    // very first turn without reporting anything useful — the turn ends `interrupted`, which
    // `Run::on_worker_done` does not advance out of `WState::Running` (that's the F3 rule: an
    // interrupted-but-still-Running worker is "idle"). The mock orchestrator's default reaction to
    // an "… was interrupted …" event is just a log line and `mantra_wait` (no `mantra_prompt`/
    // `mantra_retry`), so nothing re-prompts this worker's own agent — it is the watchdog's
    // `Expect::Working` nudge (`Run::watchdog_tick`), not any orchestrator-side self-heal, that
    // eventually wakes it back up. This is the one place in the default demo pattern where the
    // engine's other self-healing paths (the orchestrator's own "forgot to spawn" safety net,
    // `wake_orch`, `check_phase_done`, …) don't already paper over the gap, so it is what
    // `stress.sh` uses to exercise the watchdog end to end through a real running mock agent.
    if id == "p1-config" && e.turns() == 1 && std::env::var("MANTRA_MOCK_LAZY_ORCH").is_ok() {
        step!(e.think(&format!("**{title}**\n\nReading the surrounding code to match existing conventions.")).await);
        step!(e.sleep(200).await);
        return Outcome::Interrupted;
    }
    if text.contains("[mantra:retry]") || text.contains("[mantra:resume]") {
        step!(e.think("**Resuming**\n\nPicking up where the previous attempt stopped.").await);
    } else if !text.contains("[from") {
        step!(e.think(&format!("**{title}**\n\nReading the surrounding code to match existing conventions.")).await);
        step!(e.command(&format!("rg -n \"struct|fn\" {dir} || true"), "src/lib.rs:3:pub fn greet(name: &str) -> String {", 0).await);
    }
    if text.contains("[from") {
        e.absorb_steer().await;
        step!(e.think("**Following the new instruction**").await);
        step!(e.say(&format!("Adjusted as asked.\n\nSTATUS: done\nSUMMARY: {title} — applied the correction; changes stay within {dir}/.")).await);
        return Outcome::Done;
    }
    step!(e.sleep(400 + (h % 5) * 300).await);
    e.plan(&[("Read the relevant code", "completed"), ("Implement", "inProgress"), ("Verify", "pending")]);
    let file = format!("{dir}/{}.rs", id.replace('-', "_"));
    step!(e.write(&file, &format!("//! {title}\n\npub struct {0};\n\nimpl {0} {{\n    pub fn new() -> Self {{ {0} }}\n    pub fn run(&self) -> Result<(), String> {{ Ok(()) }}\n}}\n", camel(&id))).await);
    e.absorb_steer().await;
    // Demo: the UI worker strays outside its scope on the first attempt → tripwire.
    if id.ends_with("-ui") && e.turns() == 1 {
        step!(e.write("src/shared/theme.rs", "pub const ACCENT: &str = \"#F2A541\";\n").await);
    }
    // Demo: auth hits a transient API error on its first turn → automatic retry.
    if id.ends_with("-auth") && e.turns() == 1 {
        step!(e.sleep(500).await);
        return Outcome::Failed(json!({"responseStreamDisconnected": {"httpStatusCode": 502}}), "stream disconnected before completion".into());
    }
    // Demo (WP6, MANTRA_MOCK_BADMODEL=1): the provider rejects this model/role outright — a
    // deterministic 400 that must halt with ProviderRejected, never retry.
    if id.ends_with("-badmodel") && e.turns() == 1 {
        step!(e.sleep(300).await);
        return Outcome::Failed(json!("badRequest"), "Unexpected message role: developer".into());
    }
    step!(e.sleep(300 + (h % 7) * 250).await);
    step!(e.write(&format!("{dir}/{}_test.rs", id.replace('-', "_")), &format!("#[test]\nfn {}_works() {{ assert!(true); }}\n", id.replace('-', "_"))).await);
    e.plan(&[("Read the relevant code", "completed"), ("Implement", "completed"), ("Verify", "inProgress")]);
    step!(e.command("cargo test --quiet", "running 4 tests\n....\ntest result: ok. 4 passed; 0 failed", 0).await);
    e.plan(&[("Read the relevant code", "completed"), ("Implement", "completed"), ("Verify", "completed")]);
    step!(e.say(&format!("Implemented **{title}** in `{file}` with a test.\n\nSTATUS: done\nSUMMARY: added {} and its unit test; cargo test passes (4 tests).", camel(&id))).await);
    Outcome::Done
}

async fn gate(e: &Em, text: &str) -> Outcome {
    if text.contains("[mantra:event]") {
        let _ = e.tool("mantra_gate_report", json!({"pass": true, "summary": "fixes verified"})).await;
        step!(e.say("Verified the fixes.").await);
        return Outcome::Done;
    }
    let security = text.to_lowercase().contains("security");
    step!(e.think(if security { "**Security sweep**\n\nLooking for injection points, auth gaps and secrets in the new code." } else { "**Reviewing the merged result**\n\nChecking that the parallel changes fit together: names, interfaces, duplicated logic." }).await);
    step!(e.command("git log --oneline -6", "a1b2c3d mantra: phase work\n9f8e7d6 merge worker branches\n1234567 initial", 0).await);
    step!(e.command("cargo test", "running 18 tests\n..................\ntest result: ok. 18 passed; 0 failed", 0).await);
    if security {
        step!(e.write("SECURITY.md", "# Security notes\n\n- Input validation on all API handlers\n- Tokens compared in constant time\n").await);
    } else {
        step!(e.write("docs/INTEGRATION.md", "# Integration notes\n\nModules share `models::*`; config is loaded once at startup.\n").await);
    }
    let _ = e.tool("mantra_gate_report", json!({"pass": true, "summary": if security { "No critical issues; hardened token comparison." } else { "Coherent, tests green." }})).await;
    step!(e.say("GATE: pass — everything is coherent and the checks are green.").await);
    Outcome::Done
}

async fn architect(e: &Em, text: &str) -> Outcome {
    step!(e.think("**Reading the current pattern**").await);
    let cur = e.tool("mantra_read_pattern", json!({})).await;
    let ask = crate::util::trunc(text, 60);
    step!(e.think(&format!("**Designing the change**\n\nThe user asked: \"{ask}\". I'll add a documentation role and run it at the very end.")).await);
    let new = format!("{cur}\n[roles.docs]\nkind = \"gate\"\nglyph = \"■\"\ncolor = \"blue\"\nmodel = \"luna\"\neffort = \"medium\"\nsandbox = \"workspace-write\"\ndescription = \"Writes and updates documentation\"\ninstructions = \"You keep README and docs accurate and concise.\"\n\n[[flow.finale]]\nrole = \"docs\"\ntask = \"Update the README and docs to match what was built.\"\nmay_spawn = false\n");
    let r = e.tool("mantra_write_pattern", json!({"toml": new})).await;
    if r.contains("INVALID") {
        step!(e.say(&format!("The pattern didn't validate: {r}")).await);
    } else {
        step!(e.say("Added a **docs** role (■ luna/medium) and a final `docs` step after verification.").await);
    }
    Outcome::Done
}

/// Shared with `mock_claude.rs` (WP10.6).
pub(crate) fn camel(s: &str) -> String {
    s.split(|c: char| !c.is_ascii_alphanumeric()).filter(|p| !p.is_empty()).map(|p| {
        let mut c = p.chars();
        c.next().map(|f| f.to_ascii_uppercase().to_string() + c.as_str()).unwrap_or_default()
    }).collect()
}
