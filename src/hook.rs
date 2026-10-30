//! Agent-side hooks: Claude Code (UserPromptSubmit / Stop / SessionStart) and Codex (`notify`)
//! call `herdr-rooms hook <kind>`. A pane that is not part of a room exits
//! at once, so the hooks cost nothing in ordinary sessions.

use crate::state::{self, Event, EventKind};
use anyhow::Result;
use serde_json::Value;
use std::io::Read;

/// Codex runs internal turns of its own, each in a thread of its own, and notify reports
/// them like any turn: the title of a new thread, and a catch-up for a user coming back.
pub(crate) const CODEX_INTERNAL_PROMPTS: [&str; 2] =
    ["Generate a concise, single-line task title", "Write a brief catch-up for a user returning to this task"];

pub fn run(kind: &str, arg: Option<String>) -> Result<()> {
    let mut stdin = String::new();
    if kind.starts_with("claude") {
        let _ = std::io::stdin().read_to_string(&mut stdin);
    }
    let raw = match kind {
        "codex-notify" => arg.clone().unwrap_or_default(),
        _ => stdin,
    };
    let v: Value = serde_json::from_str(&raw).map_err(|e| {
        state::hook_log(&format!("{kind}: unreadable input: {e}"));
        e
    })?;
    if kind == "codex-notify" && v["type"] != "agent-turn-complete" {
        return Ok(());
    }
    let inputs: Vec<&str> =
        v["input-messages"].as_array().map(|a| a.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
    // Codex's internal turns (a thread's title, a catch-up) run in threads of their own:
    // they are nobody's report, and must neither bind a pane nor ask to be bound to it.
    let internal = |s: &&str| CODEX_INTERNAL_PROMPTS.iter().any(|p| s.starts_with(p));
    if kind == "codex-notify" && (inputs.first().is_some_and(internal) || inputs.last().is_some_and(internal)) {
        return Ok(());
    }

    let pane = std::env::var("HERDR_PANE_ID").ok().filter(|s| !s.is_empty());
    // The report comes from the pane's own process, not routed from Codex's daemon.
    let in_pane = pane.is_some();
    let found = match &pane {
        Some(pane) => state::lookup_pane(pane).map(|map| Found { map, pane: pane.clone() }),
        None if kind == "codex-notify" => match codex_daemon_session(&v, &inputs) {
            Ok(found) => Some(found),
            Err(why) => {
                state::hook_log(&format!("codex-notify dropped: {why}"));
                None
            }
        },
        None => None,
    };
    let Some(Found { map, pane }) = found else { return Ok(()) };
    let session = v["session_id"].as_str().or(v["thread-id"].as_str()).map(String::from);
    // Claude Code's prompt_id, shared by a prompt's hook and the Stop that answers it;
    // Codex's turn-id, on both halves of a notify.
    let turn = v["prompt_id"].as_str().or(v["turn-id"].as_str()).map(String::from);
    // What a session said, for the room to show beside a stray one's id.
    let said = match kind {
        "claude-prompt" => v["prompt"].as_str().unwrap_or_default().to_string(),
        "claude-stop" => format!("(answered) {}", v["last_assistant_message"].as_str().unwrap_or_default()),
        _ => inputs.last().copied().unwrap_or_default().to_string(),
    };

    // The pane is one session's, and only its reports count: a process started from the
    // pane (a `claude -p`) and an agent's subagents inherit its variables and report for it
    // too. A session the map names takes the pane, and so does the agent's own new
    // conversation when its report says so for certain: Claude Code names the one `/clear`
    // starts (SessionStart, source `clear`; a `claude -p` starts as `startup` or `resume`),
    // and a thread Codex's TUI started, reported from the pane's own process, can only be
    // the pane agent's (after `/new`, `/resume` or a restart). Another session's report is
    // dropped and, when it is a conversation of the user's own, noted for the room (a
    // `claude` restarted in the pane, a Codex on its daemon: `/rebind` takes it). A Codex
    // pane the room has not bound yet takes the agent's own threads, not those it spawned
    // nor a `codex exec` run from the pane (its record says what ran it), and the first
    // thread its record calls the user's own binds it.
    match (&map.session, session.as_deref()) {
        (bound, Some(new)) if kind == "claude-session" && v["source"] == "clear" => {
            // The plugin's hook and the room's both report it: the second finds it done.
            if bound.as_deref() == Some(new) {
                return Ok(());
            }
            if !room_agent_is(&map, "claude") {
                state::hook_log(&format!("{kind} for room {} dropped: session {new} is not the room's agent, which is no Claude", map.id));
                return Ok(());
            }
            follow(&map, &pane, new, "/clear")?;
            return Ok(());
        }
        (Some(bound), Some(new))
            if new != bound && kind == "codex-notify" && in_pane && tui_thread(new) && room_agent_is(&map, "codex") =>
        {
            follow(&map, &pane, new, "/new, /resume or a restart")?;
        }
        (Some(bound), reported) if reported != Some(bound.as_str()) => {
            let reported = reported.unwrap_or("?");
            state::hook_log(&format!("{kind} for room {} dropped: session {reported} is not the pane's {bound}", map.id));
            let not_a_conversation = kind == "codex-notify" && thread_kind(reported).is_some_and(|s| s != "user");
            if reported != "?" && !not_a_conversation {
                state::note_stray(&map.id, map.role, reported, &said);
            }
            return Ok(());
        }
        (None, Some(thread)) if kind == "codex-notify" => match thread_kind(thread).as_deref() {
            Some("user") => {
                if let Err(e) = state::bind_pane(&pane, thread) {
                    state::hook_log(&format!("could not bind pane {pane} to thread {thread}: {e}"));
                }
            }
            Some(other) => {
                state::hook_log(&format!("codex-notify for room {} dropped: thread {thread} is not the agent's own conversation ({other} thread)", map.id));
                return Ok(());
            }
            // No record of the thread (an ephemeral one, or a CODEX_HOME the hook cannot
            // see): its report goes in, but it binds nothing.
            None => {}
        },
        _ => {}
    }

    let push_with = |kind: EventKind, text: &str, background: u32| -> Result<()> {
        if text.trim().is_empty() {
            state::hook_log(&format!("{kind:?} for room {} dropped: empty text (session {})", map.id, session.as_deref().unwrap_or("?")));
            return Ok(());
        }
        let ev = Event {
            ts: state::now_ms(),
            role: map.role,
            kind,
            text: text.to_string(),
            background,
            session: session.clone(),
            turn: turn.clone(),
        };
        state::append_event(&map.id, &ev).map_err(|e| {
            state::hook_log(&format!("append to room {} failed: {e}", map.id));
            e
        })
    };
    let push = |kind: EventKind, text: &str| push_with(kind, text, 0);

    match kind {
        "claude-prompt" => {
            push(EventKind::User, v["prompt"].as_str().unwrap_or_default())?;
        }
        "claude-stop" => {
            let text = match v["last_assistant_message"].as_str() {
                Some(t) => t.to_string(),
                None => v["transcript_path"].as_str().map(last_assistant_text).unwrap_or_default(),
            };
            push_with(EventKind::Assistant, &text, waiting_on(&v))?;
        }
        // SessionStart: after a compaction, the room reminds the agent of its role, which
        // the compaction may have taken out of its context. (`/clear` was dealt with above.)
        "claude-session" => {
            if v["source"] == "compact" {
                push(EventKind::Compacted, "compact")?;
            }
        }
        "codex-notify" => {
            // Codex lists every user message of the thread so far. The ones after the count
            // reported last time began this turn: a turn that ended without a notify (an
            // error) left its prompt to the next, and a message sent mid-turn joined the
            // turn. Without a count, with a shorter list (a new thread) or with the counted
            // messages gone (a rewritten one), the last one.
            let thread = v["thread-id"].as_str().unwrap_or_default();
            let new: Vec<&str> = match state::codex_inputs_seen(&map.id, thread) {
                Some((seen, last)) if inputs.len() > seen && (seen == 0 || state::digest(inputs[seen - 1]) == last) => {
                    inputs[seen..].to_vec()
                }
                _ => inputs.last().copied().into_iter().collect(),
            };
            for input in new {
                push(EventKind::User, input)?;
            }
            push(EventKind::Assistant, v["last-assistant-message"].as_str().unwrap_or_default())?;
            if let Some(last) = inputs.last() {
                state::set_codex_inputs_seen(&map.id, thread, inputs.len(), state::digest(last));
            }
        }
        _ => {}
    }
    Ok(())
}

/// The pane a report is for, and its map.
struct Found {
    map: state::PaneMap,
    pane: String,
}

/// A Codex thread's own record says whether it is the user's own conversation: a
/// subagent's, any other the agent spawned, or one a `codex exec` ran, is no conversation
/// of a pane's.
fn thread_kind(thread: &str) -> Option<String> {
    crate::bind::codex_thread_kind(&crate::bind::codex_home(), thread)
}

/// Whether Codex's own record says its TUI started the thread.
fn tui_thread(thread: &str) -> bool {
    crate::bind::codex_tui_thread(&crate::bind::codex_home(), thread)
}

/// Whether the room's agent in `map`'s role is of `kind`: another kind of agent started in
/// the pane later is no conversation of the room's agent, whatever it reports.
fn room_agent_is(map: &state::PaneMap, kind: &str) -> bool {
    state::RoomConfig::load(&map.id).is_ok_and(|room| room.kind(map.role) == kind)
}

/// The agent in `pane` is on another conversation of its own (`how` it got there): the
/// pane is that one's from now on. The hook binds the pane itself, so the conversation's
/// next report is taken however soon it comes, and an event tells the room, which binds
/// its own record of the session as `/rebind` would (a reopened room, `/swap` and Codex's
/// record go by it) and reminds the agent of its role.
fn follow(map: &state::PaneMap, pane: &str, session: &str, how: &str) -> Result<()> {
    if let Err(e) = state::bind_pane(pane, session) {
        state::hook_log(&format!("could not bind pane {pane} to session {session}: {e}"));
    }
    state::hook_log(&format!("pane {pane} of room {} follows its agent to session {session} ({how})", map.id));
    let ev = Event {
        ts: state::now_ms(),
        role: map.role,
        kind: EventKind::Rebound,
        text: how.to_string(),
        background: 0,
        session: Some(session.to_string()),
        turn: None,
    };
    state::append_event(&map.id, &ev).map_err(|e| {
        state::hook_log(&format!("append to room {} failed: {e}", map.id));
        e
    })
}

/// A Codex session on Codex's app-server daemon runs notify in the daemon, without its
/// pane's variables. The room starts every Codex agent of its own in that agent's pane
/// process, so such a session is one a room was opened from: the one Codex agent in the
/// turn's folder that is in a room the room did not start, whatever its role now, and
/// whose thread is the one the room was bound to when it knows it. A thread of the user's
/// own in that folder that is not the bound one (the agent's after `/new`, or another
/// Codex's) is noted for the room to show and `/rebind` to take. The error says why a
/// report was not taken, for `hook.log`.
fn codex_daemon_session(v: &Value, inputs: &[&str]) -> Result<Found, String> {
    if !state::any_pane_mapped() {
        return Err("no pane is in a room".into());
    }
    let thread = v["thread-id"].as_str().unwrap_or("?");
    let cwd = std::path::Path::new(v["cwd"].as_str().ok_or("notify names no cwd")?);
    let codex = crate::herdr::agents_in("codex", cwd);
    let seen = codex.len();
    let mut opened_from = vec![];
    for a in codex {
        let Some(mut map) = state::lookup_pane_on(&a.pane, &a.socket) else { continue };
        let Ok(room) = state::RoomConfig::load(&map.id) else { continue };
        // Maps written before `started` existed deserialize it as false, even for
        // agents the room started. The config identifies the original pane across
        // role switches; never guess from the current role or keyboard focus.
        if map.started || room.host_kind != "codex" || room.host_pane != a.pane {
            continue;
        }
        match room.host_session.as_deref() {
            Some(bound) if bound != thread => {
                if thread_kind(thread).as_deref() == Some("user") {
                    state::note_stray(&map.id, map.role, thread, inputs.last().copied().unwrap_or_default());
                }
                continue;
            }
            bound => {
                if map.session.is_none() {
                    map.session = bound.map(String::from);
                }
            }
        }
        opened_from.push(Found { map, pane: a.pane.clone() });
    }
    match <[_; 1]>::try_from(opened_from) {
        Ok([found]) => {
            state::hook_log(&format!("codex-notify thread {thread} in {} → room {} as {:?}", cwd.display(), found.map.id, found.map.role));
            Ok(found)
        }
        Err(found) if found.is_empty() => {
            Err(format!("thread {thread}: none of {seen} Codex agent(s) in {} opened a room bound to it", cwd.display()))
        }
        Err(found) => {
            let rooms: Vec<&str> = found.iter().map(|f| f.map.id.as_str()).collect();
            Err(format!("thread {thread}: {} Codex sessions in {} opened rooms {}: ambiguous", found.len(), cwd.display(), rooms.join(", ")))
        }
    }
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
