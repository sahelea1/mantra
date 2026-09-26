//! Small shared helpers: file logger, glob matching, formatting.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

static LOG: OnceLock<Mutex<File>> = OnceLock::new();

pub fn init_log(path: &Path) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(f) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = LOG.set(Mutex::new(f));
    }
}

pub fn log_line(s: &str) {
    if let Some(m) = LOG.get() {
        if let Ok(mut f) = m.lock() {
            let _ = writeln!(f, "{} {}", unix_secs(), s);
        }
    }
}

#[macro_export]
macro_rules! mlog {
    ($($t:tt)*) => { $crate::util::log_line(&format!($($t)*)) };
}

pub fn unix_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Local-ish clock string HH:MM:SS (UTC offset not applied; good enough for relative event feeds).
pub fn clock() -> String {
    let s = unix_secs() + tz_offset_secs();
    format!("{:02}:{:02}:{:02}", (s / 3600) % 24, (s / 60) % 60, s % 60)
}

fn tz_offset_secs() -> u64 {
    // Best effort: honour MANTRA_TZ_OFFSET (hours), else UTC.
    std::env::var("MANTRA_TZ_OFFSET")
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
        .map(|h| (h.rem_euclid(24) * 3600) as u64)
        .unwrap_or(0)
}

pub fn fmt_dur(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{}s", s)
    } else if s < 3600 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else {
        format!("{}h{:02}m", s / 3600, (s / 60) % 60)
    }
}

pub fn fmt_tokens(n: u64) -> String {
    if n < 1000 {
        format!("{}", n)
    } else if n < 1_000_000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        format!("{:.2}M", n as f64 / 1_000_000.0)
    }
}

/// Truncate to a display width, adding an ellipsis.
pub fn trunc(s: &str, width: usize) -> String {
    use unicode_width::UnicodeWidthChar;
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut w = 0;
    let total: usize = s.chars().map(|c| c.width().unwrap_or(0)).sum();
    if total <= width {
        return s.to_string();
    }
    for c in s.chars() {
        let cw = c.width().unwrap_or(0);
        if w + cw + 1 > width {
            break;
        }
        out.push(c);
        w += cw;
    }
    out.push('…');
    out
}

pub fn width(s: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(s)
}

/// Keep only the last `max` bytes of a string (on a char boundary).
pub fn tail_bytes(s: &mut String, max: usize) {
    if s.len() > max {
        let mut cut = s.len() - max;
        while !s.is_char_boundary(cut) {
            cut += 1;
        }
        s.drain(..cut);
    }
}

/// Glob match supporting `*` (within a segment), `**` (any depth) and `?`.
pub fn glob_match(pattern: &str, path: &str) -> bool {
    let pattern = pattern.trim_start_matches("./");
    let path = path.trim_start_matches("./");
    let p: Vec<&str> = pattern.split('/').filter(|s| !s.is_empty()).collect();
    let s: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    glob_segs(&p, &s)
}

fn glob_segs(p: &[&str], s: &[&str]) -> bool {
    if p.is_empty() {
        return s.is_empty();
    }
    if p[0] == "**" {
        // match zero or more segments
        for i in 0..=s.len() {
            if glob_segs(&p[1..], &s[i..]) {
                return true;
            }
        }
        return false;
    }
    if s.is_empty() {
        return false;
    }
    seg_match(p[0].as_bytes(), s[0].as_bytes()) && glob_segs(&p[1..], &s[1..])
}

fn seg_match(p: &[u8], s: &[u8]) -> bool {
    if p.is_empty() {
        return s.is_empty();
    }
    match p[0] {
        b'*' => (0..=s.len()).any(|i| seg_match(&p[1..], &s[i..])),
        b'?' => !s.is_empty() && seg_match(&p[1..], &s[1..]),
        c => !s.is_empty() && s[0] == c && seg_match(&p[1..], &s[1..]),
    }
}

/// A path is in scope if any glob matches, or if the glob is a directory prefix.
pub fn in_scope(scope: &[String], rel_path: &str) -> bool {
    if scope.is_empty() {
        return true;
    }
    scope.iter().any(|g| {
        let g = g.trim();
        if g.is_empty() {
            return false;
        }
        if glob_match(g, rel_path) {
            return true;
        }
        let dir = g.trim_end_matches('/');
        !dir.contains('*') && (rel_path == dir || rel_path.starts_with(&format!("{}/", dir)))
    })
}

/// Count added/removed lines in a unified diff.
pub fn diff_stats(diff: &str) -> (usize, usize) {
    let mut a = 0;
    let mut d = 0;
    for l in diff.lines() {
        if l.starts_with("+++") || l.starts_with("---") {
            continue;
        }
        if l.starts_with('+') {
            a += 1;
        } else if l.starts_with('-') {
            d += 1;
        }
    }
    (a, d)
}

/// Strip ANSI escape sequences: CSI (`ESC [ … final-byte`), OSC (`ESC ] … BEL|ST`), and lone `ESC`.
/// Codex/subprocess stderr is sometimes colourized; this keeps journal lines and crash reasons plain.
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some('[') => {
                chars.next(); // consume '['
                // parameter bytes 0x30-0x3F, then intermediate bytes 0x20-0x2F
                while let Some(&nc) = chars.peek() {
                    if ('0'..='?').contains(&nc) || (' '..='/').contains(&nc) {
                        chars.next();
                    } else {
                        break;
                    }
                }
                chars.next(); // consume the final byte, if any
            }
            Some(']') => {
                chars.next(); // consume ']'
                loop {
                    match chars.next() {
                        Some('\u{7}') | None => break,
                        Some('\u{1b}') => {
                            if chars.peek() == Some(&'\\') {
                                chars.next();
                            }
                            break;
                        }
                        Some(_) => continue,
                    }
                }
            }
            _ => {} // lone ESC: drop it, nothing else consumed
        }
    }
    out
}

/// A random (not cryptographically secure) UUID v4 string, for `claude --session-id` (WP10.3).
/// No RNG crate is in the dependency list, so this mixes process/time entropy through a small
/// xorshift — good enough for a session identifier the CLI never validates for randomness.
pub fn uuid_v4() -> String {
    let seed = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(1) ^ (std::process::id() as u64).wrapping_mul(0x9E3779B97F4A7C15);
    let mut x = seed | 1; // xorshift64 needs a non-zero state
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let (a, b) = (next(), next());
    let bytes: [u8; 16] = [(a >> 56) as u8, (a >> 48) as u8, (a >> 40) as u8, (a >> 32) as u8, (a >> 24) as u8, (a >> 16) as u8, (a >> 8) as u8, a as u8, (b >> 56) as u8, (b >> 48) as u8, (b >> 40) as u8, (b >> 32) as u8, (b >> 24) as u8, (b >> 16) as u8, (b >> 8) as u8, b as u8];
    let mut b = bytes;
    b[6] = (b[6] & 0x0F) | 0x40; // version 4
    b[8] = (b[8] & 0x3F) | 0x80; // variant 10xx
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    )
}

/// True when the process's effective uid is 0 (root). Claude Code refuses
/// `--dangerously-skip-permissions` for root unless `IS_SANDBOX=1` is set (WP10.3); Linux only
/// (via `/proc/self/status`, no `libc` dependency) — always false elsewhere, which just means
/// Mantra won't set `IS_SANDBOX` there, matching Codex's own root handling.
pub fn effective_uid_is_root() -> bool {
    if cfg!(not(target_os = "linux")) {
        return false;
    }
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| s.lines().find_map(|l| l.strip_prefix("Uid:").map(|rest| rest.split_whitespace().nth(1).unwrap_or("").to_string())))
        .map(|euid| euid == "0")
        .unwrap_or(false)
}

pub fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
        if out.len() > 40 {
            break;
        }
    }
    out.trim_matches('-').to_string()
}

/// Where a bare command name resolves on `$PATH`, and whether that file can actually be
/// executed — so a failed spawn can say *which* file is wrong instead of just "os error 13".
pub fn which(cmd: &str) -> Option<PathBuf> {
    which_in(cmd, &std::env::var("PATH").unwrap_or_default())
}

/// The PATH list is a parameter so tests can synthesize one instead of mutating the process
/// environment, which is shared by every other test in the binary.
fn which_in(cmd: &str, path_var: &str) -> Option<PathBuf> {
    if cmd.is_empty() {
        return None;
    }
    // A name carrying a separator is already a path: execvp(3) doesn't search PATH for it either.
    if cmd.contains(['/', std::path::MAIN_SEPARATOR]) {
        let p = PathBuf::from(cmd);
        return p.symlink_metadata().is_ok().then_some(p);
    }
    for dir in std::env::split_paths(path_var) {
        // An empty PATH entry means the current directory, as the shell reads it.
        let cand = if dir.as_os_str().is_empty() { PathBuf::from(".") } else { dir }.join(cmd);
        // Deliberately *not* filtered down to "regular file with +x": a directory or an
        // un-executable file shadowing the real binary is exactly the case doctor must report,
        // and skipping it here would leave the user with a bare errno again.
        if cand.symlink_metadata().is_ok() {
            return Some(cand);
        }
    }
    None
}

/// This binary's own path, for re-spawning itself (`mantra mock-codex`, `mantra mcp-bridge`).
/// `current_exe()` reads `/proc/self/exe`, which some containers don't mount — then argv[0] is
/// the next best thing: a path is taken as invoked (a relative one against the cwd), a bare
/// name is looked up on `$PATH` exactly as the shell that launched us did.
pub fn self_exe() -> std::io::Result<PathBuf> {
    match std::env::current_exe() {
        Ok(p) => Ok(p),
        Err(e) => {
            let arg0 = std::env::args_os().next().unwrap_or_default().to_string_lossy().into_owned();
            let cwd = std::env::current_dir().unwrap_or_default();
            exe_from_argv0(&arg0, &std::env::var("PATH").unwrap_or_default(), &cwd)
                .ok_or_else(|| std::io::Error::new(e.kind(), format!("{e}; and argv[0] {arg0:?} does not name a file either")))
        }
    }
}

/// argv[0] → an absolute path to an existing file, or None. PATH and cwd are parameters for the
/// same reason as `which_in`'s.
fn exe_from_argv0(arg0: &str, path_var: &str, cwd: &Path) -> Option<PathBuf> {
    let p = if arg0.contains(['/', std::path::MAIN_SEPARATOR]) { PathBuf::from(arg0) } else { which_in(arg0, path_var)? };
    // (`join` keeps an already-absolute path as it is.)
    let p = cwd.join(p);
    p.is_file().then_some(p)
}

/// A one-line, actionable reason a resolved command cannot be executed, or None when it can.
pub fn exec_problem(path: &Path) -> Option<String> {
    let p = path.display();
    let md = match std::fs::metadata(path) {
        Ok(md) => md,
        // metadata() follows links, so the only way to fail after `which` saw the entry is a
        // symlink whose target is gone — which execs as a plain "not found" and misleads.
        Err(_) => return Some(format!("{p} is a broken symlink — reinstall the CLI or repoint the link")),
    };
    if md.is_dir() {
        return Some(format!("{p} is a directory — something else on your PATH shadows the real binary"));
    }
    exec_bit_problem(&md, path)
}

#[cfg(unix)]
fn exec_bit_problem(md: &std::fs::Metadata, path: &Path) -> Option<String> {
    use std::os::unix::fs::PermissionsExt;
    // Which of user/group/other applies depends on who runs it, so only "no exec bit at all" is
    // reported here; anything subtler (noexec mount, unreadable parent) stays with the raw errno.
    (md.permissions().mode() & 0o111 == 0).then(|| format!("{p} is not executable — chmod +x {p}", p = path.display()))
}

#[cfg(not(unix))]
fn exec_bit_problem(_md: &std::fs::Metadata, _path: &Path) -> Option<String> {
    None // no exec bit to inspect; executability is decided by the extension there
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn globs() {
        assert!(glob_match("src/**", "src/a/b.rs"));
        assert!(glob_match("src/**/*.rs", "src/a/b.rs"));
        assert!(glob_match("src/*.rs", "src/b.rs"));
        assert!(!glob_match("src/*.rs", "src/a/b.rs"));
        assert!(glob_match("**/test_*.py", "a/b/test_x.py"));
        assert!(in_scope(&["src/auth".into()], "src/auth/mod.rs"));
        assert!(!in_scope(&["src/auth/**".into()], "src/orders/mod.rs"));
        assert!(in_scope(&[], "anything"));
    }
    #[test]
    fn stats() {
        assert_eq!(diff_stats("--- a\n+++ b\n+x\n-y\n+z\n"), (2, 1));
    }
    #[test]
    fn argv0_fallback_for_the_self_exe() {
        let dir = std::env::temp_dir().join(format!("mantra-argv0-{}", std::process::id()));
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("mantra"), "").unwrap();
        std::fs::write(dir.join("mantra-here"), "").unwrap();
        let path_var = std::env::join_paths([dir.join("nowhere"), bin.clone()]).unwrap();
        let path_var = path_var.to_string_lossy().into_owned();
        // bare name → looked up on PATH
        assert_eq!(exe_from_argv0("mantra", &path_var, &dir), Some(bin.join("mantra")));
        // a relative path is resolved against the cwd, never searched on PATH
        assert_eq!(exe_from_argv0("./mantra-here", &path_var, &dir), Some(dir.join("./mantra-here")));
        assert_eq!(exe_from_argv0("./mantra", &path_var, &dir), None);
        // an absolute path is kept as it is
        assert_eq!(exe_from_argv0(&bin.join("mantra").to_string_lossy(), "", &dir), Some(bin.join("mantra")));
        // nothing usable: unknown name, a directory, an empty argv[0]
        assert_eq!(exe_from_argv0("no-such-binary", &path_var, &dir), None);
        assert_eq!(exe_from_argv0(&bin.to_string_lossy(), &path_var, &dir), None);
        assert_eq!(exe_from_argv0("", &path_var, &dir), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn truncation() {
        assert_eq!(trunc("hello world", 6), "hello…");
        assert_eq!(trunc("hi", 6), "hi");
    }
    #[test]
    fn strip_ansi_matches_the_f1_capture() {
        // Exact stderr tail from the F1 finding (a dim-styled timestamp, then a truncated colour
        // sequence cut off mid-escape by the old byte-limited tail).
        let input = "planner: process crashed (codex exited: \u{1b}[2m2026-09-11T22:51:26.627070Z\u{1b}[0m \u{1b}[…) — restarting & resuming";
        let out = strip_ansi(input);
        assert!(!out.contains('\u{1b}'), "no ESC byte must survive: {out:?}");
        assert_eq!(out, "planner: process crashed (codex exited: 2026-09-11T22:51:26.627070Z ) — restarting & resuming");
    }
    #[test]
    fn uuid_v4_has_the_right_shape_and_is_not_constant() {
        let a = uuid_v4();
        let b = uuid_v4();
        assert_ne!(a, b);
        for u in [&a, &b] {
            let parts: Vec<&str> = u.split('-').collect();
            assert_eq!(parts.iter().map(|p| p.len()).collect::<Vec<_>>(), vec![8, 4, 4, 4, 12]);
            assert!(u.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
            assert_eq!(parts[2].chars().next(), Some('4'), "version nibble must be 4: {u}");
            let variant = parts[3].chars().next().unwrap().to_digit(16).unwrap();
            assert!((0x8..=0xb).contains(&variant), "variant nibble must be 10xx: {u}");
        }
    }
    #[test]
    fn effective_uid_matches_proc_self_status_when_present() {
        // Don't assert a fixed outcome (root vs non-root varies by environment); just check it
        // agrees with a direct /proc/self/status read instead of always returning a fixed value.
        if let Ok(s) = std::fs::read_to_string("/proc/self/status") {
            let want = s.lines().find(|l| l.starts_with("Uid:")).map(|l| l.split_whitespace().nth(2) == Some("0")).unwrap_or(false);
            assert_eq!(effective_uid_is_root(), want);
        }
    }

    /// A unique scratch directory: no dev-dependency for temp files, and a fixed name would race
    /// with a parallel test run (cargo runs these threaded).
    fn scratch(tag: &str) -> PathBuf {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!("mantra-which-{tag}-{}-{}-{n}", std::process::id(), unix_secs()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn touch_exec(p: &Path) {
        std::fs::write(p, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    fn path_of(dirs: &[&PathBuf]) -> String {
        std::env::join_paths(dirs.iter().map(|d| d.as_os_str())).unwrap().to_string_lossy().into_owned()
    }

    #[test]
    fn which_finds_an_executable_prefers_the_first_path_entry_and_misses_cleanly() {
        let (first, second) = (scratch("first"), scratch("second"));
        touch_exec(&first.join("mantraprobe"));
        touch_exec(&second.join("mantraprobe"));
        let path = path_of(&[&first, &second]);
        assert_eq!(which_in("mantraprobe", &path), Some(first.join("mantraprobe")));
        assert_eq!(which_in("mantraprobe-absent", &path), None);
        assert_eq!(exec_problem(&first.join("mantraprobe")), None);
        // A name with a separator is a path, not a PATH lookup: it resolves with no PATH at all.
        assert_eq!(which_in(&second.join("mantraprobe").display().to_string(), ""), Some(second.join("mantraprobe")));
        assert_eq!(which_in(&second.join("nothing-here").display().to_string(), ""), None);
        let _ = std::fs::remove_dir_all(&first);
        let _ = std::fs::remove_dir_all(&second);
    }

    #[test]
    fn which_returns_a_directory_shadow_instead_of_skipping_it() {
        // The doctor case: something else on PATH owns the name, so execution fails with EACCES.
        let (shadow, real) = (scratch("shadow"), scratch("real"));
        std::fs::create_dir_all(shadow.join("mantraprobe")).unwrap();
        touch_exec(&real.join("mantraprobe"));
        let found = which_in("mantraprobe", &path_of(&[&shadow, &real])).expect("the shadowing entry must be reported, not skipped");
        assert_eq!(found, shadow.join("mantraprobe"));
        let why = exec_problem(&found).expect("a directory cannot be executed");
        assert!(why.contains(&found.display().to_string()) && why.contains("is a directory"), "must name the path: {why}");
        let _ = std::fs::remove_dir_all(&shadow);
        let _ = std::fs::remove_dir_all(&real);
    }

    #[cfg(unix)]
    #[test]
    fn exec_problem_names_the_file_that_lost_its_exec_bit() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("noexec");
        let f = dir.join("mantraprobe");
        std::fs::write(&f, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(which_in("mantraprobe", &path_of(&[&dir])), Some(f.clone()));
        let why = exec_problem(&f).expect("0o644 cannot be executed");
        let p = f.display().to_string();
        assert_eq!(why, format!("{p} is not executable — chmod +x {p}"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn strip_ansi_handles_osc_and_lone_esc() {
        assert_eq!(strip_ansi("a\u{1b}]0;title\u{7}b"), "ab");
        assert_eq!(strip_ansi("a\u{1b}]0;title\u{1b}\\b"), "ab");
        assert_eq!(strip_ansi("a\u{1b}b"), "ab");
        assert_eq!(strip_ansi("plain text"), "plain text");
    }
}

/// Can Codex's Linux sandbox (bubblewrap) start on this machine? It needs unprivileged user
/// namespaces; when the kernel or AppArmor forbids them every agent command fails before it
/// runs (`bwrap: … user namespaces`). Ok on non-Linux. The error text is the fix hint.
const SANDBOX_HINT: &str = "Codex's sandbox needs unprivileged user namespaces. Enable them (`sudo sysctl -w kernel.unprivileged_userns_clone=1`, or on Ubuntu 24.04+ `sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0`), or set sandbox = \"danger-full-access\" in ~/.mantra/settings.toml and on the worker roles (Studio) — workers stay isolated by git worktrees.";

/// The fix-it text `sandbox_probe` uses, exposed so a runtime failure (WP7/WP12.4: a
/// `commandExecution` naming `bwrap`/user namespaces) can attach it to a halt message without
/// re-running the (slower, up-to-2s) probe itself.
pub fn sandbox_fix_hint() -> &'static str {
    SANDBOX_HINT
}

pub fn sandbox_probe() -> Result<(), String> {
    if !cfg!(target_os = "linux") {
        return Ok(());
    }
    let hint = SANDBOX_HINT;
    let read = |p: &str| std::fs::read_to_string(p).ok().map(|s| s.trim().to_string());
    if read("/proc/sys/kernel/unprivileged_userns_clone").as_deref() == Some("0") {
        return Err(format!("kernel.unprivileged_userns_clone = 0 — {hint}"));
    }
    if read("/proc/sys/kernel/apparmor_restrict_unprivileged_userns").as_deref() == Some("1") {
        return Err(format!("kernel.apparmor_restrict_unprivileged_userns = 1 — {hint}"));
    }
    // A real attempt beats reading knobs: `unshare -U true` creates a user namespace and exits.
    if let Ok(mut child) = std::process::Command::new("unshare")
        .args(["-U", "true"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            match child.try_wait() {
                Ok(Some(st)) => {
                    if st.success() {
                        return Ok(());
                    }
                    return Err(format!("`unshare -U true` failed (user namespaces are blocked) — {hint}"));
                }
                Ok(None) if std::time::Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
                _ => {
                    let _ = child.kill();
                    return Ok(()); // undecidable: don't cry wolf
                }
            }
        }
    }
    Ok(())
}
