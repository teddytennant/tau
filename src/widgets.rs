//! A filterable picker and a one-line text input, drawn as centered boxes.

use crate::theme::Theme;
use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

#[derive(Clone, Debug)]
pub struct Item {
    pub label: String,
    pub detail: String,
    pub value: String,
}

impl Item {
    pub fn new(
        label: impl Into<String>,
        detail: impl Into<String>,
        value: impl Into<String>,
    ) -> Item {
        Item {
            label: label.into(),
            detail: detail.into(),
            value: value.into(),
        }
    }
}

pub enum Pick {
    None,
    Chosen(String),
    Cancel,
    /// A key the picker does not handle itself, with the highlighted value.
    Other(KeyEvent, Option<String>),
}

pub struct Picker {
    pub title: String,
    pub hint: String,
    pub items: Vec<Item>,
    pub filter: String,
    pub sel: usize,
    /// Keep the list order and match the filter as a plain substring.
    pub literal: bool,
}

impl Picker {
    pub fn new(title: impl Into<String>, items: Vec<Item>) -> Picker {
        Picker {
            title: title.into(),
            hint: "type to filter · enter to pick · esc to close".into(),
            items,
            filter: String::new(),
            sel: 0,
            literal: false,
        }
    }

    pub fn select_value(mut self, v: &str) -> Picker {
        if let Some(i) = self
            .visible()
            .iter()
            .position(|&i| self.items[i].value == v)
        {
            self.sel = i;
        }
        self
    }

    pub fn visible(&self) -> Vec<usize> {
        if self.filter.is_empty() {
            return (0..self.items.len()).collect();
        }
        let f = self.filter.to_lowercase();
        let mut v: Vec<(i64, usize)> = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(i, it)| {
                if self.literal {
                    let hay = format!("{} {}", it.label, it.detail).to_lowercase();
                    return hay.contains(&f).then_some((0, i));
                }
                // the label decides the order; the detail can still match
                crate::files::score(&it.label, &f)
                    .map(|s| (-s, i))
                    .or_else(|| {
                        it.detail
                            .to_lowercase()
                            .contains(&f)
                            .then_some((i64::MAX / 2, i))
                    })
            })
            .collect();
        v.sort();
        v.into_iter().map(|(_, i)| i).collect()
    }

    pub fn current(&self) -> Option<&Item> {
        self.visible().get(self.sel).map(|&i| &self.items[i])
    }

    pub fn on_key(&mut self, k: KeyEvent) -> Pick {
        let n = self.visible().len();
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Esc => return Pick::Cancel,
            KeyCode::Char('c') if ctrl => return Pick::Cancel,
            KeyCode::Enter => {
                return match self.current() {
                    Some(it) => Pick::Chosen(it.value.clone()),
                    None => Pick::None,
                };
            }
            KeyCode::Up => self.sel = self.sel.checked_sub(1).unwrap_or(n.saturating_sub(1)),
            KeyCode::Down | KeyCode::Tab => self.sel = if n == 0 { 0 } else { (self.sel + 1) % n },
            KeyCode::PageUp => self.sel = self.sel.saturating_sub(10),
            KeyCode::PageDown => self.sel = (self.sel + 10).min(n.saturating_sub(1)),
            KeyCode::Backspace => {
                self.filter.pop();
                self.sel = 0;
            }
            KeyCode::Char(c) if !ctrl => {
                self.filter.push(c);
                self.sel = 0;
            }
            _ => return Pick::Other(k, self.current().map(|i| i.value.clone())),
        }
        Pick::None
    }

    pub fn draw(&self, f: &mut Frame, area: Rect, t: &Theme) {
        let w = area.width.saturating_sub(4).clamp(20, 100);
        let want = self.items.len().max(1) as u16 + 4;
        let h = want.min(area.height.saturating_sub(4)).clamp(6, 24);
        let r = center(area, w, h);
        f.render_widget(Clear, r);
        let vis = self.visible();
        let rows = h.saturating_sub(4) as usize;
        let top = self.sel.saturating_sub(rows.saturating_sub(1));
        let mut lines = vec![Line::from(vec![
            Span::styled("› ", Style::default().fg(t.accent)),
            Span::raw(self.filter.clone()),
            Span::styled("▏", Style::default().fg(t.dim)),
        ])];
        let inner = w.saturating_sub(4) as usize;
        for (row, &i) in vis.iter().enumerate().skip(top).take(rows) {
            let it = &self.items[i];
            let label: String = it.label.chars().take(inner).collect();
            let room = inner.saturating_sub(label.chars().count() + 2);
            let detail: String = it.detail.chars().take(room).collect();
            let mut st = Style::default();
            if row == self.sel {
                st = st.bg(t.selected_bg).add_modifier(Modifier::BOLD);
            }
            lines.push(Line::from(vec![
                Span::styled(label, st),
                Span::styled(format!("  {detail}"), Style::default().fg(t.dim)),
            ]));
        }
        if vis.is_empty() {
            lines.push(Line::styled("nothing matches", Style::default().fg(t.dim)));
        }
        f.render_widget(
            Paragraph::new(lines).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(t.accent))
                    .title(format!(" {} ", self.title))
                    .title_bottom(Line::styled(
                        format!(" {} ", self.hint),
                        Style::default().fg(t.dim),
                    )),
            ),
            r,
        );
    }
}

pub struct TextInput {
    pub title: String,
    pub note: String,
    pub value: String,
    pub masked: bool,
}

pub enum Typed {
    None,
    Done(String),
    Cancel,
}

impl TextInput {
    pub fn new(title: impl Into<String>, note: impl Into<String>, masked: bool) -> TextInput {
        TextInput {
            title: title.into(),
            note: note.into(),
            value: String::new(),
            masked,
        }
    }

    pub fn on_key(&mut self, k: KeyEvent) -> Typed {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        match k.code {
            KeyCode::Esc => return Typed::Cancel,
            KeyCode::Char('c') if ctrl => return Typed::Cancel,
            KeyCode::Char('u') if ctrl => self.value.clear(),
            KeyCode::Enter => return Typed::Done(self.value.trim().to_string()),
            KeyCode::Backspace => {
                self.value.pop();
            }
            KeyCode::Char(c) if !ctrl => self.value.push(c),
            _ => {}
        }
        Typed::None
    }

    pub fn paste(&mut self, s: &str) {
        self.value.push_str(s.trim());
    }

    pub fn draw(&self, f: &mut Frame, area: Rect, t: &Theme) {
        let w = area.width.saturating_sub(4).clamp(20, 80);
        let r = center(area, w, 8);
        f.render_widget(Clear, r);
        let shown = if self.masked {
            let n = self.value.chars().count();
            if n <= 8 {
                "•".repeat(n)
            } else {
                // enough to recognize which key it is, not enough to use it
                let head: String = self.value.chars().take(7).collect();
                format!("{head}{}", "•".repeat((n - 7).min(40)))
            }
        } else {
            self.value.clone()
        };
        let lines = vec![
            Line::from(vec![
                Span::styled("› ", Style::default().fg(t.accent)),
                Span::raw(shown),
                Span::styled("▏", Style::default().fg(t.dim)),
            ]),
            Line::raw(""),
            Line::styled(self.note.clone(), Style::default().fg(t.dim)),
        ];
        f.render_widget(
            Paragraph::new(lines).wrap(Wrap { trim: false }).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(t.accent))
                    .title(format!(" {} ", self.title))
                    .title_bottom(Line::styled(
                        " enter to continue · esc to go back ",
                        Style::default().fg(t.dim),
                    )),
            ),
            r,
        );
    }
}

pub fn center(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

/// A box of plain lines, for help and messages.
pub fn message(
    f: &mut Frame,
    area: Rect,
    t: &Theme,
    title: &str,
    lines: Vec<Line<'static>>,
    footer: &str,
) {
    let w = area.width.saturating_sub(4).clamp(20, 76);
    // rows after wrapping, so nothing at the bottom gets cut off
    let inner = w.saturating_sub(2).max(1) as usize;
    let rows: usize = lines.iter().map(|l| l.width().max(1).div_ceil(inner)).sum();
    let h = (rows as u16 + 2).min(area.height);
    let r = center(area, w, h);
    f.render_widget(Clear, r);
    f.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(t.accent))
                .title(format!(" {title} "))
                .title_bottom(Line::styled(
                    format!(" {footer} "),
                    Style::default().fg(t.dim),
                )),
        ),
        r,
    );
}
