//! Shared app state and the glue between parts: UI feed events, the approval
//! gate, the 1 Hz stats push, startup and shutdown.

use crate::approval::Approvals;
use crate::audit::Audit;
use crate::claude::Claude;
use crate::config::{ApprovalMode, Config, Shell};
use crate::jobs::Jobs;
use crate::risk::RiskRules;
use crate::term::Term;
use serde::Serialize;
use serde_json::json;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};

pub struct Core {
    pub app: AppHandle,
    pub cfg: Config,
    pub data_dir: PathBuf,
    pub run_dir: PathBuf,
    pub workspace: PathBuf,
    pub shell: Shell,
    pub audit: Audit,
    pub risk: RiskRules,
    pub approval_mode: Mutex<ApprovalMode>,
    pub approvals: Approvals,
    pub term_claude: Arc<Term>,
    pub term_you: Arc<Term>,
    pub jobs: Jobs,
    pub claude: Claude,
    pub mcp_port: u16,
    pub mcp_token: String,
    pub tools: AtomicU64,
    pub cmds: AtomicU64,
    pub started: Instant,
}

#[derive(Clone, Serialize)]
pub struct FeedLine {
    pub pane: &'static str,
    pub ev: &'static str,
    pub prefix: String,
    pub text: String,
}

/// Prefix spans the UI's terminal panes already style.
pub mod tag {
    pub fn purple(s: &str) -> String {
        format!(r#"<span class="p">{s}</span> "#)
    }
    pub fn cyan(s: &str) -> String {
        format!(r#"<span class="c">{s}</span> "#)
    }
    pub fn green(s: &str) -> String {
        format!(r#"<span class="g">{s}</span> "#)
    }
    pub fn amber(s: &str) -> String {
        format!(r#"<span class="y">{s}</span> "#)
    }
}

impl Core {
    pub fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        m.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A line in one of the dashboard's feeds. `pane` is "top" (Claude) or
    /// "bot" (shell); `ev` is "tool" | "shell" | "log" (drives the orb ribbons).
    pub fn feed(&self, pane: &'static str, ev: &'static str, prefix: String, text: &str) {
        let one_line: String = text.lines().next().unwrap_or("").chars().take(160).collect();
        let _ = self.app.emit("feed_line", FeedLine { pane, ev, prefix, text: one_line });
    }

    pub fn emit<S: Serialize + Clone>(&self, event: &str, payload: S) {
        let _ = self.app.emit(event, payload);
    }

    /// Ok(decision) if `detail` may run, Err(reason) if the user denied it.
    pub async fn gate(&self, kind: &str, detail: &str) -> Result<&'static str, String> {
        let mode = *Self::lock(&self.approval_mode);
        let reason = match mode {
            ApprovalMode::Off => None,
            ApprovalMode::All => Some("approval is on for every command".to_string()),
            ApprovalMode::Risky => self.risk.assess(detail),
        };
        let Some(reason) = reason else {
            return Ok("auto");
        };
        self.feed("top", "log", tag::amber("approve?"), &format!("{reason}: {detail}"));
        if self.approvals.request(&self.app, kind, detail, &reason).await {
            self.feed("top", "log", tag::green("approved"), detail);
            Ok("approved")
        } else {
            self.feed("top", "log", tag::amber("denied"), detail);
            Err(reason)
        }
    }

    pub fn snapshot(&self) -> serde_json::Value {
        json!({
            "models": self.cfg.models,
            "model": self.claude.model(),
            "os": crate::config::os_label(),
            "shell": self.shell.label,
            "workspace": self.workspace.display().to_string(),
            "approval": Self::lock(&self.approval_mode).label(),
            "version": env!("CARGO_PKG_VERSION"),
            "busy": self.claude.busy(),
        })
    }

    pub fn start_stats_ticker(self: &Arc<Self>) {
        let core = Arc::clone(self);
        tauri::async_runtime::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let logs_bytes = core.audit.bytes() + dir_size(&core.data_dir.join("logs"));
                let status = if core.approvals.waiting() {
                    "WAITING"
                } else {
                    core.claude.status()
                };
                let tools = core.tools.load(Ordering::Relaxed);
                let name = core
                    .workspace
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| "linen".into());
                let _ = core.app.emit(
                    "update",
                    json!({
                        "tools": tools,
                        "cmds": core.cmds.load(Ordering::Relaxed),
                        "logs_mb": logs_bytes as f64 / 1_048_576.0,
                        "uptime_ms": core.started.elapsed().as_millis() as u64,
                        "cost_usd": core.claude.cost_usd(),
                        "jobs": core.jobs.running_count(),
                        "instances": [{
                            "name": format!("{name} \u{00b7} {}", core.claude.model()),
                            "host": "device",
                            "status": status,
                            "logs": core.audit.entry_count(),
                            "seg": (1 + tools / 2).min(16),
                        }],
                    }),
                );
            }
        });
    }

    pub fn shutdown(&self) {
        tracing::info!("shutting down");
        self.approvals.deny_all();
        self.claude.kill();
        self.jobs.kill_all();
        self.term_claude.kill();
        self.term_you.kill();
        self.audit.close();
        let _ = std::fs::remove_dir_all(&self.run_dir);
    }
}

fn dir_size(p: &std::path::Path) -> u64 {
    std::fs::read_dir(p)
        .map(|rd| rd.filter_map(|e| e.ok()?.metadata().ok()).map(|m| m.len()).sum())
        .unwrap_or(0)
}

/// Build everything and start the background parts. Called from Tauri setup.
pub fn init(app: AppHandle) -> anyhow::Result<Arc<Core>> {
    let data_dir = crate::config::data_dir();
    std::fs::create_dir_all(&data_dir)?;
    let cfg = Config::load(&data_dir);
    let workspace = cfg.workspace_path();
    let shell = cfg.resolve_shell();
    let run_dir = data_dir.join("run");
    let _ = std::fs::remove_dir_all(&run_dir);
    std::fs::create_dir_all(&run_dir)?;

    let audit = Audit::open(&data_dir, &cfg.model, &workspace)?;
    let (listener, mcp_port) = crate::mcp::bind()?;
    let mcp_token = uuid::Uuid::new_v4().simple().to_string();

    let core = Arc::new(Core {
        risk: RiskRules::new(&cfg.risky_patterns),
        approval_mode: Mutex::new(cfg.approval),
        approvals: Approvals::default(),
        term_claude: Term::new("claude", shell.clone(), workspace.clone()),
        term_you: Term::new("you", shell.clone(), workspace.clone()),
        jobs: Jobs::default(),
        claude: Claude::new(&cfg.model),
        tools: AtomicU64::new(0),
        cmds: AtomicU64::new(0),
        started: Instant::now(),
        app: app.clone(),
        data_dir,
        run_dir,
        workspace,
        shell,
        audit,
        cfg,
        mcp_port,
        mcp_token,
    });

    for t in [&core.term_claude, &core.term_you] {
        if let Err(e) = t.spawn(&app) {
            tracing::error!("terminal {} failed to start: {e}", t.name);
            core.audit.error(&format!("terminal {} failed: {e}", t.name));
        }
    }
    crate::mcp::serve(Arc::clone(&core), listener);
    core.start_stats_ticker();
    // Warm up the claude process so the first message doesn't pay cold start.
    let c = Arc::clone(&core);
    tauri::async_runtime::spawn(async move {
        if let Err(e) = c.claude.ensure_started(&c).await {
            c.feed("top", "log", tag::amber("error"), &e);
        }
    });
    Ok(core)
}
