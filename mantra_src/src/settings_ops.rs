//! The settings both UIs edit — the TUI's `/settings` screen and the web's settings page: one list
//! of fields to render from (`Settings::fields`) and one setter that validates (`set_field`), so
//! neither UI keeps rules of its own. MCP servers are rows, not fields: `mcp_add`, `mcp_remove`
//! and `mcp_set_enabled` are what `mantra mcp` and both UIs change them with.

use crate::config::{mcp_name_problem, McpServer, Registry, Settings, EXPERIMENTAL};

pub const SECTION_SOLO: &str = "Solo";
pub const SECTION_RUNS: &str = "Runs";
pub const SECTION_FEATURES: &str = "Features";
pub const SECTION_WEB: &str = "Web & remote";
pub const SECTION_APPEARANCE: &str = "Appearance";

pub const APPROVAL_MODES: &[&str] = &["untrusted", "on-request", "never", crate::config::BYPASS_SANDBOX_DANGER];
pub const SANDBOXES: &[&str] = &["read-only", "workspace-write", "danger-full-access"];
const GLYPHS: &[&str] = &["auto", "unicode", "ascii"];
const COLORS: &[&str] = &["auto", "truecolor", "256", "16"];
/// The event loop draws at 4–60 frames a second whatever the file says.
const FPS: (u32, u32) = (4, 60);

/// What a field holds, and so how a UI edits it.
#[derive(Debug, Clone, PartialEq)]
pub enum FieldKind {
    Text,
    /// Never shown: the value reads "set" or "" — typing a new one replaces it, empty clears it.
    Secret,
    /// One of these (cycled with ←→ or space, a letter jumps to the first that starts with it).
    Enum(Vec<String>),
    /// "on" / "off".
    Bool,
    Number { min: u32, max: u32 },
}

/// One editable setting as both UIs show it.
#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    pub section: &'static str,
    /// What `set_field` takes: the settings.toml key (`web.listen`), `experimental.<name>`.
    pub key: String,
    pub label: String,
    pub kind: FieldKind,
    pub value: String,
    /// One line on what it does, for the "about this setting" panel.
    pub hint: String,
}

fn on_off(b: bool) -> String {
    String::from(if b { "on" } else { "off" })
}

fn options(v: &[&str]) -> FieldKind {
    FieldKind::Enum(v.iter().map(|s| s.to_string()).collect())
}

/// "on"/"off" and the other ways people write a yes or a no.
fn parse_bool(key: &str, v: &str) -> Result<bool, String> {
    match v.trim().to_ascii_lowercase().as_str() {
        "on" | "true" | "yes" | "1" => Ok(true),
        "off" | "false" | "no" | "0" => Ok(false),
        _ => Err(format!("{key}: on or off, not '{}'", crate::util::trunc(v.trim(), 20))),
    }
}

fn one_of(key: &str, v: &str, allowed: &[&str]) -> Result<String, String> {
    let v = v.trim();
    allowed.iter().find(|a| a.eq_ignore_ascii_case(v)).map(|a| a.to_string()).ok_or_else(|| format!("{key}: one of {}", allowed.join(", ")))
}

/// When a change made through `set_field` reaches the app: "" now, else when — for the toast.
pub fn takes_effect(key: &str) -> &'static str {
    match key {
        "default_model" | "sandbox" | "solo_mcp" => "Solo's next session (/new) uses it",
        "glyphs" | "colors" | "reduce_motion" | "mouse" | "fps" => "applies at the next start",
        k if k.starts_with("web.") => "applies at the next start",
        k if k.starts_with("experimental.") => "new runs use it",
        _ => "",
    }
}

impl Settings {
    /// Every editable setting, section by section, as both UIs render them. `reg` gives the model
    /// aliases to choose from and `patterns` the pattern names (`Pattern::list`); MCP servers are
    /// rows of their own (`mcp_servers`), not fields.
    pub fn fields(&self, reg: &Registry, patterns: &[String]) -> Vec<Field> {
        let f = |section: &'static str, key: &str, label: &str, kind: FieldKind, value: String, hint: &str| Field { section, key: key.into(), label: label.into(), kind, value, hint: hint.into() };
        let aliases = FieldKind::Enum(reg.models.iter().map(|m| m.alias.clone()).collect());
        let mut servers = vec!["all".to_string(), "none".to_string()];
        servers.extend(self.mcp_servers.keys().cloned());
        let solo_mcp = match self.solo_mcp.as_slice() {
            [] => "none".to_string(),
            v if v.iter().any(|s| s.trim() == "*") => "all".to_string(),
            v => v.join(", "),
        };
        let mut out = vec![
            f(SECTION_SOLO, "default_model", "Model", aliases, self.default_model.clone(), "The model a new Solo session starts on (ctrl+k switches the live one)."),
            f(SECTION_SOLO, "approval_mode", "Approvals", options(APPROVAL_MODES), self.approval_mode.clone(), "never skips prompts but keeps the Sandbox setting. BYPASS-SANDBOX-DANGER skips prompts AND the sandbox; entering or leaving it needs /new. Shift+tab cycles the ordinary approval modes."),
            f(SECTION_SOLO, "sandbox", "Sandbox", options(SANDBOXES), self.sandbox.clone(), "What Solo's commands may touch: read-only, the project (workspace-write), or everything."),
            f(SECTION_SOLO, "solo_mcp", "MCP servers", FieldKind::Enum(servers), solo_mcp, "Which MCP servers Solo agents get: all enabled ones, none, or the ones named."),
            f(SECTION_RUNS, "default_pattern", "Pattern", FieldKind::Enum(patterns.to_vec()), self.default_pattern.clone(), "The pattern new runs use: its roles, models and flow (/studio edits them)."),
            f(SECTION_RUNS, "notify", "Notifications", FieldKind::Bool, on_off(self.notify), "A desktop notification (or the bell) when a run needs you or finishes."),
        ];
        for (name, about) in EXPERIMENTAL {
            let label = name.chars().next().map(|c| c.to_uppercase().chain(name.chars().skip(1)).collect()).unwrap_or_default();
            out.push(Field { section: SECTION_FEATURES, key: format!("experimental.{name}"), label, kind: FieldKind::Bool, value: on_off(self.experimental_on(name)), hint: format!("Experimental: {about}.") });
        }
        out.extend([
            f(SECTION_WEB, "web.listen", "Listen on", FieldKind::Text, self.web.listen.clone(), "Where --web listens: ADDR:PORT, or a port for 127.0.0.1 (empty: 127.0.0.1:7777)."),
            f(SECTION_WEB, "web.password", "Password", FieldKind::Secret, if self.web.password.is_empty() { String::new() } else { "set".into() }, "Needed off localhost; also derives the remote link's key. Empty clears it ($MANTRA_WEB_PASSWORD wins)."),
            f(SECTION_WEB, "web.tls", "HTTPS", FieldKind::Bool, on_off(self.web.tls), "Serve the web UI over HTTPS with Mantra's own certificate (install its CA from /cert.pem)."),
            f(SECTION_WEB, "web.push", "Push", FieldKind::Bool, on_off(self.web.push), "Push notifications to the phones and browsers that subscribed."),
            f(SECTION_APPEARANCE, "glyphs", "Glyphs", options(GLYPHS), self.glyphs.clone(), "Unicode symbols or plain ASCII (auto follows the terminal's locale)."),
            f(SECTION_APPEARANCE, "colors", "Colours", options(COLORS), self.colors.clone(), "Colour depth (auto reads $COLORTERM and $TERM)."),
            f(SECTION_APPEARANCE, "reduce_motion", "Reduce motion", FieldKind::Bool, on_off(self.reduce_motion), "No spinners or transitions; still frames only."),
            f(SECTION_APPEARANCE, "mouse", "Mouse", FieldKind::Bool, on_off(self.mouse), "Clicks and the wheel in the TUI (off lets the terminal select text)."),
            f(SECTION_APPEARANCE, "fps", "Frames a second", FieldKind::Number { min: FPS.0, max: FPS.1 }, self.fps.to_string(), "How often the screen redraws while something moves (idle draws nothing)."),
            f(SECTION_APPEARANCE, "side_panel", "Side panel", FieldKind::Bool, on_off(self.side_panel), "The activity panel beside a chat on wide terminals (ctrl+t toggles it)."),
        ]);
        out
    }

    /// Sets one field from what a UI sends (`Field::key`, the value as text), or says why not.
    /// Checks what settings alone can know; that `default_model` is an alias in `/models` and
    /// `default_pattern` a pattern that loads is `App::apply_settings`' to check.
    pub fn set_field(&mut self, key: &str, value: &str) -> Result<(), String> {
        let v = value.trim();
        let name = |what: &str| -> Result<String, String> {
            if v.is_empty() || v.contains(char::is_whitespace) {
                Err(format!("{key}: give a {what} name"))
            } else {
                Ok(v.to_string())
            }
        };
        match key {
            "default_model" => self.default_model = name("model")?,
            "default_pattern" => self.default_pattern = name("pattern")?,
            "approval_mode" => self.approval_mode = one_of(key, v, APPROVAL_MODES)?,
            "sandbox" => self.sandbox = one_of(key, v, SANDBOXES)?,
            "glyphs" => self.glyphs = one_of(key, v, GLYPHS)?,
            "colors" => self.colors = one_of(key, v, COLORS)?,
            "solo_mcp" => self.solo_mcp = self.parse_solo_mcp(v)?,
            "notify" => self.notify = parse_bool(key, v)?,
            "reduce_motion" => self.reduce_motion = parse_bool(key, v)?,
            "mouse" => self.mouse = parse_bool(key, v)?,
            "side_panel" => self.side_panel = parse_bool(key, v)?,
            "fps" => {
                self.fps = v.parse::<u32>().ok().filter(|n| (FPS.0..=FPS.1).contains(n)).ok_or_else(|| format!("fps: a number from {} to {}", FPS.0, FPS.1))?;
            }
            "web.listen" => {
                if !v.is_empty() && crate::web::parse_listen(v).is_none() {
                    return Err("web.listen: ADDR:PORT (e.g. 0.0.0.0:7777) or a port".into());
                }
                self.web.listen = v.to_string();
            }
            // Not trimmed: a password is what it is.
            "web.password" => self.web.password = value.to_string(),
            "web.tls" => self.web.tls = parse_bool(key, v)?,
            "web.push" => self.web.push = parse_bool(key, v)?,
            k => match k.strip_prefix("experimental.") {
                Some(feature) => self.experimental_set(feature, parse_bool(key, v)?)?,
                None => return Err(format!("no setting named {k}")),
            },
        }
        Ok(())
    }

    /// `solo_mcp` from a UI: `all` (or `*`) every enabled server, `none` (or nothing) none, else
    /// names — comma- or space-separated, each one configured.
    fn parse_solo_mcp(&self, v: &str) -> Result<Vec<String>, String> {
        let words: Vec<&str> = v.split(|c: char| c == ',' || c.is_whitespace()).filter(|w| !w.is_empty()).collect();
        match words.as_slice() {
            [] | ["none"] => Ok(vec![]),
            ["all"] | ["*"] => Ok(vec!["*".into()]),
            names => match names.iter().find(|n| !self.mcp_servers.contains_key(**n)) {
                Some(unknown) => Err(format!("no MCP server named {unknown}")),
                None => Ok(names.iter().map(|n| n.to_string()).collect()),
            },
        }
    }

    /// Switches an experimental feature (`EXPERIMENTAL`) on or off; an unknown name is refused.
    pub fn experimental_set(&mut self, name: &str, on: bool) -> Result<(), String> {
        if !EXPERIMENTAL.iter().any(|(n, _)| *n == name) {
            let known: Vec<&str> = EXPERIMENTAL.iter().map(|(n, _)| *n).collect();
            return Err(format!("unknown experimental feature '{name}' (there is: {})", known.join(", ")));
        }
        self.experimental.retain(|f| f != name);
        if on {
            self.experimental.push(name.to_string());
        }
        Ok(())
    }

    /// `Ok` when `name` is a configured server, else what there is instead.
    pub fn mcp_known(&self, name: &str) -> Result<(), String> {
        if self.mcp_servers.contains_key(name) {
            return Ok(());
        }
        let have: Vec<&str> = self.mcp_servers.keys().map(String::as_str).collect();
        Err(format!("no MCP server named {name}{}", if have.is_empty() { " (none configured)".to_string() } else { format!(" (there is: {})", have.join(", ")) }))
    }

    /// Adds a server under a name that works everywhere (`mcp_name_problem`) and is not taken.
    pub fn mcp_add(&mut self, name: &str, server: McpServer) -> Result<(), String> {
        if let Some(why) = mcp_name_problem(name) {
            return Err(why);
        }
        if server.command.trim().is_empty() {
            return Err(format!("{name}: a server needs a command"));
        }
        if self.mcp_servers.contains_key(name) {
            return Err(format!("{name} is already configured — remove it first"));
        }
        self.mcp_servers.insert(name.to_string(), server);
        Ok(())
    }

    pub fn mcp_remove(&mut self, name: &str) -> Result<(), String> {
        self.mcp_known(name)?;
        self.mcp_servers.remove(name);
        Ok(())
    }

    pub fn mcp_set_enabled(&mut self, name: &str, on: bool) -> Result<(), String> {
        self.mcp_known(name)?;
        if let Some(s) = self.mcp_servers.get_mut(name) {
            s.enabled = on;
        }
        Ok(())
    }
}

/// A command line as one types it into a UI's "command" field — `uvx blender-mcp`, `npx -y
/// server "/tmp/a b"` — split into words: whitespace separates, single and double quotes group,
/// a backslash outside single quotes takes the next character as it is. Refused unbalanced.
pub fn parse_command_line(line: &str) -> Result<Vec<String>, String> {
    let (mut words, mut cur, mut quote, mut in_word) = (Vec::new(), String::new(), None::<char>, false);
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some('\''), c) => cur.push(c),
            (_, '\\') => match chars.next() {
                Some(n) => {
                    cur.push(n);
                    in_word = true;
                }
                None => return Err("a command line cannot end in a backslash".into()),
            },
            (Some(_), c) => cur.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                in_word = true;
            }
            (None, c) if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            (None, c) => {
                cur.push(c);
                in_word = true;
            }
        }
    }
    if quote.is_some() {
        return Err("a quote is not closed".into());
    }
    if in_word {
        words.push(cur);
    }
    Ok(words)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every field `fields` lists can be set back to the value it shows (the round trip both UIs
    /// rely on), and the list covers the sections the screen shows, each key once.
    #[test]
    fn every_field_takes_back_its_own_value() {
        let mut reg = Registry::defaults();
        reg.providers.push(crate::config::bija_default_provider());
        reg.models.extend(crate::config::bija_default_model_entries("bija"));
        let patterns = vec!["mantra-default".to_string(), "mantra-lean".to_string()];
        let s = Settings::default();
        let fields = s.fields(&reg, &patterns);
        let mut keys: Vec<&str> = fields.iter().map(|f| f.key.as_str()).collect();
        keys.sort();
        let n = keys.len();
        keys.dedup();
        assert_eq!(keys.len(), n, "each key once");
        for section in [SECTION_SOLO, SECTION_RUNS, SECTION_FEATURES, SECTION_WEB, SECTION_APPEARANCE] {
            assert!(fields.iter().any(|f| f.section == section), "{section}");
        }
        for f in fields.iter().filter(|f| f.kind != FieldKind::Secret) {
            let mut t = s.clone();
            t.set_field(&f.key, &f.value).unwrap_or_else(|e| panic!("{}: {e}", f.key));
            assert_eq!(t.fields(&reg, &patterns), fields, "{} = {}", f.key, f.value);
            assert!(!f.hint.is_empty() && !f.label.is_empty(), "{}", f.key);
            if let FieldKind::Enum(o) = &f.kind {
                assert!(o.contains(&f.value), "{}: {} among {o:?}", f.key, f.value);
            }
        }
        let model = fields.iter().find(|f| f.key == "default_model").unwrap();
        assert_eq!(model.kind, FieldKind::Enum(vec!["astra".into(), "sol".into(), "terra".into(), "luna".into()]));
        assert_eq!(fields.iter().find(|f| f.key == "experimental.akasha").map(|f| (f.label.as_str(), f.value.as_str())), Some(("Akasha", "off")));
    }

    #[test]
    fn set_field_validates_and_says_why() {
        let mut s = Settings::default();
        for (key, value, words) in [
            ("approval_mode", "sometimes", "untrusted, on-request, never"),
            ("sandbox", "jail", "read-only"),
            ("glyphs", "emoji", "ascii"),
            ("fps", "120", "from 4 to 60"),
            ("fps", "fast", "from 4 to 60"),
            ("notify", "maybe", "on or off"),
            ("web.listen", "my laptop", "ADDR:PORT"),
            ("default_model", "", "model name"),
            ("default_pattern", "two words", "pattern name"),
            ("solo_mcp", "blender", "no MCP server named blender"),
            ("experimental.warp", "on", "unknown experimental feature 'warp'"),
            ("codex_command", "x", "no setting named codex_command"),
        ] {
            let e = s.set_field(key, value).expect_err(key);
            assert!(e.contains(words), "{key}={value}: {e}");
        }
        assert_eq!(s.fields(&Registry::defaults(), &[]), Settings::default().fields(&Registry::defaults(), &[]), "a refusal changes nothing");
        s.set_field("approval_mode", " Never ").unwrap();
        s.set_field("fps", "30").unwrap();
        s.set_field("web.listen", "0.0.0.0:7777").unwrap();
        s.set_field("web.tls", "yes").unwrap();
        s.set_field("experimental.akasha", "on").unwrap();
        assert_eq!((s.approval_mode.as_str(), s.fps, s.web.listen.as_str(), s.web.tls), ("never", 30, "0.0.0.0:7777", true));
        assert!(s.experimental_on("akasha"));
        s.set_field("experimental.akasha", "off").unwrap();
        assert!(s.experimental.is_empty());
        // the password: kept as typed, shown only as "set", cleared by an empty one
        s.set_field("web.password", " pw with spaces ").unwrap();
        assert_eq!(s.web.password, " pw with spaces ");
        let pw = s.fields(&Registry::defaults(), &[]).into_iter().find(|f| f.key == "web.password").unwrap();
        assert_eq!((pw.kind, pw.value.as_str()), (FieldKind::Secret, "set"));
        s.set_field("web.password", "").unwrap();
        assert!(s.web.password.is_empty());
    }

    /// Solo's MCP servers: all, none, or configured names — and the field shows which.
    #[test]
    fn solo_mcp_is_all_none_or_names() {
        let mut s = Settings::default();
        s.mcp_add("blender", McpServer { command: "uvx".into(), args: vec!["blender-mcp".into()], ..Default::default() }).unwrap();
        s.mcp_add("files", McpServer { command: "npx".into(), ..Default::default() }).unwrap();
        let shown = |s: &Settings| s.fields(&Registry::defaults(), &[]).into_iter().find(|f| f.key == "solo_mcp").unwrap();
        assert_eq!(shown(&s).value, "all");
        assert_eq!(shown(&s).kind, FieldKind::Enum(vec!["all".into(), "none".into(), "blender".into(), "files".into()]));
        s.set_field("solo_mcp", "files, blender").unwrap();
        assert_eq!((s.solo_mcp.clone(), shown(&s).value), (vec!["files".to_string(), "blender".to_string()], "files, blender".to_string()));
        s.set_field("solo_mcp", "none").unwrap();
        assert!(s.solo_mcp.is_empty());
        s.set_field("solo_mcp", "*").unwrap();
        assert_eq!(s.solo_mcp, vec!["*".to_string()]);
    }

    #[test]
    fn mcp_servers_are_added_switched_and_removed_by_name() {
        let mut s = Settings::default();
        let uvx = || McpServer { command: "uvx".into(), ..Default::default() };
        assert!(s.mcp_add("a/b", uvx()).unwrap_err().contains("letters"));
        assert!(s.mcp_add("mantra", uvx()).unwrap_err().contains("Mantra's own"));
        assert!(s.mcp_add("x", McpServer::default()).unwrap_err().contains("needs a command"));
        s.mcp_add("blender", uvx()).unwrap();
        assert!(s.mcp_add("blender", uvx()).unwrap_err().contains("already configured"));
        s.mcp_set_enabled("blender", false).unwrap();
        assert!(!s.mcp_servers["blender"].enabled);
        assert_eq!(s.mcp_set_enabled("nope", true).unwrap_err(), "no MCP server named nope (there is: blender)");
        s.mcp_remove("blender").unwrap();
        assert_eq!(s.mcp_remove("blender").unwrap_err(), "no MCP server named blender (none configured)");
    }

    #[test]
    fn a_command_line_splits_like_a_shell_would() {
        let w = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(parse_command_line("uvx blender-mcp").unwrap(), w(&["uvx", "blender-mcp"]));
        assert_eq!(parse_command_line("  npx -y srv \"/tmp/a b\"  ").unwrap(), w(&["npx", "-y", "srv", "/tmp/a b"]));
        assert_eq!(parse_command_line(r#"sh -c 'echo "hi"; exit 3'"#).unwrap(), w(&["sh", "-c", r#"echo "hi"; exit 3"#]));
        assert_eq!(parse_command_line(r"a\ b '' c").unwrap(), w(&["a b", "", "c"]));
        assert!(parse_command_line("").unwrap().is_empty());
        assert!(parse_command_line("x 'open").unwrap_err().contains("quote"));
        assert!(parse_command_line("x \\").unwrap_err().contains("backslash"));
    }

    #[test]
    fn what_applies_when() {
        assert_eq!(takes_effect("approval_mode"), "");
        assert_eq!(takes_effect("side_panel"), "");
        assert_eq!(takes_effect("default_model"), "Solo's next session (/new) uses it");
        assert_eq!(takes_effect("web.tls"), "applies at the next start");
        assert_eq!(takes_effect("experimental.akasha"), "new runs use it");
    }
}
