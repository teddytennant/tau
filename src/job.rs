//! `tau job`: commands that outlived their timeout.

use crate::exec::{Job, bound, jobs_dir, kill_group, read_lossy, tail_file};
use crate::log::{Session, now, session_dir};
use anyhow::{Context, Result, bail};
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn dir() -> Result<PathBuf> {
    let id = match std::env::var("TAU_SESSION") {
        Ok(s) if !s.is_empty() => s,
        _ => {
            let cwd = std::env::current_dir()?.display().to_string();
            Session::latest(Some(&cwd)).context("no session; run this from inside tau")?
        }
    };
    Ok(jobs_dir(&session_dir(&id)))
}

fn load(id: u32) -> Result<Job> {
    let p = dir()?.join(format!("{id}.json"));
    let s = fs::read_to_string(&p).with_context(|| format!("no job {id}"))?;
    Ok(serde_json::from_str(&s)?)
}

enum State {
    Running,
    Exited(i32),
    Lost,
}

fn state(j: &Job) -> State {
    if let Ok(s) = fs::read_to_string(&j.status)
        && let Ok(c) = s.trim().parse()
    {
        return State::Exited(c);
    }
    if unsafe { libc::kill(j.pgid, 0) } == 0 {
        State::Running
    } else {
        State::Lost
    }
}

fn describe(s: &State) -> String {
    match s {
        State::Running => "running".into(),
        State::Exited(c) => format!("exit {c}"),
        State::Lost => "gone (killed?)".into(),
    }
}

pub fn list() -> Result<()> {
    let d = dir()?;
    let mut jobs: Vec<Job> = fs::read_dir(&d)
        .map(|r| {
            r.flatten()
                .filter_map(|e| fs::read_to_string(e.path()).ok())
                .filter_map(|s| serde_json::from_str(&s).ok())
                .collect()
        })
        .unwrap_or_default();
    jobs.sort_by_key(|j| j.id);
    if jobs.is_empty() {
        println!("no jobs");
    }
    for j in jobs {
        println!(
            "{:>3}  {:<14} {:>5}s  {}",
            j.id,
            describe(&state(&j)),
            now().saturating_sub(j.started),
            j.command.lines().next().unwrap_or("")
        );
    }
    Ok(())
}

pub fn wait(id: u32, timeout: u64) -> Result<i32> {
    let j = load(id)?;
    let deadline = Instant::now() + Duration::from_secs(timeout);
    loop {
        match state(&j) {
            State::Running if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(100))
            }
            State::Running => {
                println!("{}", tail_file(&j.log, 20));
                println!("[job {id} still running after another {timeout}s]");
                return Ok(0);
            }
            s => {
                println!("{}", bound(&read_lossy(&j.log), &j.log));
                println!("[job {id}: {}]", describe(&s));
                return Ok(match s {
                    State::Exited(c) => c,
                    _ => 1,
                });
            }
        }
    }
}

pub fn tail(id: u32, n: usize) -> Result<()> {
    let j = load(id)?;
    println!("{}", tail_file(&j.log, n));
    println!("[job {id}: {}]", describe(&state(&j)));
    Ok(())
}

pub fn kill(id: u32) -> Result<()> {
    let j = load(id)?;
    if !matches!(state(&j), State::Running) {
        bail!("job {id} is not running");
    }
    kill_group(j.pgid, libc::SIGTERM);
    std::thread::sleep(Duration::from_millis(300));
    kill_group(j.pgid, libc::SIGKILL);
    println!("killed job {id}");
    Ok(())
}
