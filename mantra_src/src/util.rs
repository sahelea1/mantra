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

pub(crate) fn tz_offset_secs() -> u64 {
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

/// Directories under the home dir that package managers and build tools keep their caches, state or
/// SDK config in: a workspace-write agent cannot run them unless these are writable.
const TOOLCHAIN_HOME_DIRS: &[&str] = &[".cache", ".pub-cache", ".gradle", ".android", ".kotlin", ".m2", ".npm", ".cargo", ".dartServer", ".dart-tool", ".dart", ".flutter", ".flutter-devtools", ".config/flutter"];

/// The directories a workspace-write agent must be able to write to build with this machine's
/// toolchains: the caches under the home dir, the `PUB_CACHE` / `GRADLE_USER_HOME` / `CARGO_HOME`
/// overrides, and the Flutter SDK root. Only those that exist, each once, never the home dir itself.
pub fn toolchain_writable_roots() -> Vec<PathBuf> {
    let home = dirs::home_dir();
    let path_var = std::env::var("PATH").unwrap_or_default();
    if let Some(h) = home.as_deref() {
        ensure_toolchain_dirs(h, &path_var);
    }
    toolchain_roots_with(home.as_deref(), |k| std::env::var_os(k).map(PathBuf::from), &path_var)
}

/// The cache directories a toolchain on PATH will want on its first run, created ahead of time:
/// bubblewrap can only bind a directory that exists, so without this the first `dart analyze`
/// inside the sandbox dies on a read-only `~/.dartServer`, the first Gradle build on `~/.gradle`.
/// Only the tools actually installed get their directories; nothing is created for the rest.
fn ensure_toolchain_dirs(home: &Path, path_var: &str) {
    const BY_TOOL: &[(&str, &[&str])] = &[
        ("dart", &[".dart", ".dartServer", ".dart-tool", ".pub-cache"]),
        ("flutter", &[".dart", ".dartServer", ".dart-tool", ".pub-cache", ".flutter", ".flutter-devtools", ".config/flutter", ".gradle", ".android", ".kotlin"]),
        ("gradle", &[".gradle", ".kotlin", ".m2"]),
        ("java", &[".gradle", ".m2"]),
        ("cargo", &[".cargo"]),
        ("npm", &[".npm", ".cache"]),
    ];
    for (tool, dirs) in BY_TOOL {
        if which_in(tool, path_var).is_none() {
            continue;
        }
        for d in *dirs {
            let _ = std::fs::create_dir_all(home.join(d));
        }
    }
}

/// `toolchain_writable_roots` with its inputs passed in, so the candidate list can be tested.
fn toolchain_roots_with(home: Option<&Path>, env: impl Fn(&str) -> Option<PathBuf>, path_var: &str) -> Vec<PathBuf> {
    let mut cands: Vec<PathBuf> = home.map(|h| TOOLCHAIN_HOME_DIRS.iter().map(|d| h.join(d)).collect()).unwrap_or_default();
    for var in ["PUB_CACHE", "GRADLE_USER_HOME", "CARGO_HOME"] {
        cands.extend(env(var).filter(|p| !p.as_os_str().is_empty()));
    }
    cands.extend(flutter_sdk_root(path_var));
    let home_canon = home.and_then(|h| h.canonicalize().ok());
    let mut seen: Vec<PathBuf> = vec![];
    let mut out = vec![];
    for p in cands {
        // canonicalize() is the existence check, and it lets two spellings of one directory count once
        let Ok(canon) = p.canonicalize() else { continue };
        if !sandbox_can_own(&canon) || home_canon.as_ref() == Some(&canon) || seen.contains(&canon) {
            continue;
        }
        seen.push(canon);
        out.push(p);
    }
    out
}

/// Can bubblewrap bind this directory writable for the agent? Inside the sandbox's user namespace
/// the agent is root only over its own uid, so a directory another user owns cannot even take the
/// `.codex` marker Codex creates in every writable root (`bwrap: Can't mkdir <root>/.codex:
/// Permission denied`), and the whole sandbox fails to start. Linux only; elsewhere every
/// directory passes.
pub fn sandbox_can_own(p: &Path) -> bool {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::MetadataExt;
        let me = match std::fs::metadata("/proc/self") {
            Ok(m) => m.uid(),
            Err(_) => return true,
        };
        return std::fs::metadata(p).map(|m| m.is_dir() && m.uid() == me).unwrap_or(false);
    }
    #[allow(unreachable_code)]
    p.is_dir()
}

/// The fix hint for a bwrap failure that names a directory it could not set up: a writable root
/// the user does not own, not a missing user namespace.
pub fn writable_root_fix_hint(msg: &str) -> Option<String> {
    let rest = msg.split("Can't mkdir ").nth(1)?;
    let path = rest.split(':').next()?.trim();
    let root = path.strip_suffix("/.codex").unwrap_or(path);
    Some(format!(
        "bwrap could not set up the writable root {root}: inside the sandbox only directories you own can be bound writable. `chown` it (or a copy of it) to your user, or drop it from the pattern's writable_roots; Mantra skips toolchain directories you do not own."
    ))
}

/// `<sdk>` for the first executable `flutter` on PATH at `<sdk>/bin/flutter` (symlinks resolved).
fn flutter_sdk_root(path_var: &str) -> Option<PathBuf> {
    let bin = std::env::split_paths(path_var)
        .filter(|d| !d.as_os_str().is_empty())
        .map(|d| d.join("flutter"))
        .find(|p| std::fs::metadata(p).map(|m| m.is_file() && exec_bit_problem(&m, p).is_none()).unwrap_or(false))?;
    let real = bin.canonicalize().ok()?;
    real.parent()?.parent().map(Path::to_path_buf)
}

/// `~` and `~/rest` expand to the home dir when there is one; any other path is taken as written.
pub fn expand_tilde(p: &str, home: Option<&Path>) -> PathBuf {
    let Some(h) = home else { return PathBuf::from(p) };
    if p == "~" {
        return h.to_path_buf();
    }
    p.strip_prefix("~/").map(|rest| h.join(rest)).unwrap_or_else(|| PathBuf::from(p))
}

// ───────────────────────────── images ─────────────────────────────

/// The variable naming a screen capture command template: `{out}` is the PNG to write, `{region}`
/// (optional) the `x,y,w,h` asked for. Tests use it, and so can anyone with a tool of their own.
pub const SCREENSHOT_VAR: &str = "MANTRA_SCREENSHOT_CMD";

/// The largest image `read_image` hands a model: past this an image is mostly context spent.
pub const IMAGE_MAX_BYTES: u64 = 8 * 1024 * 1024;

/// One capture attempt at most: a tool that waits for a click (a portal dialog) is not one that
/// works. (Captures run as background jobs, never on the event loop — `tools::vision_call`.)
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(10);

/// What a capture left behind: the PNG, the tool that wrote it, and anything the model should
/// know about how it was taken (a region that could not be applied).
#[derive(Debug, Clone, PartialEq)]
pub struct Capture {
    pub path: PathBuf,
    pub via: &'static str,
    pub note: Option<&'static str>,
}

/// A screen area in pixels, from `"x,y,w,h"`.
fn parse_region(s: &str) -> Result<[u32; 4], String> {
    let bad = || format!("region must be \"x,y,w,h\" in pixels, e.g. \"0,0,800,600\" (got {s:?})");
    let n: Vec<u32> = s.split(',').map(|p| p.trim().parse::<u32>()).collect::<Result<_, _>>().map_err(|_| bad())?;
    match n[..] {
        [x, y, w, h] if w > 0 && h > 0 => Ok([x, y, w, h]),
        _ => Err(bad()),
    }
}

/// `s` as one word for `sh`.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Capture the screen of the machine Mantra runs on into `out` (a PNG): the first that works of
/// `$MANTRA_SCREENSHOT_CMD`, grim (Wayland), gnome-screenshot, scrot, ImageMagick's import (X11)
/// and screencapture (macOS) — the list bija's own `screenshot` tries, so both see the same. None
/// working → the error says what was tried and what to install.
pub fn screenshot(region: Option<&str>, display: Option<&str>, out: &Path) -> Result<Capture, String> {
    let template = std::env::var(SCREENSHOT_VAR).ok();
    capture_with(template.as_deref(), region, display, out, &which)
}

type Attempt = (&'static str, Result<std::process::Command, String>, Option<&'static str>);

/// `find` stands in for `which` so tests can leave every real capture tool out.
fn capture_with(template: Option<&str>, region: Option<&str>, display: Option<&str>, out: &Path, find: &dyn Fn(&str) -> Option<PathBuf>) -> Result<Capture, String> {
    use std::process::Command;
    let region = region.map(str::trim).filter(|r| !r.is_empty()).map(parse_region).transpose()?;
    let display = display.map(str::trim).filter(|d| !d.is_empty());
    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    let csv = region.map(|[x, y, w, h]| format!("{x},{y},{w},{h}"));
    let installed = |name: &str| find(name).map(Command::new).ok_or_else(|| "not installed".to_string());
    let x11 = |mut c: Command| {
        if let Some(d) = display {
            c.env("DISPLAY", d);
        }
        c
    };
    let attempts: Vec<Attempt> = vec![
        (
            SCREENSHOT_VAR,
            match template.map(str::trim).filter(|t| !t.is_empty()) {
                None => Err("not set".into()),
                Some(t) if !t.contains("{out}") => Err("has no {out} in it".into()),
                Some(t) => {
                    let line = t.replace("{out}", &sh_quote(&out.to_string_lossy())).replace("{region}", &csv.as_deref().map(sh_quote).unwrap_or_default());
                    let mut c = Command::new("sh");
                    c.arg("-c").arg(line);
                    Ok(x11(c))
                }
            },
            None,
        ),
        (
            "grim",
            installed("grim").map(|mut c| {
                if let Some(d) = display {
                    c.arg("-o").arg(d);
                }
                if let Some([x, y, w, h]) = region {
                    c.arg("-g").arg(format!("{x},{y} {w}x{h}"));
                }
                c.arg(out);
                c
            }),
            None,
        ),
        (
            "gnome-screenshot",
            installed("gnome-screenshot").map(|mut c| {
                c.arg("-f").arg(out);
                x11(c)
            }),
            region.map(|_| "the region was not applied: gnome-screenshot captures the whole screen"),
        ),
        (
            "scrot",
            installed("scrot").map(|mut c| {
                if let Some(r) = &csv {
                    c.arg("-a").arg(r);
                }
                c.arg(out);
                x11(c)
            }),
            None,
        ),
        (
            "import",
            installed("import").map(|mut c| {
                c.arg("-window").arg("root");
                if let Some([x, y, w, h]) = region {
                    c.arg("-crop").arg(format!("{w}x{h}+{x}+{y}"));
                }
                c.arg(out);
                x11(c)
            }),
            None,
        ),
        (
            "screencapture",
            installed("screencapture").map(|mut c| {
                c.arg("-x");
                if let Some(r) = &csv {
                    c.arg("-R").arg(r);
                }
                if let Some(d) = display {
                    c.arg("-D").arg(d);
                }
                c.arg(out);
                c
            }),
            None,
        ),
    ];
    let mut tried = vec![];
    for (via, cmd, note) in attempts {
        let failure = match cmd {
            Err(why) => why,
            Ok(cmd) => {
                let _ = std::fs::remove_file(out);
                match run_capped(cmd, CAPTURE_TIMEOUT) {
                    Err(e) => e,
                    Ok((Some(0), _)) => match std::fs::read(out) {
                        Ok(bytes) if image_mime(&bytes).is_some() => return Ok(Capture { path: out.to_path_buf(), via, note }),
                        Ok(bytes) if bytes.is_empty() => "wrote an empty file".into(),
                        Ok(_) => "wrote something that is not an image".into(),
                        Err(_) => "exit code 0, no image written".into(),
                    },
                    Ok((code, said)) => match said {
                        Some(said) => format!("exit code {}: {said}", code.unwrap_or(-1)),
                        None => format!("exit code {}, no image written", code.unwrap_or(-1)),
                    },
                }
            }
        };
        let _ = std::fs::remove_file(out);
        tried.push(format!("{via} ({failure})"));
    }
    let mut msg = String::from("no screen capture worked here. ");
    if cfg!(target_os = "linux") && std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
        msg.push_str("No display is set (DISPLAY, WAYLAND_DISPLAY). ");
    }
    msg.push_str(&format!("Tried: {}. Install grim (Wayland), scrot or ImageMagick (X11), or set {SCREENSHOT_VAR} to a command that writes a PNG to {{out}}.", tried.join("; ")));
    Err(msg)
}

/// Run a capture command to its end or `limit`, whichever comes first: its exit code and the last
/// line it printed on stderr. Its output never reaches the terminal (the TUI owns it), and on
/// Unix it gets a process group of its own, so a timeout ends the `sh -c` template's children too.
fn run_capped(mut cmd: std::process::Command, limit: Duration) -> Result<(Option<i32>, Option<String>), String> {
    use std::io::Read;
    use std::process::Stdio;
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn().map_err(|e| format!("cannot start: {e}"))?;
    // read on the side: a chatty tool must not block on a full pipe while we wait for it
    let reader = child.stderr.take().map(|mut err| {
        std::thread::spawn(move || {
            let mut s = String::new();
            let _ = err.read_to_string(&mut s);
            s
        })
    });
    let deadline = std::time::Instant::now() + limit;
    let code = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st.code(),
            Ok(None) if std::time::Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            Ok(None) => {
                #[cfg(unix)]
                let _ = std::process::Command::new("kill").args(["-KILL", "--", &format!("-{}", child.id())]).stderr(Stdio::null()).status();
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("no capture within {} s", limit.as_secs()));
            }
            Err(e) => return Err(format!("lost track of it: {e}")),
        }
    };
    let said = reader.and_then(|r| r.join().ok()).unwrap_or_default();
    let last = said.lines().rev().map(str::trim).find(|l| !l.is_empty()).map(|l| trunc(l, 200));
    Ok((code, last))
}

/// An image's media type from its first bytes (never from its name): PNG, JPEG, GIF or WebP.
pub fn image_mime(b: &[u8]) -> Option<&'static str> {
    if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if b.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if b.starts_with(b"GIF87a") || b.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if b.len() >= 12 && &b[..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// Read an image for a model: its media type and bytes. PNG, JPEG, GIF or WebP, at most
/// `IMAGE_MAX_BYTES` — the size it claims is checked before anything is read, and the read itself
/// stops one byte past the cap: a /proc file says it is empty and reads on for gigabytes.
pub fn read_image(path: &Path) -> Result<(&'static str, Vec<u8>), String> {
    use std::io::Read;
    let too_big = |n: u64| format!("{} is {} — images go up to {}", path.display(), human_bytes(n), human_bytes(IMAGE_MAX_BYTES));
    let md = std::fs::metadata(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    if !md.is_file() {
        return Err(format!("{} is not a file", path.display()));
    }
    if md.len() > IMAGE_MAX_BYTES {
        return Err(too_big(md.len()));
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path).and_then(|f| f.take(IMAGE_MAX_BYTES + 1).read_to_end(&mut bytes)).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    if bytes.len() as u64 > IMAGE_MAX_BYTES {
        return Err(format!("{} reads on past {} — images go up to that", path.display(), human_bytes(IMAGE_MAX_BYTES)));
    }
    match image_mime(&bytes) {
        Some(mime) => Ok((mime, bytes)),
        None => Err(format!("{} is not a PNG, JPEG, GIF or WebP image", path.display())),
    }
}

/// `812 B`, `40.2 KiB`, `3.1 MiB`.
pub fn human_bytes(n: u64) -> String {
    match n {
        0..=1023 => format!("{n} B"),
        1024..=1_048_575 => format!("{:.1} KiB", n as f64 / 1024.0),
        _ => format!("{:.1} MiB", n as f64 / 1_048_576.0),
    }
}

/// A PNG's width and height, from its IHDR chunk.
pub fn png_size(b: &[u8]) -> Option<(u32, u32)> {
    if image_mime(b) != Some("image/png") || b.len() < 24 || &b[12..16] != b"IHDR" {
        return None;
    }
    let be = |i: usize| u32::from_be_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
    Some((be(16), be(20)))
}

/// Standard base64 with padding — what a `data:` URL carries.
pub fn b64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        out.push(T[(n >> 18 & 63) as usize] as char);
        out.push(T[(n >> 12 & 63) as usize] as char);
        out.push(if c.len() > 1 { T[(n >> 6 & 63) as usize] as char } else { '=' });
        out.push(if c.len() > 2 { T[(n & 63) as usize] as char } else { '=' });
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    #[test]
    fn toolchain_roots_are_the_existing_caches_overrides_and_flutter_sdk_each_once() {
        let root = scratch("toolchain");
        let home = root.join("home");
        for d in [".cargo", ".pub-cache", ".config/flutter"] {
            std::fs::create_dir_all(home.join(d)).unwrap();
        }
        let pub_cache = root.join("elsewhere-pub-cache");
        std::fs::create_dir_all(&pub_cache).unwrap();
        let sdk = root.join("flutter-sdk");
        std::fs::create_dir_all(sdk.join("bin")).unwrap();
        touch_exec(&sdk.join("bin").join("flutter"));
        let path_var = path_of(&[&root.join("nothing-here"), &sdk.join("bin")]);
        let env = |k: &str| match k {
            "PUB_CACHE" => Some(pub_cache.clone()),
            "CARGO_HOME" => Some(home.join(".cargo")),
            _ => None,
        };
        let got = toolchain_roots_with(Some(&home), env, &path_var);
        for want in [home.join(".cargo"), home.join(".pub-cache"), home.join(".config/flutter"), pub_cache.clone()] {
            assert!(got.contains(&want), "{want:?} is missing from {got:?}");
        }
        assert_eq!(got.iter().filter(|p| **p == home.join(".cargo")).count(), 1, "CARGO_HOME and ~/.cargo are one root: {got:?}");
        assert!(!got.contains(&home.join(".gradle")), "a cache that does not exist is not a root: {got:?}");
        assert!(!got.contains(&home), "never the home dir itself: {got:?}");
        assert!(got.contains(&sdk.canonicalize().unwrap()), "<sdk>/bin/flutter gives <sdk> as the root: {got:?}");
        // without a home dir the caches under it are not candidates, but the env overrides and the SDK still are
        let bare = toolchain_roots_with(None, env, &path_var);
        assert!(!bare.contains(&home.join(".pub-cache")) && !bare.contains(&home.join(".config/flutter")), "{bare:?}");
        assert!(bare.contains(&pub_cache) && bare.contains(&home.join(".cargo")) && bare.contains(&sdk.canonicalize().unwrap()), "{bare:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn tilde_expands_to_the_home_dir_only_at_the_front() {
        let home = Path::new("/home/someone");
        assert_eq!(expand_tilde("~/.cargo", Some(home)), home.join(".cargo"));
        assert_eq!(expand_tilde("~", Some(home)), home.to_path_buf());
        assert_eq!(expand_tilde("/opt/x~y", Some(home)), PathBuf::from("/opt/x~y"));
        assert_eq!(expand_tilde("~/.cargo", None), PathBuf::from("~/.cargo"));
    }

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
    fn toolchain_dirs_are_created_only_for_tools_on_path() {
        let root = scratch("ensure");
        let home = root.join("home");
        std::fs::create_dir_all(&home).unwrap();
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        touch_exec(&bin.join("cargo"));
        ensure_toolchain_dirs(&home, &path_of(&[&bin]));
        assert!(home.join(".cargo").is_dir(), "cargo is on PATH: its cache dir exists");
        assert!(!home.join(".gradle").exists() && !home.join(".dartServer").exists(), "nothing for tools that are not installed");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_bwrap_mkdir_failure_names_the_writable_root_not_user_namespaces() {
        let hint = writable_root_fix_hint("bwrap: Can't mkdir /opt/flutter/.codex: Permission denied").unwrap();
        assert!(hint.contains("writable root /opt/flutter") && !hint.contains("/.codex"), "{hint}");
        assert!(writable_root_fix_hint("bwrap: setting up uid map: Permission denied").is_none());
    }

    #[test]
    fn only_an_existing_directory_can_be_a_sandbox_root() {
        let root = scratch("own");
        std::fs::create_dir_all(&root).unwrap();
        assert!(sandbox_can_own(&root));
        assert!(!sandbox_can_own(&root.join("missing")));
        std::fs::write(root.join("file"), "x").unwrap();
        assert!(!sandbox_can_own(&root.join("file")), "a file is not a root");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn strip_ansi_handles_osc_and_lone_esc() {
        assert_eq!(strip_ansi("a\u{1b}]0;title\u{7}b"), "ab");
        assert_eq!(strip_ansi("a\u{1b}]0;title\u{1b}\\b"), "ab");
        assert_eq!(strip_ansi("a\u{1b}b"), "ab");
        assert_eq!(strip_ansi("plain text"), "plain text");
    }

    /// A real (if tiny) PNG: `w`×`h` gray pixels, encoded as the demo's MCP server encodes its preview.
    pub(crate) fn tiny_png(w: u32, h: u32) -> Vec<u8> {
        crate::mock_mcp::encode_png(w, h, &vec![0x80; (w * h * 3) as usize])
    }

    #[test]
    fn images_are_known_by_their_bytes_and_capped_before_reading() {
        let dir = scratch("images");
        let png = tiny_png(3, 2);
        assert_eq!((image_mime(&png), png_size(&png)), (Some("image/png"), Some((3, 2))));
        assert_eq!(image_mime(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("image/jpeg"));
        assert_eq!(image_mime(b"GIF89a.."), Some("image/gif"));
        assert_eq!(image_mime(b"RIFF\0\0\0\0WEBPVP8 "), Some("image/webp"));
        assert_eq!(image_mime(b"%PDF-1.7"), None);
        // the name says nothing: a PNG called .jpg is a PNG, a text file called .png is refused
        std::fs::write(dir.join("shot.jpg"), &png).unwrap();
        assert_eq!(read_image(&dir.join("shot.jpg")).map(|(m, b)| (m, b.len())), Ok(("image/png", png.len())));
        std::fs::write(dir.join("notes.png"), "hello").unwrap();
        assert!(read_image(&dir.join("notes.png")).unwrap_err().contains("is not a PNG, JPEG, GIF or WebP image"));
        assert!(read_image(&dir.join("missing.png")).unwrap_err().starts_with("cannot read"));
        assert!(read_image(&dir).unwrap_err().ends_with("is not a file"));
        // one byte over the cap is refused on its size alone (a sparse file: nothing is written)
        let big = std::fs::File::create(dir.join("big.png")).unwrap();
        big.set_len(IMAGE_MAX_BYTES + 1).unwrap();
        let e = read_image(&dir.join("big.png")).unwrap_err();
        assert!(e.contains("8.0 MiB — images go up to 8.0 MiB"), "{e}");
        // a /proc file says it is empty and reads on for gigabytes: the read stops past the cap
        let pagemap = Path::new("/proc/self/pagemap");
        // (it takes reads of 8 bytes at a time only, so the cap's one extra byte may be refused as
        // an invalid argument instead — a refusal either way, never a read to its end)
        if std::fs::File::open(pagemap).is_ok() {
            let e = read_image(pagemap).unwrap_err();
            assert!(e.contains("reads on past 8.0 MiB") || e.contains("Invalid argument"), "{e}");
        }
    }

    #[test]
    fn base64_is_the_padded_standard_alphabet() {
        assert_eq!(b64(b""), "");
        assert_eq!(b64(b"f"), "Zg==");
        assert_eq!(b64(b"fo"), "Zm8=");
        assert_eq!(b64(b"foo"), "Zm9v");
        assert_eq!(b64(&[0xFB, 0xFF]), "+/8=");
    }

    #[test]
    fn a_capture_tries_the_template_first_then_names_what_it_tried() {
        let dir = scratch("capture");
        let src = dir.join("src.png");
        std::fs::write(&src, tiny_png(4, 4)).unwrap();
        let out = dir.join("shots").join("solo-1.png");
        let nothing = |_: &str| None;
        // the template, its region quoted for sh
        let t = format!("cp {} {{out}} && echo {{region}} > {}", src.display(), dir.join("region.txt").display());
        let got = capture_with(Some(&t), Some("10, 20,300,200"), None, &out, &nothing).unwrap();
        assert_eq!((got.path.clone(), got.via, got.note), (out.clone(), SCREENSHOT_VAR, None));
        assert_eq!(std::fs::read(&out).unwrap(), tiny_png(4, 4));
        assert_eq!(std::fs::read_to_string(dir.join("region.txt")).unwrap().trim(), "10,20,300,200");
        // a bad region is refused before anything runs
        assert!(capture_with(Some(&t), Some("10,20"), None, &out, &nothing).unwrap_err().starts_with("region must be"));
        // nothing works: every tool is named with why, and what to install
        let e = capture_with(Some("false"), None, None, &out, &nothing).unwrap_err();
        assert!(e.contains("MANTRA_SCREENSHOT_CMD (has no {out} in it)") && e.contains("grim (not installed)") && e.contains("screencapture (not installed)"), "{e}");
        assert!(e.contains("Install grim (Wayland), scrot or ImageMagick (X11)"), "{e}");
        let e = capture_with(Some("echo nope >&2; exit 3 # {out}"), None, None, &out, &nothing).unwrap_err();
        assert!(e.contains("MANTRA_SCREENSHOT_CMD (exit code 3: nope)"), "{e}");
        let e = capture_with(Some("echo text > {out}"), None, None, &out, &nothing).unwrap_err();
        assert!(e.contains("wrote something that is not an image") && !out.exists(), "{e}");
        let e = capture_with(None, None, None, &out, &nothing).unwrap_err();
        assert!(e.contains("MANTRA_SCREENSHOT_CMD (not set)"), "{e}");
    }

    #[test]
    fn a_found_tool_gets_the_region_its_own_way() {
        let dir = scratch("capture-tool");
        // a stand-in `scrot` that records its arguments and writes a PNG to the last one
        let bin = dir.join("scrot");
        std::fs::write(dir.join("src.png"), tiny_png(1, 1)).unwrap();
        std::fs::write(&bin, format!("#!/bin/sh\necho \"$@\" > {}\nfor a; do last=$a; done\ncp {} \"$last\"\n", dir.join("args").display(), dir.join("src.png").display())).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let find = |n: &str| (n == "scrot").then(|| bin.clone());
        let out = dir.join("x.png");
        let got = capture_with(None, Some("1,2,3,4"), None, &out, &find).unwrap();
        assert_eq!(got.via, "scrot");
        assert_eq!(std::fs::read_to_string(dir.join("args")).unwrap().trim(), format!("-a 1,2,3,4 {}", out.display()));
    }
}

/// Can Codex's Linux sandbox (bubblewrap) start on this machine? It needs unprivileged user
/// namespaces; when the kernel or AppArmor forbids them every agent command fails before it
/// runs (`bwrap: … user namespaces`). Ok on non-Linux. The error text is the fix hint.
const SANDBOX_HINT: &str = "Codex's sandbox needs unprivileged user namespaces. Enable them (`sudo sysctl -w kernel.unprivileged_userns_clone=1`, or on Ubuntu 24.04+ `sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0`). Until then Mantra runs Codex agents with danger-full-access (no isolation; git worktrees still keep workers apart).";

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
