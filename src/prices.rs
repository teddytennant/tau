//! Prices and context windows. A snapshot of OpenRouter's public model list
//! ships in the binary, and `refresh` replaces it with the live list in
//! ~/.tau/prices.tsv so new models get prices without a release.

use crate::log::tau_home;
use std::collections::HashMap;
use std::sync::OnceLock;

const SNAPSHOT: &str = include_str!("prices.tsv");

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Price {
    /// Dollars per million tokens.
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub context: Option<u64>,
}

/// `anthropic/claude-opus-5.5` and `claude-opus-5-5` are the same model.
pub fn normalize(id: &str) -> String {
    let id = id.rsplit('/').next().unwrap_or(id);
    let id = id.split(':').next().unwrap_or(id);
    id.to_ascii_lowercase().replace('.', "-")
}

pub fn parse(tsv: &str) -> HashMap<String, Price> {
    let mut m = HashMap::new();
    for line in tsv.lines() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() < 3 {
            continue;
        }
        let (Ok(input), Ok(output)) = (f[1].parse::<f64>(), f[2].parse::<f64>()) else {
            continue;
        };
        let cache_read = f.get(3).and_then(|s| s.parse().ok()).unwrap_or(input);
        let claude = f[0].contains("claude");
        m.insert(
            normalize(f[0]),
            Price {
                input,
                output,
                cache_read,
                // only Anthropic bills cache writes, at 1.25x input for the 5 minute TTL
                cache_write: if claude { input * 1.25 } else { input },
                context: f.get(4).and_then(|s| s.parse().ok()),
            },
        );
    }
    m
}

fn table() -> &'static HashMap<String, Price> {
    static T: OnceLock<HashMap<String, Price>> = OnceLock::new();
    T.get_or_init(|| {
        let mut t = parse(SNAPSHOT);
        if let Ok(live) = std::fs::read_to_string(tau_home().join("prices.tsv")) {
            t.extend(parse(&live));
        }
        t
    })
}

pub fn lookup(model: &str) -> Option<Price> {
    table().get(&normalize(model)).copied()
}

/// `1.6`, `0.075`, `15`: dollars per million without float noise.
pub fn fmt(x: f64) -> String {
    let s = format!("{x:.4}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// `$4/$20 per M`
pub fn label(model: &str) -> Option<String> {
    lookup(model).map(|p| format!("${}/${} per M", fmt(p.input), fmt(p.output)))
}

fn learned() -> &'static std::sync::Mutex<HashMap<String, u64>> {
    static L: OnceLock<std::sync::Mutex<HashMap<String, u64>>> = OnceLock::new();
    L.get_or_init(Default::default)
}

/// Context windows a provider's model list reported.
pub fn learn(models: &[crate::provider::ModelInfo]) {
    let mut l = learned().lock().unwrap_or_else(|e| e.into_inner());
    for m in models {
        if let Some(c) = m.context {
            l.insert(m.id.clone(), c);
        }
    }
}

pub fn context_window(model: &str) -> u64 {
    if let Some(w) = std::env::var("TAU_CONTEXT_WINDOW")
        .ok()
        .and_then(|s| s.parse().ok())
    {
        return w;
    }
    if let Some(w) = learned()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(model)
    {
        return *w;
    }
    if let Some(w) = lookup(model).and_then(|p| p.context) {
        return w;
    }
    if model.contains("claude") {
        1_000_000
    } else {
        128_000
    }
}

/// Fetches OpenRouter's public list (no key needed) into ~/.tau/prices.tsv.
pub fn refresh() -> anyhow::Result<usize> {
    let a = crate::provider::agent();
    let body = a
        .get("https://openrouter.ai/api/v1/models")
        .call()?
        .into_body()
        .read_to_string()?;
    let v: serde_json::Value = serde_json::from_str(&body)?;
    let mut out = String::from("# fetched from https://openrouter.ai/api/v1/models\n");
    let mut n = 0;
    for m in v["data"].as_array().into_iter().flatten() {
        let id = m["id"].as_str().unwrap_or("");
        let p = &m["pricing"];
        let num = |k: &str| {
            p[k].as_str()
                .and_then(|s| s.parse::<f64>().ok())
                .map(|x| x * 1e6)
        };
        let (Some(i), Some(o)) = (num("prompt"), num("completion")) else {
            continue;
        };
        if id.contains(':') || i <= 0.0 {
            continue;
        }
        let cr = num("input_cache_read")
            .map(|x| x.to_string())
            .unwrap_or_default();
        let ctx = m["context_length"]
            .as_u64()
            .map(|x| x.to_string())
            .unwrap_or_default();
        out.push_str(&format!("{id}\t{}\t{}\t{cr}\t{ctx}\n", fmt(i), fmt(o)));
        n += 1;
    }
    if n > 0 {
        crate::config::write_atomic(&tau_home().join("prices.tsv"), &out, 0o644)?;
    }
    Ok(n)
}

/// Refresh in the background when the local copy is missing or a week old.
pub fn refresh_if_stale() {
    let p = tau_home().join("prices.tsv");
    let stale = std::fs::metadata(&p)
        .and_then(|m| m.modified())
        .map(|t| t.elapsed().map(|e| e.as_secs() > 7 * 86400).unwrap_or(true))
        .unwrap_or(true);
    if stale && std::env::var("TAU_OFFLINE").is_err() {
        std::thread::spawn(|| {
            let _ = refresh();
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_prices_common_models() {
        let t = parse(SNAPSHOT);
        let opus = t[&normalize("claude-opus-5-5")];
        assert_eq!((opus.input, opus.output, opus.cache_read), (4.0, 20.0, 0.2));
        assert_eq!(opus.cache_write, 5.0);
        assert!(t.contains_key(&normalize("gpt-5.5")));
        assert!(t.contains_key(&normalize("grok-4.7")));
        assert_eq!(
            normalize("anthropic/claude-sonnet-4.6"),
            "claude-sonnet-4-6"
        );
        assert_eq!(normalize("openai/gpt-5.5:batch"), "gpt-5-5");
        assert_eq!(fmt(1.5999999999999999), "1.6");
        assert_eq!(fmt(15.0), "15");
        assert_eq!(fmt(0.075), "0.075");
    }
}
