# Linen v0.2

A co-working desktop shell: chat with Claude Code on the left of the deck, and
watch it work in a **real terminal** on the right. You can type into the same
terminal (to answer a `sudo` prompt, say), and risky commands wait for your click.

Rust + Tauri 2. Runs on **Windows** (ConPTY, PowerShell by default) and **NixOS/Linux** (pty, `$SHELL`).

## Run

```
run.bat          # Windows
./run.sh         # Linux / NixOS (uses shell.nix automatically on NixOS)
```

Needs: Rust stable, the `claude` CLI **logged in** (`claude`, then `/login`), and
WebView2 on Windows (built into Windows 11).

## Using it

- **Message bar**: talk to Claude. While it works, the send button becomes ■ Stop (or press Esc).
- **CLAUDE tab**: the terminal Claude's commands run in. Type into it any time.
- **YOU tab**: your own shell. Claude can *read* this screen (when you say "look at
  my terminal") but never types into it. The green bar at the bottom types here too.
- **Approval card**: risky commands pause here. Click Approve / Deny, or press `Y` / `N`.
  The APPROVE picker in the top bar switches between `risky` (default), `all`, and `off`.
- **MODEL picker**: switches model (the conversation carries over).
- Drop files onto the window to paste their quoted paths into your message.
- Keys (when not typing): `O` observe, `C` command, `S` shell (big terminal), `F` focus, `R` radar, `/` jump to the message bar.
- Terminal: `Ctrl+C` copies when there's a selection, `Ctrl+Shift+C/V` copy/paste, right-click copies or pastes.

## Files (`~/.linen/`)

| | |
|---|---|
| `config.toml` | workspace folder, shell, claude path, models, approval mode, extra risky patterns |
| `context.md` | optional notes prepended to Claude's instructions on every launch |
| `linen.db` | audit log: every message, tool call, command, approval decision, exit code |
| `logs/` | app logs (last 30 launches) |

## How it works (and what changed from v0.1/v0.1-rust)

| | v0.1 (Python) / first Rust attempt | v0.2 |
|---|---|---|
| Claude's commands | `--tools ""` + `<RUN>…</RUN>` tags parsed with a regex, one per turn, a new `claude` process per step | Linen serves its tools over **MCP** (`run_command`, `read_terminal`, `start_job`, `job_status`, `stop_job`, `list_jobs`). Claude calls them natively, several per turn, from **one long-lived** `claude` process |
| Terminal | pyte screen scraping / tmux `capture-pane` + sentinel `echo` | Real PTY + xterm.js. Output for Claude comes from a runner script that tees to a file and writes the exit code, so no screen scraping |
| Safety | `--dangerously-skip-permissions`, everything auto-runs | `--restricted`: no built-in Bash; file tools confined to the workspace; risky commands need a click |
| Platform | NixOS only (hard-coded `/run/current-system` paths) | Windows + Linux |

Source map: `src-tauri/src/` — `core.rs` (shared state, approval gate, stats),
`claude.rs` (the CLI process and its stream-json), `mcp.rs` (tool server),
`term.rs` (PTYs and command runner), `jobs.rs`, `risk.rs` (approval patterns, with tests),
`audit.rs`, `config.rs`, `prompt.rs`. UI: `ui/index.html` (+ vendored xterm.js).

`cargo test --manifest-path src-tauri/Cargo.toml` runs the unit tests.
