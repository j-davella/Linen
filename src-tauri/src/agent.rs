use regex::Regex;
use serde_json::Value;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tauri::{AppHandle, Emitter};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;

use crate::audit::AuditSession;
use crate::terminal::Terminal;

const CLAUDE_BIN: &str = "/run/current-system/sw/bin/claude";
const CONTEXT_FILE: &str = ".linen/context.md";

// ── system prompt ─────────────────────────────────────────────────────────────

fn build_system_prompt() -> String {
    let context = dirs::home_dir()
        .map(|h| h.join(CONTEXT_FILE))
        .and_then(|p| std::fs::read_to_string(p).ok())
        .unwrap_or_default();

    let base = r#"
== LINEN CO-WORK MODE ==
You are working inside Linen, a co-working shell. Always use bash/POSIX syntax.

AVAILABLE ACTIONS (one per reply, then STOP):

1. Run a shell command:
   <RUN>command here</RUN>
   Linen executes it and returns "Command output:" — then continue.

2. Long-running command — set an explicit timeout in seconds:
   <RUN timeout="600">nixos-rebuild switch</RUN>
   Default is 90s.

3. Background job — fire-and-forget, does NOT block:
   <RUN bg="true" id="mybuild">cargo build --release</RUN>
   You'll be notified automatically when it finishes.

4. Write a file directly (avoids all shell quoting issues):
   <WRITE path="/absolute/path">
   file content here
   </WRITE>

5. Read the user's current terminal:
   <RUN>tmux capture-pane -p -t linen:0</RUN>
   This gives you a clean snapshot of what's on the user's shell right now.

AUTH: If sudo asks for a password, Linen detects it and alerts the user.

RULES:
- One <RUN> or <WRITE> per reply, never more.
- Never use the built-in Bash tool."#;

    if context.is_empty() {
        base.to_string()
    } else {
        format!("== LINEN CONTEXT ==\n{}\n\n{}", context.trim(), base)
    }
}

// ── message types ─────────────────────────────────────────────────────────────

pub enum Msg {
    User(String),
    BgDone { job_id: String, exit_code: i32, output: String },
}

// ── Tauri event payloads ──────────────────────────────────────────────────────

#[derive(serde::Serialize, Clone)]
pub struct FeedLine {
    pub pane:   String,
    pub ev:     String,
    pub prefix: String,
    pub text:   String,
}

#[derive(serde::Serialize, Clone)]
pub struct StatsPayload {
    pub tools:    u32,
    pub cmds:     u32,
    pub logs_mb:  f64,
    pub uptime_ms: u64,
    pub instances: Vec<InstanceInfo>,
}

#[derive(serde::Serialize, Clone)]
pub struct InstanceInfo {
    pub name:   String,
    pub host:   String,
    pub status: String,
    pub logs:   u32,
    pub seg:    u32,
}

// ── auth patterns ─────────────────────────────────────────────────────────────

const AUTH_PATTERNS: &[&str] = &[
    "[sudo] password",
    "password for",
    "enter passphrase",
    "enter password",
    "password:",
    "(current) unix password",
];

// ── public handle ─────────────────────────────────────────────────────────────

pub struct Agent {
    tx: mpsc::UnboundedSender<Msg>,
}

impl Agent {
    pub fn new(handle: AppHandle, audit: Arc<Mutex<AuditSession>>) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let tx2 = tx.clone();
        tokio::spawn(agent_loop(rx, tx2, handle, audit));
        Agent { tx }
    }

    pub fn send(&self, text: String) {
        let _ = self.tx.send(Msg::User(text));
    }
}

// ── main loop ─────────────────────────────────────────────────────────────────

async fn agent_loop(
    mut rx: mpsc::UnboundedReceiver<Msg>,
    tx: mpsc::UnboundedSender<Msg>,
    handle: AppHandle,
    audit: Arc<Mutex<AuditSession>>,
) {
    let system = build_system_prompt();
    let mut session_id: Option<String> = None;
    let start = Instant::now();
    let mut tool_count: u32 = 0;
    let mut cmd_count:  u32 = 0;

    while let Some(msg) = rx.recv().await {
        let (text, is_followup) = match msg {
            Msg::User(t) => (t, false),
            Msg::BgDone { job_id, exit_code, output } => {
                let tail = if output.len() > 4000 { &output[output.len()-4000..] } else { &output };
                (
                    format!(
                        "Background job '{}' completed (exit code {}).\nOutput:\n```\n{}\n```\nPlease continue.",
                        job_id, exit_code, tail
                    ),
                    true,
                )
            }
        };

        if !is_followup {
            if let Ok(a) = audit.lock() { a.log_turn("user", &text); }
        }

        handle.emit("set_busy", true).ok();
        handle.emit("stream_start", ()).ok();

        run_conversation(
            text, is_followup,
            &handle, &audit, &system,
            &mut session_id, &mut tool_count, &mut cmd_count,
            &tx,
        ).await;

        handle.emit("stream_end", ()).ok();
        handle.emit("set_busy", false).ok();

        // Push stats
        let logs_mb = audit.lock().map(|a| a.db_size_mb()).unwrap_or(0.0);
        handle.emit("update", StatsPayload {
            tools: tool_count,
            cmds: cmd_count,
            logs_mb,
            uptime_ms: start.elapsed().as_millis() as u64,
            instances: vec![InstanceInfo {
                name:   "current session".into(),
                host:   "device".into(),
                status: "IDLE".into(),
                logs:   tool_count + cmd_count,
                seg:    ((tool_count + cmd_count) / 3).min(16),
            }],
        }).ok();
    }
}

// ── conversation loop (handles follow-ups) ────────────────────────────────────

#[allow(clippy::too_many_arguments)]
async fn run_conversation(
    initial_text: String,
    initial_followup: bool,
    handle: &AppHandle,
    audit: &Arc<Mutex<AuditSession>>,
    system: &str,
    session_id: &mut Option<String>,
    tool_count: &mut u32,
    cmd_count: &mut u32,
    tx: &mpsc::UnboundedSender<Msg>,
) {
    let run_re   = Regex::new(r#"<RUN\b([^>]*)>([\s\S]*?)</RUN>"#).unwrap();
    let write_re = Regex::new(r#"<WRITE\b([^>]*)>([\s\S]*?)</WRITE>"#).unwrap();
    let _attr_re = Regex::new(r#"(\w+)=["']([^"']*)["']"#).unwrap();

    let mut message    = initial_text;
    let mut followup   = initial_followup;

    loop {
        let result = do_single_turn(
            &message, followup,
            handle, audit, system, session_id,
            &run_re, &write_re,
        ).await;

        match result {
            TurnResult::Done => break,

            TurnResult::RunCommand { cmd, timeout_s, .. } => {
                *tool_count += 1;
                *cmd_count  += 1;

                let cmd_id = audit.lock().map(|a| a.log_command_start(&cmd)).unwrap_or(0);

                handle.emit("feed_line", FeedLine {
                    pane:   "bot".into(),
                    ev:     "shell".into(),
                    prefix: r#"<span class="c">$</span> "#.into(),
                    text:   cmd.chars().take(80).collect(),
                }).ok();

                // Check for auth prompts while polling
                let handle2  = handle.clone();
                let output = Terminal::run_command(&cmd, timeout_s, move |line| {
                    // Auth detection
                    let low = line.to_lowercase();
                    if AUTH_PATTERNS.iter().any(|p| low.contains(p)) {
                        handle2.emit("feed_line", FeedLine {
                            pane:   "top".into(),
                            ev:     "log".into(),
                            prefix: r#"<span class="y">auth</span> "#.into(),
                            text:   "Password required — type it in the shell bar below".into(),
                        }).ok();
                    }
                    handle2.emit("feed_line", FeedLine {
                        pane:   "bot".into(),
                        ev:     "log".into(),
                        prefix: r#"<span class="g">out</span> "#.into(),
                        text:   line.chars().take(80).collect(),
                    }).ok();
                }).await;

                if let Ok(a) = audit.lock() { a.log_command_done(cmd_id, &output); }

                message  = format!("Command output:\n```\n{}\n```\n\nPlease continue.", output);
                followup = true;
            }

            TurnResult::BgJob { job_id, cmd, .. } => {
                *tool_count += 1;
                let audit3 = audit.clone();
                let handle3 = handle.clone();
                let tx3 = tx.clone();
                let jid = job_id.clone();

                handle.emit("feed_line", FeedLine {
                    pane:   "bot".into(),
                    ev:     "shell".into(),
                    prefix: format!(r#"<span class="g">[{}]</span> "#, job_id),
                    text:   cmd.chars().take(80).collect(),
                }).ok();

                if let Ok(a) = audit.lock() { a.log_command_start(&format!("BG:{} {}", job_id, cmd)); }

                tokio::spawn(async move {
                    let mut child = tokio::process::Command::new("bash")
                        .args(["-c", &cmd])
                        .stdout(std::process::Stdio::piped())
                        .stderr(std::process::Stdio::piped())
                        .spawn()
                        .expect("failed to spawn bg job");

                    let stdout = child.stdout.take().unwrap();
                    let mut lines = BufReader::new(stdout).lines();
                    let mut all_output = String::new();

                    while let Ok(Some(line)) = lines.next_line().await {
                        all_output.push_str(&line);
                        all_output.push('\n');
                        handle3.emit("feed_line", FeedLine {
                            pane:   "bot".into(),
                            ev:     "log".into(),
                            prefix: format!(r#"<span class="g">[{}]</span> "#, &jid[..jid.len().min(8)]),
                            text:   line.chars().take(80).collect(),
                        }).ok();
                    }

                    let status = child.wait().await.ok();
                    let exit_code = status.and_then(|s| s.code()).unwrap_or(-1);

                    if let Ok(a) = audit3.lock() {
                        a.log_command_done(0, &all_output);
                    }
                    handle3.emit("feed_line", FeedLine {
                        pane:   "bot".into(),
                        ev:     "tool".into(),
                        prefix: format!(r#"<span class="g">[{}]</span> "#, &jid[..jid.len().min(8)]),
                        text:   format!("done (exit {})", exit_code),
                    }).ok();

                    let _ = tx3.send(Msg::BgDone { job_id: jid, exit_code, output: all_output });
                });

                message  = format!("Background job '{}' started. I'll be notified when it finishes.", job_id);
                followup = true;
            }

            TurnResult::WriteFile { path, content, .. } => {
                *tool_count += 1;
                let result = write_file(&path, &content);
                if let Ok(a) = audit.lock() {
                    a.log_command_start(&format!("WRITE {}", path));
                    a.log_command_done(0, &result);
                }
                message  = format!("Write result: {}", result);
                followup = true;
            }

            TurnResult::Error(e) => {
                if let Ok(a) = audit.lock() { a.log_error(&e); }
                handle.emit("feed_line", FeedLine {
                    pane:   "top".into(),
                    ev:     "log".into(),
                    prefix: r#"<span class="y">err</span> "#.into(),
                    text:   e.chars().take(80).collect(),
                }).ok();
                handle.emit("stream_end", ()).ok();
                handle.emit("set_busy", false).ok();
                return;
            }
        }
    }
}

// ── single Claude turn ────────────────────────────────────────────────────────

enum TurnResult {
    Done,
    RunCommand { cmd: String, timeout_s: u64, _text: String },
    BgJob      { job_id: String, cmd: String, _text: String },
    WriteFile  { path: String, content: String, _text: String },
    Error(String),
}

async fn do_single_turn(
    message: &str,
    is_followup: bool,
    handle: &AppHandle,
    audit: &Arc<Mutex<AuditSession>>,
    system: &str,
    session_id: &mut Option<String>,
    run_re:   &Regex,
    write_re: &Regex,
) -> TurnResult {
    let attr_re = Regex::new(r#"(\w+)=["']([^"']*)["']"#).unwrap();

    if !is_followup {
        if let Ok(a) = audit.lock() { a.log_turn("user", message); }
    }

    let mut cmd = vec![CLAUDE_BIN.to_string()];
    if let Some(sid) = session_id.as_ref() {
        cmd.extend(["--resume".into(), sid.clone()]);
    }
    cmd.extend([
        "-p".into(), message.to_string(),
        "--output-format".into(), "stream-json".into(),
        "--verbose".into(),
        "--include-partial-messages".into(),
        "--tools".into(), String::new(),
        "--model".into(), "claude-sonnet-4-6".into(),
        "--append-system-prompt".into(), system.to_string(),
    ]);

    let mut child = match tokio::process::Command::new(&cmd[0])
        .args(&cmd[1..])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return TurnResult::Error(format!("Failed to spawn claude: {}", e)),
    };

    let stdout = child.stdout.take().unwrap();
    let mut lines = BufReader::new(stdout).lines();
    let mut accumulated = String::new();
    let mut last_streamed = 0usize;

    while let Ok(Some(raw)) = lines.next_line().await {
        let raw = raw.trim().to_string();
        if raw.is_empty() { continue; }

        let evt: Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(_) => continue,
        };

        match evt["type"].as_str() {
            Some("result") => {
                let sid = evt["session_id"].as_str().unwrap_or("").to_string();
                if !sid.is_empty() && Some(&sid) != session_id.as_ref() {
                    if let Ok(a) = audit.lock() { a.update_claude_session(&sid); }
                    *session_id = Some(sid);
                }
                if evt["is_error"].as_bool().unwrap_or(false) {
                    let e = evt["api_error_status"].as_str().unwrap_or("API error").to_string();
                    return TurnResult::Error(e);
                }
            }

            Some("assistant") => {
                if let Some(blocks) = evt["message"]["content"].as_array() {
                    for block in blocks {
                        if block["type"].as_str() == Some("text") {
                            let full = block["text"].as_str().unwrap_or("");
                            if full.len() > last_streamed {
                                let new_part = &full[last_streamed..];
                                last_streamed = full.len();
                                accumulated = full.to_string();

                                // Strip tags from streamed output
                                let visible = run_re.replace_all(new_part, "");
                                let visible = write_re.replace_all(&visible, "");
                                let visible = visible.trim();
                                if !visible.is_empty() {
                                    handle.emit("stream_chunk", visible.to_string()).ok();
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    let status = child.wait().await.ok();
    let rc = status.and_then(|s| s.code()).unwrap_or(-1);

    if rc != 0 {
        let stderr = child.stderr.take()
            .map(|s| async move {
                let mut out = String::new();
                let mut r = BufReader::new(s);
                let _ = tokio::io::AsyncReadExt::read_to_string(&mut r, &mut out).await;
                out
            });
        let err = if let Some(f) = stderr { f.await } else { String::new() };
        if !err.is_empty() {
            return TurnResult::Error(format!("claude rc={}: {}", rc, &err[..err.len().min(500)]));
        }
    }

    if let Ok(a) = audit.lock() { a.log_turn("assistant", &accumulated); }

    // ── <WRITE> ───────────────────────────────────────────────────────────────
    if let Some(cap) = write_re.captures(&accumulated) {
        let attrs = parse_attrs(cap.get(1).map(|m| m.as_str()).unwrap_or(""), &attr_re);
        let path  = attrs.get("path").cloned().unwrap_or_default();
        let mut content = cap.get(2).map(|m| m.as_str()).unwrap_or("").to_string();
        if content.starts_with('\n') { content = content[1..].to_string(); }
        return TurnResult::WriteFile { path, content, _text: accumulated };
    }

    // ── <RUN> ─────────────────────────────────────────────────────────────────
    if let Some(cap) = run_re.captures(&accumulated) {
        let attrs   = parse_attrs(cap.get(1).map(|m| m.as_str()).unwrap_or(""), &attr_re);
        let run_cmd = cap.get(2).map(|m| m.as_str().trim().to_string()).unwrap_or_default();

        if matches!(attrs.get("bg").map(|s| s.as_str()), Some("true") | Some("1") | Some("yes")) {
            let job_id = attrs.get("id").cloned()
                .unwrap_or_else(|| format!("job_{}", chrono::Utc::now().timestamp()));
            handle.emit("feed_line", FeedLine {
                pane:   "top".into(),
                ev:     "tool".into(),
                prefix: r#"<span class="p">claude</span> "#.into(),
                text:   format!("[bg:{}] {}", &job_id, run_cmd.chars().take(70).collect::<String>()),
            }).ok();
            return TurnResult::BgJob { job_id, cmd: run_cmd, _text: accumulated };
        }

        let timeout_s = attrs.get("timeout")
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(90)
            .max(5);

        handle.emit("feed_line", FeedLine {
            pane:   "top".into(),
            ev:     "tool".into(),
            prefix: r#"<span class="p">claude</span> "#.into(),
            text:   run_cmd.chars().take(80).collect(),
        }).ok();

        return TurnResult::RunCommand { cmd: run_cmd, timeout_s, _text: accumulated };
    }

    TurnResult::Done
}

// ── helpers ───────────────────────────────────────────────────────────────────

fn parse_attrs(s: &str, re: &Regex) -> std::collections::HashMap<String, String> {
    re.captures_iter(s)
        .map(|c| (c[1].to_string(), c[2].to_string()))
        .collect()
}

fn write_file(path: &str, content: &str) -> String {
    let p = std::path::Path::new(path);
    let expanded = if path.starts_with('~') {
        dirs::home_dir()
            .map(|h| h.join(&path[2..]))
            .unwrap_or_else(|| p.to_path_buf())
    } else {
        p.to_path_buf()
    };
    if let Some(parent) = expanded.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return format!("Write failed: {}", e);
        }
    }
    match std::fs::write(&expanded, content) {
        Ok(_)  => format!("File written: {} ({} bytes)", path, content.len()),
        Err(e) => format!("Write failed: {}", e),
    }
}
