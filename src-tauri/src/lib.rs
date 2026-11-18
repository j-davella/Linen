mod agent;
mod audit;
mod terminal;

use std::sync::{Arc, Mutex};
use std::time::Instant;
use tauri::{Emitter, Manager, State};

use agent::{Agent, FeedLine};
use audit::AuditSession;
use terminal::Terminal;

pub struct AppState {
    agent: Agent,
    start: Instant,
}

// ── Tauri commands ────────────────────────────────────────────────────────────

#[tauri::command]
fn send_message(text: String, state: State<'_, AppState>) {
    if !text.trim().is_empty() {
        state.agent.send(text);
    }
}

#[tauri::command]
async fn send_to_terminal(cmd: String) {
    if !cmd.trim().is_empty() {
        Terminal::send_to_user(&cmd).await;
    }
}

#[tauri::command]
async fn get_terminal_snapshot() -> String {
    Terminal::get_snapshot().await
}

// ── App setup ─────────────────────────────────────────────────────────────────

pub fn run() {
    tauri::Builder::default()
        .setup(|app| {
            let handle = app.handle().clone();
            let data_dir = dirs::home_dir()
                .unwrap_or_default()
                .join(".linen");

            // Start tmux session
            let handle2 = handle.clone();
            tokio::spawn(async move {
                if let Err(e) = Terminal::start().await {
                    handle2.emit("feed_line", FeedLine {
                        pane:   "top".into(),
                        ev:     "log".into(),
                        prefix: r#"<span class="y">warn</span> "#.into(),
                        text:   format!("tmux start failed: {}", e),
                    }).ok();
                }
                // Poll terminal and push last lines to UI every 1s
                loop {
                    tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                    let lines = Terminal::get_last_lines(5).await;
                    for line in lines.iter().rev().take(1) {
                        handle2.emit("feed_line", FeedLine {
                            pane:   "bot".into(),
                            ev:     "log".into(),
                            prefix: r#"<span class="g">tmux</span> "#.into(),
                            text:   line.chars().take(80).collect(),
                        }).ok();
                    }
                }
            });

            let audit = Arc::new(Mutex::new(
                AuditSession::open(&data_dir, "claude-sonnet-4-6")
                    .expect("failed to open audit db"),
            ));

            let agent = Agent::new(handle.clone(), audit);
            app.manage(AppState { agent, start: Instant::now() });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            send_message,
            send_to_terminal,
            get_terminal_snapshot,
        ])
        .run(tauri::generate_context!())
        .expect("error running Linen");
}
