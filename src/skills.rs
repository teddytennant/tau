//! Skills (~/.tau/skills) and prompt templates (~/.tau/prompts). Both are
//! plain Markdown. Only a skill's name and description go in the system
//! prompt; the model reads the body with cat when it needs it.

use crate::log::tau_home;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
}

/// `---` delimited `key: value` lines at the top, and the body after them.
pub fn frontmatter(text: &str) -> (Vec<(String, String)>, &str) {
    let Some(rest) = text.strip_prefix("---\n") else {
        return (vec![], text);
    };
    let Some(end) = rest.find("\n---") else {
        return (vec![], text);
    };
    let fields = rest[..end]
        .lines()
        .filter_map(|l| {
            let (k, v) = l.split_once(':')?;
            Some((
                k.trim().to_string(),
                v.trim().trim_matches('"').trim_matches('\'').to_string(),
            ))
        })
        .collect();
    let body = rest[end + 4..].trim_start_matches(['-', '\n']);
    (fields, body)
}

fn field<'a>(f: &'a [(String, String)], k: &str) -> Option<&'a str> {
    f.iter().find(|(a, _)| a == k).map(|(_, v)| v.as_str())
}

pub fn expand_home(p: &str) -> PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(rest),
        None => PathBuf::from(p),
    }
}

fn skill_from(path: &Path, default_name: &str) -> Option<Skill> {
    let text = fs::read_to_string(path).ok()?;
    let (f, body) = frontmatter(&text);
    let description = field(&f, "description").map(String::from).or_else(|| {
        body.lines()
            .map(|l| l.trim_start_matches('#').trim())
            .find(|l| !l.is_empty())
            .map(String::from)
    })?;
    Some(Skill {
        name: field(&f, "name").unwrap_or(default_name).to_string(),
        description,
        path: path.to_path_buf(),
    })
}

pub fn discover(extra: &[String]) -> Vec<Skill> {
    let mut dirs = vec![tau_home().join("skills")];
    dirs.extend(extra.iter().map(|d| expand_home(d)));
    let mut out: Vec<Skill> = vec![];
    for d in dirs {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        let mut ents: Vec<_> = rd.flatten().collect();
        ents.sort_by_key(|e| e.file_name());
        for e in ents {
            let p = e.path();
            let name = e.file_name().to_string_lossy().to_string();
            let s = if p.join("SKILL.md").is_file() {
                skill_from(&p.join("SKILL.md"), &name)
            } else if p.extension().is_some_and(|x| x == "md") {
                // a loose .md is a skill only if it says so
                fs::read_to_string(&p)
                    .ok()
                    .filter(|t| field(&frontmatter(t).0, "description").is_some())
                    .and_then(|_| skill_from(&p, name.trim_end_matches(".md")))
            } else {
                None
            };
            if let Some(s) = s
                && !out.iter().any(|o| o.name == s.name)
            {
                out.push(s);
            }
        }
    }
    out
}

pub fn prompt_section(skills: &[Skill]) -> String {
    if skills.is_empty() {
        return String::new();
    }
    let mut s =
        String::from("\n\nSkills. When a task matches one, cat its file first and follow it:");
    for k in skills {
        s.push_str(&format!(
            "\n- {}: {} ({})",
            k.name,
            k.description,
            k.path.display()
        ));
    }
    s
}

#[derive(Clone, Debug)]
pub struct Template {
    pub name: String,
    pub description: String,
    pub body: String,
}

pub fn templates() -> Vec<Template> {
    let mut out = vec![];
    let Ok(rd) = fs::read_dir(tau_home().join("prompts")) else {
        return out;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().is_none_or(|x| x != "md") {
            continue;
        }
        let Ok(text) = fs::read_to_string(&p) else {
            continue;
        };
        let (f, body) = frontmatter(&text);
        let name = p
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let description = field(&f, "description")
            .map(String::from)
            .or_else(|| {
                body.lines()
                    .find(|l| !l.trim().is_empty())
                    .map(String::from)
            })
            .unwrap_or_default();
        out.push(Template {
            name,
            description,
            body: body.to_string(),
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Splits arguments like a shell would, honouring quotes.
pub fn split_args(s: &str) -> Vec<String> {
    let mut out = vec![];
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let mut any = false;
    for c in s.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => cur.push(c),
            (None, '"' | '\'') => {
                quote = Some(c);
                any = true;
            }
            (None, c) if c.is_whitespace() => {
                if !cur.is_empty() || any {
                    out.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            (None, c) => cur.push(c),
        }
    }
    if !cur.is_empty() || any {
        out.push(cur);
    }
    out
}

/// `$1`, `$@`, `$ARGUMENTS`, `${1:-default}`, `${@:-default}`, `${@:N}` and `${@:N:L}`.
pub fn expand(body: &str, args: &[String]) -> String {
    let all = args.join(" ");
    let arg = |n: usize| args.get(n.wrapping_sub(1)).cloned().unwrap_or_default();
    let mut out = String::new();
    let b: Vec<char> = body.chars().collect();
    let mut i = 0;
    while i < b.len() {
        if b[i] != '$' {
            out.push(b[i]);
            i += 1;
            continue;
        }
        let rest: String = b[i + 1..].iter().collect();
        if let Some(inner) = rest.strip_prefix('{')
            && let Some(end) = inner.find('}')
        {
            let e = &inner[..end];
            let (target, default) = match e.split_once(":-") {
                Some((t, d)) => (t, Some(d)),
                None => (e, None),
            };
            let val = if let Some(slice) = target.strip_prefix("@:") {
                let mut p = slice.split(':');
                let from: usize = p.next().and_then(|x| x.parse().ok()).unwrap_or(1);
                let skip = from.saturating_sub(1).min(args.len());
                let take: usize = p.next().and_then(|x| x.parse().ok()).unwrap_or(usize::MAX);
                args.iter()
                    .skip(skip)
                    .take(take)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(" ")
            } else if target == "@" || target == "ARGUMENTS" {
                all.clone()
            } else if let Ok(n) = target.parse::<usize>() {
                arg(n)
            } else {
                out.push('$');
                i += 1;
                continue;
            };
            out.push_str(if val.is_empty() {
                default.unwrap_or("")
            } else {
                &val
            });
            i += 2 + end + 1;
            continue;
        }
        if rest.starts_with("ARGUMENTS") {
            out.push_str(&all);
            i += 1 + "ARGUMENTS".len();
        } else if rest.starts_with('@') {
            out.push_str(&all);
            i += 2;
        } else if let Some(d) = rest.chars().next().and_then(|c| c.to_digit(10))
            && d > 0
        {
            out.push_str(&arg(d as usize));
            i += 2;
        } else {
            out.push('$');
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn template_arguments() {
        let a = split_args(r#"Button "click handler" 'x y'"#);
        assert_eq!(a, ["Button", "click handler", "x y"]);
        assert_eq!(
            expand("make $1 with $@", &a),
            "make Button with Button click handler x y"
        );
        assert_eq!(expand("${2:-none} ${4:-none}", &a), "click handler none");
        assert_eq!(expand("${@:2}", &a), "click handler x y");
        assert_eq!(
            expand("${@:2:1}|$ARGUMENTS|$5|cost $$", &a),
            "click handler|Button click handler x y||cost $$"
        );
        assert_eq!(expand("${@:-all of it}", &[]), "all of it");
    }

    #[test]
    fn frontmatter_and_skills() {
        let (f, body) =
            frontmatter("---\nname: pdf\ndescription: \"Work with PDFs\"\n---\n# PDF\nsteps");
        assert_eq!(field(&f, "name"), Some("pdf"));
        assert_eq!(field(&f, "description"), Some("Work with PDFs"));
        assert_eq!(body, "# PDF\nsteps");
        let (f, body) = frontmatter("no frontmatter");
        assert!(f.is_empty());
        assert_eq!(body, "no frontmatter");
    }
}
