//! End to end runs of the real binary against the mock provider.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

fn tau(home: &Path, mock: &str, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tau"))
        .args(args)
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap())
        .env("HOME", home)
        .env("TAU_HOME", home.join(".tau"))
        .env("TAU_PROVIDER", "mock")
        .env("TAU_MOCK", mock)
        .current_dir(home)
        .output()
        .unwrap()
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn sessions(home: &Path) -> Vec<String> {
    let mut v: Vec<String> = fs::read_dir(home.join(".tau/sessions"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    v.sort();
    v
}

fn events(home: &Path, id: &str) -> String {
    fs::read_to_string(home.join(".tau/sessions").join(id).join("events.jsonl")).unwrap()
}

#[test]
fn print_mode_spawns_a_child_through_bash() {
    let d = tempfile::tempdir().unwrap();
    let script = "asking a copy\n$ TAU_MOCK='the answer is 42' tau -p 'what is the answer'\n---\nthe copy said 42";
    let o = tau(d.path(), script, &["-p", "find the answer"]);
    assert!(o.status.success(), "stderr: {}", text(&o.stderr));
    assert_eq!(text(&o.stdout).trim(), "the copy said 42");
    assert!(text(&o.stderr).contains("$ TAU_MOCK="));

    let ids = sessions(d.path());
    assert_eq!(ids.len(), 2, "{ids:?}");
    let parent = ids
        .iter()
        .find(|id| events(d.path(), id).contains("find the answer"))
        .unwrap();
    assert!(events(d.path(), parent).contains("the answer is 42"));

    // the child records who started it and how deep it is
    let child = ids.iter().find(|id| *id != parent).unwrap();
    let meta =
        fs::read_to_string(d.path().join(".tau/sessions").join(child).join("meta.json")).unwrap();
    assert!(
        meta.contains(&format!("\"parent\": \"{parent}\"")),
        "{meta}"
    );
    assert!(meta.contains("\"depth\": 1"), "{meta}");
}

#[test]
fn from_node_copies_the_parent_prefix_byte_for_byte() {
    let d = tempfile::tempdir().unwrap();
    let script = "forking\n$ TAU_MOCK='fork ok' tau -p --from \"$TAU_NODE\" 'carry on'\n---\ndone";
    let o = tau(d.path(), script, &["-p", "start"]);
    assert!(o.status.success(), "stderr: {}", text(&o.stderr));

    let ids = sessions(d.path());
    let meta = |id: &str| {
        fs::read_to_string(d.path().join(".tau/sessions").join(id).join("meta.json")).unwrap()
    };
    let (child, parent): (Vec<&String>, Vec<&String>) =
        ids.iter().partition(|id| meta(id).contains("\"parent\""));
    let (p, c) = (events(d.path(), parent[0]), events(d.path(), child[0]));
    // the child's log starts with the parent's first three lines, unchanged:
    // system, user, and the assistant turn that ran the fork
    let prefix: String = p.lines().take(3).map(|l| format!("{l}\n")).collect();
    assert!(c.starts_with(&prefix), "parent:\n{p}\nchild:\n{c}");
    assert!(c.contains("You are a copy"));
    assert!(c.contains("fork ok"));
    let meta = meta(child[0]);
    assert!(
        meta.contains(&format!("\"parent\": \"{}\"", parent[0])),
        "{meta}"
    );
    assert!(meta.contains("\"from\": 2"), "{meta}");
}

#[test]
fn depth_limit_refuses_to_start() {
    let d = tempfile::tempdir().unwrap();
    let o = Command::new(env!("CARGO_BIN_EXE_tau"))
        .args(["-p", "hi"])
        .env("TAU_HOME", d.path().join(".tau"))
        .env("TAU_PROVIDER", "mock")
        .env("TAU_DEPTH", "4")
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(1));
    assert!(text(&o.stderr).contains("too deep"));
}

#[test]
fn tools_in_tau_bin_are_on_path() {
    let d = tempfile::tempdir().unwrap();
    let bin = d.path().join(".tau/bin");
    fs::create_dir_all(&bin).unwrap();
    let script = "$ printf '#!/bin/sh\\necho made by me\\n' > ~/.tau/bin/mytool && chmod +x ~/.tau/bin/mytool\n---\n$ mytool\n---\nok";
    let o = Command::new(env!("CARGO_BIN_EXE_tau"))
        .args(["-p", "make a tool"])
        .env("HOME", d.path())
        .env("TAU_PROVIDER", "mock")
        .env("TAU_MOCK", script)
        .current_dir(d.path())
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", text(&o.stderr));
    let ids = sessions(d.path());
    assert!(events(d.path(), &ids[0]).contains("made by me"));
}

#[test]
fn edit_and_continue() {
    let d = tempfile::tempdir().unwrap();
    fs::write(d.path().join("a.txt"), "hello world\n").unwrap();
    let script = "$ tau edit a.txt <<'EOF'\n> <<<<<<< SEARCH\n> hello world\n> =======\n> hello tau\n> >>>>>>> REPLACE\n> EOF\n---\nedited";
    let o = tau(d.path(), script, &["-p", "edit it"]);
    assert!(o.status.success(), "{}", text(&o.stderr));
    assert_eq!(
        fs::read_to_string(d.path().join("a.txt")).unwrap(),
        "hello tau\n"
    );
    let o = tau(d.path(), "continued", &["-p", "-c", "and again"]);
    assert!(o.status.success(), "{}", text(&o.stderr));
    assert_eq!(sessions(d.path()).len(), 1);
    assert!(events(d.path(), &sessions(d.path())[0]).contains("and again"));
}

#[test]
fn compacts_automatically_near_the_window() {
    let d = tempfile::tempdir().unwrap();
    let home = d.path().join(".tau");
    fs::create_dir_all(&home).unwrap();
    // keep ~1000 tokens after the summary, cut at a user message
    fs::write(home.join("settings.json"), r#"{"compact_keep": 1000}"#).unwrap();
    let big = "$ head -c 6000 /dev/zero | tr '\\0' x";
    let script = format!("{big}\n---\n{big}\n---\ndone");
    let run = |args: &[&str], script: &str| {
        Command::new(env!("CARGO_BIN_EXE_tau"))
            .args(args)
            .env("TAU_HOME", &home)
            .env("TAU_PROVIDER", "mock")
            .env("TAU_MOCK", script)
            .env("TAU_CONTEXT_WINDOW", "3000")
            .current_dir(d.path())
            .output()
            .unwrap()
    };
    let o = run(&["-p", "first"], &script);
    assert!(o.status.success(), "{}", text(&o.stderr));
    assert!(
        text(&o.stderr).contains("compacting the history"),
        "{}",
        text(&o.stderr)
    );
    let id = &sessions(d.path())[0];
    let ev = events(d.path(), id);
    assert!(ev.contains("\"kind\":\"summary\""), "{ev}");
    assert!(ev.contains("mock summary of"), "{ev}");
    assert_eq!(text(&o.stdout).trim(), "done");

    // a second message keeps the recent turn word for word after a new summary
    let o = run(
        &["-p", "-c", "second"],
        &format!("{big}\n---\n{big}\n---\nagain"),
    );
    assert!(o.status.success(), "{}", text(&o.stderr));
    let ev = events(d.path(), id);
    assert!(ev.contains("\"keep_from\""), "{ev}");
}

#[test]
fn several_calls_in_one_turn_run_at_once() {
    let d = tempfile::tempdir().unwrap();
    let script = "$ sleep 1; echo one\n$ sleep 1; echo two\n$ sleep 1; echo three\n---\nok";
    let t = std::time::Instant::now();
    let o = tau(d.path(), script, &["-p", "go"]);
    let took = t.elapsed();
    assert!(o.status.success(), "{}", text(&o.stderr));
    assert!(
        took < std::time::Duration::from_millis(2500),
        "took {took:?}"
    );
    // results are logged in call order whatever order they finished in
    let ev = events(d.path(), &sessions(d.path())[0]);
    let (a, b, c) = (
        ev.find("\"output\":\"one").unwrap(),
        ev.find("\"output\":\"two").unwrap(),
        ev.find("\"output\":\"three").unwrap(),
    );
    assert!(a < b && b < c);
}

#[test]
fn export_writes_html() {
    let d = tempfile::tempdir().unwrap();
    let o = tau(
        d.path(),
        "$ echo '<b>hi</b>'\n---\nall <done>",
        &["-p", "say hi"],
    );
    assert!(o.status.success(), "{}", text(&o.stderr));
    let out = d.path().join("s.html");
    let o = tau(d.path(), "", &["export", "-o", out.to_str().unwrap()]);
    assert!(o.status.success(), "{}", text(&o.stderr));
    let h = fs::read_to_string(&out).unwrap();
    assert!(h.contains("say hi"));
    assert!(h.contains("&lt;b&gt;hi&lt;/b&gt;"));
    assert!(h.contains("all &lt;done&gt;"));
}

#[test]
fn no_key_without_a_terminal_says_how_to_log_in() {
    let d = tempfile::tempdir().unwrap();
    let o = Command::new(env!("CARGO_BIN_EXE_tau"))
        .args(["-p", "hi"])
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap())
        .env("HOME", d.path())
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(1));
    assert!(text(&o.stderr).contains("tau login"), "{}", text(&o.stderr));
}

#[test]
fn saved_key_is_used_and_env_wins() {
    let d = tempfile::tempdir().unwrap();
    let home = d.path().join(".tau");
    fs::create_dir_all(&home).unwrap();
    // a custom server that is not there: the request fails, which shows
    // which URL and provider were picked without a real key
    fs::write(
        home.join("auth.json"),
        r#"{"custom": {"key": "saved", "base_url": "http://127.0.0.1:9/v1"}}"#,
    )
    .unwrap();
    fs::write(
        home.join("settings.json"),
        r#"{"provider": "custom", "models": {"custom": "m1"}}"#,
    )
    .unwrap();
    let o = Command::new(env!("CARGO_BIN_EXE_tau"))
        .args(["models"])
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap())
        .env("HOME", d.path())
        .output()
        .unwrap();
    assert!(!o.status.success());
    assert!(
        text(&o.stderr).contains("127.0.0.1:9"),
        "{}",
        text(&o.stderr)
    );
    let o = Command::new(env!("CARGO_BIN_EXE_tau"))
        .args(["models"])
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap())
        .env("HOME", d.path())
        .env("OPENAI_BASE_URL", "http://127.0.0.1:7/v1")
        .output()
        .unwrap();
    assert!(
        text(&o.stderr).contains("127.0.0.1:7"),
        "{}",
        text(&o.stderr)
    );
}
