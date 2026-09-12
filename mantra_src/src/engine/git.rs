//! Workspace isolation: every parallel worker gets its own git worktree; phases merge at the gate.
//! All functions are blocking and meant to run on a background thread.

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

pub fn setup(project: &Path, run_id: &str, isolation: &str) -> Result<Workspace, String> {
    let shared = |note: Option<String>| Workspace {
        worktree: false,
        repo: project.to_path_buf(),
        integ: project.to_path_buf(),
        branch: String::new(),
        base_branch: String::new(),
        git_common_dir: None,
        note,
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
    if git(&top, &["rev-parse", "HEAD"]).is_err() {
        return Ok(shared(Some("repo has no commits yet — workers share the project directory".into())));
    }
    let base_branch = git(&top, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_else(|_| "HEAD".into());
    if top.join(".mantra").exists() {
        let _ = ensure_excluded(&top); // legacy per-project dir from older versions: keep it out of `git status`
    }
    let dirty = git(&top, &["status", "--porcelain"]).map(|s| !s.is_empty()).unwrap_or(false);
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
        note: if dirty { Some("you have uncommitted changes — agents work from your last commit (commit first, or set isolation = shared)".into()) } else { None },
    })
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
    git(dir, &["-c", "user.name=Mantra", "-c", "user.email=mantra@localhost", "commit", "-q", "-m", message])?;
    Ok(true)
}

pub struct MergeResult {
    pub merged: Vec<String>,
    pub conflicts: Vec<String>,
    pub log: String,
}

pub fn merge_workers(ws: &Workspace, workers: &[(String, PathBuf, String)]) -> MergeResult {
    let mut r = MergeResult { merged: vec![], conflicts: vec![], log: String::new() };
    if !ws.worktree {
        return r;
    }
    for (task, dir, branch) in workers {
        if branch.is_empty() || dir == &ws.integ {
            // worker ran on the shared integration copy (fallback) — just commit there
            match commit_all(&ws.integ, &format!("mantra: {task}")) {
                Ok(_) => r.merged.push(task.clone()),
                Err(e) => r.log.push_str(&format!("{task}: commit failed: {e}\n")),
            }
            continue;
        }
        match commit_all(dir, &format!("mantra: {task}")) {
            Ok(true) => r.log.push_str(&format!("committed {task}\n")),
            Ok(false) => r.log.push_str(&format!("{task}: no changes\n")),
            Err(e) => r.log.push_str(&format!("{task}: commit failed: {e}\n")),
        }
        match git(&ws.integ, &["-c", "user.name=Mantra", "-c", "user.email=mantra@localhost", "merge", "--no-ff", "--no-edit", "-q", branch]) {
            Ok(_) => r.merged.push(task.clone()),
            Err(e) => {
                let _ = git(&ws.integ, &["merge", "--abort"]);
                r.log.push_str(&format!("{task}: conflict — {}\n", crate::util::trunc(&e, 200)));
                r.conflicts.push(branch.clone());
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
    git(&ws.repo, &["-c", "user.name=Mantra", "-c", "user.email=mantra@localhost", "merge", "--no-ff", "--no-edit", &ws.branch])?;
    let _ = git(&ws.repo, &["worktree", "remove", "--force", &ws.integ.to_string_lossy()]);
    Ok(format!("merged {} into {}", ws.branch, ws.base_branch))
}

#[derive(Debug, Clone)]
pub struct CheckResult {
    pub cmd: String,
    pub ok: bool,
    pub code: Option<i32>,
    pub output: String,
    pub secs: u64,
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
