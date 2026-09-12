//! Mantra — a terminal UI for running and orchestrating Codex agents.

mod agent;
mod app;
mod config;
mod discover;
mod engine;
mod hub;
mod mock;
mod rpc;
mod ui;
mod util;

use anyhow::Result;
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
  mantra doctor                          check codex, login, terminal, config
  mantra --version

OPTIONS
  --cwd DIR       project directory (default: current directory)
  --demo          simulated agents (no API calls, no cost) in a throwaway demo repo
  --pattern NAME  pattern for new runs (default from settings.toml)

FILES
  ~/.mantra/settings.toml          ui, codex command, defaults   ($MANTRA_HOME overrides the dir)
  ~/.mantra/models.toml            models, context windows, efforts, providers
  ~/.mantra/patterns/              your patterns (the Studio saves here)
  ~/.mantra/runs/<project>/<id>/   per-run plan, journal and outputs
  Nothing is written into your projects. An old ~/.config/mantra is copied over on first start.
";

struct Cli {
    cwd: Option<PathBuf>,
    demo: bool,
    run_goal: Option<String>,
    pattern: Option<String>,
    snapshot: Option<String>,
    size: (u16, u16),
}

fn parse_args() -> Result<Option<Cli>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut cli = Cli { cwd: None, demo: false, run_goal: None, pattern: None, snapshot: None, size: (120, 36) };
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
    // The mock server is a separate mode of the same binary.
    if std::env::args().nth(1).as_deref() == Some("mock-codex") {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
        rt.block_on(mock::run());
        return;
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

async fn async_main(cli: Cli) -> Result<()> {
    let mut settings = config::Settings::load();
    let registry = config::Registry::load();
    let mut demo = cli.demo;
    if !demo && !codex_available(&settings.codex_command) {
        eprintln!("mantra: `{}` not found — starting in DEMO mode (simulated agents).\n        Install Codex with `npm i -g @openai/codex`, then `codex login`.", settings.codex_command.join(" "));
        std::thread::sleep(Duration::from_millis(1500));
        demo = true;
    }
    let project = if demo { demo_project()? } else { cli.cwd.clone().map(|p| p.canonicalize().unwrap_or(p)).unwrap_or(std::env::current_dir()?) };
    if demo {
        let exe = std::env::current_exe()?;
        settings.codex_command = vec![exe.to_string_lossy().to_string(), "mock-codex".into()];
    }
    ui::theme::init(&settings);
    let (tx, mut rx) = mpsc::unbounded_channel::<AppEvent>();
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
    let hub = hub::Hub::new(settings.codex_command.clone(), hub_tx);
    let mut app = App::new(settings, registry, project, hub, tx.clone(), demo);
    if let Some(p) = cli.pattern {
        app.pattern_name = p;
    }
    app.start_solo();
    if let Some(goal) = &cli.run_goal {
        app.start_run(goal);
    }
    if let Some(script) = cli.snapshot.clone() {
        return snapshot(app, rx, &script, cli.size).await;
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
    let res = event_loop(&mut terminal, &mut app, &mut rx).await;
    stop.store(true, Ordering::Relaxed);
    app.shutdown();
    restore_terminal(kbd);
    tokio::time::sleep(Duration::from_millis(150)).await;
    if app.demo {
        eprintln!("demo project left at {}", app.project.display());
    }
    res
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

async fn event_loop<B: Backend>(terminal: &mut Terminal<B>, app: &mut App, rx: &mut mpsc::UnboundedReceiver<AppEvent>) -> Result<()> {
    let mut dirty = true;
    let frame = Duration::from_millis(1000 / app.settings.fps.clamp(4, 60) as u64);
    let mut last_draw = Instant::now() - frame;
    let mut last_title = String::new();
    loop {
        let animating = app.animating();
        if app.force_clear {
            app.force_clear = false;
            terminal.clear()?;
            dirty = true;
        }
        if dirty || (animating && last_draw.elapsed() >= frame) {
            // Synchronized output: the terminal (and tmux ≥ 3.4) paints each frame atomically — no tearing.
            let _ = ratatui::crossterm::queue!(std::io::stdout(), terminal::BeginSynchronizedUpdate);
            terminal.draw(|f| safe_draw(f, app))?;
            let _ = execute!(std::io::stdout(), terminal::EndSynchronizedUpdate);
            last_draw = Instant::now();
            dirty = false;
            let t = app.title();
            if t != last_title {
                let _ = execute!(std::io::stdout(), terminal::SetTitle(&t));
                last_title = t;
            }
        }
        let wait = if animating { frame.saturating_sub(last_draw.elapsed()).max(Duration::from_millis(5)) } else { Duration::from_millis(1000) };
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
                        if is_resize { terminal.autoresize()?; }
                        dirty = true;
                    }
                    None => break,
                }
            }
            _ = tokio::time::sleep(wait) => {}
        }
        app.tick();
        if !app.notes.is_empty() {
            let notes = std::mem::take(&mut app.notes);
            if app.settings.notify {
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
    match std::process::Command::new(&s.codex_command[0]).arg("--version").output() {
        Ok(o) if o.status.success() => println!("{} codex: {}", ok(true), String::from_utf8_lossy(&o.stdout).trim()),
        _ => println!("{} codex: `{}` not found — npm i -g @openai/codex", ok(false), s.codex_command[0]),
    }
    if let Ok(o) = std::process::Command::new(&s.codex_command[0]).args(["login", "status"]).output() {
        let t = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
        println!("{} login: {}", ok(o.status.success()), t.lines().next().unwrap_or("").trim());
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
    let r = config::Registry::load();
    println!("  models: {}", r.models.iter().map(|m| format!("{}={}", m.alias, m.model)).collect::<Vec<_>>().join(", "));
    for p in &r.providers {
        let src = if !p.env_key.trim().is_empty() && std::env::var(p.env_key.trim()).map(|v| !v.trim().is_empty()).unwrap_or(false) {
            format!("${} set", p.env_key)
        } else if p.api_key.as_ref().map(|k| !k.trim().is_empty()).unwrap_or(false) {
            "key stored in models.toml".to_string()
        } else if p.env_key.trim().is_empty() {
            "no key".to_string()
        } else {
            format!("${} NOT set", p.env_key)
        };
        println!("{} provider {}: {}", ok(p.resolve_key().is_some()), p.id, src);
    }
    println!("  log: {}", config::log_path().display());
}
