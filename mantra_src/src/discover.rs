//! Model discovery: `GET {base_url}/models` on OpenAI-compatible providers, plus turning Codex's
//! own catalog and provider listings into registry candidates the user picks from.
//!
//! Uses the system `curl` (present on every Linux/macOS box) so Mantra stays small; the API key is
//! passed on stdin (`-H @-`), never on the command line where `ps` could see it.

use crate::config::{ModelEntry, Registry};
use serde_json::Value;
use std::io::Write;
use std::process::{Command, Stdio};

/// Context assumed when a provider doesn't report one (conservative: auto-compaction must never
/// aim past a model's real limit).
pub const ASSUMED_CONTEXT: u64 = 200_000;

#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    pub id: String,
    /// Context window reported by the API, if any.
    pub context: Option<u64>,
    /// Whether the API says the model takes a reasoning effort (None = not stated).
    pub reasoning: Option<bool>,
}

/// Keys different providers use for the context window (OpenRouter, Together, vLLM, Groq, Mistral, LiteLLM, Gemini, llama.cpp …).
const CTX_KEYS: &[&str] = &[
    "context_length", "context_window", "max_context_length", "max_model_len", "max_input_tokens", "input_token_limit",
    "inputTokenLimit", "contextWindow", "context_size", "max_context_tokens", "max_context_window", "n_ctx", "max_seq_len",
];
const NESTED: &[&str] = &["top_provider", "limits", "limit", "capabilities", "metadata", "meta", "info", "architecture"];
const NOT_CHAT: &[&str] = &["embed", "whisper", "tts", "dall-e", "moderation", "rerank", "transcri", "speech", "audio", "image-gen", "text-to-image"];

pub fn list_models(base_url: &str, env_key: &str) -> Result<Vec<Found>, String> {
    let base = base_url.trim().trim_end_matches('/');
    if base.is_empty() {
        return Err("set the provider's base URL first".into());
    }
    let key = if env_key.trim().is_empty() {
        None
    } else {
        Some(std::env::var(env_key.trim()).map_err(|_| format!("${} is not set in the environment Mantra was started from", env_key.trim()))?)
    };
    let mut urls = vec![format!("{base}/models")];
    if !base.ends_with("/v1") {
        urls.push(format!("{base}/v1/models"));
    }
    let mut last = String::new();
    for url in urls {
        match curl_get(&url, key.as_deref()) {
            Ok((200..=299, body)) => {
                return parse_models(&body).ok_or_else(|| format!("{url} answered, but not with a model list: {}", crate::util::trunc(body.trim(), 120)));
            }
            Ok((code, body)) => last = format!("HTTP {code} from {url}: {}", crate::util::trunc(body.trim(), 120)),
            Err(e) => last = e,
        }
    }
    Err(last)
}

fn curl_get(url: &str, key: Option<&str>) -> Result<(u16, String), String> {
    let mut c = Command::new("curl");
    c.args(["-sS", "-L", "--max-time", "20", "-H", "Accept: application/json", "-w", "\n%{http_code}"]);
    if key.is_some() {
        c.args(["-H", "@-"]);
    }
    c.arg(url).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = c.spawn().map_err(|e| format!("couldn't run curl ({e}) — is it installed?"))?;
    if let Some(mut stdin) = child.stdin.take() {
        if let Some(k) = key {
            let _ = writeln!(stdin, "Authorization: Bearer {k}");
        }
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    let (body, code) = text.rsplit_once('\n').unwrap_or(("", text.as_str()));
    let code: u16 = code.trim().parse().unwrap_or(0);
    if code == 0 {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(format!("couldn't reach {url}: {}", if err.is_empty() { "no response".into() } else { err }));
    }
    Ok((code, body.to_string()))
}

fn num(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => n.as_u64().or_else(|| n.as_f64().map(|f| f as u64)),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn context_of(item: &Value) -> Option<u64> {
    let direct = |o: &Value| CTX_KEYS.iter().find_map(|k| o.get(*k).and_then(num)).filter(|n| *n >= 1024);
    direct(item).or_else(|| NESTED.iter().find_map(|k| item.get(*k).and_then(direct)))
}

/// OpenRouter lists `supported_parameters`; others expose a capability flag.
fn reasoning_of(item: &Value) -> Option<bool> {
    if let Some(arr) = item.get("supported_parameters").and_then(|v| v.as_array()) {
        return Some(arr.iter().filter_map(|x| x.as_str()).any(|p| p == "reasoning" || p == "include_reasoning" || p == "reasoning_effort"));
    }
    for o in [Some(item), item.get("capabilities"), item.get("features")].into_iter().flatten() {
        for k in ["reasoning", "supports_reasoning", "reasoning_effort", "thinking"] {
            if let Some(b) = o.get(k).and_then(|v| v.as_bool()) {
                return Some(b);
            }
        }
    }
    None
}

/// Accepts `{"data":[…]}`, `{"models":[…]}` or a bare array; items may be objects or strings.
pub fn parse_models(body: &str) -> Option<Vec<Found>> {
    let v: Value = serde_json::from_str(body.trim()).ok()?;
    let arr = v.get("data").or_else(|| v.get("models")).unwrap_or(&v).as_array()?;
    let mut out: Vec<Found> = vec![];
    for item in arr {
        let id = match item {
            Value::String(s) => s.clone(),
            _ => ["id", "name", "model"].iter().find_map(|k| item.get(*k).and_then(|x| x.as_str())).unwrap_or("").to_string(),
        };
        let id = id.trim().trim_start_matches("models/").to_string();
        let lower = id.to_lowercase();
        let kind = item.get("type").and_then(|t| t.as_str()).unwrap_or("").to_lowercase();
        if id.is_empty() || NOT_CHAT.iter().any(|x| lower.contains(x)) || ["embedding", "embeddings", "image", "audio", "rerank", "moderation"].contains(&kind.as_str()) {
            continue;
        }
        if !out.iter().any(|f| f.id == id) {
            out.push(Found { context: context_of(item), reasoning: reasoning_of(item), id });
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Some(out)
}

/// A short, unique alias for a discovered model ("z-ai/glm-4.6" → "glm-4.6", else "zai-glm-4.6", …).
pub fn unique_alias(model: &str, provider: &str, taken: &[String]) -> String {
    let clean = |s: &str| -> String {
        let t: String = s.to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' { c } else { '-' }).collect();
        t.trim_matches('-').to_string()
    };
    let base = clean(model.rsplit('/').next().unwrap_or(model));
    let base = if base.is_empty() { "model".to_string() } else { base };
    let prov = clean(provider);
    let mut cand = vec![base.clone(), format!("{prov}-{base}")];
    for n in 2..100 {
        cand.push(format!("{prov}-{base}-{n}"));
    }
    cand.into_iter().find(|c| !taken.contains(c)).unwrap_or(base)
}


#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CState {
    New,
    /// Already configured, but discovery can fill in something missing.
    Update,
    Configured,
}

/// One row in the discovery picker.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub provider: String,
    pub model: String,
    pub context: Option<u64>,
    pub context_known: bool,
    pub efforts: Vec<String>,
    pub default_effort: String,
    pub note: String,
    pub state: CState,
    pub selected: bool,
}

fn strs(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

/// A model found on a custom provider.
pub fn from_provider(provider: &str, f: &Found) -> Candidate {
    let reasoning = f.reasoning == Some(true);
    Candidate {
        provider: provider.to_string(),
        model: f.id.clone(),
        context: Some(f.context.unwrap_or(ASSUMED_CONTEXT)),
        context_known: f.context.is_some(),
        efforts: if reasoning { strs(&["low", "medium", "high"]) } else { vec![] },
        default_effort: if reasoning { "medium".into() } else { String::new() },
        note: format!("{provider} · ctx {}", if f.context.is_some() { "from API" } else { "assumed 200k" }),
        state: CState::New,
        selected: false,
    }
}

/// A model from Codex's own catalog (`model/list`). Codex knows these models' context windows itself.
pub fn from_codex(v: &Value) -> Option<Candidate> {
    let id = v.get("id").and_then(|x| x.as_str())?.to_string();
    if id.is_empty() || v.get("hidden").and_then(|h| h.as_bool()).unwrap_or(false) {
        return None;
    }
    let efforts: Vec<String> = v.get("supportedReasoningEfforts").and_then(|x| x.as_array()).map(|a| a.iter().filter_map(|e| e.get("reasoningEffort").and_then(|r| r.as_str()).map(|s| s.to_string())).collect()).unwrap_or_default();
    Some(Candidate {
        provider: "openai".into(),
        model: id,
        context: None,
        context_known: true,
        default_effort: v.get("defaultReasoningEffort").and_then(|x| x.as_str()).unwrap_or("medium").to_string(),
        efforts,
        note: v.get("description").and_then(|x| x.as_str()).unwrap_or("").chars().take(60).collect(),
        state: CState::New,
        selected: false,
    })
}

fn existing<'a>(reg: &'a Registry, c: &Candidate) -> Option<&'a ModelEntry> {
    reg.models.iter().find(|m| m.model == c.model && (m.provider == c.provider || (m.provider.is_empty() && c.provider == "openai")))
}

/// Mark candidates as new / updatable / already configured.
pub fn classify(reg: &Registry, c: &mut Candidate) {
    c.state = match existing(reg, c) {
        None => CState::New,
        Some(m) => {
            let missing_ctx = m.is_custom_provider() && m.context_window.is_none() && c.context.is_some();
            let missing_eff = m.efforts.is_empty() && !c.efforts.is_empty() && m.is_custom_provider();
            let changed_eff = !m.is_custom_provider() && !c.efforts.is_empty() && m.efforts != c.efforts;
            if missing_ctx || missing_eff || changed_eff {
                CState::Update
            } else {
                CState::Configured
            }
        }
    };
}

/// Add/update the selected candidates. Existing values the user set are never overwritten,
/// except OpenAI effort lists, which Codex's catalog is authoritative for.
pub fn apply(reg: &mut Registry, cands: &[Candidate]) -> (usize, usize) {
    let (mut added, mut updated) = (0, 0);
    for c in cands.iter().filter(|c| c.selected) {
        let custom = c.provider != "openai";
        match c.state {
            CState::New if existing(reg, c).is_none() => {
                let taken: Vec<String> = reg.models.iter().map(|m| m.alias.clone()).collect();
                reg.models.push(ModelEntry {
                    alias: unique_alias(&c.model, &c.provider, &taken),
                    provider: c.provider.clone(),
                    model: c.model.clone(),
                    context_window: if custom { c.context } else { None },
                    auto_compact_percent: if custom { Some(85) } else { None },
                    default_effort: c.default_effort.clone(),
                    efforts: c.efforts.clone(),
                    note: c.note.clone(),
                });
                added += 1;
            }
            CState::Update => {
                if let Some(m) = reg.models.iter_mut().find(|m| m.model == c.model && (m.provider == c.provider || (m.provider.is_empty() && !custom))) {
                    if custom {
                        if m.context_window.is_none() {
                            m.context_window = c.context;
                        }
                        if m.auto_compact_percent.is_none() {
                            m.auto_compact_percent = Some(85);
                        }
                        if m.efforts.is_empty() && !c.efforts.is_empty() {
                            m.efforts = c.efforts.clone();
                            m.default_effort = c.default_effort.clone();
                        }
                    } else if !c.efforts.is_empty() {
                        m.efforts = c.efforts.clone();
                        if !m.efforts.contains(&m.default_effort) {
                            m.default_effort = c.default_effort.clone();
                        }
                    }
                    updated += 1;
                }
            }
            _ => {}
        }
    }
    (added, updated)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_common_provider_shapes() {
        let openai = r#"{"object":"list","data":[{"id":"glm-4.6","object":"model"},{"id":"text-embedding-3-small"},{"id":"glm-4.5-air"}]}"#;
        let m = parse_models(openai).unwrap();
        assert_eq!(m.iter().map(|f| f.id.as_str()).collect::<Vec<_>>(), vec!["glm-4.5-air", "glm-4.6"]);
        assert!(m.iter().all(|f| f.context.is_none()));

        let openrouter = r#"{"data":[{"id":"z-ai/glm-4.6","context_length":202752,"top_provider":{"context_length":131072}}]}"#;
        assert_eq!(parse_models(openrouter).unwrap()[0].context, Some(202752));
        let nested = r#"{"data":[{"id":"x","top_provider":{"context_length":"131072"}}]}"#;
        assert_eq!(parse_models(nested).unwrap()[0].context, Some(131072));
        let vllm = r#"{"data":[{"id":"Qwen/Qwen3-Coder","max_model_len":262144}]}"#;
        assert_eq!(parse_models(vllm).unwrap()[0].context, Some(262144));
        let groq = r#"{"data":[{"id":"llama","context_window":131072},{"id":"whisper-large-v3","context_window":448}]}"#;
        assert_eq!(parse_models(groq).unwrap().len(), 1);
        let bare = r#"["a","b"]"#;
        assert_eq!(parse_models(bare).unwrap().len(), 2);
        let google = r#"{"models":[{"name":"models/gemini-x","inputTokenLimit":1048576}]}"#;
        assert_eq!(parse_models(google).unwrap()[0], Found { id: "gemini-x".into(), context: Some(1048576), reasoning: None });
        assert!(parse_models("<html>nope</html>").is_none());
    }
    #[test]
    fn detects_reasoning_and_builds_candidates() {
        let or = r#"{"data":[{"id":"z-ai/glm-5.2","context_length":202752,"supported_parameters":["tools","reasoning"]},{"id":"meta/llama-4","supported_parameters":["tools"]},{"id":"plain"}]}"#;
        let m = parse_models(or).unwrap();
        let get = |id: &str| m.iter().find(|f| f.id == id).unwrap().clone();
        assert_eq!(get("z-ai/glm-5.2").reasoning, Some(true));
        assert_eq!(get("meta/llama-4").reasoning, Some(false));
        assert_eq!(get("plain").reasoning, None);
        let c = from_provider("zai", &get("z-ai/glm-5.2"));
        assert_eq!((c.context, c.context_known, c.efforts.len()), (Some(202752), true, 3));
        let c = from_provider("zai", &get("plain"));
        assert_eq!((c.context, c.context_known), (Some(ASSUMED_CONTEXT), false), "unknown context assumes 200k");
        assert!(c.efforts.is_empty() && c.default_effort.is_empty(), "no effort unless the provider says so");
    }
    #[test]
    fn apply_adds_and_fills_without_clobbering() {
        let mut reg = Registry::defaults();
        let n = reg.models.len();
        let mut a = from_provider("zai", &Found { id: "glm-5.2".into(), context: None, reasoning: Some(true) });
        classify(&reg, &mut a);
        assert_eq!(a.state, CState::New);
        a.selected = true;
        assert_eq!(apply(&mut reg, &[a.clone()]), (1, 0));
        assert_eq!(reg.models.len(), n + 1);
        let m = reg.models.last().unwrap();
        assert_eq!((m.provider.as_str(), m.context_window, m.auto_compact_percent), ("zai", Some(ASSUMED_CONTEXT), Some(85)));
        classify(&reg, &mut a);
        assert_eq!(a.state, CState::Configured, "second discovery doesn't duplicate");
        // user-set context survives
        reg.models.last_mut().unwrap().context_window = Some(128_000);
        let mut b = from_provider("zai", &Found { id: "glm-5.2".into(), context: Some(999_999), reasoning: Some(true) });
        classify(&reg, &mut b);
        b.selected = true;
        apply(&mut reg, &[b]);
        assert_eq!(reg.models.last().unwrap().context_window, Some(128_000));
    }
    #[test]
    fn aliases_are_unique() {
        let taken = vec!["glm-4.6".to_string()];
        assert_eq!(unique_alias("z-ai/glm-4.6", "zai", &taken), "zai-glm-4.6");
        assert_eq!(unique_alias("glm-4.5", "zai", &taken), "glm-4.5");
    }
}
