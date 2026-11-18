//! Real terminals: a PTY per pane (ConPTY on Windows, pty on Unix), mirrored to
//! xterm.js in the UI and to a vt100 screen model on our side (so Claude can
//! read the screen without escape codes).
//!
//! Claude's commands run in the "claude" PTY so the user sees them live and can
//! type into prompts. Output for Claude is NOT scraped from the screen (ConPTY
//! re-renders the screen with cursor moves, which makes scraping unreliable):
//! each command is wrapped in a small runner script that tees its output to a
//! file and writes the exit code to another file when done.

use crate::config::{Shell, ShellKind};
use portable_pty::{native_pty_system, ChildKiller, CommandBuilder, MasterPty, PtySize};
use regex::Regex;
use serde::Serialize;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};

const REPLAY_CAP: usize = 256 * 1024;
const OUTPUT_CAP: usize = 30_000;

struct Pty {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    killer: Box<dyn ChildKiller + Send + Sync>,
}

pub struct Term {
    pub name: &'static str,
    shell: Shell,
    cwd: PathBuf,
    pty: Mutex<Option<Pty>>,
    parser: Mutex<vt100::Parser>,
    replay: Mutex<String>,
    size: Mutex<(u16, u16)>, // (cols, rows)
    alive: AtomicBool,
    shutting_down: AtomicBool,
    quick_exits: AtomicU32,
    /// One Claude command at a time in this terminal.
    pub run_lock: tokio::sync::Mutex<()>,
}

#[derive(Clone, Serialize)]
struct OutputEvent<'a> {
    id: &'a str,
    data: String,
}

pub struct RunOutcome {
    pub output: String,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub shell_exited: bool,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Term {
    pub fn new(name: &'static str, shell: Shell, cwd: PathBuf) -> Arc<Term> {
        Arc::new(Term {
            name,
            shell,
            cwd,
            pty: Mutex::new(None),
            parser: Mutex::new(vt100::Parser::new(30, 100, 0)),
            replay: Mutex::new(String::new()),
            size: Mutex::new((100, 30)),
            alive: AtomicBool::new(false),
            shutting_down: AtomicBool::new(false),
            quick_exits: AtomicU32::new(0),
            run_lock: tokio::sync::Mutex::new(()),
        })
    }

    pub fn alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    pub fn spawn(self: &Arc<Self>, app: &AppHandle) -> anyhow::Result<()> {
        let (cols, rows) = *lock(&self.size);
        let pair = native_pty_system().openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })?;

        let mut cmd = CommandBuilder::new(&self.shell.program);
        cmd.args(&self.shell.args);
        cmd.cwd(&self.cwd);
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env("LINEN", "1");
        cmd.env("LINEN_TERM", self.name);

        let mut child = pair.slave.spawn_command(cmd)?;
        drop(pair.slave);
        let killer = child.clone_killer();
        let mut reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        *lock(&self.pty) = Some(Pty { master: pair.master, writer, killer });
        self.alive.store(true, Ordering::SeqCst);
        tracing::info!("term {} spawned {:?}", self.name, self.shell.program);

        // Reader: PTY bytes -> vt100 model + replay buffer + UI.
        let me = Arc::clone(self);
        let app_r = app.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 16 * 1024];
            let mut carry: Vec<u8> = Vec::new();
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        lock(&me.parser).process(&buf[..n]);
                        let text = decode_utf8(&mut carry, &buf[..n]);
                        if text.is_empty() {
                            continue;
                        }
                        {
                            let mut r = lock(&me.replay);
                            r.push_str(&text);
                            if r.len() > REPLAY_CAP {
                                let mut cut = r.len() - REPLAY_CAP / 2;
                                while !r.is_char_boundary(cut) {
                                    cut += 1;
                                }
                                r.drain(..cut);
                            }
                        }
                        let _ = app_r.emit("term_output", OutputEvent { id: me.name, data: text });
                    }
                }
            }
        });

        // Waiter: when the shell exits, close the PTY (ConPTY keeps the pipe
        // open until then) and start a fresh shell unless we're shutting down.
        let me = Arc::clone(self);
        let app_w = app.clone();
        let started = Instant::now();
        std::thread::spawn(move || {
            let status = child.wait();
            me.alive.store(false, Ordering::SeqCst);
            lock(&me.pty).take();
            tracing::info!("term {} shell exited: {:?}", me.name, status);
            if me.shutting_down.load(Ordering::SeqCst) {
                return;
            }
            if started.elapsed() < Duration::from_secs(3) {
                if me.quick_exits.fetch_add(1, Ordering::SeqCst) >= 2 {
                    let msg = format!(
                        "\r\n\x1b[31m[linen] shell {:?} keeps exiting immediately; check `shell` in ~/.linen/config.toml]\x1b[0m\r\n",
                        me.shell.program
                    );
                    let _ = app_w.emit("term_output", OutputEvent { id: me.name, data: msg });
                    return;
                }
            } else {
                me.quick_exits.store(0, Ordering::SeqCst);
            }
            let msg = "\r\n\x1b[33m[linen] shell exited, starting a new one\x1b[0m\r\n".to_string();
            let _ = app_w.emit("term_output", OutputEvent { id: me.name, data: msg });
            std::thread::sleep(Duration::from_millis(400));
            if let Err(e) = me.spawn(&app_w) {
                tracing::error!("term {} respawn failed: {e}", me.name);
            }
        });
        Ok(())
    }

    pub fn write(&self, data: &[u8]) -> bool {
        let mut g = lock(&self.pty);
        match g.as_mut() {
            Some(p) => p.writer.write_all(data).and_then(|_| p.writer.flush()).is_ok(),
            None => false,
        }
    }

    pub fn resize(&self, cols: u16, rows: u16) {
        let cols = cols.clamp(10, 1000);
        let rows = rows.clamp(4, 500);
        {
            let mut s = lock(&self.size);
            if *s == (cols, rows) {
                return;
            }
            *s = (cols, rows);
        }
        lock(&self.parser).set_size(rows, cols);
        if let Some(p) = lock(&self.pty).as_ref() {
            let _ = p.master.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
        }
    }

    /// Plain text of what's on screen right now (no escape codes).
    pub fn screen_text(&self) -> String {
        let text = lock(&self.parser).screen().contents();
        let lines: Vec<&str> = text.lines().map(|l| l.trim_end()).collect();
        let end = lines.iter().rposition(|l| !l.is_empty()).map(|i| i + 1).unwrap_or(0);
        lines[..end].join("\n")
    }

    pub fn replay(&self) -> String {
        lock(&self.replay).clone()
    }

    pub fn kill(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        if let Some(p) = lock(&self.pty).as_mut() {
            let _ = p.killer.kill();
        }
    }

    pub fn interrupt(&self) {
        self.write(b"\x03");
    }

    /// Run `cmd` in this terminal and wait for it. `on_waiting_input` fires once
    /// if the screen looks like it's asking for a password.
    pub async fn run(
        &self,
        cmd: &str,
        timeout: Duration,
        run_dir: &Path,
        mut on_waiting_input: impl FnMut(),
    ) -> RunOutcome {
        let _guard = self.run_lock.lock().await;
        if !self.alive() {
            return RunOutcome {
                output: "[the terminal's shell is not running]".into(),
                exit_code: None,
                timed_out: false,
                shell_exited: true,
            };
        }

        let id = uuid::Uuid::new_v4().simple().to_string()[..12].to_string();
        let files = RunFiles::new(run_dir, &id, self.shell.kind);
        if let Err(e) = files.write(cmd, self.shell.kind) {
            return RunOutcome {
                output: format!("[linen could not write the runner script: {e}]"),
                exit_code: None,
                timed_out: false,
                shell_exited: false,
            };
        }

        // Leading space keeps it out of bash history (HISTCONTROL=ignorespace).
        let line = match self.shell.kind {
            ShellKind::Posix => format!(" . {}\r", sh_quote(&shell_path(&files.runner))),
            ShellKind::Foreign => format!(" bash {}\r", sh_quote(&shell_path(&files.runner))),
            ShellKind::PowerShell => format!(" . {}\r", ps_quote(&files.runner.to_string_lossy())),
        };
        self.write(line.as_bytes());

        let start = Instant::now();
        let deadline = start + timeout;
        let hard_limit = start + timeout * 2;
        let mut last_len = 0u64;
        let mut last_growth = Instant::now();
        let mut asked_input = false;
        let mut timed_out = false;
        let mut shell_exited = false;
        let mut tick = 0u32;

        let exit_code = loop {
            tokio::time::sleep(Duration::from_millis(150)).await;
            tick += 1;

            if let Some(code) = files.read_rc() {
                break Some(code);
            }
            if !self.alive() {
                shell_exited = true;
                break None;
            }
            let len = std::fs::metadata(&files.out).map(|m| m.len()).unwrap_or(0);
            if len > last_len {
                last_len = len;
                last_growth = Instant::now();
            }
            if !asked_input && tick % 4 == 0 && looks_like_password_prompt(&self.screen_text()) {
                asked_input = true;
                on_waiting_input();
            }
            let now = Instant::now();
            if now > deadline {
                // Keep waiting (up to 2x) while output is still flowing,
                // and indefinitely while a password prompt is up.
                let flowing = last_growth.elapsed() < Duration::from_secs(15) && now < hard_limit;
                let waiting_on_user = asked_input && looks_like_password_prompt(&self.screen_text());
                if flowing || waiting_on_user {
                    continue;
                }
                timed_out = true;
                self.interrupt();
                let mut code = None;
                for _ in 0..20 {
                    tokio::time::sleep(Duration::from_millis(150)).await;
                    if let Some(c) = files.read_rc() {
                        code = Some(c);
                        break;
                    }
                }
                break code;
            }
        };

        let mut output = clean_output(&read_text_file(&files.out));
        if output.is_empty() {
            output = "[no output]".into();
        }
        if timed_out {
            output.push_str(&format!(
                "\n[linen: timed out after {}s and sent Ctrl+C. Use start_job for long-running work.]",
                start.elapsed().as_secs()
            ));
        }
        if shell_exited {
            output.push_str("\n[linen: the shell exited during this command (did it run `exit`?); a new one was started]");
        }
        files.cleanup();
        RunOutcome { output, exit_code, timed_out, shell_exited }
    }
}

// ─── runner scripts ──────────────────────────────────────────────────────────

struct RunFiles {
    cmd: PathBuf,
    runner: PathBuf,
    out: PathBuf,
    rc: PathBuf,
}

impl RunFiles {
    fn new(dir: &Path, id: &str, kind: ShellKind) -> RunFiles {
        let ext = if kind == ShellKind::PowerShell { "ps1" } else { "sh" };
        RunFiles {
            cmd: dir.join(format!("{id}.cmd.{ext}")),
            runner: dir.join(format!("{id}.run.{ext}")),
            out: dir.join(format!("{id}.out")),
            rc: dir.join(format!("{id}.rc")),
        }
    }

    fn write(&self, cmd: &str, kind: ShellKind) -> std::io::Result<()> {
        let display = display_line(cmd);
        match kind {
            ShellKind::PowerShell => {
                let cmd_body = format!("\u{feff}{cmd}\r\n");
                let runner = format!(
                    "\u{feff}Write-Host -NoNewline -ForegroundColor Magenta '\u{276f} linen '; Write-Host {disp}\r\n\
                     $__linen_e = $Error.Count\r\n\
                     $global:LASTEXITCODE = 0\r\n\
                     . {cmdf} 2>&1 | Tee-Object -FilePath {out}\r\n\
                     $__linen_rc = if ($LASTEXITCODE) {{ $LASTEXITCODE }} elseif ($Error.Count -gt $__linen_e) {{ 1 }} else {{ 0 }}\r\n\
                     Set-Content -LiteralPath {rc} -Value $__linen_rc -NoNewline -Encoding ascii\r\n",
                    disp = ps_quote(&display),
                    cmdf = ps_quote(&self.cmd.to_string_lossy()),
                    out = ps_quote(&self.out.to_string_lossy()),
                    rc = ps_quote(&self.rc.to_string_lossy()),
                );
                std::fs::write(&self.cmd, cmd_body)?;
                std::fs::write(&self.runner, runner)?;
            }
            ShellKind::Posix | ShellKind::Foreign => {
                let runner = format!(
                    "printf '\\033[1;35m\u{276f} linen\\033[0m %s\\n' {disp}\n\
                     {{ . {cmdf}; }} > >(tee {out}) 2>&1\n\
                     __linen_rc=$?\n\
                     wait $! 2>/dev/null\n\
                     printf '%s' \"$__linen_rc\" > {rc}\n",
                    disp = sh_quote(&display),
                    cmdf = sh_quote(&shell_path(&self.cmd)),
                    out = sh_quote(&shell_path(&self.out)),
                    rc = sh_quote(&shell_path(&self.rc)),
                );
                std::fs::write(&self.cmd, format!("{cmd}\n"))?;
                std::fs::write(&self.runner, runner)?;
            }
        }
        Ok(())
    }

    fn read_rc(&self) -> Option<i32> {
        let s = std::fs::read_to_string(&self.rc).ok()?;
        s.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}').parse().ok()
    }

    fn cleanup(&self) {
        for p in [&self.cmd, &self.runner, &self.out, &self.rc] {
            let _ = std::fs::remove_file(p);
        }
    }
}

fn display_line(cmd: &str) -> String {
    let first = cmd.lines().next().unwrap_or("").trim();
    let mut s: String = first.chars().take(160).collect();
    if cmd.lines().count() > 1 || first.chars().count() > 160 {
        s.push_str(" \u{2026}");
    }
    s
}

/// Paths as a POSIX shell wants them (Git Bash on Windows takes C:/x/y).
fn shell_path(p: &Path) -> String {
    let s = p.to_string_lossy().to_string();
    if cfg!(windows) {
        s.replace('\\', "/")
    } else {
        s
    }
}

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn ps_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

// ─── output handling ─────────────────────────────────────────────────────────

/// Tee-Object writes UTF-16LE on Windows PowerShell 5.1, UTF-8 elsewhere.
fn read_text_file(p: &Path) -> String {
    let bytes = std::fs::read(p).unwrap_or_default();
    if bytes.starts_with(&[0xFF, 0xFE]) {
        let units: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        return String::from_utf16_lossy(&units);
    }
    let b = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&bytes);
    String::from_utf8_lossy(b).into_owned()
}

fn ansi_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\x1b\[[0-?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b[@-Z\\-_]").unwrap()
    })
}

pub fn strip_ansi(s: &str) -> String {
    ansi_re().replace_all(s, "").into_owned()
}

/// Strip escapes, collapse carriage-return progress bars, cap the size.
pub fn clean_output(raw: &str) -> String {
    let stripped = strip_ansi(raw);
    let mut lines: Vec<&str> = Vec::new();
    for line in stripped.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        let last = line.rsplit('\r').find(|seg| !seg.is_empty()).unwrap_or("");
        lines.push(last.trim_end());
    }
    let text = lines.join("\n").trim().to_string();
    truncate_middle(&text, OUTPUT_CAP)
}

pub fn truncate_middle(s: &str, cap: usize) -> String {
    if s.len() <= cap {
        return s.to_string();
    }
    let head_len = cap / 5;
    let tail_len = cap - head_len;
    let mut h = head_len;
    while !s.is_char_boundary(h) {
        h -= 1;
    }
    let mut t = s.len() - tail_len;
    while !s.is_char_boundary(t) {
        t += 1;
    }
    format!(
        "{}\n\n[... {} bytes omitted ...]\n\n{}",
        &s[..h],
        t - h,
        &s[t..]
    )
}

fn looks_like_password_prompt(screen: &str) -> bool {
    let tail: String = screen
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .to_lowercase();
    ["password", "passphrase", "[sudo]", "pin for", "verification code"]
        .iter()
        .any(|p| tail.contains(p))
}

/// Decode a PTY chunk, carrying an incomplete UTF-8 sequence to the next read.
fn decode_utf8(carry: &mut Vec<u8>, new: &[u8]) -> String {
    carry.extend_from_slice(new);
    match std::str::from_utf8(carry) {
        Ok(s) => {
            let out = s.to_string();
            carry.clear();
            out
        }
        Err(e) if e.error_len().is_none() => {
            let valid = e.valid_up_to();
            let out = String::from_utf8_lossy(&carry[..valid]).into_owned();
            carry.drain(..valid);
            if carry.len() > 4 {
                carry.clear();
            }
            out
        }
        Err(_) => {
            let out = String::from_utf8_lossy(carry).into_owned();
            carry.clear();
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_carry() {
        let mut c = Vec::new();
        let s = "a\u{276f}b".as_bytes();
        let mut out = decode_utf8(&mut c, &s[..2]);
        out += &decode_utf8(&mut c, &s[2..]);
        assert_eq!(out, "a\u{276f}b");
    }

    #[test]
    fn cleans_progress_and_ansi() {
        let raw = "\x1b[32mok\x1b[0m\r\n10%\r50%\r100%\r\ndone";
        assert_eq!(clean_output(raw), "ok\n100%\ndone");
    }

    #[test]
    fn password_prompt() {
        assert!(looks_like_password_prompt("$ sudo ls\n[sudo] password for jonah: "));
        assert!(!looks_like_password_prompt("$ echo password\npassword\n$ "));
    }

    #[test]
    fn truncates() {
        let s = "x".repeat(100);
        let t = truncate_middle(&s, 50);
        assert!(t.contains("omitted"));
    }
}
