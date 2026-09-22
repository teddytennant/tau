//! Picking a provider and saving its key or account sign-in. Used for the
//! first run, `/login` and `tau login`.

use crate::config::{Cred, Settings, load_auth, update_auth};
use crate::provider::{KNOWN, Kind, ModelInfo, Spec, known, list_models, suggest};
use crate::theme::Theme;
use crate::widgets::{Item, Pick, Picker, TextInput, Typed, message};
use anyhow::Result;
use ratatui::Frame;
use ratatui::crossterm::event::{self, Event, KeyEvent, KeyEventKind};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

enum OAuthMsg {
    Url(String, u16),
    Done(Result<Vec<ModelInfo>, String>),
}

enum Step {
    Welcome,
    /// An account sign-in: the URL is shown, the redirect comes back to the
    /// loopback listener or is pasted into `input`.
    Signing {
        url: Option<String>,
        hint: Option<String>,
        input: TextInput,
        paste: std::sync::mpsc::Sender<String>,
        rx: Receiver<OAuthMsg>,
        cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
        copied: bool,
    },
    Provider(Picker),
    Url(TextInput),
    Key(TextInput),
    Checking(Receiver<Result<Vec<ModelInfo>, String>>),
    Failed(String),
    Model(Picker),
}

pub struct Flow {
    step: Step,
    provider: String,
    base: Option<String>,
    key: String,
    spin: usize,
    /// Where the sign-in URL goes on screen, set by `draw`. The URL itself is
    /// written by `paint_url` after the frame, as one run of text the terminal
    /// wraps, so copying it (tmux included) gives one line, not a line per row.
    url_at: std::cell::Cell<Option<(u16, u16)>>,
}

pub enum Out {
    Working,
    Cancelled,
    Done(Spec, String),
}

fn env_key(provider: &str) -> Option<(&'static str, String)> {
    let k = known(provider)?;
    std::env::var(k.env)
        .ok()
        .filter(|v| !v.is_empty())
        .map(|v| (k.env, v))
}

fn provider_picker() -> Picker {
    let auth = load_auth();
    let mut items: Vec<Item> = KNOWN
        .iter()
        .map(|k| {
            let state = if env_key(k.name).is_some() {
                format!("{} is set", k.env)
            } else if auth.contains_key(k.name) {
                "key saved".into()
            } else {
                String::new()
            };
            Item::new(k.label, state, k.name)
        })
        .collect();
    for a in [&crate::oauth::CHATGPT, &crate::oauth::XAI] {
        let state = if crate::oauth::signed_in(a.name) {
            "signed in"
        } else if a.name == "chatgpt" {
            "sign in with Plus, Pro or Team"
        } else {
            "sign in with your xAI account"
        };
        items.push(Item::new(format!("{} account", a.label), state, a.name));
    }
    items.push(Item::new(
        "Custom OpenAI-compatible URL",
        "llama.cpp, vLLM, Ollama, LM Studio, a proxy",
        "custom",
    ));
    let mut p = Picker::new("Pick a provider", items);
    p.literal = true;
    p.hint = "enter to pick · esc to cancel".into();
    p
}

impl Flow {
    pub fn new(welcome: bool) -> Flow {
        Flow {
            step: if welcome {
                Step::Welcome
            } else {
                Step::Provider(provider_picker())
            },
            provider: String::new(),
            base: None,
            key: String::new(),
            spin: 0,
            url_at: std::cell::Cell::new(None),
        }
    }

    fn spec(&self) -> Spec {
        let (kind, base) = match known(&self.provider) {
            Some(k) => (k.kind, k.base.to_string()),
            None => (Kind::OpenAi, self.base.clone().unwrap_or_default()),
        };
        Spec {
            name: self.provider.clone(),
            kind,
            base,
            key: self.key.clone(),
            bearer: false,
        }
    }

    fn ask_key(&mut self) {
        let note = match (env_key(&self.provider), self.provider.as_str()) {
            (Some((var, _)), _) => format!(
                "{var} is set in your environment and wins over a saved key. Leave this empty to use it."
            ),
            (None, "custom") => "Leave empty if the server needs no key.".into(),
            (None, "anthropic") => {
                "From console.anthropic.com. API keys only; tau does not use Claude subscriptions."
                    .into()
            }
            _ => "Saved to ~/.tau/auth.json, readable only by you.".into(),
        };
        let label = known(&self.provider)
            .map(|k| k.label)
            .unwrap_or("the server");
        self.step = Step::Key(TextInput::new(format!("API key for {label}"), note, true));
    }

    fn sign_in(&mut self, a: &'static crate::oauth::Account) {
        let (tx, rx) = channel();
        let (ptx, prx) = channel::<String>();
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let c2 = cancel.clone();
        std::thread::spawn(move || {
            let r = (|| -> Result<Vec<ModelInfo>> {
                let p = crate::oauth::begin(a)?;
                let _ = tx.send(OAuthMsg::Url(p.url.clone(), p.port));
                let code = p.wait(
                    Some(&prx),
                    &|| c2.load(std::sync::atomic::Ordering::SeqCst),
                    || {},
                )?;
                p.finish(&code)?;
                let spec = Spec::for_provider(a.name, &load_auth())
                    .ok_or_else(|| anyhow::anyhow!("signed in, but the tokens did not save"))?;
                Ok(list_models(&spec).unwrap_or_default())
            })();
            let _ = tx.send(OAuthMsg::Done(r.map_err(|e| format!("{e:#}"))));
        });
        self.step = Step::Signing {
            url: None,
            hint: None,
            input: TextInput::new(
                "or paste the redirect URL",
                "After signing in, the browser lands on a localhost address. If that page fails to load, copy its address here.",
                false,
            ),
            paste: ptx,
            rx,
            cancel,
            copied: false,
        };
    }

    fn model_step(&mut self, list: Vec<ModelInfo>) {
        let suggested = suggest(&self.provider, &list).unwrap_or_default();
        let mut items: Vec<Item> = list
            .iter()
            .map(|m| {
                let ctx = m
                    .context
                    .or(crate::prices::lookup(&m.id).and_then(|p| p.context))
                    .map(|c| format!("{}k context", c / 1000))
                    .unwrap_or_default();
                let price = if crate::oauth::account(&self.provider).is_some() {
                    String::new()
                } else {
                    crate::prices::label(&m.id)
                        .map(|l| format!("  {l}"))
                        .unwrap_or_default()
                };
                Item::new(m.id.clone(), format!("{ctx}{price}"), m.id.clone())
            })
            .collect();
        if items.is_empty() {
            items.push(Item::new(
                suggested.clone(),
                "the provider listed no models",
                suggested.clone(),
            ));
        }
        let mut p = Picker::new(
            format!("Signed in. {} models. Pick the default", list.len()),
            items,
        )
        .select_value(&suggested);
        p.hint = "type to filter · enter to pick · change it later with /model".into();
        self.step = Step::Model(p);
    }

    fn check(&mut self) {
        let spec = self.spec();
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let _ = tx.send(list_models(&spec).map_err(|e| format!("{e:#}")));
        });
        self.step = Step::Checking(rx);
    }

    /// Call every tick; picks up the result of the key check.
    pub fn poll(&mut self) {
        self.spin = self.spin.wrapping_add(1);
        if let Step::Signing { url, hint, rx, .. } = &mut self.step {
            match rx.try_recv() {
                Ok(OAuthMsg::Url(u, port)) => {
                    *hint = crate::oauth::remote_hint(port);
                    if hint.is_none() {
                        let _ = std::process::Command::new("xdg-open")
                            .arg(&u)
                            .stdout(std::process::Stdio::null())
                            .stderr(std::process::Stdio::null())
                            .spawn()
                            .or_else(|_| std::process::Command::new("open").arg(&u).spawn());
                    }
                    *url = Some(u);
                }
                Ok(OAuthMsg::Done(Ok(list))) => self.model_step(list),
                Ok(OAuthMsg::Done(Err(e))) => self.step = Step::Failed(e),
                Err(_) => {}
            }
            return;
        }
        let Step::Checking(rx) = &self.step else {
            return;
        };
        let Ok(r) = rx.try_recv() else { return };
        self.step = match r {
            Err(e) => Step::Failed(e),
            Ok(list) => {
                let suggested = suggest(&self.provider, &list).unwrap_or_default();
                let mut items: Vec<Item> = list
                    .iter()
                    .map(|m| {
                        let ctx = m
                            .context
                            .or(crate::prices::lookup(&m.id).and_then(|p| p.context))
                            .map(|c| format!("{}k context", c / 1000))
                            .unwrap_or_default();
                        let price = crate::prices::label(&m.id)
                            .map(|l| format!("  {l}"))
                            .unwrap_or_default();
                        Item::new(m.id.clone(), format!("{ctx}{price}"), m.id.clone())
                    })
                    .collect();
                if items.is_empty() {
                    items.push(Item::new(
                        suggested.clone(),
                        "the server listed no models",
                        suggested.clone(),
                    ));
                }
                let mut p = Picker::new(
                    format!("Key works. {} models. Pick the default", list.len()),
                    items,
                )
                .select_value(&suggested);
                p.hint = "type to filter · enter to pick · change it later with /model".into();
                Step::Model(p)
            }
        };
    }

    pub fn on_paste(&mut self, s: &str) {
        match &mut self.step {
            Step::Key(t) | Step::Url(t) => t.paste(s),
            Step::Signing { input, .. } => input.paste(s),
            _ => {}
        }
    }

    pub fn on_key(&mut self, k: KeyEvent) -> Out {
        match &mut self.step {
            Step::Welcome => match k.code {
                event::KeyCode::Esc => return Out::Cancelled,
                _ => self.step = Step::Provider(provider_picker()),
            },
            Step::Provider(p) => match p.on_key(k) {
                Pick::Chosen(v) => {
                    self.provider = v;
                    if let Some(a) = crate::oauth::account(&self.provider) {
                        self.sign_in(a);
                    } else if self.provider == "custom" {
                        self.step = Step::Url(TextInput::new(
                            "Server URL",
                            "The OpenAI-compatible base URL, e.g. http://localhost:8080/v1",
                            false,
                        ));
                    } else {
                        self.ask_key();
                    }
                }
                Pick::Cancel => return Out::Cancelled,
                _ => {}
            },
            Step::Url(t) => match t.on_key(k) {
                Typed::Done(u) if !u.is_empty() => {
                    self.base = Some(u.trim_end_matches('/').to_string());
                    self.ask_key();
                }
                Typed::Cancel => self.step = Step::Provider(provider_picker()),
                _ => {}
            },
            Step::Key(t) => match t.on_key(k) {
                Typed::Done(key) => {
                    let key = if key.is_empty() {
                        env_key(&self.provider).map(|(_, v)| v).unwrap_or_default()
                    } else {
                        key
                    };
                    if key.is_empty() && self.provider != "custom" {
                        return Out::Working;
                    }
                    self.key = key;
                    self.check();
                }
                Typed::Cancel => self.step = Step::Provider(provider_picker()),
                Typed::None => {}
            },
            Step::Signing {
                input,
                paste,
                cancel,
                url,
                copied,
                ..
            } => {
                let ctrl = k.modifiers.contains(event::KeyModifiers::CONTROL);
                if k.code == event::KeyCode::Char('y') && ctrl {
                    if let Some(u) = url {
                        *copied = copy(u).is_ok();
                    }
                    return Out::Working;
                }
                match input.on_key(k) {
                    Typed::Done(v) if !v.is_empty() => {
                        let _ = paste.send(v);
                        input.value.clear();
                    }
                    Typed::Cancel => {
                        cancel.store(true, std::sync::atomic::Ordering::SeqCst);
                        self.step = Step::Provider(provider_picker());
                    }
                    _ => {}
                }
            }
            Step::Checking(_) => {
                if k.code == event::KeyCode::Esc {
                    self.ask_key();
                }
            }
            Step::Failed(_) if crate::oauth::account(&self.provider).is_some() => {
                self.step = Step::Provider(provider_picker())
            }
            Step::Failed(_) => self.ask_key(),
            Step::Model(p) => match p.on_key(k) {
                Pick::Chosen(model) => {
                    let spec = if crate::oauth::account(&self.provider).is_some() {
                        match Spec::for_provider(&self.provider, &load_auth()) {
                            Some(s) => s,
                            None => {
                                self.step = Step::Failed("the sign-in did not save".into());
                                return Out::Working;
                            }
                        }
                    } else {
                        self.spec()
                    };
                    if let Err(e) = save(
                        &spec,
                        &model,
                        crate::oauth::account(&self.provider).is_some()
                            || env_key(&self.provider).is_some_and(|(_, v)| v == self.key),
                    ) {
                        self.step = Step::Failed(format!("could not save: {e:#}"));
                        return Out::Working;
                    }
                    return Out::Done(spec, model);
                }
                Pick::Cancel => self.ask_key(),
                _ => {}
            },
        }
        Out::Working
    }

    pub fn draw(&self, f: &mut Frame, area: Rect, t: &Theme) {
        self.url_at.set(None);
        match &self.step {
            Step::Welcome => message(
                f,
                area,
                t,
                "welcome to tau",
                welcome_lines(t),
                "press any key",
            ),
            Step::Provider(p) | Step::Model(p) => p.draw(f, area, t),
            Step::Signing {
                url,
                hint,
                input,
                copied,
                ..
            } => {
                let label = crate::oauth::account(&self.provider)
                    .map(|a| a.label)
                    .unwrap_or("");
                f.render_widget(ratatui::widgets::Clear, area);
                let Some(u) = url else {
                    self.url_at.set(None);
                    message(
                        f,
                        area,
                        t,
                        &format!("{label} sign-in"),
                        vec![Line::raw(format!(
                            "{} starting the {label} sign-in…",
                            ["◐", "◓", "◑", "◒"][(self.spin / 3) % 4]
                        ))],
                        "esc cancels",
                    );
                    return;
                };
                // Full width and no frame, so `paint_url` can rewrite the URL
                // from column 0 as one run the terminal wraps.
                let w = area.width.max(1);
                let url_rows = (u.chars().count() as u16).div_ceil(w);
                let top = vec![
                    Line::styled(
                        format!("{label} sign-in"),
                        Style::default().fg(t.accent).add_modifier(Modifier::BOLD),
                    ),
                    Line::raw(format!(
                        "Open this URL to sign in with your {label} account:"
                    )),
                    Line::raw(""),
                ];
                let top_h = top.len() as u16;
                f.render_widget(
                    ratatui::widgets::Paragraph::new(top),
                    Rect {
                        height: top_h.min(area.height),
                        ..area
                    },
                );
                self.url_at.set(Some((area.x, area.y + top_h)));
                // The same characters go in the buffer too, so the frame after
                // this screen knows to clear them.
                let chars: Vec<char> = u.chars().collect();
                for (i, row) in chars.chunks(w as usize).enumerate() {
                    let y = area.y + top_h + i as u16;
                    if y < area.bottom() {
                        f.buffer_mut().set_string(
                            area.x,
                            y,
                            row.iter().collect::<String>(),
                            Style::default(),
                        );
                    }
                }
                let mut lines = vec![
                    Line::raw(""),
                    Line::styled(
                        if *copied {
                            "copied".to_string()
                        } else {
                            "ctrl+y copies it".to_string()
                        },
                        Style::default().fg(t.dim),
                    ),
                ];
                if let Some(h) = hint {
                    lines.push(Line::raw(""));
                    for l in h.lines() {
                        lines.push(Line::raw(l.to_string()));
                    }
                }
                lines.push(Line::raw(""));
                lines.push(Line::raw("Waiting for the browser…"));
                lines.push(Line::raw(""));
                lines.push(Line::from(vec![
                    ratatui::text::Span::styled("paste › ", Style::default().fg(t.accent)),
                    ratatui::text::Span::raw(input.value.clone()),
                    ratatui::text::Span::styled("▏", Style::default().fg(t.dim)),
                ]));
                lines.push(Line::styled(
                    "enter submits a paste · esc cancels",
                    Style::default().fg(t.dim),
                ));
                let y = area.y + top_h + url_rows;
                if y < area.bottom() {
                    f.render_widget(
                        ratatui::widgets::Paragraph::new(lines)
                            .wrap(ratatui::widgets::Wrap { trim: false }),
                        Rect {
                            y,
                            height: area.bottom() - y,
                            ..area
                        },
                    );
                }
            }
            Step::Url(i) | Step::Key(i) => i.draw(f, area, t),
            Step::Checking(_) => {
                let s = ["◐", "◓", "◑", "◒"][(self.spin / 3) % 4];
                message(
                    f,
                    area,
                    t,
                    "checking",
                    vec![Line::raw(format!("{s} listing models to check the key…"))],
                    "esc to go back",
                )
            }
            Step::Failed(e) => message(
                f,
                area,
                t,
                "that did not work",
                vec![
                    Line::styled(e.clone(), Style::default().fg(t.error)),
                    Line::raw(""),
                    Line::raw(if crate::oauth::account(&self.provider).is_some() {
                        "Try the sign-in again."
                    } else {
                        "Check the key (and URL) and try again."
                    }),
                ],
                "any key to retry",
            ),
        }
    }
}

impl Flow {
    /// Call right after each frame is drawn. Writes the sign-in URL as one
    /// unbroken run of text (an OSC 8 link) so the terminal soft-wraps it.
    /// Rows drawn cell by cell copy out of tmux as separate lines.
    pub fn paint_url(&self) {
        use std::io::Write;
        let (Some((x, y)), Step::Signing { url: Some(u), .. }) = (self.url_at.get(), &self.step)
        else {
            return;
        };
        let mut o = std::io::stdout();
        let _ = write!(
            o,
            "\x1b7\x1b[{};{}H\x1b]8;;{u}\x1b\\{u}\x1b]8;;\x1b\\\x1b8",
            y + 1,
            x + 1
        );
        let _ = o.flush();
    }
}

fn welcome_lines(t: &Theme) -> Vec<Line<'static>> {
    vec![
        Line::styled(
            "τ = 2π. It's pi, twice.",
            Style::default().fg(t.accent).add_modifier(Modifier::BOLD),
        ),
        Line::raw(""),
        Line::raw("tau is a coding agent with one tool, bash. It runs commands"),
        Line::raw("as you, in this directory. There is no sandbox, so start it"),
        Line::raw("somewhere you would let a colleague type."),
        Line::raw(""),
        Line::raw("First, pick a model provider and paste an API key. It gets"),
        Line::raw("checked with one request that lists models, then saved to"),
        Line::raw("~/.tau/auth.json (mode 600). Environment variables still win."),
    ]
}

/// Writes the key (unless it came from the environment) and the defaults.
pub fn save(spec: &Spec, model: &str, key_from_env: bool) -> Result<()> {
    if !key_from_env || spec.name == "custom" {
        update_auth(|auth| {
            auth.insert(
                spec.name.clone(),
                Cred {
                    key: spec.key.clone(),
                    base_url: (spec.name == "custom").then(|| spec.base.clone()),
                    oauth: None,
                },
            )
        })?;
    }
    let mut s = Settings::load();
    s.provider = Some(spec.name.clone());
    s.models.insert(spec.name.clone(), model.to_string());
    let fav = format!("{}/{model}", spec.name);
    if !s.favorites.contains(&fav) {
        s.favorites.push(fav);
    }
    s.save()
}

pub fn logout(provider: &str) -> Result<bool> {
    let had = update_auth(|auth| auth.remove(provider).is_some())?;
    let mut s = Settings::load();
    if s.provider.as_deref() == Some(provider) {
        s.provider = None;
        s.save()?;
    }
    Ok(had)
}

/// `tau login` and the first run outside the main UI.
pub fn standalone(welcome: bool) -> Result<Option<(Spec, String)>> {
    let theme = crate::theme::load(&Settings::load().theme);
    let mut term = ratatui::init();
    let _ = ratatui::crossterm::execute!(
        std::io::stdout(),
        ratatui::crossterm::event::EnableBracketedPaste
    );
    let mut flow = Flow::new(welcome);
    let res = (|| -> Result<Option<(Spec, String)>> {
        loop {
            flow.poll();
            term.draw(|f| flow.draw(f, f.area(), &theme))?;
            flow.paint_url();
            if event::poll(Duration::from_millis(50))? {
                match event::read()? {
                    Event::Key(k) if k.kind != KeyEventKind::Release => match flow.on_key(k) {
                        Out::Done(s, m) => return Ok(Some((s, m))),
                        Out::Cancelled => return Ok(None),
                        Out::Working => {}
                    },
                    Event::Paste(s) => flow.on_paste(&s),
                    _ => {}
                }
            }
        }
    })();
    let _ = ratatui::crossterm::execute!(
        std::io::stdout(),
        ratatui::crossterm::event::DisableBracketedPaste
    );
    ratatui::restore();
    res
}

/// Puts text on the clipboard. Inside tmux it goes through `tmux load-buffer
/// -w`, because tmux drops an application's own OSC 52 unless `set-clipboard`
/// is `on`; elsewhere a local clipboard tool, then OSC 52, which also works
/// over SSH.
pub fn copy(text: &str) -> Result<()> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut tools: Vec<(&str, Vec<&str>)> = vec![];
    if std::env::var_os("TMUX").is_some() {
        tools.push(("tmux", vec!["load-buffer", "-w", "-"]));
    }
    tools.extend([
        ("wl-copy", vec![]),
        ("xclip", vec!["-selection", "clipboard"]),
        ("pbcopy", vec![]),
    ]);
    for (cmd, args) in tools {
        let Ok(mut c) = Command::new(cmd)
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            continue;
        };
        if let Some(mut i) = c.stdin.take() {
            i.write_all(text.as_bytes())?;
        }
        if c.wait()?.success() {
            return Ok(());
        }
    }
    use base64::Engine;
    let b = base64::engine::general_purpose::STANDARD.encode(text);
    let mut o = std::io::stdout();
    write!(o, "\x1b]52;c;{b}\x07")?;
    o.flush()?;
    Ok(())
}
