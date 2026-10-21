//! Akasha (experimental): one bounded block of shared notes per agent group — what an agent chooses
//! to share of its thinking (a fact about the codebase, a decision, an interface, a pitfall) and what
//! the Conductor shares for it (what it finished, what it changed). Every agent keeps its own context;
//! the block is the part that is common to the group. Small by design (a cap in characters, explicit
//! `forget`), so it stays a notebook and never becomes a second transcript.
//!
//! The `impl Run` below is the Conductor's side of it, kept here like `resume.rs` keeps resuming:
//! `run.rs` only calls in — at spawns (`akasha_req`), at the prompts that start a member's work
//! (`with_akasha`), in tool dispatch (`akasha_tool`), on the tick (`akasha_tick`) and when a task
//! ends (`akasha_task_ended`). Everything keys off `Run::group_of`, which is `None` for every agent
//! while the feature is off: that one check is what makes a pattern with groups run as if it had none.

use super::run::{Ctx, Run, SpawnReq, WState};
use super::tools;
use crate::hub::AgentId;
use crate::util::trunc;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Entry {
    pub id: u32,
    /// The agent's name (`w3`, `orchestrator`) or `mantra` for the Conductor's own entries.
    pub author: String,
    /// Unix seconds.
    pub at: u64,
    pub text: String,
    #[serde(default)]
    pub urgent: bool,
}

/// One group's block. Persisted in `RunState` as it is.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Block {
    pub group: String,
    /// Capacity of the rendered block, in characters.
    pub cap: usize,
    pub next_id: u32,
    pub entries: Vec<Entry>,
}

/// Longest note a single entry may hold, in characters.
pub const MAX_NOTE_CHARS: usize = 1_000;

/// A busy member gets at most one push this often; what arrives in between waits and goes with
/// the next one. A steer lands at the agent's next tool boundary, and a stream of them would be
/// a stream of interruptions — the very thing a notebook is meant to spare it.
pub const PUSH_EVERY: Duration = Duration::from_secs(20);

/// Who the Conductor's own entries are by.
pub const CONDUCTOR: &str = "mantra";

impl Block {
    pub fn new(group: &str, cap: usize) -> Block {
        Block { group: group.to_string(), cap, next_id: 1, entries: vec![] }
    }

    /// The rendered block's size in characters.
    pub fn chars(&self) -> usize {
        self.render().chars().count()
    }

    /// `1.4k/16k`: the block's size against its cap, as the header and the tool replies say it.
    pub fn usage(&self) -> String {
        format!("{}/{}", fmt_k(self.chars()), fmt_k(self.cap))
    }

    /// Adds a note; `Err` says why not (full, empty, too long) in a sentence the model can act on.
    pub fn note(&mut self, author: &str, text: &str, urgent: bool, at: u64) -> Result<u32, String> {
        let text = clean(text)?;
        let id = self.next_id;
        self.entries.push(Entry { id, author: author.to_string(), at, text, urgent });
        // The cap is on what the agents read, so the note is measured where it will sit.
        let size = self.chars();
        if size > self.cap {
            self.entries.pop();
            return Err(self.full(size));
        }
        self.next_id += 1;
        Ok(id)
    }

    /// Replaces a note's text in place (same id, same position).
    pub fn amend(&mut self, id: u32, text: &str, author: &str, at: u64) -> Result<(), String> {
        let text = clean(text)?;
        let Some(i) = self.entries.iter().position(|e| e.id == id) else { return Err(missing(id)) };
        let old = std::mem::replace(&mut self.entries[i].text, text);
        let size = self.chars();
        if size > self.cap {
            self.entries[i].text = old;
            return Err(self.full(size));
        }
        self.entries[i].author = author.to_string();
        self.entries[i].at = at;
        Ok(())
    }

    pub fn forget(&mut self, id: u32) -> Result<(), String> {
        let Some(i) = self.entries.iter().position(|e| e.id == id) else { return Err(missing(id)) };
        self.entries.remove(i);
        Ok(())
    }

    /// The block as the agents read it: a header line, then one line per note (`!` before urgent ones).
    pub fn render(&self) -> String {
        let mut body = String::new();
        for e in &self.entries {
            body.push('\n');
            if e.urgent {
                body.push('!');
            }
            body.push_str(&format!("[{} · {} · {}] ", e.id, e.author, hm(e.at)));
            // A note may hold an interface over several lines: they stay under it, indented, so
            // every note still starts its own line.
            body.push_str(&e.text.replace('\n', "\n    "));
        }
        if self.entries.is_empty() {
            body.push_str("\n(no notes yet)");
        }
        let n = self.entries.len();
        let header = |size: usize| format!("## akasha · {} · {n} note{} · {}/{}", self.group, if n == 1 { "" } else { "s" }, fmt_k(size), fmt_k(self.cap));
        // The header carries the block's own size, header included: settle it (the rounded
        // number moves its own length at most once or twice).
        let body_chars = body.chars().count();
        let mut size = body_chars;
        for _ in 0..3 {
            let next = body_chars + header(size).chars().count();
            if next == size {
                break;
            }
            size = next;
        }
        format!("{}{body}", header(size))
    }

    fn full(&self, size: usize) -> String {
        format!(
            "the shared notes are full: this would take them to {size} characters, {} over the {} cap. Forget notes that went stale (mantra_akasha_forget) or amend long ones shorter, then try again.",
            size - self.cap,
            self.cap
        )
    }
}

/// A note as it is kept: trimmed, not empty, at most `MAX_NOTE_CHARS`.
fn clean(text: &str) -> Result<String, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("the note is empty — say what the others should know".into());
    }
    let n = text.chars().count();
    if n > MAX_NOTE_CHARS {
        return Err(format!("a note holds at most {MAX_NOTE_CHARS} characters (this one has {n}): say it shorter, or split it into separate notes"));
    }
    Ok(text.to_string())
}

fn missing(id: u32) -> String {
    format!("there is no note #{id} (it may have been forgotten) — mantra_akasha_read shows the current ones")
}

/// Characters in `k` of 1024, so the default cap reads `16k`: `812`, `1.4k`, `16k`.
pub(crate) fn fmt_k(n: usize) -> String {
    if n < 1000 {
        return n.to_string();
    }
    let k = n as f64 / 1024.0;
    let s = if k < 10.0 { format!("{k:.1}") } else { format!("{k:.0}") };
    format!("{}k", s.strip_suffix(".0").unwrap_or(&s))
}

/// `14:02` for unix seconds, on the journal's clock.
fn hm(at: u64) -> String {
    let s = at + crate::util::tz_offset_secs();
    format!("{:02}:{:02}", (s / 3600) % 24, (s / 60) % 60)
}

/// What the Conductor keeps about Akasha between events and never saves: pushes waiting for their
/// member's next delivery window, and the bookkeeping that keeps its own entries to one per fact.
#[derive(Default)]
pub struct Live {
    /// Notes waiting to be pushed, per member, by id: the text is read from the block when the push
    /// goes out, so a note amended in the meantime goes as it reads now and a forgotten one not at all.
    pub(super) outbox: HashMap<AgentId, Vec<u32>>,
    /// When each member last got a push.
    pub(super) pushed: HashMap<AgentId, Instant>,
    /// Tasks whose `changed` entry is in: one per task, whatever its attempts.
    changed: HashSet<String>,
    /// Groups that already dropped an entry of the Conductor's for being full: one journal line per
    /// full spell, not one per drop.
    full: HashSet<String>,
}

/// A note id from a tool call: `7`, `"7"` or `"#7"` (models write it every way).
fn note_id(v: Option<&Value>) -> Option<u32> {
    match v? {
        Value::Number(n) => n.as_u64().and_then(|n| u32::try_from(n).ok()),
        Value::String(s) => s.trim().trim_start_matches('#').parse().ok(),
        _ => None,
    }
}

/// The first useful line of a worker's report: its SUMMARY when it has one, else the first line
/// that is not the STATUS line.
fn report_head(report: &str) -> String {
    let lines: Vec<&str> = report.lines().map(str::trim).collect();
    let summary = lines.iter().position(|l| l.to_lowercase().starts_with("summary:")).and_then(|i| {
        let rest = lines[i].get("summary:".len()..).unwrap_or("").trim();
        if rest.is_empty() { lines[i + 1..].iter().find(|l| !l.is_empty()).copied() } else { Some(rest) }
    });
    let head = summary.or_else(|| lines.iter().find(|l| !l.is_empty() && !l.to_lowercase().starts_with("status:")).copied()).unwrap_or("");
    trunc(head, 160)
}

impl Run {
    /// Akasha on or off for this run (`settings.experimental`), from the app right after the `Run`
    /// is made and before it starts or resumes: on, every group of the pattern gets its block —
    /// a resumed run keeps the ones `state.json` brought back. Off, the blocks a resume restored
    /// stay as they are (saved again, shown, never used), so turning it back on finds them intact.
    pub fn enable_akasha(&mut self, on: bool) {
        self.akasha_enabled = on;
        if !on {
            return;
        }
        for g in &self.pattern.groups {
            self.akasha.entry(g.name.clone()).or_insert_with(|| Block::new(&g.name, g.shared_chars));
        }
    }

    /// The group an agent shares notes with, from its role. `None` while the feature is off.
    pub fn group_of(&self, a: AgentId) -> Option<&str> {
        let role = self.role_name_of(a)?;
        self.group_of_role(&role)
    }

    pub(super) fn group_of_role(&self, role: &str) -> Option<&str> {
        if !self.akasha_enabled {
            return None;
        }
        self.pattern.groups.iter().find(|g| g.roles.iter().any(|r| r == role) && self.akasha.contains_key(&g.name)).map(|g| g.name.as_str())
    }

    /// Run start with groups but the feature off: say so once, with the way to turn it on.
    pub(super) fn akasha_start_note(&mut self) {
        let n = self.pattern.groups.len();
        if self.akasha_enabled || n == 0 {
            return;
        }
        self.log("◌", "gray", format!("akasha: {n} group{} configured but the feature is off — `mantra experimental enable akasha`", if n == 1 { "" } else { "s" }));
    }

    /// A spawn of a member gets the four tools and the paragraph that introduces them. Done on the
    /// request, so every spawn path — fresh, respawned, re-attached on resume — offers them alike.
    pub(super) fn akasha_req(&self, mut req: SpawnReq) -> SpawnReq {
        if self.group_of_role(&req.role_name).is_some() {
            req.instructions.push_str(tools::AKASHA_PROTOCOL);
            req.tools.extend(tools::akasha_tools());
        }
        req
    }

    /// An engine prompt that starts a member's work (spawn brief, retry, respawn, nudge, resume)
    /// ends with its group's notes, when there are any. Everyone else's prompt is left as it is.
    pub(super) fn with_akasha(&self, a: AgentId, text: String) -> String {
        let Some(block) = self.group_of(a).and_then(|g| self.akasha.get(g)) else { return text };
        if block.entries.is_empty() {
            return text;
        }
        format!("{text}\n\n## Shared notes (akasha · {})\n{}", block.group, block.render())
    }

    /// `mantra_akasha_read | note | amend | forget`. Only a member has a block to act on.
    pub(super) fn akasha_tool(&mut self, ctx: &dyn Ctx, a: AgentId, tool: &str, args: &Value) -> (String, bool) {
        let Some(group) = self.group_of(a).map(str::to_string) else {
            return ("not in a group: this agent has no shared notes".into(), false);
        };
        let author = self.name_of(a);
        let now = crate::util::unix_secs();
        let text = args.get("text").and_then(Value::as_str).unwrap_or("");
        let id = note_id(args.get("id"));
        let Some(block) = self.akasha.get_mut(&group) else { return ("not in a group: this agent has no shared notes".into(), false) };
        let need_id = || ("give the note's id (the #n mantra_akasha_read shows)".to_string(), false);
        let line = match tool {
            "mantra_akasha_read" => return (block.render(), true),
            "mantra_akasha_note" => {
                let urgent = match args.get("urgent") {
                    Some(Value::Bool(b)) => *b,
                    Some(Value::String(s)) => s.trim().eq_ignore_ascii_case("true"),
                    _ => false,
                };
                let id = match block.note(&author, text, urgent, now) {
                    Ok(id) => id,
                    Err(e) => return (e, false),
                };
                let kept = block.entries.last().map(|e| e.text.clone()).unwrap_or_default();
                let reply = format!("noted #{id} ({})", block.usage());
                self.akasha_line(format!("akasha · {author} noted #{id}{}: {kept}", if urgent { " (urgent)" } else { "" }));
                self.akasha_push(ctx, a, &group, id, urgent);
                return (reply, true);
            }
            "mantra_akasha_amend" => {
                let Some(id) = id else { return need_id() };
                if let Err(e) = block.amend(id, text, &author, now) {
                    return (e, false);
                }
                (format!("amended #{id} ({})", block.usage()), format!("akasha · {author} amended #{id}: {}", text.trim()))
            }
            "mantra_akasha_forget" => {
                let Some(id) = id else { return need_id() };
                if let Err(e) = block.forget(id) {
                    return (e, false);
                }
                (format!("forgot #{id} ({})", block.usage()), format!("akasha · {author} forgot #{id}"))
            }
            other => return (format!("tool '{other}' is not available here"), false),
        };
        let (reply, journal) = line;
        self.akasha_line(journal);
        (reply, true)
    }

    /// One pulse/journal line per note, amend or forget — never a toast.
    fn akasha_line(&mut self, text: String) {
        self.log("◌", "cyan", trunc(&text.replace('\n', " "), 100));
    }

    /// A new note that the group pushes (`urgent`, or any with `push = "all"`) is queued for every
    /// *other* member that is busy right now. An idle member is never prompted for it: an idle
    /// worker is done or waiting on the orchestrator, and a prompt would start a turn nobody asked
    /// for — it reads the note in its next engine prompt instead.
    fn akasha_push(&mut self, ctx: &dyn Ctx, from: AgentId, group: &str, id: u32, urgent: bool) {
        let push = self.pattern.groups.iter().find(|g| g.name == group).map(|g| g.push.clone()).unwrap_or_default();
        if !(push == "all" || (push == "urgent" && urgent)) {
            return;
        }
        let to: Vec<AgentId> = self.all_agents().into_iter().filter(|m| *m != from && self.group_of(*m) == Some(group) && self.akasha_busy(ctx, *m)).collect();
        for m in to {
            self.akasha_live.outbox.entry(m).or_default().push(id);
        }
    }

    /// Busy for a push: a turn open that has not already yielded (a steer into a turn that ended
    /// with `mantra_wait` would come back as a fresh turn — an idle member prompted after all), and
    /// that nobody is stopping. A turn the user stopped by hand, or the orchestrator paused, stays
    /// open until the backend confirms the interrupt; a steer in that window would land in the
    /// turn being stopped — and any message clears the hand stop (`app::prompt_agent`), so the
    /// agent the user stopped would be put back to work.
    fn akasha_busy(&self, ctx: &dyn Ctx, a: AgentId) -> bool {
        let paused = self.worker_idx(a).map(|wi| self.workers[wi].paused).unwrap_or(false);
        ctx.agent(a).map(|x| x.busy() && !x.stopped_by_user).unwrap_or(false) && !self.yielded.contains(&a) && !paused
    }

    /// From the engine's tick: deliver what waits for each member as one steer, at most once per
    /// `PUSH_EVERY`. A member that went idle in the meantime gets nothing; its next engine prompt
    /// carries the block.
    pub(super) fn akasha_tick(&mut self, ctx: &mut dyn Ctx) {
        if self.akasha_live.outbox.is_empty() {
            return;
        }
        let waiting: Vec<AgentId> = self.akasha_live.outbox.keys().copied().collect();
        for m in waiting {
            if !self.akasha_busy(ctx, m) {
                self.akasha_live.outbox.remove(&m);
                continue;
            }
            if self.akasha_live.pushed.get(&m).map(|t| t.elapsed() < PUSH_EVERY).unwrap_or(false) {
                continue;
            }
            let Some(ids) = self.akasha_live.outbox.remove(&m) else { continue };
            // as the notes read now: amended ones with their new text, forgotten ones not at all
            let block = self.group_of(m).and_then(|g| self.akasha.get(g));
            let notes: Vec<String> = ids.iter().filter_map(|id| block?.entries.iter().find(|e| e.id == *id)).map(|e| format!("[akasha · {}] {}", e.author, e.text)).collect();
            if notes.is_empty() {
                continue;
            }
            self.akasha_live.pushed.insert(m, Instant::now());
            self.mark_edge(m);
            ctx.prompt(m, notes.join("\n\n"));
        }
    }

    /// A member's task ended (`on_worker_done`; `before` is its state when the turn came in): the
    /// Conductor's one-liners for the group, when it has `auto = true` — done with the report's
    /// head and the files it changed, or failed with the error's. A follow-up a done worker
    /// answered is not a second completion, and an outage failure is not the worker's.
    pub(super) fn akasha_task_ended(&mut self, ctx: &dyn Ctx, wi: usize, before: &WState) {
        let w = &self.workers[wi];
        let Some(a) = w.agent else { return };
        let Some(group) = self.group_of(a).map(str::to_string) else { return };
        if !self.pattern.groups.iter().any(|g| g.name == group && g.auto) {
            return;
        }
        let tid = w.task.id.clone();
        let mut lines = vec![];
        match &w.state {
            WState::Done if *before != WState::Done => {
                lines.push(format!("[{tid} · done] {}", report_head(&w.report)));
                let files: Vec<String> = ctx.agent(a).map(|x| x.files.keys().cloned().collect()).unwrap_or_default();
                if !files.is_empty() && !self.akasha_live.changed.contains(&tid) {
                    self.akasha_live.changed.insert(tid.clone());
                    let more = if files.len() > 3 { format!(", +{} more", files.len() - 3) } else { String::new() };
                    lines.push(format!("[{tid} · changed] {}{more}", files[..files.len().min(3)].join(", ")));
                }
            }
            WState::Failed(e) if !w.transient && !matches!(before, WState::Failed(_)) => {
                let head = if e == "blocked" { format!("blocked: {}", report_head(&w.report)) } else { trunc(e.lines().find(|l| !l.trim().is_empty()).unwrap_or(e).trim(), 160) };
                lines.push(format!("[{tid} · failed, attempt {}] {head}", w.attempt));
            }
            _ => {}
        }
        for line in lines {
            self.akasha_auto(&group, &line);
        }
    }

    /// One entry of the Conductor's: never urgent, never pushed, never twice the same. A full block
    /// drops it — a note from an agent is worth more than Mantra's bookkeeping — and says so once.
    fn akasha_auto(&mut self, group: &str, text: &str) {
        let text = trunc(text, 400);
        let Some(block) = self.akasha.get_mut(group) else { return };
        if block.entries.iter().any(|e| e.text == text) {
            return;
        }
        match block.note(CONDUCTOR, &text, false, crate::util::unix_secs()) {
            Ok(_) => {
                self.akasha_live.full.remove(group);
            }
            Err(_) => {
                if self.akasha_live.full.insert(group.to_string()) {
                    self.log("◌", "cyan", format!("akasha · {group} is full — Mantra's own entries are dropped until notes are forgotten"));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(h: u64, m: u64) -> u64 {
        h * 3600 + m * 60
    }

    #[test]
    fn notes_keep_their_order_and_ids_and_render_one_line_each() {
        let mut b = Block::new("builders", 16_384);
        // the header counts itself: 15 chars of body, 39 of header
        assert_eq!(b.render(), "## akasha · builders · 0 notes · 54/16k\n(no notes yet)");
        assert_eq!(b.chars(), 54);
        assert_eq!(b.note("w2", "  models live in src/models  ", false, at(14, 2)), Ok(1));
        assert_eq!(b.note("w3", "the config loader moved\nto src/config.rs", true, at(14, 5)), Ok(2));
        assert_eq!(b.entries[0].text, "models live in src/models", "trimmed");
        let r = b.render();
        let lines: Vec<&str> = r.lines().collect();
        assert!(lines[0].starts_with("## akasha · builders · 2 notes · "), "{r}");
        assert_eq!(lines[1], "[1 · w2 · 14:02] models live in src/models");
        assert_eq!(lines[2], "![2 · w3 · 14:05] the config loader moved");
        assert_eq!(lines[3], "    to src/config.rs", "a second line stays under its note");
        // the header's own number is the rendered size
        assert!(lines[0].ends_with(&format!(" · {}/16k", b.chars())), "{r}");
        assert_eq!(b.usage(), format!("{}/16k", b.chars()));
    }

    #[test]
    fn amend_keeps_id_and_place_and_forget_removes() {
        let mut b = Block::new("g", 4096);
        b.note("w1", "one", false, 10).unwrap();
        b.note("w2", "two", false, 20).unwrap();
        b.note("w3", "three", false, 30).unwrap();
        b.amend(2, " two, revised ", "w4", 99).unwrap();
        assert_eq!(b.entries.iter().map(|e| (e.id, e.text.as_str())).collect::<Vec<_>>(), vec![(1, "one"), (2, "two, revised"), (3, "three")]);
        assert_eq!((b.entries[1].author.as_str(), b.entries[1].at), ("w4", 99), "amend takes the new author and time");
        b.forget(1).unwrap();
        assert_eq!(b.entries.iter().map(|e| e.id).collect::<Vec<_>>(), vec![2, 3]);
        assert_eq!(b.note("w1", "four", false, 40), Ok(4), "ids are never reused");
        assert!(b.forget(1).unwrap_err().contains("no note #1"));
        assert!(b.amend(9, "x", "w1", 0).unwrap_err().contains("no note #9"));
    }

    #[test]
    fn a_note_is_refused_when_empty_too_long_or_past_the_cap() {
        let mut b = Block::new("g", 1024);
        assert!(b.note("w1", "   ", false, 0).unwrap_err().contains("empty"));
        let long = "x".repeat(MAX_NOTE_CHARS + 1);
        let e = b.note("w1", &long, false, 0).unwrap_err();
        assert!(e.contains("at most 1000") && e.contains("1001"), "{e}");
        assert_eq!(Block::new("g", 2048).note("w1", &"x".repeat(MAX_NOTE_CHARS), false, 0), Ok(1), "the longest note is fine where there is room");
        assert_eq!(b.note("w1", &"x".repeat(950), false, 0), Ok(1));
        // the block is near its 1024 chars now: the next note does not fit
        let before = b.clone();
        let e = b.note("w2", "one more fact that does not fit", false, 0).unwrap_err();
        assert!(e.contains("full") && e.contains("over the 1024 cap") && e.contains("mantra_akasha_forget"), "{e}");
        assert_eq!(b, before, "a refused note leaves the block as it was");
        assert!(b.chars() <= b.cap);
        // an amend that would overflow is refused the same way and changes nothing
        let mut c = Block::new("g", 1024);
        c.note("w1", "short", false, 0).unwrap();
        c.note("w2", &"y".repeat(900), false, 0).unwrap();
        let before = c.clone();
        assert!(c.amend(1, &"z".repeat(400), "w3", 5).unwrap_err().contains("full"));
        assert_eq!(c, before);
    }

    #[test]
    fn sizes_read_in_k_of_1024() {
        assert_eq!(fmt_k(812), "812");
        assert_eq!(fmt_k(1434), "1.4k");
        assert_eq!(fmt_k(1024), "1k");
        assert_eq!(fmt_k(16_384), "16k");
        assert_eq!(fmt_k(262_144), "256k");
    }

    #[test]
    fn ids_and_report_heads_are_read_leniently() {
        assert_eq!(note_id(Some(&serde_json::json!(7))), Some(7));
        assert_eq!(note_id(Some(&serde_json::json!("#7"))), Some(7));
        assert_eq!(note_id(Some(&serde_json::json!(" 7 "))), Some(7));
        assert_eq!(note_id(Some(&serde_json::json!("seven"))), None);
        assert_eq!(note_id(None), None);
        assert_eq!(report_head("Did it.\n\nSTATUS: done\nSUMMARY: added Config; tests pass\nmore"), "added Config; tests pass");
        assert_eq!(report_head("STATUS: done\nSUMMARY:\n  added Config\n"), "added Config");
        assert_eq!(report_head("STATUS: done\nImplemented the loader."), "Implemented the loader.");
        assert_eq!(report_head(&format!("SUMMARY: {}", "a".repeat(300))).chars().count(), 160);
    }

    #[test]
    fn a_block_round_trips_through_json() {
        let mut b = Block::new("builders", 16_384);
        b.note("w2", "a fact", true, 5).unwrap();
        let back: Block = serde_json::from_str(&serde_json::to_string(&b).unwrap()).unwrap();
        assert_eq!(back, b);
    }
}
