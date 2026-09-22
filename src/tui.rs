//! The terminal UI. It extends the same way everything else in tau does:
//! executables in ~/.tau/panels print into a side panel, executables in
//! ~/.tau/commands become /slash commands, Markdown in ~/.tau/prompts
//! becomes a template. All of them are looked up on use, so something the
//! model writes mid-session shows up without a restart.

use crate::agent::{Agent, Ev};
use crate::config::{Settings, THINKING};
use crate::exec::{self, Exec};
use crate::log::{Body, Session, Usage, list_metas, tau_home};
use crate::login;
use crate::provider::{self, ModelInfo, Spec};
use crate::theme::{self, Theme};
use crate::widgets::{Item as PItem, Pick, Picker, message};
use anyhow::Result;
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::execute;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthChar;

enum Req {
    Prompt { text: String, images: Vec<PathBuf> },
    Compact(Option<String>),
    SetProvider(Spec, String),
    SetModel(String),
    SetThinking(String),
    SetHead(u64),
    Open(String),
    New,
    Fork(String),
    Shell { cmd: String, send: bool },
}

enum Up {
    Text(String),
    Turn(Usage),
    ToolStart {
        idx: usize,
        command: String,
    },
    ToolEnd {
        idx: usize,
        output: String,
        code: Option<i32>,
        lines: usize,
    },
    Steered(String),
    Compacting,
    Compacted(String),
    Appended,
    Done(Result<String, String>),
    Note(String, bool),
    Command {
        stdout: String,
        stderr: String,
        label: String,
    },
    Reloaded(Status),
    Models(Vec<ModelInfo>),
    Ctx(u64, u64),
    Shell {
        cmd: String,
        output: String,
        code: Option<i32>,
        lines: usize,
    },
}

/// What the status line shows about the agent.
#[derive(Clone, Default)]
struct Status {
    session: String,
    provider: String,
    model: String,
    usage: Usage,
    ctx: u64,
    window: u64,
    depth: u32,
}

enum Item {
    User(String),
    Assistant(String),
    Tool {
        command: String,
        output: Option<String>,
        code: Option<i32>,
        lines: usize,
    },
    Note(String, bool),
}

#[derive(Clone, Copy, PartialEq)]
enum Purpose {
    Model,
    Resume,
    Fork,
    Tree,
    Settings,
    Logout,
}

enum Modal {
    Pick(Picker, Purpose),
    Login(login::Flow),
    Help,
}

fn executables(dir: &Path) -> Vec<(String, PathBuf)> {
    let mut v: Vec<(String, PathBuf)> = std::fs::read_dir(dir)
        .map(|r| {
            r.flatten()
                .filter(|e| {
                    e.metadata()
                        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                        .unwrap_or(false)
                })
                .filter_map(|e| Some((e.file_name().to_str()?.to_string(), e.path())))
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

/// Runs a program with a hard timeout, returning (stdout, stderr, code).
fn run_with_timeout(
    path: &Path,
    args: &[&str],
    env: &[(String, String)],
    secs: u64,
) -> (String, String, Option<i32>) {
    use std::os::unix::process::CommandExt;
    let dir = std::env::temp_dir().join(format!("tau-ui-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let tag = format!(
        "{}-{:?}",
        path.file_name().and_then(|s| s.to_str()).unwrap_or("x"),
        std::thread::current().id()
    )
    .replace(|c: char| !c.is_ascii_alphanumeric() && c != '-', "");
    let (o, e) = (
        dir.join(format!("{tag}.out")),
        dir.join(format!("{tag}.err")),
    );
    let (Ok(fo), Ok(fe)) = (std::fs::File::create(&o), std::fs::File::create(&e)) else {
        return (String::new(), "could not create temp files".into(), None);
    };
    let child = Command::new(path)
        .args(args)
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(Stdio::null())
        .stdout(fo)
        .stderr(fe)
        .process_group(0)
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(err) => return (String::new(), err.to_string(), None),
    };
    let start = Instant::now();
    let code = loop {
        if let Ok(Some(st)) = child.try_wait() {
            break st.code();
        }
        if start.elapsed() > Duration::from_secs(secs) {
            exec::kill_group(child.id() as i32, libc::SIGKILL);
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let r = (exec::read_lossy(&o), exec::read_lossy(&e), code);
    let _ = std::fs::remove_file(o);
    let _ = std::fs::remove_file(e);
    r
}

#[derive(Default)]
struct Panels {
    out: Arc<Mutex<BTreeMap<String, String>>>,
    busy: Arc<Mutex<HashSet<String>>>,
    last: Option<Instant>,
}

impl Panels {
    fn refresh(&mut self, env: &[(String, String)]) {
        self.last = Some(Instant::now());
        let found = executables(&tau_home().join("panels"));
        {
            let mut out = self.out.lock().unwrap();
            out.retain(|k, _| found.iter().any(|(n, _)| n == k));
            for (n, _) in &found {
                out.entry(n.clone()).or_insert_with(|| "…".into());
            }
        }
        for (name, path) in found {
            if !self.busy.lock().unwrap().insert(name.clone()) {
                continue;
            }
            let (out, busy, env) = (self.out.clone(), self.busy.clone(), env.to_vec());
            std::thread::spawn(move || {
                let (so, se, code) = run_with_timeout(&path, &[], &env, 5);
                let text = if code == Some(0) || !so.is_empty() {
                    so
                } else {
                    format!("error: {}", se.trim())
                };
                out.lock().unwrap().insert(name.clone(), text);
                busy.lock().unwrap().remove(&name);
            });
        }
    }
}

const BUILTINS: [(&str, &str); 18] = [
    ("help", "keys and commands"),
    ("login", "add or change a provider key"),
    ("logout", "forget a saved key"),
    ("model", "pick a model (ctrl+l)"),
    ("thinking", "cycle the thinking level (shift+tab)"),
    ("settings", "theme, thinking, compaction"),
    ("resume", "open an earlier session"),
    ("new", "start a fresh session"),
    ("tree", "jump to any point in this session and branch"),
    ("fork", "new session from an earlier message"),
    ("clone", "new session from here"),
    ("compact", "summarize the history now, /compact [focus]"),
    ("export", "write this branch to an HTML file"),
    ("session", "session id, files, tokens and cost"),
    ("name", "name this session, /name TEXT"),
    ("copy", "copy the last answer to the clipboard"),
    ("reload", "re-read files for @ completion"),
    ("quit", "exit (ctrl+d)"),
];

struct Popup {
    kind: PopKind,
    items: Vec<(String, String)>,
    sel: usize,
}

#[derive(PartialEq)]
enum PopKind {
    File,
    Command,
}

struct Ui {
    items: Vec<Item>,
    open_text: bool,
    tools: HashMap<usize, usize>,
    input: Vec<char>,
    cursor: usize,
    history: Vec<String>,
    hist_pos: Option<usize>,
    follow_up: Vec<String>,
    steer: Arc<Mutex<Vec<String>>>,
    busy: bool,
    running: usize,
    scroll: usize,
    expand: bool,
    st: Status,
    thinking: String,
    theme: Theme,
    panels: Panels,
    spin: usize,
    quit: bool,
    modal: Option<Modal>,
    popup: Option<Popup>,
    popup_closed_for: Option<String>,
    files: Arc<Mutex<Vec<String>>>,
    last_ctrl_c: Option<Instant>,
    last_esc: Option<Instant>,
    want_editor: bool,
    cwd: PathBuf,
    compacting: bool,
    has_agent: bool,
}

pub fn run(agent: Option<Agent>, initial: Option<String>, pick_resume: bool) -> Result<i32> {
    let settings = Settings::load();
    let (req_tx, req_rx) = channel::<Req>();
    let (up_tx, up_rx) = channel::<Up>();
    let steer = agent
        .as_ref()
        .map(|a| a.steer.clone())
        .unwrap_or_else(|| Arc::new(Mutex::new(vec![])));
    let cwd = std::env::current_dir().unwrap_or_default();
    let mut ui = Ui {
        items: vec![],
        open_text: false,
        tools: HashMap::new(),
        input: vec![],
        cursor: 0,
        history: vec![],
        hist_pos: None,
        follow_up: vec![],
        steer: steer.clone(),
        busy: false,
        running: 0,
        scroll: 0,
        expand: false,
        st: agent.as_ref().map(status_of).unwrap_or_default(),
        thinking: settings.thinking.clone(),
        theme: theme::load(&settings.theme),
        panels: Panels::default(),
        spin: 0,
        quit: false,
        modal: None,
        popup: None,
        popup_closed_for: None,
        files: Arc::new(Mutex::new(vec![])),
        last_ctrl_c: None,
        last_esc: None,
        want_editor: false,
        cwd: cwd.clone(),
        compacting: false,
        has_agent: agent.is_some(),
    };
    if let Some(a) = &agent {
        ui.items = history_items(&a.session);
    }
    ui.reload_files();
    crate::prices::refresh_if_stale();
    let worker_up = up_tx.clone();
    std::thread::spawn(move || worker(agent, steer, req_rx, worker_up));

    let mut term = ratatui::init();
    let enhanced = matches!(
        ratatui::crossterm::terminal::supports_keyboard_enhancement(),
        Ok(true)
    );
    let set_modes = |on: bool| {
        if on {
            let _ = execute!(std::io::stdout(), EnableBracketedPaste);
            if enhanced {
                let _ = execute!(
                    std::io::stdout(),
                    PushKeyboardEnhancementFlags(
                        KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    )
                );
            }
        } else {
            if enhanced {
                let _ = execute!(std::io::stdout(), PopKeyboardEnhancementFlags);
            }
            let _ = execute!(std::io::stdout(), DisableBracketedPaste);
        }
    };
    set_modes(true);

    if !ui.has_agent {
        ui.modal = Some(Modal::Login(login::Flow::new(true)));
    } else {
        if !settings.tips_shown {
            ui.items.push(Item::Note(TIPS.into(), false));
            let mut s = Settings::load();
            s.tips_shown = true;
            let _ = s.save();
        }
        if pick_resume {
            ui.open_resume();
        }
        ui.panels.refresh(&ui.env());
        if let Some(p) = initial {
            ui.submit(p, &req_tx, &up_tx);
        }
    }

    let res = (|| -> Result<()> {
        while !ui.quit {
            while let Ok(u) = up_rx.try_recv() {
                ui.on_up(u, &req_tx);
            }
            if let Some(Modal::Login(f)) = &mut ui.modal {
                f.poll();
            }
            if ui.has_agent
                && ui
                    .panels
                    .last
                    .is_none_or(|t| t.elapsed() > Duration::from_secs(5))
            {
                ui.panels.refresh(&ui.env());
            }
            ui.spin = ui.spin.wrapping_add(1);
            term.draw(|f| ui.draw(f))?;
            if let Some(Modal::Login(fl)) = &ui.modal {
                fl.paint_url();
            }
            if ui.want_editor {
                ui.want_editor = false;
                set_modes(false);
                ratatui::restore();
                let r = edit_externally(&ui.input.iter().collect::<String>());
                term = ratatui::init();
                set_modes(true);
                match r {
                    Ok(t) => {
                        ui.input = t.trim_end_matches('\n').chars().collect();
                        ui.cursor = ui.input.len();
                    }
                    Err(e) => ui.items.push(Item::Note(format!("editor: {e:#}"), true)),
                }
                continue;
            }
            if event::poll(Duration::from_millis(50))? {
                match event::read()? {
                    Event::Key(k) if k.kind != KeyEventKind::Release => {
                        ui.on_key(k, &req_tx, &up_tx)
                    }
                    Event::Paste(s) => ui.on_paste(&s),
                    _ => {}
                }
            }
        }
        Ok(())
    })();
    set_modes(false);
    ratatui::restore();
    if ui.busy {
        exec::interrupt();
    }
    let _ = std::fs::remove_dir_all(
        std::env::temp_dir().join(format!("tau-ui-{}", std::process::id())),
    );
    if !ui.st.session.is_empty() {
        println!(
            "tau: {} · ${:.4} · `tau -c` or `tau --resume {}` to continue",
            ui.st.session, ui.st.usage.cost, ui.st.session
        );
    }
    res.map(|_| 0)
}

const TIPS: &str = "tips: type while tau works to steer it (alt+enter waits until it is done) · esc interrupts · @ inlines a file · !cmd runs a command into the context · / lists commands · ctrl+l picks a model · shift+tab changes thinking";

fn status_of(a: &Agent) -> Status {
    Status {
        session: a.session.id.clone(),
        provider: a.provider.name().to_string(),
        model: a.provider.model().to_string(),
        usage: a.session.own_usage(),
        ctx: a.context_tokens(),
        window: a.context_window(),
        depth: a.depth.depth,
    }
}

fn edit_externally(text: &str) -> Result<String> {
    let path = std::env::temp_dir().join(format!("tau-edit-{}.md", std::process::id()));
    std::fs::write(&path, text)?;
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "nano".into());
    let st = Command::new("sh")
        .arg("-c")
        .arg(format!("{editor} \"$1\""))
        .arg("tau-editor")
        .arg(&path)
        .status()?;
    let out = std::fs::read_to_string(&path)?;
    let _ = std::fs::remove_file(&path);
    if !st.success() {
        anyhow::bail!("{editor} exited with {st}");
    }
    Ok(out)
}

/// Saves an image from the clipboard, if there is one and a tool to read it.
fn clipboard_image() -> Result<PathBuf> {
    let tries: [(&str, &[&str]); 3] = [
        ("wl-paste", &["--no-newline", "--type", "image/png"]),
        (
            "xclip",
            &["-selection", "clipboard", "-t", "image/png", "-o"],
        ),
        ("pngpaste", &["-"]),
    ];
    for (cmd, args) in tries {
        if let Ok(o) = Command::new(cmd).args(args).stderr(Stdio::null()).output()
            && o.status.success()
            && o.stdout.starts_with(&[0x89, b'P', b'N', b'G'])
        {
            let dir = tau_home().join("clipboard");
            std::fs::create_dir_all(&dir)?;
            let p = dir.join(format!("paste-{}.png", crate::log::now()));
            std::fs::write(&p, &o.stdout)?;
            return Ok(p);
        }
    }
    anyhow::bail!("no image on the clipboard (reading it needs wl-paste, xclip or pngpaste)")
}

fn copy_to_clipboard(text: &str) -> Result<()> {
    login::copy(text)
}

fn worker(
    mut agent: Option<Agent>,
    steer: Arc<Mutex<Vec<String>>>,
    rx: Receiver<Req>,
    up: Sender<Up>,
) {
    let send = |u: Up| {
        let _ = up.send(u);
    };
    let reloaded = |a: &Agent| {
        let _ = up.send(Up::Reloaded(status_of(a)));
    };
    for r in rx {
        exec::CANCEL.store(false, Ordering::SeqCst);
        // everything but a login needs an agent
        if agent.is_none() && !matches!(r, Req::SetProvider(..)) {
            send(Up::Note("log in first: /login".into(), true));
            send(Up::Done(Ok(String::new())));
            continue;
        }
        match r {
            Req::Prompt { text, images } => {
                let a = agent.as_mut().unwrap();
                let mut stored = vec![];
                for p in images {
                    match a.session.store_image(&p) {
                        Ok(s) => stored.push(s),
                        Err(e) => send(Up::Note(format!("image {}: {e}", p.display()), true)),
                    }
                }
                let res = a.run_with_images(&text, stored, &mut |ev| {
                    send(match ev {
                        Ev::Text(t) => Up::Text(t.to_string()),
                        Ev::Turn { usage } => Up::Turn(usage.clone()),
                        Ev::ToolStart { idx, command } => Up::ToolStart {
                            idx,
                            command: command.to_string(),
                        },
                        Ev::ToolEnd {
                            idx,
                            output,
                            code,
                            lines,
                        } => Up::ToolEnd {
                            idx,
                            output: output.to_string(),
                            code,
                            lines,
                        },
                        Ev::Steered(t) => Up::Steered(t.to_string()),
                        Ev::Compacting => Up::Compacting,
                        Ev::Compacted(s) => Up::Compacted(s.to_string()),
                        Ev::Appended => Up::Appended,
                    })
                });
                a.session.set_status("ok");
                send(Up::Ctx(a.context_tokens(), a.context_window()));
                send(Up::Done(res.map_err(|e| format!("{e:#}"))));
            }
            Req::Compact(focus) => {
                let a = agent.as_mut().unwrap();
                let res = a.compact(focus.as_deref(), false, &mut |ev| {
                    if let Ev::Compacted(s) = ev {
                        send(Up::Compacted(s.to_string()));
                    }
                });
                if let Err(e) = res {
                    send(Up::Note(format!("compact failed: {e:#}"), true));
                }
                send(Up::Ctx(a.context_tokens(), a.context_window()));
                send(Up::Done(Ok(String::new())));
            }
            Req::SetProvider(spec, model) => {
                let p = match provider::build(&spec, &model) {
                    Ok(mut p) => {
                        p.set_thinking(&Settings::load().thinking);
                        p
                    }
                    Err(e) => {
                        send(Up::Note(format!("{e:#}"), true));
                        send(Up::Done(Ok(String::new())));
                        continue;
                    }
                };
                match agent.as_mut() {
                    Some(a) => a.provider = p,
                    None => match crate::agent::new_session(None, 0) {
                        Ok(s) => {
                            let mut a = Agent::new(s, p);
                            a.steer = steer.clone();
                            agent = Some(a);
                        }
                        Err(e) => send(Up::Note(format!("{e:#}"), true)),
                    },
                }
                if let Some(a) = &agent {
                    reloaded(a);
                }
                send(Up::Done(Ok(String::new())));
            }
            Req::SetModel(m) => {
                let a = agent.as_mut().unwrap();
                let spec = Spec::for_provider(a.provider.name(), &crate::config::load_auth());
                match spec.map(|s| provider::build(&s, &m)) {
                    Some(Ok(mut p)) => {
                        p.set_thinking(&Settings::load().thinking);
                        a.provider = p;
                        reloaded(a);
                    }
                    Some(Err(e)) => send(Up::Note(format!("{e:#}"), true)),
                    None => send(Up::Note(
                        "the current provider has no key any more".into(),
                        true,
                    )),
                }
            }
            Req::SetThinking(l) => agent.as_mut().unwrap().provider.set_thinking(&l),
            Req::SetHead(n) => {
                let a = agent.as_mut().unwrap();
                match a.session.set_head(n) {
                    Ok(()) => reloaded(a),
                    Err(e) => send(Up::Note(format!("{e:#}"), true)),
                }
            }
            Req::Open(id) => match Session::open(&id) {
                Ok(s) => {
                    let a = agent.as_mut().unwrap();
                    a.session = s;
                    reloaded(a);
                }
                Err(e) => send(Up::Note(format!("{e:#}"), true)),
            },
            Req::New => {
                let a = agent.as_mut().unwrap();
                match crate::agent::new_session(None, a.depth.depth) {
                    Ok(s) => {
                        a.session = s;
                        reloaded(a);
                    }
                    Err(e) => send(Up::Note(format!("{e:#}"), true)),
                }
            }
            Req::Fork(spec) => {
                let a = agent.as_mut().unwrap();
                match crate::agent::fork(&spec, a.depth.depth) {
                    Ok(s) => {
                        a.session = s;
                        reloaded(a);
                    }
                    Err(e) => send(Up::Note(format!("{e:#}"), true)),
                }
            }
            Req::Shell {
                cmd,
                send: to_model,
            } => {
                let a = agent.as_mut().unwrap();
                let ex = Exec::new(a.session.dir.clone(), a.env(None));
                match ex.run(&cmd, exec::DEFAULT_TIMEOUT) {
                    Ok(o) => {
                        if to_model {
                            let _ = a.session.append(Body::User {
                                text: format!("I ran `{cmd}` myself. Its output:\n{}", o.text),
                                images: vec![],
                            });
                            send(Up::Appended);
                        }
                        send(Up::Shell {
                            cmd,
                            output: o.text,
                            code: o.code,
                            lines: o.lines,
                        });
                    }
                    Err(e) => send(Up::Note(format!("{e:#}"), true)),
                }
                send(Up::Done(Ok(String::new())));
            }
        }
    }
}

fn history_items(s: &Session) -> Vec<Item> {
    let mut items = vec![];
    let chain = s.chain(s.head);
    let mut pending: Vec<(String, String)> = vec![];
    for e in chain {
        match &e.body {
            Body::User { text, images } => {
                let mut t = strip_inlined(text);
                if !images.is_empty() {
                    t.push_str(&format!("  [{} image(s)]", images.len()));
                }
                items.push(Item::User(t))
            }
            Body::Summary { text, .. } => {
                items.push(Item::Note(format!("compacted here:\n{text}"), false))
            }
            Body::Assistant { text, calls, .. } => {
                if !text.is_empty() {
                    items.push(Item::Assistant(text.clone()));
                }
                pending = calls
                    .iter()
                    .map(|c| (c.id.clone(), c.command.clone()))
                    .collect();
            }
            Body::Tool { call_id, output } => {
                let command = pending
                    .iter()
                    .find(|(id, _)| id == call_id)
                    .map(|(_, c)| c.clone())
                    .unwrap_or_default();
                items.push(Item::Tool {
                    command,
                    lines: output.lines().count(),
                    output: Some(output.clone()),
                    code: None,
                });
            }
            Body::System { .. } => {}
        }
    }
    if !items.is_empty() {
        items.push(Item::Note(format!("session {}", s.id), false));
    }
    items
}

/// Shows `@file` references without the file bodies that were inlined.
fn strip_inlined(text: &str) -> String {
    match text
        .find("\n\n<file path=\"")
        .or(text.find("\n\n<dir path=\""))
    {
        Some(i) => text[..i].to_string(),
        None => text.to_string(),
    }
}

fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(8);
    let mut out = vec![];
    for line in text.split('\n') {
        let mut cur = String::new();
        let mut w = 0;
        for ch in line.chars() {
            let ch = if ch == '\t' { ' ' } else { ch };
            let cw = ch.width().unwrap_or(0);
            if w + cw > width {
                out.push(std::mem::take(&mut cur));
                w = 0;
            }
            cur.push(ch);
            w += cw;
        }
        out.push(cur);
    }
    out
}

const SPIN: [&str; 4] = ["◐", "◓", "◑", "◒"];

fn human(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1e6)
    } else if n >= 1000 {
        format!("{:.0}k", n as f64 / 1e3)
    } else {
        n.to_string()
    }
}

fn ago(ts: u64) -> String {
    let d = crate::log::now().saturating_sub(ts);
    match d {
        0..60 => "just now".into(),
        60..3600 => format!("{}m ago", d / 60),
        3600..86400 => format!("{}h ago", d / 3600),
        _ => format!("{}d ago", d / 86400),
    }
}

/// `▰▰▱▱▱▱▱▱ 23%`
fn meter(used: u64, window: u64) -> String {
    let pct = (used * 100).checked_div(window).unwrap_or(0).min(100);
    let full = (pct as usize * 8).div_ceil(100).min(8);
    format!("{}{} {pct}%", "▰".repeat(full), "▱".repeat(8 - full))
}

impl Ui {
    fn env(&self) -> Vec<(String, String)> {
        let sessions = crate::log::sessions_dir().join(&self.st.session);
        vec![
            ("TAU_SESSION".into(), self.st.session.clone()),
            ("TAU_NODE".into(), self.st.session.clone()),
            (
                "TAU_EVENTS".into(),
                sessions.join("events.jsonl").display().to_string(),
            ),
            ("COLUMNS".into(), "40".into()),
        ]
    }

    fn note(&mut self, s: impl Into<String>, err: bool) {
        self.items.push(Item::Note(s.into(), err));
        self.scroll = 0;
    }

    fn reload_files(&self) {
        let (files, cwd) = (self.files.clone(), self.cwd.clone());
        std::thread::spawn(move || {
            let l = crate::files::list(&cwd);
            *files.lock().unwrap() = l;
        });
    }

    fn on_up(&mut self, u: Up, req: &Sender<Req>) {
        match u {
            Up::Text(t) => {
                if t.is_empty() {
                    return;
                }
                if self.open_text
                    && let Some(Item::Assistant(s)) = self.items.last_mut()
                {
                    s.push_str(&t);
                } else {
                    self.items.push(Item::Assistant(t));
                    self.open_text = true;
                }
            }
            Up::Turn(usage) => {
                self.open_text = false;
                self.tools.clear();
                self.st.ctx = usage.input + usage.cache_read + usage.cache_write + usage.output;
                self.st.usage.add(&usage);
            }
            Up::ToolStart { idx, command } => {
                self.open_text = false;
                self.running += 1;
                self.tools.insert(idx, self.items.len());
                self.items.push(Item::Tool {
                    command,
                    output: None,
                    code: None,
                    lines: 0,
                });
            }
            Up::ToolEnd {
                idx,
                output,
                code,
                lines,
            } => {
                self.running = self.running.saturating_sub(1);
                if let Some(&i) = self.tools.get(&idx)
                    && let Some(Item::Tool {
                        output: o,
                        code: c,
                        lines: l,
                        ..
                    }) = self.items.get_mut(i)
                {
                    *o = Some(output);
                    *c = code;
                    *l = lines;
                }
            }
            Up::Steered(t) => {
                self.open_text = false;
                self.items.push(Item::User(t));
            }
            Up::Compacting => {
                self.compacting = true;
                self.note("compacting the history…", false);
            }
            Up::Compacted(s) => {
                self.compacting = false;
                self.note(format!("compacted. The model continues from:\n{s}"), false);
            }
            Up::Appended => self.panels.refresh(&self.env()),
            Up::Note(s, err) => self.note(s, err),
            Up::Command {
                stdout,
                stderr,
                label,
            } => {
                if !stderr.trim().is_empty() {
                    self.note(stderr.trim_end().to_string(), false);
                }
                if stdout.trim().is_empty() {
                    self.busy = false;
                    self.next(req);
                } else {
                    self.note(label, false);
                    self.items.push(Item::User(stdout.trim_end().to_string()));
                    let _ = req.send(Req::Prompt {
                        text: stdout,
                        images: vec![],
                    });
                }
            }
            Up::Done(r) => {
                self.busy = false;
                self.running = 0;
                self.compacting = false;
                self.open_text = false;
                if let Err(e) = r {
                    self.note(e, true);
                }
                // messages that missed the last turn go out now
                let left: Vec<String> = std::mem::take(&mut *self.steer.lock().unwrap());
                self.follow_up.splice(0..0, left);
                self.next(req);
            }
            Up::Reloaded(st) => {
                let changed = st.session != self.st.session;
                let first = self.st.session.is_empty();
                self.st = st;
                self.has_agent = true;
                if let Ok(s) = Session::open(&self.st.session) {
                    let notes = std::mem::replace(&mut self.items, history_items(&s));
                    if first {
                        // keep what login said
                        self.items.extend(notes);
                    }
                }
                if changed && self.items.is_empty() {
                    self.note(format!("new session {}", self.st.session), false);
                }
                self.scroll = 0;
            }
            Up::Models(list) => {
                let items = self.model_items(&list);
                if let Some(Modal::Pick(p, Purpose::Model)) = &mut self.modal {
                    let cur = p.current().map(|i| i.value.clone());
                    p.items = items;
                    if let Some(c) = cur
                        && let Some(i) = p.visible().iter().position(|&i| p.items[i].value == c)
                    {
                        p.sel = i;
                    }
                }
            }
            Up::Ctx(t, w) => {
                self.st.ctx = t;
                self.st.window = w;
            }
            Up::Shell {
                cmd,
                output,
                code,
                lines,
            } => {
                self.items.push(Item::Tool {
                    command: cmd,
                    output: Some(output),
                    code,
                    lines,
                });
                self.scroll = 0;
            }
        }
    }

    fn next(&mut self, req: &Sender<Req>) {
        if !self.busy && !self.follow_up.is_empty() {
            let p = self.follow_up.remove(0);
            self.send_prompt(p, req);
        }
    }

    fn send_prompt(&mut self, text: String, req: &Sender<Req>) {
        let (full, images) = crate::files::inline(&text, &self.cwd);
        let mut shown = text.clone();
        if !images.is_empty() {
            shown.push_str(&format!("  [{} image(s)]", images.len()));
        }
        self.items.push(Item::User(shown));
        self.busy = true;
        self.scroll = 0;
        let _ = req.send(Req::Prompt { text: full, images });
    }

    fn model_items(&self, list: &[ModelInfo]) -> Vec<PItem> {
        let favs = Settings::load().favorites;
        list.iter()
            .map(|m| {
                let fav = favs.contains(&format!("{}/{}", self.st.provider, m.id));
                let price = crate::prices::label(&m.id).unwrap_or_default();
                let ctx = m
                    .context
                    .or(crate::prices::lookup(&m.id).and_then(|p| p.context))
                    .map(|c| format!("{} ctx  ", human(c)))
                    .unwrap_or_default();
                let cur = if m.id == self.st.model { "● " } else { "" };
                PItem::new(
                    format!("{cur}{}{}", if fav { "★ " } else { "" }, m.id),
                    format!("{ctx}{price}"),
                    m.id.clone(),
                )
            })
            .collect()
    }

    fn open_models(&mut self, up: &Sender<Up>) {
        let auth = crate::config::load_auth();
        let Some(spec) = Spec::for_provider(&self.st.provider, &auth) else {
            self.note("no provider yet: /login", true);
            return;
        };
        let mut list = provider::cached_models(&spec);
        if list.is_empty() {
            list.push(ModelInfo {
                id: self.st.model.clone(),
                context: None,
            });
        }
        let mut p = Picker::new(
            format!("{} models", self.st.provider),
            self.model_items(&list),
        )
        .select_value(&self.st.model);
        p.hint = "enter to switch · ctrl+f favorite (ctrl+p cycles them) · esc".into();
        self.modal = Some(Modal::Pick(p, Purpose::Model));
        let up = up.clone();
        std::thread::spawn(move || match provider::list_models(&spec) {
            Ok(l) => {
                let _ = up.send(Up::Models(l));
            }
            Err(e) => {
                let _ = up.send(Up::Note(
                    format!("could not refresh the model list: {e:#}"),
                    true,
                ));
            }
        });
    }

    fn open_resume(&mut self) {
        let mut metas = list_metas();
        let cwd = self.cwd.display().to_string();
        metas.sort_by_key(|m| (m.cwd != cwd, std::cmp::Reverse(m.created)));
        let items = metas
            .iter()
            .filter(|m| !m.task.is_empty() || !m.name.is_empty())
            .map(|m| {
                let title = if m.name.is_empty() { &m.task } else { &m.name };
                let here = if m.cwd == cwd {
                    String::new()
                } else {
                    format!("  {}", m.cwd)
                };
                PItem::new(
                    title.clone(),
                    format!("{}{here}", ago(m.created)),
                    m.id.clone(),
                )
            })
            .collect();
        self.modal = Some(Modal::Pick(
            Picker::new("resume a session", items),
            Purpose::Resume,
        ));
    }

    /// Every node of the session as an indented tree, marking the current branch.
    fn open_tree(&mut self, purpose: Purpose) {
        let Ok(s) = Session::open(&self.st.session) else {
            return;
        };
        let on_path: HashSet<u64> = s.chain(s.head).iter().map(|e| e.id).collect();
        let mut kids: HashMap<Option<u64>, Vec<u64>> = HashMap::new();
        for e in &s.events {
            kids.entry(e.parent).or_default().push(e.id);
        }
        let mut items = vec![];
        let mut stack: Vec<(u64, usize)> = kids
            .get(&None)
            .map(|v| v.iter().rev().map(|&id| (id, 0)).collect())
            .unwrap_or_default();
        while let Some((id, depth)) = stack.pop() {
            let e = s.get(id).unwrap();
            let children = kids.get(&Some(id)).cloned().unwrap_or_default();
            let (show, label) = match &e.body {
                Body::User { text, .. } => {
                    (true, format!("you: {}", first_line(&strip_inlined(text))))
                }
                Body::Assistant { text, calls, .. } if calls.is_empty() => (
                    purpose == Purpose::Tree,
                    format!("tau: {}", first_line(text)),
                ),
                Body::Summary { .. } => (purpose == Purpose::Tree, "(compacted)".to_string()),
                _ => (false, String::new()),
            };
            let branches = children.len() > 1;
            let next_depth = if show && branches { depth + 1 } else { depth };
            if show && (purpose == Purpose::Tree || on_path.contains(&id)) {
                let mark = if Some(id) == s.head {
                    "◀ here"
                } else if on_path.contains(&id) {
                    "●"
                } else {
                    ""
                };
                items.push(PItem::new(
                    format!("{}{label}", "  ".repeat(depth)),
                    mark,
                    id.to_string(),
                ));
            }
            for &c in children.iter().rev() {
                stack.push((c, if branches { next_depth } else { depth }));
            }
        }
        let (title, hint) = match purpose {
            Purpose::Fork => (
                "fork from a message",
                "the new session starts just before it, with the message in the editor",
            ),
            _ => (
                "session tree",
                "enter on your message: branch from before it · on an answer: continue from there",
            ),
        };
        let mut p = Picker::new(title, items);
        p.literal = true;
        p.hint = hint.into();
        if let Some(h) = s.head {
            // highlight the newest selectable node on the current path
            let last = p.items.iter().rposition(|i| {
                i.value
                    .parse::<u64>()
                    .is_ok_and(|n| n <= h && on_path.contains(&n))
            });
            p.sel = last.unwrap_or(0);
        }
        self.modal = Some(Modal::Pick(p, purpose));
    }

    fn settings_items(&self) -> Vec<PItem> {
        let s = Settings::load();
        vec![
            PItem::new(
                format!("theme: {}", s.theme),
                "enter to cycle; ~/.tau/themes/*.json",
                "theme",
            ),
            PItem::new(
                format!("thinking: {}", s.thinking),
                "enter to cycle (shift+tab)",
                "thinking",
            ),
            PItem::new(
                format!(
                    "auto-compact: {}",
                    if s.auto_compact { "on" } else { "off" }
                ),
                format!(
                    "summarize when within {} tokens of the window",
                    s.compact_reserve
                ),
                "auto_compact",
            ),
            PItem::new(
                format!("keep after compaction: {} tokens", s.compact_keep),
                "enter to cycle 10k/20k/40k",
                "compact_keep",
            ),
        ]
    }

    fn change_setting(&mut self, key: &str, req: &Sender<Req>) {
        let mut s = Settings::load();
        match key {
            "theme" => {
                let names = theme::names();
                let i = names
                    .iter()
                    .position(|n| *n == s.theme)
                    .map(|i| i + 1)
                    .unwrap_or(0);
                s.theme = names[i % names.len()].clone();
                self.theme = theme::load(&s.theme);
            }
            "thinking" => {
                s.thinking = next_thinking(&s.thinking);
                self.thinking = s.thinking.clone();
                let _ = req.send(Req::SetThinking(s.thinking.clone()));
            }
            "auto_compact" => s.auto_compact = !s.auto_compact,
            "compact_keep" => {
                s.compact_keep = match s.compact_keep {
                    0..=10_000 => 20_000,
                    10_001..=20_000 => 40_000,
                    _ => 10_000,
                }
            }
            _ => {}
        }
        if let Err(e) = s.save() {
            self.note(format!("could not save settings: {e:#}"), true);
        }
        if key == "auto_compact" || key == "compact_keep" {
            self.note("compaction settings apply from the next start", false);
        }
    }

    fn submit(&mut self, text: String, req: &Sender<Req>, up: &Sender<Up>) {
        let t = text.trim().to_string();
        if t.is_empty() {
            return;
        }
        self.history.push(t.clone());
        self.hist_pos = None;
        self.scroll = 0;
        if !self.has_agent && !t.starts_with("/login") && !t.starts_with("/quit") {
            self.modal = Some(Modal::Login(login::Flow::new(false)));
            return;
        }
        if let Some(cmd) = t.strip_prefix("!") {
            let (cmd, send) = match cmd.strip_prefix('!') {
                Some(c) => (c.trim(), false),
                None => (cmd.trim(), true),
            };
            if self.busy {
                self.note("busy; run shell commands between turns", true);
                return;
            }
            self.busy = true;
            let _ = req.send(Req::Shell {
                cmd: cmd.to_string(),
                send,
            });
            return;
        }
        if let Some(cmd) = t.strip_prefix('/') {
            self.command(cmd, req, up);
            return;
        }
        if self.busy {
            // delivered before the model's next turn
            self.steer.lock().unwrap().push(t);
            return;
        }
        self.send_prompt(t, req);
    }

    fn command(&mut self, cmd: &str, req: &Sender<Req>, up: &Sender<Up>) {
        let (name, args) = cmd.split_once(char::is_whitespace).unwrap_or((cmd, ""));
        let args = args.trim();
        let idle = !self.busy;
        match name {
            "quit" | "exit" | "q" => self.quit = true,
            "help" | "hotkeys" => self.modal = Some(Modal::Help),
            "login" => self.modal = Some(Modal::Login(login::Flow::new(false))),
            "logout" => {
                let items: Vec<PItem> = crate::config::load_auth()
                    .keys()
                    .map(|k| PItem::new(k.clone(), "forget this key", k.clone()))
                    .collect();
                if items.is_empty() {
                    self.note(
                        "no saved keys (keys from environment variables are not saved)",
                        false,
                    );
                } else {
                    self.modal = Some(Modal::Pick(
                        Picker::new("log out of", items),
                        Purpose::Logout,
                    ));
                }
            }
            "model" => self.open_models(up),
            "thinking" => self.change_setting("thinking", req),
            "settings" => {
                let mut p = Picker::new("settings", self.settings_items());
                p.literal = true;
                p.hint = "enter to change · esc to close · ~/.tau/settings.json".into();
                self.modal = Some(Modal::Pick(p, Purpose::Settings));
            }
            "resume" if idle => self.open_resume(),
            "tree" if idle => self.open_tree(Purpose::Tree),
            "fork" if idle => self.open_tree(Purpose::Fork),
            "clone" if idle => {
                let _ = req.send(Req::Fork(self.st.session.clone()));
            }
            "new" if idle => {
                let _ = req.send(Req::New);
            }
            "compact" if idle => {
                self.busy = true;
                let _ = req.send(Req::Compact((!args.is_empty()).then(|| args.to_string())));
            }
            "resume" | "tree" | "fork" | "clone" | "new" | "compact" => {
                self.note("busy; esc to interrupt first", true)
            }
            "export" => match Session::open(&self.st.session) {
                Ok(s) => match crate::export::write(&s, (!args.is_empty()).then_some(args)) {
                    Ok(p) => self.note(format!("wrote {}", p.display()), false),
                    Err(e) => self.note(format!("export: {e:#}"), true),
                },
                Err(e) => self.note(format!("{e:#}"), true),
            },
            "session" => {
                let dir = crate::log::sessions_dir().join(&self.st.session);
                self.note(
                    format!(
                        "session {}\nlog {}\nmodel {} via {}\ncontext {} of {}\nspent ${:.4}, {} in, {} out, cache {}%",
                        self.st.session,
                        dir.join("events.jsonl").display(),
                        self.st.model,
                        self.st.provider,
                        human(self.st.ctx),
                        human(self.st.window),
                        self.st.usage.cost,
                        human(self.st.usage.input + self.st.usage.cache_read),
                        human(self.st.usage.output),
                        self.st.usage.cache_pct()
                    ),
                    false,
                );
            }
            "name" => {
                if let Ok(mut s) = Session::open(&self.st.session) {
                    s.meta.name = args.to_string();
                    let _ = s.save_meta();
                    self.note(format!("named: {args}"), false);
                }
            }
            "copy" => {
                let last = self.items.iter().rev().find_map(|i| match i {
                    Item::Assistant(t) => Some(t.clone()),
                    _ => None,
                });
                match last.map(|t| copy_to_clipboard(&t)) {
                    Some(Ok(())) => self.note("copied", false),
                    Some(Err(e)) => self.note(format!("{e:#}"), true),
                    None => self.note("nothing to copy yet", false),
                }
            }
            "reload" => {
                self.reload_files();
                self.note("reloading the file list", false);
            }
            _ => self.user_command(name, args, req, up),
        }
    }

    /// Templates, skills and executables, in that order.
    fn user_command(&mut self, name: &str, args: &str, req: &Sender<Req>, up: &Sender<Up>) {
        if self.busy {
            self.note("busy; esc to interrupt first", true);
            return;
        }
        if let Some(t) = crate::skills::templates()
            .into_iter()
            .find(|t| t.name == name)
        {
            let text = crate::skills::expand(&t.body, &crate::skills::split_args(args));
            self.send_prompt(text.trim().to_string(), req);
            return;
        }
        if let Some(skill) = name.strip_prefix("skill:") {
            let skills = crate::skills::discover(&Settings::load().skill_dirs);
            match skills.iter().find(|s| s.name == skill) {
                Some(s) => {
                    let body = std::fs::read_to_string(&s.path).unwrap_or_default();
                    let mut text = format!(
                        "Use the skill {} ({}):\n\n{}",
                        s.name,
                        s.path.display(),
                        body.trim()
                    );
                    if !args.is_empty() {
                        text.push_str(&format!("\n\nUser: {args}"));
                    }
                    self.send_prompt(text, req);
                }
                None => self.note(format!("no skill {skill} in ~/.tau/skills"), true),
            }
            return;
        }
        let path = tau_home().join("commands").join(name);
        if !path.is_file() {
            self.note(
                format!(
                    "no command /{name}. Put an executable at {} or a template at {}",
                    path.display(),
                    tau_home()
                        .join("prompts")
                        .join(format!("{name}.md"))
                        .display()
                ),
                true,
            );
            return;
        }
        self.busy = true;
        let (env, args, up, label) = (
            self.env(),
            args.to_string(),
            up.clone(),
            format!("/{name} {args}"),
        );
        std::thread::spawn(move || {
            let argv: Vec<&str> = args.split_whitespace().collect();
            let (stdout, stderr, _) = run_with_timeout(&path, &argv, &env, 120);
            let _ = up.send(Up::Command {
                stdout,
                stderr,
                label: label.trim().to_string(),
            });
        });
    }

    fn command_list(&self) -> Vec<(String, String)> {
        let mut v: Vec<(String, String)> = BUILTINS
            .iter()
            .map(|(n, d)| (format!("/{n}"), d.to_string()))
            .collect();
        for t in crate::skills::templates() {
            v.push((format!("/{}", t.name), t.description));
        }
        for (n, _) in executables(&tau_home().join("commands")) {
            v.push((format!("/{n}"), "~/.tau/commands".into()));
        }
        for s in crate::skills::discover(&Settings::load().skill_dirs) {
            v.push((format!("/skill:{}", s.name), s.description));
        }
        v
    }

    fn update_popup(&mut self) {
        let before: String = self.input[..self.cursor].iter().collect();
        if self.popup_closed_for.as_deref() == Some(before.as_str()) {
            self.popup = None;
            return;
        }
        self.popup_closed_for = None;
        let sel = self.popup.as_ref().map(|p| p.sel).unwrap_or(0);
        if let Some(q) = crate::files::at_query(&before) {
            let files = self.files.lock().unwrap();
            let items: Vec<(String, String)> = crate::files::best(&files, q, 8)
                .into_iter()
                .map(|f| (f, String::new()))
                .collect();
            self.popup = (!items.is_empty()).then(|| Popup {
                kind: PopKind::File,
                sel: sel.min(items.len() - 1),
                items,
            });
            return;
        }
        if before.starts_with('/')
            && !before.contains(char::is_whitespace)
            && self.cursor == self.input.len()
        {
            let q = &before[1..];
            let items: Vec<(String, String)> = self
                .command_list()
                .into_iter()
                .filter(|(n, _)| n[1..].starts_with(q))
                .take(10)
                .collect();
            self.popup =
                (!(items.is_empty() || items.len() == 1 && items[0].0 == before)).then(|| Popup {
                    kind: PopKind::Command,
                    sel: sel.min(items.len().saturating_sub(1)),
                    items,
                });
            return;
        }
        self.popup = None;
    }

    fn accept_popup(&mut self) {
        let Some(p) = self.popup.take() else { return };
        let Some((choice, _)) = p.items.get(p.sel).cloned() else {
            return;
        };
        let before: String = self.input[..self.cursor].iter().collect();
        let start = match p.kind {
            PopKind::File => before.rfind('@').map(|i| i + 1).unwrap_or(0),
            PopKind::Command => 0,
        };
        let start_chars = before[..start].chars().count();
        self.input.drain(start_chars..self.cursor);
        self.cursor = start_chars;
        self.insert(&choice);
        self.insert(" ");
        self.popup_closed_for = Some(self.input[..self.cursor].iter().collect());
    }

    fn insert(&mut self, s: &str) {
        for c in s.chars() {
            self.input.insert(self.cursor, c);
            self.cursor += 1;
        }
    }

    fn on_paste(&mut self, s: &str) {
        let s = s.replace("\r\n", "\n").replace('\r', "\n");
        match &mut self.modal {
            Some(Modal::Login(f)) => return f.on_paste(&s),
            Some(_) => return,
            None => {}
        }
        // a dragged-in image arrives as its path
        let t = s.trim().trim_matches('\'').trim_matches('"');
        if !t.contains('\n') && crate::log::media_type(t).is_some() && Path::new(t).is_file() {
            self.insert(&format!("@{t} "));
        } else {
            self.insert(&s);
        }
        self.update_popup();
    }

    fn on_modal_key(&mut self, k: KeyEvent, req: &Sender<Req>) {
        let Some(modal) = self.modal.as_mut() else {
            return;
        };
        match modal {
            Modal::Help => self.modal = None,
            Modal::Login(f) => match f.on_key(k) {
                login::Out::Done(spec, model) => {
                    self.modal = None;
                    self.st.provider = spec.name.clone();
                    self.st.model = model.clone();
                    let first = !self.has_agent;
                    let _ = req.send(Req::SetProvider(spec, model.clone()));
                    self.note(format!("using {model}"), false);
                    if first {
                        self.note(TIPS, false);
                        let mut s = Settings::load();
                        s.tips_shown = true;
                        let _ = s.save();
                    }
                }
                login::Out::Cancelled => {
                    self.modal = None;
                    if !self.has_agent {
                        self.note("no provider yet. /login when you are ready, or set an API key variable", false);
                    }
                }
                login::Out::Working => {}
            },
            Modal::Pick(p, purpose) => {
                let purpose = *purpose;
                match p.on_key(k) {
                    Pick::Cancel => self.modal = None,
                    Pick::Chosen(v) => self.picked(purpose, v, req),
                    Pick::Other(k, cur) => {
                        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
                        if purpose == Purpose::Model
                            && ctrl
                            && k.code == KeyCode::Char('f')
                            && let Some(m) = cur
                        {
                            let mut s = Settings::load();
                            let fav = format!("{}/{m}", self.st.provider);
                            if let Some(i) = s.favorites.iter().position(|f| *f == fav) {
                                s.favorites.remove(i);
                            } else {
                                s.favorites.push(fav);
                            }
                            let _ = s.save();
                            let ids: Vec<ModelInfo> = p
                                .items
                                .iter()
                                .map(|i| ModelInfo {
                                    id: i.value.clone(),
                                    context: None,
                                })
                                .collect();
                            let items = self.model_items(&ids);
                            if let Some(Modal::Pick(p, _)) = &mut self.modal {
                                p.items = items;
                            }
                        }
                    }
                    Pick::None => {}
                }
            }
        }
    }

    fn picked(&mut self, purpose: Purpose, v: String, req: &Sender<Req>) {
        match purpose {
            Purpose::Model => {
                self.modal = None;
                let mut s = Settings::load();
                s.models.insert(self.st.provider.clone(), v.clone());
                let _ = s.save();
                self.st.model = v.clone();
                self.st.window = crate::prices::context_window(&v);
                let _ = req.send(Req::SetModel(v.clone()));
                self.note(format!("model: {v}"), false);
            }
            Purpose::Resume => {
                self.modal = None;
                let _ = req.send(Req::Open(v));
            }
            Purpose::Logout => {
                self.modal = None;
                match login::logout(&v) {
                    Ok(_) => self.note(format!("forgot the {v} key"), false),
                    Err(e) => self.note(format!("{e:#}"), true),
                }
            }
            Purpose::Settings => {
                self.change_setting(&v, req);
                let items = self.settings_items();
                if let Some(Modal::Pick(p, _)) = &mut self.modal {
                    p.items = items;
                }
            }
            Purpose::Tree | Purpose::Fork => {
                self.modal = None;
                let Ok(id) = v.parse::<u64>() else { return };
                let Ok(s) = Session::open(&self.st.session) else {
                    return;
                };
                let Some(e) = s.get(id) else { return };
                match (&e.body, purpose) {
                    // branching at your own message: start just before it, text in the editor
                    (Body::User { text, .. }, _) => {
                        self.input = strip_inlined(text).chars().collect();
                        self.cursor = self.input.len();
                        match (purpose, e.parent) {
                            (Purpose::Fork, Some(p)) => {
                                let _ = req.send(Req::Fork(format!("{}/{p}", s.id)));
                            }
                            (_, Some(p)) => {
                                let _ = req.send(Req::SetHead(p));
                            }
                            _ => {}
                        }
                    }
                    _ => {
                        let _ = req.send(Req::SetHead(id));
                    }
                }
            }
        }
    }

    fn cycle_favorite(&mut self, req: &Sender<Req>) {
        let s = Settings::load();
        if s.favorites.is_empty() {
            self.note("no favorite models yet: ctrl+f in /model", false);
            return;
        }
        let cur = format!("{}/{}", self.st.provider, self.st.model);
        let i = s
            .favorites
            .iter()
            .position(|f| *f == cur)
            .map(|i| i + 1)
            .unwrap_or(0);
        let next = s.favorites[i % s.favorites.len()].clone();
        let Some((prov, model)) = next.split_once('/') else {
            return;
        };
        if prov == self.st.provider {
            let _ = req.send(Req::SetModel(model.to_string()));
        } else {
            match Spec::for_provider(prov, &crate::config::load_auth()) {
                Some(spec) => {
                    let _ = req.send(Req::SetProvider(spec, model.to_string()));
                }
                None => {
                    self.note(format!("no key for {prov}: /login"), true);
                    return;
                }
            }
        }
        self.st.provider = prov.to_string();
        self.st.model = model.to_string();
        self.st.window = crate::prices::context_window(model);
    }

    fn on_key(&mut self, k: KeyEvent, req: &Sender<Req>, up: &Sender<Up>) {
        if self.modal.is_some() {
            return self.on_modal_key(k, req);
        }
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let alt = k.modifiers.contains(KeyModifiers::ALT);
        let shift = k.modifiers.contains(KeyModifiers::SHIFT);
        if self.popup.is_some() {
            match k.code {
                KeyCode::Tab | KeyCode::Enter => return self.accept_popup(),
                KeyCode::Up | KeyCode::Down => {
                    let p = self.popup.as_mut().unwrap();
                    let n = p.items.len();
                    p.sel = if k.code == KeyCode::Up {
                        (p.sel + n - 1) % n
                    } else {
                        (p.sel + 1) % n
                    };
                    return;
                }
                KeyCode::Esc => {
                    self.popup = None;
                    self.popup_closed_for = Some(self.input[..self.cursor].iter().collect());
                    return;
                }
                _ => {}
            }
        }
        match k.code {
            KeyCode::Esc => {
                if self.busy {
                    exec::interrupt();
                    let mut back: Vec<String> = std::mem::take(&mut *self.steer.lock().unwrap());
                    back.append(&mut self.follow_up);
                    if !back.is_empty() {
                        let mut t: String = self.input.iter().collect();
                        for b in back {
                            if !t.is_empty() {
                                t.push('\n');
                            }
                            t.push_str(&b);
                        }
                        self.input = t.chars().collect();
                        self.cursor = self.input.len();
                    }
                    self.note("interrupted", true);
                } else if self
                    .last_esc
                    .is_some_and(|t| t.elapsed() < Duration::from_millis(500))
                {
                    self.last_esc = None;
                    self.open_tree(Purpose::Tree);
                } else {
                    self.last_esc = Some(Instant::now());
                }
            }
            KeyCode::Char('c') if ctrl => {
                if self.busy {
                    exec::interrupt();
                    self.note("interrupted", true);
                } else if !self.input.is_empty() {
                    self.input.clear();
                    self.cursor = 0;
                } else if self
                    .last_ctrl_c
                    .is_some_and(|t| t.elapsed() < Duration::from_secs(1))
                {
                    self.quit = true;
                } else {
                    self.last_ctrl_c = Some(Instant::now());
                    self.note("ctrl+c again to quit", false);
                }
            }
            KeyCode::Char('d') if ctrl && self.input.is_empty() => self.quit = true,
            KeyCode::Char('o') if ctrl => self.expand = !self.expand,
            KeyCode::Char('l') if ctrl => self.open_models(up),
            KeyCode::Char('p') if ctrl => self.cycle_favorite(req),
            KeyCode::Char('g') if ctrl => self.want_editor = true,
            KeyCode::Char('v') if ctrl => match clipboard_image() {
                Ok(p) => self.insert(&format!("@{} ", p.display())),
                Err(e) => self.note(format!("{e:#}"), true),
            },
            KeyCode::Char('u') if ctrl => {
                self.input.drain(..self.cursor);
                self.cursor = 0;
            }
            KeyCode::Char('w') if ctrl => {
                let mut i = self.cursor;
                while i > 0 && self.input[i - 1] == ' ' {
                    i -= 1;
                }
                while i > 0 && self.input[i - 1] != ' ' {
                    i -= 1;
                }
                self.input.drain(i..self.cursor);
                self.cursor = i;
            }
            KeyCode::Char('a') if ctrl => self.cursor = 0,
            KeyCode::Char('e') if ctrl => self.cursor = self.input.len(),
            KeyCode::Char('j') if ctrl => self.insert("\n"),
            KeyCode::BackTab => self.change_setting("thinking", req),
            KeyCode::Enter if shift => self.insert("\n"),
            KeyCode::Enter if alt => {
                if self.busy {
                    let t: String = self.input.drain(..).collect();
                    self.cursor = 0;
                    if !t.trim().is_empty() {
                        self.follow_up.push(t.trim().to_string());
                    }
                } else {
                    self.insert("\n");
                }
            }
            KeyCode::Enter => {
                let t: String = self.input.drain(..).collect();
                self.cursor = 0;
                self.popup = None;
                self.submit(t, req, up);
            }
            KeyCode::Char(c) => self.insert(&c.to_string()),
            KeyCode::Backspace if self.cursor > 0 => {
                self.cursor -= 1;
                self.input.remove(self.cursor);
            }
            KeyCode::Delete if self.cursor < self.input.len() => {
                self.input.remove(self.cursor);
            }
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(self.input.len()),
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = self.input.len(),
            KeyCode::PageUp => self.scroll += 10,
            KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(10),
            KeyCode::Up if shift => self.scroll += 1,
            KeyCode::Down if shift => self.scroll = self.scroll.saturating_sub(1),
            KeyCode::Up | KeyCode::Down if self.input.contains(&'\n') => {
                self.move_line(k.code == KeyCode::Up)
            }
            KeyCode::Up if !self.history.is_empty() => {
                let i = match self.hist_pos {
                    Some(i) => i.saturating_sub(1),
                    None => self.history.len() - 1,
                };
                self.hist_pos = Some(i);
                self.input = self.history[i].chars().collect();
                self.cursor = self.input.len();
            }
            KeyCode::Down if self.hist_pos.is_some() => {
                let i = self.hist_pos.unwrap() + 1;
                if i >= self.history.len() {
                    self.hist_pos = None;
                    self.input.clear();
                } else {
                    self.hist_pos = Some(i);
                    self.input = self.history[i].chars().collect();
                }
                self.cursor = self.input.len();
            }
            _ => {}
        }
        self.update_popup();
    }

    fn move_line(&mut self, up: bool) {
        let line_start = |c: usize, inp: &[char]| {
            (0..c)
                .rev()
                .find(|&i| inp[i] == '\n')
                .map(|i| i + 1)
                .unwrap_or(0)
        };
        let s = line_start(self.cursor, &self.input);
        let col = self.cursor - s;
        if up {
            if s == 0 {
                return;
            }
            let ps = line_start(s - 1, &self.input);
            self.cursor = (ps + col).min(s - 1);
        } else {
            let Some(e) = (self.cursor..self.input.len()).find(|&i| self.input[i] == '\n') else {
                return;
            };
            let ne = (e + 1..self.input.len())
                .find(|&i| self.input[i] == '\n')
                .unwrap_or(self.input.len());
            self.cursor = (e + 1 + col).min(ne);
        }
    }

    fn transcript(&self, width: usize) -> Vec<Line<'static>> {
        let t = &self.theme;
        let mut out: Vec<Line> = vec![];
        let dim = Style::default().fg(t.dim);
        for it in &self.items {
            match it {
                Item::User(text) => {
                    out.push(Line::raw(""));
                    for (i, l) in wrap(text, width - 2).into_iter().enumerate() {
                        let p = if i == 0 { "› " } else { "  " };
                        out.push(Line::styled(
                            format!("{p}{l}"),
                            Style::default().fg(t.user).add_modifier(Modifier::BOLD),
                        ));
                    }
                }
                Item::Assistant(text) => {
                    out.push(Line::raw(""));
                    let mut code = false;
                    for raw in text.trim().split('\n') {
                        if raw.trim_start().starts_with("```") {
                            code = !code;
                            out.push(Line::styled(raw.to_string(), dim));
                            continue;
                        }
                        let st = if code {
                            Style::default().fg(t.code)
                        } else {
                            Style::default().fg(t.text)
                        };
                        for l in wrap(raw, width) {
                            out.push(Line::styled(l, st));
                        }
                    }
                }
                Item::Tool {
                    command,
                    output,
                    code,
                    lines,
                } => {
                    let first = command.lines().next().unwrap_or("");
                    let more = if command.lines().count() > 1 {
                        " …"
                    } else {
                        ""
                    };
                    let status = match (output, code) {
                        (None, _) => format!(" {}", SPIN[(self.spin / 3) % 4]),
                        (Some(_), Some(0)) | (Some(_), None) => String::new(),
                        (Some(_), Some(c)) => format!(" [exit {c}]"),
                    };
                    let head: Vec<String> = wrap(&format!("$ {first}{more}"), width);
                    for (i, h) in head.iter().enumerate() {
                        let mut spans = vec![Span::styled(h.clone(), Style::default().fg(t.tool))];
                        if i + 1 == head.len() {
                            spans.push(Span::styled(
                                status.clone(),
                                if matches!(code, Some(c) if *c != 0) {
                                    Style::default().fg(t.error)
                                } else {
                                    dim
                                },
                            ));
                        }
                        out.push(Line::from(spans));
                    }
                    if let Some(o) = output {
                        let body: Vec<&str> = o.lines().collect();
                        let show = if self.expand {
                            body.len()
                        } else {
                            3.min(body.len())
                        };
                        for l in &body[..show] {
                            for w in wrap(l, width.saturating_sub(4)) {
                                out.push(Line::styled(format!("  │ {w}"), dim));
                            }
                        }
                        if !self.expand && body.len() > show {
                            out.push(Line::styled(
                                format!("  └ {lines} lines (ctrl+o to expand)"),
                                dim,
                            ));
                        }
                    }
                }
                Item::Note(text, err) => {
                    let st = if *err {
                        Style::default().fg(t.error)
                    } else {
                        dim.add_modifier(Modifier::ITALIC)
                    };
                    for l in wrap(text, width) {
                        out.push(Line::styled(l, st));
                    }
                }
            }
        }
        out
    }

    fn draw(&mut self, f: &mut ratatui::Frame) {
        let t = self.theme.clone();
        let area = f.area();
        let input_text: String = self.input.iter().collect();
        let iw = area.width.saturating_sub(4).max(8) as usize;
        let input_lines = wrap(&input_text, iw);
        let ih = (input_lines.len() as u16).clamp(1, 10) + 2;
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(3),
                Constraint::Length(ih),
                Constraint::Length(1),
            ])
            .split(area);

        let panels: Vec<(String, String)> = self
            .panels
            .out
            .lock()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let (main, side) = if !panels.is_empty() && area.width >= 90 {
            let c = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Min(40), Constraint::Length(42)])
                .split(rows[0]);
            (c[0], Some(c[1]))
        } else {
            (rows[0], None)
        };

        let w = main.width.saturating_sub(1).max(10) as usize;
        let lines = self.transcript(w);
        let h = main.height as usize;
        let max_scroll = lines.len().saturating_sub(h);
        self.scroll = self.scroll.min(max_scroll);
        let end = lines.len() - self.scroll;
        let start = end.saturating_sub(h);
        f.render_widget(
            Paragraph::new(lines[start..end].to_vec()),
            Rect {
                width: main.width.saturating_sub(1),
                ..main
            },
        );

        if let Some(side) = side {
            let n = panels.len() as u32;
            let cons: Vec<Constraint> = (0..n).map(|_| Constraint::Ratio(1, n)).collect();
            let boxes = Layout::default()
                .direction(Direction::Vertical)
                .constraints(cons)
                .split(side);
            for ((name, text), r) in panels.iter().zip(boxes.iter()) {
                f.render_widget(
                    Paragraph::new(text.trim_end().to_string())
                        .wrap(Wrap { trim: false })
                        .block(
                            Block::default()
                                .borders(Borders::ALL)
                                .border_style(Style::default().fg(t.border))
                                .title(format!(" {name} ")),
                        ),
                    *r,
                );
            }
        }

        // editor; the border color says how hard the model thinks
        let shown: Vec<Line> = if input_text.is_empty() {
            vec![Line::styled(
                if !self.has_agent {
                    "/login to pick a provider"
                } else if self.busy {
                    "enter steers it · alt+enter queues for after · esc interrupts"
                } else {
                    "what should tau do? · / for commands · @ for files"
                },
                Style::default().fg(t.dim),
            )]
        } else {
            let skip = input_lines.len().saturating_sub(10);
            input_lines[skip..]
                .iter()
                .map(|l| Line::raw(l.clone()))
                .collect()
        };
        let border = if self.busy {
            t.border
        } else {
            match self.thinking.as_str() {
                "auto" => t.accent,
                "low" => t.dim,
                "high" | "xhigh" | "max" => t.tool,
                _ => t.user,
            }
        };
        f.render_widget(
            Paragraph::new(shown).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(border)),
            ),
            rows[1],
        );
        let before: String = self.input[..self.cursor].iter().collect();
        let bl = wrap(&before, iw);
        let skip = input_lines.len().saturating_sub(10);
        let cy = (bl.len() as u16 - 1).saturating_sub(skip as u16).min(9);
        let cx: usize = bl
            .last()
            .map(|l| l.chars().map(|c| c.width().unwrap_or(0)).sum())
            .unwrap_or(0);
        if self.modal.is_none() {
            f.set_cursor_position((rows[1].x + 1 + cx as u16, rows[1].y + 1 + cy));
        }

        // completion popup just above the editor
        if let Some(p) = &self.popup {
            let hgt = p.items.len() as u16 + 2;
            let wid = p
                .items
                .iter()
                .map(|(a, b)| a.chars().count() + b.chars().count() + 4)
                .max()
                .unwrap_or(20)
                .clamp(20, area.width as usize - 2) as u16;
            let r = Rect {
                x: rows[1].x + 1,
                y: rows[1].y.saturating_sub(hgt),
                width: wid,
                height: hgt.min(rows[1].y),
            };
            let lines: Vec<Line> = p
                .items
                .iter()
                .enumerate()
                .map(|(i, (a, b))| {
                    let st = if i == p.sel {
                        Style::default()
                            .bg(t.selected_bg)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    };
                    Line::from(vec![
                        Span::styled(a.clone(), st),
                        Span::styled(format!("  {b}"), Style::default().fg(t.dim)),
                    ])
                })
                .collect();
            f.render_widget(Clear, r);
            f.render_widget(
                Paragraph::new(lines).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(Style::default().fg(t.border)),
                ),
                r,
            );
        }

        // status line
        let spin = SPIN[(self.spin / 3) % 4];
        let state = if self.compacting {
            format!("{spin} compacting")
        } else if self.running > 0 {
            format!("{spin} running {}", self.running)
        } else if self.busy {
            format!("{spin} thinking")
        } else {
            "ready".into()
        };
        let queued = self.steer.lock().unwrap().len() + self.follow_up.len();
        let mut parts = vec![format!(
            " τ {}",
            if self.st.model.is_empty() {
                "no model"
            } else {
                &self.st.model
            }
        )];
        if self.thinking != "auto" {
            parts.push(format!("think {}", self.thinking));
        }
        if self.st.window > 0 {
            parts.push(format!(
                "ctx {}/{} {}",
                human(self.st.ctx),
                human(self.st.window),
                meter(self.st.ctx, self.st.window)
            ));
        }
        parts.push(format!("${:.4}", self.st.usage.cost));
        parts.push(format!("cache {}%", self.st.usage.cache_pct()));
        if self.st.depth > 0 {
            parts.push(format!("depth {}", self.st.depth));
        }
        if queued > 0 {
            parts.push(format!("{queued} queued"));
        }
        if self.scroll > 0 {
            parts.push(format!("↑{}", self.scroll));
        }
        parts.push(state);
        let status: String = parts
            .join(" · ")
            .chars()
            .take(area.width as usize)
            .collect();
        f.render_widget(
            Paragraph::new(Line::styled(
                format!("{status:<width$}", width = area.width as usize),
                Style::default().fg(t.status_fg).bg(t.status_bg),
            )),
            rows[2],
        );

        match &self.modal {
            Some(Modal::Pick(p, _)) => p.draw(f, area, &t),
            Some(Modal::Login(fl)) => fl.draw(f, area, &t),
            Some(Modal::Help) => message(
                f,
                area,
                &t,
                "keys and commands",
                self.help_lines(),
                "any key to close",
            ),
            None => {}
        }
    }

    fn help_lines(&self) -> Vec<Line<'static>> {
        let t = &self.theme;
        let mut v: Vec<Line> = [
            "enter          send; while tau works it steers the next turn",
            "alt+enter      queue for after tau is done (newline when idle)",
            "shift+enter    newline (ctrl+j anywhere)",
            "esc            interrupt; twice when idle opens /tree",
            "ctrl+c         clear, interrupt, twice to quit",
            "ctrl+l         pick a model     ctrl+p  cycle favorites",
            "shift+tab      thinking level   ctrl+o  expand tool output",
            "ctrl+g         open $EDITOR     ctrl+v  paste an image",
            "@path          inline a file    !cmd    run into the context",
            "!!cmd          run, don't tell the model",
            "",
        ]
        .iter()
        .map(|s| Line::raw(s.to_string()))
        .collect();
        for (n, d) in BUILTINS {
            v.push(Line::from(vec![
                Span::styled(format!("/{n:<12}"), Style::default().fg(t.accent)),
                Span::raw(d),
            ]));
        }
        v
    }
}

fn first_line(s: &str) -> String {
    let l = s.trim().lines().next().unwrap_or("");
    let mut t: String = l.chars().take(70).collect();
    if l.chars().count() > 70 {
        t.push('…');
    }
    t
}

fn next_thinking(cur: &str) -> String {
    let i = THINKING
        .iter()
        .position(|t| *t == cur)
        .map(|i| i + 1)
        .unwrap_or(0);
    THINKING[i % THINKING.len()].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_respects_width_and_newlines() {
        assert_eq!(wrap("abcdefghij", 8), vec!["abcdefgh", "ij"]);
        assert_eq!(wrap("a\nb", 8), vec!["a", "b"]);
        assert_eq!(wrap("", 8), vec![""]);
    }

    #[test]
    fn meter_and_thinking_cycle() {
        assert_eq!(meter(0, 1000), "▱▱▱▱▱▱▱▱ 0%");
        assert_eq!(meter(500, 1000), "▰▰▰▰▱▱▱▱ 50%");
        assert_eq!(meter(5000, 1000), "▰▰▰▰▰▰▰▰ 100%");
        assert_eq!(next_thinking("auto"), "low");
        assert_eq!(next_thinking("max"), "auto");
    }

    #[test]
    fn inlined_files_are_hidden_in_the_transcript() {
        assert_eq!(
            strip_inlined("look at @a.rs\n\n<file path=\"a.rs\">\nfn x\n</file>"),
            "look at @a.rs"
        );
    }
}
