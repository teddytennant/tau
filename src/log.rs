//! The session log: an append-only DAG of events, one JSON object per line.
//! Everything the model sees is `render(events, head)`. Nothing already written
//! is ever rewritten, which is what keeps the provider's cached prefix valid.

use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Event {
    pub id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<u64>,
    pub ts: u64,
    #[serde(flatten)]
    pub body: Body,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Body {
    System {
        text: String,
    },
    User {
        text: String,
        /// Image files, copied into the session directory.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        images: Vec<String>,
    },
    Assistant {
        text: String,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        calls: Vec<Call>,
        /// Provider-native content (thinking blocks, signatures). Sent back
        /// untouched to the same provider, ignored by any other.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        raw: Option<Value>,
        provider: String,
        model: String,
        #[serde(default)]
        usage: Usage,
    },
    Tool {
        call_id: String,
        output: String,
    },
    /// Everything before this node is replaced by `text` when rendering,
    /// except the span from `keep_from` on, which is kept word for word.
    Summary {
        text: String,
        #[serde(default)]
        usage: Usage,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        keep_from: Option<u64>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Call {
    pub id: String,
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    #[serde(default)]
    pub cache_read: u64,
    #[serde(default)]
    pub cache_write: u64,
    #[serde(default)]
    pub cost: f64,
}

impl Usage {
    pub fn add(&mut self, o: &Usage) {
        self.input += o.input;
        self.output += o.output;
        self.cache_read += o.cache_read;
        self.cache_write += o.cache_write;
        self.cost += o.cost;
    }
    /// Share of prompt tokens served from cache, 0..=100.
    pub fn cache_pct(&self) -> u64 {
        (self.cache_read * 100)
            .checked_div(self.input + self.cache_read + self.cache_write)
            .unwrap_or(0)
    }
}

/// What a provider is asked to continue. Provider-neutral.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub enum Msg {
    User {
        text: String,
        images: Vec<Image>,
    },
    Assistant {
        text: String,
        calls: Vec<Call>,
        raw: Option<(String, Value)>,
    },
    Tool {
        call_id: String,
        output: String,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Image {
    pub media_type: String,
    /// Base64.
    pub data: String,
}

impl Msg {
    pub fn user(text: impl Into<String>) -> Msg {
        Msg::User {
            text: text.into(),
            images: vec![],
        }
    }
}

pub fn media_type(path: &str) -> Option<&'static str> {
    let p = path.to_ascii_lowercase();
    [
        (".png", "image/png"),
        (".jpg", "image/jpeg"),
        (".jpeg", "image/jpeg"),
        (".gif", "image/gif"),
        (".webp", "image/webp"),
    ]
    .iter()
    .find(|(ext, _)| p.ends_with(ext))
    .map(|(_, m)| *m)
}

fn load_image(path: &str) -> Option<Image> {
    use base64::Engine;
    let bytes = fs::read(path).ok()?;
    Some(Image {
        media_type: media_type(path)?.to_string(),
        data: base64::engine::general_purpose::STANDARD.encode(bytes),
    })
}

pub struct Context {
    pub system: String,
    pub msgs: Vec<Msg>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Meta {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// Node of the parent this session was forked from. Events with ids up to
    /// and including it were copied from the parent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<u64>,
    pub cwd: String,
    pub created: u64,
    #[serde(default)]
    pub task: String,
    #[serde(default)]
    pub depth: u32,
    #[serde(default)]
    pub status: String,
    /// Set when /tree moved the head off the last event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<u64>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
}

pub struct Session {
    pub id: String,
    pub dir: PathBuf,
    pub meta: Meta,
    pub events: Vec<Event>,
    pub head: Option<u64>,
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn tau_home() -> PathBuf {
    if let Some(h) = std::env::var_os("TAU_HOME") {
        return PathBuf::from(h);
    }
    let home = std::env::var_os("HOME").unwrap_or_else(|| ".".into());
    PathBuf::from(home).join(".tau")
}

pub fn sessions_dir() -> PathBuf {
    tau_home().join("sessions")
}

fn new_id() -> String {
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = d.as_secs();
    // civil date from unix days, so ids sort by time and read like dates
    let days = (secs / 86400) as i64;
    let (y, m, dd) = civil(days);
    let t = secs % 86400;
    let salt = (d.subsec_nanos() ^ std::process::id().wrapping_mul(2654435761)) & 0xffff;
    format!(
        "{y:04}{m:02}{dd:02}-{:02}{:02}{:02}-{salt:04x}",
        t / 3600,
        t / 60 % 60,
        t % 60
    )
}

fn civil(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    (y, m, d)
}

impl Session {
    pub fn create(parent: Option<String>, from: Option<u64>, depth: u32) -> Result<Session> {
        let id = new_id();
        let dir = sessions_dir().join(&id);
        fs::create_dir_all(dir.join("out"))?;
        let meta = Meta {
            id: id.clone(),
            parent,
            from,
            cwd: std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            created: now(),
            task: String::new(),
            depth,
            status: "running".into(),
            head: None,
            name: String::new(),
        };
        let s = Session {
            id,
            dir,
            meta,
            events: vec![],
            head: None,
        };
        s.save_meta()?;
        fs::write(s.events_path(), "")?;
        Ok(s)
    }

    pub fn open(id: &str) -> Result<Session> {
        let dir = sessions_dir().join(id);
        if !dir.is_dir() {
            bail!("no session {id}");
        }
        let meta: Meta = serde_json::from_str(&fs::read_to_string(dir.join("meta.json"))?)
            .with_context(|| format!("reading meta for {id}"))?;
        let mut events = vec![];
        for (i, line) in fs::read_to_string(dir.join("events.jsonl"))?
            .lines()
            .enumerate()
        {
            if line.trim().is_empty() {
                continue;
            }
            // a torn last line from a killed process is skipped, not fatal
            match serde_json::from_str::<Event>(line) {
                Ok(e) => events.push(e),
                Err(e) => eprintln!("tau: skipping bad event on line {} of {id}: {e}", i + 1),
            }
        }
        let head = meta
            .head
            .filter(|h| events.iter().any(|e| e.id == *h))
            .or(events.last().map(|e| e.id));
        Ok(Session {
            id: id.to_string(),
            dir,
            meta,
            events,
            head,
        })
    }

    /// Most recently created session, preferring ones started in `cwd`.
    pub fn latest(cwd: Option<&str>) -> Option<String> {
        let mut all = list_metas();
        all.sort_by_key(|m| m.created);
        if let Some(c) = cwd
            && let Some(m) = all.iter().rev().find(|m| m.cwd == c)
        {
            return Some(m.id.clone());
        }
        all.last().map(|m| m.id.clone())
    }

    pub fn events_path(&self) -> PathBuf {
        self.dir.join("events.jsonl")
    }

    pub fn save_meta(&self) -> Result<()> {
        let tmp = self.dir.join("meta.json.tmp");
        fs::write(&tmp, serde_json::to_string_pretty(&self.meta)?)?;
        fs::rename(tmp, self.dir.join("meta.json"))?;
        Ok(())
    }

    fn next_id(&self) -> u64 {
        self.events.iter().map(|e| e.id + 1).max().unwrap_or(0)
    }

    pub fn append(&mut self, body: Body) -> Result<u64> {
        let e = Event {
            id: self.next_id(),
            parent: self.head,
            ts: now(),
            body,
        };
        self.write_event(e)
    }

    /// Writes an event exactly as given. Used when copying a parent's chain.
    pub fn write_event(&mut self, e: Event) -> Result<u64> {
        let mut f = OpenOptions::new().append(true).open(self.events_path())?;
        let mut line = serde_json::to_string(&e)?;
        line.push('\n');
        f.write_all(line.as_bytes())?;
        let id = e.id;
        self.events.push(e);
        self.head = Some(id);
        if self.meta.head.take().is_some() {
            self.save_meta()?;
        }
        Ok(id)
    }

    /// Moves the head, so the next message branches from `id`.
    pub fn set_head(&mut self, id: u64) -> Result<()> {
        if self.get(id).is_none() {
            bail!("no node {id}");
        }
        self.head = Some(id);
        self.meta.head = Some(id);
        self.save_meta()
    }

    /// Copies an image into the session so the log never points at a file
    /// that later moves.
    pub fn store_image(&self, src: &std::path::Path) -> Result<String> {
        let name = src
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("image.png");
        let dir = self.dir.join("images");
        fs::create_dir_all(&dir)?;
        let n = fs::read_dir(&dir).map(|r| r.count()).unwrap_or(0);
        let dst = dir.join(format!("{n}-{name}"));
        fs::copy(src, &dst)?;
        Ok(dst.display().to_string())
    }

    pub fn get(&self, id: u64) -> Option<&Event> {
        self.events.iter().find(|e| e.id == id)
    }

    /// Events from the root to `head`, in order.
    pub fn chain(&self, head: Option<u64>) -> Vec<&Event> {
        let by_id: HashMap<u64, &Event> = self.events.iter().map(|e| (e.id, e)).collect();
        let mut out = vec![];
        let mut cur = head;
        while let Some(id) = cur {
            match by_id.get(&id) {
                Some(e) => {
                    out.push(*e);
                    cur = e.parent;
                }
                None => break,
            }
        }
        out.reverse();
        out
    }

    pub fn render(&self) -> Context {
        render(&self.chain(self.head))
    }

    /// Spend of this session alone, not counting events copied from a parent.
    pub fn own_usage(&self) -> Usage {
        let from = self.meta.from;
        let mut u = Usage::default();
        for e in &self.events {
            if from.is_some_and(|f| e.id <= f) {
                continue;
            }
            match &e.body {
                Body::Assistant { usage, .. } | Body::Summary { usage, .. } => u.add(usage),
                _ => {}
            }
        }
        u
    }

    pub fn set_status(&mut self, s: &str) {
        self.meta.status = s.to_string();
        let _ = self.save_meta();
    }
}

pub fn list_metas() -> Vec<Meta> {
    let mut out = vec![];
    if let Ok(rd) = fs::read_dir(sessions_dir()) {
        for ent in rd.flatten() {
            if let Ok(s) = fs::read_to_string(ent.path().join("meta.json"))
                && let Ok(m) = serde_json::from_str::<Meta>(&s)
            {
                out.push(m);
            }
        }
    }
    out
}

/// The context for a chain. A summary node replaces everything before it
/// except the system prompt and the span it keeps.
pub fn render(chain: &[&Event]) -> Context {
    let mut system = String::new();
    let mut last_summary = None;
    for (i, e) in chain.iter().enumerate() {
        match &e.body {
            Body::System { text } if i == 0 => system = text.clone(),
            Body::Summary { .. } => last_summary = Some(i),
            _ => {}
        }
    }
    let mut msgs = vec![];
    // the order the model sees: summary, kept span, then what came after
    let order: Vec<&Event> = match last_summary {
        None => chain.to_vec(),
        Some(i) => {
            let keep_from = match &chain[i].body {
                Body::Summary { keep_from, .. } => *keep_from,
                _ => None,
            };
            let kept_start = keep_from
                .and_then(|k| chain[..i].iter().position(|e| e.id == k))
                .unwrap_or(i);
            let mut v = vec![chain[i]];
            v.extend(&chain[kept_start..i]);
            v.extend(&chain[i + 1..]);
            v
        }
    };
    for e in order {
        match &e.body {
            Body::System { .. } => {}
            Body::User { text, images } => msgs.push(Msg::User {
                text: text.clone(),
                images: images.iter().filter_map(|p| load_image(p)).collect(),
            }),
            Body::Summary { text, .. } => msgs.push(Msg::user(format!(
                "Summary of the work so far (earlier context was compacted):\n\n{text}"
            ))),
            Body::Assistant {
                text,
                calls,
                raw,
                provider,
                ..
            } => msgs.push(Msg::Assistant {
                text: text.clone(),
                calls: calls.clone(),
                raw: raw.clone().map(|r| (provider.clone(), r)),
            }),
            // a summary made mid-turn can drop the call a result answers
            Body::Tool { call_id, output } => msgs.push(Msg::Tool {
                call_id: call_id.clone(),
                output: output.clone(),
            }),
        }
    }
    Context { system, msgs }
}

/// `session/node` or a bare session id (meaning its head).
pub fn parse_node(s: &str) -> (String, Option<u64>) {
    match s.rsplit_once('/') {
        Some((sess, n)) => match n.parse() {
            Ok(n) => (sess.to_string(), Some(n)),
            Err(_) => (s.to_string(), None),
        },
        None => (s.to_string(), None),
    }
}

pub fn session_dir(id: &str) -> PathBuf {
    sessions_dir().join(id)
}

/// One TAU_HOME for every unit test in the process. Each test makes its own
/// session, and setting different homes from parallel tests would race.
#[cfg(test)]
pub fn test_home() -> &'static std::path::Path {
    static HOME: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    let h = HOME.get_or_init(|| {
        let d = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("TAU_HOME", d.path()) };
        d
    });
    h.path()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_home<T>(f: impl FnOnce() -> T) -> T {
        test_home();
        f()
    }

    fn assistant(text: &str, cmd: Option<&str>) -> Body {
        Body::Assistant {
            text: text.into(),
            calls: cmd
                .map(|c| {
                    vec![Call {
                        id: format!("c{}", c.len()),
                        command: c.into(),
                        timeout: None,
                    }]
                })
                .unwrap_or_default(),
            raw: None,
            provider: "mock".into(),
            model: "mock".into(),
            usage: Usage::default(),
        }
    }

    fn bytes(msgs: &[Msg]) -> Vec<String> {
        crate::provider::anthropic::messages(msgs)
            .iter()
            .map(|m| m.to_string())
            .collect()
    }

    #[test]
    fn appending_never_changes_earlier_bytes() {
        with_home(|| {
            let mut s = Session::create(None, None, 0).unwrap();
            s.append(Body::System { text: "sys".into() }).unwrap();
            s.append(Body::User {
                text: "fix it".into(),
                images: vec![],
            })
            .unwrap();
            s.append(assistant("looking", Some("cat a.rs"))).unwrap();
            s.append(Body::Tool {
                call_id: "c8".into(),
                output: "fn main() {}".into(),
            })
            .unwrap();
            let before = bytes(&s.render().msgs);
            let sys_before = s.render().system;
            s.append(assistant("", Some("cargo test"))).unwrap();
            s.append(Body::Tool {
                call_id: "c10".into(),
                output: "ok".into(),
            })
            .unwrap();
            s.append(assistant("done", None)).unwrap();
            let after = bytes(&s.render().msgs);
            assert_eq!(s.render().system, sys_before);
            assert!(after.len() > before.len());
            assert_eq!(after[..before.len()], before[..]);

            // and a reload from disk renders the same bytes
            let reopened = Session::open(&s.id).unwrap();
            assert_eq!(bytes(&reopened.render().msgs), after);
        })
    }

    #[test]
    fn summary_cuts_history_but_keeps_system() {
        with_home(|| {
            let mut s = Session::create(None, None, 0).unwrap();
            s.append(Body::System { text: "sys".into() }).unwrap();
            s.append(Body::User {
                text: "old".into(),
                images: vec![],
            })
            .unwrap();
            s.append(assistant("old reply", None)).unwrap();
            s.append(Body::Summary {
                text: "did the thing".into(),
                usage: Usage::default(),
                keep_from: None,
            })
            .unwrap();
            s.append(Body::User {
                text: "new".into(),
                images: vec![],
            })
            .unwrap();
            let c = s.render();
            assert_eq!(c.system, "sys");
            assert_eq!(c.msgs.len(), 2);
            assert!(matches!(&c.msgs[0], Msg::User { text, .. } if text.contains("did the thing")));
            // nothing was deleted
            assert_eq!(Session::open(&s.id).unwrap().events.len(), 5);
        })
    }

    #[test]
    fn branches_render_their_own_chain() {
        with_home(|| {
            let mut s = Session::create(None, None, 0).unwrap();
            s.append(Body::System { text: "sys".into() }).unwrap();
            let u = s
                .append(Body::User {
                    text: "a".into(),
                    images: vec![],
                })
                .unwrap();
            s.append(assistant("first answer", None)).unwrap();
            s.head = Some(u);
            s.append(assistant("second answer", None)).unwrap();
            let c = s.render();
            assert_eq!(c.msgs.len(), 2);
            assert!(matches!(&c.msgs[1], Msg::Assistant { text, .. } if text == "second answer"));
        })
    }

    #[test]
    fn node_specs() {
        assert_eq!(parse_node("abc/7"), ("abc".into(), Some(7)));
        assert_eq!(parse_node("abc"), ("abc".into(), None));
    }

    #[test]
    fn ids_look_like_dates() {
        assert_eq!(civil(0), (1970, 1, 1));
        assert_eq!(civil(20718), (2026, 9, 22));
    }

    #[test]
    fn summary_keeps_its_span_after_it() {
        with_home(|| {
            let mut s = Session::create(None, None, 0).unwrap();
            s.append(Body::System { text: "sys".into() }).unwrap();
            s.append(Body::User {
                text: "old".into(),
                images: vec![],
            })
            .unwrap();
            s.append(assistant("old reply", None)).unwrap();
            let keep = s
                .append(Body::User {
                    text: "recent".into(),
                    images: vec![],
                })
                .unwrap();
            s.append(assistant("recent reply", None)).unwrap();
            s.append(Body::Summary {
                text: "the old part".into(),
                usage: Usage::default(),
                keep_from: Some(keep),
            })
            .unwrap();
            s.append(Body::User {
                text: "next".into(),
                images: vec![],
            })
            .unwrap();
            let texts: Vec<String> = s
                .render()
                .msgs
                .iter()
                .map(|m| match m {
                    Msg::User { text, .. } => text.clone(),
                    Msg::Assistant { text, .. } => text.clone(),
                    Msg::Tool { output, .. } => output.clone(),
                })
                .collect();
            assert!(texts[0].contains("the old part"));
            assert_eq!(texts[1..], ["recent", "recent reply", "next"]);
        })
    }

    #[test]
    fn head_set_by_tree_survives_a_reload() {
        with_home(|| {
            let mut s = Session::create(None, None, 0).unwrap();
            s.append(Body::System { text: "sys".into() }).unwrap();
            let u = s
                .append(Body::User {
                    text: "a".into(),
                    images: vec![],
                })
                .unwrap();
            s.append(assistant("b", None)).unwrap();
            s.set_head(u).unwrap();
            assert_eq!(Session::open(&s.id).unwrap().head, Some(u));
            // the next append branches and the saved head is cleared
            let n = s.append(assistant("c", None)).unwrap();
            let r = Session::open(&s.id).unwrap();
            assert_eq!(r.head, Some(n));
            assert_eq!(r.get(n).unwrap().parent, Some(u));
        })
    }
}
