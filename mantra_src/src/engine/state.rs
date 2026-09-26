//! Persisted run state.
//!
//! `state.json` in the run directory is rewritten at every transition of the Conductor and read
//! back by `mantra runs` (list / resume / delete), by the `/runs` overlay and by the welcome
//! screens (unfinished-run notice). It is the *only* thing resume needs besides the files the run
//! already writes (`plan.json`, `pattern.toml`, `brief.md`, `journal.jsonl`).

use super::git::Workspace;
use super::run::{Pending, PhaseStep, Stage};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Bump when the on-disk shape changes incompatibly. v0.1 wrote an untyped `{:?}` stage and no
/// `format` field at all; such files list as `(v0.1 format)` and cannot be resumed.
pub const FORMAT: u32 = 2;

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct RunState {
    pub format: u32,
    pub id: String,
    /// Absolute project path; a run can be resumed from any directory.
    pub project: PathBuf,
    pub brief: String,
    pub pattern: String,
    pub plan_version: u32,
    pub stage: Stage,
    /// Message of the halt in effect when the state was written (`None` while running).
    pub halted: Option<String>,
    /// A transition that became due during that halt, performed once on resume (`Pending`).
    pub pending: Option<Pending>,
    pub started_unix: u64,
    /// When the run completed or stopped, so a finished run's duration stays what it was.
    pub finished_unix: Option<u64>,
    /// Active run time (halts excluded) up to `updated_unix`; the clock resumes from here.
    /// `None` in a file written before the clock existed (its wall-clock span stands in).
    pub active_secs: Option<u64>,
    pub updated_unix: u64,
    pub ws: Option<Workspace>,
    pub handoff: String,
    pub workers: Vec<WorkerState>,
    pub agents: Vec<AgentState>,
    /// Phases already completed (for the summary screen after a resume).
    pub history: Vec<PhaseHistory>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct PhaseHistory {
    pub name: String,
    /// task id, title, role glyph, ok
    pub workers: Vec<(String, String, String, bool)>,
    pub secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct WorkerState {
    pub task: String,
    pub title: String,
    pub role: String,
    /// `queued | preparing | running | retrying | done | failed | cancelled`.
    pub state: String,
    pub error: String,
    pub attempt: u32,
    pub branch: String,
    pub worktree: Option<PathBuf>,
    pub adhoc: bool,
    /// The orchestrator's (possibly sharpened) prompt for this attempt.
    pub prompt: String,
    pub effort: Option<String>,
    /// Final report, truncated — enough for the gate prompt after a resume.
    pub report: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct AgentState {
    /// `planner | orchestrator | gate | finale | worker:<task>#<attempt>`.
    pub slot: String,
    pub name: String,
    pub role: String,
    pub provider: String,
    pub model_alias: String,
    pub effort: String,
    /// Codex thread id or Claude session id — what a resume re-attaches to.
    pub thread_id: Option<String>,
    pub cwd: PathBuf,
}

impl RunState {
    pub fn load(dir: &Path) -> Result<RunState, String> {
        let p = dir.join("state.json");
        let s = std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?;
        let v: serde_json::Value = serde_json::from_str(&s).map_err(|e| format!("{}: {e}", p.display()))?;
        let format = v.get("format").and_then(|f| f.as_u64()).unwrap_or(0) as u32;
        if format == 0 {
            return Err("v0.1 format (no typed stage) — cannot be resumed; delete it or land its branch by hand".into());
        }
        if format > FORMAT {
            return Err(format!("written by a newer Mantra (format {format} > {FORMAT})"));
        }
        serde_json::from_value(v).map_err(|e| format!("{}: {e}", p.display()))
    }

    pub fn save(&self, dir: &Path) {
        let _ = std::fs::create_dir_all(dir);
        let tmp = dir.join("state.json.tmp");
        if std::fs::write(&tmp, serde_json::to_string_pretty(self).unwrap_or_default()).is_ok() {
            let _ = std::fs::rename(&tmp, dir.join("state.json"));
        }
    }

    pub fn finished(&self) -> bool {
        matches!(self.stage, Stage::Done | Stage::Failed(_))
    }

    /// Short, human stage text — the same vocabulary the stage header uses.
    pub fn stage_label(&self) -> String {
        stage_label(&self.stage)
    }
}

pub fn stage_label(stage: &Stage) -> String {
    match stage {
        Stage::Setup => "setup".into(),
        Stage::Planning => "planning".into(),
        Stage::Review => "plan review".into(),
        Stage::Phase { idx, step } => {
            let s = match step {
                PhaseStep::Orchestrating => "working".to_string(),
                PhaseStep::Merging => "merging".into(),
                PhaseStep::Checks { round } => format!("checks r{round}"),
                PhaseStep::Gate { round } => format!("gate r{round}"),
                PhaseStep::Handoff => "handoff".into(),
            };
            format!("phase {} · {s}", idx + 1)
        }
        Stage::Finale { idx } => format!("finale {}", idx + 1),
        Stage::Done => "done".into(),
        Stage::Failed(_) => "failed".into(),
    }
}

/// One row of `mantra runs` / the `/runs` overlay.
#[derive(Debug, Clone)]
pub struct RunSummary {
    pub id: String,
    pub dir: PathBuf,
    /// `<project>-<hash>` folder name, so two projects with one name stay apart.
    pub project_key: String,
    /// Parsed state, or the reason it couldn't be parsed (old format, corrupt file).
    pub state: Result<RunState, String>,
    pub updated_unix: u64,
}

impl RunSummary {
    pub fn brief(&self) -> String {
        match &self.state {
            Ok(s) => s.brief.lines().next().unwrap_or("").to_string(),
            Err(_) => std::fs::read_to_string(self.dir.join("brief.md")).ok().and_then(|b| b.lines().next().map(|l| l.to_string())).unwrap_or_default(),
        }
    }

    pub fn stage_label(&self) -> String {
        match &self.state {
            Ok(s) => {
                let mut l = s.stage_label();
                if s.halted.is_some() && !s.finished() {
                    l.push_str(" · halted");
                }
                l
            }
            Err(_) => "(v0.1 format)".into(),
        }
    }

    pub fn project(&self) -> Option<PathBuf> {
        self.state.as_ref().ok().map(|s| s.project.clone())
    }

    /// Unfinished = there is something to resume.
    pub fn unfinished(&self) -> bool {
        self.state.as_ref().map(|s| !s.finished()).unwrap_or(false)
    }
}

/// The `<project>-<hash>` folder a project's runs live in.
pub fn project_key(project: &Path) -> String {
    crate::config::runs_dir(project).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
}

/// Every run under `$MANTRA_HOME/runs/**`, newest first.
pub fn list_all() -> Vec<RunSummary> {
    list_in(&crate::config::home().join("runs"))
}

/// Every run under one `runs/` root (`<root>/<project-key>/<run-id>/`), newest first.
pub fn list_in(root: &Path) -> Vec<RunSummary> {
    let mut v = vec![];
    let Ok(projects) = std::fs::read_dir(root) else { return v };
    for p in projects.flatten() {
        if !p.path().is_dir() {
            continue;
        }
        let key = p.file_name().to_string_lossy().to_string();
        let Ok(runs) = std::fs::read_dir(p.path()) else { continue };
        for r in runs.flatten() {
            let dir = r.path();
            if !dir.is_dir() || !(dir.join("state.json").is_file() || dir.join("brief.md").is_file()) {
                continue;
            }
            let id = r.file_name().to_string_lossy().to_string();
            let state = RunState::load(&dir);
            let updated_unix = match &state {
                Ok(s) if s.updated_unix > 0 => s.updated_unix,
                _ => mtime_unix(&dir.join("state.json")).or_else(|| mtime_unix(&dir.join("journal.jsonl"))).unwrap_or(0),
            };
            v.push(RunSummary { id, dir, project_key: key.clone(), state, updated_unix });
        }
    }
    v.sort_by(|a, b| b.updated_unix.cmp(&a.updated_unix).then_with(|| b.id.cmp(&a.id)));
    v
}

/// Runs of one project (by its runs directory), newest first.
pub fn list_project(project: &Path) -> Vec<RunSummary> {
    let key = project_key(project);
    list_all().into_iter().filter(|r| r.project_key == key).collect()
}

pub fn unfinished(project: &Path) -> Vec<RunSummary> {
    list_project(project).into_iter().filter(|r| r.unfinished()).collect()
}

/// Find a run by id, or by a unique prefix of one.
pub fn find(id: &str) -> Result<RunSummary, String> {
    find_in(list_all(), id)
}

pub fn find_in(all: Vec<RunSummary>, id: &str) -> Result<RunSummary, String> {
    if let Some(r) = all.iter().find(|r| r.id == id) {
        return Ok(r.clone());
    }
    let m: Vec<&RunSummary> = all.iter().filter(|r| r.id.starts_with(id)).collect();
    match m.len() {
        0 => Err(format!("no run '{id}' (mantra runs lists them)")),
        1 => Ok(m[0].clone()),
        n => Err(format!("'{id}' matches {n} runs: {}", m.iter().map(|r| r.id.as_str()).collect::<Vec<_>>().join(", "))),
    }
}

/// Delete a run: its worktrees and branches in the project repo, then the run directory.
/// Returns what was done, one line per step; never fails half-way silently.
pub fn delete(r: &RunSummary) -> Vec<String> {
    let mut log = vec![];
    let ws = r.state.as_ref().ok().and_then(|s| s.ws.clone());
    let repo = ws.as_ref().map(|w| w.repo.clone()).or_else(|| r.project());
    if let Some(repo) = repo.filter(|p| p.is_dir()) {
        log.extend(super::git::cleanup_run(&repo, &r.id, ws.as_ref().filter(|w| w.worktree).map(|w| w.integ.as_path())));
    }
    let wt = crate::config::worktrees_dir().join(&r.id);
    if wt.exists() {
        match std::fs::remove_dir_all(&wt) {
            Ok(()) => log.push(format!("removed {}", wt.display())),
            Err(e) => log.push(format!("could not remove {}: {e}", wt.display())),
        }
    }
    match std::fs::remove_dir_all(&r.dir) {
        Ok(()) => log.push(format!("removed {}", r.dir.display())),
        Err(e) => log.push(format!("could not remove {}: {e}", r.dir.display())),
    }
    log
}

pub fn fmt_ago(unix: u64) -> String {
    if unix == 0 {
        return "?".into();
    }
    let now = crate::util::unix_secs();
    let d = now.saturating_sub(unix);
    if d < 60 {
        "just now".into()
    } else if d < 3600 {
        format!("{}m ago", d / 60)
    } else if d < 86_400 {
        format!("{}h ago", d / 3600)
    } else {
        format!("{}d ago", d / 86_400)
    }
}

fn mtime_unix(p: &Path) -> Option<u64> {
    std::fs::metadata(p).ok()?.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok().map(|d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> RunState {
        RunState {
            format: FORMAT,
            id: "123-add-login".into(),
            project: PathBuf::from("/tmp/proj"),
            brief: "add login".into(),
            pattern: "mantra-default".into(),
            plan_version: 2,
            stage: Stage::Phase { idx: 1, step: PhaseStep::Gate { round: 2 } },
            halted: Some("paused by you".into()),
            pending: Some(Pending::Handoff { idx: 1 }),
            started_unix: 10,
            finished_unix: None,
            active_secs: Some(7),
            updated_unix: 20,
            ws: Some(Workspace { worktree: true, repo: "/tmp/proj".into(), integ: "/tmp/wt/integration".into(), branch: "mantra/123-add-login".into(), base_branch: "main".into(), git_common_dir: Some("/tmp/proj/.git".into()), note: None }),
            handoff: "phase 1 done".into(),
            workers: vec![WorkerState { task: "p2-api".into(), title: "API".into(), role: "worker-big".into(), state: "done".into(), error: String::new(), attempt: 1, branch: "mantra-w/123-add-login/p2-api-1".into(), worktree: Some("/tmp/wt/p2-api-1".into()), adhoc: false, prompt: "build it".into(), effort: Some("high".into()), report: "done".into() }],
            agents: vec![AgentState { slot: "orchestrator".into(), name: "orchestrator".into(), role: "orchestrator".into(), provider: "openai".into(), model_alias: "astra".into(), effort: "high".into(), thread_id: Some("t-1".into()), cwd: "/tmp/wt/integration".into() }],
            history: vec![PhaseHistory { name: "Foundations".into(), workers: vec![("p1-a".into(), "A".into(), "◇".into(), true)], secs: 42 }],
        }
    }

    #[test]
    fn state_round_trips_through_json() {
        let dir = std::env::temp_dir().join(format!("mantra-state-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let st = sample();
        st.save(&dir);
        let back = RunState::load(&dir).unwrap();
        assert_eq!(back, st);
        assert_eq!(back.stage_label(), "phase 2 · gate r2");
        assert!(!dir.join("state.json.tmp").exists(), "atomic rename leaves no temp file");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn old_format_is_reported_not_parsed() {
        let dir = std::env::temp_dir().join(format!("mantra-state-old-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("state.json"), r#"{"id":"1-x","brief":"x","stage":"Phase { idx: 0, step: Orchestrating }","workers":[]}"#).unwrap();
        let e = RunState::load(&dir).unwrap_err();
        assert!(e.contains("v0.1"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stage_labels_are_short_and_stable() {
        assert_eq!(stage_label(&Stage::Planning), "planning");
        assert_eq!(stage_label(&Stage::Phase { idx: 0, step: PhaseStep::Orchestrating }), "phase 1 · working");
        assert_eq!(stage_label(&Stage::Finale { idx: 2 }), "finale 3");
        assert_eq!(stage_label(&Stage::Failed("boom".into())), "failed");
    }

    #[test]
    fn listing_is_newest_first_and_prefix_lookup_must_be_unique() {
        let root = std::env::temp_dir().join(format!("mantra-runs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let proj = root.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        let rd = root.join(project_key(&proj));
        let mut a = sample();
        a.id = "100-alpha".into();
        a.project = proj.clone();
        a.save(&rd.join("100-alpha"));
        let mut b = sample();
        b.id = "200-beta".into();
        b.project = proj.clone();
        b.stage = Stage::Done;
        b.updated_unix = 30;
        b.save(&rd.join("200-beta"));
        // a v0.1 leftover: no state.json at all, just a brief
        std::fs::create_dir_all(rd.join("300-old")).unwrap();
        std::fs::write(rd.join("300-old").join("brief.md"), "old run\n").unwrap();
        let all = list_in(&root);
        assert_eq!(all.iter().filter(|r| r.state.is_ok()).map(|r| r.id.as_str()).collect::<Vec<_>>(), vec!["200-beta", "100-alpha"], "newest first");
        let old = all.iter().find(|r| r.id == "300-old").expect("old runs are still listed");
        assert!(old.state.is_err());
        assert_eq!(old.brief(), "old run");
        assert!(!old.unfinished(), "an unparseable run is never offered for resume");
        assert_eq!(all.iter().filter(|r| r.unfinished()).count(), 1);
        assert_eq!(find_in(all.clone(), "100").unwrap().id, "100-alpha");
        assert!(find_in(all.clone(), "zzz").is_err());
        let _ = std::fs::remove_dir_all(&root);
    }
}
