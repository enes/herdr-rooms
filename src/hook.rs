//! Agent-side hooks: Claude Code (UserPromptSubmit / Stop) and Codex (`notify`)
//! call `herdr-rooms hook <kind>`. A pane that is not part of a room exits
//! at once, so the hooks cost nothing in ordinary sessions.

use crate::state::{self, Event, EventKind};
use anyhow::Result;
use serde_json::Value;
use std::io::Read;

/// Codex runs a second, internal turn to title each new thread; its notify is not a reply.
const CODEX_TITLE_PROMPT: &str = "Generate a concise, single-line task title";

pub fn run(kind: &str, arg: Option<String>) -> Result<()> {
    let mut stdin = String::new();
    if kind.starts_with("claude") {
        let _ = std::io::stdin().read_to_string(&mut stdin);
    }
    let map = match std::env::var("HERDR_PANE_ID").ok().filter(|s| !s.is_empty()) {
        Some(pane) => state::lookup_pane(&pane),
        None if kind == "codex-notify" => codex_daemon_host(arg.as_deref().unwrap_or_default()),
        None => None,
    };
    let Some(map) = map else { return Ok(()) };

    let push_with = |kind: EventKind, text: &str, background: u32| -> Result<()> {
        if text.trim().is_empty() {
            return Ok(());
        }
        let ev = Event { ts: state::now_ms(), role: map.role, kind, text: text.to_string(), background };
        state::append_event(&map.id, &ev)
    };
    let push = |kind: EventKind, text: &str| push_with(kind, text, 0);

    match kind {
        "claude-prompt" => {
            let v: Value = serde_json::from_str(&stdin)?;
            push(EventKind::User, v["prompt"].as_str().unwrap_or_default())?;
        }
        "claude-stop" => {
            let v: Value = serde_json::from_str(&stdin)?;
            let text = match v["last_assistant_message"].as_str() {
                Some(t) => t.to_string(),
                None => v["transcript_path"].as_str().map(last_assistant_text).unwrap_or_default(),
            };
            push_with(EventKind::Assistant, &text, waiting_on(&v))?;
        }
        "codex-notify" => {
            let v: Value = serde_json::from_str(arg.as_deref().unwrap_or_default())?;
            if v["type"] != "agent-turn-complete" {
                return Ok(());
            }
            let inputs: Vec<&str> =
                v["input-messages"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
            if inputs.first().is_some_and(|s| s.starts_with(CODEX_TITLE_PROMPT)) {
                return Ok(());
            }
            // Codex lists every user message of the thread so far; the last one started this turn.
            if let Some(input) = inputs.last() {
                push(EventKind::User, input)?;
            }
            push(EventKind::Assistant, v["last-assistant-message"].as_str().unwrap_or_default())?;
        }
        _ => {}
    }
    Ok(())
}

/// A Codex session on Codex's app-server daemon runs notify in the daemon, without its
/// pane's variables. The room starts every Codex reviewer in its own pane's process, so
/// such a session is a host: the one Codex agent hosting a room in the turn's folder.
fn codex_daemon_host(notify: &str) -> Option<state::PaneMap> {
    if !state::any_pane_mapped() {
        return None;
    }
    let v: Value = serde_json::from_str(notify).ok()?;
    let cwd = std::path::Path::new(v["cwd"].as_str()?);
    let hosts: Vec<state::PaneMap> = crate::herdr::agents_in("codex", cwd)
        .into_iter()
        .filter_map(|a| state::lookup_pane_on(&a.pane, &a.socket))
        .filter(|m| m.role == state::Role::Host)
        .collect();
    let [host] = <[_; 1]>::try_from(hosts).ok()?;
    Some(host)
}

/// Background subagents and workflows Claude Code reports at Stop: each one reports back
/// and wakes the session for another turn. Background shells and monitors are left out,
/// as a dev server or a watcher may never end.
fn waiting_on(stop: &Value) -> u32 {
    let tasks = stop["background_tasks"].as_array().into_iter().flatten();
    tasks.filter(|t| matches!(t["type"].as_str(), Some("subagent" | "workflow"))).count() as u32
}

/// Fallback for Claude Code builds whose Stop input lacks `last_assistant_message`:
/// the assistant text after the last real user message in the transcript.
fn last_assistant_text(path: &str) -> String {
    let Ok(text) = std::fs::read_to_string(path) else { return String::new() };
    let mut parts: Vec<String> = vec![];
    for line in text.lines() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else { continue };
        let content = &entry["message"]["content"];
        match entry["type"].as_str() {
            Some("user") => {
                let is_tool_result = content
                    .as_array()
                    .is_some_and(|blocks| blocks.iter().any(|b| b["type"] == "tool_result"));
                if !is_tool_result {
                    parts.clear();
                }
            }
            Some("assistant") => {
                for block in content.as_array().into_iter().flatten() {
                    if block["type"] == "text" {
                        parts.push(block["text"].as_str().unwrap_or_default().to_string());
                    }
                }
            }
            _ => {}
        }
    }
    parts.join("\n\n")
}
