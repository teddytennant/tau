//! The loop: render, ask the model, run bash, append, repeat.

use crate::depth::Depth;
use crate::exec::{self, DEFAULT_TIMEOUT, Exec};
use crate::log::{Body, Event, Msg, Session, Usage, parse_node, render};
use crate::provider::{Provider, is_overflow};
use anyhow::{Result, bail};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

pub enum Ev<'a> {
    Text(&'a str),
    /// A model turn finished (text may be followed by calls).
    Turn {
        usage: &'a Usage,
    },
    ToolStart {
        idx: usize,
        command: &'a str,
    },
    ToolEnd {
        idx: usize,
        output: &'a str,
        code: Option<i32>,
        lines: usize,
    },
    /// A message typed while the model was working was delivered.
    Steered(&'a str),
    Compacting,
    Compacted(&'a str),
    /// Something was appended to the log.
    Appended,
}

#[derive(Clone, Debug)]
pub struct Compaction {
    pub auto: bool,
    /// Compact when the context is within this many tokens of the window.
    pub reserve: u64,
    /// Recent tokens kept word for word.
    pub keep: u64,
}

pub struct Agent {
    pub session: Session,
    pub provider: Box<dyn Provider>,
    pub depth: Depth,
    pub max_steps: usize,
    pub compaction: Compaction,
    /// Messages typed while the model works, delivered before its next turn.
    pub steer: Arc<Mutex<Vec<String>>>,
}

pub const FORKED_NOTE: &str = "(This call started you. You are a copy of the agent above, forked at this point; the original is waiting for your answer. Do the task below and reply with the result.)";

pub const COMPACT_PROMPT: &str = "Write a summary of this session for a fresh copy of yourself that will continue the work: the goal, what is done, the current state, what is left, key files and commands, and anything that failed. Be concise and specific. Do not call tools.";

/// Rough tokens for text the provider has not counted yet.
fn est(s: &str) -> u64 {
    s.len() as u64 / 4
}

fn event_chars(e: &Event) -> u64 {
    match &e.body {
        Body::System { text } | Body::User { text, .. } | Body::Summary { text, .. } => est(text),
        Body::Assistant { text, calls, .. } => {
            est(text) + calls.iter().map(|c| est(&c.command)).sum::<u64>()
        }
        Body::Tool { output, .. } => est(output),
    }
}

impl Agent {
    pub fn new(session: Session, provider: Box<dyn Provider>) -> Agent {
        let max_steps = std::env::var("TAU_MAX_STEPS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(200);
        let s = crate::config::Settings::load();
        Agent {
            session,
            provider,
            depth: Depth::from_env(),
            max_steps,
            compaction: Compaction {
                auto: s.auto_compact && std::env::var("TAU_NO_COMPACT").is_err(),
                reserve: s.compact_reserve,
                keep: s.compact_keep,
            },
            steer: Arc::new(Mutex::new(vec![])),
        }
    }

    pub fn env(&self, node: Option<u64>) -> Vec<(String, String)> {
        let tau_bin = crate::log::tau_home().join("bin");
        let self_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(PathBuf::from))
            .unwrap_or_default();
        let path = format!(
            "{}:{}:{}",
            tau_bin.display(),
            self_dir.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let node = match node {
            Some(n) => format!("{}/{n}", self.session.id),
            None => self.session.id.clone(),
        };
        let mut v = vec![
            ("PATH".into(), path),
            ("TAU_SESSION".into(), self.session.id.clone()),
            ("TAU_NODE".into(), node),
            (
                "TAU_EVENTS".into(),
                self.session.events_path().display().to_string(),
            ),
            ("TAU_PROVIDER".into(), self.provider.name().to_string()),
            ("TAU_MODEL".into(), self.provider.model().to_string()),
            ("PAGER".into(), "cat".into()),
            ("GIT_PAGER".into(), "cat".into()),
            ("GIT_TERMINAL_PROMPT".into(), "0".into()),
        ];
        v.extend(self.depth.child_env());
        v
    }

    /// Runs one user message to completion. Returns the final text.
    pub fn run_with_images(
        &mut self,
        prompt: &str,
        images: Vec<String>,
        sink: &mut dyn FnMut(Ev),
    ) -> Result<String> {
        if self.session.meta.task.is_empty() {
            self.session.meta.task = prompt
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(100)
                .collect();
            let _ = self.session.save_meta();
        }
        self.session.append(Body::User {
            text: prompt.to_string(),
            images,
        })?;
        sink(Ev::Appended);
        self.resume(sink)
    }

    /// Tokens the next request will carry: the provider's count for the last
    /// turn plus an estimate for what was appended since.
    pub fn context_tokens(&self) -> u64 {
        let chain = self.session.chain(self.session.head);
        let mut tail = 0;
        for e in chain.iter().rev() {
            match &e.body {
                Body::Assistant { usage, .. } if usage.input + usage.cache_read > 0 => {
                    return usage.input
                        + usage.cache_read
                        + usage.cache_write
                        + usage.output
                        + tail;
                }
                // older counts include context a summary has since replaced
                Body::Summary { .. } => break,
                _ => tail += event_chars(e),
            }
        }
        let c = self.session.render();
        est(&c.system) + est(&serde_json::to_string(&c.msgs).unwrap_or_default())
    }

    pub fn context_window(&self) -> u64 {
        crate::prices::context_window(self.provider.model())
    }

    fn needs_compaction(&self) -> bool {
        let w = self.context_window();
        self.compaction.auto && self.context_tokens() + self.compaction.reserve.min(w / 4) > w
    }

    /// Continues from the current head until the model stops calling bash.
    pub fn resume(&mut self, sink: &mut dyn FnMut(Ev)) -> Result<String> {
        let mut overflow_retried = false;
        for _ in 0..self.max_steps {
            if let Err(e) = self.depth.check() {
                bail!("{e}");
            }
            if exec::cancelled() {
                bail!("interrupted");
            }
            let steered: Vec<String> = std::mem::take(&mut *self.steer.lock().unwrap());
            for text in steered {
                self.session.append(Body::User {
                    text: text.clone(),
                    images: vec![],
                })?;
                sink(Ev::Steered(&text));
                sink(Ev::Appended);
            }
            if self.needs_compaction() {
                self.compact(None, true, sink)?;
            }
            let ctx = self.session.render();
            let reply = match self
                .provider
                .complete(&ctx.system, &ctx.msgs, &mut |t| sink(Ev::Text(t)))
            {
                Ok(r) => r,
                Err(e) if is_overflow(&e) && !overflow_retried => {
                    overflow_retried = true;
                    self.compact(None, true, sink)?;
                    continue;
                }
                Err(e) => return Err(e),
            };
            sink(Ev::Turn {
                usage: &reply.usage,
            });
            let node = self.session.append(Body::Assistant {
                text: reply.text.clone(),
                calls: reply.calls.clone(),
                raw: reply.raw,
                provider: self.provider.name().to_string(),
                model: self.provider.model().to_string(),
                usage: reply.usage,
            })?;
            sink(Ev::Appended);
            if reply.calls.is_empty() {
                return Ok(reply.text);
            }
            let ex = Exec::new(self.session.dir.clone(), self.env(Some(node)));
            let todo: Vec<(String, u64)> = reply
                .calls
                .iter()
                .map(|c| (c.command.clone(), c.timeout.unwrap_or(DEFAULT_TIMEOUT)))
                .collect();
            let results = if exec::cancelled() {
                vec![]
            } else {
                for (i, c) in reply.calls.iter().enumerate() {
                    sink(Ev::ToolStart {
                        idx: i,
                        command: &c.command,
                    });
                }
                ex.run_all(&todo, |i, o| {
                    sink(Ev::ToolEnd {
                        idx: i,
                        output: &o.text,
                        code: o.code,
                        lines: o.lines,
                    })
                })
            };
            // every call needs a result, in call order, or the history is invalid
            for (i, c) in reply.calls.iter().enumerate() {
                let output = match results.get(i) {
                    Some(Ok(o)) => o.text.clone(),
                    Some(Err(e)) => format!("[tau could not run this: {e:#}]"),
                    None => "[not run: interrupted by the user]".to_string(),
                };
                self.session.append(Body::Tool {
                    call_id: c.id.clone(),
                    output,
                })?;
            }
            sink(Ev::Appended);
        }
        bail!("stopped after {} steps (TAU_MAX_STEPS)", self.max_steps)
    }

    /// Appends a summary node. With `keep_recent`, the last `keep` tokens of
    /// the conversation, cut at a user message, stay word for word after it.
    /// Nothing is deleted from the log.
    pub fn compact(
        &mut self,
        instructions: Option<&str>,
        keep_recent: bool,
        sink: &mut dyn FnMut(Ev),
    ) -> Result<String> {
        sink(Ev::Compacting);
        let chain: Vec<Event> = self
            .session
            .chain(self.session.head)
            .into_iter()
            .cloned()
            .collect();
        let refs: Vec<&Event> = chain.iter().collect();
        let start = refs
            .iter()
            .rposition(|e| matches!(e.body, Body::Summary { .. }))
            .map(|i| i + 1)
            .unwrap_or(1);
        let cut = if keep_recent {
            cut_point(&refs, start, self.compaction.keep)
        } else {
            None
        };
        let upto = cut.unwrap_or(refs.len());
        let mut ctx = render(&refs[..upto]);
        let mut ask = COMPACT_PROMPT.to_string();
        if let Some(i) = instructions {
            ask.push_str(&format!("\n\nAlso: {i}"));
        }
        ctx.msgs.push(Msg::user(ask));
        let reply = self
            .provider
            .complete(&ctx.system, &ctx.msgs, &mut |_| {})?;
        let text = if reply.text.trim().is_empty() {
            "(the summary came back empty)".to_string()
        } else {
            reply.text
        };
        self.session.append(Body::Summary {
            text: text.clone(),
            usage: reply.usage,
            keep_from: cut.map(|i| refs[i].id),
        })?;
        sink(Ev::Compacted(&text));
        sink(Ev::Appended);
        Ok(text)
    }
}

/// Index of the user message where the kept tail starts, or None when the
/// tail would be everything since `start` or no user message fits.
pub fn cut_point(chain: &[&Event], start: usize, keep: u64) -> Option<usize> {
    let mut tokens = 0;
    let mut i = chain.len();
    while i > start {
        i -= 1;
        tokens += event_chars(chain[i]);
        if tokens >= keep {
            break;
        }
    }
    // cut at a user message so no tool result loses its call: the start of
    // the turn that crossed the keep limit, or failing that the next turn
    let is_user = |j: &usize| matches!(chain[*j].body, Body::User { .. });
    let at = (start + 1..=i)
        .rev()
        .find(is_user)
        .or_else(|| (i..chain.len()).find(is_user))?;
    (at > start).then_some(at)
}

/// A fresh session whose first event is the system prompt for this directory.
pub fn new_session(parent: Option<String>, depth: u32) -> Result<Session> {
    let cwd = std::env::current_dir()?;
    let mut s = Session::create(parent, None, depth)?;
    s.append(Body::System {
        text: crate::prompt::build(&cwd),
    })?;
    Ok(s)
}

/// Starts a session that begins with a copy of another session's chain up to
/// a node. The copy is byte-identical, so the provider cache still hits.
pub fn fork(spec: &str, depth: u32) -> Result<Session> {
    let (sid, node) = parse_node(spec);
    let parent = Session::open(&sid)?;
    let head = node.or(parent.head);
    if let Some(n) = head
        && parent.get(n).is_none()
    {
        bail!("session {sid} has no node {n}");
    }
    let chain: Vec<Event> = parent.chain(head).into_iter().cloned().collect();
    let mut s = Session::create(Some(sid), head, depth)?;
    for e in chain {
        s.write_event(e)?;
    }
    // a fork taken mid-turn owes the model results for the calls in flight
    let pending: Vec<String> = match s.head.and_then(|h| s.get(h)).map(|e| &e.body) {
        Some(Body::Assistant { calls, .. }) => calls.iter().map(|c| c.id.clone()).collect(),
        _ => vec![],
    };
    for id in pending {
        s.append(Body::Tool {
            call_id: id,
            output: FORKED_NOTE.into(),
        })?;
    }
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::Call;

    fn ev(id: u64, body: Body) -> Event {
        Event {
            id,
            parent: id.checked_sub(1),
            ts: 0,
            body,
        }
    }

    fn user(t: &str) -> Body {
        Body::User {
            text: t.into(),
            images: vec![],
        }
    }

    fn asst(t: &str, call: bool) -> Body {
        Body::Assistant {
            text: t.into(),
            calls: if call {
                vec![Call {
                    id: "c".into(),
                    command: "ls".into(),
                    timeout: None,
                }]
            } else {
                vec![]
            },
            raw: None,
            provider: "mock".into(),
            model: "mock".into(),
            usage: Usage::default(),
        }
    }

    #[test]
    fn cut_lands_on_a_user_message() {
        let big = "x".repeat(4000); // ~1000 tokens
        let events = [
            ev(0, Body::System { text: "s".into() }),
            ev(1, user("first")),
            ev(2, asst(&big, true)),
            ev(
                3,
                Body::Tool {
                    call_id: "c".into(),
                    output: big.clone(),
                },
            ),
            ev(4, user("second")),
            ev(5, asst(&big, true)),
            ev(
                6,
                Body::Tool {
                    call_id: "c".into(),
                    output: big.clone(),
                },
            ),
            ev(7, asst("done", false)),
        ];
        let refs: Vec<&Event> = events.iter().collect();
        // ~1500 tokens reaches into the second turn, which is kept whole
        assert_eq!(cut_point(&refs, 1, 1500), Some(4));
        // keeping everything means there is nothing to cut
        assert_eq!(cut_point(&refs, 1, 1_000_000), None);
        // a single long turn has nowhere to cut
        assert_eq!(cut_point(&refs[..4], 1, 10), None);
    }

    #[test]
    fn steering_lands_before_the_next_turn() {
        crate::log::test_home();
        let s = new_session(None, 0).unwrap();
        let mock = crate::provider::mock::Mock::new("$ echo hi\n---\nsaw it");
        let mut a = Agent::new(s, Box::new(mock));
        let steer = a.steer.clone();
        let out = a
            .run_with_images("start", vec![], &mut |ev| {
                if let Ev::ToolEnd { .. } = ev {
                    steer.lock().unwrap().push("also do X".into());
                }
            })
            .unwrap();
        assert_eq!(out, "saw it");
        let kinds: Vec<String> = a
            .session
            .chain(a.session.head)
            .iter()
            .map(|e| match &e.body {
                Body::System { .. } => "system".to_string(),
                Body::User { text, .. } => format!("user:{text}"),
                Body::Assistant { text, .. } => format!("assistant:{text}"),
                Body::Tool { .. } => "tool".to_string(),
                Body::Summary { .. } => "summary".to_string(),
            })
            .collect();
        assert_eq!(
            kinds,
            [
                "system",
                "user:start",
                "assistant:",
                "tool",
                "user:also do X",
                "assistant:saw it"
            ]
        );
    }
}
