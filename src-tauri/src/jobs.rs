//! Background jobs: plain child processes (not in a PTY) whose output is kept
//! in memory and summarised to Claude when they finish.

use crate::core::{tag, Core};
use crate::term::{clean_output, strip_ansi};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};

const KEEP_BYTES: usize = 200 * 1024;

#[derive(Clone, PartialEq)]
pub enum JobState {
    Running,
    Exited(i32),
    Killed,
    Failed(String),
}

pub struct Job {
    pub cmd: String,
    pub started: Instant,
    pub finished: Option<Instant>,
    pub state: JobState,
    pub output: String,
    stop: Option<oneshot::Sender<()>>,
}

#[derive(Default)]
pub struct Jobs {
    map: Mutex<HashMap<String, Job>>,
    counter: Mutex<u32>,
}

impl Jobs {
    fn map(&self) -> std::sync::MutexGuard<'_, HashMap<String, Job>> {
        self.map.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn running_count(&self) -> usize {
        self.map().values().filter(|j| j.state == JobState::Running).count()
    }

    fn new_id(&self, name: Option<&str>) -> String {
        let mut n = self.counter.lock().unwrap_or_else(|e| e.into_inner());
        *n += 1;
        let base: String = name
            .unwrap_or("job")
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .take(24)
            .collect();
        let base = if base.is_empty() { "job".to_string() } else { base };
        let map = self.map();
        if !map.contains_key(&base) && name.is_some() {
            base
        } else {
            format!("{base}-{n}")
        }
    }

    pub fn start(&self, core: &Arc<Core>, name: Option<&str>, cmd: &str, audit_id: i64) -> Result<String, String> {
        let id = self.new_id(name);
        let (prog, args) = core.shell.job_command(cmd);
        let mut command = Command::new(&prog);
        command
            .args(&args)
            .current_dir(&core.workspace)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("LINEN", "1")
            .kill_on_drop(true);
        #[cfg(windows)]
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        let mut child = command.spawn().map_err(|e| format!("could not start job: {e}"))?;

        let (stop_tx, stop_rx) = oneshot::channel();
        self.map().insert(
            id.clone(),
            Job {
                cmd: cmd.to_string(),
                started: Instant::now(),
                finished: None,
                state: JobState::Running,
                output: String::new(),
                stop: Some(stop_tx),
            },
        );

        let (line_tx, mut line_rx) = mpsc::unbounded_channel::<String>();
        if let Some(out) = child.stdout.take() {
            pump(out, line_tx.clone());
        }
        if let Some(err) = child.stderr.take() {
            pump(err, line_tx.clone());
        }
        drop(line_tx);

        let core = Arc::clone(core);
        let jid = id.clone();
        tauri::async_runtime::spawn(async move {
            let short: String = jid.chars().take(10).collect();
            let mut stop_rx = stop_rx;
            let mut last_feed = Instant::now() - Duration::from_secs(1);
            let mut killed = false;
            loop {
                tokio::select! {
                    line = line_rx.recv() => match line {
                        Some(l) => {
                            core.jobs.append(&jid, &l);
                            if last_feed.elapsed() > Duration::from_millis(120) && !l.trim().is_empty() {
                                last_feed = Instant::now();
                                core.feed("bot", "log", tag::green(&format!("[{short}]")), &strip_ansi(&l));
                            }
                        }
                        None => break, // both pipes closed
                    },
                    _ = &mut stop_rx, if !killed => {
                        killed = true;
                        let _ = child.start_kill();
                    }
                }
            }
            let state = match child.wait().await {
                _ if killed => JobState::Killed,
                Ok(s) => JobState::Exited(s.code().unwrap_or(-1)),
                Err(e) => JobState::Failed(e.to_string()),
            };
            core.jobs.finish(&core, &jid, state, audit_id);
        });
        Ok(id)
    }

    fn append(&self, id: &str, line: &str) {
        if let Some(j) = self.map().get_mut(id) {
            j.output.push_str(line);
            if !line.ends_with('\n') {
                j.output.push('\n');
            }
            if j.output.len() > KEEP_BYTES {
                let mut cut = j.output.len() - KEEP_BYTES * 3 / 4;
                while !j.output.is_char_boundary(cut) {
                    cut += 1;
                }
                j.output.drain(..cut);
                j.output.insert_str(0, "[... earlier output dropped ...]\n");
            }
        }
    }

    fn finish(&self, core: &Arc<Core>, id: &str, state: JobState, audit_id: i64) {
        let (cmd, output, elapsed) = {
            let mut map = self.map();
            let Some(j) = map.get_mut(id) else { return };
            j.state = state.clone();
            j.finished = Some(Instant::now());
            j.stop = None;
            (j.cmd.clone(), j.output.clone(), j.started.elapsed())
        };
        let short: String = id.chars().take(10).collect();
        let (label, code) = match &state {
            JobState::Exited(c) => (format!("exit {c}"), Some(*c as i64)),
            JobState::Killed => ("stopped".to_string(), None),
            JobState::Failed(e) => (format!("failed: {e}"), None),
            JobState::Running => unreachable!(),
        };
        core.feed("bot", "tool", tag::green(&format!("[{short}]")), &format!("done ({label})"));
        let cleaned = clean_output(&output);
        core.audit.command_done(audit_id, &cleaned, code, elapsed.as_millis() as i64);
        if state == JobState::Killed {
            return; // Claude (or the user) asked for this; no need to report back.
        }
        let tail = tail_lines(&cleaned, 80);
        let msg = format!(
            "[linen] Background job '{id}' finished ({label}) after {}s.\nCommand: {cmd}\nLast output:\n```\n{tail}\n```",
            elapsed.as_secs()
        );
        let core2 = Arc::clone(core);
        tauri::async_runtime::spawn(async move {
            core2.claude.send(&core2, &msg, false).await;
        });
    }

    pub fn status(&self, id: &str, lines: usize) -> Option<String> {
        let map = self.map();
        let j = map.get(id)?;
        let state = match &j.state {
            JobState::Running => format!("running for {}s", j.started.elapsed().as_secs()),
            JobState::Exited(c) => format!(
                "exited with code {c} after {}s",
                j.finished.map(|f| (f - j.started).as_secs()).unwrap_or(0)
            ),
            JobState::Killed => "stopped".into(),
            JobState::Failed(e) => format!("failed: {e}"),
        };
        Some(format!(
            "Job '{id}': {state}\nCommand: {}\nLast {lines} lines:\n{}",
            j.cmd,
            tail_lines(&clean_output(&j.output), lines)
        ))
    }

    pub fn list(&self) -> String {
        let map = self.map();
        if map.is_empty() {
            return "No jobs.".into();
        }
        let mut rows: Vec<_> = map.iter().collect();
        rows.sort_by_key(|(_, j)| j.started);
        rows.iter()
            .map(|(id, j)| {
                let st = match &j.state {
                    JobState::Running => "running".to_string(),
                    JobState::Exited(c) => format!("exit {c}"),
                    JobState::Killed => "stopped".into(),
                    JobState::Failed(_) => "failed".into(),
                };
                format!("{id}\t{st}\t{}", j.cmd.lines().next().unwrap_or(""))
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn stop(&self, id: &str) -> bool {
        match self.map().get_mut(id).and_then(|j| j.stop.take()) {
            Some(tx) => tx.send(()).is_ok(),
            None => false,
        }
    }

    pub fn kill_all(&self) {
        for j in self.map().values_mut() {
            if let Some(tx) = j.stop.take() {
                let _ = tx.send(());
            }
        }
    }
}

fn pump<R: AsyncRead + Unpin + Send + 'static>(r: R, tx: mpsc::UnboundedSender<String>) {
    tauri::async_runtime::spawn(async move {
        let mut reader = BufReader::new(r);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if tx.send(String::from_utf8_lossy(&buf).into_owned()).is_err() {
                        break;
                    }
                }
            }
        }
    });
}

fn tail_lines(s: &str, n: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(n);
    lines[start..].join("\n")
}
