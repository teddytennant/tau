//! `@file` references: a fuzzy file finder for the editor, and inlining of
//! referenced files when a message is sent.

use std::path::Path;
use std::process::Command;

/// Files under `cwd`, from git when it is a repository.
pub fn list(cwd: &Path) -> Vec<String> {
    if let Ok(o) = Command::new("git")
        .args(["ls-files", "--cached", "--others", "--exclude-standard"])
        .current_dir(cwd)
        .output()
        && o.status.success()
    {
        let mut v: Vec<String> = String::from_utf8_lossy(&o.stdout)
            .lines()
            .map(String::from)
            .collect();
        v.sort();
        v.dedup();
        return v;
    }
    let mut out = vec![];
    walk(cwd, cwd, &mut out);
    out.sort();
    out
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
    if out.len() > 20_000 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || name == "target" || name == "node_modules" {
            continue;
        }
        let p = e.path();
        if p.is_dir() {
            walk(root, &p, out);
        } else if let Ok(rel) = p.strip_prefix(root) {
            out.push(rel.display().to_string());
        }
    }
}

/// Subsequence match score, higher is better. None when `q` does not match.
pub fn score(path: &str, q: &str) -> Option<i64> {
    if q.is_empty() {
        return Some(-(path.len() as i64));
    }
    let p: Vec<char> = path.to_lowercase().chars().collect();
    let mut s = 0i64;
    let mut last: Option<usize> = None;
    let mut i = 0;
    for qc in q.to_lowercase().chars() {
        while i < p.len() && p[i] != qc {
            i += 1;
        }
        if i == p.len() {
            return None;
        }
        s += 10;
        if last == Some(i.wrapping_sub(1)) {
            s += 15;
        }
        if i == 0 || matches!(p[i - 1], '/' | '_' | '-' | '.') {
            s += 10;
        }
        last = Some(i);
        i += 1;
    }
    // prefer matches in the file name and shorter paths
    let base = path.rsplit('/').next().unwrap_or(path).to_lowercase();
    if base.contains(&q.to_lowercase()) {
        s += 40;
    }
    Some(s * 100 - path.len() as i64)
}

pub fn best(files: &[String], q: &str, n: usize) -> Vec<String> {
    let mut v: Vec<(i64, &String)> = files
        .iter()
        .filter_map(|f| score(f, q).map(|s| (s, f)))
        .collect();
    v.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(b.1)));
    v.into_iter().take(n).map(|(_, f)| f.clone()).collect()
}

/// The `@word` being typed at the end of `input`, if any.
pub fn at_query(input: &str) -> Option<&str> {
    let start = input.rfind('@')?;
    let q = &input[start + 1..];
    let before_ok = start == 0 || input[..start].ends_with(char::is_whitespace);
    (before_ok && !q.contains(char::is_whitespace)).then_some(q)
}

const MAX_INLINE: usize = 200_000;

/// Appends the contents of every `@path` in `text` that exists. Images are
/// returned separately so the provider can see them.
pub fn inline(text: &str, cwd: &Path) -> (String, Vec<std::path::PathBuf>) {
    let mut out = text.to_string();
    let mut images = vec![];
    let mut seen = std::collections::HashSet::new();
    for tok in text.split_whitespace() {
        let Some(raw) = tok.strip_prefix('@') else {
            continue;
        };
        let rel = raw.trim_end_matches([',', '.', ':', ';', ')', '?', '!']);
        if rel.is_empty() || !seen.insert(rel.to_string()) {
            continue;
        }
        let p = crate::skills::expand_home(rel);
        let p = if p.is_absolute() { p } else { cwd.join(p) };
        if p.is_dir() {
            let mut names: Vec<String> = std::fs::read_dir(&p)
                .map(|r| {
                    r.flatten()
                        .map(|e| {
                            let n = e.file_name().to_string_lossy().to_string();
                            if e.path().is_dir() { n + "/" } else { n }
                        })
                        .collect()
                })
                .unwrap_or_default();
            names.sort();
            names.truncate(300);
            out.push_str(&format!(
                "\n\n<dir path=\"{rel}\">\n{}\n</dir>",
                names.join("\n")
            ));
        } else if p.is_file() {
            if crate::log::media_type(rel).is_some() {
                images.push(p);
                continue;
            }
            let Ok(bytes) = std::fs::read(&p) else {
                continue;
            };
            if bytes.contains(&0) {
                out.push_str(&format!(
                    "\n\n<file path=\"{rel}\">(binary, {} bytes)</file>",
                    bytes.len()
                ));
                continue;
            }
            let mut body = String::from_utf8_lossy(&bytes).into_owned();
            if body.len() > MAX_INLINE {
                let mut cut = MAX_INLINE;
                while !body.is_char_boundary(cut) {
                    cut -= 1;
                }
                body.truncate(cut);
                body.push_str(&format!(
                    "\n[... cut at {MAX_INLINE} bytes of {}]",
                    bytes.len()
                ));
            }
            out.push_str(&format!(
                "\n\n<file path=\"{rel}\">\n{}\n</file>",
                body.trim_end()
            ));
        }
    }
    (out, images)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_prefers_file_names_and_short_paths() {
        let files: Vec<String> = [
            "src/main.rs",
            "src/tui.rs",
            "docs/maintenance.md",
            "tests/e2e.rs",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(best(&files, "main", 2)[0], "src/main.rs");
        assert_eq!(best(&files, "tui", 1), ["src/tui.rs"]);
        assert!(best(&files, "zzz", 5).is_empty());
        assert_eq!(best(&files, "e2e", 1), ["tests/e2e.rs"]);
    }

    #[test]
    fn at_query_only_after_whitespace() {
        assert_eq!(at_query("look at @src/ma"), Some("src/ma"));
        assert_eq!(at_query("@"), Some(""));
        assert_eq!(at_query("mail me@home"), None);
        assert_eq!(at_query("@a.rs and more"), None);
    }

    #[test]
    fn inlines_files_and_separates_images() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.txt"), "hello\n").unwrap();
        std::fs::write(d.path().join("pic.png"), [137, 80, 78, 71]).unwrap();
        let (t, imgs) = inline("check @a.txt, and @pic.png and @missing.rs", d.path());
        assert!(t.starts_with("check @a.txt, and"));
        assert!(t.contains("<file path=\"a.txt\">\nhello\n</file>"));
        assert!(!t.contains("missing.rs\">"));
        assert_eq!(imgs, [d.path().join("pic.png")]);
    }
}
