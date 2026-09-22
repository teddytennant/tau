//! The whole system prompt. It is short on purpose.

use std::path::{Path, PathBuf};

pub const PROMPT: &str = r#"You are tau, a coding agent in a terminal. You have one tool, bash, and it is enough: read with cat, sed -n and rg, write files with heredocs, run the tests, use git.

To change part of a file, use tau edit with exact text:
tau edit path/to/file <<'EOF'
<<<<<<< SEARCH
exact old lines
=======
new lines
>>>>>>> REPLACE
EOF

A command that outlives its timeout keeps running as a job: tau job list, wait, tail or kill.

Make your own tools. An executable you put in ~/.tau/bin is on PATH from then on, in every session. Executables in ~/.tau/panels print into a side panel of the UI, and ones in ~/.tau/commands become /slash commands. Write one when you notice you are repeating yourself.

You can run copies of yourself. tau -p "task" runs one and prints its answer. Add --from $TAU_NODE to give it everything you know so far. Start several with & and wait for them. Use copies for independent pieces of work.

Read before you edit. Check your work by running it. When you are done, say what changed and how you know it works, briefly."#;

/// AGENTS.md (or CLAUDE.md where there is no AGENTS.md) from ~/.tau, then
/// from the filesystem root down to `cwd`.
pub fn context_files(cwd: &Path) -> Vec<PathBuf> {
    let mut out = vec![];
    let global = crate::log::tau_home().join("AGENTS.md");
    if global.is_file() {
        out.push(global);
    }
    let mut dirs: Vec<&Path> = cwd.ancestors().collect();
    dirs.reverse();
    for d in dirs {
        for name in ["AGENTS.md", "CLAUDE.md"] {
            let p = d.join(name);
            if p.is_file() {
                out.push(p);
                break;
            }
        }
    }
    out
}

pub fn build(cwd: &Path) -> String {
    let mut s = String::from(PROMPT);
    s.push_str(&format!("\n\nWorking directory: {}", cwd.display()));
    for p in context_files(cwd) {
        if let Ok(t) = std::fs::read_to_string(&p)
            && !t.trim().is_empty()
        {
            s.push_str(&format!("\n\n{}:\n{}", p.display(), t.trim()));
        }
    }
    let settings = crate::config::Settings::load();
    s.push_str(&crate::skills::prompt_section(&crate::skills::discover(
        &settings.skill_dirs,
    )));
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_files_come_from_every_parent() {
        let d = tempfile::tempdir().unwrap();
        let deep = d.path().join("a/b");
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::write(d.path().join("AGENTS.md"), "top").unwrap();
        std::fs::write(d.path().join("a/CLAUDE.md"), "middle").unwrap();
        std::fs::write(deep.join("AGENTS.md"), "here").unwrap();
        std::fs::write(deep.join("CLAUDE.md"), "ignored, AGENTS.md wins").unwrap();
        let found: Vec<PathBuf> = context_files(&deep)
            .into_iter()
            .filter(|p| p.starts_with(d.path()))
            .collect();
        assert_eq!(
            found,
            [
                d.path().join("AGENTS.md"),
                d.path().join("a/CLAUDE.md"),
                deep.join("AGENTS.md")
            ]
        );
    }
}
