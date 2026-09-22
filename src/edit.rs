//! `tau edit FILE`: exact search/replace blocks on stdin. All blocks apply or none do.
//!
//! ```text
//! <<<<<<< SEARCH
//! old text
//! =======
//! new text
//! >>>>>>> REPLACE
//! ```

use anyhow::{Result, bail};
use std::fs;
use std::path::Path;

#[derive(Debug, PartialEq)]
pub struct Block {
    pub old: String,
    pub new: String,
}

pub fn parse(input: &str) -> Result<Vec<Block>> {
    enum S {
        Out,
        Old,
        New,
    }
    let mut st = S::Out;
    let mut blocks = vec![];
    let (mut old, mut new) = (vec![], vec![]);
    for line in input.lines() {
        match st {
            S::Out if line.starts_with("<<<<<<<") => st = S::Old,
            S::Out => {}
            S::Old if line.trim_end() == "=======" => st = S::New,
            S::Old => old.push(line),
            S::New if line.starts_with(">>>>>>>") => {
                blocks.push(Block {
                    old: old.join("\n"),
                    new: new.join("\n"),
                });
                old.clear();
                new.clear();
                st = S::Out;
            }
            S::New => new.push(line),
        }
    }
    if !matches!(st, S::Out) {
        bail!("unterminated block: each needs <<<<<<< SEARCH, =======, >>>>>>> REPLACE");
    }
    if blocks.is_empty() {
        bail!(
            "no blocks on stdin. Usage:\n  tau edit FILE <<'EOF'\n  <<<<<<< SEARCH\n  old\n  =======\n  new\n  >>>>>>> REPLACE\n  EOF"
        );
    }
    Ok(blocks)
}

fn line_of(s: &str, byte: usize) -> usize {
    s[..byte].matches('\n').count() + 1
}

/// Where the model probably meant, for the error message.
fn nearest(content: &str, old: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let want: Vec<&str> = old.lines().map(str::trim).collect();
    let n = want.len().max(1);
    // same text with different indentation is the most common miss
    for i in 0..lines.len().saturating_sub(n - 1) {
        if lines[i..i + n]
            .iter()
            .map(|l| l.trim())
            .eq(want.iter().copied())
        {
            return format!(
                "It matches at line {} if whitespace is ignored. Copy the exact indentation:\n{}",
                i + 1,
                lines[i..i + n].join("\n")
            );
        }
    }
    let first = want.iter().find(|l| !l.is_empty()).copied().unwrap_or("");
    let score = |l: &str| -> usize {
        let l = l.trim();
        let common = l
            .chars()
            .zip(first.chars())
            .take_while(|(a, b)| a == b)
            .count();
        common * 2 + first.split_whitespace().filter(|w| l.contains(w)).count()
    };
    match (0..lines.len()).max_by_key(|&i| score(lines[i])) {
        Some(i) if score(lines[i]) > 0 => {
            let lo = i.saturating_sub(2);
            let hi = (i + n + 2).min(lines.len());
            let ctx: Vec<String> = (lo..hi)
                .map(|j| format!("{:>5}  {}", j + 1, lines[j]))
                .collect();
            format!("Closest is near line {}:\n{}", i + 1, ctx.join("\n"))
        }
        _ => "Nothing close. Read the file first.".into(),
    }
}

/// Applies blocks in order to `content`. Returns the new content and a diff.
pub fn apply(content: &str, blocks: &[Block]) -> Result<(String, String)> {
    let mut cur = content.to_string();
    let mut diff = String::new();
    for (k, b) in blocks.iter().enumerate() {
        let which = if blocks.len() > 1 {
            format!("block {}: ", k + 1)
        } else {
            String::new()
        };
        if b.old.is_empty() {
            if !cur.is_empty() {
                bail!("{which}empty SEARCH only works on an empty or new file");
            }
            cur = b.new.clone();
            if !cur.is_empty() && !cur.ends_with('\n') {
                cur.push('\n');
            }
            for l in b.new.lines() {
                diff.push_str(&format!("+{l}\n"));
            }
            continue;
        }
        let hits: Vec<usize> = cur.match_indices(&b.old).map(|(i, _)| i).collect();
        match hits.len() {
            0 => bail!("{which}SEARCH text not found. {}", nearest(&cur, &b.old)),
            1 => {}
            n => {
                let at: Vec<String> = hits.iter().map(|&i| line_of(&cur, i).to_string()).collect();
                bail!(
                    "{which}SEARCH text matches {n} times (lines {}). Include more surrounding lines so it is unique.",
                    at.join(", ")
                );
            }
        }
        let at = hits[0];
        diff.push_str(&format!("@@ line {} @@\n", line_of(&cur, at)));
        for l in b.old.lines() {
            diff.push_str(&format!("-{l}\n"));
        }
        for l in b.new.lines() {
            diff.push_str(&format!("+{l}\n"));
        }
        cur.replace_range(at..at + b.old.len(), &b.new);
    }
    Ok((cur, diff))
}

pub fn run(path: &Path, input: &str) -> Result<String> {
    let blocks = parse(input)?;
    let content = if path.exists() {
        fs::read_to_string(path)?
    } else if blocks.len() == 1 && blocks[0].old.is_empty() {
        if let Some(p) = path.parent()
            && !p.as_os_str().is_empty()
        {
            fs::create_dir_all(p)?;
        }
        String::new()
    } else {
        bail!("{} does not exist", path.display());
    };
    let (new, diff) = apply(&content, &blocks)?;
    fs::write(path, new)?;
    Ok(format!("{}\n{diff}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blk(old: &str, new: &str) -> String {
        format!("<<<<<<< SEARCH\n{old}\n=======\n{new}\n>>>>>>> REPLACE\n")
    }

    #[test]
    fn replaces_unique_match() {
        let (out, diff) = apply(
            "fn a() {\n    1\n}\n",
            &parse(&blk("    1", "    2")).unwrap(),
        )
        .unwrap();
        assert_eq!(out, "fn a() {\n    2\n}\n");
        assert!(diff.contains("-    1") && diff.contains("+    2"));
        assert!(diff.contains("@@ line 2 @@"));
    }

    #[test]
    fn rejects_ambiguous_match() {
        let e = apply("x\ny\nx\n", &parse(&blk("x", "z")).unwrap())
            .unwrap_err()
            .to_string();
        assert!(e.contains("matches 2 times"), "{e}");
        assert!(e.contains("lines 1, 3"), "{e}");
    }

    #[test]
    fn miss_points_at_whitespace_difference() {
        let e = apply(
            "fn a() {\n    let x = 1;\n}\n",
            &parse(&blk("let x = 1;", "let x = 2;")).unwrap(),
        );
        // "let x = 1;" is a substring, so this one actually matches
        assert!(e.is_ok());
        let e = apply(
            "fn a() {\n    let x = 1;\n    let y = 2;\n}\n",
            &parse(&blk("  let x = 1;\n  let y = 2;", "")).unwrap(),
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("line 2 if whitespace is ignored"), "{e}");
    }

    #[test]
    fn miss_suggests_nearest_line() {
        let e = apply(
            "alpha\nfn compute_total(items) {\nomega\n",
            &parse(&blk("fn compute_totals(items) {", "x")).unwrap(),
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("near line 2"), "{e}");
    }

    #[test]
    fn all_or_nothing() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("f.txt");
        fs::write(&p, "a\nb\n").unwrap();
        let input = format!("{}{}", blk("a", "A"), blk("missing", "M"));
        assert!(run(&p, &input).is_err());
        assert_eq!(fs::read_to_string(&p).unwrap(), "a\nb\n");
        let input = format!("{}{}", blk("a", "A"), blk("b", "B"));
        run(&p, &input).unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "A\nB\n");
    }

    #[test]
    fn empty_search_creates_file() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("new/dir/f.txt");
        run(&p, "<<<<<<< SEARCH\n=======\nhello\n>>>>>>> REPLACE\n").unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap(), "hello\n");
    }
}
