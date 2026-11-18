//! Pending approval requests: a tool call waits on a oneshot until the user
//! clicks Approve / Deny in the UI (or 15 minutes pass, which counts as Deny).

use serde::Serialize;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;
use tauri::{AppHandle, Emitter};
use tokio::sync::oneshot;

const TIMEOUT: Duration = Duration::from_secs(15 * 60);

#[derive(Default)]
pub struct Approvals {
    pending: Mutex<HashMap<String, oneshot::Sender<bool>>>,
}

#[derive(Clone, Serialize)]
struct Request<'a> {
    id: &'a str,
    kind: &'a str,
    detail: &'a str,
    reason: &'a str,
}

impl Approvals {
    pub async fn request(&self, app: &AppHandle, kind: &str, detail: &str, reason: &str) -> bool {
        let id = uuid::Uuid::new_v4().simple().to_string()[..10].to_string();
        let (tx, rx) = oneshot::channel();
        self.map().insert(id.clone(), tx);
        let _ = app.emit("approval_request", Request { id: &id, kind, detail, reason });
        tracing::info!("approval {id} requested: {kind} {detail:.120} ({reason})");
        let decision = tokio::time::timeout(TIMEOUT, rx).await;
        self.map().remove(&id);
        let _ = app.emit("approval_closed", &id);
        tracing::info!("approval {id} resolved: {decision:?}");
        matches!(decision, Ok(Ok(true)))
    }

    pub fn decide(&self, id: &str, allow: bool) -> bool {
        tracing::info!("approval {id} decided by user: allow={allow}");
        match self.map().remove(id) {
            Some(tx) => tx.send(allow).is_ok(),
            None => false,
        }
    }

    /// Deny everything outstanding (used when the user hits Stop).
    pub fn deny_all(&self) {
        for (_, tx) in self.map().drain() {
            let _ = tx.send(false);
        }
    }

    pub fn waiting(&self) -> bool {
        !self.map().is_empty()
    }

    fn map(&self) -> std::sync::MutexGuard<'_, HashMap<String, oneshot::Sender<bool>>> {
        self.pending.lock().unwrap_or_else(|e| e.into_inner())
    }
}
