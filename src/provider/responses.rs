//! A ChatGPT subscription, reached the way Codex reaches it: the Responses
//! API at chatgpt.com/backend-api/codex with the account's OAuth token.

use super::{Provider, Reply, TOOL_DESC, parse_call, post, sse, tool_schema};
use crate::log::{Msg, Usage};
use crate::oauth::{self, CHATGPT};
use anyhow::{Result, bail};
use serde_json::{Value, json};

pub const FALLBACK_MODELS: [&str; 4] = ["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna", "gpt-5.5"];

pub struct ChatGpt {
    model: String,
    effort: Option<String>,
    session: String,
}

impl ChatGpt {
    pub fn new(model: &str) -> ChatGpt {
        let session = oauth::random_bytes(16)
            .map(|b| b.iter().map(|x| format!("{x:02x}")).collect())
            .unwrap_or_default();
        ChatGpt {
            model: model.into(),
            effort: None,
            session,
        }
    }
}

pub fn headers(access: &str, account: Option<&str>, session: &str) -> Vec<(&'static str, String)> {
    let mut h = vec![
        ("authorization", format!("Bearer {access}")),
        ("originator", "codex_cli_rs".to_string()),
        (
            "user-agent",
            format!("codex_cli_rs/{} (tau)", env!("CARGO_PKG_VERSION")),
        ),
        ("session-id", session.to_string()),
        ("openai-beta", "responses=experimental".to_string()),
        ("accept", "text/event-stream".to_string()),
        ("content-type", "application/json".to_string()),
    ];
    if let Some(a) = account {
        h.push(("chatgpt-account-id", a.to_string()));
    }
    h
}

/// The conversation as Responses `input` items. Reasoning items the model
/// returned are replayed before the turn they belong to, because with
/// `store: false` the server keeps nothing between calls.
pub fn input(msgs: &[Msg]) -> Vec<Value> {
    let mut out = vec![];
    for m in msgs {
        match m {
            Msg::User { text, images } => {
                let mut c = vec![json!({"type": "input_text", "text": text})];
                for i in images {
                    c.push(json!({"type": "input_image", "image_url": format!("data:{};base64,{}", i.media_type, i.data)}));
                }
                out.push(json!({"type": "message", "role": "user", "content": c}));
            }
            Msg::Assistant { text, calls, raw } => {
                if let Some((p, Value::Array(items))) = raw
                    && p == CHATGPT.name
                {
                    out.extend(items.iter().cloned());
                }
                if !text.is_empty() {
                    out.push(json!({"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": text}]}));
                }
                for c in calls {
                    let mut args = json!({"command": c.command});
                    if let Some(t) = c.timeout {
                        args["timeout"] = json!(t);
                    }
                    out.push(json!({"type": "function_call", "name": "bash", "arguments": args.to_string(), "call_id": c.id}));
                }
            }
            Msg::Tool { call_id, output } => out.push(
                json!({"type": "function_call_output", "call_id": call_id, "output": output}),
            ),
        }
    }
    out
}

pub fn body(model: &str, system: &str, msgs: &[Msg], effort: Option<&str>) -> Value {
    let mut b = json!({
        "model": model,
        "instructions": system,
        "input": input(msgs),
        "tools": [{"type": "function", "name": "bash", "description": TOOL_DESC, "parameters": tool_schema()}],
        "tool_choice": "auto",
        "parallel_tool_calls": true,
        "store": false,
        "stream": true,
        "include": ["reasoning.encrypted_content"],
    });
    if let Some(e) = effort {
        b["reasoning"] = json!({"effort": e, "summary": "auto"});
    }
    b
}

impl Provider for ChatGpt {
    fn name(&self) -> &str {
        CHATGPT.name
    }
    fn model(&self) -> &str {
        &self.model
    }

    fn set_thinking(&mut self, level: &str) {
        self.effort = match level {
            "auto" => None,
            "max" => Some("xhigh".into()),
            l => Some(l.to_string()),
        };
    }

    fn complete(
        &mut self,
        system: &str,
        msgs: &[Msg],
        on_text: &mut dyn FnMut(&str),
    ) -> Result<Reply> {
        let url = format!("{}/responses", oauth::chatgpt_base());
        let b = body(&self.model, system, msgs, self.effort.as_deref());
        let (mut access, mut account) = oauth::bearer(&CHATGPT)?;
        let resp = loop {
            match post(
                &url,
                &headers(&access, account.as_deref(), &self.session),
                &b,
            ) {
                Err(e) if format!("{e}").starts_with("HTTP 401") => {
                    let rejected = access.clone();
                    (access, account) = oauth::refresh_after_401(&CHATGPT, &rejected)?;
                    if access == rejected {
                        return Err(e);
                    }
                }
                r => break r?,
            }
        };
        let mut text = String::new();
        let mut calls = vec![];
        let mut reasoning = vec![];
        let mut usage = Usage::default();
        let mut done = false;
        sse(resp.into_body().into_reader(), |d| {
            let ev: Value = serde_json::from_str(d)?;
            match ev["type"].as_str().unwrap_or("") {
                "response.output_text.delta" => {
                    let t = ev["delta"].as_str().unwrap_or("");
                    on_text(t);
                    text.push_str(t);
                }
                "response.output_item.done" => {
                    let item = &ev["item"];
                    match item["type"].as_str() {
                        Some("function_call") => calls.push(parse_call(
                            item["call_id"].as_str().unwrap_or("").to_string(),
                            item["arguments"].as_str().unwrap_or(""),
                        )),
                        Some("reasoning") if item.get("encrypted_content").is_some() => {
                            reasoning.push(item.clone())
                        }
                        _ => {}
                    }
                }
                "response.completed" | "response.incomplete" => {
                    let u = &ev["response"]["usage"];
                    let cached = u["input_tokens_details"]["cached_tokens"]
                        .as_u64()
                        .unwrap_or(0);
                    usage.input = u["input_tokens"]
                        .as_u64()
                        .unwrap_or(0)
                        .saturating_sub(cached);
                    usage.cache_read = cached;
                    usage.output = u["output_tokens"].as_u64().unwrap_or(0);
                    done = true;
                }
                "response.failed" | "error" => bail!(
                    "ChatGPT: {}",
                    ev["response"]["error"]["message"]
                        .as_str()
                        .or(ev["message"].as_str())
                        .unwrap_or(d)
                ),
                _ => {}
            }
            Ok(true)
        })?;
        if !done {
            bail!("the ChatGPT stream ended early");
        }
        // a subscription, so no per-token cost
        Ok(Reply {
            text,
            calls,
            raw: (!reasoning.is_empty()).then_some(Value::Array(reasoning)),
            usage,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::Call;

    #[test]
    fn replays_reasoning_then_the_call_then_its_output() {
        let msgs = vec![
            Msg::user("go"),
            Msg::Assistant {
                text: String::new(),
                calls: vec![Call {
                    id: "call_1".into(),
                    command: "ls".into(),
                    timeout: None,
                }],
                raw: Some((
                    "chatgpt".into(),
                    json!([{"type": "reasoning", "id": "rs_1", "encrypted_content": "gAAA", "summary": []}]),
                )),
            },
            Msg::Tool {
                call_id: "call_1".into(),
                output: "a.rs".into(),
            },
        ];
        let b = body("gpt-5.6-sol", "sys", &msgs, Some("high"));
        let i = b["input"].as_array().unwrap();
        assert_eq!(i[0]["role"], "user");
        assert_eq!(i[1]["type"], "reasoning");
        assert_eq!(i[2]["type"], "function_call");
        assert_eq!(i[2]["arguments"], "{\"command\":\"ls\"}");
        assert_eq!(i[3]["type"], "function_call_output");
        assert_eq!(i[3]["call_id"], "call_1");
        assert_eq!(b["instructions"], "sys");
        assert_eq!(b["store"], false);
        assert_eq!(b["reasoning"]["effort"], "high");
        assert_eq!(b["tools"][0]["name"], "bash");
    }
}
