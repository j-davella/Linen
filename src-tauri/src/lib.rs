mod approval;
mod audit;
mod claude;
mod config;
mod core;
mod jobs;
mod mcp;
mod prompt;
mod risk;
mod term;

use crate::config::ApprovalMode;
use crate::core::Core;
use std::sync::Arc;
use tauri::{Manager, RunEvent, State};

type Linen<'a> = State<'a, Arc<Core>>;

// ─── Tauri commands (called from ui/index.html) ──────────────────────────────

#[tauri::command]
fn get_state(core: Linen<'_>) -> serde_json::Value {
    core.snapshot()
}

#[tauri::command]
async fn send_message(text: String, core: Linen<'_>) -> Result<(), String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(());
    }
    core.feed("top", "tool", core::tag::purple("you"), text);
    let core = Arc::clone(&core);
    core.claude.send(&core, text, true).await;
    Ok(())
}

#[tauri::command]
async fn stop(core: Linen<'_>) -> Result<(), String> {
    let core = Arc::clone(&core);
    core.claude.stop(&core).await;
    Ok(())
}

#[tauri::command]
async fn set_model(model: String, core: Linen<'_>) -> Result<(), String> {
    let core = Arc::clone(&core);
    core.claude.set_model(&core, model.trim()).await
}

#[tauri::command]
fn set_approval(mode: String, core: Linen<'_>) -> Result<(), String> {
    let m = ApprovalMode::parse(&mode).ok_or_else(|| format!("unknown approval mode {mode}"))?;
    *Core::lock(&core.approval_mode) = m;
    core.feed("top", "log", core::tag::amber("approvals"), m.label());
    Ok(())
}

#[tauri::command]
fn approval_decide(id: String, allow: bool, via: Option<String>, core: Linen<'_>) -> bool {
    tracing::info!("approval_decide from UI: id={id} allow={allow} via={}", via.as_deref().unwrap_or("?"));
    core.approvals.decide(&id, allow)
}

fn term<'a>(core: &'a Core, id: &str) -> &'a Arc<term::Term> {
    if id == "claude" {
        &core.term_claude
    } else {
        &core.term_you
    }
}

#[tauri::command]
fn term_input(id: String, data: String, core: Linen<'_>) {
    term(&core, &id).write(data.as_bytes());
}

#[tauri::command]
fn term_resize(id: String, cols: u16, rows: u16, core: Linen<'_>) {
    term(&core, &id).resize(cols, rows);
}

#[tauri::command]
fn term_replay(id: String, core: Linen<'_>) -> String {
    term(&core, &id).replay()
}

/// The shell bar under the dashboard types into the user's terminal.
#[tauri::command]
fn send_to_terminal(cmd: String, core: Linen<'_>) {
    let cmd = cmd.trim();
    if !cmd.is_empty() {
        core.term_you.write(format!("{cmd}\r").as_bytes());
    }
}

// ─── logging ─────────────────────────────────────────────────────────────────

fn init_logging() {
    let dir = config::data_dir().join("logs");
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(rd) = std::fs::read_dir(&dir) {
        let mut logs: Vec<_> = rd
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().map(|x| x == "log").unwrap_or(false))
            .collect();
        logs.sort();
        let excess = logs.len().saturating_sub(29);
        for old in &logs[..excess] {
            let _ = std::fs::remove_file(old);
        }
    }
    let file = dir.join(format!("{}.log", chrono::Local::now().format("%Y%m%d_%H%M%S")));
    let filter = tracing_subscriber::EnvFilter::try_from_env("LINEN_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,linen_lib=debug"));
    if let Ok(f) = std::fs::File::create(&file) {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_ansi(false)
            .with_writer(std::sync::Mutex::new(f))
            .try_init();
    }
}

// ─── entry ───────────────────────────────────────────────────────────────────

pub fn run() {
    init_logging();
    tauri::Builder::default()
        // Two Linens would fight over ~/.linen/run and the terminals: a second
        // launch just focuses the running window.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.unminimize();
                let _ = w.show();
                let _ = w.set_focus();
            }
        }))
        .setup(|app| {
            let core = core::init(app.handle().clone())?;
            app.manage(core);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_state,
            send_message,
            stop,
            set_model,
            set_approval,
            approval_decide,
            term_input,
            term_resize,
            term_replay,
            send_to_terminal,
        ])
        .build(tauri::generate_context!())
        .expect("error building Linen")
        .run(|app, event| {
            if let RunEvent::Exit = event {
                if let Some(core) = app.try_state::<Arc<Core>>() {
                    core.shutdown();
                }
            }
        });
}
