//! Any OpenAI-compatible chat completions endpoint: OpenAI, xAI, OpenRouter,
//! llama.cpp, vLLM, Ollama.

use super::{Provider, Reply, TOOL_DESC, parse_call, post, sse, tool_schema};
use crate::log::{Msg, Usage};
use anyhow::Result;
use serde_json::{Value, json};

pub struct OpenAi {
    name: String,
    base: String,
    key: String,
    model: String,
    effort: Option<String>,
}

impl OpenAi {
    pub fn new(name: &str, base: String, key: String, model: String) -> OpenAi {
        OpenAi {
            name: name.to_string(),
            base,
            key,
            model,
            effort: None,
        }
    }
}

pub fn messages(system: &str, msgs: &[Msg]) -> Vec<Value> {
    let mut out = vec![json!({"role": "system", "content": system})];
    for m in msgs {
        out.push(match m {
            Msg::User { text, images } if images.is_empty() => {
                json!({"role": "user", "content": text})
            }
            Msg::User { text, images } => {
                let mut c: Vec<Value> = images
                    .iter()
                    .map(|i| json!({"type": "image_url", "image_url": {"url": format!("data:{};base64,{}", i.media_type, i.data)}}))
                    .collect();
                c.push(json!({"type": "text", "text": text}));
                json!({"role": "user", "content": c})
            }
            Msg::Assistant { text, calls, .. } => {
                let mut a = json!({"role": "assistant", "content": text});
                if !calls.is_empty() {
                    a["tool_calls"] = calls
                        .iter()
                        .map(|c| {
                            let mut args = json!({"command": c.command});
                            if let Some(t) = c.timeout {
                                args["timeout"] = json!(t);
                            }
                            json!({"id": c.id, "type": "function", "function": {"name": "bash", "arguments": args.to_string()}})
                        })
                        .collect();
                }
                a
            }
            Msg::Tool { call_id, output } => {
                json!({"role": "tool", "tool_call_id": call_id, "content": output})
            }
        });
    }
    out
}

impl Provider for OpenAi {
    fn name(&self) -> &str {
        &self.name
    }
    fn model(&self) -> &str {
        &self.model
    }

    fn set_thinking(&mut self, level: &str) {
        // chat completions knows low, medium and high
        self.effort = match level {
            "auto" => None,
            "xhigh" | "max" => Some("high".into()),
            l => Some(l.to_string()),
        };
    }

    fn complete(
        &mut self,
        system: &str,
        msgs: &[Msg],
        on_text: &mut dyn FnMut(&str),
    ) -> Result<Reply> {
        let mut body = json!({
            "model": self.model,
            "stream": true,
            "stream_options": {"include_usage": true},
            "messages": messages(system, msgs),
            "tools": [{"type": "function", "function": {"name": "bash", "description": TOOL_DESC, "parameters": tool_schema()}}],
        });
        if let Some(e) = &self.effort {
            body["reasoning_effort"] = json!(e);
        }
        let url = format!("{}/chat/completions", self.base.trim_end_matches('/'));
        // an xAI account sign-in: a bearer that refreshes itself
        let account = crate::oauth::account(&self.name);
        let mut key = match account {
            Some(a) => crate::oauth::bearer(a)?.0,
            None => self.key.clone(),
        };
        let resp = loop {
            let mut headers = vec![("content-type", "application/json".to_string())];
            if !key.is_empty() {
                headers.push(("authorization", format!("Bearer {key}")));
            }
            match (post(&url, &headers, &body), account) {
                (Err(e), Some(a)) if format!("{e}").starts_with("HTTP 401") => {
                    let rejected = key.clone();
                    key = crate::oauth::refresh_after_401(a, &rejected)?.0;
                    if key == rejected {
                        return Err(e);
                    }
                }
                (r, _) => break r?,
            }
        };
        let mut text = String::new();
        // (id, name, arguments) by index
        let mut calls: Vec<(String, String)> = vec![];
        let mut usage = Usage::default();
        sse(resp.into_body().into_reader(), |d| {
            let ev: Value = serde_json::from_str(d)?;
            if let Some(err) = ev.get("error") {
                anyhow::bail!("{err}");
            }
            let delta = &ev["choices"][0]["delta"];
            if let Some(t) = delta["content"].as_str()
                && !t.is_empty()
            {
                on_text(t);
                text.push_str(t);
            }
            if let Some(tcs) = delta["tool_calls"].as_array() {
                for tc in tcs {
                    let i = tc["index"].as_u64().unwrap_or(calls.len() as u64) as usize;
                    while calls.len() <= i {
                        calls.push((String::new(), String::new()));
                    }
                    if let Some(id) = tc["id"].as_str() {
                        calls[i].0 = id.to_string();
                    }
                    if let Some(a) = tc["function"]["arguments"].as_str() {
                        calls[i].1.push_str(a);
                    }
                }
            }
            let u = &ev["usage"];
            if u.is_object() {
                let prompt = u["prompt_tokens"].as_u64().unwrap_or(0);
                let cached = u["prompt_tokens_details"]["cached_tokens"]
                    .as_u64()
                    .unwrap_or(0);
                usage.input = prompt.saturating_sub(cached);
                usage.cache_read = cached;
                usage.output = u["completion_tokens"].as_u64().unwrap_or(0);
            }
            Ok(true)
        })?;
        // a subscription has no per-token cost
        if crate::oauth::account(&self.name).is_none() {
            usage.cost = super::cost(&self.model, &usage);
        }
        let calls = calls
            .into_iter()
            .enumerate()
            .map(|(i, (id, args))| {
                let id = if id.is_empty() {
                    format!("call_{i}")
                } else {
                    id
                };
                parse_call(id, &args)
            })
            .collect();
        Ok(Reply {
            text,
            calls,
            raw: None,
            usage,
        })
    }
}
