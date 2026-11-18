use anyhow::Result;
use tokio::process::Command;
use tokio::time::{sleep, Duration, Instant};
use uuid::Uuid;

const SESSION: &str = "linen";
const SHELL: &str = "/run/current-system/sw/bin/bash";

// Window 0: user interactive shell   (send_to_user, get_snapshot)
// Window 1: Claude exec pane         (run_command with sentinel)

pub struct Terminal;

impl Terminal {
    pub async fn start() -> Result<()> {
        // Kill stale session, ignore error
        let _ = Command::new("tmux")
            .args(["kill-session", "-t", SESSION])
            .output().await;

        // User shell window
        Command::new("tmux")
            .args(["new-session", "-d", "-s", SESSION,
                   "-x", "220", "-y", "50", SHELL])
            .output().await?;

        // Claude exec window (stays hidden — user never attaches here)
        Command::new("tmux")
            .args(["new-window", "-t", &format!("{}:", SESSION), SHELL])
            .output().await?;

        Ok(())
    }

    /// Send a command to the user's interactive shell (window 0).
    pub async fn send_to_user(cmd: &str) {
        let _ = Command::new("tmux")
            .args(["send-keys", "-t", &format!("{}:0", SESSION), cmd, "Enter"])
            .output().await;
    }

    /// Return a clean text snapshot of the user's shell (window 0).
    /// No escape codes — ready for Claude to read.
    pub async fn get_snapshot() -> String {
        Command::new("tmux")
            .args(["capture-pane", "-p", "-t", &format!("{}:0", SESSION)])
            .output().await
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default()
    }

    /// Return the last `n` non-empty lines of the user shell for display in the UI.
    pub async fn get_last_lines(n: usize) -> Vec<String> {
        let snap = Self::get_snapshot().await;
        let mut lines: Vec<String> = snap.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| l.to_string())
            .collect();
        let start = lines.len().saturating_sub(n);
        lines.split_off(start)
    }

    /// Run a command in Claude's exec pane (window 1), wait for sentinel,
    /// stream live lines via `progress_cb`, return final output.
    pub async fn run_command<F>(
        cmd: &str,
        timeout_s: u64,
        mut progress_cb: F,
    ) -> String
    where
        F: FnMut(String),
    {
        let sentinel = format!("LINEN_{}", Uuid::new_v4().simple());

        // Write to a temp bash script to avoid all quoting issues
        let tmpdir = std::env::temp_dir().join(format!("linen_{}", &sentinel[6..22]));
        let _ = tokio::fs::create_dir_all(&tmpdir).await;
        let script = tmpdir.join("cmd.sh");
        let _ = tokio::fs::write(&script, format!("#!/usr/bin/env bash\n{}\n", cmd)).await;

        let full = format!(
            "bash {script}; _rc=$?; rm -rf {tmp}; printf '\\n{sen}:$_rc\\n'",
            script = script.display(),
            tmp    = tmpdir.display(),
            sen    = sentinel,
        );

        let _ = Command::new("tmux")
            .args(["send-keys", "-t", &format!("{}:1", SESSION), &full, "Enter"])
            .output().await;

        let deadline    = Instant::now() + Duration::from_secs(timeout_s + 60);
        let ext_limit   = Instant::now() + Duration::from_secs(timeout_s * 2);
        let mut shown   = 0usize;
        let mut last_progress = Instant::now();
        let marker = format!("{}:", sentinel);

        loop {
            sleep(Duration::from_millis(200)).await;

            let captured = Command::new("tmux")
                .args(["capture-pane", "-p", "-J", "-t", &format!("{}:1", SESSION)])
                .output().await
                .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
                .unwrap_or_default();

            // Stream new lines to UI
            let new_part = &captured[shown..];
            if !new_part.is_empty() {
                last_progress = Instant::now();
                let parts: Vec<&str> = new_part.split('\n').collect();
                let n = parts.len();
                for line in &parts[..n.saturating_sub(1)] {
                    let l = line.trim();
                    if !l.is_empty() && !l.contains(&sentinel) {
                        progress_cb(l.to_string());
                    }
                }
                shown = captured.len().saturating_sub(parts.last().map(|s| s.len()).unwrap_or(0));
            }

            // Sentinel found?
            if let Some(idx) = captured.rfind(&marker) {
                let before = &captured[..idx];
                let output = before.lines()
                    .skip(1) // skip the echoed command line
                    .collect::<Vec<_>>()
                    .join("\n")
                    .trim()
                    .to_string();
                return if output.is_empty() { "[no output]".to_string() } else { output };
            }

            // Timeout with auto-extend while output is still flowing
            if Instant::now() > deadline {
                if last_progress.elapsed() < Duration::from_secs(15) && Instant::now() < ext_limit {
                    continue;
                }
                return format!("[timed out after {}s — command may still be running in tmux window 1]", timeout_s);
            }
        }
    }

    /// Build the LINEN context block Claude sees when it reads the terminal.
    /// Uses tmux capture-pane so Claude gets clean text, no escape codes.
    pub async fn build_terminal_context() -> String {
        let snap = Self::get_snapshot().await;
        if snap.is_empty() {
            return String::new();
        }
        format!("== TERMINAL SNAPSHOT (tmux linen:0) ==\n{}\n", snap)
    }
}
