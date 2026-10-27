//! File tools (SPEC §6.1): read_file, write_file, edit_file, delete, list_dir, glob, grep.
//!
//! Each tool is a method on `Fs` returning `Result<ToolOutcome, String>`: `Err` is a sentence for
//! the model (the dispatcher prefixes the tool name), so a bad path or a missed edit is a turn the
//! model can recover from, never a crash. Reads are capped (64 KiB, 2000 lines per call) and
//! streamed, so a gigabyte log costs a page, not the process's memory. Writes go through the
//! sandbox path policy and report a unified diff (from the system `diff`, which every dev box has
//! and which is far smaller than a diff library) for Mantra's change view.

use super::sandbox::{resolve, Policy};
use super::Args;
use crate::types::{prefix, FileChange, ToolOutcome};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

/// Bytes of text one read_file call returns at most.
const READ_BYTE_CAP: usize = 64 * 1024;
/// Lines one read_file call returns by default.
const READ_LINE_DEFAULT: u64 = 2000;
/// A longer line is cut (minified files would otherwise eat the whole page in one line).
const LINE_CAP: usize = 2000;
/// How much of a file is sniffed for NUL bytes to call it binary.
const SNIFF: usize = 8192;
/// A diff larger than this is cut; it only feeds the change view, never the model.
const DIFF_CAP: usize = 256 * 1024;
const LIST_CAP: usize = 1000;
const GLOB_CAP: usize = 500;
/// Entries a glob may visit before giving up (a pattern like `**/x` over a huge tree).
const GLOB_VISIT_CAP: usize = 200_000;
const GREP_DEFAULT: u64 = 200;
const GREP_MAX: u64 = 2000;
const GREP_TIMEOUT: Duration = Duration::from_secs(60);
/// Bytes read_file scans past the page it returns to count the lines left; past this the total is
/// reported as a lower bound, so paging a multi-gigabyte log stays a page's worth of work.
const COUNT_BUDGET: u64 = 8 * 1024 * 1024;
/// Grep result lines are cut here; the model wants the location, not a minified bundle.
const GREP_LINE_CAP: usize = 300;
/// Directories list_dir does not expand and glob does not enter unless the pattern names them.
const HEAVY_DIRS: [&str; 3] = [".git", "target", "node_modules"];

/// The file tools' view of the world: where relative paths start, where writes may go, and which
/// helper programs exist (probed once at start).
pub struct Fs {
    pub cwd: PathBuf,
    pub policy: Policy,
    pub diff: bool,
    pub rg: Option<PathBuf>,
}

/// An io error as a sentence that names the path.
fn io_err(shown: &str, e: std::io::Error) -> String {
    match e.kind() {
        std::io::ErrorKind::NotFound => format!("no such file or directory: {shown}"),
        std::io::ErrorKind::PermissionDenied => format!("permission denied: {shown}"),
        _ => format!("{shown}: {e}"),
    }
}

fn plural(n: u64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn line_count(text: &[u8]) -> u64 {
    let newlines = text.iter().filter(|b| **b == b'\n').count() as u64;
    newlines + u64::from(!text.is_empty() && !text.ends_with(b"\n"))
}

fn human_size(n: u64) -> String {
    match n {
        0..=1023 => format!("{n} B"),
        1024..=1_048_575 => format!("{:.1} KiB", n as f64 / 1024.0),
        _ => format!("{:.1} MiB", n as f64 / 1_048_576.0),
    }
}

/// Cuts text to `cap` bytes on a char boundary, ending in `note` when it was cut.
fn cap_text(s: String, cap: usize, note: &str) -> String {
    if s.len() <= cap {
        return s;
    }
    format!("{}{note}", prefix(&s, cap))
}

/// Reads one line (without its `\n`) into `buf`, keeping at most `cap` bytes of it; returns
/// `Some(cut)` or `None` at end of file. Never holds more than `cap` bytes of a giant line.
fn next_line(r: &mut impl BufRead, buf: &mut Vec<u8>, cap: usize) -> std::io::Result<Option<bool>> {
    buf.clear();
    let mut cut = false;
    let mut any = false;
    loop {
        let avail = r.fill_buf()?;
        if avail.is_empty() {
            return Ok(any.then_some(cut));
        }
        any = true;
        let (take, used, done) = match avail.iter().position(|b| *b == b'\n') {
            Some(i) => (i, i + 1, true),
            None => (avail.len(), avail.len(), false),
        };
        let room = cap.saturating_sub(buf.len());
        if take > room {
            cut = true;
        }
        buf.extend_from_slice(&avail[..take.min(room)]);
        r.consume(used);
        if done {
            return Ok(Some(cut));
        }
    }
}

/// Lines left in the reader (a final line without `\n` counts), scanning at most `budget` bytes;
/// the flag is false when the budget ran out first (then the count is a lower bound).
fn count_rest(r: &mut impl BufRead, budget: u64) -> std::io::Result<(u64, bool)> {
    let mut n = 0u64;
    let mut last = b'\n';
    let mut scanned = 0u64;
    loop {
        let avail = r.fill_buf()?;
        if avail.is_empty() {
            return Ok((n + u64::from(last != b'\n'), true));
        }
        if scanned >= budget {
            return Ok((n, false));
        }
        let take = avail.len().min(usize::try_from(budget - scanned).unwrap_or(usize::MAX));
        n += avail[..take].iter().filter(|b| **b == b'\n').count() as u64;
        last = avail[take - 1];
        scanned += take as u64;
        r.consume(take);
    }
}

/// Writes `content` to `path`, the policy's resolved target. `O_NOFOLLOW` refuses a symlink that
/// appeared there between the check and the write (a background job swapping the file for a link).
fn write_resolved(path: &Path, content: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).custom_flags(libc::O_NOFOLLOW).open(path)?;
    f.write_all(content)
}

impl Fs {
    /// The path as the model should see it: relative to cwd when under it.
    fn shown(&self, p: &Path) -> String {
        match p.strip_prefix(&self.cwd) {
            Ok(rel) if rel.as_os_str().is_empty() => ".".into(),
            Ok(rel) => rel.display().to_string(),
            Err(_) => p.display().to_string(),
        }
    }

    pub fn read_file(&self, a: &Args) -> Result<ToolOutcome, String> {
        let p = resolve(&self.cwd, a.path("path")?);
        let shown = self.shown(&p);
        let offset = a.opt_u64("offset")?.unwrap_or(1).max(1);
        let limit = a.opt_u64("limit")?.unwrap_or(READ_LINE_DEFAULT).max(1);
        let meta = std::fs::metadata(&p).map_err(|e| io_err(&shown, e))?;
        if meta.is_dir() {
            return Err(format!("{shown} is a directory; use list_dir"));
        }
        if meta.len() == 0 {
            return Ok(ToolOutcome::ok(format!("{shown} is empty")));
        }
        let file = std::fs::File::open(&p).map_err(|e| io_err(&shown, e))?;
        let mut r = BufReader::with_capacity(64 * 1024, file);
        let head = r.fill_buf().map_err(|e| io_err(&shown, e))?;
        if head[..head.len().min(SNIFF)].contains(&0) {
            return Ok(ToolOutcome::ok(format!("{shown} is a binary file ({}); not shown", human_size(meta.len()))));
        }

        let mut out = String::new();
        let mut line = Vec::new();
        let mut line_no = 0u64;
        let mut last_shown = 0u64;
        let mut total: Option<(u64, bool)> = None;
        while let Some(cut) = next_line(&mut r, &mut line, LINE_CAP).map_err(|e| io_err(&shown, e))? {
            line_no += 1;
            if line_no < offset {
                continue;
            }
            let mut text = String::from_utf8_lossy(line.strip_suffix(b"\r").unwrap_or(&line)).into_owned();
            if cut {
                text = text.trim_end_matches('\u{FFFD}').to_string() + " [… line truncated]";
            }
            let entry = format!("{line_no:>6}\t{text}\n");
            let shown_lines = line_no - offset;
            if shown_lines >= limit || (out.len() + entry.len() > READ_BYTE_CAP && shown_lines > 0) {
                let (rest, exact) = count_rest(&mut r, COUNT_BUDGET).map_err(|e| io_err(&shown, e))?;
                total = Some((line_no + rest, exact));
                break;
            }
            out.push_str(&entry);
            last_shown = line_no;
        }
        if last_shown == 0 {
            return Err(format!("offset {offset} is past the end of {shown} ({})", plural(line_no, "line", "lines")));
        }
        if let Some((total, exact)) = total {
            let of = if exact { total.to_string() } else { format!("≥{total}") };
            out.push_str(&format!("[showing lines {offset}-{last_shown} of {of}; continue with offset {}]", last_shown + 1));
        }
        Ok(ToolOutcome::ok(out.trim_end_matches('\n').to_string()))
    }

    /// The unified diff of `old` against the file now at `path`, via `diff -u` (stdin as the old
    /// side, so nothing is staged on disk). Empty when `diff` is missing or fails.
    async fn unified_diff(&self, label: &str, old: &[u8], path: &Path) -> String {
        if !self.diff {
            return String::new();
        }
        let mut cmd = Command::new("diff");
        cmd.arg("-u").arg("--label").arg(label).arg("--label").arg(label).arg("-").arg(path);
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).kill_on_drop(true);
        let Ok(mut child) = cmd.spawn() else { return String::new() };
        let Some(mut stdin) = child.stdin.take() else { return String::new() };
        let Some(mut stdout) = child.stdout.take() else { return String::new() };
        // Both sides at once: a large file would fill one pipe while we block on the other.
        let feed = async move {
            let _ = stdin.write_all(old).await;
        };
        let mut out = Vec::new();
        let read = stdout.read_to_end(&mut out);
        let joined = tokio::time::timeout(Duration::from_secs(10), async { tokio::join!(feed, read, child.wait()) }).await;
        match joined {
            // 0 = identical, 1 = different; 2 = trouble.
            Ok((_, Ok(_), Ok(st))) if matches!(st.code(), Some(0 | 1)) => cap_text(String::from_utf8_lossy(&out).into_owned(), DIFF_CAP, "\n[… diff truncated]\n"),
            _ => String::new(),
        }
    }

    /// The policy check every write shares: where the bytes really land, or an error naming the
    /// path and the allowed roots.
    fn writable(&self, p: &Path, follow_final: bool) -> Result<PathBuf, String> {
        self.policy.check_write(p, follow_final).map_err(|e| format!("refused: {e}"))
    }

    pub async fn write_file(&self, a: &Args<'_>) -> Result<ToolOutcome, String> {
        let p = resolve(&self.cwd, a.path("path")?);
        let content = a.text("content")?;
        let shown = self.shown(&p);
        // A symlink is written through to the target the policy judged, never re-resolved.
        let target = self.writable(&p, true)?;
        if target.is_dir() {
            return Err(format!("{shown} is a directory"));
        }
        let old = match std::fs::read(&target) {
            Ok(bytes) => Some(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(io_err(&shown, e)),
        };
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| io_err(&self.shown(parent), e))?;
        }
        write_resolved(&target, content.as_bytes()).map_err(|e| io_err(&shown, e))?;
        let lines = plural(line_count(content.as_bytes()), "line", "lines");
        let (text, change) = match old {
            None => (format!("created {shown} ({lines})"), Some(FileChange { path: shown.clone(), kind: "add".into(), diff: cap_text(content.to_string(), DIFF_CAP, "\n[… truncated]\n") })),
            Some(old) if old == content.as_bytes() => (format!("{shown} unchanged (the content was identical)"), None),
            Some(old) => {
                let diff = self.unified_diff(&shown, &old, &target).await;
                (format!("wrote {shown} ({lines})"), Some(FileChange { path: shown.clone(), kind: "modified".into(), diff }))
            }
        };
        Ok(ToolOutcome { ok: true, text, changes: change.into_iter().collect(), ..Default::default() })
    }

    pub async fn edit_file(&self, a: &Args<'_>) -> Result<ToolOutcome, String> {
        let p = resolve(&self.cwd, a.path("path")?);
        let old = a.text("old")?;
        let new = a.text("new")?;
        let replace_all = a.opt_bool("replace_all")?.unwrap_or(false);
        let shown = self.shown(&p);
        if old.is_empty() {
            return Err("`old` is empty; to create or overwrite a whole file use write_file".into());
        }
        if old == new {
            return Err("`old` and `new` are identical; nothing to change".into());
        }
        let target = self.writable(&p, true)?;
        let bytes = std::fs::read(&target).map_err(|e| io_err(&shown, e))?;
        let text = String::from_utf8(bytes).map_err(|_| format!("{shown} is not UTF-8 text; use write_file to replace it whole"))?;

        // A model writes `\n`; a CRLF file needs the same edit spelled with `\r\n`.
        let crlf = |s: &str| s.replace("\r\n", "\n").replace('\n', "\r\n");
        let (needle, replacement) = if !text.contains(old) && text.contains("\r\n") && text.contains(&crlf(old)) { (crlf(old), crlf(new)) } else { (old.to_string(), new.to_string()) };

        let lines: Vec<u64> = match_lines(&text, &needle);
        match lines.len() {
            0 => return Err(not_found_hint(&text, old, &shown)),
            n if n > 1 && !replace_all => {
                let at: Vec<String> = lines.iter().take(10).map(|l| l.to_string()).collect();
                let more = if n > 10 { ", …" } else { "" };
                return Err(format!("`old` matches {n} times in {shown} (lines {}{more}); include more surrounding lines to make it unique, or set replace_all: true", at.join(", ")));
            }
            _ => {}
        }
        let updated = if replace_all { text.replace(&needle, &replacement) } else { text.replacen(&needle, &replacement, 1) };
        write_resolved(&target, updated.as_bytes()).map_err(|e| io_err(&shown, e))?;
        let diff = self.unified_diff(&shown, text.as_bytes(), &target).await;
        let n = lines.len() as u64;
        Ok(ToolOutcome {
            ok: true,
            text: format!("edited {shown} ({})", plural(n, "replacement", "replacements")),
            changes: vec![FileChange { path: shown, kind: "modified".into(), diff }],
            ..Default::default()
        })
    }

    pub fn delete(&self, a: &Args) -> Result<ToolOutcome, String> {
        let p = resolve(&self.cwd, a.path("path")?);
        let recursive = a.opt_bool("recursive")?.unwrap_or(false);
        let shown = self.shown(&p);
        self.writable(&p, false)?;
        // Not a root, and not a directory above one: `..` from a workspace nested in /tmp would
        // otherwise be "inside /tmp" and take the workspace with it.
        if p.parent().is_none() || self.policy.holds_root(&p) || self.cwd.starts_with(&p) {
            return Err(format!("refusing to delete {shown}: it is a writable root or holds one (the workspace itself), not something inside one"));
        }
        let meta = std::fs::symlink_metadata(&p).map_err(|e| io_err(&shown, e))?;
        let text = if meta.is_dir() {
            let entries = std::fs::read_dir(&p).map_err(|e| io_err(&shown, e))?.count() as u64;
            if entries > 0 && !recursive {
                return Err(format!("{shown} is a directory with {}; pass recursive: true to delete it and everything in it", plural(entries, "entry", "entries")));
            }
            if entries == 0 {
                std::fs::remove_dir(&p).map_err(|e| io_err(&shown, e))?;
            } else {
                std::fs::remove_dir_all(&p).map_err(|e| io_err(&shown, e))?;
            }
            format!("deleted directory {shown}/")
        } else {
            std::fs::remove_file(&p).map_err(|e| io_err(&shown, e))?;
            format!("deleted {shown}")
        };
        Ok(ToolOutcome { ok: true, text, changes: vec![FileChange { path: shown, kind: "delete".into(), diff: String::new() }], ..Default::default() })
    }

    pub fn list_dir(&self, a: &Args) -> Result<ToolOutcome, String> {
        let p = resolve(&self.cwd, a.opt_text("path")?.filter(|s| !s.trim().is_empty()).unwrap_or("."));
        let depth = a.opt_u64("depth")?.unwrap_or(1).clamp(1, 4) as usize;
        let shown = self.shown(&p);
        let meta = std::fs::metadata(&p).map_err(|e| io_err(&shown, e))?;
        if !meta.is_dir() {
            return Ok(ToolOutcome::ok(format!("{shown}  {} (a file, not a directory)", human_size(meta.len()))));
        }
        let mut out = Vec::new();
        let complete = list_into(&p, 0, depth, &mut out);
        let mut text = if out.is_empty() { format!("{shown} is empty") } else { out.join("\n") };
        if !complete {
            text.push_str(&format!("\n[… stopped at {LIST_CAP} entries; list a subdirectory or lower depth]"));
        }
        Ok(ToolOutcome::ok(text))
    }

    pub fn glob(&self, a: &Args) -> Result<ToolOutcome, String> {
        let pattern = a.path("pattern")?.trim();
        let start = resolve(&self.cwd, a.opt_text("path")?.filter(|s| !s.trim().is_empty()).unwrap_or("."));
        let (base, rest) = split_literal_prefix(&start, pattern);
        let pats: Vec<Pattern> = expand_braces(&rest).iter().map(|p| Pattern::compile(p)).filter(|p| !p.segs.is_empty()).collect();
        if pats.is_empty() {
            return Err("`pattern` matches nothing; give a pattern such as `src/**/*.rs`".into());
        }
        let enter_heavy = pats.iter().any(|p| p.segs.iter().any(|s| matches!(s, Seg::Name(n) if HEAVY_DIRS.iter().any(|h| n.iter().copied().eq(h.chars())))));
        let mut walk = Walk { pats: &pats, found: Vec::new(), more: 0, visited: 0, enter_heavy };
        let initial = walk.closure((0..pats.len()).map(|i| (i, 0)).collect());
        walk.dir(&base, initial);
        if walk.found.is_empty() {
            return Ok(ToolOutcome::ok(format!("no paths match `{pattern}` under {}", self.shown(&start))));
        }
        let mut lines: Vec<String> = walk.found.iter().map(|(p, dir)| format!("{}{}", self.shown(p), if *dir { "/" } else { "" })).collect();
        if walk.more > 0 {
            lines.push(format!("[… {} more; narrow the pattern]", walk.more));
        }
        if walk.visited >= GLOB_VISIT_CAP {
            lines.push(format!("[search stopped after {GLOB_VISIT_CAP} entries; narrow the pattern or the path]"));
        }
        Ok(ToolOutcome::ok(lines.join("\n")))
    }

    pub async fn grep(&self, a: &Args<'_>) -> Result<ToolOutcome, String> {
        let pattern = a.text("pattern")?;
        if pattern.is_empty() {
            return Err("`pattern` is empty".into());
        }
        let path = a.opt_text("path")?.filter(|s| !s.trim().is_empty()).unwrap_or(".");
        let glob = a.opt_text("glob")?.filter(|s| !s.trim().is_empty());
        let max = a.opt_u64("max")?.unwrap_or(GREP_DEFAULT).clamp(1, GREP_MAX) as usize;
        // The heavy directories are skipped on the way down, never when `path` names one: grep
        // would otherwise skip the very directory it was pointed at.
        let named = |d: &str| Path::new(path).components().any(|c| c.as_os_str() == d);
        let skipped: Vec<&str> = HEAVY_DIRS.iter().copied().filter(|d| !named(d)).collect();
        let mut cmd = match &self.rg {
            Some(rg) => {
                let mut c = Command::new(rg);
                c.args(["--line-number", "--with-filename", "--no-heading", "--color", "never", "--null", "--max-columns", "300", "--max-columns-preview"]);
                // rg skips what `.gitignore` says, which need not include these; a trailing `/`
                // matches directories only. The caller's glob comes after: the later one wins.
                for d in &skipped {
                    c.arg("--glob").arg(format!("!{d}/"));
                }
                if let Some(g) = glob {
                    c.arg("--glob").arg(g);
                }
                c
            }
            None => {
                let mut c = Command::new("grep");
                c.args(["-rnHEI", "--null"]);
                for d in &skipped {
                    c.arg(format!("--exclude-dir={d}"));
                }
                if let Some(g) = glob {
                    c.arg(format!("--include={g}"));
                }
                c
            }
        };
        cmd.arg("-e").arg(pattern).arg("--").arg(path).current_dir(&self.cwd);
        cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
        let mut child = cmd.spawn().map_err(|e| format!("could not run {}: {e}", if self.rg.is_some() { "rg" } else { "grep" }))?;
        let (Some(stdout), Some(mut stderr)) = (child.stdout.take(), child.stderr.take()) else {
            return Err("could not read the search output".into());
        };

        let mut hits: Vec<String> = Vec::new();
        let mut more = false;
        let collect = async {
            let mut r = tokio::io::BufReader::new(stdout);
            let mut line = Vec::new();
            loop {
                line.clear();
                match r.read_until(b'\n', &mut line).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if hits.len() >= max {
                            more = true;
                            break;
                        }
                        if let Some(hit) = format_hit(&line) {
                            hits.push(hit);
                        }
                    }
                }
            }
        };
        let mut err_text = Vec::new();
        let errors = async {
            let _ = (&mut stderr).take(4096).read_to_end(&mut err_text).await;
        };
        let timed_out = tokio::time::timeout(GREP_TIMEOUT, async { tokio::join!(collect, errors) }).await.is_err();
        let status = if more || timed_out {
            let _ = child.start_kill();
            None
        } else {
            child.wait().await.ok().and_then(|s| s.code())
        };
        let err_text = String::from_utf8_lossy(&err_text).trim().to_string();

        if hits.is_empty() {
            return match status {
                Some(1) | None if !timed_out => Ok(ToolOutcome::ok(format!("no matches for `{pattern}` in {path}"))),
                _ if timed_out => Err(format!("the search took longer than {}s; narrow the path or the glob", GREP_TIMEOUT.as_secs())),
                _ => Err(if err_text.is_empty() { "the search failed".into() } else { err_text }),
            };
        }
        let mut text = hits.join("\n");
        if more {
            text.push_str(&format!("\n[… more matches not shown; narrow the pattern or the path, or raise max (now {max})]"));
        } else if timed_out {
            text.push_str(&format!("\n[search stopped after {}s; results are incomplete]", GREP_TIMEOUT.as_secs()));
        }
        Ok(ToolOutcome::ok(text))
    }
}

/// `path\0line:text` (both rg and grep with `--null`) → `path:line: text`.
fn format_hit(raw: &[u8]) -> Option<String> {
    let raw = raw.strip_suffix(b"\n").unwrap_or(raw);
    let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
    let nul = raw.iter().position(|b| *b == 0)?;
    let path = String::from_utf8_lossy(&raw[..nul]);
    let path = path.strip_prefix("./").unwrap_or(&path);
    let rest = String::from_utf8_lossy(&raw[nul + 1..]);
    let (line, text) = rest.split_once(':')?;
    let text = cap_text(text.to_string(), GREP_LINE_CAP, " [… line truncated]");
    Some(format!("{path}:{line}: {text}"))
}

/// The 1-based line of every (non-overlapping) occurrence of `needle`.
fn match_lines(text: &str, needle: &str) -> Vec<u64> {
    let mut lines = Vec::new();
    let mut line = 1u64;
    let mut scanned = 0usize;
    for (at, _) in text.match_indices(needle) {
        line += text[scanned..at].bytes().filter(|b| *b == b'\n').count() as u64;
        scanned = at;
        lines.push(line);
    }
    lines
}

/// Why `old` was not found, with the likeliest fix.
fn not_found_hint(text: &str, old: &str, shown: &str) -> String {
    let mut msg = format!("`old` was not found in {shown}");
    let trimmed = old.trim();
    if trimmed != old && !trimmed.is_empty() && text.contains(trimmed) {
        msg.push_str("; it does match with its leading/trailing whitespace removed — copy the text exactly");
    } else {
        msg.push_str("; it must match the file exactly, including whitespace and indentation — read the file again and copy the text");
    }
    // Give a bounded window around the best matching line, so the next edit can be repaired
    // from the actual file instead of blindly retrying the same stale patch.
    let actual: Vec<&str> = text.lines().collect();
    let wanted: Vec<&str> = old.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let score = |line: &str| wanted.iter().map(|w| {
        if line.trim() == *w { 1000 + w.len() } else { line.split_whitespace().filter(|word| w.split_whitespace().any(|x| x == *word)).map(str::len).sum() }
    }).max().unwrap_or(0);
    if let Some((at, _)) = actual.iter().enumerate().map(|(i, line)| (i, score(line))).filter(|(_, n)| *n > 0).max_by_key(|(_, n)| *n) {
        msg.push_str("\nNearby file context (line numbers):");
        for (i, line) in actual.iter().enumerate().take((at + 4).min(actual.len())).skip(at.saturating_sub(3)) {
            msg.push_str(&format!("\n{}: {}", i + 1, prefix(line, 240)));
        }
    }
    msg
}

/// Appends `dir`'s tree to `out`; false when the entry cap was hit.
fn list_into(dir: &Path, level: usize, depth: usize, out: &mut Vec<String>) -> bool {
    let indent = "  ".repeat(level);
    let mut entries: Vec<std::fs::DirEntry> = match std::fs::read_dir(dir) {
        Ok(rd) => rd.filter_map(|e| e.ok()).collect(),
        Err(e) => {
            out.push(format!("{indent}(unreadable: {e})"));
            return true;
        }
    };
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        if out.len() >= LIST_CAP {
            return false;
        }
        let name = e.file_name().to_string_lossy().into_owned();
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_symlink() {
            let target = std::fs::read_link(e.path()).map(|t| t.display().to_string()).unwrap_or_else(|_| "?".into());
            out.push(format!("{indent}{name} -> {target}"));
        } else if ft.is_dir() {
            let expand = level + 1 < depth;
            if expand && HEAVY_DIRS.contains(&name.as_str()) {
                out.push(format!("{indent}{name}/ (not expanded)"));
            } else {
                out.push(format!("{indent}{name}/"));
                if expand && !list_into(&e.path(), level + 1, depth, out) {
                    return false;
                }
            }
        } else {
            let size = e.metadata().map(|m| human_size(m.len())).unwrap_or_default();
            out.push(format!("{indent}{name}  {size}"));
        }
    }
    true
}

// ---- glob ----------------------------------------------------------------------------------------

/// One `/`-separated piece of a glob: `**` (any number of directories) or a name pattern.
#[derive(Debug, Clone, PartialEq)]
enum Seg {
    Deep,
    Name(Vec<char>),
}

#[derive(Debug, Clone, PartialEq)]
struct Pattern {
    segs: Vec<Seg>,
}

impl Pattern {
    fn compile(p: &str) -> Pattern {
        let mut segs: Vec<Seg> = Vec::new();
        for s in p.split('/').filter(|s| !s.is_empty() && *s != ".") {
            if s == "**" {
                if segs.last() != Some(&Seg::Deep) {
                    segs.push(Seg::Deep);
                }
            } else {
                segs.push(Seg::Name(s.chars().collect()));
            }
        }
        // `dir/**` means everything inside dir, not dir itself.
        if segs.last() == Some(&Seg::Deep) {
            segs.push(Seg::Name(vec!['*']));
        }
        Pattern { segs }
    }
}

fn has_magic(s: &str) -> bool {
    s.contains(['*', '?', '[', '{'])
}

/// Moves the pattern's leading literal directories (`src/tools/*.rs` → `src/tools`, `/etc/*`,
/// `../x/*`) onto the base, so the walk starts where it can match instead of at cwd.
fn split_literal_prefix(start: &Path, pattern: &str) -> (PathBuf, String) {
    let mut base = if pattern.starts_with('/') { PathBuf::from("/") } else { start.to_path_buf() };
    let parts: Vec<&str> = pattern.split('/').filter(|s| !s.is_empty()).collect();
    let mut i = 0;
    while i + 1 < parts.len() && !has_magic(parts[i]) {
        base.push(parts[i]);
        i += 1;
    }
    (super::sandbox::normalize(&base), parts[i..].join("/"))
}

/// `{a,b}` alternatives, expanded up front (nested ones too), at most 64 patterns.
fn expand_braces(p: &str) -> Vec<String> {
    let mut out = Vec::new();
    expand_into(p, &mut out);
    out
}

fn expand_into(p: &str, out: &mut Vec<String>) {
    if out.len() >= 64 {
        return;
    }
    let Some((open, close, alts)) = first_brace_group(p) else {
        out.push(p.to_string());
        return;
    };
    for alt in alts {
        expand_into(&format!("{}{}{}", &p[..open], alt, &p[close + 1..]), out);
    }
}

/// The first `{…}` with a top-level comma: its byte span and alternatives. A brace without a comma
/// or without its closing partner is literal.
fn first_brace_group(p: &str) -> Option<(usize, usize, Vec<&str>)> {
    let mut search_from = 0;
    while let Some(rel) = p[search_from..].find('{') {
        let open = search_from + rel;
        let mut depth = 0;
        let mut cuts = vec![open + 1];
        for (i, c) in p[open..].char_indices() {
            let at = open + i;
            match c {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        if cuts.len() > 1 {
                            let mut alts = Vec::new();
                            let mut from = cuts[0];
                            for &cut in &cuts[1..] {
                                alts.push(&p[from..cut - 1]);
                                from = cut;
                            }
                            alts.push(&p[from..at]);
                            return Some((open, at, alts));
                        }
                        break;
                    }
                }
                ',' if depth == 1 => cuts.push(at + 1),
                _ => {}
            }
        }
        search_from = open + 1;
    }
    None
}

/// Matches one name against one segment: `*`, `?`, `[abc]`, `[a-z]`, `[!x]`, `\` escapes.
fn segment_matches(p: &[char], s: &[char]) -> bool {
    let (mut pi, mut si) = (0, 0);
    // Where the last `*` was and how much of `s` it has swallowed so far, for backtracking.
    let mut star: Option<(usize, usize)> = None;
    while si < s.len() {
        if pi < p.len() {
            match p[pi] {
                '*' => {
                    star = Some((pi, si));
                    pi += 1;
                    continue;
                }
                '?' => {
                    pi += 1;
                    si += 1;
                    continue;
                }
                '[' => {
                    if let Some((hit, next)) = class_matches(p, pi, s[si]) {
                        if hit {
                            pi = next;
                            si += 1;
                            continue;
                        }
                    } else if s[si] == '[' {
                        pi += 1;
                        si += 1;
                        continue;
                    }
                }
                '\\' if pi + 1 < p.len() => {
                    if p[pi + 1] == s[si] {
                        pi += 2;
                        si += 1;
                        continue;
                    }
                }
                c if c == s[si] => {
                    pi += 1;
                    si += 1;
                    continue;
                }
                _ => {}
            }
        }
        match star {
            Some((sp, ss)) => {
                pi = sp + 1;
                si = ss + 1;
                star = Some((sp, ss + 1));
            }
            None => return false,
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// A `[…]` class at `p[start]`: whether `c` is in it and the index after `]`; `None` when the
/// class is unterminated (then `[` is literal).
fn class_matches(p: &[char], start: usize, c: char) -> Option<(bool, usize)> {
    let mut i = start + 1;
    let negate = matches!(p.get(i), Some('!') | Some('^'));
    if negate {
        i += 1;
    }
    let mut hit = false;
    let mut first = true;
    while i < p.len() {
        if p[i] == ']' && !first {
            return Some((hit != negate, i + 1));
        }
        first = false;
        if i + 2 < p.len() && p[i + 1] == '-' && p[i + 2] != ']' {
            hit |= p[i] <= c && c <= p[i + 2];
            i += 3;
        } else {
            hit |= p[i] == c;
            i += 1;
        }
    }
    None
}

/// A glob walk: a set of (pattern, segment) positions per directory, so only directories some
/// pattern can still match below are entered.
struct Walk<'a> {
    pats: &'a [Pattern],
    found: Vec<(PathBuf, bool)>,
    more: usize,
    visited: usize,
    enter_heavy: bool,
}

type States = Vec<(usize, usize)>;

impl Walk<'_> {
    /// Adds the positions reachable by letting a `**` match zero directories.
    fn closure(&self, mut states: States) -> States {
        let mut i = 0;
        while i < states.len() {
            let (p, s) = states[i];
            if self.pats[p].segs.get(s) == Some(&Seg::Deep) && !states.contains(&(p, s + 1)) {
                states.push((p, s + 1));
            }
            i += 1;
        }
        states
    }

    fn step(&self, states: &States, name: &[char]) -> States {
        let mut next: States = Vec::new();
        for &(p, s) in states {
            let to = match self.pats[p].segs.get(s) {
                Some(Seg::Deep) => (p, s),
                Some(Seg::Name(n)) if segment_matches(n, name) => (p, s + 1),
                _ => continue,
            };
            if !next.contains(&to) {
                next.push(to);
            }
        }
        self.closure(next)
    }

    fn dir(&mut self, dir: &Path, states: States) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        let mut entries: Vec<std::fs::DirEntry> = rd.filter_map(|e| e.ok()).collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            if self.visited >= GLOB_VISIT_CAP {
                return;
            }
            self.visited += 1;
            let name = e.file_name().to_string_lossy().into_owned();
            if name == ".git" {
                continue;
            }
            let chars: Vec<char> = name.chars().collect();
            let next = self.step(&states, &chars);
            if next.is_empty() {
                continue;
            }
            let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
            if next.iter().any(|&(p, s)| s == self.pats[p].segs.len()) {
                if self.found.len() < GLOB_CAP {
                    self.found.push((e.path(), is_dir));
                } else {
                    self.more += 1;
                }
            }
            let deeper = next.iter().any(|&(p, s)| s < self.pats[p].segs.len());
            if is_dir && deeper && (self.enter_heavy || !HEAVY_DIRS.contains(&name.as_str())) {
                self.dir(&e.path(), next);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::Sandbox;
    use serde_json::json;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bija-fs-{tag}-{}", crate::types::uuid_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    fn fs_in(dir: &Path) -> Fs {
        Fs { cwd: dir.to_path_buf(), policy: Policy::with_roots(Sandbox::WorkspaceWrite, vec![dir.to_path_buf()]), diff: true, rg: crate::tools::which("rg") }
    }

    fn args(v: &serde_json::Value) -> Args<'_> {
        Args::new(v).unwrap()
    }

    fn put(dir: &Path, rel: &str, content: &str) {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    #[test]
    fn stale_edit_reports_bounded_actual_context() {
        let text = "header\nfn callback() {\n    verify_nonce();\n    complete_login();\n}\nfooter\n";
        let hint = not_found_hint(text, "fn callback() {\n    verify_old_nonce();", "callback.rs");
        assert!(hint.contains("Nearby file context") && hint.contains("3:     verify_nonce();"));
        assert!(hint.len() < 2200);
    }

    #[test]
    fn segment_matcher_table() {
        let m = |p: &str, s: &str| segment_matches(&p.chars().collect::<Vec<_>>(), &s.chars().collect::<Vec<_>>());
        assert!(m("*.rs", "main.rs") && m("*", "") && m("a*b*c", "aXXbYYc") && m("*.md", ".hidden.md"));
        assert!(!m("*.rs", "main.rsx") && !m("a*b", "ac"));
        assert!(m("[ab]?.txt", "a1.txt") && m("[ab]?.txt", "bz.txt"));
        assert!(!m("[ab]?.txt", "c1.txt") && !m("[ab]?.txt", "a12.txt") && !m("[ab]?.txt", "a.txt"));
        assert!(m("[a-c]x", "bx") && !m("[!a-c]x", "bx") && m("[!a-c]x", "dx"));
        assert!(m("[]]", "]") && m("x[", "x[") && m("\\*", "*") && !m("\\*", "a"));
        assert!(m("é?", "éü"));
    }

    #[test]
    fn braces_expand_and_stay_literal_without_a_comma() {
        assert_eq!(expand_braces("*.{rs,toml}"), vec!["*.rs", "*.toml"]);
        assert_eq!(expand_braces("{a,b{c,d}}x"), vec!["ax", "bcx", "bdx"]);
        assert_eq!(expand_braces("{x}"), vec!["{x}"]);
        assert_eq!(expand_braces("a{b"), vec!["a{b"]);
    }

    #[test]
    fn glob_walks_only_what_can_match() {
        let d = scratch("glob");
        for f in ["src/main.rs", "src/tools/mod.rs", "src/tools/fs.rs", "src/readme.md", "README.md", "a1.txt", "bz.txt", "c1.txt", "a12.txt", ".git/HEAD.md", "target/x.rs", "docs/x.md"] {
            put(&d, f, "x");
        }
        let fs = fs_in(&d);
        let run = |pat: &str| fs.glob(&args(&json!({"pattern": pat}))).unwrap().text;
        assert_eq!(run("src/**/*.rs"), "src/main.rs\nsrc/tools/fs.rs\nsrc/tools/mod.rs");
        assert_eq!(run("*.md"), "README.md");
        assert_eq!(run("[ab]?.txt"), "a1.txt\nbz.txt");
        let all_md = run("**/*.md");
        assert!(all_md.contains("docs/x.md") && all_md.contains("src/readme.md") && !all_md.contains(".git"), "{all_md}");
        assert!(!run("**/*.rs").contains("target/"), "target is not entered through **");
        assert_eq!(run("target/*.rs"), "target/x.rs", "a pattern naming target enters it");
        assert_eq!(run("src/**"), "src/main.rs\nsrc/readme.md\nsrc/tools/\nsrc/tools/fs.rs\nsrc/tools/mod.rs");
        assert_eq!(run("src/*.{rs,md}"), "src/main.rs\nsrc/readme.md");
        assert!(run("nothing*").starts_with("no paths match"));
        let under = fs.glob(&args(&json!({"pattern": "*.rs", "path": "src/tools"}))).unwrap().text;
        assert_eq!(under, "src/tools/fs.rs\nsrc/tools/mod.rs");
    }

    #[tokio::test]
    async fn edit_file_requires_an_exact_unique_match() {
        let d = scratch("edit");
        put(&d, "a.txt", "one\ntwo\none\nthree\n");
        let fs = fs_in(&d);
        let none = fs.edit_file(&args(&json!({"path": "a.txt", "old": "four", "new": "4"}))).await.unwrap_err();
        assert!(none.contains("not found") && none.contains("exactly"), "{none}");
        let two = fs.edit_file(&args(&json!({"path": "a.txt", "old": "one", "new": "1"}))).await.unwrap_err();
        assert!(two.contains("2 times") && two.contains("lines 1, 3") && two.contains("replace_all"), "{two}");
        let ok = fs.edit_file(&args(&json!({"path": "a.txt", "old": "two\n", "new": "2\n"}))).await.unwrap();
        assert!(ok.ok && ok.text.contains("1 replacement"), "{}", ok.text);
        let c = &ok.changes[0];
        assert_eq!((c.path.as_str(), c.kind.as_str()), ("a.txt", "modified"));
        assert!(c.diff.contains("--- a.txt") && c.diff.contains("-two") && c.diff.contains("+2"), "{}", c.diff);
        let all = fs.edit_file(&args(&json!({"path": "a.txt", "old": "one", "new": "1", "replace_all": true}))).await.unwrap();
        assert!(all.text.contains("2 replacements"));
        assert_eq!(std::fs::read_to_string(d.join("a.txt")).unwrap(), "1\n2\n1\nthree\n");
        assert!(fs.edit_file(&args(&json!({"path": "a.txt", "new": "x"}))).await.unwrap_err().contains("`old` is required"));
        let ws = fs.edit_file(&args(&json!({"path": "a.txt", "old": "  three\n", "new": "3"}))).await.unwrap_err();
        assert!(ws.contains("whitespace removed"), "{ws}");
    }

    #[tokio::test]
    async fn edit_file_handles_crlf_files() {
        let d = scratch("crlf");
        put(&d, "w.txt", "a\r\nb\r\nc\r\n");
        let fs = fs_in(&d);
        fs.edit_file(&args(&json!({"path": "w.txt", "old": "a\nb", "new": "x\ny"}))).await.unwrap();
        assert_eq!(std::fs::read_to_string(d.join("w.txt")).unwrap(), "x\r\ny\r\nc\r\n");
    }

    #[tokio::test]
    async fn write_file_creates_parents_and_reports_add_or_modified() {
        let d = scratch("write");
        let fs = fs_in(&d);
        let add = fs.write_file(&args(&json!({"path": "new/dir/f.txt", "content": "hello\n"}))).await.unwrap();
        assert_eq!(add.changes[0].kind, "add");
        assert_eq!(add.changes[0].diff, "hello\n");
        assert_eq!(add.changes[0].path, "new/dir/f.txt");
        let m = fs.write_file(&args(&json!({"path": "new/dir/f.txt", "content": "bye\n"}))).await.unwrap();
        assert_eq!(m.changes[0].kind, "modified");
        assert!(m.changes[0].diff.contains("-hello") && m.changes[0].diff.contains("+bye"));
        let same = fs.write_file(&args(&json!({"path": "new/dir/f.txt", "content": "bye\n"}))).await.unwrap();
        assert!(same.changes.is_empty() && same.text.contains("unchanged"));
        let empty = fs.write_file(&args(&json!({"path": "e.txt", "content": ""}))).await.unwrap();
        assert!(empty.ok);
        assert!(fs.write_file(&args(&json!({"path": "x.txt"}))).await.unwrap_err().contains("`content` is required"));
    }

    #[tokio::test]
    async fn without_diff_installed_the_diff_is_empty() {
        let d = scratch("nodiff");
        put(&d, "f.txt", "a\n");
        let mut fs = fs_in(&d);
        fs.diff = false;
        let m = fs.write_file(&args(&json!({"path": "f.txt", "content": "b\n"}))).await.unwrap();
        assert_eq!((m.changes[0].kind.as_str(), m.changes[0].diff.as_str()), ("modified", ""));
    }

    #[test]
    fn read_file_pages_numbers_and_caps() {
        let d = scratch("read");
        let body: String = (1..=3000).map(|i| format!("line {i}\n")).collect();
        put(&d, "big.txt", &body);
        let fs = fs_in(&d);
        let first = fs.read_file(&args(&json!({"path": "big.txt"}))).unwrap().text;
        assert!(first.starts_with("     1\tline 1\n"), "{}", &first[..40]);
        assert!(first.contains("  2000\tline 2000\n") && !first.contains("line 2001\n"));
        assert!(first.ends_with("[showing lines 1-2000 of 3000; continue with offset 2001]"), "{}", &first[first.len() - 80..]);
        let page = fs.read_file(&args(&json!({"path": "big.txt", "offset": 2999, "limit": "5"}))).unwrap().text;
        assert_eq!(page, "  2999\tline 2999\n  3000\tline 3000");
        let past = fs.read_file(&args(&json!({"path": "big.txt", "offset": 5000}))).unwrap_err();
        assert!(past.contains("past the end") && past.contains("3000 lines"), "{past}");
        // The byte cap: 2000 lines of 100 bytes is far over 64 KiB.
        put(&d, "wide.txt", &format!("{}\n", "x".repeat(99)).repeat(2000));
        let wide = fs.read_file(&args(&json!({"path": "wide.txt"}))).unwrap().text;
        assert!(wide.len() <= READ_BYTE_CAP + 200 && wide.contains("continue with offset"), "{}", wide.len());
        put(&d, "long.txt", &"y".repeat(10_000));
        let long = fs.read_file(&args(&json!({"path": "long.txt"}))).unwrap().text;
        assert!(long.ends_with("[… line truncated]") && long.len() < 2100);
    }

    #[test]
    fn read_file_reports_binary_empty_dirs_and_missing() {
        let d = scratch("bin");
        std::fs::write(d.join("blob.bin"), [0x7f, b'E', 0, 1, 2, 3]).unwrap();
        put(&d, "empty.txt", "");
        let fs = fs_in(&d);
        let bin = fs.read_file(&args(&json!({"path": "blob.bin"}))).unwrap().text;
        assert!(bin.contains("binary") && bin.contains("6 B"), "{bin}");
        assert!(fs.read_file(&args(&json!({"path": "empty.txt"}))).unwrap().text.contains("empty"));
        assert!(fs.read_file(&args(&json!({"path": "."}))).unwrap_err().contains("list_dir"));
        assert!(fs.read_file(&args(&json!({"path": "nope.txt"}))).unwrap_err().contains("no such file"));
        assert!(fs.read_file(&args(&json!({}))).unwrap_err().contains("`path` is required"));
        assert!(fs.read_file(&args(&json!({"path": "empty.txt", "offset": -3}))).unwrap_err().contains("non-negative"));
    }

    #[test]
    fn list_dir_respects_depth_and_does_not_expand_heavy_dirs() {
        let d = scratch("list");
        for f in ["src/a.rs", "src/deep/b.rs", "target/debug/x", ".git/HEAD", "README.md"] {
            put(&d, f, "12345");
        }
        let fs = fs_in(&d);
        let one = fs.list_dir(&args(&json!({}))).unwrap().text;
        assert_eq!(one, ".git/\nREADME.md  5 B\nsrc/\ntarget/");
        let two = fs.list_dir(&args(&json!({"depth": 2}))).unwrap().text;
        assert_eq!(two, ".git/ (not expanded)\nREADME.md  5 B\nsrc/\n  a.rs  5 B\n  deep/\ntarget/ (not expanded)");
        let three = fs.list_dir(&args(&json!({"path": "src", "depth": 9}))).unwrap().text;
        assert_eq!(three, "a.rs  5 B\ndeep/\n  b.rs  5 B");
    }

    #[test]
    fn delete_needs_recursive_for_full_dirs_and_never_deletes_a_root() {
        let d = scratch("del");
        put(&d, "f.txt", "x");
        put(&d, "dir/inner.txt", "x");
        std::fs::create_dir_all(d.join("empty")).unwrap();
        let fs = fs_in(&d);
        let ok = fs.delete(&args(&json!({"path": "f.txt"}))).unwrap();
        assert_eq!((ok.changes[0].kind.as_str(), ok.changes[0].path.as_str()), ("delete", "f.txt"));
        assert!(!d.join("f.txt").exists());
        fs.delete(&args(&json!({"path": "empty"}))).unwrap();
        let refused = fs.delete(&args(&json!({"path": "dir"}))).unwrap_err();
        assert!(refused.contains("recursive: true") && refused.contains("1 entry"), "{refused}");
        fs.delete(&args(&json!({"path": "dir", "recursive": "true"}))).unwrap();
        assert!(!d.join("dir").exists());
        assert!(fs.delete(&args(&json!({"path": ".", "recursive": true}))).unwrap_err().contains("writable root"));
        assert!(fs.delete(&args(&json!({"path": "gone"}))).unwrap_err().contains("no such file"));
    }

    /// A workspace nested inside another root (/tmp): its parent is "inside /tmp", but deleting it
    /// would take the workspace with it.
    #[test]
    fn delete_refuses_a_directory_that_holds_the_workspace() {
        let parent = scratch("nested");
        let ws = parent.join("ws");
        put(&ws, "keep.txt", "x");
        let fs = Fs { cwd: ws.clone(), policy: Policy::with_roots(Sandbox::WorkspaceWrite, vec![ws.clone(), parent.clone()]), diff: false, rg: None };
        for path in ["..".to_string(), parent.display().to_string()] {
            let err = fs.delete(&args(&json!({"path": path, "recursive": true}))).unwrap_err();
            assert!(err.contains("holds one"), "{path}: {err}");
        }
        assert!(ws.join("keep.txt").exists());
        put(&parent, "other/x.txt", "x");
        assert!(fs.delete(&args(&json!({"path": parent.join("other").display().to_string(), "recursive": true}))).is_ok(), "a sibling is fine");
    }

    /// A link into the workspace is written through; a dangling one out of it is refused and its
    /// target never created.
    #[tokio::test]
    async fn writes_follow_links_only_to_where_the_policy_allows() {
        let d = scratch("links");
        let outside = scratch("links-out");
        let fs = fs_in(&d);
        put(&d, "real.txt", "old\n");
        std::os::unix::fs::symlink(d.join("real.txt"), d.join("alias.txt")).unwrap();
        fs.write_file(&args(&json!({"path": "alias.txt", "content": "new\n"}))).await.unwrap();
        assert_eq!(std::fs::read_to_string(d.join("real.txt")).unwrap(), "new\n");
        assert!(std::fs::symlink_metadata(d.join("alias.txt")).unwrap().file_type().is_symlink(), "the link stays a link");
        fs.edit_file(&args(&json!({"path": "alias.txt", "old": "new", "new": "edited"}))).await.unwrap();
        assert_eq!(std::fs::read_to_string(d.join("real.txt")).unwrap(), "edited\n");

        std::os::unix::fs::symlink(outside.join("escaped.txt"), d.join("link")).unwrap();
        let err = fs.write_file(&args(&json!({"path": "link", "content": "x"}))).await.unwrap_err();
        assert!(err.starts_with("refused:") && err.contains("outside the writable roots"), "{err}");
        assert!(!outside.join("escaped.txt").exists());
        // Deleting the dangling link itself is fine: it lives in the workspace.
        fs.delete(&args(&json!({"path": "link"}))).unwrap();
        assert!(std::fs::symlink_metadata(d.join("link")).is_err());
    }

    #[test]
    fn counting_the_rest_stops_at_its_budget() {
        let text = "a\nb\nc\nd";
        assert_eq!(count_rest(&mut text.as_bytes(), 1 << 20).unwrap(), (4, true));
        assert_eq!(count_rest(&mut text.as_bytes(), 4).unwrap(), (2, false), "a lower bound");
        assert_eq!(count_rest(&mut text.as_bytes(), text.len() as u64).unwrap(), (4, true), "a budget that ends at the end is exact");
        assert_eq!(count_rest(&mut "".as_bytes(), 0).unwrap(), (0, true));
    }

    #[tokio::test]
    async fn grep_formats_hits_and_caps_them() {
        let d = scratch("grep");
        put(&d, "src/a.rs", "fn main() {}\nlet x = 1;\nfn helper() {}\n");
        put(&d, "notes.md", "fn in markdown\n");
        put(&d, "target/debug/gen.rs", "fn generated() {}\n");
        put(&d, "web/node_modules/dep/index.js", "fn vendored() {}\n");
        let mut fs = fs_in(&d);
        for rg in [crate::tools::which("rg"), None] {
            fs.rg = rg;
            let all = fs.grep(&args(&json!({"pattern": "fn [a-z]+\\("}))).await.unwrap().text;
            assert_eq!(all, "src/a.rs:1: fn main() {}\nsrc/a.rs:3: fn helper() {}", "the heavy dirs are skipped, rg={:?}", fs.rg);
            let inside = fs.grep(&args(&json!({"pattern": "fn [a-z]+\\(", "path": "target"}))).await.unwrap().text;
            assert_eq!(inside, "target/debug/gen.rs:1: fn generated() {}", "unless the path names one, rg={:?}", fs.rg);
            let globbed = fs.grep(&args(&json!({"pattern": "fn", "glob": "*.md"}))).await.unwrap().text;
            assert_eq!(globbed, "notes.md:1: fn in markdown");
            let capped = fs.grep(&args(&json!({"pattern": "fn", "max": 1}))).await.unwrap().text;
            assert!(capped.lines().count() == 2 && capped.contains("raise max"), "{capped}");
            let none = fs.grep(&args(&json!({"pattern": "zzz_nope"}))).await.unwrap().text;
            assert!(none.starts_with("no matches"), "{none}");
            assert!(fs.grep(&args(&json!({"pattern": "("}))).await.is_err(), "a bad regex is an error, not a crash");
        }
    }

    #[test]
    fn hit_lines_keep_colons_in_the_text() {
        assert_eq!(format_hit(b"./a/b.rs\x0012:let x: u8 = 1;\n").as_deref(), Some("a/b.rs:12: let x: u8 = 1;"));
        assert_eq!(format_hit(b"no-nul-here"), None);
    }
}
