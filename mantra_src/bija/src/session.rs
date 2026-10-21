//! Sessions on disk (SPEC §5): `session.json` + `transcript.jsonl`, replay and repair on resume.
//!
//! The transcript is the source of truth for the conversation: every message is appended the moment
//! it exists, as one JSON line written with a single `write` (so a crash tears at most the last
//! line), and `open` rebuilds the exact message list the API needs from it. Two things are derived
//! from the *record stream* rather than from the trailing message, because the message list alone
//! cannot tell them apart:
//!
//! - whether a turn is still open (`Pending`): a user or tool record opens one, a final assistant
//!   text or a `turn` record closes it, and `note` records never touch it — an interrupted turn ends
//!   in note + turn, an idle `$ shell` leaves a lone note, and neither must be resumed;
//! - where history starts: everything before the last `compact` record is gone from the model's view,
//!   but the open/closed state carries across it (an idle compaction after a finished turn is not a
//!   pending turn).
//!
//! The transcript file handle holds an exclusive `flock` for the session's lifetime, so a second
//! process resuming a live session fails cleanly instead of interleaving lines with the first.

use crate::types::{now_unix, Message, Usage, Wire};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const META_FILE: &str = "session.json";
const META_TMP: &str = "session.json.tmp";
const TRANSCRIPT_FILE: &str = "transcript.jsonl";

/// The first line of the user message a compaction leaves behind (SPEC §3).
pub const SUMMARY_HEADER: &str = "[context summary — the conversation so far was compacted]";

/// The result synthesized for a tool call that a crash cut off (SPEC §5.3).
pub const RESTART_RESULT: &str = "interrupted: the process was restarted before this tool finished";

/// How long `open` waits for a lock held by a process that is still dying (Mantra kills, then
/// relaunches with `--resume` right away; the kernel drops the old lock a moment later).
const LOCK_WAIT: Duration = Duration::from_secs(2);

/// The variable the API key is read from unless `--api-key-env` names another.
pub const DEFAULT_KEY_ENV: &str = "BIJA_API_KEY";

/// `session.json` (SPEC §5.1).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Meta {
    pub id: String,
    pub name: String,
    pub created_unix: u64,
    pub updated_unix: u64,
    pub cwd: PathBuf,
    pub provider_url: String,
    pub wire: Wire,
    pub api_key_env: String,
    pub model: String,
    pub effort: String,
    pub context_window: u64,
    pub compact_at: u8,
    pub max_output_tokens: Option<u32>,
    pub system: String,
    pub tools_file: Option<PathBuf>,
    /// `--mcp-file`: the MCP servers started with the session (again on every resume).
    pub mcp_file: Option<PathBuf>,
    /// `--vision`: the `screenshot` and `view_image` tools are offered.
    pub vision: bool,
    pub sandbox: String,
    pub writable: Vec<PathBuf>,
    pub network: bool,
    pub turns: u64,
    pub usage_total: Usage,
    pub last_in: u64,
    pub archived: bool,
    pub version: u32,
}

impl Default for Meta {
    fn default() -> Self {
        Meta {
            id: String::new(),
            name: String::new(),
            created_unix: 0,
            updated_unix: 0,
            cwd: PathBuf::new(),
            provider_url: String::new(),
            wire: Wire::OpenaiChat,
            api_key_env: DEFAULT_KEY_ENV.into(),
            model: String::new(),
            effort: String::new(),
            context_window: 200_000,
            compact_at: 85,
            max_output_tokens: None,
            system: String::new(),
            tools_file: None,
            mcp_file: None,
            vision: false,
            sandbox: "workspace-write".into(),
            writable: vec![],
            network: false,
            turns: 0,
            usage_total: Usage::default(),
            last_in: 0,
            archived: false,
            version: 1,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fsync {
    Turn,
    Always,
    Never,
}

impl Fsync {
    pub fn parse(s: &str) -> Option<Fsync> {
        match s.trim().to_ascii_lowercase().as_str() {
            "turn" => Some(Fsync::Turn),
            "always" => Some(Fsync::Always),
            "never" => Some(Fsync::Never),
            _ => None,
        }
    }
}

/// What `Session::open` found at the end of the transcript (SPEC §5.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pending {
    /// The last turn finished; wait for a prompt.
    None,
    /// The transcript ends inside a turn (a user message, or tool results, without a final answer):
    /// the agent continues it right away.
    Turn,
}

/// How far the turn that was open at the end of the transcript had got, so a resumed turn keeps
/// numbering its messages (`t<N>-m<K>`) and fallback call ids past the ones already emitted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OpenTurn {
    pub assistant_messages: usize,
    pub tool_calls: usize,
}

/// One line of `transcript.jsonl` (SPEC §5.2).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Record {
    Msg {
        #[serde(flatten)]
        message: Message,
        #[serde(default)]
        at: u64,
    },
    Compact {
        summary: String,
        #[serde(default)]
        from: u64,
        #[serde(default)]
        to: u64,
        #[serde(default)]
        at: u64,
    },
    Note {
        text: String,
        #[serde(default)]
        at: u64,
    },
    Turn {
        n: u64,
        status: String,
        #[serde(default)]
        at: u64,
    },
}

impl Record {
    pub fn parse(line: &str) -> Option<Record> {
        serde_json::from_str(line.trim()).ok()
    }
}

/// The `msg` record written from a borrowed message (the owned `Record::Msg` would force a clone of
/// every message just to serialize it). Produces exactly what `Record::Msg` parses.
#[derive(Serialize)]
struct MsgLine<'a> {
    t: &'static str,
    #[serde(flatten)]
    message: &'a Message,
    at: u64,
}

/// The user message a note becomes for the model.
pub fn note_message(text: &str) -> Message {
    Message::User { text: format!("[note] {text}") }
}

/// The user message a compaction leaves as the whole history.
pub fn summary_message(summary: &str) -> Message {
    Message::User { text: format!("{SUMMARY_HEADER}\n{}", summary.trim()) }
}

/// Session ids become a path component: keep them to a safe alphabet.
pub fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && !id.starts_with('.') && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

pub fn transcript_path(dir: &Path) -> PathBuf {
    dir.join(TRANSCRIPT_FILE)
}

pub struct Session {
    pub meta: Meta,
    pub dir: PathBuf,
    pub messages: Vec<Message>,
    pub fsync: Fsync,
    /// Append handle of transcript.jsonl; owns the session lock.
    transcript: File,
    open_turn: OpenTurn,
}

impl Session {
    pub fn dir_for(home: &Path, id: &str) -> PathBuf {
        home.join("sessions").join(id)
    }

    /// Creates `<home>/sessions/<id>/` and writes session.json. Fails if a session already lives
    /// there; a directory that exists without one is fine (Mantra pre-creates it with tools.json and
    /// system.md before launching, SPEC §9.2).
    pub fn create(home: &Path, mut meta: Meta, fsync: Fsync) -> io::Result<Session> {
        check_id(&meta.id)?;
        let dir = Session::dir_for(home, &meta.id);
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;
        if dir.join(META_FILE).exists() {
            return Err(exists(&meta.id));
        }
        // `create_new` on the transcript is the atomic "nobody else is creating this" check.
        let transcript = match OpenOptions::new().append(true).create_new(true).open(transcript_path(&dir)) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => return Err(exists(&meta.id)),
            Err(e) => return Err(e),
        };
        lock(&transcript, &meta.id, Duration::ZERO)?;
        let now = now_unix();
        if meta.created_unix == 0 {
            meta.created_unix = now;
        }
        let mut s = Session { meta, dir, messages: Vec::new(), fsync, transcript, open_turn: OpenTurn::default() };
        s.save_meta()?;
        Ok(s)
    }

    /// Reopens a session: meta + transcript replay + tail repair (SPEC §5.3).
    pub fn open(home: &Path, id: &str, fsync: Fsync) -> io::Result<(Session, Pending)> {
        Session::open_waiting(home, id, fsync, LOCK_WAIT)
    }

    fn open_waiting(home: &Path, id: &str, fsync: Fsync, lock_wait: Duration) -> io::Result<(Session, Pending)> {
        check_id(id)?;
        let dir = Session::dir_for(home, id);
        let meta = read_meta(&dir).map_err(|e| match e.kind() {
            io::ErrorKind::NotFound => io::Error::new(io::ErrorKind::NotFound, format!("no session {id} in {}", home.display())),
            _ => e,
        })?;
        let mut transcript = OpenOptions::new().read(true).append(true).create(true).open(transcript_path(&dir))?;
        lock(&transcript, id, lock_wait)?;
        let mut bytes = Vec::new();
        transcript.read_to_end(&mut bytes)?;
        let replay = replay(&bytes);
        for line in &replay.skipped {
            eprintln!("bija: {id}: transcript line {line} is unreadable, skipped");
        }
        // Appending after a torn line would fuse the next record onto the garbage.
        match replay.tail {
            Tail::Clean => {}
            Tail::Unterminated => transcript.write_all(b"\n")?,
            Tail::Torn(len) => {
                eprintln!("bija: {id}: dropped a torn last transcript line");
                transcript.set_len(len as u64)?;
            }
        }
        let mut s = Session { meta, dir, messages: replay.messages, fsync, transcript, open_turn: replay.open_turn };
        s.repair(RESTART_RESULT)?;
        let pending = if replay.open && !s.messages.is_empty() { Pending::Turn } else { Pending::None };
        Ok((s, pending))
    }

    /// Appends a message to memory and to the transcript.
    pub fn push(&mut self, m: Message) -> io::Result<()> {
        let line = serde_json::to_string(&MsgLine { t: "msg", message: &m, at: now_unix() }).map_err(io::Error::other);
        let sync = self.fsync == Fsync::Always && !matches!(m, Message::User { .. });
        self.messages.push(m);
        self.append(&line?, sync)
    }

    /// Appends a `note` record (becomes a `[note] …` user message for the model).
    pub fn note(&mut self, text: &str) -> io::Result<()> {
        self.messages.push(note_message(text));
        self.write_record(&Record::Note { text: text.to_string(), at: now_unix() }, false)
    }

    /// Replaces the history with the summary (SPEC §3 compaction) and records it.
    pub fn compact(&mut self, summary: &str, from: u64, to: u64) -> io::Result<()> {
        self.messages = vec![summary_message(summary)];
        let sync = self.fsync != Fsync::Never;
        self.write_record(&Record::Compact { summary: summary.trim().to_string(), from, to, at: now_unix() }, sync)
    }

    /// Records the end of a turn and fsyncs per policy; the meta (turn count, usage) is saved with it.
    pub fn end_turn(&mut self, status: &str) -> io::Result<()> {
        self.open_turn = OpenTurn::default();
        let sync = self.fsync != Fsync::Never;
        let rec = self.write_record(&Record::Turn { n: self.meta.turns, status: status.to_string(), at: now_unix() }, sync);
        let meta = Session::save_meta_at(&self.dir, &mut self.meta, self.fsync != Fsync::Never);
        rec.and(meta)
    }

    /// Rewrites session.json atomically.
    pub fn save_meta(&mut self) -> io::Result<()> {
        Session::save_meta_at(&self.dir, &mut self.meta, self.fsync == Fsync::Always)
    }

    /// `save_meta` on borrowed parts, for callers that hold `messages` borrowed elsewhere (a `set`
    /// arriving while a completion over those messages is in flight). `durable` fsyncs the new file
    /// before it replaces the old one.
    pub fn save_meta_at(dir: &Path, meta: &mut Meta, durable: bool) -> io::Result<()> {
        meta.updated_unix = now_unix();
        let body = serde_json::to_vec_pretty(meta).map_err(io::Error::other)?;
        let tmp = dir.join(META_TMP);
        {
            let mut f = File::create(&tmp)?;
            f.write_all(&body)?;
            f.write_all(b"\n")?;
            if durable {
                f.sync_all()?;
            }
        }
        std::fs::rename(&tmp, dir.join(META_FILE))
    }

    /// Lists `<home>/sessions/*/session.json`, newest first.
    pub fn list(home: &Path) -> Vec<Meta> {
        let Ok(entries) = std::fs::read_dir(home.join("sessions")) else {
            return Vec::new();
        };
        let mut metas: Vec<Meta> = entries.flatten().filter_map(|e| read_meta(&e.path()).ok()).collect();
        metas.sort_by(|a, b| b.updated_unix.cmp(&a.updated_unix).then(b.created_unix.cmp(&a.created_unix)).then(a.id.cmp(&b.id)));
        metas
    }

    /// Answers every tool call of the last assistant message that has no result yet with an error
    /// result carrying `text`, so the message list is valid for the API again. Returns how many.
    pub fn repair(&mut self, text: &str) -> io::Result<usize> {
        let missing = unanswered_calls(&self.messages);
        let n = missing.len();
        for (call_id, name) in missing {
            self.push(Message::Tool { call_id, name, text: text.to_string(), is_error: true, images: vec![] })?;
        }
        Ok(n)
    }

    /// How far the open turn had got when the transcript was replayed (zero after `end_turn`).
    pub fn open_turn(&self) -> OpenTurn {
        self.open_turn
    }

    /// Makes the session durable on the way out: the transcript fsynced and the meta saved and
    /// fsynced, inline (unless the policy is `never`). Shutdown does this before anything else,
    /// because whoever asked for it may SIGKILL the process a moment later.
    pub fn close(&mut self) -> io::Result<()> {
        let durable = self.fsync != Fsync::Never;
        let synced = if durable { self.transcript.sync_data() } else { Ok(()) };
        let saved = Session::save_meta_at(&self.dir, &mut self.meta, durable);
        synced.and(saved)
    }

    fn write_record(&mut self, rec: &Record, sync: bool) -> io::Result<()> {
        let line = serde_json::to_string(rec).map_err(io::Error::other)?;
        self.append(&line, sync)
    }

    /// One `write` per record: the file sees whole lines or, after a crash, one torn last line.
    fn append(&mut self, line: &str, sync: bool) -> io::Result<()> {
        let mut buf = String::with_capacity(line.len() + 1);
        buf.push_str(line);
        buf.push('\n');
        self.transcript.write_all(buf.as_bytes())?;
        if sync {
            self.sync_soon();
        }
        Ok(())
    }

    /// fsync without stalling the event loop: on a blocking-pool thread when a runtime is around,
    /// inline otherwise (tests, tools).
    fn sync_soon(&self) {
        let file = match self.transcript.try_clone() {
            Ok(f) => f,
            Err(e) => return eprintln!("bija: fsync transcript: {e}"),
        };
        let sync = move || {
            if let Err(e) = file.sync_data() {
                eprintln!("bija: fsync transcript: {e}");
            }
        };
        match tokio::runtime::Handle::try_current() {
            Ok(rt) => drop(rt.spawn_blocking(sync)),
            Err(_) => sync(),
        }
    }
}

fn check_id(id: &str) -> io::Result<()> {
    if valid_id(id) {
        Ok(())
    } else {
        Err(io::Error::new(io::ErrorKind::InvalidInput, format!("invalid session id {id:?} (letters, digits, '-', '_', '.')")))
    }
}

fn exists(id: &str) -> io::Error {
    io::Error::new(io::ErrorKind::AlreadyExists, format!("session {id} already exists (use --resume {id})"))
}

fn read_meta(dir: &Path) -> io::Result<Meta> {
    let text = std::fs::read_to_string(dir.join(META_FILE))?;
    serde_json::from_str(&text).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("{}: {e}", dir.join(META_FILE).display())))
}

/// Takes the session's exclusive lock, retrying for `wait` while another process holds it.
fn lock(file: &File, id: &str, wait: Duration) -> io::Result<()> {
    let deadline = Instant::now() + wait;
    loop {
        // SAFETY: flock on a descriptor we own; it has no memory effects.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::WouldBlock {
            return Err(e);
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(io::ErrorKind::WouldBlock, format!("session {id} is in use by another bija process")));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Tool calls of the last assistant message without a result after it, in call order.
fn unanswered_calls(messages: &[Message]) -> Vec<(String, String)> {
    let Some(pos) = messages.iter().rposition(|m| matches!(m, Message::Assistant { .. })) else {
        return Vec::new();
    };
    let Message::Assistant { tool_calls, .. } = &messages[pos] else {
        return Vec::new();
    };
    let answered: HashSet<&str> = messages[pos + 1..]
        .iter()
        .filter_map(|m| match m {
            Message::Tool { call_id, .. } => Some(call_id.as_str()),
            _ => None,
        })
        .collect();
    tool_calls.iter().filter(|c| !answered.contains(c.id.as_str())).map(|c| (c.id.clone(), c.name.clone())).collect()
}

/// How the transcript file ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tail {
    Clean,
    /// The last line parsed but lacks its newline.
    Unterminated,
    /// The last line is garbage (a write cut short): truncate the file to this length.
    Torn(usize),
}

#[derive(Debug)]
struct Replay {
    messages: Vec<Message>,
    /// A turn was open when the transcript ended.
    open: bool,
    open_turn: OpenTurn,
    /// 1-based numbers of unreadable lines that were skipped (not counting a torn last line).
    skipped: Vec<usize>,
    tail: Tail,
}

/// Rebuilds the message list and the turn state from the raw transcript bytes.
fn replay(bytes: &[u8]) -> Replay {
    let mut r = Replay { messages: Vec::new(), open: false, open_turn: OpenTurn::default(), skipped: Vec::new(), tail: Tail::Clean };
    let mut start = 0;
    let mut line_no = 0;
    while start < bytes.len() {
        line_no += 1;
        let (end, terminated) = match bytes[start..].iter().position(|&b| b == b'\n') {
            Some(i) => (start + i, true),
            None => (bytes.len(), false),
        };
        let record = match std::str::from_utf8(&bytes[start..end]).map(str::trim) {
            Ok("") => None,
            Ok(line) => Some(Record::parse(line)),
            // Not even UTF-8: as unreadable as bad JSON.
            Err(_) => Some(None),
        };
        match record {
            None => {}
            Some(Some(rec)) => {
                apply(&mut r, rec);
                if !terminated {
                    r.tail = Tail::Unterminated;
                }
            }
            Some(None) if terminated => r.skipped.push(line_no),
            Some(None) => r.tail = Tail::Torn(start),
        }
        start = end + 1;
    }
    r
}

fn apply(r: &mut Replay, rec: Record) {
    match rec {
        Record::Msg { message, .. } => {
            match &message {
                Message::User { .. } | Message::Tool { .. } => r.open = true,
                Message::Assistant { tool_calls, .. } => {
                    r.open_turn.assistant_messages += 1;
                    r.open_turn.tool_calls += tool_calls.len();
                    r.open = !tool_calls.is_empty();
                }
            }
            r.messages.push(message);
        }
        Record::Note { text, .. } => r.messages.push(note_message(&text)),
        Record::Compact { summary, .. } => r.messages = vec![summary_message(&summary)],
        Record::Turn { .. } => {
            r.open = false;
            r.open_turn = OpenTurn::default();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{uuid_v4, ToolCall};
    use serde_json::json;

    /// A fresh home under the system temp dir, removed when dropped.
    struct Home(PathBuf);
    impl Home {
        fn new() -> Home {
            let p = std::env::temp_dir().join(format!("bija-session-test-{}", uuid_v4()));
            std::fs::create_dir_all(&p).unwrap();
            Home(p)
        }
    }
    impl Drop for Home {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn meta(id: &str) -> Meta {
        Meta { id: id.into(), model: "m".into(), cwd: "/work".into(), provider_url: "http://x/v1".into(), ..Meta::default() }
    }

    fn user(t: &str) -> Message {
        Message::User { text: t.into() }
    }
    fn assistant(t: &str, calls: &[(&str, &str)]) -> Message {
        Message::Assistant { text: t.into(), reasoning: String::new(), tool_calls: calls.iter().map(|(id, name)| ToolCall { id: (*id).into(), name: (*name).into(), args: json!({}), raw: String::new() }).collect() }
    }
    fn tool(id: &str, t: &str) -> Message {
        Message::Tool { images: vec![], call_id: id.into(), name: "bash".into(), text: t.into(), is_error: false }
    }

    fn transcript(home: &Home, id: &str) -> String {
        std::fs::read_to_string(transcript_path(&Session::dir_for(&home.0, id))).unwrap()
    }

    fn write_transcript(home: &Home, id: &str, text: &str) {
        let s = Session::create(&home.0, meta(id), Fsync::Never).unwrap();
        drop(s);
        std::fs::write(transcript_path(&Session::dir_for(&home.0, id)), text).unwrap();
    }

    #[test]
    fn records_have_the_documented_shapes_and_parse_back() {
        let home = Home::new();
        let mut s = Session::create(&home.0, meta("a"), Fsync::Never).unwrap();
        s.push(user("hi")).unwrap();
        s.push(Message::Assistant { text: "ok".into(), reasoning: "think".into(), tool_calls: vec![ToolCall { id: "c1".into(), name: "bash".into(), args: json!({"command": "ls"}), raw: String::new() }] }).unwrap();
        s.push(tool("c1", "out")).unwrap();
        s.note("turn interrupted").unwrap();
        s.meta.turns = 3;
        s.end_turn("completed").unwrap();
        s.compact("sum", 171000, 4100).unwrap();
        let text = transcript(&home, "a");
        let lines: Vec<serde_json::Value> = text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
        assert_eq!(lines[0]["t"], "msg");
        assert_eq!(lines[0]["role"], "user");
        assert_eq!(lines[0]["text"], "hi");
        assert!(lines[0]["at"].as_u64().unwrap() > 0);
        assert_eq!(lines[1]["role"], "assistant");
        assert_eq!(lines[1]["reasoning"], "think");
        assert_eq!(lines[1]["tool_calls"][0], json!({"id": "c1", "name": "bash", "args": {"command": "ls"}}));
        assert_eq!(lines[2]["role"], "tool");
        assert_eq!(lines[2]["call_id"], "c1");
        assert_eq!(lines[2]["is_error"], false);
        assert_eq!(lines[3]["t"], "note");
        assert_eq!(lines[3]["text"], "turn interrupted");
        assert_eq!((lines[4]["t"].as_str(), lines[4]["n"].as_u64(), lines[4]["status"].as_str()), (Some("turn"), Some(3), Some("completed")));
        assert_eq!((lines[5]["t"].as_str(), lines[5]["from"].as_u64(), lines[5]["to"].as_u64()), (Some("compact"), Some(171000), Some(4100)));
        assert!(text.lines().next().unwrap().starts_with(r#"{"t":"msg","role":"user""#), "t leads the line");
        for l in text.lines() {
            assert!(Record::parse(l).is_some(), "{l}");
        }
        assert_eq!(Record::parse(text.lines().nth(2).unwrap()), Some(Record::Msg { message: tool("c1", "out"), at: lines[2]["at"].as_u64().unwrap() }));
    }

    #[test]
    fn a_dangling_tool_call_is_answered_and_the_turn_is_pending() {
        let home = Home::new();
        {
            let mut s = Session::create(&home.0, meta("d"), Fsync::Never).unwrap();
            s.push(user("go")).unwrap();
            s.push(assistant("", &[("c1", "bash"), ("c2", "read_file")])).unwrap();
            s.push(tool("c1", "done")).unwrap();
        }
        let (s, pending) = Session::open(&home.0, "d", Fsync::Never).unwrap();
        assert_eq!(pending, Pending::Turn);
        assert_eq!(s.messages.len(), 4);
        assert_eq!(s.messages[3], Message::Tool { images: vec![], call_id: "c2".into(), name: "read_file".into(), text: RESTART_RESULT.into(), is_error: true });
        assert_eq!(s.open_turn(), OpenTurn { assistant_messages: 1, tool_calls: 2 });
        drop(s);
        // The repair was written down: a second open finds nothing left to repair.
        assert_eq!(transcript(&home, "d").lines().count(), 4);
        let (s, pending) = Session::open(&home.0, "d", Fsync::Never).unwrap();
        assert_eq!((s.messages.len(), pending), (4, Pending::Turn));
        assert_eq!(transcript(&home, "d").lines().count(), 4);
    }

    #[test]
    fn a_trailing_user_message_is_pending_and_a_finished_turn_is_not() {
        let home = Home::new();
        {
            let mut s = Session::create(&home.0, meta("u"), Fsync::Never).unwrap();
            s.push(user("first")).unwrap();
        }
        let (mut s, pending) = Session::open(&home.0, "u", Fsync::Never).unwrap();
        assert_eq!(pending, Pending::Turn);
        s.push(assistant("answer", &[])).unwrap();
        drop(s);
        // A final assistant text closes the turn even without its `turn` record (crash right after).
        let (mut s, pending) = Session::open(&home.0, "u", Fsync::Never).unwrap();
        assert_eq!(pending, Pending::None);
        s.meta.turns = 1;
        s.end_turn("completed").unwrap();
        drop(s);
        let (s, pending) = Session::open(&home.0, "u", Fsync::Never).unwrap();
        assert_eq!((pending, s.messages.len(), s.open_turn()), (Pending::None, 2, OpenTurn::default()));
    }

    #[test]
    fn notes_never_open_a_turn() {
        let home = Home::new();
        {
            let mut s = Session::create(&home.0, meta("n"), Fsync::Never).unwrap();
            s.push(user("go")).unwrap();
            s.push(assistant("", &[("c1", "bash")])).unwrap();
            s.repair("interrupted").unwrap();
            s.note("turn interrupted").unwrap();
            s.end_turn("interrupted").unwrap();
            s.note("$ ls\na.txt").unwrap();
        }
        let (s, pending) = Session::open(&home.0, "n", Fsync::Never).unwrap();
        assert_eq!(pending, Pending::None);
        assert_eq!(s.messages.last(), Some(&Message::User { text: "[note] $ ls\na.txt".into() }));
        assert_eq!(s.messages[2], Message::Tool { images: vec![], call_id: "c1".into(), name: "bash".into(), text: "interrupted".into(), is_error: true });
    }

    #[test]
    fn a_torn_last_line_is_dropped_and_appends_stay_valid() {
        let home = Home::new();
        let good = r#"{"t":"msg","role":"user","text":"hi","at":1}"#;
        write_transcript(&home, "t", &format!("{good}\n{{\"t\":\"msg\",\"role\":\"assis"));
        let (mut s, pending) = Session::open(&home.0, "t", Fsync::Never).unwrap();
        assert_eq!((s.messages.len(), pending), (1, Pending::Turn));
        s.push(assistant("ok", &[])).unwrap();
        drop(s);
        let text = transcript(&home, "t");
        assert_eq!(text.lines().count(), 2);
        assert!(text.lines().all(|l| Record::parse(l).is_some()), "{text}");
    }

    #[test]
    fn an_unterminated_but_complete_last_line_is_kept() {
        let home = Home::new();
        write_transcript(&home, "w", r#"{"t":"msg","role":"user","text":"hi","at":1}"#);
        let (mut s, _) = Session::open(&home.0, "w", Fsync::Never).unwrap();
        assert_eq!(s.messages, vec![user("hi")]);
        s.push(assistant("ok", &[])).unwrap();
        drop(s);
        assert_eq!(transcript(&home, "w").lines().filter(|l| Record::parse(l).is_some()).count(), 2);
    }

    #[test]
    fn unreadable_middle_lines_are_skipped() {
        let home = Home::new();
        write_transcript(&home, "g", "{\"t\":\"msg\",\"role\":\"user\",\"text\":\"a\"}\nnot json\n\n{\"t\":\"msg\",\"role\":\"assistant\",\"text\":\"b\"}\n");
        let (s, pending) = Session::open(&home.0, "g", Fsync::Never).unwrap();
        assert_eq!(s.messages, vec![user("a"), assistant("b", &[])]);
        assert_eq!(pending, Pending::None);
        let r = replay(b"{\"t\":\"note\",\"text\":\"x\"}\nnot json\n");
        assert_eq!((r.skipped, r.tail), (vec![2], Tail::Clean));
    }

    #[test]
    fn a_compact_record_resets_history_but_not_the_turn_state() {
        let home = Home::new();
        {
            let mut s = Session::create(&home.0, meta("c"), Fsync::Never).unwrap();
            s.push(user("old")).unwrap();
            s.push(assistant("old answer", &[])).unwrap();
            s.end_turn("completed").unwrap();
            s.compact("what happened so far", 9000, 12).unwrap();
            assert_eq!(s.messages, vec![summary_message("what happened so far")]);
        }
        // An idle compaction after a finished turn: the summary is a user message, yet nothing is pending.
        let (mut s, pending) = Session::open(&home.0, "c", Fsync::Never).unwrap();
        assert_eq!(pending, Pending::None);
        assert_eq!(s.messages, vec![summary_message("what happened so far")]);
        match &s.messages[0] {
            Message::User { text } => assert!(text.starts_with(SUMMARY_HEADER) && text.ends_with("what happened so far")),
            other => panic!("{other:?}"),
        }
        // A compaction in the middle of a turn keeps the turn open.
        s.push(user("next")).unwrap();
        s.compact("mid-turn summary", 900, 5).unwrap();
        drop(s);
        let (s, pending) = Session::open(&home.0, "c", Fsync::Never).unwrap();
        assert_eq!((s.messages.len(), pending), (1, Pending::Turn));
    }

    #[test]
    fn meta_round_trips_and_the_write_is_atomic() {
        let home = Home::new();
        let mut m = meta("m");
        m.effort = "high".into();
        m.writable = vec!["/tmp/x".into()];
        m.max_output_tokens = Some(1234);
        m.tools_file = Some("/s/tools.json".into());
        m.mcp_file = Some("/s/mcp.json".into());
        m.vision = true;
        let mut s = Session::create(&home.0, m, Fsync::Always).unwrap();
        s.meta.usage_total = Usage { input: 10, output: 2, cached: 4, estimated: false };
        s.meta.turns = 7;
        s.save_meta().unwrap();
        let want = s.meta.clone();
        drop(s);
        assert!(!Session::dir_for(&home.0, "m").join(META_TMP).exists());
        let (s, _) = Session::open(&home.0, "m", Fsync::Never).unwrap();
        assert_eq!(s.meta, want);
        assert!(s.meta.created_unix > 0 && s.meta.updated_unix >= s.meta.created_unix);
        // Old or partial files still load: missing fields take their defaults.
        std::fs::write(Session::dir_for(&home.0, "m").join(META_FILE), r#"{"id":"m","model":"x","future_field":1}"#).unwrap();
        drop(s);
        let (s, _) = Session::open(&home.0, "m", Fsync::Never).unwrap();
        assert_eq!((s.meta.model.as_str(), s.meta.context_window, s.meta.compact_at), ("x", 200_000, 85));
        assert_eq!((s.meta.mcp_file.as_deref(), s.meta.vision), (None, false), "a session from before MCP has none");
    }

    #[test]
    fn a_corrupt_meta_is_an_error_not_a_panic() {
        let home = Home::new();
        drop(Session::create(&home.0, meta("bad"), Fsync::Never).unwrap());
        std::fs::write(Session::dir_for(&home.0, "bad").join(META_FILE), "{nope").unwrap();
        let e = Session::open(&home.0, "bad", Fsync::Never).err().unwrap();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
        let e = Session::open(&home.0, "missing", Fsync::Never).err().unwrap();
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
        assert!(e.to_string().contains("no session missing"), "{e}");
    }

    #[test]
    fn create_refuses_an_existing_session_but_accepts_a_prepared_dir() {
        let home = Home::new();
        drop(Session::create(&home.0, meta("x"), Fsync::Never).unwrap());
        let e = Session::create(&home.0, meta("x"), Fsync::Never).err().unwrap();
        assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
        let dir = Session::dir_for(&home.0, "prepared");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("tools.json"), "[]").unwrap();
        let s = Session::create(&home.0, meta("prepared"), Fsync::Never).unwrap();
        assert!(s.dir.join("tools.json").exists() && s.dir.join(META_FILE).exists());
        for bad in ["", "../up", "a/b", ".hidden"] {
            assert_eq!(Session::create(&home.0, meta(bad), Fsync::Never).err().map(|e| e.kind()), Some(io::ErrorKind::InvalidInput), "{bad:?}");
        }
    }

    #[test]
    fn a_live_session_cannot_be_opened_twice() {
        let home = Home::new();
        let s = Session::create(&home.0, meta("l"), Fsync::Never).unwrap();
        let e = Session::open_waiting(&home.0, "l", Fsync::Never, Duration::ZERO).err().unwrap();
        assert_eq!(e.kind(), io::ErrorKind::WouldBlock);
        assert!(e.to_string().contains("in use"), "{e}");
        drop(s);
        assert!(Session::open_waiting(&home.0, "l", Fsync::Never, Duration::ZERO).is_ok());
    }

    #[test]
    fn list_is_newest_first_and_skips_junk() {
        let home = Home::new();
        for (id, updated) in [("old", 100), ("new", 300), ("mid", 200)] {
            let mut s = Session::create(&home.0, meta(id), Fsync::Never).unwrap();
            s.meta.updated_unix = updated;
            let body = serde_json::to_string(&s.meta).unwrap();
            std::fs::write(s.dir.join(META_FILE), body).unwrap();
        }
        std::fs::create_dir_all(home.0.join("sessions").join("empty")).unwrap();
        std::fs::write(home.0.join("sessions").join("stray.txt"), "x").unwrap();
        let ids: Vec<String> = Session::list(&home.0).into_iter().map(|m| m.id).collect();
        assert_eq!(ids, vec!["new", "mid", "old"]);
        assert!(Session::list(&home.0.join("nowhere")).is_empty());
    }

    #[test]
    fn every_fsync_policy_writes_the_same_records() {
        for policy in [Fsync::Turn, Fsync::Always, Fsync::Never] {
            let home = Home::new();
            let mut s = Session::create(&home.0, meta("f"), policy).unwrap();
            s.push(user("a")).unwrap();
            s.push(assistant("b", &[])).unwrap();
            s.end_turn("completed").unwrap();
            s.close().unwrap();
            drop(s);
            assert_eq!(transcript(&home, "f").lines().count(), 3, "{policy:?}");
        }
        assert_eq!(Fsync::parse("Always"), Some(Fsync::Always));
        assert_eq!(Fsync::parse("sometimes"), None);
    }
}
