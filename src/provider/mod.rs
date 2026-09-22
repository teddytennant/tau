pub mod anthropic;
pub mod mock;
pub mod openai;
pub mod responses;

use crate::config::{Auth, Settings};
use crate::log::{Call, Msg, Usage};
use anyhow::{Result, bail};
use serde_json::{Value, json};

pub struct Reply {
    pub text: String,
    pub calls: Vec<Call>,
    pub raw: Option<Value>,
    pub usage: Usage,
}

pub trait Provider: Send {
    fn name(&self) -> &str;
    fn model(&self) -> &str;
    /// Streams text through `on_text` and returns the finished reply.
    fn complete(
        &mut self,
        system: &str,
        msgs: &[Msg],
        on_text: &mut dyn FnMut(&str),
    ) -> Result<Reply>;
    /// One of `config::THINKING`.
    fn set_thinking(&mut self, _level: &str) {}
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    Anthropic,
    OpenAi,
    /// A ChatGPT subscription over the Responses API.
    ChatGpt,
    Mock,
}

pub struct Known {
    pub name: &'static str,
    pub label: &'static str,
    pub kind: Kind,
    pub base: &'static str,
    pub env: &'static str,
    /// Used when the live model list cannot be read.
    pub fallback: &'static str,
    /// First one of these present in the live list is the suggested default.
    pub prefer: &'static [&'static str],
}

pub const KNOWN: [Known; 4] = [
    Known {
        name: "anthropic",
        label: "Anthropic",
        kind: Kind::Anthropic,
        base: "https://api.anthropic.com",
        env: "ANTHROPIC_API_KEY",
        fallback: "claude-opus-5-5",
        prefer: &["claude-opus-5-5", "claude-opus-5", "claude-sonnet-5"],
    },
    Known {
        name: "openai",
        label: "OpenAI",
        kind: Kind::OpenAi,
        base: "https://api.openai.com/v1",
        env: "OPENAI_API_KEY",
        fallback: "gpt-5.5",
        prefer: &["gpt-5.5", "gpt-5.6-sol", "gpt-5"],
    },
    Known {
        name: "xai",
        label: "xAI",
        kind: Kind::OpenAi,
        base: "https://api.x.ai/v1",
        env: "XAI_API_KEY",
        fallback: "grok-4.7",
        prefer: &["grok-4.7", "grok-4.6", "grok-4"],
    },
    Known {
        name: "openrouter",
        label: "OpenRouter",
        kind: Kind::OpenAi,
        base: "https://openrouter.ai/api/v1",
        env: "OPENROUTER_API_KEY",
        fallback: "anthropic/claude-opus-5.5",
        prefer: &["anthropic/claude-opus-5.5", "anthropic/claude-opus-5"],
    },
];

pub fn known(name: &str) -> Option<&'static Known> {
    KNOWN.iter().find(|k| k.name == name)
}

/// Where requests go and with which key.
#[derive(Clone, Debug, PartialEq)]
pub struct Spec {
    pub name: String,
    pub kind: Kind,
    pub base: String,
    pub key: String,
    /// Send the key as a bearer token (ANTHROPIC_AUTH_TOKEN) instead of x-api-key.
    pub bearer: bool,
}

fn env(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.is_empty())
}

impl Spec {
    /// Credentials for a provider: the environment first, then auth.json.
    pub fn for_provider(name: &str, auth: &Auth) -> Option<Spec> {
        if name == "mock" {
            return Some(Spec {
                name: "mock".into(),
                kind: Kind::Mock,
                base: String::new(),
                key: String::new(),
                bearer: false,
            });
        }
        if let Some(a) = crate::oauth::account(name) {
            if !crate::oauth::signed_in(name) {
                return None;
            }
            let (kind, base) = if a.name == crate::oauth::CHATGPT.name {
                (Kind::ChatGpt, crate::oauth::chatgpt_base())
            } else {
                (Kind::OpenAi, crate::oauth::xai_base())
            };
            return Some(Spec {
                name: name.into(),
                kind,
                base,
                key: String::new(),
                bearer: true,
            });
        }
        if name == "custom" {
            let saved = auth.get("custom");
            let base = env("OPENAI_BASE_URL").or_else(|| saved.and_then(|c| c.base_url.clone()))?;
            let key = env("OPENAI_API_KEY")
                .filter(|_| env("OPENAI_BASE_URL").is_some())
                .or_else(|| saved.map(|c| c.key.clone()))
                .unwrap_or_default();
            return Some(Spec {
                name: "custom".into(),
                kind: Kind::OpenAi,
                base,
                key,
                bearer: false,
            });
        }
        let k = known(name)?;
        let mut key = env(k.env);
        let mut bearer = false;
        if k.kind == Kind::Anthropic && key.is_none() {
            key = env("ANTHROPIC_AUTH_TOKEN");
            bearer = key.is_some();
        }
        let key = key.or_else(|| auth.get(name).map(|c| c.key.clone()))?;
        let mut base = auth
            .get(name)
            .and_then(|c| c.base_url.clone())
            .unwrap_or_else(|| k.base.to_string());
        if k.kind == Kind::Anthropic
            && let Some(b) = env("ANTHROPIC_BASE_URL")
        {
            base = b;
        }
        Some(Spec {
            name: name.into(),
            kind: k.kind,
            base,
            key,
            bearer,
        })
    }
}

/// Picks the provider: the flag, then TAU_PROVIDER, then the saved choice if it
/// still has a key, then whichever key is in the environment, then auth.json.
pub fn resolve(flag: Option<&str>, settings: &Settings, auth: &Auth) -> Result<Option<Spec>> {
    if let Some(p) = flag.map(String::from).or_else(|| env("TAU_PROVIDER")) {
        if p == "auto" {
            return Ok(auto(settings, auth));
        }
        // v0.1 called every compatible endpoint "openai"
        let p = if p == "openai" && env("OPENAI_BASE_URL").is_some() {
            "custom".to_string()
        } else {
            p
        };
        if p != "custom"
            && p != "mock"
            && known(&p).is_none()
            && crate::oauth::account(&p).is_none()
        {
            bail!(
                "unknown provider {p}; use anthropic, openai, xai, openrouter, chatgpt, xai-oauth or custom"
            );
        }
        return match Spec::for_provider(&p, auth) {
            Some(s) => Ok(Some(s)),
            None => bail!("no key for {p}. Run `tau login` or set its API key variable"),
        };
    }
    Ok(auto(settings, auth))
}

fn auto(settings: &Settings, auth: &Auth) -> Option<Spec> {
    if let Some(p) = &settings.provider
        && let Some(s) = Spec::for_provider(p, auth)
    {
        return Some(s);
    }
    let mut order: Vec<&str> = vec!["anthropic"];
    if env("OPENAI_BASE_URL").is_some() {
        order.push("custom");
    }
    order.extend(["openai", "xai", "openrouter"]);
    for p in &order {
        let has_env = match *p {
            "anthropic" => env("ANTHROPIC_API_KEY")
                .or_else(|| env("ANTHROPIC_AUTH_TOKEN"))
                .is_some(),
            "custom" => true,
            p => known(p).and_then(|k| env(k.env)).is_some(),
        };
        if has_env {
            return Spec::for_provider(p, auth);
        }
    }
    auth.keys().find_map(|p| Spec::for_provider(p, auth))
}

/// The model to use: the flag or TAU_MODEL, the saved default, then the fallback.
pub fn pick_model(spec: &Spec, flag: Option<&str>, settings: &Settings) -> Result<String> {
    if let Some(m) = flag.map(String::from).or_else(|| env("TAU_MODEL")) {
        return Ok(m);
    }
    if let Some(m) = settings.models.get(&spec.name) {
        return Ok(m.clone());
    }
    match spec.kind {
        Kind::Mock => Ok("mock".into()),
        Kind::ChatGpt => Ok(responses::FALLBACK_MODELS[0].into()),
        _ if spec.name == crate::oauth::XAI.name => Ok("grok-4.7".into()),
        _ => match known(&spec.name) {
            Some(k) => Ok(k.fallback.into()),
            None => bail!(
                "no model chosen for {}. Run `tau login` or set TAU_MODEL",
                spec.name
            ),
        },
    }
}

pub fn build(spec: &Spec, model: &str) -> Result<Box<dyn Provider>> {
    Ok(match spec.kind {
        Kind::Mock => Box::new(mock::Mock::from_env()?),
        Kind::Anthropic => Box::new(anthropic::Anthropic::new(spec, model)),
        Kind::ChatGpt => Box::new(responses::ChatGpt::new(model)),
        Kind::OpenAi => Box::new(openai::OpenAi::new(
            &spec.name,
            spec.base.clone(),
            spec.key.clone(),
            model.into(),
        )),
    })
}

#[derive(Clone, Debug, PartialEq)]
pub struct ModelInfo {
    pub id: String,
    pub context: Option<u64>,
}

/// Model ids from a provider's list endpoint, newest first where the provider says.
pub fn parse_models(kind: Kind, v: &Value) -> Vec<ModelInfo> {
    let skip = [
        "embed",
        "tts",
        "whisper",
        "dall-e",
        "moderation",
        "audio",
        "realtime",
        "transcribe",
        "image",
        "davinci",
        "babbage",
        "sora",
    ];
    let mut out: Vec<ModelInfo> = v["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| {
            let id = m["id"].as_str()?.to_string();
            let context = ["max_input_tokens", "context_length", "context_window"]
                .iter()
                .find_map(|k| m[*k].as_u64());
            Some(ModelInfo { id, context })
        })
        .filter(|m| kind != Kind::OpenAi || !skip.iter().any(|s| m.id.contains(s)))
        .collect();
    if kind == Kind::OpenAi {
        // most compatible servers list in no useful order
        out.sort_by(|a, b| a.id.cmp(&b.id));
    }
    out
}

/// Lists models with a real request. This doubles as the key check at login.
pub fn list_models(spec: &Spec) -> Result<Vec<ModelInfo>> {
    use anyhow::Context as _;
    let url = match spec.kind {
        Kind::Anthropic => format!("{}/v1/models", spec.base.trim_end_matches('/')),
        _ => format!("{}/models", spec.base.trim_end_matches('/')),
    };
    list_models_inner(spec).with_context(|| format!("listing models at {url}"))
}

fn list_models_inner(spec: &Spec) -> Result<Vec<ModelInfo>> {
    let a = agent();
    let base = spec.base.trim_end_matches('/');
    let resp = match spec.kind {
        Kind::Mock => {
            return Ok(vec![ModelInfo {
                id: "mock".into(),
                context: None,
            }]);
        }
        Kind::ChatGpt => {
            let (access, account) = crate::oauth::bearer(&crate::oauth::CHATGPT)?;
            let mut req = a.get(format!("{base}/models"));
            for (k, v) in responses::headers(&access, account.as_deref(), "tau") {
                req = req.header(k, v);
            }
            req.call()?
        }
        Kind::OpenAi if spec.name == crate::oauth::XAI.name => {
            let (access, _) = crate::oauth::bearer(&crate::oauth::XAI)?;
            a.get(format!("{base}/models"))
                .header("authorization", format!("Bearer {access}"))
                .call()?
        }
        Kind::Anthropic => {
            let req = a
                .get(format!("{base}/v1/models?limit=1000"))
                .header("anthropic-version", "2023-06-01");
            if spec.bearer {
                req.header("authorization", format!("Bearer {}", spec.key))
                    .call()?
            } else {
                req.header("x-api-key", &spec.key).call()?
            }
        }
        Kind::OpenAi => {
            let mut req = a.get(format!("{base}/models"));
            if !spec.key.is_empty() {
                req = req.header("authorization", format!("Bearer {}", spec.key));
            }
            req.call()?
        }
    };
    let status = resp.status().as_u16();
    let body = resp.into_body().read_to_string().unwrap_or_default();
    if status >= 300 {
        let msg: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        let m = msg["error"]["message"]
            .as_str()
            .or(msg["error"].as_str())
            .map(String::from)
            .unwrap_or_else(|| body.chars().take(300).collect());
        bail!("HTTP {status}: {m}");
    }
    let v: Value = serde_json::from_str(&body)?;
    let mut list = parse_models(spec.kind, &v);
    if list.is_empty() && spec.kind == Kind::ChatGpt {
        list = responses::FALLBACK_MODELS
            .iter()
            .map(|m| ModelInfo {
                id: m.to_string(),
                context: None,
            })
            .collect();
    }
    crate::prices::learn(&list);
    let _ = crate::config::write_atomic(
        &crate::log::tau_home()
            .join("models")
            .join(format!("{}.json", spec.name)),
        &body,
        0o644,
    );
    Ok(list)
}

/// The last list fetched for a provider, for pickers that should not wait.
pub fn cached_models(spec: &Spec) -> Vec<ModelInfo> {
    std::fs::read_to_string(
        crate::log::tau_home()
            .join("models")
            .join(format!("{}.json", spec.name)),
    )
    .ok()
    .and_then(|s| serde_json::from_str(&s).ok())
    .map(|v| parse_models(spec.kind, &v))
    .inspect(|l| crate::prices::learn(l))
    .unwrap_or_default()
}

/// The provider's own preferences first; a custom server gets anything any
/// provider prefers, then its first model.
pub fn suggest(name: &str, list: &[ModelInfo]) -> Option<String> {
    let own: &[&str] = match name {
        "chatgpt" => &responses::FALLBACK_MODELS,
        "xai-oauth" => &["grok-4.7", "grok-4.6"],
        _ => &[],
    };
    if let Some(m) = own.iter().find(|p| list.iter().any(|m| m.id == **p)) {
        return Some(m.to_string());
    }
    let k = known(name);
    let others = KNOWN
        .iter()
        .filter(|o| o.name != name)
        .flat_map(|o| o.prefer.iter());
    k.into_iter()
        .flat_map(|k| k.prefer.iter())
        .chain(others)
        .find(|p| list.iter().any(|m| m.id == **p))
        .map(|s| s.to_string())
        .or_else(|| list.first().map(|m| m.id.clone()))
        .or_else(|| k.map(|k| k.fallback.to_string()))
}

pub const TOOL_DESC: &str = "Run a bash command. stdout and stderr come back together, bounded to the head and tail. A command still running after `timeout` seconds (default 120) keeps going as a background job.";

pub fn tool_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "command": {"type": "string", "description": "The bash command."},
            "timeout": {"type": "integer", "description": "Seconds before it becomes a job. Default 120."}
        },
        "required": ["command"]
    })
}

/// Tool arguments to a call. Bad JSON becomes a call that reports the problem.
pub fn parse_call(id: String, args: &str) -> Call {
    match serde_json::from_str::<Value>(if args.trim().is_empty() { "{}" } else { args }) {
        Ok(v) => {
            let command = v["command"].as_str().unwrap_or("").to_string();
            Call {
                id,
                command: if command.is_empty() {
                    "echo 'tau: the bash tool needs a command argument' >&2; exit 2".into()
                } else {
                    command
                },
                timeout: v["timeout"].as_u64(),
            }
        }
        Err(e) => Call {
            id,
            command: format!(
                "echo {} >&2; exit 2",
                shell_quote(&format!("tau: tool arguments were not valid JSON: {e}"))
            ),
            timeout: None,
        },
    }
}

pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

pub fn cost(model: &str, u: &Usage) -> f64 {
    match crate::prices::lookup(model) {
        Some(p) => {
            (u.input as f64 * p.input
                + u.output as f64 * p.output
                + u.cache_read as f64 * p.cache_read
                + u.cache_write as f64 * p.cache_write)
                / 1e6
        }
        None => 0.0,
    }
}

/// True when an API error says the prompt no longer fits.
pub fn is_overflow(e: &anyhow::Error) -> bool {
    let s = format!("{e:#}").to_ascii_lowercase();
    [
        "prompt is too long",
        "context_length_exceeded",
        "maximum context length",
        "context window",
        "too many tokens",
    ]
    .iter()
    .any(|p| s.contains(p))
}

/// Reads an SSE body line by line, calling `f` with each `data:` payload.
/// Stops early if the user interrupts.
pub fn sse(reader: impl std::io::Read, mut f: impl FnMut(&str) -> Result<bool>) -> Result<()> {
    use std::io::BufRead;
    let r = std::io::BufReader::new(reader);
    for line in r.lines() {
        if crate::exec::cancelled() {
            bail!("interrupted");
        }
        let line = line?;
        if let Some(d) = line.strip_prefix("data:") {
            let d = d.trim_start();
            if d == "[DONE]" {
                break;
            }
            if !f(d)? {
                break;
            }
        }
    }
    Ok(())
}

pub fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_connect(Some(std::time::Duration::from_secs(20)))
        .build()
        .into()
}

/// POSTs with retries on 429, 5xx and dropped connections. Returns the open response.
pub fn post(
    url: &str,
    headers: &[(&str, String)],
    body: &Value,
) -> Result<ureq::http::Response<ureq::Body>> {
    let a = agent();
    let mut wait = 2u64;
    for attempt in 0..5 {
        let mut req = a.post(url);
        for (k, v) in headers {
            req = req.header(*k, v);
        }
        match req.send_json(body) {
            Ok(resp) => {
                let st = resp.status().as_u16();
                if st < 300 {
                    return Ok(resp);
                }
                let retry = st == 429 || st >= 500;
                let text = resp.into_body().read_to_string().unwrap_or_default();
                if !retry || attempt == 4 {
                    bail!("HTTP {st}: {}", text.chars().take(2000).collect::<String>());
                }
                eprintln!("tau: HTTP {st}, retrying in {wait}s");
            }
            Err(e) if attempt < 4 => eprintln!("tau: {e}, retrying in {wait}s"),
            Err(e) => return Err(e.into()),
        }
        for _ in 0..wait * 10 {
            if crate::exec::cancelled() {
                bail!("interrupted");
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        wait *= 2;
    }
    unreachable!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_anthropic_model_list() {
        let v = json!({"data": [
            {"id": "claude-opus-5-5", "type": "model", "max_input_tokens": 1000000},
            {"id": "claude-sonnet-5", "type": "model"}
        ], "has_more": false});
        let l = parse_models(Kind::Anthropic, &v);
        assert_eq!(l[0].id, "claude-opus-5-5");
        assert_eq!(l[0].context, Some(1_000_000));
        assert_eq!(suggest("anthropic", &l).unwrap(), "claude-opus-5-5");
    }

    #[test]
    fn parses_openai_compatible_list_and_drops_non_chat_models() {
        let v = json!({"object": "list", "data": [
            {"id": "text-embedding-3-large"},
            {"id": "grok-4.7", "context_window": 500000},
            {"id": "gpt-4o-mini-tts"},
            {"id": "grok-4.6"}
        ]});
        let l = parse_models(Kind::OpenAi, &v);
        let ids: Vec<&str> = l.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["grok-4.6", "grok-4.7"]);
        assert_eq!(l[1].context, Some(500_000));
        assert_eq!(suggest("xai", &l).unwrap(), "grok-4.7");
        // a custom server gets whatever any provider prefers
        assert_eq!(suggest("custom", &l).unwrap(), "grok-4.7");
        let odd = vec![ModelInfo {
            id: "llama-3".into(),
            context: None,
        }];
        assert_eq!(suggest("custom", &odd).unwrap(), "llama-3");
        // and the fallback when the list is empty
        assert_eq!(suggest("openai", &[]).unwrap(), "gpt-5.5");
    }

    #[test]
    fn overflow_errors_are_recognized() {
        let e = anyhow::anyhow!("HTTP 400: prompt is too long: 1200000 tokens > 1000000 maximum");
        assert!(is_overflow(&e));
        assert!(!is_overflow(&anyhow::anyhow!(
            "HTTP 401: invalid x-api-key"
        )));
    }
}
