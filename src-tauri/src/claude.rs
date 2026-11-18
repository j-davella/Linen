//! One long-lived `claude -p --input-format stream-json` process.
//!
//! v1 started a new claude process (with --resume) for every message and every
//! command result. Here the process stays up between turns; messages are
//! written to its stdin as stream-json, and it is only restarted after Stop, a
//! model change, or a crash (then with --resume so the conversation continues).

use crate::core::{tag, Core};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};

struct Proc {
    stdin: ChildStdin,
}

#[derive(Default)]
struct Turn {
    streaming: bool,
    saw_delta: bool,
    text: String,
    tool_rows: HashMap<String, (i64, Instant, bool)>, // tool_use_id -> (audit row, start, is linen tool)
}

pub struct Claude {
    proc: tokio::sync::Mutex<Option<Proc>>,
    child: Mutex<Option<Arc<Mutex<Child>>>>,
    session_id: Mutex<Option<String>>,
    model: Mutex<String>,
    pending: AtomicU32,
    gen: AtomicU64,
    errored: AtomicBool,
    last_done: Mutex<Option<Instant>>,
    cost_base: Mutex<f64>,
    cost_cur: Mutex<f64>,
    turn: Mutex<Turn>,
    stderr_tail: Arc<Mutex<String>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Claude {
    pub fn new(model: &str) -> Claude {
        Claude {
            proc: tokio::sync::Mutex::new(None),
            child: Mutex::new(None),
            session_id: Mutex::new(None),
            model: Mutex::new(model.to_string()),
            pending: AtomicU32::new(0),
            gen: AtomicU64::new(0),
            errored: AtomicBool::new(false),
            last_done: Mutex::new(None),
            cost_base: Mutex::new(0.0),
            cost_cur: Mutex::new(0.0),
            turn: Mutex::new(Turn::default()),
            stderr_tail: Arc::new(Mutex::new(String::new())),
        }
    }

    pub fn model(&self) -> String {
        lock(&self.model).clone()
    }

    pub fn busy(&self) -> bool {
        self.pending.load(Ordering::SeqCst) > 0
    }

    pub fn cost_usd(&self) -> f64 {
        *lock(&self.cost_base) + *lock(&self.cost_cur)
    }

    pub fn status(&self) -> &'static str {
        if self.busy() {
            "ACTIVE"
        } else if self.errored.load(Ordering::SeqCst) {
            "ERROR"
        } else if lock(&self.last_done).map(|t| t.elapsed() < Duration::from_secs(8)).unwrap_or(false) {
            "DONE"
        } else {
            "IDLE"
        }
    }

    pub async fn ensure_started(&self, core: &Arc<Core>) -> Result<(), String> {
        let mut g = self.proc.lock().await;
        if g.is_none() {
            *g = Some(self.spawn(core)?);
        }
        Ok(())
    }

    fn spawn(&self, core: &Arc<Core>) -> Result<Proc, String> {
        let bin = core.cfg.resolve_claude().map_err(|e| e.to_string())?;

        let mcp_path = core.data_dir.join("mcp.json");
        let mcp = json!({"mcpServers": {"linen": {
            "type": "http",
            "url": format!("http://127.0.0.1:{}/mcp", core.mcp_port),
            "headers": {"Authorization": format!("Bearer {}", core.mcp_token)}
        }}});
        std::fs::write(&mcp_path, mcp.to_string()).map_err(|e| format!("write mcp.json: {e}"))?;
        let prompt_path = core.data_dir.join("system-prompt.md");
        std::fs::write(&prompt_path, crate::prompt::build(core)).map_err(|e| format!("write prompt: {e}"))?;

        let tools = if core.cfg.web_tools {
            "Read,Write,Edit,Glob,Grep,WebSearch,WebFetch"
        } else {
            "Read,Write,Edit,Glob,Grep"
        };
        let allowed = format!("{tools},mcp__linen");
        let model = self.model();
        let mut args: Vec<String> = vec![
            "-p".into(),
            "--input-format".into(), "stream-json".into(),
            "--output-format".into(), "stream-json".into(),
            "--verbose".into(),
            "--include-partial-messages".into(),
            "--model".into(), model,
            // No built-in Bash/PowerShell: shell work goes through Linen's
            // terminal (and its approval gate). File tools stay in the workspace.
            "--restricted".into(),
            "--tools".into(), tools.into(),
            "--allowedTools".into(), allowed,
            "--permission-mode".into(), "acceptEdits".into(),
            "--permission-prompts".into(), "none".into(),
            "--mcp-config".into(), mcp_path.to_string_lossy().into_owned(),
            "--strict-mcp-config".into(),
            "--append-system-prompt-file".into(), prompt_path.to_string_lossy().into_owned(),
        ];
        if let Some(sid) = lock(&self.session_id).clone() {
            args.push("--resume".into());
            args.push(sid);
        }

        let mut cmd = Command::new(&bin);
        cmd.args(&args)
            .current_dir(&core.workspace)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env("MCP_TOOL_TIMEOUT", "10800000")
            .env("MCP_TIMEOUT", "30000")
            .env("LINEN", "1")
            .kill_on_drop(true);
        #[cfg(windows)]
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        let mut child = cmd.spawn().map_err(|e| format!("could not start {}: {e}", bin.display()))?;
        tracing::info!("claude started: {} {}", bin.display(), args.join(" "));

        let gen = self.gen.fetch_add(1, Ordering::SeqCst) + 1;
        {
            let mut base = lock(&self.cost_base);
            let mut cur = lock(&self.cost_cur);
            *base += *cur;
            *cur = 0.0;
        }
        lock(&self.stderr_tail).clear();
        *lock(&self.turn) = Turn::default();

        let stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
        let stderr = child.stderr.take().ok_or("no stderr")?;
        let child = Arc::new(Mutex::new(child));
        *lock(&self.child) = Some(Arc::clone(&child));

        let tail = Arc::clone(&self.stderr_tail);
        tauri::async_runtime::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(l)) = lines.next_line().await {
                tracing::debug!("claude stderr: {l}");
                let mut t = lock(&tail);
                t.push_str(&l);
                t.push('\n');
                if t.len() > 4000 {
                    let mut cut = t.len() - 3000;
                    while !t.is_char_boundary(cut) {
                        cut += 1;
                    }
                    t.drain(..cut);
                }
            }
        });

        let core2 = Arc::clone(core);
        tauri::async_runtime::spawn(async move {
            let mut reader = BufReader::new(stdout);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match reader.read_until(b'\n', &mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let line = String::from_utf8_lossy(&buf);
                        let line = line.trim();
                        if line.is_empty() {
                            continue;
                        }
                        match serde_json::from_str::<Value>(line) {
                            Ok(v) => core2.claude.handle(&core2, gen, &v),
                            Err(_) => tracing::debug!("claude non-json: {line}"),
                        }
                    }
                }
            }
            core2.claude.on_exit(&core2, gen, child).await;
        });

        Ok(Proc { stdin })
    }

    async fn on_exit(&self, core: &Arc<Core>, gen: u64, child: Arc<Mutex<Child>>) {
        // Give the process a moment to finish exiting so we can read its status.
        let mut status = None;
        for _ in 0..20 {
            if let Ok(Some(s)) = lock(&child).try_wait() {
                status = Some(s);
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        if gen != self.gen.load(Ordering::SeqCst) {
            return; // we killed it on purpose
        }
        *self.proc.lock().await = None;
        lock(&self.child).take();
        let tail = lock(&self.stderr_tail).trim().to_string();
        tracing::warn!("claude exited unexpectedly: {status:?}; stderr: {tail}");
        let detail = if tail.is_empty() {
            format!("claude exited ({})", status.map(|s| s.to_string()).unwrap_or_else(|| "unknown".into()))
        } else {
            tail.lines().last().unwrap_or("").to_string()
        };
        core.audit.error(&format!("claude exited: {status:?} {tail}"));
        self.errored.store(true, Ordering::SeqCst);
        self.finish_stream(core);
        if self.pending.swap(0, Ordering::SeqCst) > 0 {
            core.emit("set_busy", false);
        }
        core.feed("top", "log", tag::amber("error"), &detail);
        core.emit("claude_error", detail);
    }

    /// Send a message. `from_user` false = a Linen notification (job finished).
    pub async fn send(&self, core: &Arc<Core>, text: &str, from_user: bool) {
        tracing::info!("send (user={from_user}): {:.120}", text.replace('\n', " "));
        if from_user {
            core.audit.turn("user", text);
        } else {
            core.audit.turn("linen", text);
            core.feed("top", "log", tag::green("linen"), text);
        }
        let line = json!({
            "type": "user",
            "message": {"role": "user", "content": [{"type": "text", "text": text}]}
        })
        .to_string()
            + "\n";

        for attempt in 0..2 {
            if let Err(e) = self.ensure_started(core).await {
                core.feed("top", "log", tag::amber("error"), &e);
                core.emit("claude_error", e);
                return;
            }
            self.pending.fetch_add(1, Ordering::SeqCst);
            self.errored.store(false, Ordering::SeqCst);
            core.emit("set_busy", true);
            let ok = {
                let mut g = self.proc.lock().await;
                match g.as_mut() {
                    Some(p) => p.stdin.write_all(line.as_bytes()).await.is_ok() && p.stdin.flush().await.is_ok(),
                    None => false,
                }
            };
            if ok {
                return;
            }
            self.pending.fetch_sub(1, Ordering::SeqCst);
            tracing::warn!("claude stdin write failed (attempt {attempt}); restarting");
            self.kill_proc().await;
        }
        core.emit("set_busy", self.busy());
        core.feed("top", "log", tag::amber("error"), "could not reach the claude process");
    }

    async fn kill_proc(&self) {
        self.gen.fetch_add(1, Ordering::SeqCst);
        *self.proc.lock().await = None;
        if let Some(c) = lock(&self.child).take() {
            let _ = lock(&c).start_kill();
        }
    }

    /// Synchronous kill for shutdown.
    pub fn kill(&self) {
        self.gen.fetch_add(1, Ordering::SeqCst);
        if let Some(c) = lock(&self.child).take() {
            let _ = lock(&c).start_kill();
        }
    }

    pub async fn stop(&self, core: &Arc<Core>) {
        self.kill_proc().await;
        core.approvals.deny_all();
        core.term_claude.interrupt();
        self.finish_stream(core);
        self.pending.store(0, Ordering::SeqCst);
        core.emit("set_busy", false);
        core.feed("top", "log", tag::amber("stopped"), "Claude stopped. Next message resumes the conversation.");
        core.audit.turn("linen", "[stopped by user]");
    }

    pub async fn set_model(&self, core: &Arc<Core>, model: &str) -> Result<(), String> {
        if self.busy() {
            return Err("wait for Claude to finish (or Stop) before switching models".into());
        }
        *lock(&self.model) = model.to_string();
        core.audit.set_model(model);
        self.kill_proc().await;
        core.feed("top", "log", tag::purple("model"), model);
        self.ensure_started(core).await
    }

    fn finish_stream(&self, core: &Core) {
        let mut t = lock(&self.turn);
        if t.streaming {
            t.streaming = false;
            core.emit("stream_end", ());
        }
    }

    fn handle(&self, core: &Arc<Core>, gen: u64, v: &Value) {
        if gen != self.gen.load(Ordering::SeqCst) {
            return;
        }
        match v["type"].as_str().unwrap_or("") {
            "system" if v["subtype"] == "init" => {
                if let Some(sid) = v["session_id"].as_str() {
                    self.set_session(core, sid);
                }
                let linen = v["mcp_servers"]
                    .as_array()
                    .and_then(|a| a.iter().find(|s| s["name"] == "linen"))
                    .and_then(|s| s["status"].as_str())
                    .unwrap_or("missing")
                    .to_string();
                tracing::info!("claude init: model={} linen mcp={linen}", v["model"]);
                if linen != "connected" {
                    core.feed("top", "log", tag::amber("warn"), &format!("Linen tools not connected ({linen})"));
                    core.audit.error(&format!("mcp linen status: {linen}"));
                }
            }
            "stream_event" => {
                let ev = &v["event"];
                match ev["type"].as_str().unwrap_or("") {
                    "message_start" => lock(&self.turn).saw_delta = false,
                    "content_block_delta" if ev["delta"]["type"] == "text_delta" => {
                        let text = ev["delta"]["text"].as_str().unwrap_or("");
                        let mut t = lock(&self.turn);
                        t.saw_delta = true;
                        if !t.streaming {
                            t.streaming = true;
                            core.emit("stream_start", ());
                        }
                        t.text.push_str(text);
                        core.emit("stream_chunk", text);
                    }
                    "content_block_stop" => {
                        let mut t = lock(&self.turn);
                        if t.streaming {
                            t.streaming = false;
                            t.text.push('\n');
                            core.emit("stream_end", ());
                        }
                    }
                    _ => {}
                }
            }
            "assistant" => {
                let Some(blocks) = v["message"]["content"].as_array() else { return };
                for b in blocks {
                    match b["type"].as_str().unwrap_or("") {
                        "text" => {
                            let mut t = lock(&self.turn);
                            if !t.saw_delta {
                                let text = b["text"].as_str().unwrap_or("");
                                if !text.trim().is_empty() {
                                    core.emit("stream_start", ());
                                    core.emit("stream_chunk", text);
                                    core.emit("stream_end", ());
                                    t.text.push_str(text);
                                    t.text.push('\n');
                                }
                            }
                        }
                        "tool_use" => {
                            core.tools.fetch_add(1, Ordering::Relaxed);
                            let name = b["name"].as_str().unwrap_or("tool");
                            let id = b["id"].as_str().unwrap_or("").to_string();
                            let linen_tool = name.starts_with("mcp__linen__");
                            if !linen_tool {
                                core.feed("top", "tool", tag::purple("claude"), &describe(name, &b["input"]));
                            }
                            let summary: String = format!("{name} {}", b["input"]).chars().take(2000).collect();
                            let row = core.audit.command_start("tool", &summary);
                            lock(&self.turn).tool_rows.insert(id, (row, Instant::now(), linen_tool));
                        }
                        _ => {}
                    }
                }
            }
            "user" => {
                let Some(blocks) = v["message"]["content"].as_array() else { return };
                for b in blocks.iter().filter(|b| b["type"] == "tool_result") {
                    let id = b["tool_use_id"].as_str().unwrap_or("");
                    let Some((row, t0, linen_tool)) = lock(&self.turn).tool_rows.remove(id) else { continue };
                    let is_error = b["is_error"].as_bool().unwrap_or(false);
                    let text = result_text(&b["content"]);
                    let short: String = text.chars().take(4000).collect();
                    core.audit.command_done(row, &short, Some(is_error as i64), t0.elapsed().as_millis() as i64);
                    if is_error && !linen_tool {
                        core.feed("top", "log", tag::amber("tool err"), &text);
                    }
                }
            }
            "result" => {
                if let Some(sid) = v["session_id"].as_str() {
                    self.set_session(core, sid);
                }
                if let Some(c) = v["total_cost_usd"].as_f64() {
                    let mut cur = lock(&self.cost_cur);
                    *cur = cur.max(c);
                }
                self.finish_stream(core);
                let is_error = v["is_error"].as_bool().unwrap_or(false)
                    || v["subtype"].as_str().map(|s| s != "success").unwrap_or(false);
                if is_error {
                    let msg = v["result"]
                        .as_str()
                        .or_else(|| v["subtype"].as_str())
                        .unwrap_or("Claude reported an error")
                        .to_string();
                    core.feed("top", "log", tag::amber("error"), &msg);
                    core.audit.error(&msg);
                    self.errored.store(true, Ordering::SeqCst);
                    core.emit("claude_error", msg);
                }
                let text = std::mem::take(&mut lock(&self.turn).text);
                if !text.trim().is_empty() {
                    core.audit.turn("assistant", text.trim());
                }
                let prev = self.pending.load(Ordering::SeqCst);
                if prev > 0 {
                    self.pending.store(prev - 1, Ordering::SeqCst);
                }
                if !self.busy() {
                    *lock(&self.last_done) = Some(Instant::now());
                    core.emit("set_busy", false);
                }
            }
            _ => {}
        }
    }

    fn set_session(&self, core: &Core, sid: &str) {
        let mut cur = lock(&self.session_id);
        if cur.as_deref() != Some(sid) {
            *cur = Some(sid.to_string());
            core.audit.set_claude_session(sid);
        }
    }
}

fn result_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|i| i["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn short_path(p: &str) -> String {
    let n = p.chars().count();
    if n <= 70 {
        p.to_string()
    } else {
        format!("\u{2026}{}", p.chars().skip(n - 69).collect::<String>())
    }
}

/// One-line description of a built-in tool call for the feed.
fn describe(name: &str, input: &Value) -> String {
    let s = |k: &str| input[k].as_str().unwrap_or("").to_string();
    match name {
        "Read" => format!("reading {}", short_path(&s("file_path"))),
        "Write" => format!("writing {}", short_path(&s("file_path"))),
        "Edit" | "MultiEdit" => format!("editing {}", short_path(&s("file_path"))),
        "Glob" => format!("finding {}", s("pattern")),
        "Grep" => format!("searching for {}", s("pattern")),
        "WebSearch" => format!("web search: {}", s("query")),
        "WebFetch" => format!("fetching {}", s("url")),
        "TodoWrite" => "updating its task list".into(),
        "Task" | "Agent" => format!("subagent: {}", s("description")),
        other => other.to_string(),
    }
}
