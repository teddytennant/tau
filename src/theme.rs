//! Colors. `dark` and `light` are built in; ~/.tau/themes/NAME.json can
//! override any key, e.g. {"accent": "#ff8800", "dim": 244}.

use ratatui::style::Color;
use serde_json::Value;

#[derive(Clone, Debug, PartialEq)]
pub struct Theme {
    pub name: String,
    pub text: Color,
    pub accent: Color,
    pub user: Color,
    pub tool: Color,
    pub error: Color,
    pub dim: Color,
    pub code: Color,
    pub status_fg: Color,
    pub status_bg: Color,
    pub border: Color,
    pub selected_bg: Color,
}

pub fn dark() -> Theme {
    Theme {
        name: "dark".into(),
        text: Color::Reset,
        accent: Color::Cyan,
        user: Color::Cyan,
        tool: Color::Yellow,
        error: Color::Red,
        dim: Color::DarkGray,
        code: Color::Yellow,
        status_fg: Color::Black,
        status_bg: Color::Cyan,
        border: Color::DarkGray,
        selected_bg: Color::Rgb(45, 45, 48),
    }
}

pub fn light() -> Theme {
    Theme {
        name: "light".into(),
        text: Color::Reset,
        accent: Color::Rgb(0, 95, 175),
        user: Color::Rgb(0, 95, 175),
        tool: Color::Rgb(135, 85, 0),
        error: Color::Rgb(180, 30, 30),
        dim: Color::Rgb(120, 120, 120),
        code: Color::Rgb(135, 85, 0),
        status_fg: Color::White,
        status_bg: Color::Rgb(0, 95, 175),
        border: Color::Rgb(170, 170, 170),
        selected_bg: Color::Rgb(225, 228, 235),
    }
}

pub fn parse_color(v: &Value) -> Option<Color> {
    if let Some(n) = v.as_u64() {
        return u8::try_from(n).ok().map(Color::Indexed);
    }
    let s = v.as_str()?.trim();
    if let Some(h) = s.strip_prefix('#')
        && h.len() == 6
    {
        let c = u32::from_str_radix(h, 16).ok()?;
        return Some(Color::Rgb((c >> 16) as u8, (c >> 8) as u8, c as u8));
    }
    s.parse::<Color>().ok()
}

pub fn from_json(name: &str, v: &Value) -> Theme {
    let mut t = if v["base"].as_str() == Some("light") {
        light()
    } else {
        dark()
    };
    t.name = name.to_string();
    let set = |k: &str, slot: &mut Color| {
        if let Some(c) = parse_color(&v[k]) {
            *slot = c;
        }
    };
    set("text", &mut t.text);
    set("accent", &mut t.accent);
    set("user", &mut t.user);
    set("tool", &mut t.tool);
    set("error", &mut t.error);
    set("dim", &mut t.dim);
    set("code", &mut t.code);
    set("status_fg", &mut t.status_fg);
    set("status_bg", &mut t.status_bg);
    set("border", &mut t.border);
    set("selected_bg", &mut t.selected_bg);
    t
}

pub fn names() -> Vec<String> {
    let mut v = vec!["dark".to_string(), "light".to_string()];
    if let Ok(rd) = std::fs::read_dir(crate::log::tau_home().join("themes")) {
        let mut user: Vec<String> = rd
            .flatten()
            .filter_map(|e| {
                let p = e.path();
                if p.extension()? != "json" {
                    return None;
                }
                Some(p.file_stem()?.to_string_lossy().to_string())
            })
            .collect();
        user.sort();
        v.extend(user);
    }
    v
}

pub fn load(name: &str) -> Theme {
    match name {
        "light" => light(),
        "dark" | "" => dark(),
        n => std::fs::read_to_string(
            crate::log::tau_home()
                .join("themes")
                .join(format!("{n}.json")),
        )
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .map(|v| from_json(n, &v))
        .unwrap_or_else(dark),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_theme_overrides_keys() {
        let t = from_json(
            "sunset",
            &serde_json::json!({"base": "light", "accent": "#ff8800", "dim": 244, "error": "magenta"}),
        );
        assert_eq!(t.accent, Color::Rgb(255, 136, 0));
        assert_eq!(t.dim, Color::Indexed(244));
        assert_eq!(t.error, Color::Magenta);
        assert_eq!(t.status_bg, light().status_bg);
    }
}
