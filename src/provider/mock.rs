//! Replays a script instead of calling a model, for tests and demos.
//!
//! TAU_MOCK is the script, or a path to a file holding it. Turns are separated
//! by a line `---`. In a turn, a line starting with `$ ` is a bash call, a
//! line starting with `> ` continues the call above it, and everything else is
//! text.

use super::{Provider, Reply};
use crate::log::{Call, Msg, Usage};
use anyhow::Result;

pub struct Mock {
    turns: Vec<String>,
    next: usize,
    seen: u64,
}

impl Mock {
    pub fn from_env() -> Result<Mock> {
        let s = std::env::var("TAU_MOCK").unwrap_or_default();
        let s = if !s.contains('\n') && std::path::Path::new(&s).is_file() {
            std::fs::read_to_string(&s)?
        } else {
            s
        };
        Ok(Mock::new(&s))
    }

    pub fn new(script: &str) -> Mock {
        let mut turns = vec![];
        let mut cur = vec![];
        for line in script.lines() {
            if line.trim() == "---" {
                turns.push(cur.join("\n"));
                cur.clear();
            } else {
                cur.push(line);
            }
        }
        turns.push(cur.join("\n"));
        Mock {
            turns,
            next: 0,
            seen: 0,
        }
    }
}

impl Provider for Mock {
    fn name(&self) -> &str {
        "mock"
    }
    fn model(&self) -> &str {
        "mock"
    }

    fn complete(
        &mut self,
        system: &str,
        msgs: &[Msg],
        on_text: &mut dyn FnMut(&str),
    ) -> Result<Reply> {
        // compaction requests get a canned summary and do not use up a turn
        let compacting = matches!(msgs.last(), Some(Msg::User { text, .. }) if text.starts_with(crate::agent::COMPACT_PROMPT));
        let turn = if compacting {
            format!("mock summary of {} messages", msgs.len() - 1)
        } else {
            self.next += 1;
            self.turns
                .get(self.next - 1)
                .cloned()
                .unwrap_or_else(|| "(mock script exhausted)".into())
        };
        let mut text = vec![];
        let mut calls = vec![];
        for line in turn.lines() {
            if let Some(c) = line.strip_prefix("$ ") {
                calls.push(Call {
                    id: format!("mock_{}_{}", self.next, calls.len()),
                    command: c.to_string(),
                    timeout: None,
                });
            } else if let (Some(c), Some(last)) = (line.strip_prefix("> "), calls.last_mut()) {
                last.command.push('\n');
                last.command.push_str(c);
            } else {
                text.push(line);
            }
        }
        let text = text.join("\n").trim().to_string();
        on_text(&text);
        // pretend everything seen last turn was cached, like a real prefix cache
        let total = (system.len() + serde_json::to_string(msgs)?.len()) as u64 / 4;
        let mut usage = Usage {
            input: total.saturating_sub(self.seen),
            output: (text.len() as u64 + 20) / 4,
            cache_read: self.seen.min(total),
            ..Default::default()
        };
        self.seen = total;
        usage.cost = super::cost("mock", &usage);
        Ok(Reply {
            text,
            calls,
            raw: None,
            usage,
        })
    }
}
