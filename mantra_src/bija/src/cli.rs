//! argv parsing and the subcommands (SPEC §2). `run()` returns the process exit code.
//!
//! Parsing is hand-rolled like mantra's and kept apart from everything with side effects: `parse`
//! turns argv into a `Command`, and the settings that fall back to the environment are resolved
//! against an injected lookup, so both are unit-tested without touching the process. Options are
//! `Option`s because a `--resume` keeps the stored meta and lets only the flags actually given
//! override it. The API key is read from the variable the meta names and never appears in argv.

use crate::agent::{self, Agent, Input, Out, Sink, Start};
use crate::api::{Client, ClientConfig};
use crate::control::{self, Control};
use crate::mcp;
use crate::proto::{Event, Op, TurnStatus};
use crate::session::{self, Fsync, Meta, Pending, Record, Session};
use crate::tools::{Runtime, RuntimeConfig, Sandbox};
use crate::types::{absolute, clip, now_unix, uuid_v4, Message, ToolDef, Wire};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::io::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::str::FromStr;
use std::task::{ready, Context, Poll};
use std::time::Duration;
use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader, Interest, ReadBuf};
use tokio::net::UnixStream;
use tokio::sync::mpsc::{self, UnboundedSender};

const VERSION: &str = env!("CARGO_PKG_VERSION");

const HELP: &str = "\
bija — Mantra's seed harness: a minimal, resilient coding agent for any
OpenAI- or Anthropic-compatible API. Every message is on disk as it happens;
a crashed session resumes.

Usage:
  bija [OPTIONS]                 serve NDJSON on stdio (what Mantra runs)
  bija run [OPTIONS] <prompt>    one turn; prints the final text (\"-\": stdin)
  bija sessions                  list sessions, most recent first
  bija show <id> [-f]            print a session's transcript; -f, --follow
                                 keeps following it
  bija send <id> <text>          prompt a running --control session
  bija send <id> --json <op>     send one protocol op, e.g. '{\"op\":\"status\"}'
  bija -V, --version | -h, --help | help
  sessions, show and send take --home DIR too.

Options (serve and run):                                              [default]
  --home DIR               sessions live in DIR/sessions/<id>/
                                                        [$BIJA_HOME or ~/.bija]
  --session ID             id of a new session                    [random uuid]
  --resume ID              reopen a session and continue it (its stored
                           settings; flags given override them)
  -C, --cwd DIR            the agent's working directory          [current dir]
  --provider-url URL       API root, e.g. https://api.openai.com/v1
                                                           [$BIJA_PROVIDER_URL]
  --wire openai-chat|anthropic   the API the URL speaks           [openai-chat]
  --api-key-env NAME       env var holding the API key           [BIJA_API_KEY]
  --model ID               model id sent to the provider          [$BIJA_MODEL]
  --effort LEVEL           low|medium|high|xhigh|max; none sends no reasoning
  --context-window N       tokens to manage the context against        [200000]
  --compact-at PCT         auto-compact above PCT of the window; 0 never   [85]
  --max-output-tokens N    max tokens per reply
                                         [provider default; 16384 on anthropic]
  --system TEXT            system prompt, after bija's own preamble
  --system-file PATH       the same, read from a file
  --tools-file PATH        JSON array of external tools, answered over the
                           protocol
  --mcp-file PATH          MCP servers to start with the session, as
                           {\"mcpServers\": {\"<name>\": {\"command\", \"args\"}}};
                           their tools are named mcp__<server>__<tool>
  --vision                 offer screenshot and view_image, for a model that
                           takes images (screenshot sees this machine's screen)
                           — this launch only: a resume without it has none
  --sandbox MODE           read-only|workspace-write|full     [workspace-write]
  --writable DIR           an extra writable root (repeatable)
  --network                allow network inside sandboxed commands
  --no-bwrap               never wrap commands in bubblewrap
  --max-retries N          retries per API call                             [8]
  --timeout-secs N         time budget per API request, each retry its own
                                                                          [600]
  --no-stream              non-streaming requests (gateways that break SSE)
  --name TEXT              display name of the session
  --control                serve: listen on <session dir>/control.sock
  --fsync turn|always|never   when the transcript is fsynced             [turn]
  -v, --verbose            run: print the events to stderr

MCP servers run in the working directory, outside the sandbox, without the
API key's variable; their stderr is shown prefixed `mcp <name>:`. One that
does not answer within 20 s is skipped with a warning. Images (screenshots,
MCP images) are saved under <session dir>/images/. A capture uses
$MANTRA_SCREENSHOT_CMD when set (`{out}`: the file to write, `{region}`:
x,y,w,h), else grim, gnome-screenshot, scrot, import or screencapture.

Exit codes: 0 ok, 1 runtime failure, 2 usage error, 128+N (see Signals).

Signals: SIGTERM and SIGHUP stop at once but leave a running turn open, as a
crash would: the next --resume continues it (exit 143 or 129). The shutdown
op and stdin EOF end the turn first (exit 0). SIGINT is a shutdown for serve
and an interrupt for run.
";

/// The options shared by serve and run (canonical spellings, for `only`).
const SESSION_OPTIONS: &[&str] = &[
    "--home", "--session", "--resume", "--cwd", "--provider-url", "--wire", "--api-key-env", "--model", "--effort", "--context-window", "--compact-at", "--max-output-tokens", "--system", "--system-file", "--tools-file", "--mcp-file", "--vision",
    "--sandbox", "--writable", "--network", "--no-bwrap", "--max-retries", "--timeout-secs", "--no-stream", "--name", "--fsync",
];

/// An environment lookup: the process environment in `run`, a map in tests.
type Env<'a> = &'a dyn Fn(&str) -> Option<String>;

#[derive(Debug, PartialEq)]
enum Command {
    Help,
    Version,
    Serve(Flags),
    Run { flags: Flags, prompt: String },
    Sessions { home: Option<PathBuf> },
    Show { id: String, home: Option<PathBuf>, follow: bool },
    Send { id: String, home: Option<PathBuf>, what: SendWhat },
}

#[derive(Debug, PartialEq)]
enum SendWhat {
    Prompt(String),
    Op(Op),
}

#[derive(Debug, Clone, PartialEq)]
enum SystemSource {
    Text(String),
    File(PathBuf),
}

#[derive(Debug, Clone, Default, PartialEq)]
struct Flags {
    home: Option<PathBuf>,
    session: Option<String>,
    resume: Option<String>,
    cwd: Option<PathBuf>,
    provider_url: Option<String>,
    wire: Option<Wire>,
    api_key_env: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    context_window: Option<u64>,
    compact_at: Option<u8>,
    max_output_tokens: Option<u32>,
    system: Option<SystemSource>,
    tools_file: Option<PathBuf>,
    mcp_file: Option<PathBuf>,
    vision: bool,
    sandbox: Option<Sandbox>,
    writable: Vec<PathBuf>,
    network: bool,
    no_bwrap: bool,
    max_retries: Option<u32>,
    timeout_secs: Option<u64>,
    no_stream: bool,
    name: Option<String>,
    control: bool,
    fsync: Option<Fsync>,
    verbose: bool,
    follow: bool,
    json: Option<String>,
    help: bool,
    version: bool,
    /// Canonical names of the options given, so a subcommand can refuse the ones it ignores.
    given: Vec<&'static str>,
}

/// A failure and the exit code it maps to (SPEC §2).
#[derive(Debug, PartialEq)]
enum Fail {
    Usage(String),
    Runtime(String),
}

impl Fail {
    fn runtime(e: impl std::fmt::Display) -> Fail {
        Fail::Runtime(e.to_string())
    }
}

pub async fn run() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let env = |k: &str| std::env::var(k).ok();
    let result = match parse(&args) {
        Err(m) => Err(Fail::Usage(m)),
        Ok(Command::Help) => {
            say(HELP.trim_end());
            Ok(0)
        }
        Ok(Command::Version) => {
            say(&format!("bija {VERSION}"));
            Ok(0)
        }
        Ok(Command::Serve(flags)) => serve(flags, &env).await,
        Ok(Command::Run { flags, prompt }) => run_once(flags, prompt, &env).await,
        Ok(Command::Sessions { home }) => sessions(home, &env),
        Ok(Command::Show { id, home, follow }) => show(&id, home, follow, &env).await,
        Ok(Command::Send { id, home, what }) => send(&id, home, what, &env).await,
    };
    match result {
        Ok(code) => code,
        Err(Fail::Usage(m)) => {
            eprintln!("bija: {m}");
            2
        }
        Err(Fail::Runtime(m)) => {
            eprintln!("bija: {m}");
            1
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Parsing

fn parse(args: &[String]) -> Result<Command, String> {
    let sub = args.first().map(String::as_str);
    let rest = if matches!(sub, Some("run" | "sessions" | "show" | "send" | "help")) { &args[1..] } else { args };
    let (flags, words) = parse_flags(rest)?;
    if flags.help || sub == Some("help") {
        return Ok(Command::Help);
    }
    if flags.version {
        return Ok(Command::Version);
    }
    match sub {
        Some("run") => {
            only(&flags, &[SESSION_OPTIONS, &["--verbose"][..]].concat(), "bija run")?;
            if words.is_empty() {
                return Err("bija run: a prompt is required (\"-\" reads it from stdin)".into());
            }
            Ok(Command::Run { prompt: words.join(" "), flags })
        }
        Some("sessions") => {
            only(&flags, &["--home"], "bija sessions")?;
            no_words(&words, "bija sessions")?;
            Ok(Command::Sessions { home: flags.home })
        }
        Some("show") => {
            only(&flags, &["--home", "--follow"], "bija show")?;
            let [id] = words.as_slice() else {
                return Err("usage: bija show <id> [--home DIR] [-f]".into());
            };
            Ok(Command::Show { id: id.clone(), home: flags.home, follow: flags.follow })
        }
        Some("send") => {
            only(&flags, &["--home", "--json"], "bija send")?;
            let Some((id, text)) = words.split_first() else {
                return Err("usage: bija send <id> <text> | --json <op>".into());
            };
            let what = match (&flags.json, text.is_empty()) {
                (Some(op), true) => SendWhat::Op(Op::parse(op).map_err(|e| format!("--json: {e}"))?),
                (None, false) if !text.join(" ").trim().is_empty() => SendWhat::Prompt(text.join(" ")),
                _ => return Err("usage: bija send <id> <text> | --json <op>".into()),
            };
            Ok(Command::Send { id: id.clone(), home: flags.home, what })
        }
        _ => {
            only(&flags, &[SESSION_OPTIONS, &["--control"][..]].concat(), "serve mode")?;
            no_words(&words, "bija")?;
            Ok(Command::Serve(flags))
        }
    }
}

/// Options anywhere, `--opt value` or `--opt=value`; everything else is a word.
fn parse_flags(args: &[String]) -> Result<(Flags, Vec<String>), String> {
    let mut f = Flags::default();
    let mut words = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        let (name, inline) = match arg.split_once('=') {
            Some((n, v)) if n.starts_with("--") => (n, Some(v.to_string())),
            _ => (arg, None),
        };
        let mut value = || -> Result<String, String> {
            if let Some(v) = &inline {
                return Ok(v.clone());
            }
            i += 1;
            args.get(i).cloned().ok_or_else(|| format!("{name} needs a value"))
        };
        let canonical = match name {
            "-h" | "--help" => {
                f.help = true;
                i += 1;
                continue;
            }
            "-V" | "--version" => {
                f.version = true;
                i += 1;
                continue;
            }
            "--home" => {
                f.home = Some(PathBuf::from(value()?));
                "--home"
            }
            "--session" => {
                f.session = Some(value()?);
                "--session"
            }
            "--resume" => {
                f.resume = Some(value()?);
                "--resume"
            }
            "--cwd" | "-C" => {
                f.cwd = Some(PathBuf::from(value()?));
                "--cwd"
            }
            "--provider-url" => {
                f.provider_url = Some(value()?);
                "--provider-url"
            }
            "--wire" => {
                let v = value()?;
                f.wire = Some(Wire::parse(&v).ok_or_else(|| format!("--wire {v}: expected openai-chat or anthropic"))?);
                "--wire"
            }
            "--api-key-env" => {
                f.api_key_env = Some(value()?);
                "--api-key-env"
            }
            "--model" => {
                f.model = Some(value()?);
                "--model"
            }
            "--effort" => {
                f.effort = Some(value()?);
                "--effort"
            }
            "--context-window" => {
                f.context_window = Some(number(name, &value()?)?);
                "--context-window"
            }
            "--compact-at" => {
                let pct: u8 = number(name, &value()?)?;
                if pct > 100 {
                    return Err(format!("--compact-at {pct}: a percentage, 0..=100"));
                }
                f.compact_at = Some(pct);
                "--compact-at"
            }
            "--max-output-tokens" => {
                f.max_output_tokens = Some(number(name, &value()?)?);
                "--max-output-tokens"
            }
            "--system" => {
                set_system(&mut f, SystemSource::Text(value()?))?;
                "--system"
            }
            "--system-file" => {
                set_system(&mut f, SystemSource::File(PathBuf::from(value()?)))?;
                "--system-file"
            }
            "--tools-file" => {
                f.tools_file = Some(PathBuf::from(value()?));
                "--tools-file"
            }
            "--mcp-file" => {
                f.mcp_file = Some(PathBuf::from(value()?));
                "--mcp-file"
            }
            "--vision" => {
                f.vision = true;
                "--vision"
            }
            "--sandbox" => {
                let v = value()?;
                f.sandbox = Some(Sandbox::parse(&v).ok_or_else(|| format!("--sandbox {v}: expected read-only, workspace-write or full"))?);
                "--sandbox"
            }
            "--writable" => {
                f.writable.push(PathBuf::from(value()?));
                "--writable"
            }
            "--network" => {
                f.network = true;
                "--network"
            }
            "--no-bwrap" => {
                f.no_bwrap = true;
                "--no-bwrap"
            }
            "--max-retries" => {
                f.max_retries = Some(number(name, &value()?)?);
                "--max-retries"
            }
            "--timeout-secs" => {
                let secs: u64 = number(name, &value()?)?;
                if secs == 0 {
                    return Err("--timeout-secs must be at least 1".into());
                }
                f.timeout_secs = Some(secs);
                "--timeout-secs"
            }
            "--no-stream" => {
                f.no_stream = true;
                "--no-stream"
            }
            "--name" => {
                f.name = Some(value()?);
                "--name"
            }
            "--control" => {
                f.control = true;
                "--control"
            }
            "--fsync" => {
                let v = value()?;
                f.fsync = Some(Fsync::parse(&v).ok_or_else(|| format!("--fsync {v}: expected turn, always or never"))?);
                "--fsync"
            }
            "-v" | "--verbose" => {
                f.verbose = true;
                "--verbose"
            }
            "-f" | "--follow" => {
                f.follow = true;
                "--follow"
            }
            "--json" => {
                f.json = Some(value()?);
                "--json"
            }
            "--" => {
                words.extend(args[i + 1..].iter().cloned());
                break;
            }
            s if s.starts_with('-') && s.len() > 1 => return Err(format!("unknown option {s} (see bija --help)")),
            _ => {
                words.push(arg.to_string());
                i += 1;
                continue;
            }
        };
        if inline.is_some() && FLAGS_WITHOUT_VALUE.contains(&canonical) {
            return Err(format!("{canonical} takes no value"));
        }
        f.given.push(canonical);
        i += 1;
    }
    Ok((f, words))
}

const FLAGS_WITHOUT_VALUE: &[&str] = &["--network", "--no-bwrap", "--no-stream", "--control", "--verbose", "--follow", "--vision"];

fn set_system(f: &mut Flags, src: SystemSource) -> Result<(), String> {
    if f.system.is_some() {
        return Err("--system and --system-file: give one of them, once".into());
    }
    f.system = Some(src);
    Ok(())
}

fn number<T: FromStr>(flag: &str, v: &str) -> Result<T, String> {
    v.trim().parse().map_err(|_| format!("{flag} {v}: not a valid number"))
}

fn only(flags: &Flags, allowed: &[&str], what: &str) -> Result<(), String> {
    match flags.given.iter().find(|g| !allowed.contains(g)) {
        Some(g) => Err(format!("{g} does not apply to {what} (see bija --help)")),
        None => Ok(()),
    }
}

fn no_words(words: &[String], what: &str) -> Result<(), String> {
    match words.first() {
        Some(w) => Err(format!("{what}: unexpected argument {w} (see bija --help)")),
        None => Ok(()),
    }
}

// ---------------------------------------------------------------------------------------------
// Resolving flags, environment and stored meta into a launch

fn home_dir(flag: Option<PathBuf>, env: Env) -> Result<PathBuf, Fail> {
    let home = match (flag, env("BIJA_HOME").filter(|h| !h.trim().is_empty())) {
        (Some(h), _) => h,
        (None, Some(h)) => PathBuf::from(h),
        (None, None) => match env("HOME").filter(|h| !h.trim().is_empty()) {
            Some(h) => PathBuf::from(h).join(".bija"),
            None => return Err(Fail::Usage("no home directory: pass --home DIR or set BIJA_HOME".into())),
        },
    };
    Ok(absolute(&home))
}

/// The agent's working directory: absolute, resolved, and a directory that exists.
fn resolve_cwd(flag: Option<&Path>) -> Result<PathBuf, Fail> {
    let dir = absolute(flag.unwrap_or(Path::new(".")));
    let dir = dir.canonicalize().map_err(|e| Fail::Runtime(format!("working directory {}: {e}", dir.display())))?;
    if !dir.is_dir() {
        return Err(Fail::Runtime(format!("working directory {} is not a directory", dir.display())));
    }
    Ok(dir)
}

fn required(flag: &Option<String>, env: Env, var: &str, option: &str) -> Result<String, Fail> {
    flag.clone().or_else(|| env(var)).map(|v| v.trim().to_string()).filter(|v| !v.is_empty()).ok_or_else(|| Fail::Usage(format!("{option} is required (or set {var})")))
}

/// The meta of a new session: flags, then the environment fallbacks, then the defaults.
fn new_meta(flags: &Flags, env: Env, id: String) -> Result<Meta, Fail> {
    let d = Meta::default();
    Ok(Meta {
        id,
        name: flags.name.clone().unwrap_or_default(),
        cwd: resolve_cwd(flags.cwd.as_deref())?,
        provider_url: required(&flags.provider_url, env, "BIJA_PROVIDER_URL", "--provider-url")?.trim_end_matches('/').to_string(),
        wire: flags.wire.unwrap_or_default(),
        api_key_env: flags.api_key_env.clone().unwrap_or(d.api_key_env),
        model: required(&flags.model, env, "BIJA_MODEL", "--model")?,
        effort: agent::normalize_effort(flags.effort.as_deref().unwrap_or("")),
        context_window: flags.context_window.unwrap_or(d.context_window),
        compact_at: flags.compact_at.unwrap_or(d.compact_at),
        max_output_tokens: flags.max_output_tokens,
        tools_file: flags.tools_file.as_deref().map(absolute),
        mcp_file: flags.mcp_file.as_deref().map(absolute),
        vision: flags.vision,
        sandbox: flags.sandbox.unwrap_or(Sandbox::WorkspaceWrite).as_str().to_string(),
        writable: flags.writable.iter().map(|p| absolute(p)).collect(),
        network: flags.network,
        ..d
    })
}

/// `--resume`: the stored meta, with every flag given taking precedence (and saved with it).
fn apply_overrides(meta: &mut Meta, flags: &Flags) -> Result<(), Fail> {
    if let Some(cwd) = &flags.cwd {
        meta.cwd = resolve_cwd(Some(cwd))?;
    }
    if let Some(url) = &flags.provider_url {
        meta.provider_url = url.trim().trim_end_matches('/').to_string();
    }
    if let Some(w) = flags.wire {
        meta.wire = w;
    }
    if let Some(k) = &flags.api_key_env {
        meta.api_key_env = k.clone();
    }
    if let Some(m) = flags.model.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
        meta.model = m.to_string();
    }
    if let Some(e) = &flags.effort {
        meta.effort = agent::normalize_effort(e);
    }
    if let Some(n) = flags.context_window {
        meta.context_window = n;
    }
    if let Some(p) = flags.compact_at {
        meta.compact_at = p;
    }
    if flags.max_output_tokens.is_some() {
        meta.max_output_tokens = flags.max_output_tokens;
    }
    if let Some(t) = &flags.tools_file {
        meta.tools_file = Some(absolute(t));
    }
    if let Some(m) = &flags.mcp_file {
        meta.mcp_file = Some(absolute(m));
    }
    // Unlike --network, it follows the launch: whether the model takes images is the model's,
    // and a resume may bring another one (Mantra says it on every launch). Left out, it is off.
    meta.vision = flags.vision;
    if let Some(s) = flags.sandbox {
        meta.sandbox = s.as_str().to_string();
    }
    if !flags.writable.is_empty() {
        meta.writable = flags.writable.iter().map(|p| absolute(p)).collect();
    }
    if flags.network {
        meta.network = true;
    }
    if let Some(n) = &flags.name {
        meta.name = n.clone();
    }
    Ok(())
}

/// A failure to open or create a session as something to act on: bija's own errors already name
/// the session ("in use", "already exists"); a bare OS error gets the directory it happened in.
fn session_error(e: io::Error, dir: &Path) -> Fail {
    match e.raw_os_error() {
        Some(_) => Fail::Runtime(format!("{}: {e}", dir.display())),
        None => Fail::runtime(e),
    }
}

fn read_text(what: &str, path: &Path) -> Result<String, Fail> {
    std::fs::read_to_string(path).map_err(|e| Fail::Runtime(format!("{what} {}: {e}", path.display())))
}

/// Everything a session needs before its first event.
struct Launch {
    session: Session,
    pending: bool,
    resumed: bool,
    client: Client,
    runtime: Runtime,
    external: Vec<ToolDef>,
    warnings: Vec<String>,
}

enum Opened {
    Existing(Session, Pending),
    New(Meta),
}

/// Opens or creates the session and builds the client and the tool runtime. Every input is read
/// and checked before a new session directory is created, so a bad flag leaves nothing behind.
async fn launch(flags: &Flags, env: Env<'_>) -> Result<Launch, Fail> {
    let home = home_dir(flags.home.clone(), env)?;
    let fsync = flags.fsync.unwrap_or(Fsync::Turn);
    let mut warnings = Vec::new();
    let user_system = match &flags.system {
        Some(SystemSource::Text(t)) => Some(t.clone()),
        Some(SystemSource::File(p)) => Some(read_text("--system-file", &absolute(p))?),
        None => None,
    };
    let opened = match &flags.resume {
        Some(id) => {
            if flags.session.as_deref().is_some_and(|s| s != id) {
                return Err(Fail::Usage(format!("--session {} and --resume {id} name different sessions", flags.session.as_deref().unwrap_or(""))));
            }
            let (mut s, pending) = Session::open(&home, id, fsync).map_err(|e| session_error(e, &Session::dir_for(&home, id)))?;
            apply_overrides(&mut s.meta, flags)?;
            if !s.meta.cwd.is_dir() {
                return Err(Fail::Runtime(format!("the session's working directory {} is gone (pass --cwd)", s.meta.cwd.display())));
            }
            Opened::Existing(s, pending)
        }
        None => {
            let id = flags.session.clone().unwrap_or_else(uuid_v4);
            if !session::valid_id(&id) {
                return Err(Fail::Usage(format!("--session {id:?}: letters, digits, '-', '_' and '.' only")));
            }
            Opened::New(new_meta(flags, env, id)?)
        }
    };
    let meta = match &opened {
        Opened::Existing(s, _) => &s.meta,
        Opened::New(m) => m,
    };
    let external = match (&flags.tools_file, &meta.tools_file) {
        (Some(_), Some(path)) => agent::parse_tools(&read_text("--tools-file", path)?).map_err(|e| Fail::Runtime(format!("--tools-file {}: {e}", path.display())))?,
        // Stored by an earlier launch: a file gone since is worth a warning, not a dead session.
        (None, Some(path)) => match std::fs::read_to_string(path).map_err(|e| e.to_string()).and_then(|t| agent::parse_tools(&t)) {
            Ok(tools) => tools,
            Err(e) => {
                warnings.push(format!("external tools from {} unavailable: {e}", path.display()));
                Vec::new()
            }
        },
        _ => Vec::new(),
    };
    // Read and checked now, started once the session is open: a bad file leaves nothing behind,
    // and a session that cannot be opened starts no servers.
    let mcp_servers = match (&flags.mcp_file, &meta.mcp_file) {
        (Some(_), Some(path)) => {
            let (servers, skipped) = mcp::parse_file(&read_text("--mcp-file", path)?).map_err(|e| Fail::Runtime(format!("--mcp-file {}: {e}", path.display())))?;
            warnings.extend(skipped);
            servers
        }
        (None, Some(path)) => match std::fs::read_to_string(path).map_err(|e| e.to_string()).and_then(|t| mcp::parse_file(&t)) {
            Ok((servers, skipped)) => {
                warnings.extend(skipped);
                servers
            }
            Err(e) => {
                warnings.push(format!("MCP servers from {} unavailable: {e}", path.display()));
                Vec::new()
            }
        },
        _ => Vec::new(),
    };
    let sandbox = Sandbox::parse(&meta.sandbox).unwrap_or_else(|| {
        warnings.push(format!("unknown sandbox mode {:?} in session.json; using workspace-write", meta.sandbox));
        Sandbox::WorkspaceWrite
    });
    let dir = Session::dir_for(&home, &meta.id);
    let (jobs_dir, images_dir) = (dir.join("jobs"), dir.join("images"));
    // The key's variable (and bija's default one) never reaches a command's environment.
    let scrub_env: Vec<String> = [meta.api_key_env.trim(), session::DEFAULT_KEY_ENV].iter().filter(|v| !v.is_empty()).map(|v| v.to_string()).collect();
    let mut runtime = Runtime::new(RuntimeConfig { cwd: meta.cwd.clone(), sandbox, writable: meta.writable.clone(), network: meta.network, no_bwrap: flags.no_bwrap, jobs_dir, scrub_env, vision: meta.vision, images_dir }).await;
    // The preamble is rebuilt on every launch (cwd, sandbox, bwrap and the servers may have
    // changed); the caller's part is the flag's, or the stored one.
    let user = user_system.unwrap_or_else(|| agent::user_system(&meta.system).to_string());
    let mut notes = runtime.system_notes();
    if let Some(note) = mcp::system_note(&mcp_servers) {
        notes.push('\n');
        notes.push_str(&note);
    }
    let system = agent::compose_system(&agent::preamble(&notes), &user);
    let (session, pending, resumed) = match opened {
        Opened::Existing(mut s, pending) => {
            s.meta.system = system;
            s.save_meta().map_err(|e| Fail::Runtime(format!("saving session.json: {e}")))?;
            (s, pending == Pending::Turn, true)
        }
        Opened::New(mut m) => {
            m.system = system;
            let dir = Session::dir_for(&home, &m.id);
            (Session::create(&home, m, fsync).map_err(|e| session_error(e, &dir))?, false, false)
        }
    };
    warnings.extend(runtime.start_mcp(mcp_servers).await);
    let api_key = env(&session.meta.api_key_env).filter(|k| !k.trim().is_empty());
    let client = Client::new(ClientConfig {
        base_url: session.meta.provider_url.clone(),
        wire: session.meta.wire,
        api_key,
        max_retries: flags.max_retries.unwrap_or(8),
        timeout: Duration::from_secs(flags.timeout_secs.unwrap_or(600)),
        stream: !flags.no_stream,
        user_agent: format!("bija/{VERSION}"),
    });
    Ok(Launch { session, pending, resumed, client, runtime, external, warnings })
}

// ---------------------------------------------------------------------------------------------
// serve and run

async fn serve(flags: Flags, env: Env<'_>) -> Result<i32, Fail> {
    let l = launch(&flags, env).await?;
    let (tx, mut inbox) = mpsc::unbounded_channel();
    let mut warnings = l.warnings;
    let control = if flags.control {
        match Control::start(&l.session.dir, tx.clone()) {
            Ok(c) => Some(c),
            Err(e) => {
                warnings.push(format!("control socket unavailable: {e}"));
                None
            }
        }
    } else {
        None
    };
    let out = Out::new(Sink::Stdout, control.as_ref().map(Control::fanout));
    spawn_stdin(tx.clone());
    spawn_signals(tx, Op::Shutdown);
    let mut agent = Agent::new(l.session, l.client, l.runtime, l.external, out);
    let code = agent.serve(&mut inbox, Start { resumed: l.resumed, pending: l.pending, once: false, warnings }).await;
    drop(control);
    if code != 0 {
        // The line a parent shows as the reason for the exit (Mantra's hub reads the last one).
        eprintln!("bija: terminated by a signal");
    }
    Ok(code)
}

async fn run_once(flags: Flags, prompt: String, env: Env<'_>) -> Result<i32, Fail> {
    let prompt = if prompt == "-" {
        let mut s = String::new();
        io::stdin().read_to_string(&mut s).map_err(|e| Fail::Runtime(format!("reading the prompt from stdin: {e}")))?;
        s
    } else {
        prompt
    };
    if prompt.trim().is_empty() {
        return Err(Fail::Usage("bija run: the prompt is empty".into()));
    }
    let l = launch(&flags, env).await?;
    let (tx, mut inbox) = mpsc::unbounded_channel();
    // Ctrl-C ends the turn the clean way (transcript repaired, `--resume` consistent) and exits 1.
    spawn_signals(tx, Op::Interrupt);
    let sink = if flags.verbose { Sink::Stderr } else { Sink::Discard };
    let id = l.session.meta.id.clone();
    let mut agent = Agent::new(l.session, l.client, l.runtime, l.external, Out::new(sink, None));
    agent.queue_prompt(prompt);
    let code = agent.serve(&mut inbox, Start { resumed: l.resumed, pending: l.pending, once: true, warnings: l.warnings }).await;
    if code != 0 {
        // A signal (SIGTERM, SIGHUP) left the turn open rather than ending it.
        eprintln!("bija: terminated by a signal; the turn stays open — `bija --resume {id}` continues it");
        return Ok(code);
    }
    match agent.report() {
        Some(r) if r.status == TurnStatus::Completed => {
            let text = r.text.trim_end_matches('\n');
            if !text.is_empty() {
                say(text);
            }
            Ok(0)
        }
        Some(r) => Err(Fail::Runtime(match &r.error {
            Some(e) => format!("turn failed: {e}"),
            None => "turn interrupted".to_string(),
        })),
        None => Err(Fail::Runtime("no turn ran".into())),
    }
}

/// stdin lines into the inbox; EOF becomes `Input::Eof` (shutdown).
fn spawn_stdin(tx: UnboundedSender<Input>) {
    tokio::spawn(async move {
        let mut reader = BufReader::new(stdin_stream());
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf).await {
                Ok(0) => break,
                Ok(_) => {
                    if let Some(input) = Input::from_line(&String::from_utf8_lossy(&buf)) {
                        if tx.send(input).is_err() {
                            return;
                        }
                    }
                }
                Err(e) => {
                    eprintln!("bija: stdin: {e}");
                    break;
                }
            }
        }
        let _ = tx.send(Input::Eof);
    });
}

/// SIGTERM and SIGHUP are a crash-safe stop (`Input::Terminate`): Mantra itself ends bija with
/// the `shutdown` op, so a signal is someone else wanting the process gone, and the turn it was in
/// must survive for `--resume`. SIGINT sends `on_interrupt` (shutdown when serving, an interrupt
/// for `bija run`).
fn spawn_signals(tx: UnboundedSender<Input>, on_interrupt: Op) {
    use tokio::signal::unix::{signal, SignalKind};
    let (Ok(mut term), Ok(mut hup), Ok(mut int)) = (signal(SignalKind::terminate()), signal(SignalKind::hangup()), signal(SignalKind::interrupt())) else {
        return eprintln!("bija: signal handlers unavailable");
    };
    tokio::spawn(async move {
        loop {
            let input = tokio::select! {
                _ = term.recv() => Input::Terminate(libc::SIGTERM),
                _ = hup.recv() => Input::Terminate(libc::SIGHUP),
                _ = int.recv() => Input::Op(on_interrupt.clone()),
            };
            if tx.send(input).is_err() {
                return;
            }
        }
    });
}

fn stdin_stream() -> Box<dyn AsyncRead + Unpin + Send> {
    match PipeStdin::new() {
        Some(p) => Box::new(p),
        None => Box::new(tokio::io::stdin()),
    }
}

/// stdin as a non-blocking pipe parked in the reactor (SPEC §8: an idle bija sits in epoll), where
/// tokio's own stdin would keep a blocking thread per process — 80 of them in a swarm. Only for a
/// pipe, whose read end is ours alone: O_NONBLOCK on a terminal or a socket would leak into the
/// descriptor stdout may share.
struct PipeStdin(AsyncFd<StdinFd>);

struct StdinFd;

impl AsRawFd for StdinFd {
    fn as_raw_fd(&self) -> RawFd {
        libc::STDIN_FILENO
    }
}

impl PipeStdin {
    fn new() -> Option<PipeStdin> {
        // SAFETY: fstat fills a zeroed stat for fd 0; fcntl only reads and sets fd 0's status flags.
        unsafe {
            let mut st: libc::stat = std::mem::zeroed();
            if libc::fstat(libc::STDIN_FILENO, &mut st) != 0 || (st.st_mode & libc::S_IFMT) != libc::S_IFIFO {
                return None;
            }
            let flags = libc::fcntl(libc::STDIN_FILENO, libc::F_GETFL);
            if flags < 0 || libc::fcntl(libc::STDIN_FILENO, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
                return None;
            }
        }
        AsyncFd::with_interest(StdinFd, Interest::READABLE).ok().map(PipeStdin)
    }
}

impl AsyncRead for PipeStdin {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        loop {
            let mut guard = ready!(self.0.poll_read_ready(cx))?;
            let unfilled = buf.initialize_unfilled();
            let res = guard.try_io(|fd| {
                // SAFETY: reads at most `unfilled.len()` bytes into memory we own and initialized.
                let n = unsafe { libc::read(fd.as_raw_fd(), unfilled.as_mut_ptr().cast(), unfilled.len()) };
                if n < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(n as usize)
                }
            });
            match res {
                Ok(Ok(n)) => {
                    buf.advance(n);
                    return Poll::Ready(Ok(()));
                }
                Ok(Err(e)) if e.kind() == io::ErrorKind::Interrupted => continue,
                Ok(Err(e)) => return Poll::Ready(Err(e)),
                // Spurious readiness: try_io cleared it, wait again.
                Err(_) => continue,
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// sessions, show, send

/// Prints a line to stdout; false when stdout is gone (`| head`), which ends a listing quietly
/// instead of panicking like `println!`.
fn say(line: &str) -> bool {
    let mut out = io::stdout().lock();
    writeln!(out, "{line}").and_then(|_| out.flush()).is_ok()
}

fn sessions(home: Option<PathBuf>, env: Env) -> Result<i32, Fail> {
    let home = home_dir(home, env)?;
    let list = Session::list(&home);
    if list.is_empty() {
        say(&format!("no sessions in {}", home.display()));
        return Ok(0);
    }
    let now = now_unix();
    say(&format!("{:<36}  {:>5}  {:>8}  {:<24}  NAME / CWD", "ID", "TURNS", "ACTIVE", "MODEL"));
    for m in list {
        let label = if m.name.is_empty() { m.cwd.display().to_string() } else { format!("{}  ({})", m.name, m.cwd.display()) };
        if !say(&format!("{:<36}  {:>5}  {:>8}  {:<24}  {label}", m.id, m.turns, ago(now, m.updated_unix), clip(&m.model, 23))) {
            break;
        }
    }
    Ok(0)
}

fn ago(now: u64, then: u64) -> String {
    match now.saturating_sub(then) {
        s @ 0..=59 => format!("{s}s ago"),
        s @ 60..=3599 => format!("{}m ago", s / 60),
        s @ 3600..=86_399 => format!("{}h ago", s / 3600),
        s => format!("{}d ago", s / 86_400),
    }
}

/// At most `max` lines of `text`, continuation lines indented, the rest counted.
fn clip_lines(text: &str, max: usize) -> String {
    let lines: Vec<&str> = text.trim_end().lines().collect();
    let mut s = lines.iter().take(max).copied().collect::<Vec<_>>().join("\n    ");
    if lines.len() > max {
        s.push_str(&format!("\n    … ({} more lines)", lines.len() - max));
    }
    s
}

/// One transcript record as `bija show` prints it.
fn render(rec: &Record) -> String {
    match rec {
        Record::Msg { message: Message::User { text }, .. } => format!("\n> {}", clip_lines(text, 40)),
        Record::Msg { message: Message::Assistant { text, reasoning, tool_calls }, .. } => {
            let mut parts = Vec::new();
            if !reasoning.trim().is_empty() {
                parts.push(format!("  (thinking) {}", clip_lines(reasoning, 3)));
            }
            if !text.trim().is_empty() {
                parts.push(text.trim_end().to_string());
            }
            for c in tool_calls {
                let args = if c.args.is_null() { c.raw.clone() } else { c.args.to_string() };
                parts.push(format!("  → {} {}", c.name, clip(&args, 200)));
            }
            parts.join("\n")
        }
        Record::Msg { message: Message::Tool { name, text, is_error, .. }, .. } => format!("  ← {name}{}: {}", if *is_error { " (error)" } else { "" }, clip_lines(text, 8)),
        Record::Note { text, .. } => format!("  [note] {}", clip_lines(text, 8)),
        Record::Compact { summary, from, to, .. } => format!("  [compacted: {from} → {to} tokens]\n    {}", clip_lines(summary, 40)),
        Record::Turn { n, status, .. } => format!("  [turn {n} {status}]"),
    }
}

fn render_line(line: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(line);
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    Some(match Record::parse(text) {
        Some(rec) => render(&rec),
        None => "  [unreadable line]".to_string(),
    })
}

async fn show(id: &str, home: Option<PathBuf>, follow: bool, env: Env<'_>) -> Result<i32, Fail> {
    let home = home_dir(home, env)?;
    if !session::valid_id(id) {
        return Err(Fail::Usage(format!("invalid session id {id:?}")));
    }
    let dir = Session::dir_for(&home, id);
    let meta: Meta = std::fs::read_to_string(dir.join("session.json"))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .ok_or_else(|| Fail::Runtime(format!("no session {id} in {}", home.display())))?;
    let name = if meta.name.is_empty() { String::new() } else { format!(" ({})", meta.name) };
    say(&format!("session {}{name} — {} via {}, {} turns, cwd {}", meta.id, meta.model, meta.provider_url, meta.turns, meta.cwd.display()));
    let path = session::transcript_path(&dir);
    let mut offset = 0u64;
    let mut partial: Vec<u8> = Vec::new();
    loop {
        let fresh = read_from(&path, &mut offset).map_err(|e| Fail::Runtime(format!("{}: {e}", path.display())))?;
        partial.extend_from_slice(&fresh);
        while let Some(pos) = partial.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = partial.drain(..=pos).collect();
            if let Some(text) = render_line(&line) {
                if !say(&text) {
                    return Ok(0);
                }
            }
        }
        if !follow {
            if let Some(text) = render_line(&partial) {
                say(&text);
            }
            return Ok(0);
        }
        // A viewer, not the agent: a plain poll is the simplest tail there is.
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// The bytes appended since `offset` (advanced). A file that shrank (a torn line repaired on
/// resume) is followed from its new end.
fn read_from(path: &Path, offset: &mut u64) -> io::Result<Vec<u8>> {
    let mut f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let len = f.metadata()?.len();
    if len < *offset {
        *offset = len;
    }
    f.seek(SeekFrom::Start(*offset))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    *offset += buf.len() as u64;
    Ok(buf)
}

async fn send(id: &str, home: Option<PathBuf>, what: SendWhat, env: Env<'_>) -> Result<i32, Fail> {
    let home = home_dir(home, env)?;
    if !session::valid_id(id) {
        return Err(Fail::Usage(format!("invalid session id {id:?}")));
    }
    let path = control::socket_path(&Session::dir_for(&home, id));
    let stream = UnixStream::connect(&path).await.map_err(|e| Fail::Runtime(format!("cannot reach session {id} ({e}); is it running with --control?")))?;
    let (r, mut w) = stream.into_split();
    let op = match &what {
        SendWhat::Prompt(text) => Op::Prompt { text: text.clone() },
        SendWhat::Op(op) => op.clone(),
    };
    let mut line = serde_json::to_string(&op).map_err(Fail::runtime)?;
    line.push('\n');
    w.write_all(line.as_bytes()).await.map_err(|e| Fail::Runtime(format!("sending to session {id}: {e}")))?;
    let mut lines = BufReader::new(r).lines();
    match what {
        SendWhat::Op(Op::Shutdown) => Ok(0),
        // Ops like `set` or `steer` answer with nothing: don't wait for an event that never comes.
        SendWhat::Op(_) => {
            if let Ok(Ok(Some(l))) = tokio::time::timeout(Duration::from_secs(3), lines.next_line()).await {
                say(&l);
            }
            Ok(0)
        }
        SendWhat::Prompt(_) => {
            // The prompt may wait behind a running turn: ours is the first turn that starts now.
            let mut ours: Option<String> = None;
            loop {
                let l = match lines.next_line().await {
                    Ok(Some(l)) => l,
                    Ok(None) | Err(_) => return Err(Fail::Runtime(format!("session {id} closed the connection"))),
                };
                if !say(&l) {
                    return Ok(0);
                }
                match serde_json::from_str::<Event>(&l) {
                    Ok(Event::TurnStarted { turn }) if ours.is_none() => ours = Some(turn),
                    Ok(Event::TurnDone { turn, status, .. }) if ours.as_deref() == Some(turn.as_str()) => return Ok(if status == TurnStatus::Completed { 0 } else { 1 }),
                    Ok(Event::Error { message }) if ours.is_none() => return Err(Fail::Runtime(format!("session {id}: {message}"))),
                    Ok(Event::Exit { .. }) => return Err(Fail::Runtime(format!("session {id} exited"))),
                    _ => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ToolCall;
    use serde_json::json;
    use std::collections::HashMap;

    fn args(s: &[&str]) -> Vec<String> {
        s.iter().map(|a| a.to_string()).collect()
    }

    fn parse_ok(s: &[&str]) -> Command {
        parse(&args(s)).unwrap_or_else(|e| panic!("{s:?}: {e}"))
    }

    fn parse_err(s: &[&str]) -> String {
        parse(&args(s)).err().unwrap_or_else(|| panic!("{s:?} should fail"))
    }

    fn serve_flags(s: &[&str]) -> Flags {
        match parse_ok(s) {
            Command::Serve(f) => f,
            other => panic!("{other:?}"),
        }
    }

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |k: &str| map.get(k).cloned()
    }

    #[test]
    fn no_arguments_serve_with_defaults() {
        assert_eq!(parse_ok(&[]), Command::Serve(Flags::default()));
    }

    #[test]
    fn serve_flags_parse_with_values_in_both_spellings() {
        let f = serve_flags(&[
            "--home", "/h", "--session=abc", "--cwd", "/w", "--provider-url", "http://127.0.0.1:1/v1", "--wire", "anthropic", "--api-key-env", "MY_KEY", "--model", "m", "--effort", "high", "--context-window", "1000", "--compact-at", "50",
            "--max-output-tokens", "99", "--system", "be brief", "--tools-file", "t.json", "--sandbox", "read-only", "--writable", "/a", "--writable=/b", "--network", "--no-bwrap", "--max-retries", "3", "--timeout-secs", "60", "--no-stream",
            "--name", "worker 1", "--control", "--fsync", "always",
        ]);
        assert_eq!(f.home, Some(PathBuf::from("/h")));
        assert_eq!(f.session.as_deref(), Some("abc"));
        assert_eq!(f.wire, Some(Wire::Anthropic));
        assert_eq!((f.context_window, f.compact_at, f.max_output_tokens), (Some(1000), Some(50), Some(99)));
        assert_eq!(f.system, Some(SystemSource::Text("be brief".into())));
        assert_eq!(f.sandbox, Some(Sandbox::ReadOnly));
        assert_eq!(f.writable, vec![PathBuf::from("/a"), PathBuf::from("/b")]);
        assert!(f.network && f.no_bwrap && f.no_stream && f.control);
        assert_eq!((f.max_retries, f.timeout_secs, f.fsync), (Some(3), Some(60), Some(Fsync::Always)));
        assert_eq!(f.name.as_deref(), Some("worker 1"));
        assert!(f.given.contains(&"--writable") && f.given.contains(&"--control"));
    }

    #[test]
    fn bad_values_and_unknown_options_are_usage_errors() {
        assert!(parse_err(&["--model"]).contains("--model needs a value"));
        assert!(parse_err(&["--context-window", "lots"]).contains("not a valid number"));
        assert!(parse_err(&["--compact-at", "101"]).contains("0..=100"));
        assert!(parse_err(&["--wire", "grpc"]).contains("openai-chat or anthropic"));
        assert!(parse_err(&["--sandbox", "maybe"]).contains("read-only"));
        assert!(parse_err(&["--fsync", "sometimes"]).contains("turn, always or never"));
        assert!(parse_err(&["--timeout-secs", "0"]).contains("at least 1"));
        assert!(parse_err(&["--frobnicate"]).contains("unknown option --frobnicate"));
        assert!(parse_err(&["sesions"]).contains("unexpected argument sesions"));
        assert!(parse_err(&["--system", "a", "--system-file", "b"]).contains("one of them"));
        assert!(parse_err(&["--network=yes"]).contains("takes no value"));
        assert!(parse_err(&["-v"]).contains("does not apply to serve mode"));
    }

    #[test]
    fn help_and_version_win_anywhere() {
        assert_eq!(parse_ok(&["--help"]), Command::Help);
        assert_eq!(parse_ok(&["run", "-h"]), Command::Help);
        assert_eq!(parse_ok(&["help"]), Command::Help);
        assert_eq!(parse_ok(&["--model", "x", "-V"]), Command::Version);
        assert!(HELP.contains("bija run") && HELP.contains("--tools-file") && HELP.contains("Exit codes"));
        // it reads whole in an 80-column terminal
        for line in HELP.lines() {
            assert!(line.chars().count() <= 79, "{} columns: {line}", line.chars().count());
        }
    }

    /// Every option the parser takes is in the help, with its short form where it has one.
    #[test]
    fn the_help_lists_every_option() {
        for option in SESSION_OPTIONS.iter().chain(FLAGS_WITHOUT_VALUE).chain(&["--json"]) {
            assert!(HELP.contains(option), "{option} is missing from --help");
        }
        for short in ["-C, --cwd", "-v, --verbose", "-f, --follow", "-V, --version", "-h, --help"] {
            assert!(HELP.contains(short), "{short} is missing from --help");
        }
        for (short, long) in [("-C", "--cwd"), ("-v", "--verbose"), ("-f", "--follow")] {
            let (a, b) = (parse_flags(&args(&[short, "x"])).map(|(f, _)| f.given), parse_flags(&args(&[long, "x"])).map(|(f, _)| f.given));
            assert_eq!(a, b, "{short} and {long} parse alike");
        }
    }

    #[test]
    fn run_takes_the_prompt_from_its_words() {
        match parse_ok(&["run", "--model", "m", "fix", "the", "bug", "-v"]) {
            Command::Run { flags, prompt } => {
                assert_eq!(prompt, "fix the bug");
                assert!(flags.verbose);
                assert_eq!(flags.model.as_deref(), Some("m"));
            }
            other => panic!("{other:?}"),
        }
        match parse_ok(&["run", "--", "--not-a-flag"]) {
            Command::Run { prompt, .. } => assert_eq!(prompt, "--not-a-flag"),
            other => panic!("{other:?}"),
        }
        assert!(parse_err(&["run", "--model", "m"]).contains("prompt is required"));
        assert!(parse_err(&["run", "--control", "hi"]).contains("--control does not apply to bija run"));
    }

    #[test]
    fn the_inspection_subcommands_parse() {
        assert_eq!(parse_ok(&["sessions", "--home", "/h"]), Command::Sessions { home: Some("/h".into()) });
        assert!(parse_err(&["sessions", "--model", "m"]).contains("does not apply"));
        assert_eq!(parse_ok(&["show", "abc", "-f"]), Command::Show { id: "abc".into(), home: None, follow: true });
        assert!(parse_err(&["show"]).starts_with("usage: bija show"));
        assert_eq!(parse_ok(&["send", "abc", "do", "it"]), Command::Send { id: "abc".into(), home: None, what: SendWhat::Prompt("do it".into()) });
        assert_eq!(parse_ok(&["send", "abc", "--json", r#"{"op":"status"}"#]), Command::Send { id: "abc".into(), home: None, what: SendWhat::Op(Op::Status) });
        assert!(parse_err(&["send", "abc", "--json", "{nope"]).starts_with("--json: bad op"));
        assert!(parse_err(&["send", "abc"]).starts_with("usage: bija send"));
        assert!(parse_err(&["send", "abc", "text", "--json", r#"{"op":"status"}"#]).starts_with("usage: bija send"));
    }

    #[test]
    fn home_falls_back_to_the_environment() {
        assert_eq!(home_dir(Some("/flag".into()), &env_of(&[("BIJA_HOME", "/env")])).unwrap(), PathBuf::from("/flag"));
        assert_eq!(home_dir(None, &env_of(&[("BIJA_HOME", "/env"), ("HOME", "/root")])).unwrap(), PathBuf::from("/env"));
        assert_eq!(home_dir(None, &env_of(&[("BIJA_HOME", " "), ("HOME", "/root")])).unwrap(), PathBuf::from("/root/.bija"));
        assert!(matches!(home_dir(None, &env_of(&[])), Err(Fail::Usage(_))));
        assert!(home_dir(Some("rel".into()), &env_of(&[])).unwrap().is_absolute());
    }

    #[test]
    fn a_new_session_takes_url_and_model_from_flags_then_env() {
        let cwd = std::env::temp_dir();
        let mut flags = Flags { cwd: Some(cwd.clone()), ..Flags::default() };
        let env = env_of(&[("BIJA_PROVIDER_URL", "http://env/v1/"), ("BIJA_MODEL", "env-model")]);
        let m = new_meta(&flags, &env, "s1".into()).unwrap();
        assert_eq!((m.provider_url.as_str(), m.model.as_str()), ("http://env/v1", "env-model"), "trailing slash trimmed");
        assert_eq!((m.api_key_env.as_str(), m.context_window, m.compact_at, m.sandbox.as_str()), ("BIJA_API_KEY", 200_000, 85, "workspace-write"));
        assert_eq!(m.cwd, cwd.canonicalize().unwrap());
        flags.model = Some("flag-model".into());
        flags.effort = Some("None".into());
        let m = new_meta(&flags, &env, "s1".into()).unwrap();
        assert_eq!((m.model.as_str(), m.effort.as_str()), ("flag-model", ""));
        match new_meta(&Flags { cwd: Some(cwd.clone()), ..Flags::default() }, &env_of(&[]), "s".into()) {
            Err(Fail::Usage(m)) => assert!(m.contains("--provider-url is required"), "{m}"),
            other => panic!("{other:?}"),
        }
        let missing = Flags { cwd: Some(cwd.join("does-not-exist-bija")), ..Flags::default() };
        assert!(matches!(new_meta(&missing, &env, "s".into()), Err(Fail::Runtime(_))));
    }

    #[test]
    fn resume_overrides_only_what_was_given() {
        let mut meta = Meta { model: "old".into(), effort: "high".into(), provider_url: "http://a/v1".into(), writable: vec!["/keep".into()], network: true, name: "n".into(), ..Meta::default() };
        let before = meta.clone();
        apply_overrides(&mut meta, &Flags::default()).unwrap();
        assert_eq!(meta, before);
        // --vision is per launch: a session that had it, resumed without it (a switch to a model
        // that does not take images), has no vision tools; with it, it has them again
        meta.vision = true;
        apply_overrides(&mut meta, &Flags::default()).unwrap();
        assert!(!meta.vision, "left out: off");
        apply_overrides(&mut meta, &serve_flags(&["--vision"])).unwrap();
        assert!(meta.vision);
        let flags = serve_flags(&["--model", "new", "--effort", "low", "--compact-at", "70", "--sandbox", "full"]);
        apply_overrides(&mut meta, &flags).unwrap();
        assert_eq!((meta.model.as_str(), meta.effort.as_str(), meta.compact_at, meta.sandbox.as_str()), ("new", "low", 70, "full"));
        assert_eq!((meta.writable.clone(), meta.network, meta.provider_url.as_str()), (vec![PathBuf::from("/keep")], true, "http://a/v1"));
    }

    #[test]
    fn transcript_records_render_for_humans() {
        let user = Record::Msg { message: Message::User { text: "fix it".into() }, at: 0 };
        assert_eq!(render(&user), "\n> fix it");
        let call = ToolCall { id: "c".into(), name: "bash".into(), args: json!({"command": "ls"}), raw: String::new() };
        let a = Record::Msg { message: Message::Assistant { text: "Looking.".into(), reasoning: String::new(), tool_calls: vec![call] }, at: 0 };
        assert_eq!(render(&a), "Looking.\n  → bash {\"command\":\"ls\"}");
        let long: String = (1..=12).map(|i| format!("line {i}\n")).collect();
        let t = Record::Msg { message: Message::Tool { images: vec![], call_id: "c".into(), name: "bash".into(), text: long, is_error: true }, at: 0 };
        let r = render(&t);
        assert!(r.starts_with("  ← bash (error): line 1\n    line 2") && r.ends_with("… (4 more lines)"), "{r}");
        assert_eq!(render(&Record::Turn { n: 2, status: "completed".into(), at: 0 }), "  [turn 2 completed]");
        assert_eq!(render_line(b"garbage").as_deref(), Some("  [unreadable line]"));
        assert_eq!(render_line(b"  \n"), None);
    }

    #[test]
    fn small_formatting_helpers() {
        assert_eq!(ago(100, 95), "5s ago");
        assert_eq!(ago(10_000, 10_000 - 7200), "2h ago");
        assert_eq!(ago(0, 50), "0s ago", "a clock that went backwards is not a panic");
    }

    #[test]
    fn read_from_follows_appends_and_survives_truncation() {
        let p = std::env::temp_dir().join(format!("bija-cli-tail-{}", uuid_v4()));
        let mut offset = 0;
        assert!(read_from(&p, &mut offset).unwrap().is_empty(), "a missing file is just empty");
        std::fs::write(&p, "abc\n").unwrap();
        assert_eq!(read_from(&p, &mut offset).unwrap(), b"abc\n");
        std::fs::write(&p, "ab").unwrap();
        assert!(read_from(&p, &mut offset).unwrap().is_empty());
        assert_eq!(offset, 2);
        let _ = std::fs::remove_file(&p);
    }
}
