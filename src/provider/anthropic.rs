use super::{Provider, Reply, TOOL_DESC, parse_call, post, sse, tool_schema};
use crate::log::{Call, Msg, Usage};
use anyhow::{Result, bail};
use serde_json::{Value, json};

pub struct Anthropic {
    key: String,
    model: String,
    base: String,
    bearer: bool,
    effort: Option<String>,
}

impl Anthropic {
    pub fn new(spec: &super::Spec, model: &str) -> Anthropic {
        Anthropic {
            key: spec.key.clone(),
            model: model.to_string(),
            base: spec.base.clone(),
            bearer: spec.bearer,
            effort: None,
        }
    }
}

fn tool_use(c: &Call) -> Value {
    let mut input = json!({"command": c.command});
    if let Some(t) = c.timeout {
        input["timeout"] = json!(t);
    }
    json!({"type": "tool_use", "id": c.id, "name": "bash", "input": input})
}

/// Messages in Anthropic's shape. A pure function of `msgs`, so an unchanged
/// history serializes to the same bytes every turn.
pub fn messages(msgs: &[Msg]) -> Vec<Value> {
    let mut out: Vec<Value> = vec![];
    for m in msgs {
        match m {
            Msg::User { text, images } => {
                let mut c: Vec<Value> = images
                    .iter()
                    .map(|i| json!({"type": "image", "source": {"type": "base64", "media_type": i.media_type, "data": i.data}}))
                    .collect();
                c.push(
                    json!({"type": "text", "text": if text.is_empty() { "(image)" } else { text }}),
                );
                out.push(json!({"role": "user", "content": c}));
            }
            Msg::Assistant { text, calls, raw } => {
                let content = match raw {
                    Some((p, r)) if p == "anthropic" => r.clone(),
                    _ => {
                        let mut c = vec![];
                        if !text.is_empty() {
                            c.push(json!({"type": "text", "text": text}));
                        }
                        c.extend(calls.iter().map(tool_use));
                        Value::Array(c)
                    }
                };
                if content.as_array().is_some_and(|a| a.is_empty()) {
                    out.push(json!({"role": "assistant", "content": [{"type": "text", "text": "(no reply)"}]}));
                } else {
                    out.push(json!({"role": "assistant", "content": content}));
                }
            }
            Msg::Tool { call_id, output } => {
                let block = json!({"type": "tool_result", "tool_use_id": call_id, "content": if output.is_empty() { "(no output)" } else { output }});
                // all results for one assistant turn go in a single user message
                if let Some(last) = out.last_mut()
                    && last["role"] == "user"
                    && last["content"][0]["type"] == "tool_result"
                {
                    last["content"].as_array_mut().unwrap().push(block);
                    continue;
                }
                out.push(json!({"role": "user", "content": [block]}));
            }
        }
    }
    out
}

pub fn body(model: &str, system: &str, msgs: &[Msg], effort: Option<&str>) -> Value {
    let mut b = json!({
        "model": model,
        "max_tokens": 64000,
        "stream": true,
        "system": [{"type": "text", "text": system}],
        "tools": [{"name": "bash", "description": TOOL_DESC, "input_schema": tool_schema()}],
        "messages": messages(msgs),
        // automatic caching: the breakpoint follows the end of the history
        "cache_control": {"type": "ephemeral"},
    });
    if let Some(e) = effort {
        b["output_config"] = json!({"effort": e});
    }
    b
}

impl Provider for Anthropic {
    fn name(&self) -> &str {
        "anthropic"
    }
    fn model(&self) -> &str {
        &self.model
    }

    fn set_thinking(&mut self, level: &str) {
        // "auto" leaves effort to the model's own default
        self.effort = (level != "auto").then(|| level.to_string());
    }

    fn complete(
        &mut self,
        system: &str,
        msgs: &[Msg],
        on_text: &mut dyn FnMut(&str),
    ) -> Result<Reply> {
        let auth = if self.bearer {
            ("authorization", format!("Bearer {}", self.key))
        } else {
            ("x-api-key", self.key.clone())
        };
        let effort = std::env::var("TAU_EFFORT")
            .ok()
            .filter(|e| !e.is_empty())
            .or_else(|| self.effort.clone());
        let headers = [
            auth,
            ("anthropic-version", "2023-06-01".to_string()),
            ("content-type", "application/json".to_string()),
        ];
        let resp = post(
            &format!("{}/v1/messages", self.base.trim_end_matches('/')),
            &headers,
            &body(&self.model, system, msgs, effort.as_deref()),
        )?;
        let mut blocks: Vec<Value> = vec![];
        let mut partial: Vec<String> = vec![];
        let mut usage = Usage::default();
        let mut stop = String::new();
        sse(resp.into_body().into_reader(), |d| {
            let ev: Value = serde_json::from_str(d)?;
            match ev["type"].as_str().unwrap_or("") {
                "message_start" => {
                    let u = &ev["message"]["usage"];
                    usage.input = u["input_tokens"].as_u64().unwrap_or(0);
                    usage.cache_read = u["cache_read_input_tokens"].as_u64().unwrap_or(0);
                    usage.cache_write = u["cache_creation_input_tokens"].as_u64().unwrap_or(0);
                    usage.output = u["output_tokens"].as_u64().unwrap_or(0);
                }
                "content_block_start" => {
                    let i = ev["index"].as_u64().unwrap_or(0) as usize;
                    while blocks.len() <= i {
                        blocks.push(Value::Null);
                        partial.push(String::new());
                    }
                    blocks[i] = ev["content_block"].clone();
                }
                "content_block_delta" => {
                    let i = ev["index"].as_u64().unwrap_or(0) as usize;
                    let d = &ev["delta"];
                    let Some(b) = blocks.get_mut(i) else {
                        return Ok(true);
                    };
                    match d["type"].as_str().unwrap_or("") {
                        "text_delta" => {
                            let t = d["text"].as_str().unwrap_or("");
                            on_text(t);
                            let s = b["text"].as_str().unwrap_or("").to_string() + t;
                            b["text"] = json!(s);
                        }
                        "thinking_delta" => {
                            let s = b["thinking"].as_str().unwrap_or("").to_string()
                                + d["thinking"].as_str().unwrap_or("");
                            b["thinking"] = json!(s);
                        }
                        "signature_delta" => b["signature"] = d["signature"].clone(),
                        "input_json_delta" => {
                            partial[i].push_str(d["partial_json"].as_str().unwrap_or(""))
                        }
                        _ => {}
                    }
                }
                "content_block_stop" => {
                    let i = ev["index"].as_u64().unwrap_or(0) as usize;
                    if let Some(b) = blocks.get_mut(i)
                        && b["type"] == "tool_use"
                    {
                        let p = &partial[i];
                        b["input"] = serde_json::from_str(if p.is_empty() { "{}" } else { p })
                            .unwrap_or_else(|_| json!({"__invalid_json": p}));
                    }
                }
                "message_delta" => {
                    if let Some(o) = ev["usage"]["output_tokens"].as_u64() {
                        usage.output = o;
                    }
                    if let Some(s) = ev["delta"]["stop_reason"].as_str() {
                        stop = s.to_string();
                    }
                }
                "error" => bail!("{}", ev["error"]),
                _ => {}
            }
            Ok(true)
        })?;
        if stop == "refusal" {
            bail!("the model declined this request (stop_reason refusal)");
        }
        if stop.is_empty() {
            bail!("the stream ended early");
        }
        blocks.retain(|b| !(b.is_null() || b["type"] == "text" && b["text"] == ""));
        let mut text = String::new();
        let mut calls = vec![];
        for b in &blocks {
            match b["type"].as_str() {
                Some("text") => text.push_str(b["text"].as_str().unwrap_or("")),
                Some("tool_use") => {
                    let id = b["id"].as_str().unwrap_or("").to_string();
                    let args = if b["input"].get("__invalid_json").is_some() {
                        b["input"]["__invalid_json"]
                            .as_str()
                            .unwrap_or("")
                            .to_string()
                    } else {
                        b["input"].to_string()
                    };
                    calls.push(parse_call(id, &args));
                }
                _ => {}
            }
        }
        usage.cost = super::cost(&self.model, &usage);
        Ok(Reply {
            text,
            calls,
            raw: Some(Value::Array(blocks)),
            usage,
        })
    }
}
