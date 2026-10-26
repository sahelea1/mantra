//! Mantra — a terminal UI for running and orchestrating coding agents.

mod agent;
mod app;
mod config;
mod discover;
mod engine;
mod hub;
mod mcp;
mod mcp_bridge;
mod mock;
mod mock_claude;
mod mock_mcp;
mod mock_openai;
mod provider_key;
mod rpc;
mod settings_ops;
mod ui;
mod util;
mod web;

use anyhow::Result;
use serde_json::json;
use app::{App, AppEvent};
use ratatui::backend::{Backend, CrosstermBackend, TestBackend};
use ratatui::crossterm::event::{self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags};
use ratatui::crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::crossterm::{execute, style::Print};
use ratatui::Terminal;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

/// Set while rendering: a panic inside a draw is caught and logged instead of killing the app (and every agent).
static IN_DRAW: AtomicBool = AtomicBool::new(false);

fn safe_draw(f: &mut ratatui::Frame, app: &mut App) {
    IN_DRAW.store(true, Ordering::SeqCst);
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| ui::draw(f, app)));
    IN_DRAW.store(false, Ordering::SeqCst);
    if r.is_err() {
        let area = f.area();
        f.render_widget(ratatui::widgets::Clear, area);
        f.render_widget(ratatui::widgets::Paragraph::new("mantra: a rendering error was caught and logged (agents keep running). Resize or press ctrl+l; please report the log."), area);
    }
}
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

const HELP: &str = "mantra — one terminal for coding agents on configured Bija, Codex or Claude Code models

  Solo and every built-in pattern start on `sol` and its siblings through the OpenAI API (the key
  in $OPENAI_API_KEY, or pasted in /models). /models adds any other provider from just its API key
  — Mantra finds the API and lists its models — a local server with no key at all, and Codex or
  Claude Code as extra backends.

USAGE
  mantra [--cwd DIR] [--demo]           open the TUI (Solo mode)
  mantra run \"<goal>\" [--pattern NAME]  open straight into a Mandala run
             [--parallel N]             … with up to N workers at once, this run only (1-128)
  mantra runs                            list every run (all projects): id, stage, when, goal
  mantra runs resume <id> [--take-over]  reopen a run where it stopped (id prefix is enough);
                                         refused while another Mantra has it open, unless --take-over
  mantra runs delete <id> [--yes]        remove a run: its branches, worktrees and journal
  mantra doctor                          check bija, the providers, the optional backends, the terminal
  mantra experimental                    list experimental features, on or off
  mantra experimental enable|disable <name>   switch one in settings.toml (akasha: shared notes
                                         for agent groups)
  mantra mcp                             list the MCP servers agents can be given (settings.toml)
  mantra mcp add <name> [--env K=V]… -- <command> [args…]
                                         add one, e.g. mantra mcp add blender -- uvx blender-mcp
                                         (Solo gets every enabled one; a role with mcp = [\"name\"])
  mantra mcp remove|enable|disable <name>
  mantra mcp test <name> [--timeout S]   start it, list its tools (exit 1 if it does not answer
                                         within S seconds, 20 by default)
  mantra --version

  mantra --web [ADDR:PORT] [--web-password PW] [--web-tls]   web UI (phone + desktop) on this machine/LAN
  mantra --remote [URL]                                     reach this session from anywhere via a relay (E2EE)
  mantra --headless --web ... / --remote                     the same without a terminal

OPTIONS
  --cwd DIR       project directory (default: current directory)
  --demo          simulated agents (no API calls, no cost) in a throwaway demo repo
  --pattern NAME  pattern for new runs (default from settings.toml)
  --parallel N    with a goal: up to N workers at once for that run, whatever the pattern says
                  (1-128; above 32 the provider's rate limits are the ceiling)
  --resume-last   reopen this project's most recent unfinished run (cwd / --cwd; with --demo:
                  the last demo run) — other projects' runs only by id, with `runs resume`
  --no-sandbox-check  start `mantra run` without asking when the command sandbox can't work here
  --web-listen ADDR:PORT   where the web UI listens (default 127.0.0.1:7777; 8080 = 127.0.0.1:8080)
  --web-password PW        web UI password — needed off localhost and with --headless;
                           MANTRA_WEB_PASSWORD is better (flags show up in `ps`). Also derives
                           the --remote link's key. Without one, localhost is trusted: a
                           port-forward or tunnel to the port (ssh -L, docker -p) lets anyone in
  --web-tls                HTTPS with Mantra's own certificate (install its CA from /cert.pem)
  --web-cert F --web-key F HTTPS with your own PEM certificate and key
  --remote-site URL        the website remote links open, when it is not on the relay's host
                           (the link then names its relay: …#k=…&r=wss://relay)
                           The relay is dialed through https_proxy (an http:// proxy, CONNECT;
                           no_proxy exempts hosts) and SSL_CERT_FILE adds trusted CA roots
  --headless               no terminal UI (needs --web or --remote, and a password with --web);
                           ctrl+c / SIGTERM stop it

FILES
  ~/.mantra/settings.toml          ui, backend commands, defaults   ($MANTRA_HOME overrides the dir)
  ~/.mantra/models.toml            models, context windows, efforts, providers
  ~/.mantra/bija/sessions/         bija agents' sessions, resumable (bija show <id> --home ~/.mantra/bija)
  ~/.mantra/patterns/              your patterns (the Studio saves here)
  ~/.mantra/runs/<project>/<id>/   per-run plan, journal and outputs
  ~/.mantra/web/                   TLS cert, push keys/subscriptions, sessions, remote identity (0600)
  Nothing is written into your projects. An old ~/.config/mantra is copied over on first start.
";

struct Cli {
    cwd: Option<PathBuf>,
    demo: bool,
    run_goal: Option<String>,
    pattern: Option<String>,
    snapshot: Option<String>,
    size: (u16, u16),
    /// `mantra runs resume <id>`.
    resume: Option<String>,
    resume_last: bool,
    /// `mantra runs resume <id> --take-over`: resume although `owner.json` names a live process.
    take_over: bool,
    no_sandbox_check: bool,
    web: web::CliWeb,
    /// `--parallel N`: the width of every worker class for the run started from the command line.
    parallel: Option<usize>,
}

fn parse_args() -> Result<Option<Cli>> {
    parse_cli(&std::env::args().skip(1).collect::<Vec<String>>())
}

fn parse_cli(args: &[String]) -> Result<Option<Cli>> {
    let mut cli = Cli { cwd: None, demo: false, run_goal: None, pattern: None, snapshot: None, size: (120, 36), resume: None, resume_last: false, take_over: false, no_sandbox_check: false, web: web::CliWeb::default(), parallel: None };
    // `run` takes the goal from the first bare word, wherever it stands:
    // `mantra run --parallel 8 "goal"` and `mantra run "goal" --parallel 8` are the same run.
    let (mut run, mut bare) = (false, None::<String>);
    let mut i = 0;
    while i < args.len() {
        let a = args[i].clone();
        // Once `run` is in, a word that is not an option is the goal, whatever it says —
        // `mantra run doctor` is a run about a doctor, `mantra run "--dry-run flag for deploy"`
        // one about a flag — as `run <goal>` took any goal before options could sit between the two.
        let command = !run;
        let mut next = || -> Result<String> {
            i += 1;
            args.get(i).cloned().ok_or_else(|| anyhow::anyhow!("{a} needs a value"))
        };
        match a.as_str() {
            "-h" | "--help" => {
                print!("{HELP}");
                return Ok(None);
            }
            "help" if command => {
                print!("{HELP}");
                return Ok(None);
            }
            "-V" | "--version" => {
                println!("mantra {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            "--cwd" | "-C" => cli.cwd = Some(PathBuf::from(next()?)),
            "--demo" => cli.demo = true,
            "--pattern" => cli.pattern = Some(next()?),
            "--snapshot" => cli.snapshot = Some(next()?),
            "--size" => {
                let s = next()?;
                let (w, h) = s.split_once('x').ok_or_else(|| anyhow::anyhow!("--size WxH"))?;
                cli.size = (w.parse()?, h.parse()?);
            }
            "run" => run = true,
            "--parallel" => cli.parallel = Some(engine::pattern::parse_parallel(&next()?).map_err(|e| anyhow::anyhow!("--parallel: {e}"))?),
            // gone in 0.8; said for one release, so a script that used it learns what replaced it
            "--swarm" => anyhow::bail!("--swarm is gone: every pattern has a worker-swarm class now, which the planner uses where it fits — mantra run \"<goal>\" --parallel N sets how many run at once"),
            "experimental" if command => {
                match experimental_cmd(&args[i + 1..]) {
                    Ok(out) => print!("{out}"),
                    Err((code, e)) => {
                        eprintln!("mantra: {e}");
                        std::process::exit(code);
                    }
                }
                return Ok(None);
            }
            "mcp" if command => {
                match mcp_cmd(&args[i + 1..]) {
                    Ok(out) => print!("{out}"),
                    Err((code, e)) => {
                        eprintln!("mantra: {e}");
                        std::process::exit(code);
                    }
                }
                return Ok(None);
            }
            "--resume-last" => cli.resume_last = true,
            "--take-over" => cli.take_over = true,
            "--no-sandbox-check" => cli.no_sandbox_check = true,
            "--web" => {
                cli.web.web = true;
                // Optional value: only taken when it reads as an address (`--web run "…"` stays a run).
                if let Some(v) = args.get(i + 1).filter(|v| !v.starts_with('-') && web::parse_listen(v).is_some()) {
                    cli.web.listen = Some(v.clone());
                    i += 1;
                }
            }
            "--web-listen" => {
                cli.web.web = true;
                cli.web.listen = Some(next()?);
            }
            "--web-password" => cli.web.password = Some(next()?),
            "--web-tls" => cli.web.tls = true,
            "--web-cert" => cli.web.cert = Some(PathBuf::from(next()?)),
            "--web-key" => cli.web.key = Some(PathBuf::from(next()?)),
            "--remote" => {
                let url = args.get(i + 1).filter(|v| v.starts_with("ws://") || v.starts_with("wss://")).cloned();
                if url.is_some() {
                    i += 1;
                }
                cli.web.remote = Some(url);
            }
            "--remote-site" => cli.web.remote_site = Some(next()?),
            "--headless" => cli.web.headless = true,
            "runs" if command => match next().ok().as_deref() {
                None | Some("list") | Some("ls") => {
                    runs_list();
                    return Ok(None);
                }
                Some("resume") => cli.resume = Some(next()?),
                Some("delete") | Some("rm") => {
                    let id = next()?;
                    let yes = matches!(next().ok().as_deref(), Some("--yes") | Some("-y"));
                    runs_delete(&id, yes);
                    return Ok(None);
                }
                Some(other) => anyhow::bail!("mantra runs: unknown subcommand {other} (list | resume <id> | delete <id>)"),
            },
            "doctor" if command => {
                doctor();
                return Ok(None);
            }
            other if bare.is_none() && (!command || !other.starts_with('-')) => bare = Some(other.to_string()),
            other => anyhow::bail!("unknown argument {other} (see mantra --help)"),
        }
        i += 1;
    }
    match bare {
        Some(goal) if run => cli.run_goal = Some(goal),
        Some(other) => anyhow::bail!("unknown argument {other} (see mantra --help)"),
        None if run => anyhow::bail!("run needs a goal: mantra run \"<goal>\""),
        None => {}
    }
    if cli.parallel.is_some() && cli.run_goal.is_none() {
        anyhow::bail!("--parallel needs a run: mantra run \"<goal>\" --parallel N");
    }
    Ok(Some(cli))
}

/// `mantra experimental [list | enable <name> | disable <name>]`: what goes to stdout, or the exit
/// code and the error — 2 for a usage error or an unknown name, which comes with the list.
fn experimental_cmd(args: &[String]) -> std::result::Result<String, (i32, String)> {
    let list = || {
        let s = config::Settings::load();
        app::experimental_list(&s).iter().map(|l| format!("  {l}\n")).collect::<String>()
    };
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    let (name, on) = match words.as_slice() {
        [] | ["list"] => return Ok(format!("experimental features (mantra experimental enable|disable <name>):\n{}", list())),
        ["enable", name] => (*name, true),
        ["disable", name] => (*name, false),
        _ => return Err((2, "usage: mantra experimental [list | enable <name> | disable <name>]".into())),
    };
    if !app::is_experimental(name) {
        return Err((2, format!("unknown experimental feature '{name}'; there is:\n{}", list().trim_end())));
    }
    app::save_experimental(name, on).map_err(|e| (1, e))?;
    let what = if on { "on — runs started from now on use it" } else { "off" };
    Ok(format!("{name} {what} (saved in {})\n", config::home().join("settings.toml").display()))
}

const MCP_USAGE: &str = "usage: mantra mcp [list | add <name> [--env KEY=VALUE]… -- <command> [args…] | remove <name> | enable <name> | disable <name> | test <name> [--timeout SECONDS]]";

/// How long `mantra mcp test` gives a server to start, shake hands and list its tools, unless
/// `--timeout` says otherwise (a first `uvx` run downloads the server before it can answer).
const MCP_TEST_BUDGET: Duration = Duration::from_secs(20);

/// `mantra mcp …`: the `[mcp_servers]` of settings.toml as it is on disk (`Settings::load_on_disk`
/// — never the session's own). Stdout, or the exit code and the error: 2 for a usage error or an
/// unknown name, 1 for a server that fails its test or a file that cannot be read or saved.
fn mcp_cmd(args: &[String]) -> std::result::Result<String, (i32, String)> {
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    let load = || config::Settings::load_on_disk().map_err(|e| (1, e));
    let save = |s: &config::Settings| s.save().map_err(|e| (1, format!("could not save {}: {e}", config::home().join("settings.toml").display())));
    // The same rules the app's settings screen goes by (`settings_ops`); a refusal is a usage error.
    let usage = |e: String| (2, e);
    match words.as_slice() {
        [] | ["list"] => Ok(mcp_list(&load()?)),
        ["add", name, rest @ ..] => {
            if let Some(why) = config::mcp_name_problem(name) {
                return Err((2, why));
            }
            let mut server = config::McpServer::default();
            let mut rest = rest;
            loop {
                match rest {
                    ["--env", kv, tail @ ..] => {
                        let (k, v) = kv.split_once('=').filter(|(k, _)| !k.is_empty()).ok_or_else(|| (2, format!("--env takes KEY=VALUE, not {kv}")))?;
                        server.env.insert(k.to_string(), v.to_string());
                        rest = tail;
                    }
                    ["--", tail @ ..] => {
                        rest = tail;
                        break;
                    }
                    _ => break,
                }
            }
            // A dangling `--env` (or a flag before `--`) is not the server's command.
            let [command, cmd_args @ ..] = rest else { return Err((2, MCP_USAGE.into())) };
            if command.starts_with("--") {
                return Err((2, format!("{command} is not a command — the server's command line goes after --\n{MCP_USAGE}")));
            }
            server.command = command.to_string();
            server.args = cmd_args.iter().map(|a| a.to_string()).collect();
            let mut s = load()?;
            if let Some(old) = s.mcp_servers.get(*name) {
                return Err((2, format!("{name} is already configured ({}) — mantra mcp remove {name} first", shown_command(old))));
            }
            let shown = shown_command(&server);
            let secret = !server.env.is_empty();
            s.mcp_add(name, server).map_err(usage)?;
            save(&s)?;
            let solo = if mcp_solo_gets(&s, name) { "Solo agents get it" } else { "Solo does not (/settings in the app)" };
            let env = if secret { "\nIts env makes settings.toml readable by you only (0600); the values still reach the command line of Codex and Claude Code agents given the server." } else { "" };
            Ok(format!("added {name}: {shown}\n{solo}; a pattern role gets it with mcp = [\"{name}\"]. Check it with `mantra mcp test {name}`.{env}\n"))
        }
        ["remove" | "rm", name] => {
            let mut s = load()?;
            s.mcp_remove(name).map_err(usage)?;
            save(&s)?;
            Ok(format!("removed {name}\n"))
        }
        [verb @ ("enable" | "disable"), name] => {
            let mut s = load()?;
            let on = *verb == "enable";
            s.mcp_set_enabled(name, on).map_err(usage)?;
            save(&s)?;
            // An open Mantra reads the change at its next Solo (`/new`) or run, not mid-run.
            Ok(format!("{name} {}\n", if on { "enabled — Solo agents and runs started from now on get it" } else { "disabled — kept in settings.toml; Solo agents and runs started from now on go without it" }))
        }
        ["test", name, rest @ ..] => {
            let budget = match rest {
                [] => MCP_TEST_BUDGET,
                ["--timeout", secs] => match secs.parse::<u64>() {
                    Ok(n) if (1..=3600).contains(&n) => Duration::from_secs(n),
                    _ => return Err((2, format!("--timeout takes whole seconds, 1 to 3600, not {secs}"))),
                },
                _ => return Err((2, MCP_USAGE.into())),
            };
            let s = load()?;
            s.mcp_known(name).map_err(usage)?;
            let server = s.mcp_servers[*name].clone();
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| (1, format!("no runtime: {e}")))?;
            let report = rt.block_on(mcp_test(name, &server, budget)).map_err(|e| (1, e))?;
            Ok(if server.enabled { report } else { format!("{report}({name} is disabled: `mantra mcp enable {name}` gives it to agents)\n") })
        }
        _ => Err((2, MCP_USAGE.into())),
    }
}

/// Whether a Solo agent started now gets `name` (`solo_mcp` names it or says `*`, and it is on).
fn mcp_solo_gets(s: &config::Settings, name: &str) -> bool {
    s.mcp_for(&s.solo_mcp).iter().any(|(n, _)| n == name)
}

/// The command line a server runs, an argument with a space or a quote in it single-quoted.
fn shown_command(s: &config::McpServer) -> String {
    let shown = |a: &str| if a.is_empty() || a.contains([' ', '\t', '"', '\'']) { format!("'{}'", a.replace('\'', "'\\''")) } else { a.to_string() };
    std::iter::once(s.command.as_str()).chain(s.args.iter().map(String::as_str)).map(shown).collect::<Vec<_>>().join(" ")
}

fn mcp_list(s: &config::Settings) -> String {
    if s.mcp_servers.is_empty() {
        return "no MCP servers configured — mantra mcp add <name> -- <command> [args…]\n  e.g. Blender's add-on: mantra mcp add blender -- uvx blender-mcp\n".into();
    }
    let width = s.mcp_servers.keys().map(|k| k.chars().count()).max().unwrap_or(0);
    let mut out = format!("MCP servers ({}):\n", config::home().join("settings.toml").display());
    for (name, server) in &s.mcp_servers {
        let state = if server.enabled { "enabled " } else { "disabled" };
        let solo = if mcp_solo_gets(s, name) { "yes" } else { "no" };
        out.push_str(&format!("  {name:<width$}  {state}  solo: {solo:<3}  {}\n", shown_command(server)));
        // written by hand into settings.toml: no agent gets it until it is renamed
        if let Some(why) = config::mcp_name_problem(name) {
            out.push_str(&format!("  {:<width$}  ↳ never given to an agent: {why}\n", ""));
        }
    }
    out.push_str("A pattern role gets servers with mcp = [\"<name>\"] (or [\"*\"]); Solo with solo_mcp (/settings in the app).\n");
    out
}

/// Starts the server, shakes hands and lists its tools, all within `budget`: the report, or why
/// not — with the last lines the server wrote on stderr, which usually say it.
async fn mcp_test(name: &str, server: &config::McpServer, budget: Duration) -> std::result::Result<String, String> {
    let mut c = mcp::Client::launch(server).map_err(|e| format!("{name}: {e}"))?;
    let outcome = tokio::time::timeout(budget, async {
        c.initialize().await?;
        c.list_tools().await
    })
    .await;
    let why = match outcome {
        Ok(Ok(tools)) => return Ok(mcp_test_report(name, server, &c, &tools)),
        Ok(Err(e)) => e,
        Err(_) => format!("no answer within {} s", budget.as_secs()),
    };
    let tail = c.last_words().await;
    Err(if tail.is_empty() { format!("{name}: {why}") } else { format!("{name}: {why}\n  its stderr:\n    {}", tail.replace('\n', "\n    ")) })
}

/// What `mantra mcp test` prints for a server that answered: who it is, and each tool's first line.
/// A tool's description as one line for `mantra mcp test`: its first sentence, from a first
/// paragraph that many servers hard-wrap ("…whether the addon matches this server, and which" was
/// a line of blender-mcp's, not what it says).
fn tool_about(description: &str) -> String {
    let para: Vec<&str> = description.lines().map(str::trim).skip_while(|l| l.is_empty()).take_while(|l| !l.is_empty()).collect();
    let para = para.join(" ");
    match para.find(". ") {
        Some(end) => para[..=end].to_string(),
        None => para,
    }
}

/// Room for a tool's line in `mantra mcp test` after its name column: what the terminal leaves
/// (a line that wraps under the names is hard to scan), 100 when stdout is not one, never under 30.
fn about_room(name_width: usize) -> usize {
    use std::io::IsTerminal;
    let cols = if std::io::stdout().is_terminal() { terminal::size().map(|(w, _)| w as usize).unwrap_or(0) } else { 0 };
    if cols == 0 {
        return 100;
    }
    cols.saturating_sub(name_width + 7).clamp(30, 100)
}

fn mcp_test_report(name: &str, server: &config::McpServer, c: &mcp::Client, tools: &[serde_json::Value]) -> String {
    let mut out = format!("{name}: {}\n  connected: {}, protocol {}\n", shown_command(server), if c.server.is_empty() { "a server with no name" } else { &c.server }, c.protocol);
    let tool_name = |t: &serde_json::Value| t.get("name").and_then(|n| n.as_str()).unwrap_or("?").to_string();
    let width = tools.iter().map(|t| tool_name(t).chars().count()).max().unwrap_or(0);
    out.push_str(&format!("  {} tool{}{}\n", tools.len(), if tools.len() == 1 { "" } else { "s" }, if tools.is_empty() { "" } else { ":" }));
    for t in tools {
        let about = tool_about(t.get("description").and_then(|d| d.as_str()).unwrap_or(""));
        out.push_str(format!("    {:<width$}  {}", tool_name(t), util::trunc(&about, about_room(width))).trim_end());
        out.push('\n');
    }
    out
}

fn main() {
    // The mock servers and the MCP bridge (WP10.4/10.6) are separate modes of the same binary.
    match std::env::args().nth(1).as_deref() {
        Some("mock-codex") => {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
            rt.block_on(mock::run());
            return;
        }
        Some("mock-claude") => {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
            rt.block_on(mock_claude::run());
            return;
        }
        Some("mcp-bridge") => {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
            rt.block_on(mcp_bridge::run());
            return;
        }
        Some("mock-mcp") => {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
            rt.block_on(mock_mcp::run());
            return;
        }
        _ => {}
    }
    let cli = match parse_args() {
        Ok(Some(c)) => c,
        Ok(None) => return,
        Err(e) => {
            eprintln!("mantra: {e}");
            std::process::exit(2);
        }
    };
    let env_pw = take_web_password_env();
    util::init_log(&config::log_path());
    mlog!("mantra {} starting", env!("CARGO_PKG_VERSION"));
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().expect("runtime");
    let res = rt.block_on(async_main(cli, env_pw));
    rt.shutdown_timeout(Duration::from_millis(500));
    if let Err(e) = res {
        eprintln!("mantra: {e:#}");
        std::process::exit(1);
    }
}

/// `--demo`: the registry's `bija` provider — added, with the default models through it, when
/// this models.toml has none — points at the in-process mock OpenAI server, keyless. In memory
/// only, like the demo's Codex and Claude Code models: nothing here is saved.
fn point_bija_at_mock(reg: &mut config::Registry, port: u16) {
    let url = format!("http://127.0.0.1:{port}/v1");
    match reg.providers.iter_mut().find(|p| p.id == "bija") {
        Some(p) => {
            p.kind = config::ProviderKind::Bija;
            p.base_url = url;
            p.wire = String::new();
            p.env_key = String::new();
            p.api_key = None;
        }
        None => reg.providers.push(config::ProviderEntry { base_url: url, env_key: String::new(), ..config::bija_default_provider() }),
    }
    for m in config::bija_default_model_entries("bija") {
        if reg.get(&m.alias).is_none() {
            reg.models.push(m);
        }
    }
}

fn demo_project() -> Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!("mantra-demo-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("src"))?;
    std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n")?;
    std::fs::write(dir.join("README.md"), "# demo\nA small demo project for Mantra.\n")?;
    std::fs::write(dir.join("src/lib.rs"), "pub fn greet(name: &str) -> String {\n    format!(\"hello, {name}\")\n}\n")?;
    std::fs::write(dir.join("src/main.rs"), "fn main() {\n    println!(\"{}\", demo::greet(\"world\"));\n}\n")?;
    let git = |args: &[&str]| std::process::Command::new("git").args(args).current_dir(&dir).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status();
    if git(&["init", "-q"]).map(|s| s.success()).unwrap_or(false) {
        let _ = git(&["add", "-A"]);
        let _ = git(&["-c", "user.name=Mantra", "-c", "user.email=mantra@localhost", "commit", "-qm", "initial"]);
    }
    Ok(dir)
}

/// Find a configured Codex alias by its familiar demo name, when the demo has seeded both
/// Bija and Codex variants. Production model IDs come entirely from models.toml.
fn codex_alias_of(reg: &config::Registry, plain: &str) -> Option<String> {
    reg.models.iter().find(|m| !m.is_custom_provider() && (m.alias == plain || m.alias == format!("{plain}-codex"))).map(|m| m.alias.clone())
}

/// Why this session cannot start, or `None`: bija is the harness, so a missing one is fatal when
/// Solo's model runs on it — said plainly here rather than as a demo nobody asked for or a spawn
/// error later. A missing Codex or Claude Code is never said at startup: it matters only to an
/// agent on one of their models (the hub says so then) and to `doctor`.
fn startup_problem(found: config::BijaFound, settings: &config::Settings, reg: &config::Registry) -> Option<String> {
    // An unconfigured install opens `/models` without starting an agent. Do not treat the
    // unresolved default alias as a Bija model and require its binary at this point.
    let on_bija = reg.get(&settings.default_model).is_some_and(|m| reg.backend_of(m) == config::ProviderKind::Bija);
    (found == config::BijaFound::Missing && on_bija).then(|| {
        format!("bija, the harness `{}` runs on, is not installed: `{}` is not on PATH and not beside mantra.\n        Reinstall mantra (bija ships beside it), or point $MANTRA_BIJA at the binary.", settings.default_model, settings.bija_command.join(" "))
    })
}

/// `MANTRA_WEB_PASSWORD`, taken out of the process environment so no child inherits it — not the
/// agents' backends, not their shell commands, not a third-party MCP server. Called from `main`
/// while it is still the only thread (changing the environment is not thread safe).
fn take_web_password_env() -> Option<String> {
    let pw = std::env::var("MANTRA_WEB_PASSWORD").ok();
    std::env::remove_var("MANTRA_WEB_PASSWORD");
    pw.filter(|p| !p.is_empty())
}

async fn async_main(mut cli: Cli, env_pw: Option<String>) -> Result<()> {
    let mut settings = config::Settings::load();
    let mut registry = config::Registry::load();
    // Validate the web flags before anything else starts (a bad address is a usage error).
    let web_cfg = web::WebConfig::resolve(&cli.web, &settings.web, env_pw)?;
    let headless = cli.web.headless;
    if headless {
        // Nobody can answer a question on stdin.
        cli.no_sandbox_check = true;
    }
    // Resolved once and handed to the hub; never written back, so a later `Settings::save` keeps
    // the configured command rather than this machine's path.
    let (mut bija_cmd, bija_found) = config::resolve_bija_command(&settings.bija_command);
    let demo = cli.demo;
    if let Some(why) = startup_problem(bija_found, &settings, &registry).filter(|_| !demo) {
        anyhow::bail!(why);
    }
    // A resumed run brings its own project directory (it was saved absolute), so `mantra runs
    // resume <id>` works from anywhere. `--resume-last` is this project's last unfinished run —
    // the one in cwd / --cwd; with --demo the previous demo's temp repo — never another
    // project's: that run belongs to whoever is working there, and may well be open.
    let resume = match (&cli.resume, cli.resume_last) {
        (Some(id), _) => Some(engine::state::find(id).map_err(|e| anyhow::anyhow!(e))?),
        (None, true) => {
            let here = if demo { None } else { Some(cli.cwd.clone().map(|p| p.canonicalize().unwrap_or(p)).unwrap_or(std::env::current_dir()?)) };
            let pick = resume_last_pick(engine::state::list_all(), here.as_deref());
            Some(pick.ok_or_else(|| anyhow::anyhow!("no unfinished run of this project to resume (mantra runs lists every project's; mantra runs resume <id> takes any)"))?)
        }
        _ => None,
    };
    if let Some(r) = &resume {
        claim_for_resume(r, cli.take_over).map_err(|e| anyhow::anyhow!(e))?;
    }
    let project = match &resume {
        Some(r) => {
            let p = r.project().ok_or_else(|| anyhow::anyhow!("run {} cannot be resumed: {}", r.id, r.state.as_ref().err().cloned().unwrap_or_default()))?;
            if !p.is_dir() {
                anyhow::bail!("run {}: its project directory {} no longer exists (mantra runs delete {} cleans it up)", r.id, p.display(), r.id);
            }
            p.canonicalize().unwrap_or(p)
        }
        // MANTRA_DEMO_PROJECT reuses an earlier demo repo (stress.sh: the /runs overlay and the
        // welcome-screen notice need a project that already has runs).
        None if demo => match std::env::var("MANTRA_DEMO_PROJECT") {
            Ok(p) if std::path::Path::new(&p).is_dir() => PathBuf::from(p),
            _ => demo_project()?,
        },
        None => cli.cwd.clone().map(|p| p.canonicalize().unwrap_or(p)).unwrap_or(std::env::current_dir()?),
    };
    if demo {
        // The demo needs nothing but this binary and its temp repo — so the exe lookup must
        // survive a container without /proc/self/exe (util::self_exe falls back to argv[0]).
        let exe = util::self_exe()?;
        settings.codex_command = vec![exe.to_string_lossy().to_string(), "mock-codex".into()];
        settings.claude_command = vec![exe.to_string_lossy().to_string(), "mock-claude".into()];
        // bija itself is real in the demo — the one built beside this binary — and only the API
        // behind it is simulated, so every built-in pattern drives the harness end to end with
        // zero API calls.
        if let Some(beside) = exe.parent().map(|d| d.join(config::bija_exe_name())).filter(|p| p.is_file()) {
            bija_cmd = vec![beside.to_string_lossy().into_owned()];
        }
        let port = mock_openai::start().await?;
        point_bija_at_mock(&mut registry, port);
        // WP10.6: the two add-on backends run on their mocks, their models added in memory (never
        // saved) whatever is installed — the mixed-backend runs need no real `codex` or `claude`:
        // the demo's whole point is zero external dependencies. After bija's, so the plain
        // aliases stay bija's.
        // Demo fixtures are local mock ids, never a source of models for a real account.
        for m in config::codex_default_models() {
            if !registry.models.iter().any(|x| !x.is_custom_provider() && x.model == m.model) {
                let alias = if registry.get(&m.alias).is_some() { format!("{}-codex", m.alias) } else { m.alias.clone() };
                registry.models.push(config::ModelEntry { alias, ..m });
            }
        }
        let _ = provider_key::add_backend_with(&mut registry, provider_key::Backend::ClaudeCode, true);
        // The demo shows what there is to see, the experiments included — in memory, like the
        // mocks: settings.toml is not touched.
        if !settings.experimental_on(config::EXPERIMENTAL_AKASHA) {
            settings.experimental.push(config::EXPERIMENTAL_AKASHA.to_string());
        }
        // MCP and images too: the stand-in server (`mantra mock-mcp`) for Solo and, through
        // `App::start_run_with`, every pattern role — and every model taking images, so the
        // screenshot tools are offered. A real server of the user's never starts in the demo.
        settings.mcp_servers.insert(app::DEMO_MCP.into(), config::McpServer { command: exe.to_string_lossy().into_owned(), args: vec!["mock-mcp".into()], ..Default::default() });
        settings.solo_mcp = vec![app::DEMO_MCP.into()];
        for m in &mut registry.models {
            m.vision = true;
        }
        // Solo stays on the Codex mock: its scripted command approvals are what the snapshot and
        // web e2e scripts drive in the demo's Solo.
        if registry.get(&settings.default_model).is_some_and(|m| registry.backend_of(m) == config::ProviderKind::Bija) {
            if let Some(sol) = codex_alias_of(&registry, "sol") {
                settings.default_model = sol;
            }
        }
    }
    // L1: on a Linux box where unprivileged user namespaces are off, every worker command dies
    // in bubblewrap. Warn once, up front, and continue automatically (sandbox falls back to
    // danger-full-access) unless the user types n.
    // (MANTRA_SANDBOX_WARNING=<text> forces the notice — stress.sh renders it in demo mode.)
    let sandbox_warning = std::env::var("MANTRA_SANDBOX_WARNING").ok().filter(|w| !w.is_empty()).or_else(|| if demo || cli.snapshot.is_some() { None } else { util::sandbox_probe().err() });
    if let (Some(w), true, false) = (&sandbox_warning, cli.run_goal.is_some() || cli.resume.is_some() || cli.resume_last, cli.no_sandbox_check) {
        eprintln!("mantra: sandbox check failed — {w}");
        eprint!("agents' commands will run WITHOUT the sandbox (git worktrees still keep workers apart). Continue? [Y/n] ");
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
        if matches!(line.trim().to_lowercase().as_str(), "n" | "no") {
            println!("not started. (mantra --no-sandbox-check skips this question)");
            return Ok(());
        }
    }
    ui::theme::init(&settings);
    let (tx, mut rx) = mpsc::unbounded_channel::<AppEvent>();
    // (Also with --snapshot, so /settings' web & remote rows can be rendered headlessly.)
    let mut web = match web_cfg {
        Some(c) => Some(web::Web::start(c, tx.clone()).await?),
        None => None,
    };
    let (hub_tx, mut hub_rx) = mpsc::unbounded_channel();
    {
        let tx = tx.clone();
        tokio::spawn(async move {
            while let Some(e) = hub_rx.recv().await {
                if tx.send(AppEvent::Hub(e)).is_err() {
                    break;
                }
            }
        });
    }
    // The MCP-bridge socket (WP10.4) is only worth opening when a Claude Code agent could actually
    // run — real or (in `--demo`) mocked.
    let enable_bridge = registry.providers.iter().any(|p| p.kind == config::ProviderKind::ClaudeCode);
    let hub = hub::Hub::new(settings.codex_command.clone(), settings.claude_command.clone(), bija_cmd, hub_tx, enable_bridge);
    let mut app = App::new(settings, registry, project, hub, tx.clone(), demo);
    app.web = web.as_ref().map(|w| w.link());
    if let Some(p) = cli.pattern {
        app.session_pattern(&p);
    }
    app.start_solo();
    if let Some(w) = sandbox_warning {
        app.set_sandbox_warning(w);
    }
    // Web::start already logged these; the toast is for the person looking at the TUI. There is one
    // toast slot, so what startup already put there (a lost MCP bridge) goes first, not under.
    if let Some(w) = web.as_ref().map(|w| w.cfg.warnings()).filter(|w| !w.is_empty()) {
        let said = app.toast.take().map(|(t, _, _)| t);
        app.toast(said.into_iter().chain(w).collect::<Vec<_>>().join(" · "), crate::agent::Level::Warn);
    }
    if let Some(goal) = &cli.run_goal {
        app.start_run_with(goal, None, cli.parallel);
    }
    if let Some(r) = resume {
        app.resume_run(r);
    }
    if let Some(script) = cli.snapshot.clone() {
        return snapshot(app, rx, &script, cli.size).await;
    }
    if let Some(w) = &web {
        announce_web(w, headless).await;
    }
    if headless {
        let (quit_tx, quit_rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            #[cfg(unix)]
            {
                let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
                let sigterm = async {
                    match term.as_mut() {
                        Some(t) => {
                            t.recv().await;
                        }
                        None => std::future::pending::<()>().await,
                    }
                };
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = sigterm => {}
                }
            }
            #[cfg(not(unix))]
            let _ = tokio::signal::ctrl_c().await;
            let _ = quit_tx.send(());
        });
        mlog!("headless: running (ctrl+c or SIGTERM stops)");
        let res = event_loop(Option::<&mut Terminal<TestBackend>>::None, &mut app, &mut rx, &mut web, Some(quit_rx)).await;
        eprintln!("mantra: shutting down");
        if let Some(w) = web.as_mut() {
            w.shutdown();
        }
        let stopping = app.shutdown();
        // bija agents get to finish their commands and transcripts before the runtime goes (it
        // kills every child still running); the sleep is the other backends' moment, as before.
        tokio::join!(tokio::time::sleep(Duration::from_millis(300)), stopping.wait());
        if app.demo {
            eprintln!("demo project left at {}", app.project.display());
        }
        return res;
    }

    terminal::enable_raw_mode()?;
    let mut out = std::io::stdout();
    execute!(out, EnterAlternateScreen, EnableBracketedPaste)?;
    if app.settings.mouse {
        execute!(out, EnableMouseCapture)?;
    }
    // tmux/screen/linux console don't need (or answer) the kitty keyboard query.
    let term_env = std::env::var("TERM").unwrap_or_default();
    let muxed = in_tmux() || term_env.starts_with("screen") || term_env == "linux";
    let kbd = !muxed && terminal::supports_keyboard_enhancement().unwrap_or(false);
    let _ = execute!(out, Print("\x1b[22;0t")); // push the window title (restored on exit)
    if kbd {
        let _ = execute!(out, PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES));
    }
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if IN_DRAW.load(Ordering::SeqCst) {
            mlog!("render panic (caught): {info}");
            return;
        }
        restore_terminal(kbd);
        mlog!("PANIC: {info}");
        default_hook(info);
    }));
    let mut terminal = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    terminal.clear()?;

    let stop = Arc::new(AtomicBool::new(false));
    {
        let tx = tx.clone();
        let stop = stop.clone();
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                if event::poll(Duration::from_millis(100)).unwrap_or(false) {
                    match event::read() {
                        Ok(e) => {
                            if tx.send(AppEvent::Term(e)).is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
            }
        });
    }
    if in_tmux() {
        if let Some(ms) = tmux_option(&["show-options", "-sv", "escape-time"]).and_then(|v| v.parse::<u64>().ok()) {
            if ms > 100 {
                app.toast(format!("tmux escape-time is {ms}ms, so Esc will feel slow. Add `set -sg escape-time 10` to ~/.tmux.conf"), agent::Level::Warn);
            }
        }
    }
    if let Some(u) = web.as_ref().and_then(|w| w.link().info.urls.first().cloned()) {
        app.toast(format!("web UI on {u} — /settings for details"), agent::Level::Info);
    }
    let res = event_loop(Some(&mut terminal), &mut app, &mut rx, &mut web, None).await;
    stop.store(true, Ordering::Relaxed);
    if let Some(w) = web.as_mut() {
        w.shutdown();
    }
    let stopping = app.shutdown();
    restore_terminal(kbd);
    // As in the headless path above, with the terminal already back to the user while bija agents
    // finish.
    tokio::join!(tokio::time::sleep(Duration::from_millis(150)), stopping.wait());
    if app.demo {
        eprintln!("demo project left at {}", app.project.display());
    }
    res
}

/// `mantra runs`: one line per run, every project, newest first.
fn runs_list() {
    let all = engine::state::list_all();
    if all.is_empty() {
        println!("no runs yet — `mantra run \"<goal>\"` starts one (or `mantra --demo`)");
        return;
    }
    println!("{:<36} {:<20} {:<18} {:>9}  id", "run", "project", "stage", "updated");
    for r in &all {
        let project = r.project().and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string())).unwrap_or_else(|| r.project_key.rsplit_once('-').map(|(a, _)| a.to_string()).unwrap_or_else(|| r.project_key.clone()));
        println!("{:<36} {:<20} {:<18} {:>9}  {}", util::trunc(&r.title(), 36), util::trunc(&project, 20), util::trunc(&r.stage_label(), 18), engine::state::fmt_ago(r.updated_unix), r.id);
    }
    let n = all.iter().filter(|r| r.unfinished()).count();
    println!("\n{} run{}, {n} unfinished · mantra runs resume <id> · mantra runs delete <id>", all.len(), if all.len() == 1 { "" } else { "s" });
}

/// `mantra runs delete <id> [--yes]`: worktrees + branches in the project repo, then the journal.
fn runs_delete(id: &str, yes: bool) {
    let r = match engine::state::find(id) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("mantra: {e}");
            std::process::exit(2);
        }
    };
    if let Some(owner) = engine::state::live_owner(&r.dir) {
        // its worktrees are that process's working copies
        eprintln!("mantra: {}", owner.delete_refusal(&r.id));
        std::process::exit(2);
    }
    let where_ = r.project().map(|p| p.display().to_string()).unwrap_or_else(|| r.project_key.clone());
    println!("run {} [{}] ({}) — {}\n  {}", r.title(), r.id, r.stage_label(), where_, util::trunc(&r.brief(), 100));
    if r.unfinished() {
        println!("  this run is not finished; its work lives on branch mantra/{} until deleted", r.id);
    }
    if !yes {
        print!("delete it, its worktrees and branches? [y/N] ");
        use std::io::Write;
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
        if !matches!(line.trim().to_lowercase().as_str(), "y" | "yes") {
            println!("kept.");
            return;
        }
    }
    for l in engine::state::delete(&r) {
        println!("  {l}");
    }
    println!("deleted {}", r.id);
}

fn in_tmux() -> bool {
    std::env::var("TMUX").map(|v| !v.is_empty()).unwrap_or(false)
}

fn tmux_option(args: &[&str]) -> Option<String> {
    let o = std::process::Command::new("tmux").args(args).stderr(std::process::Stdio::null()).output().ok()?;
    o.status.success().then(|| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

fn restore_terminal(kbd: bool) {
    let mut out = std::io::stdout();
    if kbd {
        let _ = execute!(out, PopKeyboardEnhancementFlags);
    }
    let _ = execute!(out, DisableMouseCapture, DisableBracketedPaste, LeaveAlternateScreen, ratatui::crossterm::cursor::Show, Print("\x1b[23;0t"));
    let _ = terminal::disable_raw_mode();
}

/// Tell the user where the web UI / remote link is. Headless: stderr is the only place, so the
/// relay task prints the remote link and password there (once, after the relay is first reached —
/// see `remote::Inner::announce`); with a TUI it lives in `/settings` (Web & remote).
async fn announce_web(w: &web::Web, headless: bool) {
    let link = w.link();
    if headless {
        for u in &link.info.urls {
            eprintln!("mantra web: {u}{}", if link.info.password { "  (password required)" } else { "" });
        }
        if link.info.self_signed {
            if let Some(u) = link.info.urls.first() {
                eprintln!("mantra web: trust this server on your devices: {u}/cert.pem");
            }
        }
    }
    if let Some(r) = &w.remote {
        if headless {
            // Unless the relay task got there first: a link printed before the relay answered
            // looks valid but leads nowhere, so it follows the first "connected".
            if !r.info().connected {
                eprintln!("mantra remote: waiting for the relay {}…", r.inner().relay);
            }
        } else {
            r.ready(Duration::from_secs(10)).await;
        }
    }
}

/// Minimum spacing of web deltas (≈15/s): enough for streaming text to feel live.
const WEB_PUBLISH_EVERY: Duration = Duration::from_millis(66);

async fn next_web_control(web: &mut Option<web::Web>) -> Option<web::Control> {
    match web {
        Some(w) => w.next_control().await,
        None => std::future::pending().await,
    }
}

async fn event_loop<B: Backend>(mut terminal: Option<&mut Terminal<B>>, app: &mut App, rx: &mut mpsc::UnboundedReceiver<AppEvent>, web: &mut Option<web::Web>, quit: Option<tokio::sync::oneshot::Receiver<()>>) -> Result<()> {
    let mut dirty = true;
    let mut web_dirty = true;
    let mut last_draw = Instant::now() - Duration::from_secs(1);
    let mut last_pub = Instant::now() - WEB_PUBLISH_EVERY;
    let mut last_title = String::new();
    let mut quit = quit;
    // what the terminal was set up with: `/settings` may change either while the app runs
    let mut mouse = app.settings.mouse;
    loop {
        let frame = Duration::from_millis(1000 / app.settings.fps.clamp(4, 60) as u64);
        let animating = app.animating();
        if let Some(t) = terminal.as_deref_mut() {
            if app.settings.mouse != mouse {
                mouse = app.settings.mouse;
                let _ = if mouse { execute!(std::io::stdout(), EnableMouseCapture) } else { execute!(std::io::stdout(), DisableMouseCapture) };
            }
            if app.force_clear {
                app.force_clear = false;
                t.clear()?;
                dirty = true;
            }
            if dirty || (animating && last_draw.elapsed() >= frame) {
                // Synchronized output: the terminal (and tmux ≥ 3.4) paints each frame atomically — no tearing.
                let _ = ratatui::crossterm::queue!(std::io::stdout(), terminal::BeginSynchronizedUpdate);
                t.draw(|f| safe_draw(f, app))?;
                let _ = execute!(std::io::stdout(), terminal::EndSynchronizedUpdate);
                last_draw = Instant::now();
                dirty = false;
                let title = app.title();
                if title != last_title {
                    let _ = execute!(std::io::stdout(), terminal::SetTitle(&title));
                    last_title = title;
                }
            }
        } else {
            app.force_clear = false;
            dirty = false;
        }
        let mut wait = if animating && terminal.is_some() { frame.saturating_sub(last_draw.elapsed()).max(Duration::from_millis(5)) } else { Duration::from_millis(1000) };
        if web.is_some() && web_dirty {
            // A change the rate limit held back still goes out: wake up when it may.
            wait = wait.min(WEB_PUBLISH_EVERY.saturating_sub(last_pub.elapsed()).max(Duration::from_millis(5)));
        }
        let quit_signal = async {
            match quit.as_mut() {
                Some(q) => {
                    let _ = q.await;
                }
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            ev = rx.recv() => {
                match ev {
                    Some(e) => {
                        let is_resize = matches!(e, AppEvent::Term(Event::Resize(..)));
                        app.on_event(e);
                        let mut n = 0;
                        while n < 512 {
                            match rx.try_recv() {
                                Ok(e) => { app.on_event(e); n += 1; }
                                Err(_) => break,
                            }
                        }
                        if is_resize {
                            if let Some(t) = terminal.as_deref_mut() {
                                t.autoresize()?;
                            }
                        }
                        dirty = true;
                        web_dirty = true;
                    }
                    None => break,
                }
            }
            c = next_web_control(web) => match (c, web.as_mut()) {
                // Relay state / key changes: publish on the normal cadence, redraw the header.
                (Some(web::Control::Refresh), Some(_)) => {
                    web_dirty = true;
                    dirty = true;
                }
                (Some(c), Some(w)) => {
                    w.on_control(c, app);
                    last_pub = Instant::now();
                    web_dirty = false;
                }
                _ => {}
            },
            _ = quit_signal => {
                app.quit = true;
            }
            _ = tokio::time::sleep(wait) => {
                // Idle (no events, nothing animating) but a clock is on screen: the 1s wait just
                // elapsed, so let it tick — one frame, not an animation.
                if !animating && app.clocks_visible() && last_draw.elapsed() >= Duration::from_millis(900) {
                    dirty = true;
                }
            }
        }
        app.tick();
        if !app.notes.is_empty() {
            let notes = std::mem::take(&mut app.notes);
            if let Some(w) = web.as_ref() {
                w.notes(&notes, app);
            }
            if app.settings.notify && terminal.is_some() {
                let mut out = std::io::stdout();
                for n in notes {
                    let n = n.replace(['\x07', '\x1b'], "");
                    if in_tmux() {
                        // DCS passthrough (needs `allow-passthrough on`) + a bell, which tmux shows as a window flag.
                        let _ = execute!(out, Print(format!("\x1bPtmux;\x1b\x1b]9;{n}\x07\x1b\\\x07")));
                    } else {
                        let _ = execute!(out, Print(format!("\x1b]9;{n}\x07")));
                    }
                }
                let _ = out.flush();
            }
        }
        if let Some(w) = web.as_mut() {
            // Timers (elapsed, quiet, toasts expiring) move while animating even without events.
            let since = last_pub.elapsed();
            if (web_dirty && since >= WEB_PUBLISH_EVERY) || (animating && since >= Duration::from_secs(1)) {
                w.publish(app);
                last_pub = Instant::now();
                web_dirty = false;
            }
        }
        if app.quit {
            break;
        }
    }
    Ok(())
}

/// Headless run for tests/screenshots: --snapshot "wait:2;type:hello;key:enter;until:Done@60;snap"
async fn snapshot(mut app: App, mut rx: mpsc::UnboundedReceiver<AppEvent>, script: &str, size: (u16, u16)) -> Result<()> {
    let mut term = Terminal::new(TestBackend::new(size.0, size.1))?;
    fn pump(app: &mut App, rx: &mut mpsc::UnboundedReceiver<AppEvent>) {
        while let Ok(e) = rx.try_recv() {
            app.on_event(e);
        }
        app.tick();
    }
    for step in script.split(';').map(|s| s.trim()).filter(|s| !s.is_empty()) {
        let (cmd, arg) = step.split_once(':').unwrap_or((step, ""));
        match cmd {
            "wait" => {
                let end = Instant::now() + Duration::from_secs_f64(arg.parse().unwrap_or(1.0));
                while Instant::now() < end {
                    pump(&mut app, &mut rx);
                    tokio::time::sleep(Duration::from_millis(40)).await;
                }
            }
            "until" => {
                let (text, t) = arg.rsplit_once('@').unwrap_or((arg, "120"));
                let end = Instant::now() + Duration::from_secs(t.parse().unwrap_or(120));
                loop {
                    pump(&mut app, &mut rx);
                    term.draw(|f| ui::draw(f, &mut app))?;
                    if dump(&term).contains(text) {
                        break;
                    }
                    if Instant::now() > end {
                        println!("!! timeout waiting for {text:?}");
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
            "type" => {
                for c in arg.chars() {
                    app.on_event(AppEvent::Term(Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))));
                }
            }
            "key" => {
                let (m, k) = match arg.split_once('+') {
                    Some(("ctrl", k)) => (KeyModifiers::CONTROL, k),
                    Some(("alt", k)) => (KeyModifiers::ALT, k),
                    Some(("shift", k)) => (KeyModifiers::SHIFT, k),
                    _ => (KeyModifiers::NONE, arg),
                };
                let code = match k {
                    "enter" => KeyCode::Enter,
                    "esc" => KeyCode::Esc,
                    "tab" => KeyCode::Tab,
                    "backtab" => KeyCode::BackTab,
                    "up" => KeyCode::Up,
                    "down" => KeyCode::Down,
                    "left" => KeyCode::Left,
                    "right" => KeyCode::Right,
                    "space" => KeyCode::Char(' '),
                    "pgup" => KeyCode::PageUp,
                    "pgdn" => KeyCode::PageDown,
                    "delete" | "del" => KeyCode::Delete,
                    "backspace" => KeyCode::Backspace,
                    "home" => KeyCode::Home,
                    "end" => KeyCode::End,
                    "f1" => KeyCode::F(1),
                    s => KeyCode::Char(s.chars().next().unwrap_or(' ')),
                };
                app.on_event(AppEvent::Term(Event::Key(KeyEvent { code, modifiers: m, kind: KeyEventKind::Press, state: KeyEventState::NONE })));
            }
            "resize" => {
                let (w, h) = arg.split_once('x').and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?))).unwrap_or(size);
                term.backend_mut().resize(w, h);
                term.resize(ratatui::layout::Rect::new(0, 0, w, h))?;
            }
            "sweep" => {
                const SIZES: &[(u16, u16)] = &[(40, 12), (41, 13), (50, 14), (60, 18), (72, 20), (80, 24), (99, 30), (100, 30), (110, 32), (120, 36), (140, 42), (200, 60), (320, 90)];
                pump(&mut app, &mut rx);
                for (w, h) in SIZES {
                    term.backend_mut().resize(*w, *h);
                    term.resize(ratatui::layout::Rect::new(0, 0, *w, *h))?;
                    term.draw(|f| ui::draw(f, &mut app))?;
                }
                term.backend_mut().resize(size.0, size.1);
                term.resize(ratatui::layout::Rect::new(0, 0, size.0, size.1))?;
                println!("sweep ok: {arg}");
            }
            "snap" => {
                pump(&mut app, &mut rx);
                term.draw(|f| ui::draw(f, &mut app))?;
                println!("──── snapshot {arg} ────\n{}", dump(&term));
            }
            _ => {}
        }
    }
    pump(&mut app, &mut rx);
    let stopping = app.shutdown();
    tokio::join!(tokio::time::sleep(Duration::from_millis(200)), stopping.wait());
    Ok(())
}

fn dump(t: &Terminal<TestBackend>) -> String {
    let b = t.backend().buffer();
    let mut s = String::new();
    for y in 0..b.area.height {
        let mut line = String::new();
        for x in 0..b.area.width {
            line.push_str(b[(x, y)].symbol());
        }
        s.push_str(line.trim_end());
        s.push('\n');
    }
    s
}

fn doctor() {
    let ok = |b: bool| if b { "✓" } else { "✗" };
    let s = config::Settings::load();
    let r = config::Registry::load();
    println!("mantra {} doctor\n", env!("CARGO_PKG_VERSION"));
    // Bija is required only when a configured model uses it.
    let (bija_cmd, found) = config::resolve_bija_command(&s.bija_command);
    let (good, text) = probe_cli(bija_cmd.first().map(String::as_str).unwrap_or("bija"), " — it ships beside mantra: reinstall, or set bija_command / $MANTRA_BIJA");
    let bija_used = r.models.iter().any(|m| r.backend_of(m) == config::ProviderKind::Bija);
    println!("{} bija: {text} · {} ({})", if bija_used { ok(good) } else { " " }, bija_cmd.join(" "), found.describe());
    let (verdict, text) = provider_row(&s, &r);
    println!("{} provider: {text}", verdict.map(ok).unwrap_or(" "));
    // Codex and Claude Code are optional backends: found or not, graded only when a model in
    // /models runs on them. WP10.5: every CLI probe has a 5s cap (thread + `recv_timeout`), so a
    // hung binary (observed for `claude` under `subscription` auth, §0.3) can never hang doctor.
    let uses = |kind: config::ProviderKind| r.models.iter().any(|m| r.backend_of(m) == kind);
    let first = |cmd: &[String], name: &str| cmd.first().cloned().unwrap_or_else(|| name.to_string());
    let (codex0, claude0) = (first(&s.codex_command, "codex"), first(&s.claude_command, "claude"));
    let codex_used = uses(config::ProviderKind::Codex);
    let (codex_good, text) = probe_cli(&codex0, "");
    optional_backend_row("codex", codex_good, &text, codex_used);
    if codex_used && codex_good && !s.codex_command.is_empty() {
        // Readiness, not presence: the same spawn + `initialize` handshake an agent does, one
        // no-op command through the configured sandbox, then a clean shutdown — each its own row
        // and verdict, each bounded by PROBE_TIMEOUT, each failure quoting Codex's own error line.
        let (server, sandbox) = probe_app_server(&s, &r);
        match &server {
            Ok(t) => println!("{} codex app-server: {t}", ok(true)),
            Err(e) => println!("{} codex app-server: {e}", ok(false)),
        }
        match &sandbox {
            Ok(t) => println!("{} sandbox: {t}", ok(true)),
            Err(e) if e.starts_with("skipped") => println!("{} sandbox: {e}", ok(false)),
            Err(e) => {
                // A real failure (not "skipped"): the kernel knob explains it when it can, and
                // either way say what Codex agents run with meanwhile (the automatic fallback).
                let fallback = "Codex agents run with danger-full-access until this is fixed";
                match util::sandbox_probe() {
                    Err(why) => println!("{} sandbox: {e}\n  {why}\n  {fallback}", ok(false)),
                    _ => println!("{} sandbox: {e}\n  {fallback}", ok(false)),
                }
            }
        }
        if let Ok(o) = std::process::Command::new(&codex0).args(["login", "status"]).output() {
            let t = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
            let line = config::login_status_line(&t);
            let needed = codex_account_needed(&s, &r);
            match (o.status.success(), needed.is_empty()) {
                (true, _) => println!("{} login: {line}", ok(true)),
                (false, false) => println!("{} login: {line} — needed by {} (run `codex login`)", ok(false), needed.join(", ")),
                (false, true) => println!("  login: {line} — not needed: neither the default model nor a role of the default pattern is on Codex's account"),
            }
        }
    } else {
        // bija's own sandbox wraps commands in bubblewrap where the kernel lets it, and runs them
        // plainly where it does not (worktrees still keep workers apart): worth a line, no verdict.
        match util::sandbox_probe() {
            Ok(()) => println!("{} sandbox: unprivileged user namespaces are available", ok(true)),
            Err(why) => println!("  sandbox: {why} — bija runs commands without bubblewrap here"),
        }
    }
    let claude_used = uses(config::ProviderKind::ClaudeCode);
    let (good, text) = probe_cli(&claude0, "");
    optional_backend_row("claude", good, &text, claude_used);
    if claude_used && crate::util::effective_uid_is_root() {
        println!("  IS_SANDBOX note: running as root — Mantra sets IS_SANDBOX=1 for claude agents (--dangerously-skip-permissions is otherwise refused for uid 0)");
    }
    let git = std::process::Command::new("git").arg("--version").output().map(|o| o.status.success()).unwrap_or(false);
    println!("{} git (needed for isolated worktrees)", ok(git));
    println!("  terminal: TERM={} COLORTERM={} TERM_PROGRAM={}", std::env::var("TERM").unwrap_or_default(), std::env::var("COLORTERM").unwrap_or_default(), std::env::var("TERM_PROGRAM").unwrap_or_default());
    ui::theme::init(&s);
    println!("  colors: {:?} · glyphs: {}", ui::theme::depth(), if ui::theme::ascii() { "ascii" } else { "unicode" });
    if in_tmux() {
        println!("\n  tmux:");
        let esc = tmux_option(&["show-options", "-sv", "escape-time"]).and_then(|v| v.parse::<u64>().ok()).unwrap_or(500);
        println!("{} escape-time {esc}ms{}", ok(esc <= 50), if esc > 50 { "  → set -sg escape-time 10   (otherwise Esc lags)" } else { "" });
        let dt = tmux_option(&["show-options", "-gv", "default-terminal"]).unwrap_or_default();
        println!("{} default-terminal {dt}{}", ok(dt.contains("256")), if dt.contains("256") { "" } else { "  → set -g default-terminal tmux-256color" });
        let feats = format!("{} {}", tmux_option(&["show-options", "-sv", "terminal-features"]).unwrap_or_default(), tmux_option(&["show-options", "-sv", "terminal-overrides"]).unwrap_or_default());
        let rgb = feats.contains("RGB") || feats.contains("Tc");
        println!("{} truecolor{}", ok(rgb), if rgb { "" } else { "  → set -as terminal-features ',*:RGB'   (else 256 colours, still fine)" });
        let pt = tmux_option(&["show-options", "-gv", "allow-passthrough"]).unwrap_or_default();
        println!("{} allow-passthrough {pt}{}", ok(pt == "on" || pt == "all"), if pt == "on" || pt == "all" { "" } else { "  → set -g allow-passthrough on   (desktop notifications; bells still work)" });
        println!("  side panel / pulse toggle is ctrl+t (ctrl+b is your tmux prefix)");
    }
    println!("  config: {}", config::home().display());
    println!("  models: {}", r.models.iter().map(|m| format!("{}={}", m.alias, m.model)).collect::<Vec<_>>().join(", "));
    for p in &r.providers {
        let key_src = |p: &config::ProviderEntry| -> String {
            if !p.env_key.trim().is_empty() && std::env::var(p.env_key.trim()).map(|v| !v.trim().is_empty()).unwrap_or(false) {
                format!("${} set", p.env_key)
            } else if p.api_key.as_ref().map(|k| !k.trim().is_empty()).unwrap_or(false) {
                "key stored in models.toml".to_string()
            } else if p.env_key.trim().is_empty() {
                "no key".to_string()
            } else {
                format!("${} NOT set", p.env_key)
            }
        };
        if p.kind == config::ProviderKind::Bija {
            // A bija key is optional (a local server takes none), so a missing one is no failure.
            match p.draft_reason() {
                Some(why) => println!("{} provider {} (Bija): {why} (/models to fix)", ok(false), p.id),
                None => {
                    // Whether a missing key matters depends on where the requests go: say which.
                    let host = p.host().unwrap_or_default();
                    let name = host.rsplit_once(':').filter(|(_, port)| port.chars().all(|c| c.is_ascii_digit())).map(|(h, _)| h).unwrap_or(&host).trim_matches(['[', ']']).to_string();
                    let local = name == "localhost" || name == "::1" || name == "0.0.0.0" || name.ends_with(".local") || ["127.", "10.", "192.168."].iter().any(|pre| name.starts_with(pre));
                    let why = if local { "fine for a local server".to_string() } else { format!("{name} will want one") };
                    let key = if p.resolve_key().is_some() {
                        key_src(p)
                    } else if p.env_key.trim().is_empty() {
                        format!("no key — {why}")
                    } else {
                        format!("${} not set — {why}", p.env_key.trim())
                    };
                    // the wire in /models' words (`openai` | `anthropic`), not bija's own flag value
                    println!("{} provider {} (Bija, wire {}): {} · {key}", if p.resolve_key().is_some() { ok(true) } else { " " }, p.id, p.bija_wire().trim_end_matches("-chat"), p.base_url.trim());
                }
            }
        } else if p.kind == config::ProviderKind::ClaudeCode {
            if p.auth == "api_key" {
                println!("{} provider {} (Claude Code, api_key): {}", ok(p.resolve_key().is_some()), p.id, key_src(p));
            } else {
                match claude_auth_status(&claude0) {
                    Some(line) => println!("{} provider {} (Claude Code, subscription): {line}", ok(true), p.id),
                    None => println!("  provider {} (Claude Code, subscription): OAuth login — no `claude auth status` to check; run `claude` once and log in", p.id),
                }
            }
        } else {
            println!("{} provider {}: {}", ok(p.resolve_key().is_some()), p.id, key_src(p));
        }
    }
    println!("  log: {}", config::log_path().display());
}

/// One optional backend's doctor row: graded only when a model in /models runs on it (`used`) —
/// then a CLI that does not answer is a ✗ that says what to do — else just what the probe found
/// (`text`: the version, "not found on PATH", or why it did not answer).
fn optional_backend_row(name: &str, found: bool, text: &str, used: bool) {
    let what = if text.contains("not found on PATH") { "not installed" } else { text };
    match (used, found) {
        (true, true) => println!("✓ {name}: {what}"),
        (true, false) => println!("✗ {name}: {what} — models in /models run on it: install it, or delete them there"),
        (false, _) => println!("  {name}: {what} — optional backend, no model uses it (/models adds it)"),
    }
}

/// Hard cap on each readiness probe in `doctor` (handshake, no-op exec, `GET /models`), so a hung
/// app-server or a black-holed provider host can never hang doctor itself.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Launches the configured `codex_command` exactly as `hub::run_process` does (`rpc::spawn` with
/// the registry's `-c model_providers.*` args and the default model's key in the environment),
/// runs the `initialize`/`initialized` handshake, then asks the server for one `command/exec` of
/// `true` under the configured sandbox policy — the same exec path an agent's commands take — and
/// shuts the process down cleanly (stdin closed, short grace, kill). Returns the app-server row
/// and the sandbox row; a failure carries Codex's own last stderr line, not a generic message.
fn probe_app_server(s: &config::Settings, r: &config::Registry) -> (Result<String, String>, Result<String, String>) {
    let rt = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => return (Err(format!("no runtime: {e}")), Err("skipped".into())),
    };
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let m = r.resolve(&s.default_model);
    let envs: Vec<(String, String)> = r.providers.iter().find(|p| p.id == m.provider).and_then(|p| p.resolve_key().map(|k| (p.env_var_name(), k))).into_iter().collect();
    let extra = r.provider_args();
    let sandbox = s.sandbox.clone();
    rt.block_on(async move {
        let started = Instant::now();
        let (conn, mut inc, mut child) = match rpc::spawn(&s.codex_command, &extra, &cwd, &envs) {
            Ok(x) => x,
            Err(e) => return (Err(e.to_string()), Err("skipped — app-server did not start".into())),
        };
        let hello = match tokio::time::timeout(PROBE_TIMEOUT, rpc::handshake(&conn)).await {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                let tail = rpc::stderr_after_failure(&conn, &mut inc, Duration::from_millis(1500)).await;
                let code = child.try_wait().ok().flatten().and_then(|st| st.code()).map(|c| format!(" (exit code {c})")).unwrap_or_default();
                let _ = child.kill().await;
                let why = if tail.trim().is_empty() { format!("handshake failed: {e}{code}") } else { format!("handshake failed: {e}{code}\n    {}", tail.trim().replace('\n', "\n    ")) };
                return (Err(why), Err("skipped — app-server not ready".into()));
            }
            Err(_) => {
                let _ = child.kill().await;
                return (Err(format!("no answer to `initialize` within {}s", PROBE_TIMEOUT.as_secs())), Err("skipped — app-server not ready".into()));
            }
        };
        let agent = hello.get("userAgent").and_then(|u| u.as_str()).unwrap_or("").to_string();
        let server = Ok(format!("ready — handshake in {:.1}s{}", started.elapsed().as_secs_f32(), if agent.is_empty() { String::new() } else { format!(" · {agent}") }));

        // One harmless command through the sandbox policy the Solo agent would get.
        let policy = match sandbox.as_str() {
            "danger-full-access" => json!({ "type": "dangerFullAccess" }),
            "read-only" => json!({ "type": "readOnly" }),
            _ => json!({ "type": "workspaceWrite", "writableRoots": [cwd.to_string_lossy()] }),
        };
        let params = json!({ "command": ["true"], "cwd": cwd.to_string_lossy(), "sandboxPolicy": policy, "timeoutMs": 5000 });
        let exec = conn.request_timeout("command/exec", params, PROBE_TIMEOUT).await;
        let sandbox_row = match exec {
            Ok(v) => {
                let code = v.get("exitCode").and_then(|c| c.as_i64()).unwrap_or(-1);
                let err = v.get("stderr").and_then(|x| x.as_str()).unwrap_or("").trim().to_string();
                if code == 0 {
                    Ok(format!("{sandbox} — `true` ran through the app-server sandbox{}", if sandbox == "danger-full-access" { " (no isolation in this mode)" } else { "" }))
                } else {
                    Err(format!("{sandbox} — `true` exited {code} in the sandbox{}", if err.is_empty() { String::new() } else { format!(": {}", util::trunc(&err, 300)) }))
                }
            }
            // Codex before `command/exec` existed: probe the sandbox helper it runs commands with.
            // (The app-server answers an unknown method with -32600 "unknown variant", not -32601.)
            Err(e) if e.code == -32601 || (e.code == -32600 && e.message.contains("unknown variant")) => sandbox_helper_probe(&s.codex_command[0], &cwd),
            Err(e) => {
                let tail = rpc::stderr_after_failure(&conn, &mut inc, Duration::from_millis(1500)).await;
                let last = tail.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_string();
                Err(format!("{sandbox} — command/exec failed: {e}{}", if last.is_empty() { String::new() } else { format!("\n    {}", util::trunc(&last, 300)) }))
            }
        };

        // Clean shutdown: closing stdin is how the app-server is told to go; kill only if it lingers.
        drop(conn);
        if tokio::time::timeout(Duration::from_secs(2), child.wait()).await.is_err() {
            let _ = child.kill().await;
        }
        (server, sandbox_row)
    })
}

/// `codex sandbox -- true`: the helper (bundled bubblewrap on Linux, seatbelt on macOS) an older
/// app-server runs every command through, with a hard cap so a stuck helper can't hang doctor.
fn sandbox_helper_probe(cmd0: &str, cwd: &std::path::Path) -> Result<String, String> {
    let name = cmd0.to_string();
    let dir = cwd.to_path_buf();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(std::process::Command::new(&name).args(["sandbox", "-C"]).arg(&dir).args(["--", "true"]).output());
    });
    match rx.recv_timeout(PROBE_TIMEOUT) {
        Ok(Ok(o)) if o.status.success() => Ok("`codex sandbox -- true` ran (this app-server has no command/exec; its sandbox helper works)".into()),
        Ok(Ok(o)) => {
            let err = util::strip_ansi(&String::from_utf8_lossy(&o.stderr));
            let last = err.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_string();
            Err(format!("`codex sandbox -- true` exited {:?}{}", o.status.code(), if last.is_empty() { String::new() } else { format!(": {}", util::trunc(&last, 300)) }))
        }
        Ok(Err(e)) => Err(format!("couldn't run `{cmd0} sandbox`: {e}")),
        Err(_) => Err(format!("`codex sandbox -- true` gave no answer within {}s", PROBE_TIMEOUT.as_secs())),
    }
}

/// The `provider` doctor row for the default model's provider: is the key source there (the
/// env var set, or a key stored in models.toml), and does the endpoint accept it — checked with
/// `GET /models`, which is free on every provider; no inference request is ever sent. The verdict
/// is `None` when there is nothing Mantra can check (Codex's own account, a subscription login).
fn provider_row(s: &config::Settings, r: &config::Registry) -> (Option<bool>, String) {
    let alias = &s.default_model;
    let Some(m) = r.get(alias) else {
        return (None, format!("no default model configured — add a provider and model in /models, then choose it in /settings"));
    };
    if !m.is_custom_provider() {
        return (None, format!("`{alias}` runs on Codex's own account — see the login row"));
    }
    let Some(p) = r.providers.iter().find(|p| p.id == m.provider) else {
        return (Some(false), format!("`{alias}` uses provider '{}' which is not in /models", m.provider));
    };
    let name = r.provider_name(&p.id);
    // A placeholder (or malformed) base_url never gets a request: the key would go to example.com.
    if let Some(why) = p.url_problem().or_else(|| p.wire_problem()) {
        return (Some(false), format!("{name} for `{alias}`: {why} (/models to fix)"));
    }
    let key_src = if !p.env_key.trim().is_empty() && std::env::var(p.env_key.trim()).map(|v| !v.trim().is_empty()).unwrap_or(false) {
        format!("${} set", p.env_key.trim())
    } else if p.api_key.as_ref().map(|k| !k.trim().is_empty()).unwrap_or(false) {
        "key stored in models.toml".to_string()
    } else {
        format!("${} NOT set and no key stored (/models to fix)", p.env_var_name())
    };
    if p.kind == config::ProviderKind::ClaudeCode {
        if p.auth != "api_key" {
            return (None, format!("{name} (Claude Code, subscription) for `{alias}`: `claude` holds the login — nothing to check here"));
        }
        return (Some(p.resolve_key().is_some()), format!("{name} (Claude Code, api_key) for `{alias}`: {key_src}"));
    }
    let key = p.resolve_key();
    if key.is_none() && p.needs_key() {
        return (Some(false), format!("{name} for `{alias}`: {key_src}"));
    }
    // Only a Bija provider gets here without a key (it is optional): the endpoint itself says
    // whether it wants one — a local server answers, a hosted API says 401.
    let key_src = if key.is_some() { key_src } else { "no key".to_string() };
    // `GET /models` on a thread with a hard cap: curl's own limit is longer than PROBE_TIMEOUT.
    let (base, model, header, sent) = (p.base_url.clone(), m.model.clone(), discover::key_header(p), key.is_some());
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(discover::list_models(&base, key.as_deref(), header));
    });
    let host = p.base_url.trim().trim_end_matches('/').to_string();
    match rx.recv_timeout(PROBE_TIMEOUT) {
        Ok(Ok(found)) => {
            let listed = found.iter().any(|f| f.id == model);
            let verdict = if sent { "accepts the key" } else { "answers without one" };
            (Some(true), format!("{name} for `{alias}`: {key_src} · {host}/models {verdict} ({} models{})", found.len(), if listed { format!(", `{model}` listed") } else { format!("; `{model}` not among them — the tag is sent as-is") }))
        }
        Ok(Err(e)) if (e.contains("HTTP 401") || e.contains("HTTP 403")) && !sent => (Some(false), format!("{name} for `{alias}`: no key, and {host} wants one — set ${} or paste one in /models ({e})", p.env_var_name())),
        Ok(Err(e)) if e.contains("HTTP 401") || e.contains("HTTP 403") => (Some(false), format!("{name} for `{alias}`: {key_src} · key rejected — {e}")),
        Ok(Err(e)) if e.starts_with("couldn't reach") || e.starts_with("couldn't run") => (Some(false), format!("{name} for `{alias}`: {key_src} · {e}")),
        // Answered, but not with a model list (no /models endpoint): the key is there, that is all doctor can say.
        Ok(Err(e)) => (None, format!("{name} for `{alias}`: {key_src} · not verified ({e})")),
        Err(_) => (Some(false), format!("{name} for `{alias}`: {key_src} · {host}/models gave no answer within {}s", PROBE_TIMEOUT.as_secs())),
    }
}

/// Who actually needs Codex's own OpenAI login: the Solo default model and the default pattern's
/// roles that run on the built-in `openai` provider. Empty when everything goes through custom
/// providers (their keys are checked per provider below), so "Not logged in" is not a failure.
/// An alias that is not in /models needs no login — it needs fixing there (a run's preflight
/// says so), and nothing defaults to Codex any more.
fn codex_account_needed(s: &config::Settings, r: &config::Registry) -> Vec<String> {
    let mut v = vec![];
    let on_codex_account = |alias: &str| r.get(alias).is_some_and(|m| !m.is_custom_provider() && r.backend_of(m) == config::ProviderKind::Codex);
    if on_codex_account(&s.default_model) {
        v.push(format!("the default model `{}`", s.default_model));
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    if let Ok(p) = engine::pattern::Pattern::load(&s.default_pattern, &cwd) {
        let roles: Vec<String> = p.ordered_roles().into_iter().filter(|(_, role)| on_codex_account(&role.model)).map(|(n, _)| n).collect();
        if !roles.is_empty() {
            v.push(format!("pattern `{}` roles {}", p.name, roles.join(", ")));
        }
    }
    v
}

/// The doctor line for one CLI probe (`<cmd0> --version`, 5s cap), as (healthy, text) so the caller
/// keeps its own ✓/✗ glyph: the version when it runs, the file at fault when it doesn't, and
/// `not_found_hint` (the install advice, which differs per CLI) only when PATH holds no such name.
fn probe_cli(cmd0: &str, not_found_hint: &str) -> (bool, String) {
    match version_with_timeout(cmd0, Duration::from_secs(5)) {
        Some(Ok(v)) => (true, v),
        Some(Err(e)) => (false, e),
        None => (false, format!("`{cmd0}` not found on PATH{not_found_hint}")),
    }
}

/// Why a spawn failed, in terms of a file the user can act on — `None` when nothing of that name
/// is on PATH. A bare "Permission denied (os error 13)" names nothing, yet the cause is always a
/// specific file: a directory shadowing the binary, or a wrapper script that lost its exec bit.
fn spawn_failure(cmd0: &str, e: &std::io::Error) -> Option<String> {
    let path = util::which(cmd0)?;
    Some(util::exec_problem(&path).unwrap_or_else(|| format!("{}: {e}", path.display())))
}

/// Runs `<cmd0> --version` on a thread with a hard timeout: `Some(Ok(version))` on success,
/// `Some(Err(reason))` if it ran but failed or timed out, `None` if the binary isn't even there
/// (WP10.5 — a `claude` under `subscription` auth can hang indefinitely in some sandboxes, §0.3).
fn version_with_timeout(cmd0: &str, timeout: Duration) -> Option<Result<String, String>> {
    let name = cmd0.to_string();
    let spawned = name.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(std::process::Command::new(&spawned).arg("--version").output());
    });
    match rx.recv_timeout(timeout) {
        Ok(Ok(o)) if o.status.success() => Some(Ok(String::from_utf8_lossy(&o.stdout).trim().to_string())),
        Ok(Ok(o)) => Some(Err(format!("--version exited {:?}", o.status.code()))),
        // Any spawn error resolves through PATH first: ENOENT can equally mean "the wrapper is
        // right there but its `#!` interpreter is gone", and EACCES never says which file it is.
        Ok(Err(e)) => match spawn_failure(&name, &e) {
            Some(why) => Some(Err(why)),
            None => (e.kind() != std::io::ErrorKind::NotFound).then(|| Err(e.to_string())),
        },
        Err(_) => Some(Err(format!("timed out after {}s", timeout.as_secs()))),
    }
}

/// `claude auth status`, if the subcommand exists and says anything (§10.5: "if that subcommand
/// exists else skip"). Real Claude Code answers JSON (`{"loggedIn":…,"authMethod":…,…}`) — pull
/// out the two fields that matter rather than dumping raw `{`; fall back to the first line for
/// whatever a future/older CLI shape prints instead.
fn claude_auth_status(cmd0: &str) -> Option<String> {
    let o = std::process::Command::new(cmd0).args(["auth", "status"]).output().ok()?;
    let t = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(t.trim()) {
        if let Some(logged_in) = v.get("loggedIn").and_then(|b| b.as_bool()) {
            let method = v.get("authMethod").and_then(|m| m.as_str()).unwrap_or("");
            return Some(if logged_in { format!("logged in ({method})") } else { "not logged in".into() });
        }
    }
    let first = t.lines().next().unwrap_or("").trim().to_string();
    (!first.is_empty()).then_some(first)
}

/// `--resume-last`: the newest unfinished run of the project in `here` (cwd / --cwd) whose
/// directory still exists — never another project's: that run belongs to whoever is working
/// there, and may well be open. `None` = `--demo`, where it is the last demo run (each demo has
/// its own temp repo, so "this project" would never match).
fn resume_last_pick(all: Vec<engine::state::RunSummary>, here: Option<&std::path::Path>) -> Option<engine::state::RunSummary> {
    let key = here.map(engine::state::project_key);
    all.into_iter().filter(|r| r.unfinished()).find(|r| {
        let Some(p) = r.project().filter(|p| p.is_dir()) else { return false };
        match &key {
            Some(k) => r.project_key == *k,
            None => p.to_string_lossy().contains("mantra-demo-"),
        }
    })
}

/// One owner per run: a run another Mantra has open right now (`owner.json`, a live pid on this
/// host) is refused before anything starts, unless the user says the check is wrong
/// (`--take-over`, which clears the claim). A dead owner is a crash and resumes as usual; the
/// TUI's `/runs` and the web `run_resume` refuse the same way in `App::resume_run`.
fn claim_for_resume(r: &engine::state::RunSummary, take_over: bool) -> Result<(), String> {
    let Some(owner) = engine::state::live_owner(&r.dir) else { return Ok(()) };
    if !take_over {
        return Err(owner.refusal(&r.id));
    }
    eprintln!("mantra: taking run {} over from pid {} on {}", r.id, owner.pid, owner.hostname);
    engine::state::Owner::clear(&r.dir);
    Ok(())
}

#[cfg(test)]
mod resume_tests {
    use super::*;
    use engine::state::{list_in, project_key, Owner, RunState};

    /// A runs root with one unfinished run per project in `projects` (newest last).
    fn runs_root(tag: &str, projects: &[&std::path::Path]) -> PathBuf {
        let root = std::env::temp_dir().join(format!("mantra-resume-last-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for (i, p) in projects.iter().enumerate() {
            let dir = root.join(project_key(p)).join(format!("{i}-run"));
            RunState { format: engine::state::FORMAT, id: format!("{i}-run"), project: p.to_path_buf(), updated_unix: 100 + i as u64, ..Default::default() }.save(&dir);
        }
        root
    }

    /// `--resume-last` in project A never picks project B's run, however much newer it is; with
    /// --demo it is the last demo run, wherever the demo repo is.
    #[test]
    fn resume_last_only_looks_at_this_projects_runs() {
        let base = std::env::temp_dir().join(format!("mantra-resume-projects-{}", std::process::id()));
        let (a, b, demo) = (base.join("alpha"), base.join("beta"), base.join("mantra-demo-x"));
        for d in [&a, &b, &demo] {
            std::fs::create_dir_all(d).unwrap();
        }
        let root = runs_root("pick", &[&a, &b, &demo]);
        assert_eq!(resume_last_pick(list_in(&root), Some(&a)).map(|r| r.id), Some("0-run".into()), "A's own run, not B's newer one");
        assert_eq!(resume_last_pick(list_in(&root), Some(&b)).map(|r| r.id), Some("1-run".into()));
        assert_eq!(resume_last_pick(list_in(&root), None).map(|r| r.id), Some("2-run".into()), "--demo: the last demo run");
        assert!(resume_last_pick(list_in(&root), Some(&base)).is_none(), "a project with no run of its own resumes nothing");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A resume of a run a live Mantra owns is refused (and names the way around it); a dead
    /// owner is a crash and goes ahead; `--take-over` clears a live claim.
    #[test]
    fn a_resume_is_refused_while_the_owner_lives_and_take_over_clears_it() {
        let project = std::env::temp_dir().join(format!("mantra-resume-owned-{}", std::process::id()));
        std::fs::create_dir_all(&project).unwrap();
        let root = runs_root("owned", &[&project]);
        let r = list_in(&root).remove(0);
        assert!(claim_for_resume(&r, false).is_ok(), "no owner file: nothing to refuse");
        // our own pid in the file is a recycled pid's leftover (a container restart), not a claim
        Owner::this_process().write(&r.dir);
        assert!(claim_for_resume(&r, false).is_ok(), "this process is no other Mantra");
        // another live process holds the run
        let stranger = engine::state::tests::Stranger::spawn();
        stranger.owner().write(&r.dir);
        let e = claim_for_resume(&r, false).unwrap_err();
        assert!(e.contains("open in another Mantra") && e.contains("--take-over"), "{e}");
        assert!(Owner::load(&r.dir).is_some(), "a refusal leaves the claim alone");
        assert!(claim_for_resume(&r, true).is_ok());
        assert!(Owner::load(&r.dir).is_none(), "--take-over clears it");
        // a crash: the owner's pid is gone
        Owner { pid: 4_000_000_000, ..Owner::this_process() }.write(&r.dir);
        assert!(claim_for_resume(&r, false).is_ok(), "a dead owner does not block the resume");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&project);
    }

    /// The TUI's `/runs` and the web `run_resume` go through `App::resume_run`: it refuses a
    /// live owner the same way, with a toast, and opens nothing.
    #[test]
    fn the_app_refuses_to_resume_a_run_another_mantra_has_open() {
        let project = std::env::temp_dir().join(format!("mantra-resume-app-{}", std::process::id()));
        std::fs::create_dir_all(&project).unwrap();
        let root = runs_root("app", &[&project]);
        let r = list_in(&root).remove(0);
        let stranger = engine::state::tests::Stranger::spawn();
        stranger.owner().write(&r.dir);
        let mut app = crate::web::snapshot::tests::test_app();
        app.resume_run(r.clone());
        assert!(app.run.is_none(), "nothing was opened");
        let toast = app.toast.as_ref().map(|t| t.0.clone()).unwrap_or_default();
        assert!(toast.contains("open in another Mantra"), "{toast}");
        assert!(Owner::load(&r.dir).is_some(), "the owner's claim stands");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&project);
    }
}

#[cfg(test)]
mod doctor_tests {
    use super::*;

    /// A missing bija stops the start when Solo runs on it — with what to do, never a silent demo
    /// — and is no reason to stop when Solo's model is on another backend; Codex is never asked for.
    #[test]
    fn a_missing_bija_stops_the_start_only_when_solo_needs_it() {
        let mut reg = config::Registry::defaults();
        let s = config::Settings::default();
        assert_eq!(startup_problem(config::BijaFound::Missing, &s, &reg), None, "setup without models does not require Bija");
        reg.providers.push(config::bija_default_provider());
        reg.models.push(config::bija_default_model_entries("bija").into_iter().find(|m| m.alias == "sol").unwrap());
        let why = startup_problem(config::BijaFound::Missing, &s, &reg).expect("Solo is on bija");
        assert!(why.contains("bija") && why.contains("$MANTRA_BIJA") && why.contains("Reinstall"), "{why}");
        for found in [config::BijaFound::Configured, config::BijaFound::BesideMantra, config::BijaFound::Env] {
            assert_eq!(startup_problem(found, &s, &reg), None, "{found:?}");
        }
        let mut with_codex = reg.clone();
        with_codex.add_codex_model("sol-codex", "account-model").unwrap();
        let on_codex = config::Settings { default_model: "sol-codex".into(), ..Default::default() };
        assert_eq!(startup_problem(config::BijaFound::Missing, &on_codex, &with_codex), None);
    }

    /// Who needs Codex's own login: nobody on a fresh install; the default model or a pattern role
    /// once they are on Codex's account — and never an alias /models does not have.
    #[test]
    fn codex_login_is_needed_only_by_what_runs_on_codex() {
        // under a home of its own: the default pattern is read from `$MANTRA_HOME/patterns`
        crate::web::snapshot::tests::with_mantra_home(|| {
            let mut reg = config::Registry::defaults();
            let s = config::Settings::default();
            assert!(codex_account_needed(&s, &reg).is_empty(), "{:?}", codex_account_needed(&s, &reg));
            reg.add_codex_model("sol-codex", "account-model").unwrap();
            reg.add_codex_model("astra-codex", "another-account-model").unwrap();
            assert!(codex_account_needed(&s, &reg).is_empty(), "added, but nothing uses it");
            let on_codex = config::Settings { default_model: "sol-codex".into(), ..Default::default() };
            assert_eq!(codex_account_needed(&on_codex, &reg), vec!["the default model `sol-codex`".to_string()]);
            let mut p = engine::pattern::Pattern::builtin();
            p.roles.get_mut("planner").unwrap().model = "astra-codex".into();
            p.roles.get_mut("qa").unwrap().model = "no-such-alias".into();
            p.save().unwrap();
            assert_eq!(codex_account_needed(&s, &reg), vec![format!("pattern `{}` roles planner", engine::pattern::NAME_DEFAULT)]);
            assert_eq!(codex_alias_of(&reg, "sol").as_deref(), Some("sol-codex"));
            assert_eq!(codex_alias_of(&config::Registry::defaults(), "sol"), None);
        });
    }

    /// The demo's bija provider points at the mock, keyless — a real key never goes there — and a
    /// models.toml from before bija gets the provider and the default models, in memory.
    #[test]
    fn the_demo_points_bija_at_the_mock_and_adds_what_is_missing() {
        let mut reg = config::Registry::defaults();
        if let Some(p) = reg.providers.iter_mut().find(|p| p.id == "bija") {
            p.api_key = Some("sk-real".into());
        }
        point_bija_at_mock(&mut reg, 4242);
        let p = reg.providers.iter().find(|p| p.id == "bija").expect("the bija provider");
        assert_eq!((p.base_url.as_str(), p.env_key.as_str(), p.api_key.as_deref()), ("http://127.0.0.1:4242/v1", "", None));
        assert_eq!(reg.models.iter().filter(|m| m.alias == "sol").count(), 1, "nothing doubled");
        let mut old = config::Registry { models: vec![config::ModelEntry { alias: "sol".into(), model: "gpt-5.6-sol".into(), ..Default::default() }], providers: vec![] };
        point_bija_at_mock(&mut old, 7);
        assert_eq!(old.backend_of(&old.resolve("luna")), config::ProviderKind::Bija);
        assert_eq!(old.backend_of(&old.resolve("sol")), config::ProviderKind::Codex, "the file's own `sol` stays");
        assert_eq!(old.provider_of("astra").map(|p| p.base_url.as_str()), Some("http://127.0.0.1:7/v1"));
        assert_eq!(old.alias_problem("terra"), None);
        assert!(old.preflight(&engine::pattern::Pattern::builtin()).is_empty());
    }

    /// A Bija provider is checked like any custom one, its wire included, before any request.
    #[test]
    fn doctor_refuses_a_bija_provider_with_an_unknown_wire_before_any_request() {
        let mut r = config::Registry::defaults();
        let mut p = config::bija_default_provider();
        p.wire = "grpc".into();
        r.providers.push(p);
        r.models.push(config::bija_default_model_entries("bija").into_iter().find(|m| m.alias == "sol").unwrap());
        let (ok, row) = provider_row(&config::Settings::default(), &r);
        assert_eq!(ok, Some(false));
        assert!(row.contains("wire 'grpc'") && row.contains("/models to fix"), "{row}");
    }

    /// The provider row never sends the key to a placeholder: the `n` row with a key configured
    /// is refused before `GET /models` is built (doctor used to be the one path around the draft rules).
    #[test]
    fn doctor_refuses_a_placeholder_provider_before_any_request() {
        let mut r = config::Registry { models: vec![], providers: vec![] };
        r.providers.push(config::ProviderEntry { id: "myprovider".into(), base_url: "https://example.com/v1".into(), api_key: Some("sk-1".into()), ..Default::default() });
        r.models.push(config::ModelEntry { alias: "testing".into(), provider: "myprovider".into(), model: "testing".into(), ..Default::default() });
        r.models.push(config::ModelEntry { alias: "gpt".into(), model: "gpt-5".into(), ..Default::default() });
        let s = config::Settings { default_model: "testing".into(), ..Default::default() };
        let (ok, row) = provider_row(&s, &r);
        assert_eq!(ok, Some(false));
        assert!(row.contains("example.com placeholder") && row.contains("/models to fix"), "{row}");
        // a malformed URL the same way; Codex's own account has nothing to send
        r.providers[0].base_url = "api.riti.dev/v1".into();
        let (ok, row) = provider_row(&s, &r);
        assert!(ok == Some(false) && row.contains("not a URL"), "{row}");
        let s = config::Settings { default_model: "gpt".into(), ..Default::default() };
        assert_eq!(provider_row(&s, &r).0, None);
    }
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Cli> {
        parse_cli(&args.iter().map(|a| a.to_string()).collect::<Vec<_>>()).map(|c| c.expect("a session, not a subcommand"))
    }

    /// `--parallel N` is any pattern's run with N at once in every worker class, before or after
    /// `run` and the goal; outside 1..=128 or without a goal it is a usage error. `--swarm` is
    /// gone, and says what replaced it.
    #[test]
    fn parallel_parses_and_checks_its_range() {
        for args in [&["run", "add a parser", "--parallel", "6"][..], &["run", "--parallel", "6", "add a parser"], &["--parallel", "6", "run", "add a parser"]] {
            let c = parse(args).unwrap_or_else(|e| panic!("{args:?}: {e}"));
            assert_eq!((c.run_goal.as_deref(), c.parallel, c.pattern.as_deref()), (Some("add a parser"), Some(6), None), "{args:?}");
        }
        let c = parse(&["run", "add a parser", "--pattern", "mantra-lean", "--parallel", "24"]).unwrap();
        assert_eq!((c.parallel, c.pattern.as_deref()), (Some(24), Some("mantra-lean")), "any pattern");
        assert_eq!(parse(&["run", "g", "--parallel", "128"]).unwrap().parallel, Some(128));
        assert_eq!(parse(&["run", "g", "--parallel", "1"]).unwrap().parallel, Some(1));
        for (args, words) in [
            (&["run", "g", "--parallel", "0"][..], "from 1 to 128"),
            (&["--parallel", "129", "run", "g"], "from 1 to 128"),
            (&["run", "g", "--parallel", "lots"], "--parallel"),
            (&["--parallel", "8"], "--parallel needs a run"),
            (&["--swarm", "8", "g"], "--swarm is gone"),
            (&["run", "g", "--swarm", "8"], "--parallel N"),
            (&["run"], "run needs a goal"),
            (&["just words"], "unknown argument just words"),
            (&["run", "g", "h"], "unknown argument h"),
        ] {
            let e = parse(args).err().unwrap_or_else(|| panic!("{args:?} must be refused")).to_string();
            assert!(e.contains(words), "{args:?}: {e}");
        }
        // after `run`, any word is the goal — a subcommand's name or one that starts with a dash
        // too — as `run <goal>` always took it; only the options keep their meaning
        for (args, goal) in [
            (&["run", "doctor"][..], "doctor"),
            (&["run", "help"], "help"),
            (&["run", "runs"], "runs"),
            (&["run", "experimental"], "experimental"),
            (&["run", "--dry-run flag for deploy"], "--dry-run flag for deploy"),
            (&["run", "--parallel", "8", "-x is broken"], "-x is broken"),
            (&["run", "--pattern", "mantra-lean", "help"], "help"),
        ] {
            let c = parse(args).unwrap_or_else(|e| panic!("{args:?}: {e}"));
            assert_eq!(c.run_goal.as_deref(), Some(goal), "{args:?}");
        }
        let e = parse(&["run", "g", "--not-an-option"]).err().expect("one goal, then only options").to_string();
        assert!(e.contains("unknown argument --not-an-option"), "{e}");
        assert!(parse_cli(&["run".to_string(), "--help".to_string()]).unwrap().is_none(), "--help is still help");
        // what was there before is parsed as before
        let c = parse(&["run", "add a parser", "--pattern", "mantra-lean"]).unwrap();
        assert_eq!((c.run_goal.as_deref(), c.parallel, c.pattern.as_deref()), (Some("add a parser"), None, Some("mantra-lean")));
        let c = parse(&["--demo", "--cwd", "/tmp"]).unwrap();
        assert!(c.run_goal.is_none() && c.parallel.is_none() && c.demo);
    }

    /// `mantra experimental` lists, enables and disables in settings.toml; an unknown name or a
    /// bad subcommand is exit 2, with the list.
    #[test]
    fn experimental_enable_disable_and_list() {
        crate::web::snapshot::tests::with_mantra_home(|| {
            let s = |v: &[&str]| v.iter().map(|a| a.to_string()).collect::<Vec<_>>();
            let out = experimental_cmd(&s(&[])).unwrap();
            assert!(out.contains("akasha") && out.contains("off") && out.contains("shared notes"), "{out}");
            assert_eq!(experimental_cmd(&s(&["list"])).unwrap(), out);
            let on = experimental_cmd(&s(&["enable", "akasha"])).unwrap();
            assert!(on.contains("akasha on") && on.contains("settings.toml"), "{on}");
            assert!(config::Settings::load().experimental_on(config::EXPERIMENTAL_AKASHA));
            assert!(experimental_cmd(&s(&["list"])).unwrap().lines().any(|l| l.contains("akasha") && l.contains(" on ")));
            // enabling twice keeps one entry
            experimental_cmd(&s(&["enable", "akasha"])).unwrap();
            assert_eq!(config::Settings::load().experimental, vec!["akasha".to_string()]);
            experimental_cmd(&s(&["disable", "akasha"])).unwrap();
            assert!(!config::Settings::load().experimental_on(config::EXPERIMENTAL_AKASHA));
            let (code, e) = experimental_cmd(&s(&["enable", "warp"])).unwrap_err();
            assert!(code == 2 && e.contains("'warp'") && e.contains("akasha"), "{code} {e}");
            for bad in [&["enable"][..], &["toggle", "akasha"], &["enable", "akasha", "now"]] {
                assert_eq!(experimental_cmd(&s(bad)).unwrap_err().0, 2, "{bad:?}");
            }
        });
    }

    /// The web password leaves the environment as it is read, so no agent, shell command or MCP
    /// server started later inherits it.
    #[test]
    fn the_web_password_is_taken_out_of_the_environment() {
        std::env::set_var("MANTRA_WEB_PASSWORD", "hunter2");
        assert_eq!(take_web_password_env().as_deref(), Some("hunter2"));
        assert!(std::env::var_os("MANTRA_WEB_PASSWORD").is_none());
        std::env::set_var("MANTRA_WEB_PASSWORD", "");
        assert_eq!(take_web_password_env(), None, "empty is none");
        assert!(std::env::var_os("MANTRA_WEB_PASSWORD").is_none());
    }

    /// `mantra mcp` adds, lists, switches and removes servers in settings.toml, refusing names
    /// that would not survive as a TOML key or a tool prefix; `test` starts the stand-in server
    /// (`mantra mock-mcp`, run from this test binary — `mock_mcp::tests::test_server`) with the
    /// `--env` it was added with, and lists its tools; one that dies is exit 1 with its stderr.
    /// `mantra mcp test` prints a tool's first sentence, whole, even from a hard-wrapped description.
    #[test]
    fn a_tools_line_is_its_first_sentence_not_its_first_line() {
        let wrapped = "Check the connected Blender: its version, whether the addon matches this server, and which\n  features are on. Call it first.\n\nArgs: none";
        assert_eq!(tool_about(wrapped), "Check the connected Blender: its version, whether the addon matches this server, and which features are on.");
        assert_eq!(tool_about("Search a library of existing assets. The sources:\n- Poly Haven"), "Search a library of existing assets.");
        assert_eq!(tool_about("\n  Run Python in Blender (bpy). Whatever it prints is returned."), "Run Python in Blender (bpy).");
        assert_eq!(tool_about("Version 1.2 of the scene"), "Version 1.2 of the scene");
        assert_eq!(tool_about(""), "");
    }

    #[test]
    fn mcp_add_list_enable_test_and_remove() {
        crate::web::snapshot::tests::with_mantra_home(|| {
            let s = |v: &[&str]| v.iter().map(|a| a.to_string()).collect::<Vec<_>>();
            let out = mcp_cmd(&s(&[])).unwrap();
            assert!(out.contains("no MCP servers") && out.contains("uvx blender-mcp"), "{out}");
            let mock = crate::mock_mcp::tests::test_server();
            let mut add = vec!["add", "demo"];
            let env = mock.env.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>();
            for kv in &env {
                add.extend(["--env", kv.as_str()]);
            }
            add.push("--");
            add.push(&mock.command);
            add.extend(mock.args.iter().map(String::as_str));
            let added = mcp_cmd(&s(&add)).unwrap();
            assert!(added.contains("added demo") && added.contains("Solo agents get it"), "{added}");
            assert!(added.contains("(0600)") != mock.env.is_empty(), "an env is said to be a secret: {added}");
            let saved = config::Settings::load_on_disk().unwrap();
            assert_eq!(saved.mcp_servers["demo"], mock, "command, args and env as given");
            assert_eq!(mcp_cmd(&s(&add)).unwrap_err().0, 2, "a second add of the same name is refused");
            // without `--` too, and an argument with a space shows quoted
            mcp_cmd(&s(&["add", "files", "npx", "-y", "@modelcontextprotocol/server-filesystem", "/tmp/a b"])).unwrap();
            let list = mcp_cmd(&s(&["list"])).unwrap();
            assert!(list.lines().any(|l| l.contains("files") && l.contains("enabled") && l.contains("solo: yes") && l.contains("'/tmp/a b'")), "{list}");

            let tested = mcp_cmd(&s(&["test", "demo"])).unwrap();
            assert!(tested.contains("connected: mantra-mock-mcp") && tested.contains("2 tools:"), "{tested}");
            assert!(tested.lines().any(|l| l.trim_start().starts_with("scene_info") && l.contains("objects")), "{tested}");
            assert!(tested.lines().any(|l| l.trim_start().starts_with("render_preview")), "{tested}");

            assert!(mcp_cmd(&s(&["disable", "files"])).unwrap().contains("disabled"));
            let list = mcp_cmd(&s(&[])).unwrap();
            assert!(list.lines().any(|l| l.contains("files") && l.contains("disabled") && l.contains("solo: no")), "{list}");
            assert!(config::Settings::load_on_disk().unwrap().mcp_for(&["*".to_string()]).iter().all(|(n, _)| n != "files"));
            mcp_cmd(&s(&["enable", "files"])).unwrap();
            assert!(config::Settings::load_on_disk().unwrap().mcp_servers["files"].enabled);
            assert!(mcp_cmd(&s(&["remove", "files"])).unwrap().contains("removed files"));
            assert!(!config::Settings::load_on_disk().unwrap().mcp_servers.contains_key("files"));

            // a server that dies before answering: exit 1, with what it said
            let broken = config::McpServer { command: "sh".into(), args: s(&["-c", "echo 'blender-mcp: cannot reach Blender on :9876' >&2; exit 3"]), ..Default::default() };
            let mut cfg = config::Settings::load_on_disk().unwrap();
            cfg.mcp_servers.insert("broken".into(), broken);
            cfg.save().unwrap();
            let (code, e) = mcp_cmd(&s(&["test", "broken"])).unwrap_err();
            assert!(code == 1 && e.contains("broken:") && e.contains("cannot reach Blender"), "{code} {e}");
            // a name typed into settings.toml that `add` would refuse is listed as never given
            cfg.mcp_servers.insert("blender.v2".into(), config::McpServer { command: "uvx".into(), ..Default::default() });
            cfg.save().unwrap();
            let list = mcp_cmd(&s(&[])).unwrap();
            assert!(list.contains("↳ never given to an agent: 'blender.v2'"), "{list}");
            cfg.mcp_servers.remove("blender.v2");
            cfg.save().unwrap();

            for (bad, code, words) in [
                (&["add", "a/b", "--", "x"][..], 2, "letters"),
                (&["add", "mantra", "--", "x"], 2, "Mantra's own"),
                (&["add", "x"], 2, "usage"),
                (&["add", "--", "uvx", "blender-mcp"], 2, "cannot start with -"),
                (&["add", "x", "--env"], 2, "--env is not a command"),
                (&["add", "x", "--env", "A=1"], 2, "usage"),
                (&["add", "x", "--env", "NOEQUALS", "--", "y"], 2, "KEY=VALUE"),
                (&["remove", "nope"], 2, "no MCP server named nope"),
                (&["test", "nope"], 2, "there is: broken, demo"),
                (&["enable"], 2, "usage"),
                (&["frobnicate", "x"], 2, "usage"),
            ] {
                let (c, e) = mcp_cmd(&s(bad)).unwrap_err();
                assert!(c == code && e.contains(words), "{bad:?}: {c} {e}");
            }
        });
    }

    /// `mantra mcp test` exits 1 with a one-line reason for a server that is gone before it
    /// answers and for one that never says a word (`--timeout` keeps the wait short); a bad
    /// `--timeout` is a usage error (2), and the silent server does not outlive the test.
    #[test]
    fn mcp_test_exits_1_for_a_server_that_dies_or_never_answers() {
        crate::web::snapshot::tests::with_mantra_home(|| {
            let s = |v: &[&str]| v.iter().map(|a| a.to_string()).collect::<Vec<_>>();
            let marker = format!("{}", 60_000 + std::process::id() % 1000);
            let mut cfg = config::Settings::load_on_disk().unwrap();
            cfg.mcp_servers.insert("dead".into(), config::McpServer { command: "false".into(), ..Default::default() });
            cfg.mcp_servers.insert("mute".into(), config::McpServer { command: "sleep".into(), args: vec![marker.clone()], ..Default::default() });
            cfg.save().unwrap();
            let (code, e) = mcp_cmd(&s(&["test", "dead"])).unwrap_err();
            assert!(code == 1 && e.starts_with("dead: ") && e.lines().count() == 1, "{code} {e}");
            let started = Instant::now();
            let (code, e) = mcp_cmd(&s(&["test", "mute", "--timeout", "1"])).unwrap_err();
            assert_eq!((code, e.as_str()), (1, "mute: no answer within 1 s"));
            assert!(started.elapsed() < Duration::from_secs(10), "the budget, not the server, ended it: {:?}", started.elapsed());
            let alive = std::process::Command::new("pgrep").args(["-f", &format!("^sleep {marker}$")]).output().map(|o| o.status.success()).unwrap_or(false);
            assert!(!alive, "the silent server was killed with the test");
            for bad in ["0", "soon", "99999"] {
                let (code, e) = mcp_cmd(&s(&["test", "mute", "--timeout", bad])).unwrap_err();
                assert!(code == 2 && e.contains("--timeout takes whole seconds"), "{bad}: {code} {e}");
            }
            assert_eq!(mcp_cmd(&s(&["test", "mute", "--later"])).unwrap_err().0, 2);
        });
    }
}
