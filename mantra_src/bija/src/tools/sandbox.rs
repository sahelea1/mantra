//! The sandbox (SPEC §6.3): a path policy for the file tools and an optional bubblewrap wrapper for
//! commands. Two layers on purpose: the path policy is pure Rust and always on (it is what keeps a
//! confused model from writing outside the workspace through the file tools), while bwrap is a
//! best-effort extra for commands — Linux only, auto-detected by one probe at start, and never a
//! reason to refuse work: when it is missing the agent still runs, plainly, and says so once.
//! Mantra's worktree isolation is the real boundary between workers; this is belt and braces.

use super::Sandbox;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

/// How long the one-time `bwrap … true` probe may take before bwrap counts as unusable.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// The warning (and system-note) wording when commands cannot be wrapped.
pub const UNAVAILABLE_WARNING: &str = "bwrap unavailable — commands run unsandboxed";

/// Toolchain caches under `$HOME` that builds expect to write (the same set Codex opens).
const HOME_CACHES: [&str; 4] = [".cache", ".cargo", ".npm", ".rustup"];

/// Lexically normalizes a path: drops `.`, folds `..` into its parent, never touches the disk (so it
/// works for paths that do not exist yet). `..` above the root stays at the root.
pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() && !out.has_root() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

/// Resolves a tool's `path` argument against the working directory (`~` is deliberately not
/// expanded: the model gets exactly the path it wrote).
pub fn resolve(cwd: &Path, raw: &str) -> PathBuf {
    let p = Path::new(raw.trim());
    if p.is_absolute() {
        normalize(p)
    } else {
        normalize(&cwd.join(p))
    }
}

/// Symlinks followed while resolving one path before it counts as a loop (the kernel's own limit).
const MAX_LINKS: u32 = 40;

/// One step of a path walk.
enum Step {
    Root,
    Up,
    Name(OsString),
}

fn steps(path: &Path) -> Vec<Step> {
    path.components()
        .filter_map(|c| match c {
            Component::RootDir | Component::Prefix(_) => Some(Step::Root),
            Component::CurDir => None,
            Component::ParentDir => Some(Step::Up),
            Component::Normal(n) => Some(Step::Name(n.to_os_string())),
        })
        .collect()
}

/// The path as the filesystem will really see it, walked component by component the way the kernel
/// walks it: every symlink is followed — a dangling one too, since a write through it creates its
/// target — and `..` steps back from where the walk really is, never from the spelling. Components
/// that do not exist yet are appended unchanged. `None` when the path cannot be resolved: a symlink
/// loop, or a `..` after a missing component (the kernel would refuse it too).
fn physical(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    let mut todo = steps(path);
    todo.reverse();
    let mut links = 0u32;
    // A component was missing: from here on the rest is spelling, appended as it is.
    let mut missing = false;
    while let Some(step) = todo.pop() {
        match step {
            Step::Root => out = PathBuf::from("/"),
            Step::Up if missing => return None,
            Step::Up => {
                if !out.pop() && !out.has_root() {
                    out.push("..");
                }
            }
            Step::Name(name) => {
                let next = out.join(&name);
                if !missing {
                    match std::fs::symlink_metadata(&next) {
                        Ok(m) if m.file_type().is_symlink() => {
                            links += 1;
                            if links > MAX_LINKS {
                                return None;
                            }
                            // Relative targets continue from the link's directory, which `out` is.
                            let mut target = steps(&std::fs::read_link(&next).ok()?);
                            target.reverse();
                            todo.extend(target);
                            continue;
                        }
                        Ok(_) => {}
                        Err(_) => missing = true,
                    }
                }
                out = next;
            }
        }
    }
    Some(out)
}

/// `physical`, or the path as given when it cannot be resolved (for the roots and their display).
fn real_path(path: &Path) -> PathBuf {
    physical(path).unwrap_or_else(|| path.to_path_buf())
}

/// Where the file tools may write. Roots are kept both as given and canonicalized, so `/tmp` on a
/// system where it is a symlink (`/private/tmp`) matches either spelling.
#[derive(Debug, Clone)]
pub struct Policy {
    pub mode: Sandbox,
    roots: Vec<PathBuf>,
}

impl Policy {
    /// The policy for `mode`: in workspace-write the roots are cwd, every `--writable` root, `/tmp`
    /// and `$TMPDIR`; read-only and full need none (one refuses all writes, the other none).
    pub fn new(mode: Sandbox, cwd: &Path, writable: &[PathBuf]) -> Policy {
        let mut roots = vec![cwd.to_path_buf()];
        roots.extend(writable.iter().map(|w| if w.is_absolute() { normalize(w) } else { normalize(&cwd.join(w)) }));
        roots.push(PathBuf::from("/tmp"));
        if let Some(tmp) = std::env::var_os("TMPDIR").filter(|t| !t.is_empty()) {
            roots.push(normalize(Path::new(&tmp)));
        }
        Policy::with_roots(mode, roots)
    }

    /// A policy over exactly these roots (tests use it to leave `/tmp` out).
    pub fn with_roots(mode: Sandbox, roots: Vec<PathBuf>) -> Policy {
        let mut all: Vec<PathBuf> = Vec::new();
        for root in roots {
            let canon = real_path(&root);
            for p in [root, canon] {
                if !all.contains(&p) {
                    all.push(p);
                }
            }
        }
        Policy { mode, roots: all }
    }

    pub fn read_only(&self) -> bool {
        self.mode == Sandbox::ReadOnly
    }

    /// The writable roots as the model should read them (given spellings, duplicates dropped).
    pub fn roots_display(&self) -> String {
        let mut shown: Vec<String> = Vec::new();
        for r in &self.roots {
            let s = r.display().to_string();
            // A canonical twin of a root already listed is noise for the reader.
            if !shown.iter().any(|x| real_path(Path::new(x)) == *r) {
                shown.push(s);
            }
        }
        shown.join(", ")
    }

    /// The roots that exist on disk, deduplicated by their real path — what bwrap binds writable.
    fn existing_roots(&self) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = Vec::new();
        for r in &self.roots {
            if let Ok(canon) = r.canonicalize() {
                if !out.contains(&canon) {
                    out.push(canon);
                }
            }
        }
        out
    }

    /// May a tool write `path` (absolute, normalized)? `Ok` is where the bytes really land — the
    /// path with every symlink resolved, which is what the caller then writes; `Err` is the sentence
    /// the model gets. `follow_final` is false for delete, which removes a symlink itself, never its
    /// target.
    pub fn check_write(&self, path: &Path, follow_final: bool) -> Result<PathBuf, String> {
        if self.mode == Sandbox::ReadOnly {
            return Err("the sandbox is read-only: no file may be written".into());
        }
        let real = match (follow_final, path.parent(), path.file_name()) {
            (false, Some(parent), Some(name)) => physical(parent).map(|p| p.join(name)),
            _ => physical(path),
        };
        let Some(real) = real else {
            return Err(format!("{} cannot be resolved (a symlink loop, or a link through a missing directory)", path.display()));
        };
        // Judged by where the bytes would really land, never by the spelling.
        if self.mode == Sandbox::Full || self.roots.iter().any(|r| real.starts_with(r)) {
            Ok(real)
        } else {
            Err(format!("{} is outside the writable roots ({})", path.display(), self.roots_display()))
        }
    }

    /// Is `path` a writable root, or a directory that holds one? Deleting either would take the
    /// workspace with it, which is never what was meant. Judged like a delete: the parent resolved,
    /// the last component as named.
    pub fn holds_root(&self, path: &Path) -> bool {
        let real = match (path.parent(), path.file_name()) {
            (Some(parent), Some(name)) => real_path(parent).join(name),
            _ => real_path(path),
        };
        self.roots.iter().any(|r| r.starts_with(path) || r.starts_with(&real))
    }
}

/// Why commands run without bwrap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plain {
    /// `--sandbox full`: nothing to enforce.
    Full,
    /// `--no-bwrap`: the operator's choice; no warning.
    Disabled,
    /// Not Linux, not on PATH, or the probe failed.
    Unavailable,
}

/// How commands are run: wrapped in bwrap with these writable binds, or plainly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Isolation {
    Bwrap { program: PathBuf, binds: Vec<PathBuf>, network: bool },
    Plain(Plain),
}

impl Isolation {
    /// Decides once at start. The probe is the only cost and only runs when bwrap could be used.
    pub async fn detect(policy: &Policy, no_bwrap: bool, network: bool) -> Isolation {
        if policy.mode == Sandbox::Full {
            return Isolation::Plain(Plain::Full);
        }
        if no_bwrap {
            return Isolation::Plain(Plain::Disabled);
        }
        if !cfg!(target_os = "linux") {
            return Isolation::Plain(Plain::Unavailable);
        }
        match super::which("bwrap") {
            Some(program) => Isolation::with_program(program, policy, network).await,
            None => Isolation::Plain(Plain::Unavailable),
        }
    }

    /// Probes `program` and, when it works, builds the wrapper around it.
    pub async fn with_program(program: PathBuf, policy: &Policy, network: bool) -> Isolation {
        if probe(&program).await {
            Isolation::Bwrap { program, binds: writable_binds(policy), network }
        } else {
            Isolation::Plain(Plain::Unavailable)
        }
    }

    /// The command that runs `script` with `shell -c` in `cwd`, wrapped when bwrap is in use.
    pub fn command(&self, shell: &Path, script: &str, cwd: &Path) -> Command {
        match self {
            Isolation::Plain(_) => {
                let mut cmd = Command::new(shell);
                cmd.arg("-c").arg(script).current_dir(cwd);
                cmd
            }
            Isolation::Bwrap { program, binds, network } => {
                let mut cmd = Command::new(program);
                // A mount hides whatever an earlier one put beneath it. So: the read-only root, then
                // the writable roots over it, then a fresh /dev and /proc over those — a root that
                // holds them (`--cwd /`) must not bring back the host's devices and processes — and
                // last the writable places inside /dev (`/dev/shm`), which `--dev` would hide.
                cmd.args(["--ro-bind", "/", "/"]);
                let (dev, roots): (Vec<&PathBuf>, Vec<&PathBuf>) = binds.iter().partition(|b| b.starts_with("/dev"));
                for b in roots {
                    cmd.arg("--bind").arg(b).arg(b);
                }
                cmd.args(["--dev", "/dev", "--proc", "/proc"]);
                for b in dev {
                    cmd.arg("--bind").arg(b).arg(b);
                }
                // `--new-session` keeps the command off the terminal; with it the command is in a group
                // of its own that a `killpg` of bwrap's never reaches, so `--unshare-pid` makes its
                // pid namespace the unit instead: when bwrap dies, `--die-with-parent` kills the
                // namespace's init and the kernel takes every process in it down too.
                cmd.arg("--chdir").arg(cwd).args(["--unshare-pid", "--die-with-parent", "--new-session"]);
                if !network {
                    cmd.arg("--unshare-net");
                }
                cmd.arg("--").arg(shell).arg("-c").arg(script).current_dir(cwd);
                cmd
            }
        }
    }

    /// The once-at-start warning: only when a sandbox was asked for and bwrap could not provide it.
    pub fn warning(&self) -> Option<String> {
        match self {
            Isolation::Plain(Plain::Unavailable) => Some(UNAVAILABLE_WARNING.into()),
            _ => None,
        }
    }

    /// One line for the system prompt saying how commands run.
    pub fn note(&self) -> String {
        match self {
            Isolation::Bwrap { network: true, .. } => "Commands run inside a bubblewrap sandbox (network allowed).".into(),
            Isolation::Bwrap { network: false, .. } => "Commands run inside a bubblewrap sandbox (no network).".into(),
            Isolation::Plain(Plain::Full) => "Commands run unsandboxed (full access).".into(),
            Isolation::Plain(Plain::Disabled) => "Commands run unsandboxed (bwrap disabled); keep their writes inside the writable roots.".into(),
            Isolation::Plain(Plain::Unavailable) => format!("{UNAVAILABLE_WARNING}; keep their writes inside the writable roots."),
        }
    }
}

/// `bwrap --ro-bind / / --dev /dev --proc /proc --unshare-pid true` within 5 s, exit 0 — anything
/// else (missing user namespaces in a container, a seccomp profile, a hang) means "not usable here".
/// The pid namespace is part of the probe because the wrapper needs it (and a fresh `/proc` only
/// mounts for a user who owns one).
pub async fn probe(program: &Path) -> bool {
    let mut cmd = Command::new(program);
    cmd.args(["--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc", "--unshare-pid", "true"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let Ok(mut child) = cmd.spawn() else { return false };
    matches!(tokio::time::timeout(PROBE_TIMEOUT, child.wait()).await, Ok(Ok(status)) if status.success())
}

/// The paths bwrap binds writable: `/tmp` and `/dev/shm` always; in workspace-write also every
/// existing policy root and the toolchain caches under `$HOME`. Only existing paths (bwrap refuses
/// to bind a missing source), each once, ancestors before what they hold (a later bind hides an
/// earlier one beneath it).
fn writable_binds(policy: &Policy) -> Vec<PathBuf> {
    let mut candidates = vec![PathBuf::from("/tmp"), PathBuf::from("/dev/shm")];
    if policy.mode == Sandbox::WorkspaceWrite {
        candidates.extend(policy.existing_roots());
        if let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) {
            candidates.extend(HOME_CACHES.iter().map(|c| Path::new(&home).join(c)));
        }
    }
    let mut out: Vec<PathBuf> = Vec::new();
    for c in candidates {
        if let Ok(real) = c.canonicalize() {
            if !out.contains(&real) {
                out.push(real);
            }
        }
    }
    out.sort_by_key(|p| p.components().count());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("bija-sandbox-{tag}-{}", crate::types::uuid_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    #[test]
    fn normalize_folds_dots_without_touching_disk() {
        assert_eq!(normalize(Path::new("/a/./b/../c")), PathBuf::from("/a/c"));
        assert_eq!(normalize(Path::new("/../x")), PathBuf::from("/x"));
        assert_eq!(resolve(Path::new("/w"), "src/../lib.rs"), PathBuf::from("/w/lib.rs"));
        assert_eq!(resolve(Path::new("/w"), "/etc/x"), PathBuf::from("/etc/x"));
        assert_eq!(resolve(Path::new("/w"), "~/x"), PathBuf::from("/w/~/x"), "~ is not expanded");
    }

    #[test]
    fn workspace_write_allows_roots_and_refuses_the_rest() {
        let ws = scratch("ws");
        let extra = scratch("extra");
        let p = Policy::with_roots(Sandbox::WorkspaceWrite, vec![ws.clone(), extra.clone()]);
        assert!(p.check_write(&ws.join("new/deep/file.rs"), true).is_ok());
        assert!(p.check_write(&extra.join("x"), true).is_ok());
        let err = p.check_write(Path::new("/usr/bija-nope/x"), true).unwrap_err();
        assert!(err.contains("/usr/bija-nope/x") && err.contains(&ws.display().to_string()), "{err}");
        assert!(p.holds_root(&ws) && !p.holds_root(&ws.join("a")));
        // Where the bytes land is what the caller writes.
        assert_eq!(p.check_write(&ws.join("new/f.rs"), true).unwrap(), ws.join("new/f.rs"));
    }

    #[test]
    fn a_dangling_symlink_is_judged_by_the_target_it_would_create() {
        let ws = scratch("dangling");
        let outside = scratch("dangling-out");
        let p = Policy::with_roots(Sandbox::WorkspaceWrite, vec![ws.clone()]);
        // Absolute and relative dangling links out of the workspace, and a link whose missing
        // target dir is reached through another link: all refused, the target never created.
        std::os::unix::fs::symlink(outside.join("made.txt"), ws.join("abs")).unwrap();
        let up = format!("../{}/made.txt", outside.file_name().unwrap().to_string_lossy());
        std::os::unix::fs::symlink(&up, ws.join("rel")).unwrap();
        std::os::unix::fs::symlink(outside.join("missing-dir"), ws.join("dir")).unwrap();
        for name in ["abs", "rel", "dir/f.txt"] {
            let err = p.check_write(&ws.join(name), true).unwrap_err();
            assert!(err.contains("outside the writable roots"), "{name}: {err}");
        }
        // A chain ending in a dangling link inside the workspace is fine, and lands at the end.
        std::os::unix::fs::symlink(ws.join("later.txt"), ws.join("hop2")).unwrap();
        std::os::unix::fs::symlink("hop2", ws.join("hop1")).unwrap();
        assert_eq!(p.check_write(&ws.join("hop1"), true).unwrap(), ws.join("later.txt"));
        // A loop cannot be resolved, so it is refused rather than guessed at.
        std::os::unix::fs::symlink("loop-b", ws.join("loop-a")).unwrap();
        std::os::unix::fs::symlink("loop-a", ws.join("loop-b")).unwrap();
        assert!(p.check_write(&ws.join("loop-a"), true).unwrap_err().contains("cannot be resolved"));
        // `..` in a target steps back from where the link really is, not from its spelling.
        std::fs::create_dir_all(outside.join("deep")).unwrap();
        std::os::unix::fs::symlink(outside.join("deep"), ws.join("deep")).unwrap();
        std::os::unix::fs::symlink("deep/../x.txt", ws.join("via")).unwrap();
        assert!(p.check_write(&ws.join("via"), true).is_err(), "deep/.. is the outside dir, not the workspace");
        assert!(!outside.join("made.txt").exists());
    }

    #[test]
    fn an_ancestor_of_a_root_holds_it() {
        let parent = scratch("nest");
        let ws = parent.join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let p = Policy::with_roots(Sandbox::WorkspaceWrite, vec![ws.clone(), PathBuf::from("/tmp")]);
        assert!(p.holds_root(&parent) && p.holds_root(&ws) && p.holds_root(Path::new("/tmp")));
        assert!(!p.holds_root(&parent.join("sibling")) && !p.holds_root(&ws.join("src")));
    }

    #[test]
    fn a_symlink_out_of_the_workspace_is_judged_by_its_target() {
        let ws = scratch("link");
        let outside = scratch("outside");
        std::os::unix::fs::symlink(&outside, ws.join("escape")).unwrap();
        let p = Policy::with_roots(Sandbox::WorkspaceWrite, vec![ws.clone()]);
        assert!(p.check_write(&ws.join("escape/f.txt"), true).is_err());
        // Deleting the link itself stays inside the workspace.
        assert!(p.check_write(&ws.join("escape"), false).is_ok());
    }

    #[test]
    fn read_only_refuses_everything_and_full_nothing() {
        let ro = Policy::new(Sandbox::ReadOnly, Path::new("/w"), &[]);
        assert!(ro.check_write(Path::new("/w/a"), true).is_err());
        let full = Policy::new(Sandbox::Full, Path::new("/w"), &[]);
        assert!(full.check_write(Path::new("/etc/bija-x"), true).is_ok());
    }

    #[tokio::test]
    async fn a_failing_probe_means_plain_execution_with_the_warning() {
        let ws = scratch("probe");
        let policy = Policy::with_roots(Sandbox::WorkspaceWrite, vec![ws.clone()]);
        assert!(!probe(Path::new("/bin/false")).await);
        assert!(!probe(Path::new("/nonexistent/bwrap")).await);
        let iso = Isolation::with_program(PathBuf::from("/bin/false"), &policy, false).await;
        assert_eq!(iso, Isolation::Plain(Plain::Unavailable));
        assert_eq!(iso.warning().as_deref(), Some(UNAVAILABLE_WARNING));
        assert!(iso.note().contains("unsandboxed"));
        let out = iso.command(Path::new("/bin/sh"), "pwd", &ws).output().await.unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), ws.display().to_string());
        // Full and --no-bwrap are choices, not failures: no warning.
        assert_eq!(Isolation::detect(&Policy::with_roots(Sandbox::Full, vec![]), false, false).await.warning(), None);
        assert_eq!(Isolation::detect(&policy, true, false).await, Isolation::Plain(Plain::Disabled));
    }

    #[test]
    fn the_wrapper_mounts_the_read_only_root_then_the_writable_roots_then_dev_and_proc() {
        let iso = Isolation::Bwrap { program: "bwrap".into(), binds: vec!["/tmp".into(), "/dev/shm".into(), "/w".into()], network: false };
        let cmd = iso.command(Path::new("/bin/bash"), "make", Path::new("/w"));
        let args: Vec<String> = cmd.as_std().get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        let joined = args.join(" ");
        assert!(joined.starts_with("--ro-bind / / --bind /tmp /tmp --bind /w /w --dev /dev --proc /proc --bind /dev/shm /dev/shm --chdir /w"), "{joined}");
        assert!(joined.ends_with("--chdir /w --unshare-pid --die-with-parent --new-session --unshare-net -- /bin/bash -c make"), "{joined}");
        // A root that holds /dev and /proc still gets the sandbox's own: they are mounted after it.
        let everything = Isolation::Bwrap { program: "bwrap".into(), binds: vec!["/".into()], network: false };
        let joined = everything.command(Path::new("sh"), "x", Path::new("/")).as_std().get_args().map(|a| a.to_string_lossy().into_owned()).collect::<Vec<_>>().join(" ");
        assert!(joined.starts_with("--ro-bind / / --bind / / --dev /dev --proc /proc --chdir /"), "{joined}");
        let net = Isolation::Bwrap { program: "bwrap".into(), binds: vec![], network: true };
        let args: Vec<String> = net.command(Path::new("sh"), "x", Path::new("/")).as_std().get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert!(!args.iter().any(|a| a == "--unshare-net"));
    }

    #[test]
    fn read_only_binds_only_the_scratch_dirs() {
        let binds = writable_binds(&Policy::new(Sandbox::ReadOnly, Path::new("/"), &[]));
        let tmp = PathBuf::from("/tmp").canonicalize().unwrap();
        assert!(binds.contains(&tmp));
        assert!(binds.iter().all(|b| b == &tmp || b.starts_with("/dev")), "{binds:?}");
    }

    #[test]
    fn writable_binds_put_ancestors_first() {
        let ws = scratch("order");
        std::fs::create_dir_all(ws.join("a/b")).unwrap();
        let policy = Policy::with_roots(Sandbox::WorkspaceWrite, vec![ws.join("a/b"), ws.join("a")]);
        let binds = writable_binds(&policy);
        let at = |p: &Path| binds.iter().position(|b| b == p).unwrap_or_else(|| panic!("{} not bound: {binds:?}", p.display()));
        assert!(at(&ws.join("a")) < at(&ws.join("a/b")), "{binds:?}");
    }

    /// The wrapper with the real bwrap, where one is installed and works (skipped, saying why,
    /// where not): writes land only in the writable roots, and the command is pid 1's child in a
    /// namespace of its own.
    #[tokio::test]
    async fn the_real_bwrap_is_detected_and_confines_the_command() {
        if super::super::which("bwrap").is_none() {
            return eprintln!("skipped: bwrap is not installed");
        }
        let ws = scratch("real");
        let policy = Policy::with_roots(Sandbox::WorkspaceWrite, vec![ws.clone()]);
        let iso = Isolation::detect(&policy, false, false).await;
        let Isolation::Bwrap { binds, .. } = &iso else {
            return eprintln!("skipped: bwrap is installed but cannot create its namespaces here ({iso:?})");
        };
        assert!(binds.contains(&ws) && binds.contains(&PathBuf::from("/tmp").canonicalize().unwrap()), "{binds:?}");
        let script = "pwd; touch inside.txt && echo wrote-inside; touch /usr/bija-sandbox-test 2>&1; echo \"pid $$\"; cat /proc/net/dev | grep -c :";
        let out = iso.command(Path::new("/bin/sh"), script, &ws).output().await.unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.first().copied(), Some(ws.display().to_string().as_str()), "{text}");
        assert!(text.contains("wrote-inside") && ws.join("inside.txt").exists(), "{text}");
        assert!(text.contains("Read-only file system") && !Path::new("/usr/bija-sandbox-test").exists(), "{text}");
        assert!(lines.iter().any(|l| l.starts_with("pid ") && l[4..].parse::<u32>().is_ok_and(|p| p < 10)), "a pid namespace of its own: {text}");
        assert_eq!(lines.last().copied(), Some("1"), "no network but loopback: {text}");
    }
}
