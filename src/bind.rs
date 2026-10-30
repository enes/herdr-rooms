//! Which session a room talks to. `open` prints a token, and the agent that ran it keeps
//! that output in its own record of the conversation (a Codex rollout, a Claude Code
//! transcript). The popup finds the one record that holds the token and binds the room to
//! that session; the hooks then take that session's reports for the host pane and no
//! other's. A thread forked from the session later has another id, and a Codex subagent's
//! rollout says it is one.

use anyhow::{anyhow, Result};
use serde_json::Value;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

/// A token for `open` to print: 16 hex digits from the random keys std seeds its hash
/// maps with.
pub fn new_token() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(crate::state::now_ms());
    h.write_u32(std::process::id());
    format!("{:016x}", h.finish())
}

pub fn codex_home() -> PathBuf {
    std::env::var_os("CODEX_HOME").map(PathBuf::from).unwrap_or_else(|| home().join(".codex"))
}

pub fn claude_home() -> PathBuf {
    std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from).unwrap_or_else(|| home().join(".claude"))
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
}

/// The session of an agent of `kind` that ran `open`, by the token it printed since
/// `since_ms`; `None` for a kind that keeps no record this looks in.
pub fn session_of(kind: &str, token: &str, since_ms: u64) -> Result<Option<String>> {
    match kind {
        "codex" => codex_thread(&codex_home(), token, since_ms).map(Some),
        "claude" => claude_session(&claude_home(), token, since_ms).map(Some),
        _ => Ok(None),
    }
}

/// The Codex thread of the user's own whose rollout records `token` as the output of a
/// command it ran: `!room` as a user shell command, `$room` as a tool call.
pub fn codex_thread(codex_home: &Path, token: &str, since_ms: u64) -> Result<String> {
    let mut found = vec![];
    for path in recent_logs(&codex_home.join("sessions"), since_ms) {
        let Ok(text) = fs::read_to_string(&path) else { continue };
        if !text.contains(token) {
            continue;
        }
        let mut lines = text.lines();
        let Some(meta) = lines.next().and_then(|l| serde_json::from_str::<Value>(l).ok()) else { continue };
        if meta["payload"]["thread_source"].as_str().unwrap_or("user") != "user" {
            continue;
        }
        let Some(id) = meta["payload"]["id"].as_str() else { continue };
        let as_command_output = lines.filter(|l| l.contains(token)).any(|l| {
            let Ok(v) = serde_json::from_str::<Value>(l) else { return false };
            let p = &v["payload"];
            match p["type"].as_str() {
                Some("message") => p["role"] == "user" && text_of(&p["content"]).contains("<user_shell_command>"),
                Some("custom_tool_call_output" | "function_call_output") => true,
                _ => false,
            }
        });
        if as_command_output {
            found.push(id.to_string());
        }
    }
    only(found, "Codex")
}

/// The Claude Code session whose transcript records `token` in a message of the user's:
/// the `/room` command's output goes into the prompt the command expands to.
pub fn claude_session(claude_home: &Path, token: &str, since_ms: u64) -> Result<String> {
    let mut found = vec![];
    for path in recent_logs(&claude_home.join("projects"), since_ms) {
        let Ok(text) = fs::read_to_string(&path) else { continue };
        let hit = text
            .lines()
            .filter(|l| l.contains(token))
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .find(|v| v["type"] == "user" && text_of(&v["message"]["content"]).contains(token));
        if let Some(v) = hit {
            let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned());
            if let Some(id) = v["sessionId"].as_str().map(String::from).or(stem) {
                found.push(id);
            }
        }
    }
    only(found, "Claude Code")
}

/// What a Codex thread's own record says it is. `user` is a conversation of the user's own:
/// the TUI's (`source` `cli`) or the app server's (`vscode`). Anything else is no
/// conversation of a pane's, though the record may call it the user's: a `subagent` thread,
/// or one `codex exec` (`exec`) or an MCP client (`mcp`) ran; a hook binds no pane to those
/// and offers none to `/rebind`. A record with no `source` (an older Codex) counts as the
/// user's.
pub fn codex_thread_kind(codex_home: &Path, thread: &str) -> Option<String> {
    let payload = codex_thread_meta(codex_home, thread)?;
    let thread_source = payload["thread_source"].as_str()?;
    if thread_source != "user" {
        return Some(thread_source.to_string());
    }
    match payload["source"].as_str() {
        None | Some("cli") | Some("vscode") => Some("user".into()),
        Some(other) => Some(other.to_string()),
    }
}

/// Whether Codex's TUI started a thread of the user's own (`source` `cli`). A Codex in a
/// pane's own process reports with that pane's variables, and nothing else there runs the
/// TUI: such a thread is the pane agent's conversation, after `/new`, `/resume` or a
/// restart. A `codex exec` thread says `exec`, one on Codex's daemon `vscode`, and a
/// record with no `source` (an older Codex) cannot tell.
pub fn codex_tui_thread(codex_home: &Path, thread: &str) -> bool {
    codex_thread_meta(codex_home, thread).is_some_and(|p| p["thread_source"] == "user" && p["source"] == "cli")
}

/// The payload of the session meta a Codex thread's record starts with.
fn codex_thread_meta(codex_home: &Path, thread: &str) -> Option<Value> {
    let path = codex_rollout(codex_home, thread)?;
    let file = fs::File::open(&path).ok()?;
    let first = BufReader::new(file).lines().next()?.ok()?;
    let mut meta: Value = serde_json::from_str(&first).ok()?;
    Some(meta["payload"].take())
}

/// The error Codex's record of a thread gives for its last turn, when the turn ended in
/// one (a usage limit, say): such a turn sends no notify, so this is how the room learns
/// what happened.
pub fn codex_last_error(codex_home: &Path, thread: &str) -> Option<String> {
    let path = codex_rollout(codex_home, thread)?;
    let text = fs::read_to_string(path).ok()?;
    // The last turn's end, and only if no turn began after it: a turn interrupted with
    // Esc writes no end, and an older error must not be taken for its own.
    let last_turn_end = text.lines().rev().take_while(|l| !l.contains("\"task_started\"")).find(|l| l.contains("\"task_complete\""))?;
    let v: Value = serde_json::from_str(last_turn_end).ok()?;
    v["payload"]["error"]["message"].as_str().map(|m| m.lines().next().unwrap_or_default().to_string())
}

/// The rollout file of a Codex thread, wherever under `sessions/` it is.
pub fn codex_rollout(codex_home: &Path, thread: &str) -> Option<PathBuf> {
    let suffix = format!("-{thread}.jsonl");
    let mut stack = vec![codex_home.join("sessions")];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.file_name().is_some_and(|n| n.to_string_lossy().ends_with(&suffix)) {
                return Some(path);
            }
        }
    }
    None
}

/// `.jsonl` files under `dir`, at any depth, modified since `since_ms` (with a second's
/// slack): a record that got the token is one written after `open` printed it.
fn recent_logs(dir: &Path, since_ms: u64) -> Vec<PathBuf> {
    let mut found = vec![];
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = fs::read_dir(&d) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|x| x == "jsonl") && modified_ms(&path) + 1_000 >= since_ms {
                found.push(path);
            }
        }
    }
    found
}

fn modified_ms(path: &Path) -> u64 {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A message's text, whether a string or content blocks.
fn text_of(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(blocks) => blocks.iter().filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    }
}

/// The one session found, or what went wrong, for the popup to show.
fn only(found: Vec<String>, agent: &str) -> Result<String> {
    match found.as_slice() {
        [one] => Ok(one.clone()),
        [] => Err(anyhow!(
            "no {agent} session ran this room's open: open the room from inside your agent, with /room or !room"
        )),
        many => Err(anyhow!(
            "{} {agent} sessions hold this room's token ({}): close the copies and open the room again",
            many.len(),
            many.join(", ")
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("herdr-rooms-bind-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn rollout(home: &Path, thread: &str, thread_source: &str, lines: &[String]) {
        rollout_from(home, thread, thread_source, "cli", lines);
    }

    /// A rollout whose record says who started the thread: `cli` (the TUI), `exec`
    /// (`codex exec`), `vscode` (the app server).
    fn rollout_from(home: &Path, thread: &str, thread_source: &str, source: &str, lines: &[String]) {
        let dir = home.join("sessions/2026/10/29");
        fs::create_dir_all(&dir).unwrap();
        let meta = format!(r#"{{"type":"session_meta","payload":{{"id":"{thread}","thread_source":"{thread_source}","source":"{source}"}}}}"#);
        let mut text = vec![meta];
        text.extend(lines.iter().cloned());
        fs::write(dir.join(format!("rollout-2026-10-29T10-00-00-{thread}.jsonl")), text.join("\n") + "\n").unwrap();
    }

    fn shell_command(token: &str) -> String {
        format!(
            r#"{{"type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_text","text":"<user_shell_command> <command> room </command> <result> Output: Room popup opened. (room token {token}) </result> </user_shell_command>"}}]}}}}"#
        )
    }

    #[test]
    fn the_codex_thread_that_ran_open_is_found_by_its_token() {
        let home = scratch("codex");
        let since = crate::state::now_ms() - 60_000;
        rollout(&home, "main", "user", &[shell_command("tok1")]);
        // a subagent spawned later inherits the conversation, token included
        rollout(&home, "sub", "subagent", &[shell_command("tok1")]);
        // another session that merely quotes the token in a message of its own
        let quoted = r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"what is tok1?"}]}}"#;
        rollout(&home, "other", "user", &[quoted.to_string()]);
        assert_eq!(codex_thread(&home, "tok1", since).unwrap(), "main");
        assert_eq!(codex_thread_kind(&home, "sub").as_deref(), Some("subagent"));
        assert_eq!(codex_thread_kind(&home, "main").as_deref(), Some("user"));
        assert_eq!(codex_thread_kind(&home, "none"), None);
        // a `codex exec` thread's record calls it the user's too; its source says what ran it
        rollout_from(&home, "probe", "user", "exec", &[]);
        assert_eq!(codex_thread_kind(&home, "probe").as_deref(), Some("exec"));
        rollout_from(&home, "app", "user", "vscode", &[]);
        assert_eq!(codex_thread_kind(&home, "app").as_deref(), Some("user"));
        // only a thread of the user's the TUI started is a pane agent's own for certain
        assert!(codex_tui_thread(&home, "main"));
        assert!(!codex_tui_thread(&home, "sub") && !codex_tui_thread(&home, "probe") && !codex_tui_thread(&home, "app"));
        assert!(!codex_tui_thread(&home, "none"));
        // an older Codex's record has no source: the user's, but not whose
        let old = r#"{"type":"session_meta","payload":{"id":"old","thread_source":"user"}}"#;
        fs::write(home.join("sessions/2026/10/29/rollout-2026-10-29T10-00-00-old.jsonl"), format!("{old}\n")).unwrap();
        assert_eq!(codex_thread_kind(&home, "old").as_deref(), Some("user"));
        assert!(!codex_tui_thread(&home, "old"));
        // the token nowhere, or in two sessions of the user's own
        assert!(codex_thread(&home, "tok2", since).unwrap_err().to_string().contains("no Codex session"));
        rollout(&home, "fork", "user", &[shell_command("tok1")]);
        assert!(codex_thread(&home, "tok1", since).unwrap_err().to_string().contains("2 Codex sessions"));
        // a record older than the token cannot hold it
        assert!(codex_thread(&home, "tok1", crate::state::now_ms() + 120_000).is_err());
    }

    #[test]
    fn the_error_that_ended_a_codex_turn_is_read_from_its_record() {
        let home = scratch("error");
        let ok = r#"{"type":"event_msg","payload":{"type":"task_complete","last_agent_message":"done","error":null}}"#;
        let failed = r#"{"type":"event_msg","payload":{"type":"task_complete","last_agent_message":null,"error":{"message":"You’ve hit your usage limit. Upgrade to Pro\nmore"}}}"#;
        let started = r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"t"}}"#;
        rollout(&home, "t1", "user", &[started.to_string(), ok.to_string(), started.to_string(), failed.to_string()]);
        rollout(&home, "t2", "user", &[started.to_string(), failed.to_string(), started.to_string(), ok.to_string()]);
        // a turn that began after the error and never ended (Esc) is not the error's
        rollout(&home, "t3", "user", &[started.to_string(), failed.to_string(), started.to_string()]);
        assert_eq!(codex_last_error(&home, "t1").as_deref(), Some("You’ve hit your usage limit. Upgrade to Pro"));
        assert_eq!(codex_last_error(&home, "t2"), None);
        assert_eq!(codex_last_error(&home, "t3"), None);
        assert_eq!(codex_last_error(&home, "none"), None);
    }

    #[test]
    fn the_claude_session_that_ran_room_is_found_by_its_token() {
        let home = scratch("claude");
        let since = crate::state::now_ms() - 60_000;
        let dir = home.join("projects/-Users-me-proj");
        fs::create_dir_all(&dir).unwrap();
        let user = |text: &str, session: &str| {
            format!(r#"{{"type":"user","sessionId":"{session}","message":{{"role":"user","content":"{text}"}}}}"#)
        };
        fs::write(dir.join("aaa.jsonl"), user("<command-name>/room</command-name> Room popup opened. (room token tok1)", "aaa") + "\n").unwrap();
        let answer = r#"{"type":"assistant","sessionId":"bbb","message":{"role":"assistant","content":[{"type":"text","text":"the token was tok1"}]}}"#;
        fs::write(dir.join("bbb.jsonl"), answer.to_string() + "\n").unwrap();
        assert_eq!(claude_session(&home, "tok1", since).unwrap(), "aaa");
        assert!(claude_session(&home, "tok9", since).is_err());
    }
}
