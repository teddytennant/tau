//! The one tool. Runs a command in its own process group, bounds what comes
//! back, and turns a command that outlives its timeout into a job.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

/// Set by Ctrl-C. Checked by the command wait loop and the stream reader.
pub static CANCEL: AtomicBool = AtomicBool::new(false);
/// Process groups of the commands running in the foreground.
static FOREGROUND: Mutex<Option<HashSet<i32>>> = Mutex::new(None);

fn foreground(add: Option<i32>, remove: Option<i32>) -> Vec<i32> {
    let mut g = FOREGROUND.lock().unwrap_or_else(|e| e.into_inner());
    let set = g.get_or_insert_with(HashSet::new);
    if let Some(a) = add {
        set.insert(a);
    }
    if let Some(r) = remove {
        set.remove(&r);
    }
    set.iter().copied().collect()
}

pub const HEAD_LINES: usize = 60;
pub const TAIL_LINES: usize = 60;
pub const MAX_BYTES: usize = 16_000;
pub const DEFAULT_TIMEOUT: u64 = 120;

pub fn cancelled() -> bool {
    CANCEL.load(Ordering::SeqCst)
}

pub fn kill_group(pgid: i32, sig: i32) {
    if pgid > 0 {
        unsafe {
            libc::kill(-pgid, sig);
        }
    }
}

/// Kill every foreground command's whole group. Called from the Ctrl-C handler thread.
pub fn interrupt() {
    CANCEL.store(true, Ordering::SeqCst);
    let groups = foreground(None, None);
    for &pg in &groups {
        kill_group(pg, libc::SIGTERM);
    }
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(500));
        for pg in groups {
            kill_group(pg, libc::SIGKILL);
        }
    });
}

pub struct Outcome {
    /// What the model sees.
    pub text: String,
    pub code: Option<i32>,
    pub lines: usize,
}

/// Keep the head and tail of long output. The full text stays at `full`.
pub fn bound(out: &str, full: &Path) -> String {
    let lines: Vec<&str> = out.lines().collect();
    let over_lines = lines.len() > HEAD_LINES + TAIL_LINES;
    if !over_lines && out.len() <= MAX_BYTES {
        return out.to_string();
    }
    let (head, tail): (Vec<&str>, Vec<&str>) = if over_lines {
        (
            lines[..HEAD_LINES].to_vec(),
            lines[lines.len() - TAIL_LINES..].to_vec(),
        )
    } else {
        let n = lines.len() / 2;
        (lines[..n].to_vec(), lines[n..].to_vec())
    };
    let clip = |v: Vec<&str>, budget: usize, from_end: bool| -> String {
        let s = v.join("\n");
        if s.len() <= budget {
            return s;
        }
        if from_end {
            let mut i = s.len() - budget;
            while !s.is_char_boundary(i) {
                i += 1;
            }
            s[i..].to_string()
        } else {
            let mut i = budget;
            while !s.is_char_boundary(i) {
                i -= 1;
            }
            s[..i].to_string()
        }
    };
    let h = clip(head, MAX_BYTES / 2, false);
    let t = clip(tail, MAX_BYTES / 2, true);
    format!(
        "{h}\n[... {} lines, {} bytes total. Full output: {} ...]\n{t}",
        lines.len(),
        out.len(),
        full.display()
    )
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Job {
    pub id: u32,
    pub pgid: i32,
    pub command: String,
    pub log: PathBuf,
    pub status: PathBuf,
    pub started: u64,
}

pub fn jobs_dir(session_dir: &Path) -> PathBuf {
    session_dir.join("jobs")
}

pub struct Exec {
    pub session_dir: PathBuf,
    pub env: Vec<(String, String)>,
    counter: AtomicU32,
}

impl Exec {
    pub fn new(session_dir: PathBuf, env: Vec<(String, String)>) -> Exec {
        let counter = fs::read_dir(session_dir.join("out"))
            .map(|r| r.count() as u32)
            .unwrap_or(0);
        Exec {
            session_dir,
            env,
            counter: AtomicU32::new(counter),
        }
    }

    /// Runs several commands at once. `on_done` is called on this thread as
    /// each one finishes; results come back in call order.
    pub fn run_all(
        &self,
        calls: &[(String, u64)],
        mut on_done: impl FnMut(usize, &Outcome),
    ) -> Vec<Result<Outcome>> {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut out: Vec<Option<Result<Outcome>>> = (0..calls.len()).map(|_| None).collect();
        std::thread::scope(|s| {
            for (i, (c, t)) in calls.iter().enumerate() {
                let tx = tx.clone();
                s.spawn(move || {
                    let _ = tx.send((i, self.run(c, *t)));
                });
            }
            drop(tx);
            for (i, r) in rx {
                if let Ok(o) = &r {
                    on_done(i, o);
                }
                out[i] = Some(r);
            }
        });
        out.into_iter()
            .map(|r| r.unwrap_or_else(|| Err(anyhow::anyhow!("command thread panicked"))))
            .collect()
    }

    pub fn run(&self, command: &str, timeout: u64) -> Result<Outcome> {
        let n = self.counter.fetch_add(1, Ordering::SeqCst) + 1;
        let out_dir = self.session_dir.join("out");
        fs::create_dir_all(&out_dir)?;
        let log = out_dir.join(format!("{n}.log"));
        let status = out_dir.join(format!("{n}.status"));
        let file = File::create(&log)?;
        // The wrapper records the exit code, so a detached job still reports it
        // after this process has gone.
        let mut child = Command::new("bash")
            .arg("-c")
            .arg(r#"bash -c "$1"; echo $? > "$2""#)
            .arg("tau")
            .arg(command)
            .arg(&status)
            .stdin(Stdio::null())
            .stdout(file.try_clone()?)
            .stderr(file)
            .envs(self.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .process_group(0)
            .spawn()?;
        let pgid = child.id() as i32;
        foreground(Some(pgid), None);
        let start = Instant::now();
        let limit = Duration::from_secs(timeout.max(1));
        let mut interrupted = false;
        let finished = loop {
            if child.try_wait()?.is_some() {
                break true;
            }
            if cancelled() {
                kill_group(pgid, libc::SIGKILL);
                let _ = child.wait();
                interrupted = true;
                break true;
            }
            if start.elapsed() >= limit {
                break false;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        foreground(None, Some(pgid));
        // Ctrl-C may kill the group before this loop sees the flag
        interrupted |= cancelled();
        let out = read_lossy(&log);
        let bounded = bound(&out, &log);
        let lines = out.lines().count();
        if finished {
            let code = if interrupted {
                Some(130)
            } else {
                fs::read_to_string(&status)
                    .ok()
                    .and_then(|s| s.trim().parse().ok())
            };
            let mut text = bounded;
            if interrupted {
                text.push_str("\n[interrupted by the user]");
            } else if let Some(c) = code
                && c != 0
            {
                text.push_str(&format!("\n[exit {c}]"));
            }
            return Ok(Outcome { text, code, lines });
        }
        // Still running: hand it back as a job instead of killing it.
        let jobs = jobs_dir(&self.session_dir);
        fs::create_dir_all(&jobs)?;
        let id = next_job_id(&jobs);
        let job = Job {
            id,
            pgid,
            command: command.to_string(),
            log: log.clone(),
            status,
            started: crate::log::now(),
        };
        fs::write(
            jobs.join(format!("{id}.json")),
            serde_json::to_string(&job)?,
        )?;
        // reap it when it ends so it does not linger as a zombie
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        let text = format!(
            "{bounded}\n[still running after {timeout}s, now job {id}. `tau job wait {id}`, `tau job tail {id}` or `tau job kill {id}`]"
        );
        Ok(Outcome {
            text,
            code: None,
            lines,
        })
    }
}

fn next_job_id(dir: &Path) -> u32 {
    fs::read_dir(dir)
        .map(|r| {
            r.flatten()
                .filter_map(|e| {
                    e.path()
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .and_then(|s| s.parse::<u32>().ok())
                })
                .max()
                .unwrap_or(0)
        })
        .unwrap_or(0)
        + 1
}

pub fn read_lossy(p: &Path) -> String {
    let mut b = vec![];
    if let Ok(mut f) = File::open(p) {
        let _ = f.read_to_end(&mut b);
    }
    String::from_utf8_lossy(&b).into_owned()
}

/// Last `n` lines of a file without reading all of a huge log.
pub fn tail_file(p: &Path, n: usize) -> String {
    let Ok(mut f) = File::open(p) else {
        return String::new();
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let want = (n as u64 * 400).max(64 * 1024).min(len);
    let _ = f.seek(SeekFrom::Start(len - want));
    let mut b = vec![];
    let _ = f.read_to_end(&mut b);
    let s = String::from_utf8_lossy(&b);
    let lines: Vec<&str> = s.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_output_is_untouched() {
        let s = "a\nb\nc";
        assert_eq!(bound(s, Path::new("/x")), s);
    }

    #[test]
    fn long_output_keeps_head_and_tail() {
        let s: String = (0..1000).map(|i| format!("line {i}\n")).collect();
        let b = bound(&s, Path::new("/tmp/full.log"));
        assert!(b.starts_with("line 0\n"));
        assert!(b.contains("line 59\n"));
        assert!(!b.contains("line 60\n"));
        assert!(b.contains("line 999"));
        assert!(b.contains("1000 lines"));
        assert!(b.contains("/tmp/full.log"));
        assert!(b.len() < MAX_BYTES + 200);
    }

    #[test]
    fn one_giant_line_is_clipped() {
        let s = "é".repeat(50_000);
        let b = bound(&s, Path::new("/f"));
        assert!(b.len() < MAX_BYTES + 200);
    }

    #[test]
    fn runs_and_reports_exit_code() {
        let d = tempfile::tempdir().unwrap();
        let ex = Exec::new(d.path().to_path_buf(), vec![]);
        let o = ex.run("echo hi; echo err >&2; exit 3", 10).unwrap();
        assert_eq!(o.code, Some(3));
        assert!(o.text.contains("hi"));
        assert!(o.text.contains("err"));
        assert!(o.text.contains("[exit 3]"));
    }

    #[test]
    fn slow_command_is_detached_as_job() {
        let d = tempfile::tempdir().unwrap();
        let ex = Exec::new(d.path().to_path_buf(), vec![]);
        let t = Instant::now();
        let o = ex.run("echo started; sleep 2; echo finished", 1).unwrap();
        assert!(t.elapsed() < Duration::from_millis(1800));
        assert_eq!(o.code, None);
        assert!(o.text.contains("job 1"), "{}", o.text);
        let job: Job =
            serde_json::from_str(&fs::read_to_string(jobs_dir(d.path()).join("1.json")).unwrap())
                .unwrap();
        // the job keeps running and records its exit code when done
        let deadline = Instant::now() + Duration::from_secs(10);
        while !job.status.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(fs::read_to_string(&job.status).unwrap().trim(), "0");
        assert!(read_lossy(&job.log).contains("finished"));
    }

    #[test]
    fn parallel_calls_overlap_and_keep_order() {
        let d = tempfile::tempdir().unwrap();
        let ex = Exec::new(d.path().to_path_buf(), vec![]);
        let calls = vec![
            ("sleep 1; echo first".to_string(), 10),
            ("sleep 1; echo second".to_string(), 10),
            ("sleep 1; echo third".to_string(), 10),
        ];
        let mut done = vec![];
        let t = Instant::now();
        let out = ex.run_all(&calls, |i, _| done.push(i));
        let took = t.elapsed();
        assert!(took < Duration::from_millis(2500), "took {took:?}");
        let texts: Vec<String> = out
            .into_iter()
            .map(|o| o.unwrap().text.trim().to_string())
            .collect();
        assert_eq!(texts, ["first", "second", "third"]);
        done.sort();
        assert_eq!(done, [0, 1, 2]);
    }
}
