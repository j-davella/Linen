//! Linen's tools, served to the claude CLI over MCP (streamable HTTP, JSON
//! responses only) on 127.0.0.1 with a per-launch bearer token.
//!
//! This replaces v1's <RUN>/<WRITE> tag protocol: Claude calls these as
//! native tools, can make several per turn, and gets structured results.

use crate::core::{tag, Core};
use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use serde_json::{json, Value};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub fn bind() -> anyhow::Result<(std::net::TcpListener, u16)> {
    let l = std::net::TcpListener::bind("127.0.0.1:0")?;
    l.set_nonblocking(true)?;
    let port = l.local_addr()?.port();
    Ok((l, port))
}

pub fn serve(core: Arc<Core>, listener: std::net::TcpListener) {
    tauri::async_runtime::spawn(async move {
        let listener = match tokio::net::TcpListener::from_std(listener) {
            Ok(l) => l,
            Err(e) => {
                tracing::error!("mcp listener: {e}");
                return;
            }
        };
        let app = Router::new()
            .route(
                "/mcp",
                post(handle)
                    .get(|| async { StatusCode::METHOD_NOT_ALLOWED })
                    .delete(|| async { StatusCode::OK }),
            )
            .with_state(core);
        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!("mcp server stopped: {e}");
        }
    });
}

async fn handle(State(core): State<Arc<Core>>, headers: HeaderMap, body: Bytes) -> Response {
    let expected = format!("Bearer {}", core.mcp_token);
    let ok = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(|v| v == expected)
        .unwrap_or(false);
    if !ok {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let msg: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return Json(rpc_error(Value::Null, -32700, &format!("parse error: {e}"))).into_response(),
    };
    if let Value::Array(items) = msg {
        let mut out = Vec::new();
        for m in items {
            if let Some(r) = dispatch(&core, m).await {
                out.push(r);
            }
        }
        return if out.is_empty() {
            StatusCode::ACCEPTED.into_response()
        } else {
            Json(Value::Array(out)).into_response()
        };
    }
    match dispatch(&core, msg).await {
        Some(r) => Json(r).into_response(),
        None => StatusCode::ACCEPTED.into_response(),
    }
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

async fn dispatch(core: &Arc<Core>, msg: Value) -> Option<Value> {
    let id = msg.get("id").cloned()?; // notifications get no response
    let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    let result = match method {
        "initialize" => json!({
            "protocolVersion": params.get("protocolVersion").and_then(|v| v.as_str()).unwrap_or("2025-06-18"),
            "capabilities": {"tools": {"listChanged": false}},
            "serverInfo": {"name": "linen", "version": env!("CARGO_PKG_VERSION")},
            "instructions": "Linen's shared terminal and background jobs. Use run_command for all shell work."
        }),
        "ping" => json!({}),
        "tools/list" => json!({"tools": tool_list()}),
        "tools/call" => {
            let name = params.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            let (text, is_error) = call(core, name, &args).await;
            json!({"content": [{"type": "text", "text": text}], "isError": is_error})
        }
        "resources/list" => json!({"resources": []}),
        "prompts/list" => json!({"prompts": []}),
        _ => return Some(rpc_error(id, -32601, &format!("method not found: {method}"))),
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
}

fn tool_list() -> Value {
    json!([
        {
            "name": "run_command",
            "description": "Run a shell command in Linen's CLAUDE terminal (visible to the user) and wait for it. \
                Shell state (cwd, variables) persists between calls. Returns combined stdout/stderr and the exit code. \
                Risky commands wait for the user's approval. For work over a few minutes use start_job instead.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "The command (may be multi-line)."},
                    "timeout_s": {"type": "integer", "minimum": 5, "maximum": 3600,
                                  "description": "Seconds to wait before interrupting. Extended automatically while output is still flowing."}
                },
                "required": ["command"]
            }
        },
        {
            "name": "read_terminal",
            "description": "Read the text currently on screen in one of Linen's terminals. Default is the user's own terminal ('you').",
            "inputSchema": {
                "type": "object",
                "properties": {"which": {"type": "string", "enum": ["you", "claude"]}}
            }
        },
        {
            "name": "start_job",
            "description": "Start a background job (build, install, test suite, dev server). Returns at once with a job id; \
                you are messaged automatically with the result when it finishes.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "command": {"type": "string"},
                    "name": {"type": "string", "description": "Short id for the job, e.g. 'build'."}
                },
                "required": ["command"]
            }
        },
        {
            "name": "job_status",
            "description": "State and recent output of a background job.",
            "inputSchema": {
                "type": "object",
                "properties": {"id": {"type": "string"}, "lines": {"type": "integer", "minimum": 1, "maximum": 500}},
                "required": ["id"]
            }
        },
        {
            "name": "stop_job",
            "description": "Stop a running background job.",
            "inputSchema": {"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"]}
        },
        {
            "name": "list_jobs",
            "description": "List background jobs and their states.",
            "inputSchema": {"type": "object", "properties": {}}
        }
    ])
}

fn arg_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty())
}

async fn call(core: &Arc<Core>, name: &str, args: &Value) -> (String, bool) {
    match name {
        "run_command" => run_command(core, args).await,
        "read_terminal" => {
            let which = arg_str(args, "which").unwrap_or("you");
            let term = if which == "claude" { &core.term_claude } else { &core.term_you };
            core.feed("top", "tool", tag::purple("claude"), &format!("reading {which} terminal"));
            let text = term.screen_text();
            if text.is_empty() {
                ("[terminal is empty]".into(), false)
            } else {
                (text, false)
            }
        }
        "start_job" => {
            let Some(cmd) = arg_str(args, "command") else {
                return ("command is required".into(), true);
            };
            core.cmds.fetch_add(1, Ordering::Relaxed);
            core.feed("bot", "shell", tag::green("job"), cmd);
            let audit_id = core.audit.command_start("job", cmd);
            match core.gate("job", cmd).await {
                Ok(d) => core.audit.command_decision(audit_id, d),
                Err(reason) => {
                    core.audit.command_decision(audit_id, "denied");
                    return (denied_text(&reason), true);
                }
            }
            match core.jobs.start(core, arg_str(args, "name"), cmd, audit_id) {
                Ok(id) => (
                    format!("Started job '{id}'. You'll get a message with its output when it finishes; job_status shows progress."),
                    false,
                ),
                Err(e) => (e, true),
            }
        }
        "job_status" => {
            let id = arg_str(args, "id").unwrap_or("");
            let lines = args.get("lines").and_then(|v| v.as_u64()).unwrap_or(40).clamp(1, 500) as usize;
            match core.jobs.status(id, lines) {
                Some(s) => (s, false),
                None => (format!("No job '{id}'. Jobs:\n{}", core.jobs.list()), true),
            }
        }
        "stop_job" => {
            let id = arg_str(args, "id").unwrap_or("");
            if core.jobs.stop(id) {
                core.feed("bot", "log", tag::amber("stop"), id);
                (format!("Stopping job '{id}'."), false)
            } else {
                (format!("Job '{id}' is not running."), true)
            }
        }
        "list_jobs" => (core.jobs.list(), false),
        _ => (format!("unknown tool {name}"), true),
    }
}

fn denied_text(reason: &str) -> String {
    format!(
        "The user denied this ({reason}). Do not retry it in another form; \
         explain what you wanted to do and ask how they'd like to proceed."
    )
}

async fn run_command(core: &Arc<Core>, args: &Value) -> (String, bool) {
    let Some(cmd) = arg_str(args, "command") else {
        return ("command is required".into(), true);
    };
    let timeout_s = args
        .get("timeout_s")
        .and_then(|v| v.as_u64())
        .unwrap_or(core.cfg.command_timeout_s)
        .clamp(5, 3600);

    core.cmds.fetch_add(1, Ordering::Relaxed);
    core.feed("bot", "shell", tag::cyan("$"), cmd);
    let audit_id = core.audit.command_start("run", cmd);
    match core.gate("command", cmd).await {
        Ok(d) => core.audit.command_decision(audit_id, d),
        Err(reason) => {
            core.audit.command_decision(audit_id, "denied");
            return (denied_text(&reason), true);
        }
    }

    let started = Instant::now();
    let core_cb = Arc::clone(core);
    let outcome = core
        .term_claude
        .run(cmd, Duration::from_secs(timeout_s), &core.run_dir, move || {
            core_cb.feed(
                "top",
                "log",
                tag::amber("input"),
                "A command is waiting for input (password?). Type it in the CLAUDE terminal.",
            );
            core_cb.emit("attention", "claude");
        })
        .await;

    let last = outcome.output.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("");
    let code_label = match outcome.exit_code {
        Some(c) if outcome.timed_out => format!("timed out, interrupted, exit {c}"),
        Some(c) => format!("exit {c}"),
        None if outcome.shell_exited => "shell exited".into(),
        None if outcome.timed_out => "timed out, still running".into(),
        None => "no exit code".into(),
    };
    core.feed("bot", "log", tag::green(&code_label), last);
    core.audit.command_done(
        audit_id,
        &outcome.output,
        outcome.exit_code.map(i64::from),
        started.elapsed().as_millis() as i64,
    );

    // A non-zero exit is a normal result; only "never finished" is a tool error.
    let text = format!("{}\n\n[{}]", outcome.output, code_label);
    (text, outcome.exit_code.is_none())
}
