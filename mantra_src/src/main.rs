//! Mantra — a terminal UI for running and orchestrating Codex agents.

mod agent;
mod app;
mod config;
mod discover;
mod engine;
mod hub;
mod mcp_bridge;
mod mock;
mod mock_claude;
mod rpc;
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

const HELP: &str = "mantra — one terminal for Codex agents

USAGE
  mantra [--cwd DIR] [--demo]           open the TUI (Solo mode)
  mantra run \"<goal>\" [--pattern NAME]  open straight into a Mandala run
  mantra runs                            list every run (all projects): id, stage, when, goal
  mantra runs resume <id>                reopen a run where it stopped (id prefix is enough)
  mantra runs delete <id> [--yes]        remove a run: its branches, worktrees and journal
  mantra doctor                          check codex, login, terminal, config
  mantra --version

  mantra --web [ADDR:PORT] [--web-password PW] [--web-tls]   web UI (phone + desktop) on this machine/LAN
  mantra --remote [URL]                                     reach this session from anywhere via a relay (E2EE)
  mantra --headless --web ... / --remote                     the same without a terminal

OPTIONS
  --cwd DIR       project directory (default: current directory)
  --demo          simulated agents (no API calls, no cost) in a throwaway demo repo
  --pattern NAME  pattern for new runs (default from settings.toml)
  --resume-last   reopen the most recent unfinished run (with --demo: the last demo run)
  --no-sandbox-check  start `mantra run` without asking when Codex's sandbox can't work here
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
  ~/.mantra/settings.toml          ui, codex command, defaults   ($MANTRA_HOME overrides the dir)
  ~/.mantra/models.toml            models, context windows, efforts, providers
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
    no_sandbox_check: bool,
    web: web::CliWeb,
}

fn parse_args() -> Result<Option<Cli>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut cli = Cli { cwd: None, demo: false, run_goal: None, pattern: None, snapshot: None, size: (120, 36), resume: None, resume_last: false, no_sandbox_check: false, web: web::CliWeb::default() };
    let mut i = 0;
    while i < args.len() {
        let a = args[i].clone();
        let mut next = || -> Result<String> {
            i += 1;
            args.get(i).cloned().ok_or_else(|| anyhow::anyhow!("{a} needs a value"))
        };
        match a.as_str() {
            "-h" | "--help" | "help" => {
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
            "run" => cli.run_goal = Some(next()?),
            "--resume-last" => cli.resume_last = true,
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
            "runs" => match next().ok().as_deref() {
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
            "doctor" => {
                doctor();
                return Ok(None);
            }
            other => anyhow::bail!("unknown argument {other} (see mantra --help)"),
        }
        i += 1;
    }
    Ok(Some(cli))
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
    util::init_log(&config::log_path());
    mlog!("mantra {} starting", env!("CARGO_PKG_VERSION"));
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().expect("runtime");
    let res = rt.block_on(async_main(cli));
    rt.shutdown_timeout(Duration::from_millis(500));
    if let Err(e) = res {
        eprintln!("mantra: {e:#}");
        std::process::exit(1);
    }
}

fn codex_available(cmd: &[String]) -> bool {
    cmd.first().map(|p| std::process::Command::new(p).arg("--version").stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().map(|s| s.success()).unwrap_or(false)).unwrap_or(false)
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

async fn async_main(mut cli: Cli) -> Result<()> {
    let mut settings = config::Settings::load();
    let mut registry = config::Registry::load();
    // Validate the web flags before anything else starts (a bad address is a usage error).
    let env_pw = std::env::var("MANTRA_WEB_PASSWORD").ok().filter(|p| !p.is_empty());
    let web_cfg = web::WebConfig::resolve(&cli.web, &settings.web, env_pw)?;
    let headless = cli.web.headless;
    if headless {
        // Nobody can answer a question on stdin.
        cli.no_sandbox_check = true;
    }
    let mut demo = cli.demo;
    if !demo && !codex_available(&settings.codex_command) {
        eprintln!("mantra: `{}` not found — starting in DEMO mode (simulated agents).\n        Install Codex with `npm i -g @openai/codex`, then `codex login`.", settings.codex_command.join(" "));
        std::thread::sleep(Duration::from_millis(1500));
        demo = true;
    }
    // A resumed run brings its own project directory (it was saved absolute), so `mantra runs
    // resume <id>` works from anywhere — and with --demo from the previous demo's temp repo.
    let resume = match (&cli.resume, cli.resume_last) {
        (Some(id), _) => Some(engine::state::find(id).map_err(|e| anyhow::anyhow!(e))?),
        (None, true) => {
            let pick = engine::state::list_all().into_iter().find(|r| r.unfinished() && r.project().map(|p| p.is_dir() && (!demo || p.to_string_lossy().contains("mantra-demo-"))).unwrap_or(false));
            Some(pick.ok_or_else(|| anyhow::anyhow!("no unfinished run to resume (mantra runs lists them)"))?)
        }
        _ => None,
    };
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
        // WP10.6: mirror the Codex override above for the Claude Code backend. Injected in memory
        // (never saved) regardless of `config::claude_available()`, so `--demo --pattern
        // mantra-default-claude` needs no real `claude` install: the demo's whole point is zero
        // external dependencies.
        settings.claude_command = vec![exe.to_string_lossy().to_string(), "mock-claude".into()];
        if !registry.providers.iter().any(|p| p.kind == config::ProviderKind::ClaudeCode) {
            registry.providers.push(config::ProviderEntry { id: "claude".into(), name: "Claude Code".into(), kind: config::ProviderKind::ClaudeCode, auth: "subscription".into(), ..Default::default() });
            registry.models.extend(config::claude_default_model_entries("claude"));
        }
    }
    // L1: on a Linux box where unprivileged user namespaces are off, every worker command dies
    // in bubblewrap. Say so once, up front — and don't start a run on it without a nod.
    // (MANTRA_SANDBOX_WARNING=<text> forces the notice — stress.sh renders it in demo mode.)
    let sandbox_warning = std::env::var("MANTRA_SANDBOX_WARNING").ok().filter(|w| !w.is_empty()).or_else(|| if demo || cli.snapshot.is_some() { None } else { util::sandbox_probe().err() });
    if let (Some(w), true, false) = (&sandbox_warning, cli.run_goal.is_some() || cli.resume.is_some() || cli.resume_last, cli.no_sandbox_check) {
        eprintln!("mantra: sandbox check failed — {w}\n");
        eprint!("start anyway? workers' commands will fail unless the roles use sandbox = \"danger-full-access\" [y/N] ");
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
        if !matches!(line.trim().to_lowercase().as_str(), "y" | "yes") {
            println!("not started. (mantra --no-sandbox-check skips this question)");
            return Ok(());
        }
    }
    ui::theme::init(&settings);
    let (tx, mut rx) = mpsc::unbounded_channel::<AppEvent>();
    // (Also with --snapshot, so the /web and /remote overlays can be rendered headlessly.)
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
    let hub = hub::Hub::new(settings.codex_command.clone(), settings.claude_command.clone(), hub_tx, enable_bridge);
    let mut app = App::new(settings, registry, project, hub, tx.clone(), demo);
    app.web = web.as_ref().map(|w| w.link());
    if let Some(p) = cli.pattern {
        app.pattern_name = p;
    }
    app.start_solo();
    if let Some(w) = sandbox_warning {
        app.set_sandbox_warning(w);
    }
    // Web::start already logged these; the toast is for the person looking at the TUI.
    if let Some(w) = web.as_ref().map(|w| w.cfg.warnings()).filter(|w| !w.is_empty()) {
        app.toast(w.join(" · "), crate::agent::Level::Warn);
    }
    if let Some(goal) = &cli.run_goal {
        app.start_run(goal);
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
        app.shutdown();
        tokio::time::sleep(Duration::from_millis(300)).await;
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
        app.toast(format!("web UI on {u} — /web for details"), agent::Level::Info);
    }
    let res = event_loop(Some(&mut terminal), &mut app, &mut rx, &mut web, None).await;
    stop.store(true, Ordering::Relaxed);
    if let Some(w) = web.as_mut() {
        w.shutdown();
    }
    app.shutdown();
    restore_terminal(kbd);
    tokio::time::sleep(Duration::from_millis(150)).await;
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
    println!("{:<28} {:<22} {:<18} {:>9}  goal", "run", "project", "stage", "updated");
    for r in &all {
        let project = r.project().and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string())).unwrap_or_else(|| r.project_key.rsplit_once('-').map(|(a, _)| a.to_string()).unwrap_or_else(|| r.project_key.clone()));
        println!("{:<28} {:<22} {:<18} {:>9}  {}", util::trunc(&r.id, 28), util::trunc(&project, 22), util::trunc(&r.stage_label(), 18), engine::state::fmt_ago(r.updated_unix), util::trunc(&r.brief(), 60));
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
    let where_ = r.project().map(|p| p.display().to_string()).unwrap_or_else(|| r.project_key.clone());
    println!("run {} ({}) — {}\n  {}", r.id, r.stage_label(), where_, util::trunc(&r.brief(), 100));
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
/// see `remote::Inner::announce`); with a TUI it lives in `/web` and `/remote`.
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
    let frame = Duration::from_millis(1000 / app.settings.fps.clamp(4, 60) as u64);
    let mut last_draw = Instant::now() - frame;
    let mut last_pub = Instant::now() - WEB_PUBLISH_EVERY;
    let mut last_title = String::new();
    let mut quit = quit;
    loop {
        let animating = app.animating();
        if let Some(t) = terminal.as_deref_mut() {
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
            _ = tokio::time::sleep(wait) => {}
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
    app.shutdown();
    tokio::time::sleep(Duration::from_millis(200)).await;
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
    println!("mantra {} doctor\n", env!("CARGO_PKG_VERSION"));
    // WP10.5: both CLI probes share one 5s timeout (thread + `recv_timeout`), so a hung binary
    // (observed in this sandbox for `claude` under `subscription` auth, §0.3) can never hang doctor.
    let (good, text) = probe_cli(&s.codex_command[0], " — npm i -g @openai/codex");
    println!("{} codex executable: {text}", ok(good));
    let r = config::Registry::load();
    // Readiness, not presence: the same spawn + `initialize` handshake an agent does, one no-op
    // command through the configured sandbox, then a clean shutdown — each its own row and
    // verdict, each bounded by PROBE_TIMEOUT, each failure quoting Codex's own error line.
    let (server, sandbox) = probe_app_server(&s, &r);
    match &server {
        Ok(t) => println!("{} codex app-server: {t}", ok(true)),
        Err(e) => println!("{} codex app-server: {e}", ok(false)),
    }
    let (verdict, text) = provider_row(&s, &r);
    println!("{} provider: {text}", verdict.map(ok).unwrap_or(" "));
    match &sandbox {
        Ok(t) => println!("{} sandbox: {t}", ok(true)),
        Err(e) => match util::sandbox_probe() {
            // The kernel knob explains the failure — say so, with the fix.
            Err(why) if !e.starts_with("skipped") => println!("{} sandbox: {e}\n  {why}", ok(false)),
            _ => println!("{} sandbox: {e}", ok(false)),
        },
    }
    if let Ok(o) = std::process::Command::new(&s.codex_command[0]).args(["login", "status"]).output() {
        let t = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
        let line = login_status_line(&t);
        let needed = codex_account_needed(&s, &r);
        match (o.status.success(), needed.is_empty()) {
            (true, _) => println!("{} login: {line}", ok(true)),
            (false, false) => println!("{} login: {line} — needed by {} (run `codex login`)", ok(false), needed.join(", ")),
            (false, true) => println!("  login: {line} — not needed: the default model and pattern roles all run on custom providers"),
        }
    }
    let (good, text) = probe_cli(&s.claude_command[0], " (optional — only needed for Claude Code agents; npm i -g @anthropic-ai/claude-code)");
    println!("{} claude: {text}", ok(good));
    if crate::util::effective_uid_is_root() {
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
        if p.kind == config::ProviderKind::ClaudeCode {
            if p.auth == "api_key" {
                println!("{} provider {} (Claude Code, api_key): {}", ok(p.resolve_key().is_some()), p.id, key_src(p));
            } else {
                match claude_auth_status(&s.claude_command[0]) {
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
    let m = r.resolve(alias);
    if !m.is_custom_provider() {
        return (None, format!("`{alias}` runs on Codex's own account — see the login row"));
    }
    let Some(p) = r.providers.iter().find(|p| p.id == m.provider) else {
        return (Some(false), format!("`{alias}` uses provider '{}' which is not in /models", m.provider));
    };
    let name = r.provider_name(&p.id);
    // A placeholder (or malformed) base_url never gets a request: the key would go to example.com.
    if let Some(why) = p.url_problem() {
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
    let Some(key) = p.resolve_key() else {
        return (Some(false), format!("{name} for `{alias}`: {key_src}"));
    };
    // `GET /models` on a thread with a hard cap: curl's own limit is longer than PROBE_TIMEOUT.
    let (base, model) = (p.base_url.clone(), m.model.clone());
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(discover::list_models(&base, Some(&key)));
    });
    let host = p.base_url.trim().trim_end_matches('/').to_string();
    match rx.recv_timeout(PROBE_TIMEOUT) {
        Ok(Ok(found)) => {
            let listed = found.iter().any(|f| f.id == model);
            (Some(true), format!("{name} for `{alias}`: {key_src} · {host}/models accepts the key ({} models{})", found.len(), if listed { format!(", `{model}` listed") } else { format!("; `{model}` not among them — the tag is sent as-is") }))
        }
        Ok(Err(e)) if e.contains("HTTP 401") || e.contains("HTTP 403") => (Some(false), format!("{name} for `{alias}`: {key_src} · key rejected — {e}")),
        Ok(Err(e)) if e.starts_with("couldn't reach") || e.starts_with("couldn't run") => (Some(false), format!("{name} for `{alias}`: {key_src} · {e}")),
        // Answered, but not with a model list (no /models endpoint): the key is there, that is all doctor can say.
        Ok(Err(e)) => (None, format!("{name} for `{alias}`: {key_src} · not verified ({e})")),
        Err(_) => (Some(false), format!("{name} for `{alias}`: {key_src} · {host}/models gave no answer within {}s", PROBE_TIMEOUT.as_secs())),
    }
}

/// The one line of `codex login status` output that says whether we are logged in. Codex prints
/// warnings first (`WARNING: proceeding, even though we could not create PATH aliases …`), on the
/// same stream, so "the first line" used to show the warning instead of the verdict.
fn login_status_line(out: &str) -> String {
    let meaningful = |l: &str| {
        let t = l.trim();
        let low = t.to_lowercase();
        !t.is_empty() && !low.starts_with("warning") && !low.starts_with("warn:") && !low.starts_with("error") && !low.contains("bubblewrap")
    };
    out.lines().filter(|l| meaningful(l)).last().or_else(|| out.lines().find(|l| !l.trim().is_empty())).unwrap_or("").trim().to_string()
}

/// Who actually needs Codex's own OpenAI login: the Solo default model and the default pattern's
/// roles that run on the built-in `openai` provider. Empty when everything goes through custom
/// providers (their keys are checked per provider below), so "Not logged in" is not a failure.
fn codex_account_needed(s: &config::Settings, r: &config::Registry) -> Vec<String> {
    let mut v = vec![];
    let on_codex_account = |alias: &str| {
        let m = r.resolve(alias);
        !m.is_custom_provider() && r.backend_of(&m) == config::ProviderKind::Codex
    };
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

#[cfg(test)]
mod doctor_tests {
    use super::*;

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
