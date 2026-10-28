//! Small parsing and presentation helpers shared by the run state machine.

use super::{CheckResult, HaltReason, Path, trunc};

/// FNV-1a of a string: enough to tell "the pattern changed since it was written" without keeping
/// a second copy of the TOML around (`Run::pattern_hash`).
pub(super) fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// A plan title as a run name: one line, single spaces, at most 60 characters.
pub fn tidy_title(title: &str) -> String {
    trunc(title.split_whitespace().collect::<Vec<_>>().join(" ").trim(), 60)
}

/// A run's name for people before it has a plan: the goal's first six words.
pub fn title_from_goal(brief: &str) -> String {
    let words: Vec<&str> = brief.split_whitespace().take(6).collect();
    let t = words.join(" ");
    let t = t.trim_matches(|c: char| matches!(c, '.' | ',' | ';' | ':' | '-' | '"' | '\'' | '`')).trim();
    if t.is_empty() {
        "new run".into()
    } else {
        trunc(t, 60)
    }
}

/// Files a QA report or a check's output names: tokens that exist under `root`, or that look like
/// the path of a file (a `/` and an extension) a worker is to create. In order of first mention,
/// at most 12 — the scope of the fix worker the findings go to. Never a path that leaves `root`.
pub(super) fn paths_in(text: &str, root: &Path) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    let is_sep = |c: char| c.is_whitespace() || matches!(c, '`' | '"' | '\'' | '(' | ')' | '[' | ']' | '<' | '>' | ',' | ';' | '{' | '}' | '=' | '|');
    for raw in text.split(is_sep) {
        // `src/x.rs:12:5`, `src/x.rs#L10`, a trailing sentence dot
        let t = raw.split([':', '#']).next().unwrap_or("").trim_matches(|c: char| matches!(c, '.' | '*' | '!' | '?' | '-'));
        let t = t.strip_prefix("./").unwrap_or(t);
        if t.len() < 3 || t.len() > 200 || t.starts_with('/') || t.starts_with('-') || t.contains("://") || t.split('/').any(|seg| seg == "..") {
            continue;
        }
        // a diff header names `a/src/x.rs`; the file is `src/x.rs`
        let candidates = [t.strip_prefix("a/").or_else(|| t.strip_prefix("b/")).unwrap_or(t).to_string(), t.to_string()];
        let mut pick: Option<String> = None;
        for c in candidates {
            let looks_like_a_file = c.contains('/') && c.rsplit('/').next().and_then(|f| f.rsplit_once('.')).map(|(stem, ext)| !stem.is_empty() && (1..=6).contains(&ext.len()) && ext.chars().all(|ch| ch.is_ascii_alphanumeric())).unwrap_or(false);
            let exists = (c.contains('.') || c.contains('/')) && root.join(&c).exists();
            if exists || looks_like_a_file {
                pick = Some(c);
                break;
            }
        }
        if let Some(p) = pick {
            if !out.contains(&p) {
                out.push(p);
                if out.len() == 12 {
                    break;
                }
            }
        }
    }
    out
}

/// One line per failing check, for halt messages.
pub(super) fn failing_cmds(checks: &[CheckResult]) -> String {
    checks.iter().filter(|c| !c.ok).map(|c| c.cmd.as_str()).collect::<Vec<_>>().join(" ; ")
}

/// Identity of a verify round's failure: which checks failed, how, and the end of their output —
/// equal across two rounds means nothing the gate agent did touched the failure.
pub(super) fn failing_sig(checks: &[CheckResult]) -> String {
    checks
        .iter()
        .filter(|c| !c.ok)
        .map(|c| {
            let mut o = c.output.trim().to_string();
            crate::util::tail_bytes(&mut o, 240);
            format!("{}|{:?}|{}", c.cmd, c.code, o)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The run brief is explicit user context, not a dump of project/settings files. Carry it to
/// every worker (including retries and ad-hoc fixes), with obvious credential lines omitted.
pub(super) fn worker_brief_context(brief: &str) -> String {
    const LIMIT: usize = 12_000;
    let mut safe = String::new();
    let mut private_block = false;
    for line in brief.lines() {
        let low = line.to_ascii_lowercase();
        if low.contains("-----begin ") && low.contains("private key") { private_block = true; }
        let key = low.split_once(['=', ':']).map(|(k, _)| k.trim().trim_start_matches("export ").replace(['-', ' '], "_"));
        let credential = key.as_ref().map(|k| k.contains("password") || k.contains("secret") || k.contains("api_key") || k.ends_with("token") || k.ends_with("authorization")).unwrap_or(false)
            || low.contains("bearer ")
            || low.split_once("://").map(|(_, rest)| rest.split_whitespace().next().unwrap_or_default().contains('@')).unwrap_or(false);
        if private_block || credential {
            safe.push_str("[credential omitted]\n");
        } else {
            safe.push_str(line);
            safe.push('\n');
        }
        if low.contains("-----end ") && low.contains("private key") { private_block = false; }
    }
    let clipped = safe.chars().count() > LIMIT;
    if clipped { safe = safe.chars().take(LIMIT).collect(); }
    format!("Run brief — apply project and per-ticket rules (including language/order of status updates) to this task; read applicable AGENTS.md and project instructions. Do not print credentials or settings files. If a needed rule is omitted or unclear, ask the orchestrator before the affected action.\n{}{}", safe, if clipped { "\n[brief clipped at 12000 characters; ask for missing ticket/project rules]" } else { "" })
}

/// True when `dir` holds nothing but `.git`, `.mantra` and dotfiles — a fresh/greenfield project
/// the planner does not need to explore before submitting a plan. An unreadable directory is
/// never treated as empty (the old "explore first" behaviour is the safe default).
pub(super) fn project_is_empty(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else { return false };
    !entries.filter_map(|e| e.ok()).any(|e| {
        let name = e.file_name();
        let name = name.to_string_lossy();
        name != ".git" && name != ".mantra" && !name.starts_with('.')
    })
}

pub(super) fn reason_label(r: HaltReason) -> String {
    match r {
        HaltReason::GateExhausted => "gate exhausted".into(),
        HaltReason::AttemptsExhausted => "attempts exhausted".into(),
        HaltReason::AgentTurnFailed => "an agent's turn failed".into(),
        other => format!("{other:?}").to_lowercase(),
    }
}

/// Prepend an optional watchdog/manual note to a resume prompt.
pub(super) fn join_note(note: Option<String>, base: String) -> String {
    match note {
        Some(n) => format!("{n}\n\n{base}"),
        None => base,
    }
}

pub(super) fn parse_gate_from_text(t: Option<&str>) -> Option<(bool, String)> {
    let t = t?;
    let low = t.to_lowercase();
    if low.contains("gate: pass") || low.contains("gate passed") {
        return Some((true, trunc(t, 200)));
    }
    if low.contains("gate: fail") {
        return Some((false, trunc(t, 200)));
    }
    None
}
