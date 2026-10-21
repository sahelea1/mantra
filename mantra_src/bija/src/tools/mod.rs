//! The tool runtime (SPEC §6): built-in tool definitions, dispatch, sandbox policy, background jobs.
//!
//! `Runtime` is the one object the agent talks to. It owns the decisions made once at start (the
//! working directory, the path policy, whether bwrap works, which shell and helper programs exist)
//! and turns every call into a `ToolOutcome` — a malformed argument, a refused path or a command
//! that cannot start all come back as `ok: false` text the model can act on, never as a panic or
//! an aborted turn. The file tools live in `fs.rs`, commands and jobs in `shell.rs`, the policy and
//! the wrapper in `sandbox.rs`; `args.rs` shapes every call's arguments to its schema first.

pub mod args;
pub mod fs;
pub mod sandbox;
pub mod shell;

use crate::types::{absolute, ToolDef, ToolOutcome};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedSender;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sandbox {
    ReadOnly,
    WorkspaceWrite,
    Full,
}

impl Sandbox {
    pub fn parse(s: &str) -> Option<Sandbox> {
        match s.trim().to_ascii_lowercase().replace('_', "-").as_str() {
            "read-only" | "readonly" | "ro" => Some(Sandbox::ReadOnly),
            "workspace-write" | "workspace" | "rw" => Some(Sandbox::WorkspaceWrite),
            "full" | "danger-full-access" | "none" => Some(Sandbox::Full),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Sandbox::ReadOnly => "read-only",
            Sandbox::WorkspaceWrite => "workspace-write",
            Sandbox::Full => "full",
        }
    }
}

#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    pub cwd: PathBuf,
    pub sandbox: Sandbox,
    pub writable: Vec<PathBuf>,
    pub network: bool,
    pub no_bwrap: bool,
    /// `<session dir>/jobs` — background job logs.
    pub jobs_dir: PathBuf,
    /// Variables removed from every command's environment: the ones holding bija's API key. A
    /// command that could print the key would put it in the transcript, the events and the next
    /// request.
    pub scrub_env: Vec<String>,
}

/// Every built-in, in the order the model sees them.
const BUILTINS: [&str; 9] = ["read_file", "write_file", "edit_file", "delete", "list_dir", "glob", "grep", "bash", "job"];
/// The tools read-only mode neither offers nor runs.
const WRITE_TOOLS: [&str; 3] = ["write_file", "edit_file", "delete"];

const BASH_TIMEOUT_DEFAULT: u64 = 120;
const JOB_WAIT_DEFAULT: u64 = 300;
const TIMEOUT_MAX: u64 = 1800;

/// `name` on PATH (or `name` itself when it is a path), if it is an executable file.
pub fn which(name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let executable = |p: &Path| std::fs::metadata(p).map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false);
    if name.contains('/') {
        let p = PathBuf::from(name);
        return executable(&p).then_some(p);
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|dir| dir.join(name)).find(|p| executable(p))
}

/// A tool call's arguments, read leniently: models send `"10"` for 10 and `"true"` for true often
/// enough that refusing them would only cost a round trip. Errors name the argument and are
/// prefixed with the tool name by the dispatcher.
pub struct Args<'a> {
    map: Option<&'a Map<String, Value>>,
}

impl<'a> Args<'a> {
    /// `null` (no or unparsable arguments) reads as an empty object, so the model is told which
    /// argument is missing rather than that its JSON was wrong in some unspecified way.
    pub fn new(v: &'a Value) -> Result<Args<'a>, String> {
        match v {
            Value::Object(m) => Ok(Args { map: Some(m) }),
            Value::Null => Ok(Args { map: None }),
            _ => Err("the arguments must be a JSON object".into()),
        }
    }

    fn get(&self, key: &str) -> Option<&'a Value> {
        self.map.and_then(|m| m.get(key)).filter(|v| !v.is_null())
    }

    /// A required string, possibly empty (file content, a replacement).
    pub fn text(&self, key: &str) -> Result<&'a str, String> {
        match self.get(key) {
            Some(Value::String(s)) => Ok(s),
            Some(_) => Err(format!("`{key}` must be a string")),
            None => Err(format!("`{key}` is required")),
        }
    }

    /// A required, non-blank string (a path, a pattern).
    pub fn path(&self, key: &str) -> Result<&'a str, String> {
        let s = self.text(key)?;
        if s.trim().is_empty() {
            return Err(format!("`{key}` is empty"));
        }
        Ok(s)
    }

    pub fn opt_text(&self, key: &str) -> Result<Option<&'a str>, String> {
        match self.get(key) {
            None => Ok(None),
            Some(_) => self.text(key).map(Some),
        }
    }

    pub fn opt_u64(&self, key: &str) -> Result<Option<u64>, String> {
        let bad = || format!("`{key}` must be a non-negative integer");
        match self.get(key) {
            None => Ok(None),
            Some(Value::Number(n)) => match (n.as_u64(), n.as_f64()) {
                (Some(u), _) => Ok(Some(u)),
                (None, Some(f)) if f >= 0.0 && f.fract() == 0.0 && f <= u64::MAX as f64 => Ok(Some(f as u64)),
                _ => Err(bad()),
            },
            Some(Value::String(s)) => s.trim().parse::<u64>().map(Some).map_err(|_| bad()),
            Some(_) => Err(bad()),
        }
    }

    pub fn opt_bool(&self, key: &str) -> Result<Option<bool>, String> {
        match self.get(key) {
            None => Ok(None),
            Some(Value::Bool(b)) => Ok(Some(*b)),
            Some(Value::String(s)) if s.trim().eq_ignore_ascii_case("true") => Ok(Some(true)),
            Some(Value::String(s)) if s.trim().eq_ignore_ascii_case("false") => Ok(Some(false)),
            Some(_) => Err(format!("`{key}` must be true or false")),
        }
    }
}

fn def(name: &str, description: &str, parameters: Value) -> ToolDef {
    ToolDef { name: name.into(), description: description.into(), parameters }
}

/// The definition of one built-in: what the model reads, so every constraint is stated here.
fn builtin_def(name: &str) -> Option<ToolDef> {
    let path = json!({"type": "string", "description": "Path, relative to the working directory or absolute."});
    let d = match name {
        "read_file" => def(
            name,
            "Read a text file as numbered lines (`cat -n` style). At most 2000 lines / 64 KiB per call; page through longer files with offset and limit. Binary files report their size only.",
            json!({"type": "object", "properties": {
                "path": path,
                "offset": {"type": "integer", "description": "1-based line to start at (default 1)."},
                "limit": {"type": "integer", "description": "Number of lines to return (default 2000)."}
            }, "required": ["path"]}),
        ),
        "write_file" => def(
            name,
            "Create or overwrite a file with exactly `content`; missing parent directories are created. To change part of an existing file use edit_file.",
            json!({"type": "object", "properties": {
                "path": path,
                "content": {"type": "string", "description": "The complete new file content."}
            }, "required": ["path", "content"]}),
        ),
        "edit_file" => def(
            name,
            "Replace text in a file. `old` must match the file exactly — whitespace and indentation included — and must occur exactly once unless replace_all is true; include enough surrounding lines to make it unique. Read the file first.",
            json!({"type": "object", "properties": {
                "path": path,
                "old": {"type": "string", "description": "The exact text to replace."},
                "new": {"type": "string", "description": "The replacement text."},
                "replace_all": {"type": "boolean", "description": "Replace every occurrence (default false)."}
            }, "required": ["path", "old", "new"]}),
        ),
        "delete" => def(
            name,
            "Delete a file, a symlink or an empty directory. A non-empty directory needs recursive: true.",
            json!({"type": "object", "properties": {
                "path": path,
                "recursive": {"type": "boolean", "description": "Delete a directory and everything in it (default false)."}
            }, "required": ["path"]}),
        ),
        "list_dir" => def(
            name,
            "List a directory: `name/` for directories, sizes for files. depth 1-4 (default 1); .git, target and node_modules are listed but not expanded.",
            json!({"type": "object", "properties": {
                "path": {"type": "string", "description": "Directory (default: the working directory)."},
                "depth": {"type": "integer", "description": "Levels to show, 1-4 (default 1)."}
            }}),
        ),
        "glob" => def(
            name,
            "Find files by path pattern: `*` and `?` within a name, `**` across directories, `[abc]` sets, `{a,b}` alternatives — e.g. `src/**/*.rs`. At most 500 results; .git is skipped, target and node_modules only when the pattern names them.",
            json!({"type": "object", "properties": {
                "pattern": {"type": "string", "description": "Glob pattern, relative to `path`."},
                "path": {"type": "string", "description": "Directory to search from (default: the working directory)."}
            }, "required": ["pattern"]}),
        ),
        "grep" => def(
            name,
            "Search file contents with a regular expression (ripgrep syntax; POSIX extended when ripgrep is missing). Returns `path:line: text`, at most `max` results (default 200). Skips binary files and the .git, target and node_modules directories (name one in path to search inside it), and gitignored files when ripgrep is used.",
            json!({"type": "object", "properties": {
                "pattern": {"type": "string", "description": "Regular expression."},
                "path": {"type": "string", "description": "File or directory to search (default: the working directory)."},
                "glob": {"type": "string", "description": "Only search files whose name matches, e.g. `*.rs`."},
                "max": {"type": "integer", "description": "Maximum results (default 200)."}
            }, "required": ["pattern"]}),
        ),
        "bash" => def(
            name,
            "Run a shell command in the working directory; stdin is closed, so it must not wait for input. stdout and stderr are merged and long output is truncated to its last 48 KiB. Killed after timeout_secs (default 120, max 1800). For servers, watchers and long builds use background: true and then the job tool.",
            json!({"type": "object", "properties": {
                "command": {"type": "string", "description": "The command line."},
                "timeout_secs": {"type": "integer", "description": "Seconds before the command is killed (default 120, max 1800)."},
                "background": {"type": "boolean", "description": "Start it as a background job and return its id at once (default false)."}
            }, "required": ["command"]}),
        ),
        "job" => def(
            name,
            "Manage background commands started by bash with background: true. wait: block until the job exits (up to timeout_secs, default 300) and return its exit code and output tail; output: the output tail so far; kill: stop it; list: all jobs.",
            json!({"type": "object", "properties": {
                "action": {"type": "string", "enum": ["wait", "output", "kill", "list"]},
                "id": {"type": "integer", "description": "Job id (optional when there is only one job)."},
                "timeout_secs": {"type": "integer", "description": "For wait: seconds to wait at most (default 300, max 1800)."}
            }, "required": ["action"]}),
        ),
        _ => return None,
    };
    Some(d)
}

pub struct Runtime {
    pub cfg: RuntimeConfig,
    fs: fs::Fs,
    isolation: sandbox::Isolation,
    shell: PathBuf,
    jobs: shell::Jobs,
}

impl Runtime {
    /// Settles everything that is decided once: the absolute cwd, the policy, the bwrap probe (only
    /// when a sandbox is asked for), the shell, and whether `diff` and `rg` exist.
    pub async fn new(cfg: RuntimeConfig) -> Runtime {
        let mut cfg = cfg;
        cfg.cwd = sandbox::normalize(&absolute(&cfg.cwd));
        let policy = sandbox::Policy::new(cfg.sandbox, &cfg.cwd, &cfg.writable);
        let isolation = sandbox::Isolation::detect(&policy, cfg.no_bwrap, cfg.network).await;
        let fs = fs::Fs { cwd: cfg.cwd.clone(), policy, diff: which("diff").is_some(), rg: which("rg") };
        let jobs = shell::Jobs::new(cfg.jobs_dir.clone());
        Runtime { cfg, fs, isolation, shell: shell::default_shell(), jobs }
    }

    /// The built-in tools offered to the model (write tools left out in read-only).
    pub fn defs(&self) -> Vec<ToolDef> {
        let read_only = self.fs.policy.read_only();
        BUILTINS.iter().filter(|n| !(read_only && WRITE_TOOLS.contains(n))).filter_map(|n| builtin_def(n)).collect()
    }

    /// True for every built-in name, offered or not: a write tool called by name in read-only mode
    /// gets a clear refusal from `call`, not "unknown tool".
    pub fn is_builtin(&self, name: &str) -> bool {
        BUILTINS.contains(&name)
    }

    /// Runs one built-in tool. `progress` gets streamed command output chunks. Cancel-safe: dropping
    /// the future kills a running command's process group.
    pub async fn call(&mut self, name: &str, args: &Value, progress: Option<UnboundedSender<String>>) -> ToolOutcome {
        let started = Instant::now();
        let mut out = match self.dispatch(name, args, progress.as_ref()).await {
            Ok(o) => o,
            Err(e) => ToolOutcome::err(format!("{name}: {e}")),
        };
        out.duration_ms = started.elapsed().as_millis() as u64;
        out
    }

    async fn dispatch(&mut self, name: &str, args: &Value, progress: Option<&UnboundedSender<String>>) -> Result<ToolOutcome, String> {
        let a = Args::new(args)?;
        if self.fs.policy.read_only() && WRITE_TOOLS.contains(&name) {
            return Err("not available: the sandbox is read-only (files can be read, not written)".into());
        }
        match name {
            "read_file" => self.fs.read_file(&a),
            "write_file" => self.fs.write_file(&a).await,
            "edit_file" => self.fs.edit_file(&a).await,
            "delete" => self.fs.delete(&a),
            "list_dir" => self.fs.list_dir(&a),
            "glob" => self.fs.glob(&a),
            "grep" => self.fs.grep(&a).await,
            "bash" => self.bash(&a, progress).await,
            "job" => self.job(&a).await,
            _ => Err(format!("unknown tool; the built-in tools are {}", BUILTINS.join(", "))),
        }
    }

    async fn bash(&mut self, a: &Args<'_>, progress: Option<&UnboundedSender<String>>) -> Result<ToolOutcome, String> {
        let command = a.path("command")?;
        let timeout = Duration::from_secs(a.opt_u64("timeout_secs")?.unwrap_or(BASH_TIMEOUT_DEFAULT).clamp(1, TIMEOUT_MAX));
        let cmd = self.command(command, true);
        if a.opt_bool("background")?.unwrap_or(false) {
            let (id, log) = self.jobs.start(cmd, command)?;
            return Ok(ToolOutcome::ok(json!({"job": id, "log": log.display().to_string()}).to_string()));
        }
        let captured = shell::run(cmd, timeout, progress).await?;
        Ok(shell::outcome(captured, timeout))
    }

    async fn job(&mut self, a: &Args<'_>) -> Result<ToolOutcome, String> {
        let action = a.path("action")?;
        let id = a.opt_u64("id")?;
        match action.trim() {
            "wait" => {
                let timeout = a.opt_u64("timeout_secs")?.unwrap_or(JOB_WAIT_DEFAULT).clamp(1, TIMEOUT_MAX);
                self.jobs.wait(id, Duration::from_secs(timeout)).await
            }
            "output" => self.jobs.output(id),
            "kill" => self.jobs.kill(id).await,
            "list" => Ok(self.jobs.list()),
            other => Err(format!("unknown action `{other}`; use wait, output, kill or list")),
        }
    }

    /// `Op::Shell`: the user's own command, never sandboxed, same streaming.
    pub async fn shell(&mut self, command: &str, progress: Option<UnboundedSender<String>>) -> ToolOutcome {
        let started = Instant::now();
        let timeout = Duration::from_secs(TIMEOUT_MAX);
        let mut out = match shell::run(self.command(command, false), timeout, progress.as_ref()).await {
            Ok(captured) => shell::outcome(captured, timeout),
            Err(e) => ToolOutcome::err(e),
        };
        out.duration_ms = started.elapsed().as_millis() as u64;
        out
    }

    /// `shell -c script` in the working directory — wrapped when `sandboxed` and bwrap is in use —
    /// without bija's own credentials in its environment.
    fn command(&self, script: &str, sandboxed: bool) -> tokio::process::Command {
        let mut cmd = if sandboxed { self.isolation.command(&self.shell, script, &self.fs.cwd) } else { shell::plain(&self.shell, script, &self.fs.cwd) };
        for name in &self.cfg.scrub_env {
            cmd.env_remove(name);
        }
        cmd
    }

    /// Number of background jobs still running (for `Event::Status`).
    pub fn running_jobs(&self) -> usize {
        self.jobs.running()
    }

    /// Lines for the system prompt preamble: cwd, sandbox mode, whether commands are wrapped.
    pub fn system_notes(&self) -> String {
        let mode = match self.cfg.sandbox {
            Sandbox::ReadOnly => "Sandbox: read-only — files can be read but not written (write_file, edit_file and delete are unavailable).".to_string(),
            Sandbox::WorkspaceWrite => format!("Sandbox: workspace-write — file tools may write only under {}.", self.fs.policy.roots_display()),
            Sandbox::Full => "Sandbox: full access.".to_string(),
        };
        format!("Working directory: {}\n{mode}\n{}", self.cfg.cwd.display(), self.isolation.note())
    }

    /// A warning to emit once at start, if any ("bwrap unavailable — commands run unsandboxed").
    pub fn startup_warning(&self) -> Option<String> {
        self.isolation.warning()
    }

    /// Ends every background job and leftover command group (SIGTERM, a 1 s grace, SIGKILL);
    /// called at shutdown, once the session is on disk.
    pub async fn shutdown(&mut self) {
        self.jobs.shutdown().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::sandbox::{Isolation, Plain, Policy};

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bija-rt-{tag}-{}", crate::types::uuid_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    async fn runtime(dir: &Path, sandbox: Sandbox) -> Runtime {
        let cfg = RuntimeConfig { cwd: dir.to_path_buf(), sandbox, writable: vec![], network: false, no_bwrap: true, jobs_dir: dir.join(".jobs"), scrub_env: vec!["BIJA_RT_TEST_KEY".into()] };
        Runtime::new(cfg).await
    }

    fn names(rt: &Runtime) -> Vec<String> {
        rt.defs().into_iter().map(|d| d.name).collect()
    }

    #[test]
    fn args_are_read_leniently_and_errors_name_the_argument() {
        let v = json!({"n": "12", "f": 3.0, "b": "TRUE", "s": 5, "neg": -1});
        let a = Args::new(&v).unwrap();
        assert_eq!(a.opt_u64("n").unwrap(), Some(12));
        assert_eq!(a.opt_u64("f").unwrap(), Some(3));
        assert_eq!(a.opt_bool("b").unwrap(), Some(true));
        assert_eq!(a.opt_u64("missing").unwrap(), None);
        assert_eq!(a.text("s").unwrap_err(), "`s` must be a string");
        assert!(a.opt_u64("neg").is_err());
        assert_eq!(Args::new(&Value::Null).unwrap().path("path").unwrap_err(), "`path` is required");
        assert!(Args::new(&json!([1])).is_err());
    }

    #[tokio::test]
    async fn every_offered_tool_has_an_object_schema() {
        let d = scratch("defs");
        let rt = runtime(&d, Sandbox::WorkspaceWrite).await;
        assert_eq!(names(&rt), BUILTINS.to_vec());
        for t in rt.defs() {
            assert_eq!(t.parameters["type"], "object", "{}", t.name);
            assert!(!t.description.is_empty() && rt.is_builtin(&t.name));
        }
        assert!(!rt.is_builtin("web_search"));
    }

    #[tokio::test]
    async fn read_only_omits_and_refuses_the_write_tools() {
        let d = scratch("ro");
        let mut rt = runtime(&d, Sandbox::ReadOnly).await;
        let offered = names(&rt);
        assert!(WRITE_TOOLS.iter().all(|w| !offered.contains(&w.to_string())), "{offered:?}");
        assert!(offered.contains(&"bash".to_string()) && offered.contains(&"read_file".to_string()));
        for w in WRITE_TOOLS {
            assert!(rt.is_builtin(w));
            let out = rt.call(w, &json!({"path": "x.txt", "content": "x", "old": "a", "new": "b"}), None).await;
            assert!(!out.ok && out.text.contains("read-only"), "{w}: {}", out.text);
        }
        assert!(!d.join("x.txt").exists());
        assert!(rt.system_notes().contains("read-only"));
    }

    #[tokio::test]
    async fn writes_outside_the_roots_are_refused_and_writable_roots_allowed() {
        let ws = scratch("ws");
        let extra = scratch("extra");
        let third = scratch("third");
        let mut rt = runtime(&ws, Sandbox::WorkspaceWrite).await;
        // Every scratch dir lives under /tmp, which is always a root; leave it out to test the rule.
        rt.fs.policy = Policy::with_roots(Sandbox::WorkspaceWrite, vec![ws.clone(), extra.clone()]);
        let target = third.join("f.txt");
        let out = rt.call("write_file", &json!({"path": target.display().to_string(), "content": "x"}), None).await;
        assert!(!out.ok && out.text.contains("refused") && out.text.contains(&target.display().to_string()) && out.text.contains(&ws.display().to_string()), "{}", out.text);
        assert!(!target.exists());
        std::fs::write(&target, "a").unwrap();
        let edit = rt.call("edit_file", &json!({"path": target.display().to_string(), "old": "a", "new": "b"}), None).await;
        assert!(!edit.ok && edit.text.contains("refused"), "{}", edit.text);
        let del = rt.call("delete", &json!({"path": target.display().to_string()}), None).await;
        assert!(!del.ok && target.exists());
        let ok = rt.call("write_file", &json!({"path": extra.join("y.txt").display().to_string(), "content": "y"}), None).await;
        assert!(ok.ok, "{}", ok.text);
        assert!(extra.join("y.txt").exists());
        let rel = rt.call("write_file", &json!({"path": "../escape.txt", "content": "x"}), None).await;
        assert!(!rel.ok, "a relative path climbing out is refused too: {}", rel.text);
    }

    #[tokio::test]
    async fn the_real_policy_includes_writable_roots_and_tmp() {
        let ws = scratch("cfg");
        let cfg = RuntimeConfig { cwd: ws.clone(), sandbox: Sandbox::WorkspaceWrite, writable: vec![PathBuf::from("/opt/bija-extra")], network: false, no_bwrap: true, jobs_dir: ws.join("jobs"), scrub_env: vec![] };
        let rt = Runtime::new(cfg).await;
        let notes = rt.system_notes();
        assert!(notes.contains(&ws.display().to_string()) && notes.contains("/opt/bija-extra") && notes.contains("/tmp"), "{notes}");
        assert!(rt.fs.policy.check_write(Path::new("/opt/bija-extra/a"), true).is_ok());
        assert!(rt.fs.policy.check_write(Path::new("/usr/a"), true).is_err());
    }

    #[tokio::test]
    async fn bad_arguments_never_panic() {
        let d = scratch("bad");
        let mut rt = runtime(&d, Sandbox::WorkspaceWrite).await;
        for name in BUILTINS {
            for args in [Value::Null, json!([]), json!({"path": 3, "command": false, "action": 1, "pattern": {}})] {
                if name == "list_dir" && args.is_null() {
                    continue; // every list_dir argument is optional
                }
                let out = rt.call(name, &args, None).await;
                assert!(!out.ok && out.text.starts_with(&format!("{name}: ")), "{name} {args}: {}", out.text);
            }
        }
        let unknown = rt.call("nope", &json!({}), None).await;
        assert!(!unknown.ok && unknown.text.contains("read_file"));
        let action = rt.call("job", &json!({"action": "explode"}), None).await;
        assert!(action.text.contains("wait, output, kill or list"));
    }

    #[tokio::test]
    async fn bash_reports_exit_code_and_merged_output() {
        let d = scratch("bash");
        let mut rt = runtime(&d, Sandbox::WorkspaceWrite).await;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let out = rt.call("bash", &json!({"command": "pwd; echo oops >&2; exit 7"}), Some(tx)).await;
        assert_eq!(out.exit_code, Some(7));
        assert!(!out.ok && out.text.contains(&d.display().to_string()) && out.text.contains("oops") && out.text.ends_with("[exit code 7]"), "{}", out.text);
        let mut streamed = String::new();
        while let Ok(chunk) = rx.try_recv() {
            streamed.push_str(&chunk);
        }
        assert!(streamed.contains("oops"));
        let ok = rt.call("bash", &json!({"command": "true"}), None).await;
        assert!(ok.ok && ok.text == "(no output)\n[exit code 0]", "{}", ok.text);
    }

    #[tokio::test]
    async fn bash_timeout_kills_the_command() {
        let d = scratch("timeout");
        let mut rt = runtime(&d, Sandbox::WorkspaceWrite).await;
        let started = Instant::now();
        let out = rt.call("bash", &json!({"command": "echo $$; exec sleep 30", "timeout_secs": 1}), None).await;
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(!out.ok && out.text.contains("timed out after 1s") && out.exit_code.is_none(), "{}", out.text);
        let pid: i32 = out.text.lines().next().unwrap().trim().parse().unwrap();
        // We reaped it ourselves, so even a plain kill -0 must fail now.
        assert_ne!(unsafe { libc::kill(pid, 0) }, 0, "sleep {pid} is still there");
    }

    #[tokio::test]
    async fn bash_output_is_tail_truncated_with_a_note() {
        let d = scratch("trunc");
        let mut rt = runtime(&d, Sandbox::WorkspaceWrite).await;
        let out = rt.call("bash", &json!({"command": "seq 1 20000"}), None).await;
        assert!(out.ok);
        let first = out.text.lines().next().unwrap();
        assert!(first.starts_with("[… ") && first.ends_with(" lines truncated]"), "{first}");
        assert!(out.text.ends_with("19999\n20000\n[exit code 0]"));
        assert!(out.text.len() <= shell::OUTPUT_CAP + 100, "{}", out.text.len());
        // The line after the note is a whole line, not the tail end of one.
        let second: u32 = out.text.lines().nth(1).unwrap().parse().unwrap();
        let dropped: u32 = first.trim_start_matches("[… ").trim_end_matches(" lines truncated]").parse().unwrap();
        assert_eq!(second, dropped + 1);
    }

    #[tokio::test]
    async fn background_jobs_wait_output_list_and_kill() {
        let d = scratch("jobs");
        let mut rt = runtime(&d, Sandbox::WorkspaceWrite).await;
        let started = rt.call("bash", &json!({"command": "echo begin; sleep 0.3; echo end; exit 4", "background": true}), None).await;
        let v: Value = serde_json::from_str(&started.text).unwrap();
        assert_eq!(v["job"], 1);
        assert!(v["log"].as_str().unwrap().ends_with(".jobs/1.log"));
        assert_eq!(rt.running_jobs(), 1);
        let waited = rt.call("job", &json!({"action": "wait", "id": 1, "timeout_secs": 10}), None).await;
        assert_eq!(waited.exit_code, Some(4));
        assert!(waited.text.contains("exited with code 4") && waited.text.contains("begin\nend"), "{}", waited.text);
        assert_eq!(rt.running_jobs(), 0);

        let long = rt.call("bash", &json!({"command": "echo $$; exec sleep 30", "background": true}), None).await;
        assert!(long.text.contains("\"job\":2"), "{}", long.text);
        tokio::time::sleep(Duration::from_millis(200)).await;
        let early = rt.call("job", &json!({"action": "wait", "id": "2", "timeout_secs": 1}), None).await;
        assert!(early.ok && early.text.contains("still running"), "{}", early.text);
        let output = rt.call("job", &json!({"action": "output", "id": 2}), None).await;
        let pid: i32 = output.text.lines().nth(1).unwrap().trim().parse().unwrap();
        let list = rt.call("job", &json!({"action": "list"}), None).await.text;
        assert!(list.contains("1  exited with code 4") && list.contains("2  running"), "{list}");
        let need_id = rt.call("job", &json!({"action": "kill"}), None).await;
        assert!(!need_id.ok && need_id.text.contains("`id` is required"), "{}", need_id.text);
        let killed = rt.call("job", &json!({"action": "kill", "id": 2}), None).await;
        assert!(killed.ok && killed.text.contains("killed"), "{}", killed.text);
        assert!(shell::gone_within(pid, Duration::from_secs(3)).await, "job pid {pid} survived kill");
        assert_eq!(rt.running_jobs(), 0);
        let again = rt.call("job", &json!({"action": "kill", "id": 2}), None).await;
        assert!(again.text.contains("already exited"));
    }

    #[tokio::test]
    async fn shutdown_stops_running_jobs() {
        let d = scratch("shutdown");
        let mut rt = runtime(&d, Sandbox::WorkspaceWrite).await;
        rt.call("bash", &json!({"command": "sleep 30", "background": true}), None).await;
        rt.call("bash", &json!({"command": "trap '' TERM; sleep 30", "background": true}), None).await;
        assert_eq!(rt.running_jobs(), 2);
        let started = Instant::now();
        rt.shutdown().await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(rt.running_jobs(), 0, "a job that ignores SIGTERM is killed after the grace");
        assert!(started.elapsed() < Duration::from_secs(4));
    }

    #[tokio::test]
    async fn dropping_a_running_call_kills_its_process_group() {
        let d = scratch("drop");
        let mut rt = runtime(&d, Sandbox::WorkspaceWrite).await;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let args = json!({"command": "echo $$; exec sleep 30"});
        let dropped = tokio::time::timeout(Duration::from_millis(800), rt.call("bash", &args, Some(tx))).await;
        assert!(dropped.is_err(), "the call should still have been running");
        let mut streamed = String::new();
        while let Ok(chunk) = rx.try_recv() {
            streamed.push_str(&chunk);
        }
        let pid: i32 = streamed.trim().parse().unwrap();
        // Nobody reaps the dropped child promptly, so a zombie counts as gone.
        assert!(shell::gone_within(pid, Duration::from_secs(3)).await, "sleep {pid} outlived the dropped call");
    }

    #[tokio::test]
    async fn without_bwrap_commands_run_plainly_and_the_warning_is_given() {
        let d = scratch("nobwrap");
        let mut rt = runtime(&d, Sandbox::WorkspaceWrite).await;
        rt.isolation = Isolation::with_program(PathBuf::from("/bin/false"), &rt.fs.policy, false).await;
        assert_eq!(rt.isolation, Isolation::Plain(Plain::Unavailable));
        assert_eq!(rt.startup_warning().as_deref(), Some(sandbox::UNAVAILABLE_WARNING));
        assert!(rt.system_notes().contains("unsandboxed"));
        let out = rt.call("bash", &json!({"command": "echo plain"}), None).await;
        assert!(out.ok && out.text.starts_with("plain"), "{}", out.text);
        // --no-bwrap is a choice: notes say so, no warning.
        let chosen = runtime(&d, Sandbox::WorkspaceWrite).await;
        assert_eq!(chosen.startup_warning(), None);
        assert!(chosen.system_notes().contains("bwrap disabled"));
    }

    /// A runtime whose commands really run under bwrap; `None`, saying why, where bwrap is missing
    /// or cannot create its namespaces, so the tests that need one skip instead of failing.
    async fn bwrap_runtime(dir: &Path) -> Option<Runtime> {
        if which("bwrap").is_none() {
            eprintln!("skipped: bwrap is not installed");
            return None;
        }
        let cfg = RuntimeConfig { cwd: dir.to_path_buf(), sandbox: Sandbox::WorkspaceWrite, writable: vec![], network: false, no_bwrap: false, jobs_dir: dir.join(".jobs"), scrub_env: vec![] };
        let rt = Runtime::new(cfg).await;
        if !matches!(rt.isolation, Isolation::Bwrap { .. }) {
            eprintln!("skipped: bwrap is installed but cannot create its namespaces here");
            return None;
        }
        Some(rt)
    }

    /// Whether a process whose command line contains `marker` is still alive (pids inside bwrap's
    /// namespace mean nothing out here, so the command lines are the handle).
    fn running(marker: &str) -> bool {
        let Ok(dir) = std::fs::read_dir("/proc") else { return false };
        dir.flatten().filter_map(|e| std::fs::read(e.path().join("cmdline")).ok()).any(|raw| String::from_utf8_lossy(&raw).replace('\0', " ").contains(marker))
    }

    async fn ends_within(marker: &str, d: Duration) -> bool {
        let until = Instant::now() + d;
        while running(marker) && Instant::now() < until {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        !running(marker)
    }

    /// Inside bwrap the command is in a session and a pid namespace of its own, out of reach of a
    /// `killpg` of bwrap's group. Every way a command ends early must still take down everything
    /// it forked: the namespace goes with bwrap.
    #[tokio::test]
    async fn under_bwrap_every_early_end_takes_the_whole_command_down() {
        let d = scratch("bwrap-kill");
        let Some(mut rt) = bwrap_runtime(&d).await else { return };
        let out = rt.call("bash", &json!({"command": "sleep 61.01 & sleep 61.02; wait", "timeout_secs": 1}), None).await;
        assert!(!out.ok && out.text.contains("timed out after 1s"), "{}", out.text);
        assert!(ends_within("sleep 61.0", Duration::from_secs(3)).await, "a timeout left sandboxed processes behind");

        let dropped = tokio::time::timeout(Duration::from_millis(800), rt.call("bash", &json!({"command": "sleep 62.01 & sleep 62.02; wait"}), None)).await;
        assert!(dropped.is_err(), "the call should still have been running");
        assert!(ends_within("sleep 62.0", Duration::from_secs(3)).await, "an interrupted call left sandboxed processes behind");

        rt.call("bash", &json!({"command": "sleep 63.01 & sleep 63.02; wait", "background": true}), None).await;
        let killed = rt.call("job", &json!({"action": "kill", "id": 1}), None).await;
        assert!(killed.ok && killed.text.contains("killed"), "{}", killed.text);
        assert!(ends_within("sleep 63.0", Duration::from_secs(3)).await, "job kill left sandboxed processes behind");

        // A SIGTERM-proof job inside the sandbox still ends at once: bwrap does not ignore it.
        rt.call("bash", &json!({"command": "trap '' TERM; sleep 64.01 & sleep 64.02; wait", "background": true}), None).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        let started = Instant::now();
        rt.shutdown().await;
        assert!(ends_within("sleep 64.0", Duration::from_secs(3)).await, "shutdown left sandboxed processes behind");
        assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
    }

    /// The key bija calls the API with never reaches a command: the model's (sandboxed or not), a
    /// background job's, or the user's own `$ shell`. (The e2e test checks it in a real process.)
    #[tokio::test]
    async fn commands_never_see_the_api_key_variable() {
        let d = scratch("scrub");
        let rt = runtime(&d, Sandbox::WorkspaceWrite).await;
        for sandboxed in [true, false] {
            let cmd = rt.command("env", sandboxed);
            let removed: Vec<_> = cmd.as_std().get_envs().filter(|(_, v)| v.is_none()).map(|(k, _)| k.to_string_lossy().into_owned()).collect();
            assert_eq!(removed, ["BIJA_RT_TEST_KEY"], "sandboxed={sandboxed}");
        }
    }

    #[tokio::test]
    async fn the_users_shell_runs_unsandboxed_with_streaming() {
        let d = scratch("usershell");
        let mut rt = runtime(&d, Sandbox::ReadOnly).await;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let out = rt.shell("echo hi > made.txt && cat made.txt", Some(tx)).await;
        assert!(out.ok && out.text.starts_with("hi") && d.join("made.txt").exists(), "{}", out.text);
        assert_eq!(rx.recv().await.as_deref(), Some("hi\n"));
    }
}
