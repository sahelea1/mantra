//! Workspace isolation: every parallel worker gets its own git worktree; phases merge at the gate.
//! All functions are blocking and meant to run on a background thread.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Workspace {
    pub worktree: bool,
    /// The user's repository (or project dir in shared mode).
    pub repo: PathBuf,
    /// Where integration happens (gate + finale agents work here).
    pub integ: PathBuf,
    /// Run branch (worktree mode).
    pub branch: String,
    pub base_branch: String,
    pub git_common_dir: Option<PathBuf>,
    pub note: Option<String>,
    /// The commit the run branch started from (worktree mode): the base of the run's whole diff.
    /// Absent in a state file written before it existed.
    #[serde(default)]
    pub base_commit: Option<String>,
    /// The repo had no commit at all (`git init` and nothing else): `setup` made an empty one so
    /// the worktrees had something to branch from. Logged once, when the run starts.
    #[serde(default)]
    pub initial_commit: bool,
}

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("git not available: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(format!("git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim()))
    }
}

/// Repository-local policy only: never copy global settings or credentials into a prompt.
fn project_values(dir: &Path, key: &str) -> Result<Vec<String>, String> {
    let out = Command::new("git").arg("-C").arg(dir).args(["config", "--local", "-z", "--get-all", key]).stdin(Stdio::null()).output().map_err(|e| e.to_string())?;
    if out.status.code() == Some(1) { return Ok(vec![]); }
    if !out.status.success() { return Err(format!("cannot read project policy {key}")); }
    Ok(String::from_utf8_lossy(&out.stdout).split('\0').filter(|s| !s.is_empty()).map(str::to_string).collect())
}

/// Git also resolves the committer while preparing a merge with --no-commit.
/// Use one identity policy for every command that can create or prepare a commit.
fn identity_args(dir: &Path) -> Vec<String> {
    let mut owned: Vec<String> = vec![];
    for (key, fallback) in [("user.name", "Mantra"), ("user.email", "mantra@localhost")] {
        if git(dir, &["config", key]).map(|v| v.is_empty()).unwrap_or(true) {
            owned.extend(["-c".into(), format!("{key}={fallback}")]);
        }
    }
    owned
}

fn git_with_identity(dir: &Path, args: &[&str]) -> Result<String, String> {
    let mut owned = identity_args(dir);
    owned.extend(args.iter().map(|s| s.to_string()));
    git(dir, &owned.iter().map(String::as_str).collect::<Vec<_>>())
}

fn commit(dir: &Path, args: &[&str]) -> Result<(), String> {
    let mut owned: Vec<String> = vec![];
    owned.push("commit".into());
    owned.extend(args.iter().map(|s| s.to_string()));
    for trailer in project_values(dir, "mantra.commitTrailer")? {
        let valid = trailer.split_once(':').map(|(k, v)| !k.is_empty() && k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') && !v.trim().is_empty()).unwrap_or(false);
        if !valid || trailer.chars().any(char::is_control) { return Err("invalid mantra.commitTrailer: use one Key: value per git config entry".into()); }
        owned.extend(["--trailer".into(), trailer]);
    }
    git_with_identity(dir, &owned.iter().map(String::as_str).collect::<Vec<_>>()).map(|_| ())
}

fn decision_logs(dir: &Path) -> Result<Vec<String>, String> {
    let mut paths = vec!["DECISIONS.md".to_string()];
    paths.extend(project_values(dir, "mantra.decisionLog")?);
    if paths.len() > 33 { return Err("too many mantra.decisionLog paths (maximum 32)".into()); }
    for path in &paths {
        if path.is_empty() || !Path::new(path).components().all(|c| matches!(c, std::path::Component::Normal(_))) || path.split('/').any(|c| c == ".git") {
            return Err("mantra.decisionLog must be a repository-relative file path".into());
        }
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

/// Headings outside fenced code, counted so deleting one of two identical entries is caught.
fn headings(text: &str) -> BTreeMap<String, usize> {
    let mut out = BTreeMap::new();
    let mut fence: Option<(char, usize)> = None;
    let mut previous = "";
    for line in text.lines() {
        if line.starts_with("    ") || line.starts_with('\t') {
            previous = "";
            continue;
        }
        let t = line.trim();
        let first = t.chars().next().unwrap_or(' ');
        let run = if first == '`' || first == '~' { t.chars().take_while(|c| *c == first).count() } else { 0 };
        if run >= 3 {
            let ch = t.chars().next().unwrap();
            match fence {
                Some((open, n)) if ch == open && run >= n => fence = None,
                None => fence = Some((ch, run)),
                _ => {},
            }
            previous = "";
            continue;
        }
        if fence.is_some() { continue; }
        let hashes = t.bytes().take_while(|b| *b == b'#').count();
        let heading = if (1..=6).contains(&hashes) && (t.len() == hashes || t.as_bytes()[hashes].is_ascii_whitespace()) {
            Some(t.trim_end_matches('#').trim().to_string())
        } else if !previous.is_empty() && t.len() >= 3 && (t.bytes().all(|b| b == b'=') || t.bytes().all(|b| b == b'-')) {
            Some(format!("{} {previous}", if t.starts_with('=') { "#" } else { "##" }))
        } else { None };
        if let Some(h) = heading { *out.entry(h).or_default() += 1; }
        previous = t;
    }
    out
}

/// Inspect the exact staged tree that would be committed, including deletions and renames.
fn protect_decisions(dir: &Path) -> Result<(), String> {
    for path in decision_logs(dir)? {
        let Ok(before) = git(dir, &["show", &format!("HEAD:{path}")]) else { continue };
        let after = git(dir, &["show", &format!(":{path}")]).unwrap_or_default();
        let after = headings(&after);
        for (heading, count) in headings(&before) {
            if after.get(&heading).copied().unwrap_or(0) < count {
                return Err(format!("decision-log safeguard: {path} lost heading {heading:?}; preserve past decisions and append a superseding entry"));
            }
        }
    }
    Ok(())
}

fn guarded_merge(dir: &Path, branch: &str) -> Result<(), String> {
    if let Err(e) = git_with_identity(dir, &["merge", "--no-ff", "--no-commit", "-q", branch]) {
        let conflict = git(dir, &["ls-files", "-u"]).map(|s| !s.is_empty()).unwrap_or(false);
        let _ = git(dir, &["merge", "--abort"]);
        return Err(if conflict { format!("merge conflict: {e}") } else { e });
    }
    let result = protect_decisions(dir).and_then(|()| {
        // An already-integrated branch does not create MERGE_HEAD and needs no commit.
        if git(dir, &["rev-parse", "-q", "--verify", "MERGE_HEAD"]).is_ok() { commit(dir, &["-q", "--no-edit"]) } else { Ok(()) }
    });
    if result.is_err() { let _ = git(dir, &["merge", "--abort"]); }
    result
}

pub fn setup(project: &Path, run_id: &str, isolation: &str) -> Result<Workspace, String> {
    let shared = |note: Option<String>| Workspace {
        worktree: false,
        repo: project.to_path_buf(),
        integ: project.to_path_buf(),
        branch: String::new(),
        base_branch: String::new(),
        git_common_dir: None,
        note,
        base_commit: None,
        initial_commit: false,
    };
    if isolation == "shared" {
        return Ok(shared(None));
    }
    let top = match git(project, &["rev-parse", "--show-toplevel"]) {
        Ok(t) => PathBuf::from(t),
        Err(_) => {
            if isolation == "worktree" {
                return Err("isolation = worktree needs a git repository".into());
            }
            return Ok(shared(Some("not a git repo — workers share the project directory".into())));
        }
    };
    // `git init` and nothing else: an unborn HEAD has nothing to branch a worktree from. One empty
    // commit gives it one — the shared fallback used to kick in here and quietly lose the
    // isolation the pattern asked for; it is kept only for a directory that is no repo at all.
    let initial_commit = git(&top, &["rev-parse", "HEAD"]).is_err();
    if initial_commit {
        initial_empty_commit(&top)?;
    }
    let base_branch = git(&top, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_else(|_| "HEAD".into());
    if top.join(".mantra").exists() {
        let _ = ensure_excluded(&top); // legacy per-project dir from older versions: keep it out of `git status`
    }
    let dirty = git(&top, &["status", "--porcelain"]).map(|s| !s.is_empty()).unwrap_or(false);
    let base_commit = git(&top, &["rev-parse", "HEAD"]).ok();
    let branch = format!("mantra/{run_id}");
    let integ = crate::config::worktrees_dir().join(run_id).join("integration");
    let _ = std::fs::create_dir_all(integ.parent().unwrap_or(&integ));
    git(&top, &["worktree", "add", "-b", &branch, &integ.to_string_lossy(), "HEAD"])?;
    let common = git(&top, &["rev-parse", "--git-common-dir"]).ok().map(|c| {
        let p = PathBuf::from(&c);
        if p.is_absolute() {
            p
        } else {
            top.join(p)
        }
    });
    let _ = ensure_excluded(&top);
    Ok(Workspace {
        worktree: true,
        repo: top,
        integ,
        branch,
        base_branch,
        git_common_dir: common,
        base_commit,
        note: match (dirty, initial_commit) {
            // the files are there but none was ever committed: the worktrees start out empty
            (true, true) => Some("nothing here was ever committed — agents start from an empty tree (commit your files first, or set isolation = shared)".into()),
            (true, false) => Some("you have uncommitted changes — agents work from your last commit (commit first, or set isolation = shared)".into()),
            _ => None,
        },
        initial_commit,
    })
}

/// `git commit --allow-empty` on an unborn HEAD. `--only` with no paths keeps it empty even when
/// files are already staged: those stay staged and are the user's to commit, not Mantra's. The
/// repo's own identity is used when it has one; a fresh `git init` on a machine with no
/// `user.name`/`user.email` gets Mantra's, so the commit does not fail on "please tell me who you are".
/// `--no-verify`: the repo's hooks are for the user's own commits — a linter or a commit-message
/// policy has nothing to check in an empty one, and must not stop the run before it starts.
fn initial_empty_commit(top: &Path) -> Result<(), String> {
    commit(top, &["-q", "--allow-empty", "--only", "--no-verify", "-m", "mantra: initial commit"])
        .map_err(|e| format!("the repository has no commit yet and Mantra could not create one for the worktrees ({e}) — commit once yourself, or set isolation = shared"))
}

fn ensure_excluded(top: &Path) -> std::io::Result<()> {
    let ex = top.join(".git").join("info").join("exclude");
    if let Ok(s) = std::fs::read_to_string(&ex) {
        if s.lines().any(|l| l.trim() == ".mantra/") {
            return Ok(());
        }
    }
    if ex.parent().map(|p| p.exists()).unwrap_or(false) {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(ex)?;
        writeln!(f, ".mantra/")?;
    }
    Ok(())
}

/// Create a worker worktree branching off the integration branch tip.
pub fn add_worker(ws: &Workspace, run_id: &str, task_id: &str, attempt: u32) -> Result<(PathBuf, String), String> {
    if !ws.worktree {
        return Ok((ws.integ.clone(), String::new()));
    }
    // Separate namespace: `mantra/<run>` already exists as a ref, so `mantra/<run>/x` can't.
    let branch = format!("mantra-w/{run_id}/{task_id}-{attempt}");
    let dir = crate::config::worktrees_dir().join(run_id).join(format!("{task_id}-{attempt}"));
    if dir.exists() {
        let _ = git(&ws.repo, &["worktree", "remove", "--force", &dir.to_string_lossy()]);
        let _ = std::fs::remove_dir_all(&dir);
    }
    let _ = git(&ws.repo, &["branch", "-D", &branch]);
    git(&ws.repo, &["worktree", "add", "-b", &branch, &dir.to_string_lossy(), &ws.branch])?;
    Ok((dir, branch))
}

/// Commit whatever the worker changed in its worktree.
pub fn commit_all(dir: &Path, message: &str) -> Result<bool, String> {
    git(dir, &["add", "-A"])?;
    let changed = git(dir, &["status", "--porcelain"]).map(|s| !s.is_empty()).unwrap_or(false);
    if !changed {
        return Ok(false);
    }
    protect_decisions(dir)?;
    commit(dir, &["-q", "-m", message])?;
    Ok(true)
}

pub struct MergeResult {
    pub merged: Vec<String>,
    pub conflicts: Vec<String>,
    /// Policy or commit failures must be escalated, never handed to an automatic merge fixer.
    pub blocked: Vec<String>,
    pub log: String,
}

pub fn merge_workers(ws: &Workspace, workers: &[(String, PathBuf, String)]) -> MergeResult {
    let mut r = MergeResult { merged: vec![], conflicts: vec![], blocked: vec![], log: String::new() };
    if !ws.worktree {
        return r;
    }
    for (task, dir, branch) in workers {
        if branch.is_empty() || dir == &ws.integ {
            // worker ran on the shared integration copy (fallback) — just commit there
            match commit_all(&ws.integ, &format!("mantra: {task}")) {
                Ok(_) => r.merged.push(task.clone()),
                Err(e) => {
                    r.log.push_str(&format!("{task}: commit failed: {e}\n"));
                    r.blocked.push(format!("{task}: commit failed: {e}"));
                }
            }
            continue;
        }
        match commit_all(dir, &format!("mantra: {task}")) {
            Ok(true) => r.log.push_str(&format!("committed {task}\n")),
            Ok(false) => r.log.push_str(&format!("{task}: no changes\n")),
            Err(e) => {
                r.log.push_str(&format!("{task}: commit failed: {e}\n"));
                r.blocked.push(format!("{task} ({branch}): commit failed: {e}"));
                continue;
            }
        }
        match guarded_merge(&ws.integ, branch) {
            Ok(_) => r.merged.push(task.clone()),
            Err(e) => {
                let _ = git(&ws.integ, &["merge", "--abort"]);
                r.log.push_str(&format!("{task}: merge refused — {e}\n"));
                if e.starts_with("merge conflict:") {
                    r.conflicts.push(branch.clone());
                } else {
                    r.blocked.push(format!("{task} ({branch}): {e}"));
                }
            }
        }
    }
    r
}

pub fn remove_worker(ws: &Workspace, dir: &Path, branch: &str, keep_branch: bool) {
    if !ws.worktree || dir == ws.integ {
        return;
    }
    let _ = git(&ws.repo, &["worktree", "remove", "--force", &dir.to_string_lossy()]);
    if !keep_branch && !branch.is_empty() {
        let _ = git(&ws.repo, &["branch", "-D", branch]);
    }
}

/// Re-create a run's integration worktree after its directory went missing (a cleaned `/tmp`, a
/// moved home) — possible as long as the run branch still exists in the repo.
pub fn reattach(ws: &Workspace) -> Result<(), String> {
    if !ws.worktree || ws.integ.is_dir() {
        return Ok(());
    }
    let _ = git(&ws.repo, &["worktree", "prune"]);
    let _ = std::fs::create_dir_all(ws.integ.parent().unwrap_or(&ws.integ));
    git(&ws.repo, &["worktree", "add", &ws.integ.to_string_lossy(), &ws.branch]).map(|_| ())
}

/// Remove everything a run left in the repo: its integration and worker worktrees (whatever
/// `git worktree list` still knows under the run's worktree folder) and the `mantra/<run>` +
/// `mantra-w/<run>/*` branches. Best effort; returns one line per step so `mantra runs delete`
/// can show exactly what happened.
pub fn cleanup_run(repo: &Path, run_id: &str, integ: Option<&Path>) -> Vec<String> {
    let mut log = vec![];
    let wt_root = crate::config::worktrees_dir().join(run_id);
    if let Ok(list) = git(repo, &["worktree", "list", "--porcelain"]) {
        for line in list.lines() {
            let Some(p) = line.strip_prefix("worktree ") else { continue };
            let p = PathBuf::from(p.trim());
            if p.starts_with(&wt_root) || integ.map(|i| i == p).unwrap_or(false) {
                match git(repo, &["worktree", "remove", "--force", &p.to_string_lossy()]) {
                    Ok(_) => log.push(format!("removed worktree {}", p.display())),
                    Err(e) => log.push(e),
                }
            }
        }
    }
    let _ = git(repo, &["worktree", "prune"]);
    let refs = git(repo, &["for-each-ref", "--format=%(refname:short)", &format!("refs/heads/mantra-w/{run_id}/"), &format!("refs/heads/mantra/{run_id}")]).unwrap_or_default();
    for b in refs.lines().map(str::trim).filter(|b| !b.is_empty()) {
        match git(repo, &["branch", "-D", b]) {
            Ok(_) => log.push(format!("deleted branch {b}")),
            Err(e) => log.push(e),
        }
    }
    log
}

pub fn phase_commit(ws: &Workspace, msg: &str) -> Result<bool, String> {
    if !ws.worktree {
        return Ok(false);
    }
    commit_all(&ws.integ, msg)
}

/// Merge the finished run branch into the user's current branch.
pub fn land(ws: &Workspace) -> Result<String, String> {
    if !ws.worktree {
        return Ok("shared mode — changes are already in your project".into());
    }
    let dirty = git(&ws.repo, &["status", "--porcelain"]).map(|s| !s.is_empty()).unwrap_or(true);
    if dirty {
        return Err("your working tree has uncommitted changes — commit or stash, then /land again".into());
    }
    guarded_merge(&ws.repo, &ws.branch)?;
    let _ = git(&ws.repo, &["worktree", "remove", "--force", &ws.integ.to_string_lossy()]);
    Ok(format!("merged {} into {}", ws.branch, ws.base_branch))
}

/// How far back a review diff reaches.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DiffSpan {
    /// Since the last phase commit (`mantra: phase …`) — what the current phase's gate reviews.
    Phase,
    /// Since the run started — what the finale reviews.
    Run,
}

/// What a reviewer reads instead of exploring the repo: the `--stat` and the (capped) diff.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ReviewDiff {
    pub stat: String,
    pub diff: String,
    /// The diff was cut at the cap; the rest is one `git diff` away.
    pub truncated: bool,
}

impl ReviewDiff {
    pub fn is_empty(&self) -> bool {
        self.stat.trim().is_empty() && self.diff.trim().is_empty()
    }

    /// Prompt-ready text, or a one-line note when git has nothing to show.
    pub fn render(&self) -> String {
        if self.is_empty() {
            return "(no changes in the working tree, or no git diff available)".into();
        }
        let more = if self.truncated { "\n[diff cut here — run `git diff` for the rest, limited to the files you need]" } else { "" };
        format!("{}\n\n{}{more}", self.stat.trim(), self.diff.trim())
    }
}

/// Lockfiles and the like add bulk and no signal to a review.
const DIFF_EXCLUDES: &[&str] = &[":(exclude)*.lock", ":(exclude)package-lock.json", ":(exclude)pnpm-lock.yaml", ":(exclude)*.min.js", ":(exclude)*.map"];

/// `git diff --stat` and the diff of the integration copy over `span`, capped at `max_bytes`.
/// Blocking — run it on a background thread. Worktree mode marks new files "intent to add" first
/// (Mantra commits everything with `add -A` anyway) so they show up as additions; in shared mode
/// the user's index is left alone and untracked files are only listed.
pub fn review_diff(ws: &Workspace, span: DiffSpan, max_bytes: usize) -> ReviewDiff {
    let dir = ws.integ.as_path();
    if git(dir, &["rev-parse", "--git-dir"]).is_err() {
        return ReviewDiff::default();
    }
    let base = if ws.worktree {
        let phase = (span == DiffSpan::Phase).then(|| git(dir, &["log", "-1", "--format=%H", "--grep=^mantra: phase ", "HEAD"]).ok().filter(|h| !h.is_empty())).flatten();
        phase.or_else(|| ws.base_commit.clone()).unwrap_or_else(|| "HEAD".into())
    } else {
        "HEAD".into()
    };
    if ws.worktree {
        let _ = git(dir, &["add", "-N", "--", "."]);
    }
    let mut args = vec!["diff", "--no-color", "--no-ext-diff", "--no-textconv", "-U3"];
    let mut stat_args = vec!["diff", "--no-color", "--no-ext-diff", "--no-textconv", "--stat=100"];
    for a in [&mut args, &mut stat_args] {
        a.push(&base);
        a.push("--");
        a.push(".");
        a.extend(DIFF_EXCLUDES.iter().copied());
    }
    let mut stat = git(dir, &stat_args).unwrap_or_default();
    let mut diff = git(dir, &args).unwrap_or_default();
    if !ws.worktree {
        let fresh = git(dir, &["ls-files", "--others", "--exclude-standard"]).unwrap_or_default();
        let list: Vec<&str> = fresh.lines().filter(|l| !l.trim().is_empty()).collect();
        if !list.is_empty() {
            let shown = list.iter().take(30).copied().collect::<Vec<_>>().join(", ");
            let more = if list.len() > 30 { format!(" … and {} more", list.len() - 30) } else { String::new() };
            stat.push_str(&format!("\nuntracked (new) files: {shown}{more}"));
        }
    }
    let truncated = diff.len() > max_bytes;
    if truncated {
        let mut cut = max_bytes;
        while cut > 0 && !diff.is_char_boundary(cut) {
            cut -= 1;
        }
        diff.truncate(cut);
    }
    ReviewDiff { stat, diff, truncated }
}

#[derive(Debug, Clone)]
pub struct CheckResult {
    pub cmd: String,
    pub ok: bool,
    pub code: Option<i32>,
    pub output: String,
    pub secs: u64,
}

/// `git status --porcelain -z` as path → XY code ("??" for untracked). Ignored files are absent,
/// exactly as `add -A` would leave them out.
pub type StatusSnapshot = BTreeMap<String, String>;

pub fn status_snapshot(dir: &Path) -> Option<StatusSnapshot> {
    let out = Command::new("git").arg("-C").arg(dir).args(["status", "--porcelain", "-z"]).stdin(Stdio::null()).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut snap = BTreeMap::new();
    // Entries are `XY path\0`; a rename or copy (R/C in either column) is followed by its source
    // path as an entry of its own, which is skipped here — by position, never by length, or a
    // short source name would leave the *next* real entry (a worker's file) out of the snapshot.
    let mut it = text.split('\0').take_while(|e| !e.is_empty());
    while let Some(e) = it.next() {
        if e.len() < 4 {
            continue;
        }
        let (xy, path) = e.split_at(2);
        snap.insert(path[1..].to_string(), xy.to_string());
        if xy.contains('R') || xy.contains('C') {
            it.next();
        }
    }
    Some(snap)
}

/// Drop what the gate checks left behind so the phase commit never picks it up: a test runner's
/// `__pycache__`, a build's scratch output, a formatter's edits. Only paths that were clean before
/// the checks ran are touched — anything a worker or the gate agent produced was already in the
/// `before` snapshot and stays. New untracked paths are removed (`git clean`, so ignored files
/// under them survive), modified or deleted tracked files are restored from HEAD. Returns the
/// paths dropped.
pub fn discard_check_artifacts(dir: &Path, before: &StatusSnapshot) -> Vec<String> {
    let Some(after) = status_snapshot(dir) else { return vec![] };
    let mut dropped = vec![];
    for (path, xy) in &after {
        if before.contains_key(path) {
            continue;
        }
        let ok = if xy == "??" {
            git(dir, &["clean", "-fdq", "--", path]).is_ok()
        } else {
            git(dir, &["checkout", "-q", "HEAD", "--", path]).is_ok()
        };
        if ok {
            dropped.push(path.clone());
        }
    }
    dropped
}

pub fn run_checks(dir: &Path, cmds: &[String], timeout: Duration) -> Vec<CheckResult> {
    let mut out = vec![];
    for cmd in cmds {
        let start = Instant::now();
        let child = Command::new("sh")
            .arg("-c")
            .arg(format!("{cmd} 2>&1"))
            .current_dir(dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        let mut child = match child {
            Ok(c) => c,
            Err(e) => {
                out.push(CheckResult { cmd: cmd.clone(), ok: false, code: None, output: e.to_string(), secs: 0 });
                continue;
            }
        };
        let stdout = child.stdout.take();
        let reader = std::thread::spawn(move || {
            let mut s = String::new();
            if let Some(mut o) = stdout {
                use std::io::Read;
                let _ = o.read_to_string(&mut s);
            }
            s
        });
        let code = loop {
            match child.try_wait() {
                Ok(Some(st)) => break st.code(),
                Ok(None) if start.elapsed() > timeout => {
                    let _ = child.kill();
                    break None;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(100)),
                Err(_) => break None,
            }
        };
        let mut output = reader.join().unwrap_or_default();
        crate::util::tail_bytes(&mut output, 6000);
        if code.is_none() {
            output.push_str("\n[mantra] check timed out");
        }
        out.push(CheckResult { cmd: cmd.clone(), ok: code == Some(0), code, output, secs: start.elapsed().as_secs() });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway repo with one committed file, a `.gitignore` and a non-empty `HEAD`.
    fn temp_repo(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mantra-git-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q"]).unwrap();
        std::fs::write(dir.join("tracked.txt"), "one\n").unwrap();
        std::fs::write(dir.join(".gitignore"), "*.log\n").unwrap();
        commit_all(&dir, "init").unwrap();
        dir
    }

    fn head_files(dir: &Path) -> Vec<String> {
        git(dir, &["ls-tree", "-r", "--name-only", "HEAD"]).unwrap().lines().map(str::to_string).collect()
    }

    fn worker_repo(dir: &Path, tag: &str) -> PathBuf {
        let worker = dir.with_file_name(format!("mantra-worker-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&worker);
        git(dir, &["worktree", "add", "-q", "-b", "test-worker", &worker.to_string_lossy(), "HEAD"]).unwrap();
        worker
    }

    #[test]
    fn project_trailers_and_identity_reach_worker_and_merge_commits() {
        let dir = temp_repo("trailers");
        git(&dir, &["config", "user.name", "Project Author"]).unwrap();
        git(&dir, &["config", "user.email", "project@example.test"]).unwrap();
        git(&dir, &["config", "--add", "mantra.commitTrailer", "Co-Authored-By: Project Helper <helper@example.test>"]).unwrap();
        git(&dir, &["config", "--add", "mantra.commitTrailer", "Claude-Session: configured-session"]).unwrap();
        let worker = worker_repo(&dir, "trailers");
        std::fs::write(worker.join("worker.txt"), "change\n").unwrap();
        let ws = test_ws(&dir, None);
        let result = merge_workers(&ws, &[("task".into(), worker.clone(), "test-worker".into())]);
        assert_eq!(result.merged, vec!["task"]);
        assert!(result.conflicts.is_empty(), "{}", result.log);
        for rev in ["test-worker", "HEAD"] {
            let message = git(&dir, &["log", "-1", "--format=%B", rev]).unwrap();
            assert!(message.contains("Co-Authored-By: Project Helper <helper@example.test>"), "{message}");
            assert!(message.contains("Claude-Session: configured-session"), "{message}");
            assert!(!message.contains("Claude Fable"), "{message}");
            assert_eq!(git(&dir, &["log", "-1", "--format=%an <%ae>", rev]).unwrap(), "Project Author <project@example.test>");
            assert_eq!(git(&dir, &["log", "-1", "--format=%cn <%ce>", rev]).unwrap(), "Project Author <project@example.test>");
        }
        let _ = std::fs::remove_dir_all(worker);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn worker_merge_and_land_work_without_an_effective_git_identity() {
        for landing in [false, true] {
            let tag = if landing { "fallback-land" } else { "fallback-merge" };
            let dir = temp_repo(tag);
            // Empty local entries override global configuration too, so a developer's configured
            // identity cannot mask the merge preparation's need for the Mantra fallback.
            git(&dir, &["config", "--local", "user.name", ""]).unwrap();
            git(&dir, &["config", "--local", "user.email", ""]).unwrap();
            let worker = worker_repo(&dir, tag);
            std::fs::write(worker.join("worker.txt"), "worker contribution\n").unwrap();
            let mut ws = test_ws(&dir, None);
            if landing {
                commit_all(&worker, "worker commit").unwrap();
                ws.integ = worker.clone();
                ws.branch = "test-worker".into();
                land(&ws).unwrap();
            } else {
                let result = merge_workers(&ws, &[("task".into(), worker.clone(), "test-worker".into())]);
                assert_eq!(result.merged, vec!["task"], "{}", result.log);
                assert!(result.conflicts.is_empty() && result.blocked.is_empty(), "{}", result.log);
            }
            assert_eq!(git(&dir, &["show", "HEAD:worker.txt"]).unwrap(), "worker contribution");
            assert_eq!(git(&dir, &["log", "-1", "--format=%an <%ae>|%cn <%ce>"]).unwrap(), "Mantra <mantra@localhost>|Mantra <mantra@localhost>");
            assert_eq!(git(&dir, &["config", "--local", "user.name"]).unwrap(), "", "fallback is per-command; project config is unchanged");
            let _ = std::fs::remove_dir_all(worker);
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn deleted_decision_headings_refuse_merge_and_land_without_advancing_head() {
        for landing in [false, true] {
            let tag = if landing { "decisions-land" } else { "decisions-merge" };
            let dir = temp_repo(tag);
            git(&dir, &["config", "--add", "mantra.decisionLog", "docs/history.md"]).unwrap();
            std::fs::create_dir_all(dir.join("docs")).unwrap();
            std::fs::write(dir.join("DECISIONS.md"), "# Decisions\n\n## #106 Past choice\nKeep this.\n").unwrap();
            std::fs::write(dir.join("docs/history.md"), "Earlier decision\n================\nKeep this too.\n").unwrap();
            commit_all(&dir, "history").unwrap();
            let before = git(&dir, &["rev-parse", "HEAD"]).unwrap();
            let worker = worker_repo(&dir, tag);
            let path = if landing { "docs/history.md" } else { "DECISIONS.md" };
            std::fs::write(worker.join(path), "# Replacement\n").unwrap();
            // Simulate a commit created externally: the merge still has to defend history.
            git(&worker, &["add", "-A"]).unwrap();
            commit(&worker, &["-q", "-m", "overwrite history"]).unwrap();
            let mut ws = test_ws(&dir, None);
            ws.branch = "test-worker".into();
            if landing {
                ws.integ = worker.clone();
                assert!(land(&ws).unwrap_err().contains("decision-log safeguard"));
            } else {
                let result = merge_workers(&ws, &[("task".into(), worker.clone(), "test-worker".into())]);
                assert!(result.merged.is_empty());
                assert!(result.conflicts.is_empty());
                assert_eq!(result.blocked.len(), 1);
                assert!(result.log.contains("decision-log safeguard"), "{}", result.log);
            }
            assert_eq!(git(&dir, &["rev-parse", "HEAD"]).unwrap(), before);
            assert!(git(&dir, &["status", "--porcelain"]).unwrap().is_empty());
            assert!(git(&dir, &["rev-parse", "-q", "--verify", "MERGE_HEAD"]).is_err());
            let _ = std::fs::remove_dir_all(worker);
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn failed_worker_commit_does_not_merge_an_older_branch_tip() {
        let dir = temp_repo("commit-refusal");
        std::fs::write(dir.join("DECISIONS.md"), "# Decisions\n## Historical decision\n").unwrap();
        commit_all(&dir, "history").unwrap();
        let before = git(&dir, &["rev-parse", "HEAD"]).unwrap();
        let worker = worker_repo(&dir, "commit-refusal");
        std::fs::write(worker.join("safe.txt"), "earlier commit\n").unwrap();
        commit_all(&worker, "earlier worker work").unwrap();
        std::fs::remove_file(worker.join("DECISIONS.md")).unwrap();
        let result = merge_workers(&test_ws(&dir, None), &[("task".into(), worker.clone(), "test-worker".into())]);
        assert!(result.merged.is_empty());
        assert!(result.log.contains("commit failed: decision-log safeguard"), "{}", result.log);
        assert_eq!(git(&dir, &["rev-parse", "HEAD"]).unwrap(), before);
        assert!(!dir.join("safe.txt").exists());
        let _ = std::fs::remove_dir_all(worker);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn appending_superseding_decisions_is_allowed_but_duplicate_heading_loss_is_not() {
        let dir = temp_repo("append-decisions");
        std::fs::write(dir.join("DECISIONS.md"), "# Decisions\n## Choice\n## Choice\n```md\n## Example\n```\n").unwrap();
        commit_all(&dir, "history").unwrap();
        std::fs::write(dir.join("DECISIONS.md"), "# Decisions\n## Choice\n## Choice\n## Superseding choice\n~~~md\n## Other example\n~~~\n").unwrap();
        assert!(commit_all(&dir, "append").unwrap());
        std::fs::write(dir.join("DECISIONS.md"), "# Decisions\n## Choice\n## Superseding choice\n").unwrap();
        assert!(commit_all(&dir, "remove duplicate").unwrap_err().contains("lost heading"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn ordinary_content_conflicts_remain_fixable_and_keep_both_branch_tips() {
        let dir = temp_repo("real-conflict");
        let worker = worker_repo(&dir, "real-conflict");
        std::fs::write(worker.join("tracked.txt"), "worker contribution\n").unwrap();
        std::fs::write(dir.join("tracked.txt"), "integration contribution\n").unwrap();
        commit_all(&dir, "other task").unwrap();
        let before = git(&dir, &["rev-parse", "HEAD"]).unwrap();
        let result = merge_workers(&test_ws(&dir, None), &[("task".into(), worker.clone(), "test-worker".into())]);
        assert_eq!(result.conflicts, vec!["test-worker"]);
        assert!(result.blocked.is_empty(), "{}", result.log);
        assert_eq!(git(&dir, &["rev-parse", "HEAD"]).unwrap(), before);
        assert_eq!(git(&dir, &["show", "test-worker:tracked.txt"]).unwrap(), "worker contribution");
        assert_eq!(git(&dir, &["show", "HEAD:tracked.txt"]).unwrap(), "integration contribution");
        assert!(git(&dir, &["status", "--porcelain"]).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(worker);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn check_leftovers_never_reach_the_phase_commit() {
        let dir = temp_repo("checks");
        // what a worker produced before the checks ran stays a candidate for the commit
        std::fs::write(dir.join("worker.txt"), "by a worker\n").unwrap();
        let before = status_snapshot(&dir).unwrap();
        let cmds = vec!["mkdir -p __pycache__ && echo x > __pycache__/t.pyc && echo two >> tracked.txt && echo l > run.log".to_string()];
        let res = run_checks(&dir, &cmds, Duration::from_secs(10));
        assert!(res[0].ok, "{}", res[0].output);
        let mut dropped = discard_check_artifacts(&dir, &before);
        dropped.sort();
        assert_eq!(dropped, vec!["__pycache__/", "tracked.txt"], "only the checks' leftovers, not the worker's file or the ignored log");
        assert!(!dir.join("__pycache__").exists());
        assert_eq!(std::fs::read_to_string(dir.join("tracked.txt")).unwrap(), "one\n", "the tracked file is back at HEAD");
        assert!(dir.join("worker.txt").exists(), "a worker's file is never deleted");
        assert!(dir.join("run.log").exists(), "ignored files are none of our business");
        assert!(commit_all(&dir, "mantra: phase 1").unwrap());
        let files = head_files(&dir);
        assert!(files.contains(&"worker.txt".to_string()), "{files:?}");
        assert!(!files.iter().any(|f| f.starts_with("__pycache__")), "{files:?}");
        assert_eq!(git(&dir, &["show", "HEAD:tracked.txt"]).unwrap(), "one");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A staged rename whose source name is short ("abc" → "def") used to be dropped by a length
    /// filter, so the skip meant for the source ate the next entry instead — and a worker's
    /// untracked file listed right after it went missing from the snapshot, then got "cleaned".
    #[test]
    fn a_short_rename_source_does_not_hide_the_next_entry() {
        let dir = temp_repo("rename");
        std::fs::write(dir.join("abc"), "x\n").unwrap();
        git(&dir, &["add", "abc"]).unwrap();
        commit_all(&dir, "abc").unwrap();
        git(&dir, &["mv", "abc", "def"]).unwrap();
        std::fs::write(dir.join("zzz.txt"), "by a worker\n").unwrap();
        let before = status_snapshot(&dir).unwrap();
        assert_eq!(before.get("def").map(String::as_str), Some("R "), "{before:?}");
        assert_eq!(before.get("zzz.txt").map(String::as_str), Some("??"), "{before:?}");
        assert!(!before.contains_key("abc"), "the rename source is not an entry: {before:?}");
        let res = run_checks(&dir, &["echo x > scratch.txt".to_string()], Duration::from_secs(10));
        assert!(res[0].ok);
        assert_eq!(discard_check_artifacts(&dir, &before), vec!["scratch.txt"]);
        assert!(dir.join("zzz.txt").exists(), "a worker's file is never deleted");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn test_ws(dir: &Path, base: Option<String>) -> Workspace {
        Workspace { worktree: true, repo: dir.to_path_buf(), integ: dir.to_path_buf(), branch: "main".into(), base_branch: "main".into(), git_common_dir: None, note: None, base_commit: base, initial_commit: false }
    }

    #[test]
    fn review_diff_covers_the_phase_since_the_last_phase_commit_and_the_run_since_its_base() {
        let dir = temp_repo("review");
        let base = git(&dir, &["rev-parse", "HEAD"]).unwrap();
        // phase 1: committed by Mantra
        std::fs::write(dir.join("one.txt"), "phase one\n").unwrap();
        assert!(commit_all(&dir, "mantra: phase 1 — first").unwrap());
        // phase 2: an edit, a brand-new file, and a lockfile that must stay out
        std::fs::write(dir.join("tracked.txt"), "one\ntwo\n").unwrap();
        std::fs::write(dir.join("new.rs"), "fn main() {}\n").unwrap();
        std::fs::write(dir.join("Cargo.lock"), "lock\n").unwrap();
        let ws = test_ws(&dir, Some(base));
        let phase = review_diff(&ws, DiffSpan::Phase, 100_000);
        assert!(phase.stat.contains("tracked.txt") && phase.stat.contains("new.rs"), "{}", phase.stat);
        assert!(!phase.stat.contains("one.txt"), "phase one is already committed: {}", phase.stat);
        assert!(!phase.stat.contains("Cargo.lock") && !phase.diff.contains("Cargo.lock"), "lockfiles stay out");
        assert!(phase.diff.contains("+two") && phase.diff.contains("+fn main() {}"), "{}", phase.diff);
        assert!(!phase.truncated);
        let run = review_diff(&ws, DiffSpan::Run, 100_000);
        assert!(run.stat.contains("one.txt") && run.stat.contains("new.rs"), "the run reaches back to its base: {}", run.stat);
        assert!(run.render().contains("one.txt"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn review_diff_is_capped_and_says_so() {
        let dir = temp_repo("review-cap");
        std::fs::write(dir.join("big.txt"), "é".repeat(5000)).unwrap();
        let ws = test_ws(&dir, None);
        let d = review_diff(&ws, DiffSpan::Phase, 1000);
        assert!(d.truncated);
        assert!(d.diff.len() <= 1000, "{}", d.diff.len());
        assert!(d.render().contains("diff cut here"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn review_diff_in_shared_mode_leaves_the_index_alone_and_lists_untracked_files() {
        let dir = temp_repo("review-shared");
        std::fs::write(dir.join("tracked.txt"), "one\nedited\n").unwrap();
        std::fs::write(dir.join("fresh.txt"), "new\n").unwrap();
        let mut ws = test_ws(&dir, None);
        ws.worktree = false;
        let d = review_diff(&ws, DiffSpan::Phase, 100_000);
        assert!(d.stat.contains("tracked.txt"), "{}", d.stat);
        assert!(d.stat.contains("untracked (new) files: fresh.txt"), "{}", d.stat);
        let status = git(&dir, &["status", "--porcelain"]).unwrap();
        assert!(status.lines().any(|l| l == "?? fresh.txt"), "no intent-to-add in the user's repo: {status}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn review_diff_of_a_non_repo_is_empty() {
        let dir = std::env::temp_dir().join(format!("mantra-git-norepo-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ws = test_ws(&dir, None);
        let d = review_diff(&ws, DiffSpan::Run, 1000);
        assert!(d.is_empty());
        assert!(d.render().contains("no git diff"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_clean_check_drops_nothing_and_a_worker_edit_survives() {
        let dir = temp_repo("clean");
        std::fs::write(dir.join("tracked.txt"), "edited by a worker\n").unwrap();
        let before = status_snapshot(&dir).unwrap();
        let res = run_checks(&dir, &["true".to_string()], Duration::from_secs(10));
        assert!(res[0].ok);
        assert!(discard_check_artifacts(&dir, &before).is_empty());
        assert_eq!(std::fs::read_to_string(dir.join("tracked.txt")).unwrap(), "edited by a worker\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `git init` and nothing else: the run still gets its worktrees, off an empty commit Mantra
    /// makes — the shared fallback is only for a directory that is not a repo at all.
    #[test]
    fn an_unborn_repo_gets_an_initial_commit_and_isolated_worktrees() {
        crate::web::snapshot::tests::with_mantra_home(|| {
            let dir = std::env::temp_dir().join(format!("mantra-git-unborn-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            git(&dir, &["init", "-q"]).unwrap();
            // no identity configured in this repo: the commit must still go through
            git(&dir, &["config", "--local", "--unset-all", "user.name"]).ok();
            git(&dir, &["config", "--local", "--unset-all", "user.email"]).ok();
            assert!(git(&dir, &["rev-parse", "HEAD"]).is_err(), "the fixture must have no commit");
            for iso in ["worktree", "auto"] {
                let ws = setup(&dir, &format!("r-{iso}"), iso).unwrap_or_else(|e| panic!("{iso}: {e}"));
                assert!(ws.worktree, "{iso}: isolated, not shared");
                assert!(ws.integ.is_dir() && ws.integ != dir, "{iso}: an integration worktree exists");
                assert_eq!(ws.initial_commit, iso == "worktree", "{iso}: only the first setup had to create the commit");
                assert!(ws.base_commit.is_some());
            }
            assert_eq!(git(&dir, &["log", "--format=%s", "-1"]).unwrap(), "mantra: initial commit");
            assert_eq!(git(&dir, &["rev-list", "--count", "HEAD"]).unwrap(), "1", "one commit, not one per run");
            // files staged before the first run stay staged: the commit Mantra makes is empty
            let staged = std::env::temp_dir().join(format!("mantra-git-staged-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&staged);
            std::fs::create_dir_all(&staged).unwrap();
            git(&staged, &["init", "-q"]).unwrap();
            std::fs::write(staged.join("app.py"), "print('hi')\n").unwrap();
            git(&staged, &["add", "app.py"]).unwrap();
            // a hook that rejects every commit is the user's policy, not the empty commit's
            let hook = staged.join(".git").join("hooks").join("pre-commit");
            std::fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            let ws = setup(&staged, "r-staged", "worktree").unwrap();
            assert!(ws.worktree && ws.initial_commit);
            assert_eq!(git(&staged, &["ls-tree", "-r", "--name-only", "HEAD"]).unwrap(), "", "an empty commit");
            assert_eq!(git(&staged, &["status", "--porcelain"]).unwrap(), "A  app.py", "still staged, still the user's");
            assert!(ws.note.as_deref().map(|n| n.contains("empty tree")).unwrap_or(false), "{:?}", ws.note);
            let _ = std::fs::remove_dir_all(&staged);
            // a plain directory (outside any repo) is still shared, and `worktree` still refuses it
            let plain = std::env::temp_dir().join(format!("mantra-git-plain-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&plain);
            std::fs::create_dir_all(&plain).unwrap();
            let ws = setup(&plain, "r-plain", "auto").unwrap();
            assert!(!ws.worktree && ws.note.as_deref().map(|n| n.contains("not a git repo")).unwrap_or(false), "{:?}", ws.note);
            assert!(setup(&plain, "r-plain2", "worktree").is_err());
            let _ = std::fs::remove_dir_all(&plain);
            let _ = std::fs::remove_dir_all(&dir);
        });
    }
}
