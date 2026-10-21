//! Commands (SPEC §6.1 `bash`, `job`): one shell per call in its own process group, stdout and stderr
//! merged through a single pipe (so the model reads them interleaved exactly as a terminal would),
//! streamed out in ≤ 4 KiB chunks and kept as a 48 KiB tail — the end of a build log is where the
//! error is. Every way a command can end early (timeout, interrupt, a dropped future, `job kill`)
//! goes through the process group: SIGTERM, a 2 s grace, then SIGKILL, so nothing it forked
//! outlives it. A group that may still have members afterwards — a command cut short, whose
//! SIGKILL waits on a thread that dies with bija, or one that exited leaving a `server &` behind —
//! is remembered, and `Jobs::shutdown` ends it with the jobs, all at once and with a shorter 1 s
//! grace: at shutdown the clock is the parent's, not the command's. Background jobs write to
//! `<session dir>/jobs/<n>.log` instead of a pipe, so their output survives bija itself and stays
//! readable with the file tools.

use crate::types::{clip, ToolOutcome};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::net::unix::pipe;
use tokio::process::{Child, Command};
use tokio::sync::{mpsc::UnboundedSender, watch};

/// Output kept per command (and shown per job tail): the last 48 KiB.
pub const OUTPUT_CAP: usize = 48 * 1024;
/// One read from the output pipe — also the largest chunk a `tool_output` event carries.
const READ_CHUNK: usize = 4096;
/// Between SIGTERM and SIGKILL for a command cut short (timeout, interrupt, `job kill`).
const KILL_GRACE: Duration = Duration::from_secs(2);
/// The same at shutdown, paid once for every job and leftover group, and short: Mantra kills bija
/// soon after asking it to stop, so what counts is that every group got its SIGTERM.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(1);
/// After the shell exits, how long to wait for the pipe to close. A `server &` left behind keeps
/// the write end open forever; past this the call returns with what it has.
const DRAIN_GRACE: Duration = Duration::from_millis(250);

/// The environment every command gets on top of bija's own: no pagers and no credential prompts,
/// since nothing can answer them (stdin is closed).
fn quiet(cmd: &mut Command) {
    cmd.env("PAGER", "cat").env("GIT_PAGER", "cat").env("GIT_TERMINAL_PROMPT", "0").stdin(Stdio::null()).process_group(0);
}

/// The exit code as a shell reports it: the code, or 128 + the signal that killed it.
pub fn code_of(status: ExitStatus) -> i32 {
    status.code().unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
}

/// Sends `sig` to the process group; false when the group is gone. Refuses pgid 0 and 1, which
/// would mean "our own group" and "everything" — a child that was already reaped has no id.
fn signal_group(pgid: i32, sig: i32) -> bool {
    pgid > 1 && unsafe { libc::killpg(pgid, sig) } == 0
}

/// Process groups that may outlive the call that started them (see the module doc).
static LEFTOVERS: Mutex<Vec<i32>> = Mutex::new(Vec::new());

/// Remembers `pgid` while it still has members. Groups that emptied are forgotten on the way, so a
/// pid reused much later is never mistaken for one of ours.
fn remember(pgid: i32) {
    if let Ok(mut groups) = LEFTOVERS.lock() {
        groups.retain(|g| signal_group(*g, 0));
        if signal_group(pgid, 0) && !groups.contains(&pgid) {
            groups.push(pgid);
        }
    }
}

/// The remembered groups that still have members, forgetting them all.
fn take_leftovers() -> Vec<i32> {
    let groups = LEFTOVERS.lock().map(|mut g| std::mem::take(&mut *g)).unwrap_or_default();
    groups.into_iter().filter(|g| signal_group(*g, 0)).collect()
}

fn pgid_of(child: &Child) -> Option<i32> {
    child.id().and_then(|pid| i32::try_from(pid).ok()).filter(|p| *p > 1)
}

/// SIGTERM now, SIGKILL after the grace for whatever is left of the group. Runs on a tiny thread
/// because it is called from `Drop`, where nothing can be awaited and the runtime may be shutting
/// down; the thread sleeps once rather than polling (a group that already went makes the SIGKILL
/// a no-op).
fn kill_group_detached(pgid: i32) {
    if !signal_group(pgid, libc::SIGTERM) {
        return;
    }
    // The thread dies with bija; a shutdown right after this must not leave the group behind.
    remember(pgid);
    let reaper = std::thread::Builder::new().name("bija-kill".into()).stack_size(64 * 1024).spawn(move || {
        std::thread::sleep(KILL_GRACE);
        signal_group(pgid, libc::SIGKILL);
    });
    if reaper.is_err() {
        signal_group(pgid, libc::SIGKILL);
    }
}

/// Kills a command's process group when the future running it is dropped (interrupt, cancel) —
/// the cancel-safety of `Runtime::call`. Disarmed once the command ended on its own terms.
struct GroupGuard {
    pgid: Option<i32>,
}

impl GroupGuard {
    fn disarm(&mut self) {
        self.pgid = None;
    }
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        if let Some(pgid) = self.pgid.take() {
            kill_group_detached(pgid);
        }
    }
}

/// SIGTERM the group, give it the grace, SIGKILL whatever is left; returns the shell's status.
async fn terminate(child: &mut Child, pgid: Option<i32>) -> Option<ExitStatus> {
    let Some(pgid) = pgid else { return child.wait().await.ok() };
    signal_group(pgid, libc::SIGTERM);
    let status = match tokio::time::timeout(KILL_GRACE, child.wait()).await {
        Ok(st) => st.ok(),
        Err(_) => {
            signal_group(pgid, libc::SIGKILL);
            child.wait().await.ok()
        }
    };
    // Members that ignored SIGTERM outlive their shell; the group id stays theirs until they go.
    signal_group(pgid, libc::SIGKILL);
    status
}

/// The last `cap` bytes of a stream, cut at a line boundary, counting what was dropped.
pub struct TailBuffer {
    buf: Vec<u8>,
    cap: usize,
    dropped_bytes: u64,
    dropped_lines: u64,
    /// Whether the dropped part ended at a line boundary (then the tail starts with a whole line).
    dropped_at_newline: bool,
    /// False once bytes were skipped unseen (`skipped`): then only the byte count is known.
    lines_counted: bool,
}

impl TailBuffer {
    pub fn new(cap: usize) -> TailBuffer {
        TailBuffer { buf: Vec::new(), cap, dropped_bytes: 0, dropped_lines: 0, dropped_at_newline: true, lines_counted: true }
    }

    /// `n` bytes before the first `push` were never read (a seek past the head of a log).
    fn skipped(&mut self, n: u64) {
        self.dropped_bytes += n;
        self.dropped_at_newline = false;
        self.lines_counted = false;
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
        // Trimming at 2× keeps the copy amortized instead of shifting the buffer on every chunk.
        if self.buf.len() > self.cap.saturating_mul(2) {
            self.drop_front(self.buf.len() - self.cap);
        }
    }

    fn drop_front(&mut self, n: usize) {
        let n = n.min(self.buf.len());
        if n == 0 {
            return;
        }
        let gone = &self.buf[..n];
        self.dropped_lines += gone.iter().filter(|b| **b == b'\n').count() as u64;
        self.dropped_bytes += n as u64;
        self.dropped_at_newline = gone.last() == Some(&b'\n');
        self.buf.drain(..n);
    }

    pub fn truncated(&self) -> bool {
        self.dropped_bytes > 0 || self.buf.len() > self.cap
    }

    /// The kept text, prefixed with `[… N lines truncated]` when anything was dropped.
    pub fn finish(mut self) -> String {
        if self.buf.len() > self.cap {
            self.drop_front(self.buf.len() - self.cap);
        }
        if self.dropped_bytes == 0 {
            return String::from_utf8_lossy(&self.buf).into_owned();
        }
        // A half line at the top reads as garbage; drop it too unless it is the only line there is.
        if !self.dropped_at_newline {
            if let Some(nl) = self.buf.iter().position(|b| *b == b'\n') {
                self.drop_front(nl + 1);
            }
        }
        let note = if self.lines_counted && self.dropped_lines > 0 {
            format!("[… {} lines truncated]\n", self.dropped_lines)
        } else {
            format!("[… {} bytes truncated]\n", self.dropped_bytes)
        };
        note + &String::from_utf8_lossy(&self.buf)
    }
}

/// Turns a byte stream into UTF-8 text chunks without splitting a character across two chunks.
#[derive(Default)]
struct Utf8Stream {
    carry: Vec<u8>,
}

impl Utf8Stream {
    fn decode(&mut self, bytes: &[u8]) -> String {
        self.carry.extend_from_slice(bytes);
        let keep = incomplete_tail(&self.carry);
        let cut = self.carry.len() - keep;
        let text = String::from_utf8_lossy(&self.carry[..cut]).into_owned();
        self.carry.drain(..cut);
        text
    }

    fn flush(&mut self) -> String {
        let text = String::from_utf8_lossy(&self.carry).into_owned();
        self.carry.clear();
        text
    }
}

/// How many trailing bytes start a UTF-8 character that is not complete yet.
fn incomplete_tail(b: &[u8]) -> usize {
    for i in 1..=b.len().min(3) {
        let byte = b[b.len() - i];
        if byte & 0xC0 == 0x80 {
            continue;
        }
        let need = match byte {
            0xF0..=0xFF => 4,
            0xE0..=0xEF => 3,
            0xC0..=0xDF => 2,
            _ => 1,
        };
        return if need > i { i } else { 0 };
    }
    0
}

/// What a finished command left behind.
pub struct Captured {
    pub output: String,
    /// The shell's exit code (128 + signal when killed); `None` when it could not be observed.
    pub exit: Option<i32>,
    pub timed_out: bool,
}

/// Where the output goes while it streams.
struct Sink<'a> {
    tail: TailBuffer,
    utf8: Utf8Stream,
    progress: Option<&'a UnboundedSender<String>>,
}

impl Sink<'_> {
    fn push(&mut self, bytes: &[u8]) {
        self.tail.push(bytes);
        if let Some(p) = self.progress {
            let text = self.utf8.decode(bytes);
            if !text.is_empty() {
                let _ = p.send(text);
            }
        }
    }

    fn finish(mut self) -> String {
        if let Some(p) = self.progress {
            let rest = self.utf8.flush();
            if !rest.is_empty() {
                let _ = p.send(rest);
            }
        }
        self.tail.finish()
    }
}

/// Reads whatever is still in the pipe for at most `budget` (after a kill, the last words of a
/// dying process are often the useful ones).
async fn drain(rx: &mut pipe::Receiver, sink: &mut Sink<'_>, budget: Duration) {
    let mut buf = [0u8; READ_CHUNK];
    let until = tokio::time::Instant::now() + budget;
    loop {
        match tokio::time::timeout_at(until, rx.read(&mut buf)).await {
            Ok(Ok(n)) if n > 0 => sink.push(&buf[..n]),
            _ => return,
        }
    }
}

/// Runs `cmd` to completion or `timeout`, merging stdout and stderr into one pipe. Cancel-safe:
/// dropping the future kills the command's process group. `Err` only when it could not start.
pub async fn run(mut cmd: Command, timeout: Duration, progress: Option<&UnboundedSender<String>>) -> Result<Captured, String> {
    let (tx, mut rx) = pipe::pipe().map_err(|e| format!("could not create a pipe: {e}"))?;
    let write = tx.into_blocking_fd().map_err(|e| format!("could not create a pipe: {e}"))?;
    let write_err = write.try_clone().map_err(|e| format!("could not create a pipe: {e}"))?;
    quiet(&mut cmd);
    cmd.stdout(Stdio::from(write)).stderr(Stdio::from(write_err));
    let spawned = cmd.spawn();
    // The command object still holds our copies of the write end; until they close, EOF never comes.
    drop(cmd);
    let mut child = spawned.map_err(|e| format!("could not start the shell: {e}"))?;
    let pgid = pgid_of(&child);
    let mut guard = GroupGuard { pgid };

    let mut sink = Sink { tail: TailBuffer::new(OUTPUT_CAP), utf8: Utf8Stream::default(), progress };
    let mut buf = [0u8; READ_CHUNK];
    let deadline = tokio::time::Instant::now() + timeout;
    let mut status: Option<ExitStatus> = None;
    let mut exited = false;
    let mut eof = false;
    let mut timed_out = false;
    let mut drain_until = deadline;

    while !(eof && exited) {
        tokio::select! {
            r = rx.read(&mut buf), if !eof => match r {
                Ok(n) if n > 0 => sink.push(&buf[..n]),
                _ => eof = true,
            },
            st = child.wait(), if !exited => {
                exited = true;
                status = st.ok();
                drain_until = drain_until.min(tokio::time::Instant::now() + DRAIN_GRACE);
            }
            _ = tokio::time::sleep_until(drain_until) => {
                if !exited {
                    timed_out = true;
                    status = terminate(&mut child, pgid).await;
                    drain(&mut rx, &mut sink, Duration::from_millis(100)).await;
                }
                break;
            }
        }
    }
    guard.disarm();
    if let Some(pgid) = pgid {
        // Anything still in the group was left behind on purpose (`server &`): it may run, but
        // not outlive bija.
        remember(pgid);
    }
    Ok(Captured { output: sink.finish(), exit: status.map(code_of), timed_out })
}

/// The tool result for a finished command: its output (or `(no output)`), then one bracketed line
/// with how it ended.
pub fn outcome(c: Captured, timeout: Duration) -> ToolOutcome {
    let mut text = if c.output.trim().is_empty() { "(no output)".to_string() } else { c.output.trim_end_matches('\n').to_string() };
    if c.timed_out {
        text.push_str(&format!("\n[timed out after {}s: the command was killed; run long commands with background: true]", timeout.as_secs()));
        return ToolOutcome::err(text);
    }
    match c.exit {
        Some(code) => {
            text.push_str(&format!("\n[exit code {code}]"));
            ToolOutcome { ok: code == 0, text, exit_code: Some(code), ..Default::default() }
        }
        None => {
            text.push_str("\n[the command's exit status could not be read]");
            ToolOutcome::err(text)
        }
    }
}

/// How a background job ended.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Finished {
    code: i32,
    after: Duration,
}

struct Job {
    id: u32,
    command: String,
    log: PathBuf,
    pgid: Option<i32>,
    started: Instant,
    /// Filled by the job's waiter task; the task owns the child, so it is reaped even if nobody asks.
    done: watch::Receiver<Option<Finished>>,
}

impl Job {
    fn finished(&self) -> Option<Finished> {
        *self.done.borrow()
    }

    fn state(&self) -> String {
        match self.finished() {
            Some(f) => format!("exited with code {} after {}", f.code, secs(f.after)),
            None => format!("running for {}", secs(self.started.elapsed())),
        }
    }

    /// The last 48 KiB of the log, with the truncation note pointing at the full file. Reads at
    /// most twice that from the end (enough to start on a whole line), however big the log is and
    /// however fast the job is still writing it: this runs on the event loop.
    fn tail(&self) -> String {
        let mut file = match std::fs::File::open(&self.log) {
            Ok(f) => f,
            Err(e) => return format!("(the log {} could not be read: {e})", self.log.display()),
        };
        let len = file.metadata().map(|m| m.len()).unwrap_or(0);
        let mut tail = TailBuffer::new(OUTPUT_CAP);
        let mut start = len.saturating_sub(2 * OUTPUT_CAP as u64);
        if start > 0 && file.seek(SeekFrom::Start(start)).is_ok() {
            tail.skipped(start);
        } else {
            start = 0;
        }
        let mut file = file.take(len - start);
        let mut buf = vec![0u8; 64 * 1024];
        while let Ok(n) = file.read(&mut buf) {
            if n == 0 {
                break;
            }
            tail.push(&buf[..n]);
        }
        let truncated = tail.truncated();
        let text = tail.finish();
        let text = text.trim_end_matches('\n');
        if text.is_empty() {
            "(no output yet)".into()
        } else if truncated {
            format!("{text}\n[full log: {}]", self.log.display())
        } else {
            text.to_string()
        }
    }
}

fn secs(d: Duration) -> String {
    format!("{:.1}s", d.as_secs_f64())
}

/// The session's background jobs. Ids count from 1 and continue past the logs a previous process
/// of the same session left in `jobs/`, so a resumed session never overwrites one.
pub struct Jobs {
    dir: PathBuf,
    next: u32,
    list: Vec<Job>,
}

impl Jobs {
    pub fn new(dir: PathBuf) -> Jobs {
        Jobs { dir, next: 0, list: Vec::new() }
    }

    pub fn running(&self) -> usize {
        self.list.iter().filter(|j| j.finished().is_none()).count()
    }

    fn first_free_id(&self) -> u32 {
        let highest = std::fs::read_dir(&self.dir)
            .map(|rd| rd.filter_map(|e| e.ok()).filter_map(|e| e.file_name().to_str().and_then(|n| n.strip_suffix(".log")).and_then(|n| n.parse::<u32>().ok())).max().unwrap_or(0))
            .unwrap_or(0);
        highest + 1
    }

    /// Starts `cmd` with its output in `jobs/<n>.log`; returns the job id and the log path.
    pub fn start(&mut self, mut cmd: Command, command: &str) -> Result<(u32, PathBuf), String> {
        std::fs::create_dir_all(&self.dir).map_err(|e| format!("could not create {}: {e}", self.dir.display()))?;
        if self.next == 0 {
            self.next = self.first_free_id();
        }
        let id = self.next;
        let log = self.dir.join(format!("{id}.log"));
        let file = std::fs::File::create(&log).map_err(|e| format!("could not create {}: {e}", log.display()))?;
        let file_err = file.try_clone().map_err(|e| format!("could not open {}: {e}", log.display()))?;
        quiet(&mut cmd);
        cmd.stdout(Stdio::from(file)).stderr(Stdio::from(file_err));
        let mut child = cmd.spawn().map_err(|e| format!("could not start the shell: {e}"))?;
        self.next += 1;
        let pgid = pgid_of(&child);
        let started = Instant::now();
        let (tx, done) = watch::channel(None);
        tokio::spawn(async move {
            let code = child.wait().await.map(code_of).unwrap_or(-1);
            let _ = tx.send(Some(Finished { code, after: started.elapsed() }));
        });
        self.list.push(Job { id, command: command.to_string(), log: log.clone(), pgid, started, done });
        Ok((id, log))
    }

    /// The job `id` names, or the only job when `id` is absent.
    fn pick(&self, id: Option<u64>) -> Result<&Job, String> {
        match id {
            Some(n) => self.list.iter().find(|j| u64::from(j.id) == n).ok_or_else(|| format!("there is no job {n}; {}", self.ids())),
            None if self.list.len() == 1 => Ok(&self.list[0]),
            None => Err(format!("`id` is required; {}", self.ids())),
        }
    }

    fn ids(&self) -> String {
        if self.list.is_empty() {
            "no background jobs were started (use bash with background: true)".into()
        } else {
            let ids: Vec<String> = self.list.iter().map(|j| j.id.to_string()).collect();
            format!("jobs: {}", ids.join(", "))
        }
    }

    /// Blocks up to `timeout` for the job to exit; returns its exit code and output tail either way.
    pub async fn wait(&self, id: Option<u64>, timeout: Duration) -> Result<ToolOutcome, String> {
        let job = self.pick(id)?;
        let mut done = job.done.clone();
        let finished = match tokio::time::timeout(timeout, done.wait_for(|f| f.is_some())).await {
            Ok(Ok(f)) => *f,
            _ => job.finished(),
        };
        Ok(match finished {
            Some(f) => ToolOutcome { ok: f.code == 0, text: format!("job {} {}\n{}", job.id, job.state(), job.tail()), exit_code: Some(f.code), ..Default::default() },
            None => ToolOutcome::ok(format!("job {} is still {} (waited {}s); output so far:\n{}", job.id, job.state(), timeout.as_secs(), job.tail())),
        })
    }

    /// The output tail so far, without waiting.
    pub fn output(&self, id: Option<u64>) -> Result<ToolOutcome, String> {
        let job = self.pick(id)?;
        Ok(ToolOutcome { ok: true, text: format!("job {} {}\n{}", job.id, job.state(), job.tail()), exit_code: job.finished().map(|f| f.code), ..Default::default() })
    }

    pub async fn kill(&self, id: Option<u64>) -> Result<ToolOutcome, String> {
        let job = self.pick(id)?;
        if let Some(f) = job.finished() {
            return Ok(ToolOutcome::ok(format!("job {} had already exited with code {}", job.id, f.code)));
        }
        stop_group(job).await;
        Ok(ToolOutcome { ok: true, text: format!("job {} killed\n{}", job.id, job.tail()), exit_code: job.finished().map(|f| f.code), ..Default::default() })
    }

    pub fn list(&self) -> ToolOutcome {
        if self.list.is_empty() {
            return ToolOutcome::ok("no background jobs");
        }
        let lines: Vec<String> = self.list.iter().map(|j| format!("{}  {}  {}", j.id, j.state(), one_line(&j.command))).collect();
        ToolOutcome::ok(lines.join("\n"))
    }

    /// Stops every running job and every leftover command group (shutdown): all are signalled at
    /// once, so the grace (`SHUTDOWN_GRACE`) is paid once.
    pub async fn shutdown(&mut self) {
        let running: Vec<&Job> = self.list.iter().filter(|j| j.finished().is_none()).collect();
        let groups: Vec<i32> = running.iter().filter_map(|j| j.pgid).chain(take_leftovers()).collect();
        for pgid in &groups {
            signal_group(*pgid, libc::SIGTERM);
        }
        let until = tokio::time::Instant::now() + SHUTDOWN_GRACE;
        for job in &running {
            let mut done = job.done.clone();
            let _ = tokio::time::timeout_at(until, done.wait_for(|f| f.is_some())).await;
        }
        // A leftover group has no waiter to report its end (its members were reparented away from
        // us), so only asking the kernel tells: a short poll, bounded by the grace, at shutdown only.
        while tokio::time::Instant::now() < until && groups.iter().any(|g| signal_group(*g, 0)) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        for pgid in &groups {
            signal_group(*pgid, libc::SIGKILL);
        }
    }
}

/// SIGTERM, the grace, SIGKILL — and waits (briefly) for the waiter task to record the exit.
async fn stop_group(job: &Job) {
    let Some(pgid) = job.pgid else { return };
    let mut done = job.done.clone();
    signal_group(pgid, libc::SIGTERM);
    if tokio::time::timeout(KILL_GRACE, done.wait_for(|f| f.is_some())).await.is_err() {
        signal_group(pgid, libc::SIGKILL);
        let _ = tokio::time::timeout(Duration::from_secs(1), done.wait_for(|f| f.is_some())).await;
    }
    signal_group(pgid, libc::SIGKILL);
}

/// Jobs never outlive a runtime that is dropped without `shutdown` (a panic, a test).
impl Drop for Jobs {
    fn drop(&mut self) {
        for job in self.list.iter().filter(|j| j.finished().is_none()) {
            if let Some(pgid) = job.pgid {
                signal_group(pgid, libc::SIGKILL);
            }
        }
    }
}

/// A command as one listing line: its first line, clipped, with `…` when there is more.
fn one_line(s: &str) -> String {
    let mut lines = s.trim().lines();
    let first = clip(lines.next().unwrap_or("").trim(), 80);
    if lines.next().is_some() && !first.ends_with('…') {
        first + "…"
    } else {
        first
    }
}

/// The shell commands run with: `bash` when it is on PATH, else `/bin/sh`.
pub fn default_shell() -> PathBuf {
    super::which("bash").unwrap_or_else(|| PathBuf::from("/bin/sh"))
}

/// A plain `shell -c script` in `cwd` (the user's own `Op::Shell`, and tests).
pub fn plain(shell: &Path, script: &str, cwd: &Path) -> Command {
    let mut cmd = Command::new(shell);
    cmd.arg("-c").arg(script).current_dir(cwd);
    cmd
}

/// Alive and not a zombie (a killed orphan may wait a moment for its reaper). For tests.
#[cfg(test)]
pub(crate) fn alive(pid: i32) -> bool {
    if unsafe { libc::kill(pid, 0) } != 0 {
        return false;
    }
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat.rsplit(')').next().map(|rest| !rest.trim_start().starts_with('Z')).unwrap_or(true),
        Err(_) => false,
    }
}

#[cfg(test)]
pub(crate) async fn gone_within(pid: i32, d: Duration) -> bool {
    let until = Instant::now() + d;
    while Instant::now() < until {
        if !alive(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    !alive(pid)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> Command {
        plain(&default_shell(), script, &std::env::temp_dir())
    }

    #[test]
    fn the_tail_buffer_keeps_the_end_and_counts_the_dropped_lines() {
        let mut t = TailBuffer::new(20);
        for i in 0..100 {
            t.push(format!("line {i:03}\n").as_bytes());
        }
        // The last 20 bytes are "7\nline 098\nline 099\n"; the half line "7" goes too.
        assert_eq!(t.finish(), "[… 98 lines truncated]\nline 098\nline 099\n");
        let mut small = TailBuffer::new(100);
        small.push(b"short\n");
        assert_eq!(small.finish(), "short\n");
    }

    #[test]
    fn utf8_chunks_never_split_a_character() {
        let mut s = Utf8Stream::default();
        let bytes = "aé€😀".as_bytes();
        let mut out = String::new();
        for b in bytes {
            out.push_str(&s.decode(std::slice::from_ref(b)));
        }
        out.push_str(&s.flush());
        assert_eq!(out, "aé€😀");
    }

    #[tokio::test]
    async fn merged_output_and_exit_code() {
        let c = run(sh("echo out; echo err >&2; exit 3"), Duration::from_secs(10), None).await.unwrap();
        assert_eq!(c.exit, Some(3));
        assert!(c.output.contains("out\n") && c.output.contains("err\n"), "{}", c.output);
        let o = outcome(c, Duration::from_secs(10));
        assert!(!o.ok && o.exit_code == Some(3) && o.text.ends_with("[exit code 3]"), "{}", o.text);
    }

    #[tokio::test]
    async fn a_timeout_kills_the_group_including_children() {
        let started = Instant::now();
        let c = run(sh("sleep 30 & echo $!; wait"), Duration::from_secs(1), None).await.unwrap();
        assert!(c.timed_out && started.elapsed() < Duration::from_secs(5));
        let pid: i32 = c.output.trim().parse().unwrap();
        assert!(gone_within(pid, Duration::from_secs(3)).await, "sleep {pid} survived the timeout");
        let o = outcome(c, Duration::from_secs(1));
        assert!(!o.ok && o.text.contains("timed out after 1s"), "{}", o.text);
    }

    #[tokio::test]
    async fn a_lingering_background_child_does_not_hold_the_call_open() {
        let started = Instant::now();
        let c = run(sh("sleep 5 & echo started"), Duration::from_secs(20), None).await.unwrap();
        assert_eq!(c.exit, Some(0));
        assert!(started.elapsed() < Duration::from_secs(3), "waited for the orphan: {:?}", started.elapsed());
    }

    #[tokio::test]
    async fn output_streams_to_progress_in_order() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let c = run(sh("printf 'a\\n'; printf 'b\\n' >&2; printf 'c\\n'"), Duration::from_secs(10), Some(&tx)).await.unwrap();
        drop(tx);
        let mut streamed = String::new();
        while let Some(chunk) = rx.recv().await {
            streamed.push_str(&chunk);
        }
        assert_eq!(streamed, "a\nb\nc\n");
        assert_eq!(c.output, "a\nb\nc\n");
    }

    #[tokio::test]
    async fn a_child_left_behind_by_a_finished_command_dies_at_shutdown() {
        // It ignores SIGTERM, so only the SIGKILL after the grace ends it.
        let c = run(sh("trap '' TERM; sleep 30 & echo $!"), Duration::from_secs(10), None).await.unwrap();
        assert_eq!(c.exit, Some(0));
        let pid: i32 = c.output.trim().parse().unwrap();
        assert!(alive(pid), "the background child runs on after its command");
        let mut jobs = Jobs::new(std::env::temp_dir().join(format!("bija-left-{}", crate::types::uuid_v4())));
        let started = Instant::now();
        jobs.shutdown().await;
        assert!(started.elapsed() < Duration::from_secs(4), "{:?}", started.elapsed());
        assert!(gone_within(pid, Duration::from_secs(2)).await, "sleep {pid} outlived shutdown");
    }

    #[test]
    fn a_job_tail_reads_only_the_end_of_a_big_log() {
        let dir = std::env::temp_dir().join(format!("bija-tail-{}", crate::types::uuid_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("1.log");
        let body: String = (1..=20_000).map(|i| format!("line {i:06}\n")).collect();
        std::fs::write(&log, &body).unwrap();
        let job = Job { id: 1, command: "x".into(), log: log.clone(), pgid: None, started: Instant::now(), done: watch::channel(None).1 };
        let text = job.tail();
        let first = text.lines().next().unwrap();
        assert!(first.starts_with("[… ") && first.ends_with(" bytes truncated]"), "unseen lines are not counted: {first}");
        let second = text.lines().nth(1).unwrap();
        assert!(second.len() == 11 && second.starts_with("line "), "a whole line follows the note: {second:?}");
        assert!(text.ends_with(&format!("line 020000\n[full log: {}]", log.display())), "{}", &text[text.len() - 80..]);
        assert!(text.len() <= OUTPUT_CAP + 200, "{}", text.len());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn jobs_continue_numbering_past_existing_logs() {
        let dir = std::env::temp_dir().join(format!("bija-jobs-{}", crate::types::uuid_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("4.log"), "old").unwrap();
        let mut jobs = Jobs::new(dir.clone());
        let (id, log) = jobs.start(sh("echo fresh"), "echo fresh").unwrap();
        assert_eq!(id, 5);
        let w = jobs.wait(None, Duration::from_secs(10)).await.unwrap();
        assert!(w.ok && w.text.contains("fresh"), "{}", w.text);
        assert_eq!(std::fs::read_to_string(dir.join("4.log")).unwrap(), "old");
        assert!(log.ends_with("5.log"));
        assert!(jobs.pick(Some(9)).is_err());
    }
}
