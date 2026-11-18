//! User config (~/.linen/config.toml) and platform resolution (shell, claude binary).

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalMode {
    /// Pause only for commands matching the risk rules.
    Risky,
    /// Pause for every command and job.
    All,
    /// Never pause (v1 behaviour).
    Off,
}

impl ApprovalMode {
    pub fn label(self) -> &'static str {
        match self {
            ApprovalMode::Risky => "risky",
            ApprovalMode::All => "all",
            ApprovalMode::Off => "off",
        }
    }
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "risky" => Some(Self::Risky),
            "all" => Some(Self::All),
            "off" => Some(Self::Off),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub workspace: String,
    pub shell: String,
    pub claude_bin: String,
    pub model: String,
    pub models: Vec<String>,
    pub approval: ApprovalMode,
    pub risky_patterns: Vec<String>,
    pub web_tools: bool,
    pub command_timeout_s: u64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            workspace: String::new(),
            shell: String::new(),
            claude_bin: String::new(),
            model: "sonnet".into(),
            models: vec!["sonnet".into(), "opus".into(), "haiku".into(), "fable".into()],
            approval: ApprovalMode::Risky,
            risky_patterns: Vec::new(),
            web_tools: true,
            command_timeout_s: 120,
        }
    }
}

const DEFAULT_TOML: &str = r#"# Linen config. Restart Linen after editing.

# Folder Claude works in. Its file tools (Read/Write/Edit) are confined here.
# Empty = your home folder.
workspace = ""

# Shell for both terminals. Empty = auto
# (Windows: pwsh if installed, else powershell.exe; Linux: $SHELL, else bash).
# Examples: "C:/Program Files/Git/bin/bash.exe", "/run/current-system/sw/bin/bash"
shell = ""

# Path to the claude CLI. Empty = find it on PATH.
claude_bin = ""

# Model alias or full name, and the list shown in the model picker.
model = "sonnet"
models = ["sonnet", "opus", "haiku", "fable"]

# When Claude's commands wait for your click:
#   "risky" = only commands matching the risk rules (sudo, rm -r, git push, ...)
#   "all"   = every command and background job
#   "off"   = never (like Linen v1)
approval = "risky"

# Extra regexes (case-insensitive) that count as risky, e.g. ["\\bterraform\\s+apply\\b"]
risky_patterns = []

# Let Claude use WebSearch / WebFetch.
web_tools = true

# Default run_command timeout in seconds (Claude can ask for longer per command).
command_timeout_s = 120
"#;

pub fn data_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".linen")
}

impl Config {
    pub fn load(dir: &Path) -> Config {
        let path = dir.join("config.toml");
        if !path.exists() {
            let _ = std::fs::write(&path, DEFAULT_TOML);
        }
        match std::fs::read_to_string(&path) {
            Ok(text) => match toml::from_str::<Config>(&text) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!("config.toml invalid, using defaults: {e}");
                    Config::default()
                }
            },
            Err(_) => Config::default(),
        }
    }

    pub fn workspace_path(&self) -> PathBuf {
        let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
        if self.workspace.trim().is_empty() {
            return home;
        }
        let p = expand_tilde(self.workspace.trim());
        if p.is_dir() {
            p
        } else {
            tracing::warn!("workspace {:?} not found, using home", p);
            home
        }
    }

    pub fn resolve_claude(&self) -> Result<PathBuf> {
        if !self.claude_bin.trim().is_empty() {
            let p = expand_tilde(self.claude_bin.trim());
            if p.exists() {
                return Ok(p);
            }
            return which::which(self.claude_bin.trim())
                .map_err(|_| anyhow!("claude_bin {:?} not found", self.claude_bin));
        }
        if let Ok(p) = which::which("claude") {
            return Ok(p);
        }
        // Common install locations not always on a GUI app's PATH.
        let home = dirs::home_dir().unwrap_or_default();
        let candidates = [
            home.join(".local").join("bin").join(exe("claude")),
            home.join(".claude").join("local").join(exe("claude")),
            PathBuf::from("/run/current-system/sw/bin/claude"),
            home.join(".nix-profile").join("bin").join("claude"),
        ];
        candidates
            .into_iter()
            .find(|p| p.exists())
            .ok_or_else(|| anyhow!("claude CLI not found on PATH; set claude_bin in ~/.linen/config.toml"))
    }

    pub fn resolve_shell(&self) -> Shell {
        let program: PathBuf = if !self.shell.trim().is_empty() {
            let p = expand_tilde(self.shell.trim());
            if p.exists() {
                p
            } else {
                which::which(self.shell.trim()).unwrap_or(p)
            }
        } else if cfg!(windows) {
            which::which("pwsh").unwrap_or_else(|_| PathBuf::from("powershell.exe"))
        } else {
            std::env::var_os("SHELL")
                .map(PathBuf::from)
                .filter(|p| p.exists())
                .or_else(|| which::which("bash").ok())
                .unwrap_or_else(|| PathBuf::from("/bin/sh"))
        };
        Shell::from_program(program)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellKind {
    /// bash / zsh / sh: commands are sourced so cwd and env persist.
    Posix,
    /// fish and other non-POSIX interactive shells: commands run in a bash child.
    Foreign,
    PowerShell,
}

#[derive(Debug, Clone)]
pub struct Shell {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub kind: ShellKind,
    pub label: String,
}

impl Shell {
    fn from_program(program: PathBuf) -> Shell {
        let stem = program
            .file_stem()
            .map(|s| s.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        let (kind, label) = match stem.as_str() {
            "pwsh" => (ShellKind::PowerShell, "PowerShell 7"),
            "powershell" => (ShellKind::PowerShell, "Windows PowerShell"),
            "bash" => (ShellKind::Posix, "bash"),
            "zsh" => (ShellKind::Posix, "zsh"),
            "sh" | "dash" => (ShellKind::Posix, "sh"),
            "fish" => (ShellKind::Foreign, "fish"),
            _ => (ShellKind::Foreign, "shell"),
        };
        let args = match kind {
            ShellKind::PowerShell => vec!["-NoLogo".into(), "-ExecutionPolicy".into(), "Bypass".into()],
            ShellKind::Posix if cfg!(windows) => vec!["--login".into(), "-i".into()],
            _ => vec![],
        };
        Shell { program, args, kind, label: label.to_string() }
    }

    /// Program + args for running a one-shot command outside the PTY (background jobs).
    pub fn job_command(&self, cmd: &str) -> (PathBuf, Vec<String>) {
        match self.kind {
            ShellKind::PowerShell => (
                self.program.clone(),
                vec![
                    "-NoLogo".into(),
                    "-NoProfile".into(),
                    "-ExecutionPolicy".into(),
                    "Bypass".into(),
                    "-Command".into(),
                    cmd.into(),
                ],
            ),
            ShellKind::Posix => (self.program.clone(), vec!["-c".into(), cmd.into()]),
            ShellKind::Foreign => {
                let bash = which::which("bash").unwrap_or_else(|_| PathBuf::from("/bin/sh"));
                (bash, vec!["-c".into(), cmd.into()])
            }
        }
    }

    /// Which language Claude should write commands in.
    pub fn syntax_label(&self) -> &'static str {
        match self.kind {
            ShellKind::PowerShell => "PowerShell",
            _ => "bash",
        }
    }
}

pub fn os_label() -> &'static str {
    if cfg!(windows) {
        "WINDOWS"
    } else if std::path::Path::new("/etc/NIXOS").exists() {
        "NIXOS"
    } else if cfg!(target_os = "macos") {
        "MACOS"
    } else {
        "LINUX"
    }
}

fn exe(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

fn expand_tilde(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/").or_else(|| s.strip_prefix("~\\")) {
        if let Some(h) = dirs::home_dir() {
            return h.join(rest);
        }
    }
    PathBuf::from(s)
}
