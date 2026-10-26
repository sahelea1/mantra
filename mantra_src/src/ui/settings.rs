//! `/settings`: everything that is not a model, a provider or a pattern on one screen — Solo,
//! runs, features, MCP servers, web & remote, appearance — as rows edited in place (`ui::form`),
//! each change saved through `App::set_setting`. The selected row's hint stands beside the form.
//! The web & remote rows show what the web layer is doing right now, as `/web` and `/remote` did.

use super::{theme, *};
use crate::app::{App, Choice, EditTarget, Overlay, Select, SelectThen};
use crate::settings_ops::{Field, FieldKind, SECTION_APPEARANCE, SECTION_FEATURES, SECTION_RUNS, SECTION_SOLO, SECTION_WEB};
use crate::ui::form::{self, Edit, Outcome, Start};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout};

const SECTION_MCP: &str = "MCP servers";
/// The order the sections are drawn in.
const SECTIONS: &[&str] = &[SECTION_SOLO, SECTION_RUNS, SECTION_FEATURES, SECTION_MCP, SECTION_WEB, SECTION_APPEARANCE];

/// One row of the form.
#[derive(Clone, Debug)]
pub enum Row {
    /// A field of `Settings::fields`.
    Field(Field),
    /// The effort a new Solo session starts at: the default model's own (models.toml).
    Effort { model: String, options: Vec<String>, value: String },
    /// An MCP server.
    Mcp { name: String, command: String, enabled: bool },
    /// Where a new MCP server is added (ctrl+n): its name, then its command line, typed here.
    McpNew,
    /// A line of what the web layer is doing, and what ⏎ does on it.
    Info { label: String, value: String, tone: (u8, u8, u8), action: Option<Action>, hint: &'static str },
}

/// What ⏎ does on an info row.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Action {
    /// The remote link's QR code, full size.
    Qr,
    /// A new remote link; the old one, its code and QR stop working.
    Rotate,
}

/// The command line a server runs, an argument with a space or a quote in it quoted.
fn command_line(s: &crate::config::McpServer) -> String {
    let shown = |a: &str| if a.is_empty() || a.contains([' ', '\t', '"', '\'']) { format!("'{}'", a.replace('\'', "'\\''")) } else { a.to_string() };
    std::iter::once(s.command.as_str()).chain(s.args.iter().map(String::as_str)).map(shown).collect::<Vec<_>>().join(" ")
}

/// Every row, section by section, as the screen shows them now.
pub fn rows(app: &App) -> Vec<(&'static str, Row)> {
    let fields = app.settings.fields(&app.registry, &app.settings_ui.patterns);
    let mut out: Vec<(&'static str, Row)> = vec![];
    for section in SECTIONS {
        match *section {
            SECTION_MCP => {
                for (name, s) in &app.settings.mcp_servers {
                    out.push((SECTION_MCP, Row::Mcp { name: name.clone(), command: command_line(s), enabled: s.enabled }));
                }
                out.push((SECTION_MCP, Row::McpNew));
            }
            SECTION_WEB => {
                out.extend(fields.iter().filter(|f| f.section == SECTION_WEB).map(|f| (SECTION_WEB, Row::Field(f.clone()))));
                out.extend(web_rows(app).into_iter().map(|r| (SECTION_WEB, r)));
            }
            s => {
                for f in fields.iter().filter(|f| f.section == s) {
                    out.push((f.section, Row::Field(f.clone())));
                    // the effort sits under the model it belongs to
                    if f.key == "default_model" {
                        let m = app.registry.resolve(&app.settings.default_model);
                        out.push((SECTION_SOLO, Row::Effort { model: m.alias.clone(), options: m.efforts(), value: m.default_effort.clone() }));
                    }
                }
            }
        }
    }
    out
}

/// What `/web` and `/remote` showed, as rows: where the web UI is and how it is protected, the
/// relay link with its code, password and QR.
fn web_rows(app: &App) -> Vec<Row> {
    let info = |label: &str, value: String, tone, action, hint| Row::Info { label: label.into(), value, tone, action, hint };
    let mut v = vec![];
    match app.web.as_ref().map(|w| &w.info).filter(|i| i.listen.is_some()) {
        None => v.push(info("web UI", "off — mantra --web starts it".into(), theme::MUTED, None, "The web UI is off. mantra --web serves it to this machine (http://127.0.0.1:7777); mantra --web 0.0.0.0:7777 --web-tls to your phone on the LAN, which then needs a password.")),
        Some(i) => {
            for (n, u) in i.urls.iter().enumerate() {
                v.push(info(if n == 0 { "open" } else { "" }, u.clone(), theme::CYAN, None, "Where a browser opens the web UI."));
            }
            let (cert, tone) = if i.self_signed {
                (format!("{}/cert.pem — install it on each device", i.urls.first().cloned().unwrap_or_default()), theme::GREEN)
            } else if i.tls {
                ("your certificate".to_string(), theme::GREEN)
            } else {
                ("none (http) — phones can't install the app or get notifications".to_string(), theme::AMBER)
            };
            v.push(info("certificate", cert, tone, None, "Mantra's own certificate authority: install it from /cert.pem on each device so the browser trusts the HTTPS address."));
            let subs = app.web.as_ref().and_then(|x| x.push.as_ref()).filter(|_| i.push).map(|p| p.store.lock().map(|s| s.list().len()).unwrap_or(0));
            if let Some(n) = subs {
                v.push(info("push", format!("{n} device{} subscribed", if n == 1 { "" } else { "s" }), theme::MUTED, None, "The phones and browsers that get a notification when a run needs you."));
            }
            let n = app.web.as_ref().map(|x| x.reg.count(crate::web::ConnKind::Local)).unwrap_or(0);
            v.push(info("browsers", format!("{n} connected"), theme::MUTED, None, "Browsers on the local web UI right now."));
        }
    }
    match app.web.as_ref().and_then(|x| x.remote.as_ref()) {
        None => v.push(info("remote", "off — mantra --remote starts it".into(), theme::MUTED, None, "Remote access is off. mantra --remote reaches this session from anywhere through remote.mantra.codes: end-to-end encrypted, the relay only ever sees ciphertext.")),
        Some(rem) => {
            let r = rem.info();
            let (state, tone) = match (r.connected, &r.last_error) {
                (true, _) => (format!("{} · connected · {} client{}", r.relay, r.clients, if r.clients == 1 { "" } else { "s" }), theme::GREEN),
                (false, Some(e)) => (format!("{} · reconnecting — {e}", r.relay), theme::AMBER),
                (false, None) => (format!("{} · connecting…", r.relay), theme::AMBER),
            };
            v.push(info("relay", state, tone, None, "The relay this session is reachable through."));
            v.push(info("link", r.link.clone().unwrap_or_else(|| "deriving the key…".into()), theme::CYAN, r.link.as_ref().map(|_| Action::Qr), "The link opens this session from anywhere: share it like a key. ⏎ shows it as a QR code to scan."));
            v.push(info("code", format!("{}  + the password, at remote.mantra.codes", r.code.unwrap_or_default()), theme::TEXT, None, "Typed at remote.mantra.codes with the password, instead of the link."));
            v.push(info("password", r.password.unwrap_or_default(), theme::SAFFRON, None, "The remote password: with the code it opens this session."));
            v.push(info("new link", "⏎ makes a new link — the old link, code and QR stop working".into(), theme::MUTED, Some(Action::Rotate), "A new link, code and QR (and password, when it was generated): every device that had the old ones is shut out."));
        }
    }
    v
}

/// A row's label, as the form's first column shows it.
fn label(row: &Row) -> String {
    match row {
        Row::Field(f) => f.label.clone(),
        Row::Effort { .. } => "Effort".into(),
        Row::Mcp { name, .. } => name.clone(),
        Row::McpNew => String::new(),
        Row::Info { label, .. } => label.clone(),
    }
}

/// When a change to the row applies, for the about panel ("" = at once).
fn when(row: &Row) -> &'static str {
    match row {
        Row::Field(f) if !crate::app::applied_live(&f.key) => crate::settings_ops::takes_effect(&f.key),
        Row::Effort { .. } => crate::settings_ops::takes_effect("default_model"),
        _ => "",
    }
}

/// What the about panel says of a row.
fn about(row: &Row) -> String {
    match row {
        Row::Field(f) => f.hint.clone(),
        Row::Effort { model, .. } => format!("The reasoning effort a new Solo session starts {model} at — the model's own default in /models (alt+↑↓ changes the live one)."),
        Row::Mcp { .. } => "A stdio MCP server agents can be given: Solo by the MCP servers row above, a pattern role by its mcp field. space switches it on or off, ctrl+t starts it and lists its tools, del (twice) removes it.".into(),
        Row::McpNew => "A new MCP server: its name (letters, digits, - and _), then its command line as you would type it — uvx blender-mcp.".into(),
        Row::Info { hint, .. } => hint.to_string(),
    }
}

/// The selected row's index, inside the list.
fn sel(app: &App, n: usize) -> usize {
    app.settings_ui.sel.min(n.saturating_sub(1))
}

/// A change through `App::set_setting`, toasted with when it takes effect — or why it was refused.
fn set(app: &mut App, f: &Field, value: &str) -> bool {
    match app.set_setting(&f.key, value) {
        Ok(when) => {
            // approvals say themselves (`set_approval_mode`); a password is never shown
            if f.key != "approval_mode" {
                let shown = match f.kind {
                    FieldKind::Secret if value.is_empty() => "cleared".to_string(),
                    FieldKind::Secret => "set".to_string(),
                    _ => app.settings.fields(&app.registry, &app.settings_ui.patterns).into_iter().find(|x| x.key == f.key).map(|x| x.value).unwrap_or_else(|| value.to_string()),
                };
                let when = if when.is_empty() { String::new() } else { format!(" · {when}") };
                app.toast(format!("{} → {shown}{when}", f.label.to_lowercase()), crate::agent::Level::Ok);
            }
            true
        }
        Err(e) => {
            app.toast(e, crate::agent::Level::Warn);
            false
        }
    }
}

/// ←→ (`d`), space or ⏎ on a choice, an on/off or a number: the next value, saved.
fn step(app: &mut App, row: &Row, d: i32) {
    match row {
        Row::Field(f) => {
            let next = match &f.kind {
                FieldKind::Enum(o) => form::cycle(o, &f.value, d),
                FieldKind::Bool => if f.value == "on" { "off" } else { "on" }.to_string(),
                FieldKind::Number { min, max } => (f.value.parse::<i64>().unwrap_or(*min as i64) + d as i64).clamp(*min as i64, *max as i64).to_string(),
                _ => return,
            };
            if next != f.value {
                set(app, f, &next);
            }
        }
        Row::Effort { options, value, .. } if !options.is_empty() => {
            let next = form::cycle(options, value, d);
            set_effort(app, &next);
        }
        Row::Effort { model, .. } => app.toast(format!("{model} has no reasoning-effort setting — list its efforts in /models"), crate::agent::Level::Info),
        Row::Mcp { name, enabled, .. } => toggle_mcp(app, name, !enabled),
        _ => {}
    }
}

fn set_effort(app: &mut App, effort: &str) {
    match app.set_default_effort(effort) {
        Ok(()) => app.toast(format!("effort → {effort} · Solo's next session (/new) starts on it"), crate::agent::Level::Ok),
        Err(e) => app.toast(e, crate::agent::Level::Warn),
    }
}

fn toggle_mcp(app: &mut App, name: &str, on: bool) {
    match app.mcp_set_server_enabled(name, on) {
        Ok(()) => app.toast(format!("{name} {} · Solo's next session and new runs", if on { "on" } else { "off" }), crate::agent::Level::Ok),
        Err(e) => app.toast(e, crate::agent::Level::Warn),
    }
}

/// The checklist of servers Solo gets (`solo_mcp`), ticked as it is now.
fn solo_mcp_picker(app: &mut App) {
    let all = app.settings.solo_mcp.iter().any(|s| s.trim() == "*");
    let items: Vec<Choice> = app.settings.mcp_servers.iter().map(|(name, s)| Choice { key: name.clone(), label: name.clone(), detail: format!("{}{}", command_line(s), if s.enabled { "" } else { " (off)" }), on: all || app.settings.solo_mcp.contains(name) }).collect();
    if items.is_empty() {
        return app.toast("no MCP servers yet — ctrl+n adds one", crate::agent::Level::Info);
    }
    app.overlays.push(Overlay::Select(Select { title: "MCP servers Solo gets".into(), items, sel: 0, multi: true, then: SelectThen::SoloMcp }));
}

/// The checklist's ticks into `solo_mcp`: all of them is every enabled server, none is none.
pub fn solo_mcp_chosen(app: &mut App, ticked: &[String]) {
    let value = if ticked.is_empty() {
        "none".to_string()
    } else if ticked.len() == app.settings.mcp_servers.len() {
        "all".to_string()
    } else {
        ticked.join(", ")
    };
    if let Some(Row::Field(f)) = rows(app).into_iter().map(|(_, r)| r).find(|r| matches!(r, Row::Field(f) if f.key == "solo_mcp")) {
        set(app, &f, &value);
    }
}

/// Starts editing a field row (`start`; a typed `c` for `Overwrite`).
fn begin(app: &mut App, f: &Field, start: Start, c: Option<char>) {
    let target = EditTarget::SettingField(f.key.clone());
    // a password is never shown, so never edited around: it is typed anew
    let value = if f.kind == FieldKind::Secret { String::new() } else { f.value.clone() };
    let mut e = match (start, c) {
        (Start::Overwrite, Some(c)) => Edit::overwrite(target, &value, c),
        (s, _) => Edit::new(target, &value, s),
    };
    if f.kind == FieldKind::Secret {
        e = e.secret();
    }
    app.edit = Some(e);
}

/// ctrl+n (or typing on the "add a server" row): the new server's name field, in place.
fn add_mcp(app: &mut App, c: Option<char>) {
    app.settings_ui.mcp_name = None;
    if let Some(i) = rows(app).iter().position(|(_, r)| matches!(r, Row::McpNew)) {
        app.settings_ui.sel = i;
    }
    app.edit = Some(match c {
        Some(c) => Edit::overwrite(EditTarget::McpName, "", c),
        None => Edit::new(EditTarget::McpName, "", Start::Clear),
    });
}

/// A finished edit: saved through `set_setting`, or the next step of a new server. Returns
/// false — the edit stays open — when it was refused.
fn commit(app: &mut App, e: &Edit) -> bool {
    let text = e.text();
    match &e.target {
        EditTarget::SettingField(key) => {
            if !e.changed() {
                return true;
            }
            let Some(Row::Field(f)) = rows(app).into_iter().map(|(_, r)| r).find(|r| matches!(r, Row::Field(f) if &f.key == key)) else { return true };
            set(app, &f, &text)
        }
        EditTarget::McpName => {
            let name = text.trim().to_string();
            if name.is_empty() {
                return true;
            }
            if let Some(why) = crate::config::mcp_name_problem(&name).or_else(|| app.settings.mcp_servers.contains_key(&name).then(|| format!("{name} is configured already"))) {
                app.toast(why, crate::agent::Level::Warn);
                return false;
            }
            app.settings_ui.mcp_name = Some(name);
            app.edit = Some(Edit::new(EditTarget::McpCommand, "", Start::Clear));
            true
        }
        EditTarget::McpCommand => {
            let name = app.settings_ui.mcp_name.clone().unwrap_or_default();
            match app.mcp_add_server(&name, &text) {
                Ok(()) => {
                    app.settings_ui.mcp_name = None;
                    if let Some(i) = rows(app).iter().position(|(_, r)| matches!(r, Row::Mcp { name: n, .. } if *n == name)) {
                        app.settings_ui.sel = i;
                    }
                    app.toast(format!("added {name} — ctrl+t tests it"), crate::agent::Level::Ok);
                    true
                }
                Err(why) => {
                    app.toast(why, crate::agent::Level::Warn);
                    false
                }
            }
        }
        _ => true,
    }
}

fn edit_key(app: &mut App, k: KeyEvent) {
    let Some(e) = app.edit.as_mut() else { return };
    let outcome = e.key(k);
    match outcome {
        Outcome::Editing => {}
        Outcome::Revert => {
            if matches!(e.target, EditTarget::McpName | EditTarget::McpCommand) {
                app.settings_ui.mcp_name = None;
            }
            app.edit = None;
        }
        Outcome::Commit | Outcome::CommitMove(_) => {
            let Some(e) = app.edit.take() else { return };
            let adding = matches!(e.target, EditTarget::McpName);
            if !commit(app, &e) {
                app.edit = Some(e);
                return;
            }
            if let (Outcome::CommitMove(d), false) = (outcome, adding) {
                let n = rows(app).len();
                app.settings_ui.sel = (app.settings_ui.sel as i64 + d as i64).clamp(0, n.saturating_sub(1) as i64) as usize;
            }
        }
    }
}

pub fn settings_key(app: &mut App, k: KeyEvent) {
    if app.edit.is_some() {
        return edit_key(app, k);
    }
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    if k.code != KeyCode::Delete {
        let armed = form::armed(&app.confirm).is_some();
        app.confirm = None;
        if armed && k.code == KeyCode::Esc {
            return;
        }
    }
    let all = rows(app);
    let n = all.len();
    let i = sel(app, n);
    let Some((_, row)) = all.get(i).cloned() else { return };
    match k.code {
        KeyCode::Char('s') if ctrl => {
            app.save_settings();
            app.toast("settings saved — each change is saved as it is made", crate::agent::Level::Ok);
        }
        KeyCode::Char('n') if ctrl => add_mcp(app, None),
        KeyCode::Char('t') if ctrl => match &row {
            Row::Mcp { name, .. } => app.mcp_test_server(name),
            _ => app.toast("ctrl+t tests an MCP server — select one", crate::agent::Level::Info),
        },
        _ if ctrl => {}
        KeyCode::Esc => app.leave_screen(),
        KeyCode::Up => app.settings_ui.sel = i.saturating_sub(1),
        KeyCode::Down => app.settings_ui.sel = (i + 1).min(n.saturating_sub(1)),
        KeyCode::PageUp => app.settings_ui.sel = i.saturating_sub(10),
        KeyCode::PageDown => app.settings_ui.sel = (i + 10).min(n.saturating_sub(1)),
        KeyCode::Home => app.settings_ui.sel = 0,
        KeyCode::End => app.settings_ui.sel = n.saturating_sub(1),
        KeyCode::Delete => {
            if let Row::Mcp { name, .. } = &row {
                if form::confirm(&mut app.confirm, name) {
                    match app.mcp_remove_server(name) {
                        Ok(()) => app.toast(format!("removed {name}"), crate::agent::Level::Ok),
                        Err(e) => app.toast(e, crate::agent::Level::Warn),
                    }
                    app.settings_ui.sel = i.min(rows(app).len().saturating_sub(1));
                }
            }
        }
        KeyCode::Left | KeyCode::Right => step(app, &row, if k.code == KeyCode::Left { -1 } else { 1 }),
        _ => match &row {
            Row::Field(f) => field_key(app, f, k),
            Row::Effort { options, value, .. } => match k.code {
                KeyCode::Enter | KeyCode::Char(' ' | '+') => step(app, &row, 1),
                KeyCode::Char('-') => step(app, &row, -1),
                KeyCode::Char(c) => {
                    if let Some(v) = form::jump(options, value, c) {
                        set_effort(app, &v);
                    }
                }
                _ => {}
            },
            Row::Mcp { name, enabled, .. } => {
                if matches!(k.code, KeyCode::Char(' ') | KeyCode::Enter) {
                    toggle_mcp(app, name, !enabled);
                }
            }
            Row::McpNew => match k.code {
                KeyCode::Enter => add_mcp(app, None),
                KeyCode::Char(c) if !c.is_whitespace() => add_mcp(app, Some(c)),
                _ => {}
            },
            Row::Info { action, .. } => {
                if k.code == KeyCode::Enter {
                    match action {
                        Some(Action::Qr) => app.overlays.push(Overlay::Qr),
                        Some(Action::Rotate) => {
                            if let Some(r) = app.web.as_ref().and_then(|w| w.remote.clone()) {
                                r.rotate();
                                app.toast("new remote link — the old link, code and QR no longer work", crate::agent::Level::Ok);
                            }
                        }
                        None => {}
                    }
                }
            }
        },
    }
}

/// The keys of a settings field that is not being edited.
fn field_key(app: &mut App, f: &Field, k: KeyEvent) {
    let row = Row::Field(f.clone());
    match (&f.kind, k.code) {
        (FieldKind::Enum(_), KeyCode::Enter) if f.key == "solo_mcp" => solo_mcp_picker(app),
        (FieldKind::Enum(_) | FieldKind::Bool, KeyCode::Enter | KeyCode::Char(' ' | '+')) => step(app, &row, 1),
        (FieldKind::Enum(_) | FieldKind::Bool | FieldKind::Number { .. }, KeyCode::Char('-')) => step(app, &row, -1),
        (FieldKind::Number { .. }, KeyCode::Char('+')) => step(app, &row, 1),
        (FieldKind::Enum(o), KeyCode::Char(c)) => {
            if let Some(v) = form::jump(o, &f.value, c) {
                set(app, f, &v);
            }
        }
        (FieldKind::Bool, KeyCode::Char(c @ ('y' | 'n'))) => {
            let v = if c == 'y' { "on" } else { "off" };
            if v != f.value {
                set(app, f, v);
            }
        }
        (FieldKind::Text | FieldKind::Secret | FieldKind::Number { .. }, KeyCode::Enter) => begin(app, f, Start::Append, None),
        (FieldKind::Text | FieldKind::Secret | FieldKind::Number { .. }, KeyCode::Backspace) => begin(app, f, Start::Clear, None),
        (FieldKind::Number { .. }, KeyCode::Char(c)) if c.is_ascii_digit() => begin(app, f, Start::Overwrite, Some(c)),
        (FieldKind::Text | FieldKind::Secret, KeyCode::Char(c)) if !c.is_control() => begin(app, f, Start::Overwrite, Some(c)),
        _ => {}
    }
}

// ───────────────────────────── drawing ─────────────────────────────

pub fn draw_settings(f: &mut Frame, app: &mut App) {
    let area = f.area();
    let rows_v = Layout::default().direction(Direction::Vertical).constraints([Constraint::Length(1), Constraint::Min(6), Constraint::Length(1)]).split(area);
    header(f, rows_v[0], vec![Span::styled(format!("  {} settings", theme::g("›", ">")), theme::faint())], vec![Span::styled(home_rel(&crate::config::home().join("settings.toml")), theme::faint()), Span::raw(" ")]);
    let all = rows(app);
    let n = all.len();
    let si = sel(app, n);
    app.settings_ui.sel = si;
    // wide: the form and the about panel side by side; narrow: the panel under the form
    let wide = rows_v[1].width >= 96;
    let (form_area, about_area) = if wide {
        let c = Layout::default().direction(Direction::Horizontal).constraints([Constraint::Min(50), Constraint::Length(36)]).split(rows_v[1]);
        (c[0], c[1])
    } else {
        let c = Layout::default().direction(Direction::Vertical).constraints([Constraint::Min(6), Constraint::Length(5)]).split(rows_v[1]);
        (c[0], c[1])
    };
    let blk = block("settings", theme::VIOLET);
    let inner = blk.inner(form_area);
    f.render_widget(blk, form_area);
    let lw = all.iter().map(|(_, r)| w_of(&label(r))).max().unwrap_or(0).clamp(14, 22) + 2;
    let vw = (inner.width as usize).saturating_sub(lw + 3);
    let mut lines: Vec<Line> = vec![];
    let (mut sel_line, mut cursor) = (0, None);
    let mut section = "";
    for (i, (sec, row)) in all.iter().enumerate() {
        if *sec != section {
            if !lines.is_empty() {
                lines.push(Line::default());
            }
            lines.push(Line::from(Span::styled(format!(" {sec}"), theme::bold(theme::fg(theme::VIOLET)))));
            section = sec;
        }
        let is = i == si;
        if is {
            sel_line = lines.len();
        }
        let lst = if is { theme::bold(theme::accent()) } else { theme::muted() };
        let name = Span::styled(format!("   {:<lw$}", trunc(&label(row), lw - 1)), lst);
        // a field being edited is its input, cursor and all
        let editing = app.edit.as_ref().filter(|_| is).filter(|e| match (&e.target, row) {
            (EditTarget::SettingField(k), Row::Field(f)) => *k == f.key,
            (EditTarget::McpName | EditTarget::McpCommand, Row::McpNew) => true,
            _ => false,
        });
        let line = match (row, editing) {
            (Row::McpNew, Some(e)) => {
                let (pre, room) = match e.target {
                    EditTarget::McpCommand => (format!("{}  command ", app.settings_ui.mcp_name.clone().unwrap_or_default()), vw.saturating_sub(app.settings_ui.mcp_name.as_deref().map(w_of).unwrap_or(0) + 11)),
                    _ => ("name ".to_string(), vw.saturating_sub(5)),
                };
                let (shown, cur) = e.view(room.max(8));
                cursor = Some((inner.x + 3 + w_of(&pre) as u16 + cur as u16, lines.len()));
                Line::from(vec![Span::styled(format!("   {} ", theme::g("+", "+")), theme::bold(theme::accent())), Span::styled(pre, theme::muted()), Span::styled(format!("{shown:<w$}", w = room.max(8)), theme::bold(theme::text()).add_modifier(Modifier::UNDERLINED))])
            }
            (_, Some(e)) => {
                let (shown, cur) = e.view(vw);
                cursor = Some((inner.x + 3 + lw as u16 + cur as u16, lines.len()));
                Line::from(vec![name, Span::styled(format!("{shown:<vw$}"), theme::bold(theme::text()).add_modifier(Modifier::UNDERLINED))])
            }
            (Row::Field(f), None) => {
                let (value, st) = match &f.kind {
                    FieldKind::Secret if f.value.is_empty() => ("not set".to_string(), theme::dim()),
                    FieldKind::Secret => (theme::g("••••••••", "********").to_string(), theme::text()),
                    FieldKind::Bool if f.value == "on" => ("on".to_string(), theme::fg(theme::GREEN)),
                    FieldKind::Bool => ("off".to_string(), theme::muted()),
                    _ if f.value.is_empty() && f.key == "web.listen" => ("127.0.0.1:7777 (the default)".to_string(), theme::dim()),
                    _ if f.value.is_empty() => ("—".to_string(), theme::dim()),
                    _ => (f.value.clone(), theme::text()),
                };
                let mark = match &f.kind {
                    _ if !is => "",
                    FieldKind::Enum(_) | FieldKind::Bool | FieldKind::Number { .. } => theme::g(" ◂ ▸", " < >"),
                    _ => theme::g(" ⏎", " <enter>"),
                };
                Line::from(vec![name, Span::styled(trunc(&value, vw.saturating_sub(w_of(mark))), st), Span::styled(mark, theme::faint())])
            }
            (Row::Effort { options, value, .. }, None) => {
                let v = if options.is_empty() { "— (the model has none)".to_string() } else { value.clone() };
                let mark = if is && !options.is_empty() { theme::g(" ◂ ▸", " < >") } else { "" };
                Line::from(vec![name, Span::styled(v, theme::fg(effort_color(value))), Span::styled(mark, theme::faint())])
            }
            (Row::Mcp { name: n, command, enabled }, None) => {
                let status = app.settings_ui.mcp_status.get(n).cloned().unwrap_or_default();
                let (state, sst) = if *enabled { (format!("{} on ", theme::g("●", "*")), theme::fg(theme::GREEN)) } else { (format!("{} off", theme::g("○", "-")), theme::dim()) };
                let stw = w_of(&status).min(vw / 3);
                let cw = vw.saturating_sub(7 + stw + 1).max(8);
                let tone = if status.starts_with("ok") {
                    theme::fg(theme::GREEN)
                } else if status.starts_with("error") {
                    theme::fg(theme::RED)
                } else {
                    theme::fg(theme::AMBER)
                };
                let mut spans = vec![name, Span::styled(format!("{:<cw$} ", trunc(command, cw)), theme::text()), Span::styled(state, sst)];
                if !status.is_empty() {
                    let lead = if status == "testing…" { format!("  {} ", anim::spinner()) } else { "  ".into() };
                    spans.push(Span::styled(format!("{lead}{}", trunc(&status, stw)), tone));
                }
                Line::from(spans)
            }
            (Row::McpNew, None) => Line::from(Span::styled(format!("   {} add a server (ctrl+n)", theme::g("+", "+")), if is { theme::bold(theme::accent()) } else { theme::faint() })),
            (Row::Info { value, tone, action, .. }, None) => {
                let mark = if is && action.is_some() { theme::g(" ⏎", " <enter>") } else { "" };
                Line::from(vec![name, Span::styled(trunc(value, vw.saturating_sub(w_of(mark))), theme::fg(*tone)), Span::styled(mark, theme::faint())])
            }
        };
        lines.push(line);
    }
    let h = inner.height as usize;
    // the selected row in view, and the section header above it when there is room for it
    let scroll = form::scroll_for(sel_line, 0, h, lines.len()).min(sel_line.saturating_sub(1));
    let scroll = if sel_line >= scroll + h { sel_line + 1 - h } else { scroll };
    f.render_widget(Paragraph::new(lines).scroll((scroll as u16, 0)), inner);
    form::band_line(f.buffer_mut(), inner, sel_line, scroll);
    if let Some((x, line)) = cursor.filter(|(_, l)| *l >= scroll && *l < scroll + h) {
        f.set_cursor_position((x, inner.y + (line - scroll) as u16));
    }

    // about this setting
    let ablk = block("about", theme::FAINT);
    let ainner = ablk.inner(about_area);
    f.render_widget(ablk, about_area);
    if let Some((_, row)) = all.get(si) {
        let mut al = vec![Line::from(Span::styled(format!(" {}", label(row)), theme::bold(theme::text())))];
        let pad = || vec![Span::raw(" ")];
        al.extend(md::wrap(vec![Span::styled(about(row), theme::muted())], (ainner.width as usize).saturating_sub(1), pad(), pad()));
        let when = when(row);
        if !when.is_empty() {
            let lead = || vec![Span::styled(format!(" {} ", theme::g("↳", "-")), theme::faint())];
            al.extend(md::wrap(vec![Span::styled(when.to_string(), theme::faint())], (ainner.width as usize).saturating_sub(1), lead(), vec![Span::raw("   ")]));
        }
        if !wide {
            al.remove(0);
        }
        f.render_widget(Paragraph::new(al), ainner);
    }
    let hints = settings_footer(app, all.get(si).map(|(_, r)| r));
    footer(f, rows_v[2], &hints.iter().map(|(k, d)| (*k, d.as_str())).collect::<Vec<_>>());
}

/// The footer for the selected row (or the edit, or an armed Delete): eight at the most.
fn settings_footer(app: &App, row: Option<&Row>) -> Vec<(&'static str, String)> {
    if let Some(e) = &app.edit {
        return match e.target {
            EditTarget::McpName => vec![("⏎", "next: its command line".into()), ("esc", "cancel".into())],
            EditTarget::McpCommand => vec![("⏎", "add it".into()), ("esc", "cancel".into())],
            _ => vec![("⏎", "save".into()), ("esc", "revert".into()), ("↑↓", "save and move".into())],
        };
    }
    if let Some(what) = form::armed(&app.confirm) {
        return vec![("del", format!("again to remove {what}")), ("esc", "keep it".into())];
    }
    let mut v: Vec<(&'static str, String)> = vec![("↑↓", "select".into())];
    match row {
        Some(Row::Field(f)) => v.push(match &f.kind {
            FieldKind::Enum(_) if f.key == "solo_mcp" => ("←→", "change · ⏎ pick".into()),
            FieldKind::Enum(_) | FieldKind::Bool => ("←→", "change".into()),
            FieldKind::Number { .. } => ("type ←→", "change".into()),
            _ => ("type ⏎", "edit".into()),
        }),
        Some(Row::Effort { .. }) => v.push(("←→", "change".into())),
        Some(Row::Mcp { .. }) => {
            v.push(("space", "on/off".into()));
            v.push(("ctrl+t", "test".into()));
            v.push(("del", "remove".into()));
        }
        Some(Row::McpNew) => v.push(("⏎", "add a server".into())),
        Some(Row::Info { action: Some(Action::Qr), .. }) => v.push(("⏎", "QR code".into())),
        Some(Row::Info { action: Some(Action::Rotate), .. }) => v.push(("⏎", "new link".into())),
        _ => {}
    }
    v.push(("ctrl+n", "add MCP server".into()));
    v.push(("esc", "back".into()));
    v
}

/// `Overlay::Qr`: the remote link as a QR code, as large as the terminal lets it be scanned from.
pub fn draw_qr(f: &mut Frame, area: Rect, app: &App) {
    let info = app.web.as_ref().and_then(|w| w.remote.as_ref()).map(|r| r.info());
    let rows = info.as_ref().and_then(|i| i.qr.clone());
    let mut l: Vec<Line> = vec![];
    let st = Style::default().fg(ratatui::style::Color::White).bg(ratatui::style::Color::Black);
    match rows.filter(|_| !theme::ascii()) {
        Some(rows) => {
            let m: Vec<Vec<bool>> = rows.iter().map(|r| r.chars().map(|c| c == '1').collect()).collect();
            let q = crate::web::qr::half_blocks(&m, 2);
            let qw = q.first().map(|r| w_of(r)).unwrap_or(0);
            if q.len() as u16 + 4 > area.height || qw as u16 + 4 > area.width {
                l.push(Line::from(Span::styled("  the terminal is too small for the QR code — enlarge it, or open the link", theme::muted())));
            } else {
                let pad = " ".repeat((area.width as usize).saturating_sub(qw + 2) / 2);
                for r in q {
                    l.push(Line::from(vec![Span::raw(pad.clone()), Span::styled(r, st)]));
                }
            }
        }
        None => l.push(Line::from(Span::styled("  no QR code: remote access is off, or its key is still being derived", theme::muted()))),
    }
    l.push(Line::default());
    l.push(Line::from(Span::styled("  scan it to open this session · esc", theme::faint())));
    let h = (l.len() as u16 + 2).min(area.height);
    let r = Rect { x: area.x, y: area.y + (area.height - h) / 2, width: area.width, height: h };
    f.render_widget(Clear, r);
    f.render_widget(Paragraph::new(l).block(block("remote link", theme::VIOLET)), r);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(app: &mut App, code: KeyCode) {
        settings_key(app, KeyEvent::new(code, KeyModifiers::NONE));
    }

    fn typed(app: &mut App, s: &str) {
        for c in s.chars() {
            press(app, KeyCode::Char(c));
        }
    }

    fn select(app: &mut App, pick: impl Fn(&Row) -> bool) {
        app.settings_ui.sel = rows(app).iter().position(|(_, r)| pick(r)).expect("the row");
    }

    fn field(key: &str) -> impl Fn(&Row) -> bool + '_ {
        move |r| matches!(r, Row::Field(f) if f.key == key)
    }

    /// A real (not demo) app on a temp home, so changes are saved and read back from disk.
    fn real_app() -> App {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (ev_tx, _ev_rx) = tokio::sync::mpsc::unbounded_channel();
        let hub = crate::hub::Hub::new(vec![], vec![], vec![], ev_tx, false);
        // These UI tests exercise settings against the established model fixture. A fresh
        // production registry is intentionally empty.
        let mut registry = crate::config::Registry::defaults();
        registry.providers.push(crate::config::bija_default_provider());
        registry.models.extend(crate::config::bija_default_model_entries("bija"));
        let mut app = App::new(crate::config::Settings::default(), registry, std::path::PathBuf::from("/nonexistent"), hub, tx, false);
        app.open_settings();
        app
    }

    fn screen(app: &mut App, w: u16, h: u16) -> (String, ratatui::buffer::Buffer) {
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
        term.draw(|f| super::super::draw(f, app)).unwrap();
        let b = term.backend().buffer().clone();
        ((0..b.area.height).map(|y| (0..b.area.width).map(|x| b[(x, y)].symbol()).collect::<String>() + "\n").collect(), b)
    }

    /// The screen has the sections of the brief, in order, the effort under the model and the
    /// web layer's own rows under its fields.
    #[test]
    fn the_sections_are_solo_runs_features_mcp_web_appearance() {
        crate::web::snapshot::tests::with_mantra_home(|| {
            let app = real_app();
            let all = rows(&app);
            let mut order: Vec<&str> = all.iter().map(|(s, _)| *s).collect();
            order.dedup();
            assert_eq!(order, SECTIONS.to_vec());
            let keys: Vec<String> = all.iter().filter_map(|(_, r)| match r { Row::Field(f) => Some(f.key.clone()), Row::Effort { .. } => Some("(effort)".into()), _ => None }).take(3).collect();
            assert_eq!(keys, vec!["default_model", "(effort)", "approval_mode"]);
            assert!(all.iter().any(|(s, r)| *s == SECTION_WEB && matches!(r, Row::Info { label, .. } if label == "web UI")), "the web UI is off: said so");
            assert!(all.iter().any(|(s, r)| *s == SECTION_MCP && matches!(r, Row::McpNew)));
        });
    }

    /// Each kind of field edits the one way: a choice steps with ←→ and jumps on a letter, an
    /// on/off flips with space, a number types, a text overwrites — each change saved through
    /// `set_setting`, toasted with when it applies, and readable back from settings.toml.
    #[test]
    fn every_kind_of_field_edits_in_place_and_saves() {
        crate::web::snapshot::tests::with_mantra_home(|| {
            let mut app = real_app();
            select(&mut app, field("sandbox"));
            press(&mut app, KeyCode::Right);
            assert_eq!(app.settings.sandbox, "danger-full-access");
            assert_eq!(app.toast.as_ref().map(|t| t.0.as_str()), Some("sandbox → danger-full-access · Solo's next session (/new) uses it"));
            typed(&mut app, "r");
            assert_eq!(crate::config::Settings::load_on_disk().unwrap().sandbox, "read-only", "a letter jumps, and it is saved");
            select(&mut app, field("notify"));
            let was = app.settings.notify;
            press(&mut app, KeyCode::Char(' '));
            assert_eq!(app.settings.notify, !was);
            select(&mut app, field("fps"));
            typed(&mut app, "45");
            assert!(matches!(&app.edit, Some(e) if e.text() == "45"));
            press(&mut app, KeyCode::Enter);
            assert_eq!((app.settings.fps, crate::config::Settings::load_on_disk().unwrap().fps), (45, 45));
            assert_eq!(app.toast.as_ref().map(|t| t.0.as_str()), Some("frames a second → 45"), "the app applies it at once");
            typed(&mut app, "99");
            press(&mut app, KeyCode::Enter);
            assert!(app.toast.as_ref().is_some_and(|t| t.0.contains("from 4 to 60")), "{:?}", app.toast);
            assert!(app.edit.is_some(), "refused: still editing");
            press(&mut app, KeyCode::Esc);
            assert_eq!(app.settings.fps, 45);
            select(&mut app, field("web.listen"));
            typed(&mut app, "0.0.0.0:8080");
            press(&mut app, KeyCode::Down);
            assert_eq!(app.settings.web.listen, "0.0.0.0:8080");
            assert!(matches!(&rows(&app)[app.settings_ui.sel].1, Row::Field(f) if f.key == "web.password"), "↓ saved and moved on");
            // the password: typed masked, never shown, cleared on purpose only
            typed(&mut app, "hunter2");
            let (s, _) = screen(&mut app, 120, 40);
            assert!(!s.contains("hunter2"), "masked while typed");
            press(&mut app, KeyCode::Enter);
            assert_eq!(app.settings.web.password, "hunter2");
            assert_eq!(app.toast.as_ref().map(|t| t.0.as_str()), Some("password → set · applies at the next start"));
            let (s, _) = screen(&mut app, 120, 40);
            assert!(!s.contains("hunter2"));
            press(&mut app, KeyCode::Enter);
            press(&mut app, KeyCode::Enter);
            assert_eq!(app.settings.web.password, "hunter2", "⏎ ⏎ on it changes nothing");
            press(&mut app, KeyCode::Backspace);
            press(&mut app, KeyCode::Enter);
            assert!(app.settings.web.password.is_empty(), "backspace, ⏎ clears it");
        });
    }

    /// The effort row is the default model's own default effort in models.toml.
    #[test]
    fn the_effort_row_is_the_default_models_effort() {
        crate::web::snapshot::tests::with_mantra_home(|| {
            let mut app = real_app();
            select(&mut app, |r| matches!(r, Row::Effort { .. }));
            typed(&mut app, "h");
            assert_eq!(app.registry.resolve("sol").default_effort, "high");
            assert_eq!(crate::config::Registry::load().resolve("sol").default_effort, "high", "saved to models.toml");
        });
    }

    /// MCP servers: ctrl+n types a name and then a command line in place, space switches one off,
    /// Delete twice removes it (and from Solo's list) — all in settings.toml as `mantra mcp` would.
    #[test]
    fn mcp_servers_are_added_switched_and_removed_in_place() {
        crate::web::snapshot::tests::with_mantra_home(|| {
            let mut app = real_app();
            settings_key(&mut app, KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL));
            assert!(matches!(&app.edit, Some(e) if e.target == EditTarget::McpName));
            typed(&mut app, "bad name");
            press(&mut app, KeyCode::Enter);
            assert!(app.edit.is_some(), "a name that cannot work is refused: {:?}", app.toast);
            press(&mut app, KeyCode::Esc);
            assert!(app.edit.is_none() && app.settings_ui.mcp_name.is_none(), "esc cancels");
            settings_key(&mut app, KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL));
            typed(&mut app, "blender");
            press(&mut app, KeyCode::Enter);
            assert!(matches!(&app.edit, Some(e) if e.target == EditTarget::McpCommand));
            typed(&mut app, "uvx blender-mcp \"--port 9\"");
            press(&mut app, KeyCode::Enter);
            let disk = crate::config::Settings::load_on_disk().unwrap();
            assert_eq!((disk.mcp_servers["blender"].command.as_str(), disk.mcp_servers["blender"].args.clone()), ("uvx", vec!["blender-mcp".to_string(), "--port 9".to_string()]));
            assert!(matches!(&rows(&app)[app.settings_ui.sel].1, Row::Mcp { name, .. } if name == "blender"), "the new row is selected");
            assert!(app.toast.as_ref().is_some_and(|t| t.0 == "added blender — ctrl+t tests it"));
            press(&mut app, KeyCode::Char(' '));
            assert!(!crate::config::Settings::load_on_disk().unwrap().mcp_servers["blender"].enabled);
            app.set_setting("solo_mcp", "blender").unwrap();
            press(&mut app, KeyCode::Delete);
            assert!(app.settings.mcp_servers.contains_key("blender"), "armed only");
            press(&mut app, KeyCode::Delete);
            let disk = crate::config::Settings::load_on_disk().unwrap();
            assert!(disk.mcp_servers.is_empty() && disk.solo_mcp.is_empty() && app.settings.solo_mcp.is_empty(), "gone, from Solo's list too");
        });
    }

    /// ctrl+t starts a server off the event loop; its line arrives in its row.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ctrl_t_tests_a_server_into_its_row() {
        let mut app = crate::web::snapshot::tests::test_app();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        app.tx = tx;
        app.settings.mcp_servers.insert("nope".into(), crate::config::McpServer { command: "/nonexistent/mcp-server".into(), ..Default::default() });
        app.open_settings();
        select(&mut app, |r| matches!(r, Row::Mcp { .. }));
        settings_key(&mut app, KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
        assert_eq!(app.settings_ui.mcp_status.get("nope").map(String::as_str), Some("testing…"));
        assert!(app.animating());
        let ev = tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv()).await.expect("an answer").expect("an event");
        app.on_event(ev);
        let line = app.settings_ui.mcp_status.get("nope").cloned().unwrap_or_default();
        assert!(line.starts_with("error: could not start `/nonexistent/mcp-server`"), "{line}");
    }

    /// The selected row is the band across the form, the about panel says what it does; with
    /// the web off, its section says how to start it.
    #[test]
    fn the_form_bands_the_selected_row_and_explains_it() {
        crate::web::snapshot::tests::with_mantra_home(|| {
            let mut app = real_app();
            select(&mut app, field("approval_mode"));
            let (s, buf) = screen(&mut app, 120, 40);
            let lines: Vec<&str> = s.lines().collect();
            let y = lines.iter().position(|l| l.contains("Approvals") && l.contains("on-request")).unwrap() as u16;
            let band = theme::row_sel().bg.unwrap();
            let inner_w = 120 - 36 - 2;
            assert!((1..1 + inner_w).all(|x| buf[(x, y)].bg == band), "the whole row of the form");
            assert!(s.contains("When Solo asks"), "the hint beside it: {s}");
            assert!(s.contains("off — mantra --web starts it"), "{s}");
            for w in [80u16, 100, 160] {
                let (s, _) = screen(&mut app, w, 30);
                assert!(s.lines().all(|l| crate::util::width(l) <= w as usize), "{w}: nothing past the edge");
                assert!(s.lines().last().is_some_and(|l| l.contains("esc back") || w < 100), "{w}: {}", s.lines().last().unwrap_or_default());
            }
        });
    }

    /// Solo's MCP servers are picked from a checklist: all ticked is every server, none is none.
    #[test]
    fn solo_mcp_is_picked_from_a_checklist() {
        crate::web::snapshot::tests::with_mantra_home(|| {
            let mut app = real_app();
            app.mcp_add_server("blender", "uvx blender-mcp").unwrap();
            app.mcp_add_server("files", "npx files").unwrap();
            select(&mut app, field("solo_mcp"));
            press(&mut app, KeyCode::Enter);
            let Some(Overlay::Select(s)) = app.overlays.pop() else { panic!("the checklist") };
            assert!(s.multi && s.items.iter().all(|c| c.on), "all ticked: Solo gets every server by default");
            solo_mcp_chosen(&mut app, &["files".to_string()]);
            assert_eq!(crate::config::Settings::load_on_disk().unwrap().solo_mcp, vec!["files".to_string()]);
            solo_mcp_chosen(&mut app, &[]);
            assert!(app.settings.solo_mcp.is_empty());
            solo_mcp_chosen(&mut app, &["blender".to_string(), "files".to_string()]);
            assert_eq!(app.settings.solo_mcp, vec!["*".to_string()]);
        });
    }
}
