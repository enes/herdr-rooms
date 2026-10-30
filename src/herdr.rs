//! Thin wrapper over the `herdr` CLI. Every call inherits HERDR_SOCKET_PATH,
//! so it talks to the session this process runs in.

use anyhow::{anyhow, Result};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug)]
pub struct HerdrError {
    pub code: String,
    pub message: String,
}

impl std::fmt::Display for HerdrError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}

impl std::error::Error for HerdrError {}

pub fn error_code(err: &anyhow::Error) -> Option<&str> {
    err.downcast_ref::<HerdrError>().map(|e| e.code.as_str())
}

fn bin() -> String {
    if let Some(path) = std::env::var("HERDR_BIN_PATH").ok().filter(|s| !s.is_empty()) {
        return path;
    }
    // Codex's app-server daemon runs hooks with its own PATH, which may lack ~/.local/bin.
    let on_path = std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join("herdr").is_file()));
    let local = std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/bin/herdr"));
    match local {
        Some(local) if !on_path && local.is_file() => local.to_string_lossy().into_owned(),
        _ => "herdr".into(),
    }
}

pub fn call(args: &[&str]) -> Result<Value> {
    call_on(None, args)
}

/// `call` against the herdr server at `socket` instead of the one this process runs in.
fn call_on(socket: Option<&str>, args: &[&str]) -> Result<Value> {
    let mut cmd = Command::new(bin());
    if let Some(socket) = socket {
        cmd.env("HERDR_SOCKET_PATH", socket);
    }
    let out = cmd.args(args).output()?;
    let parse = |bytes: &[u8]| serde_json::from_slice::<Value>(bytes).ok();
    let body = parse(&out.stdout).or_else(|| parse(&out.stderr));
    if let Some(err) = body.as_ref().and_then(|v| v.get("error")) {
        return Err(HerdrError {
            code: err["code"].as_str().unwrap_or_default().into(),
            message: err["message"].as_str().unwrap_or_default().into(),
        }
        .into());
    }
    if !out.status.success() {
        return Err(anyhow!("herdr {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(body.unwrap_or(Value::Null))
}

pub struct AgentInfo {
    pub kind: String,
    pub status: String,
    pub cwd: String,
}

pub fn agent_get(target: &str) -> Result<AgentInfo> {
    let v = call(&["agent", "get", target])?;
    let a = &v["result"]["agent"];
    Ok(AgentInfo {
        kind: a["agent"].as_str().unwrap_or_default().into(),
        status: a["agent_status"].as_str().unwrap_or("unknown").into(),
        cwd: a["cwd"].as_str().unwrap_or_default().into(),
    })
}

pub struct Located {
    pub pane: String,
    pub socket: String,
    pub focused: bool,
}

/// Agents of `kind` working in `cwd`, on every running herdr server. For a process that
/// lost its pane's HERDR_* variables: a Codex session on Codex's shared app-server daemon
/// runs `!` commands and hooks in the daemon, not in its pane.
pub fn agents_in(kind: &str, cwd: &Path) -> Vec<Located> {
    let same = |dir: &str| {
        let canon = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
        !dir.is_empty() && canon(Path::new(dir)) == canon(cwd)
    };
    let Ok(list) = call(&["session", "list", "--json"]) else { return vec![] };
    let mut found = vec![];
    for session in list["sessions"].as_array().into_iter().flatten().filter(|s| s["running"] == true) {
        let Some(socket) = session["socket_path"].as_str() else { continue };
        let Ok(agents) = call_on(Some(socket), &["agent", "list"]) else { continue };
        for a in agents["result"]["agents"].as_array().into_iter().flatten() {
            let in_cwd = same(a["cwd"].as_str().unwrap_or_default()) || same(a["foreground_cwd"].as_str().unwrap_or_default());
            if a["agent"] == kind && in_cwd {
                found.push(Located {
                    pane: a["pane_id"].as_str().unwrap_or_default().into(),
                    socket: socket.into(),
                    focused: a["focused"] == true,
                });
            }
        }
    }
    found
}

/// The ids of the panes of the herdr server this process runs in.
pub fn pane_ids() -> Result<Vec<String>> {
    let v = call(&["pane", "list"])?;
    Ok(v["result"]["panes"].as_array().into_iter().flatten().filter_map(|p| p["pane_id"].as_str().map(String::from)).collect())
}

pub fn pane_split(target: &str, direction: &str, cwd: &str) -> Result<String> {
    let v = call(&["pane", "split", target, "--direction", direction, "--cwd", cwd, "--no-focus"])?;
    v["result"]["pane"]["pane_id"].as_str().map(String::from).ok_or_else(|| anyhow!("pane split returned no pane id"))
}

pub fn agent_start(name: &str, kind: &str, pane: &str, extra: &[String]) -> Result<()> {
    let mut args = vec!["agent", "start", name, "--kind", kind, "--pane", pane, "--timeout", "120000", "--"];
    args.extend(extra.iter().map(String::as_str));
    call(&args).map(|_| ())
}

/// Submits a prompt and reports whether the agent visibly started on it (`true`) or showed
/// no activity at all (`false`): an agent that is still booting can swallow input.
pub fn agent_prompt_started(target: &str, text: &str) -> Result<bool> {
    // The CLI has no `--` separator: keep a leading dash from reading as an option.
    let text = if text.starts_with('-') { format!(" {text}") } else { text.to_string() };
    match call(&["agent", "prompt", target, &text, "--wait", "--timeout", "15000"]) {
        Ok(_) => Ok(true),
        Err(e) => match error_code(&e) {
            // Activity was seen and the turn is still running.
            Some("timeout") => Ok(true),
            Some("agent_prompt_stalled") => Ok(false),
            _ => Err(e),
        },
    }
}

/// Presses Enter and reports whether that started a turn: it submits text left in the
/// composer, and does nothing on an empty one.
pub fn agent_submit(target: &str) -> bool {
    call(&["agent", "send-keys", target, "enter"]).is_ok()
        && call(&["agent", "wait", target, "--until", "working", "--until", "blocked", "--timeout", "5000"]).is_ok()
}

/// Closes a pane, ending whatever runs in it.
pub fn close_pane(pane: &str) -> Result<()> {
    call(&["pane", "close", pane]).map(|_| ())
}

pub fn notify(title: &str, body: &str) {
    let _ = call(&["notification", "show", title, "--body", body, "--sound", "done"]);
}

pub fn open_plugin_pane(entrypoint: &str, extra: &[&str]) -> Result<()> {
    let mut args = vec!["plugin", "pane", "open", "--plugin", "herdr-rooms", "--entrypoint", entrypoint];
    args.extend_from_slice(extra);
    call(&args).map(|_| ())
}
