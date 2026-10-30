//! Exercise the actual notify entrypoint with isolated state and a fake herdr server.
use serde_json::{json, Value};
use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Fixture {
    root: PathBuf,
    agents: Vec<Value>,
}

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "herdr-rooms-hook-test-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(root.join("panes")).unwrap();
        let herdr = root.join("herdr");
        fs::write(&herdr, r#"#!/bin/sh
case "$1 $2" in
  "session list") printf '%s\n' '{"sessions":[{"running":true,"socket_path":"/test/herdr.sock"}]}' ;;
  "agent list") cat "$HOOK_TEST_AGENTS" ;;
  *) exit 1 ;;
esac
"#).unwrap();
        fs::set_permissions(herdr, fs::Permissions::from_mode(0o755)).unwrap();
        Self { root, agents: vec![] }
    }

    // Legacy maps omit `started`; `original` says whether Codex opened this room.
    fn room(&mut self, id: &str, original: bool, swapped: bool) {
        let pane = format!("{id}:codex");
        let other = format!("{id}:claude");
        let role = if original != swapped { "host" } else { "reviewer" };
        let config = json!({
            "id": id, "cwd": self.root, "host_pane": if original { &pane } else { &other },
            "host_kind": if original { "codex" } else { "claude" },
            "reviewer_pane": if original { &other } else { &pane },
            "reviewer_kind": if original { "claude" } else { "codex" },
            "reviewer_read_only": true, "max_rounds": 3, "swapped": swapped
        });
        fs::create_dir_all(self.root.join("rooms").join(id)).unwrap();
        fs::write(self.root.join("rooms").join(id).join("config.json"), config.to_string()).unwrap();
        let map = json!({"id": id, "role": role, "socket": "/test/herdr.sock"});
        fs::create_dir_all(self.root.join("panes/_test_herdr.sock")).unwrap();
        fs::write(self.map_path(&pane), map.to_string()).unwrap();
        self.agents.push(json!({"agent": "codex", "cwd": self.root, "pane_id": pane}));
    }

    /// Where the map of `pane` lives: in the folder of the test's herdr server.
    fn map_path(&self, pane: &str) -> PathBuf {
        self.root.join("panes/_test_herdr.sock").join(format!("{}.json", pane.replace(':', "_")))
    }

    fn notify(&self, pane: Option<&str>) {
        self.notify_inputs(pane, "test-thread", &["test request"]);
    }

    /// A notify of `thread`, listing `inputs` as the thread's user messages so far.
    fn notify_inputs(&self, pane: Option<&str>, thread: &str, inputs: &[&str]) {
        let agents = self.root.join("agents.json");
        fs::write(&agents, json!({"result": {"agents": self.agents}}).to_string()).unwrap();
        let payload = json!({
            "type": "agent-turn-complete", "cwd": self.root, "thread-id": thread, "turn-id": "test-turn",
            "input-messages": inputs, "last-assistant-message": "test answer"
        });
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_herdr-rooms"));
        cmd.args(["hook", "codex-notify", &payload.to_string()])
            .env("HERDR_ROOMS_HOME", &self.root)
            .env("HERDR_BIN_PATH", self.root.join("herdr"))
            .env("HERDR_SOCKET_PATH", "/test/herdr.sock")
            .env("HOOK_TEST_AGENTS", agents)
            .env("CODEX_HOME", self.root.join("codex"))
            .env_remove("HERDR_PANE_ID");
        if let Some(pane) = pane {
            cmd.env("HERDR_PANE_ID", pane);
        }
        let out = cmd.output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    }

    /// A Claude Code hook of `kind` (`claude-prompt`, `claude-session`, …) that a session in
    /// `pane` runs, with `input` on its stdin.
    fn claude(&self, pane: &str, kind: &str, input: Value) {
        let mut child = Command::new(env!("CARGO_BIN_EXE_herdr-rooms"))
            .args(["hook", kind])
            .env("HERDR_ROOMS_HOME", &self.root)
            .env("HERDR_BIN_PATH", self.root.join("herdr"))
            .env("HERDR_SOCKET_PATH", "/test/herdr.sock")
            .env("HERDR_PANE_ID", pane)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(input.to_string().as_bytes()).unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    }

    /// Maps the pane of room `id`'s Claude agent, which has the role Codex's has not, and
    /// binds it to `session`.
    fn claude_pane(&self, id: &str, session: &str) {
        let codex = self.pane_map(&format!("{id}:codex"));
        let role = if codex["role"] == "host" { "reviewer" } else { "host" };
        let map = json!({"id": id, "role": role, "socket": "/test/herdr.sock", "session": session});
        fs::write(self.map_path(&format!("{id}:claude")), map.to_string()).unwrap();
    }

    fn events(&self, id: &str) -> Vec<Value> {
        let path = self.root.join("rooms").join(id).join("events.jsonl");
        fs::read_to_string(path).unwrap_or_default().lines().map(|l| serde_json::from_str(l).unwrap()).collect()
    }

    fn pane_map(&self, pane: &str) -> Value {
        serde_json::from_str(&fs::read_to_string(self.map_path(pane)).unwrap()).unwrap()
    }

    /// The room bound `pane` to `session` (the popup found it, or the room started it).
    fn bind(&self, pane: &str, session: &str) {
        let path = self.map_path(pane);
        let mut map: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        map["session"] = json!(session);
        fs::write(path, map.to_string()).unwrap();
    }

    fn set_host_session(&self, id: &str, session: &str) {
        let path = self.root.join("rooms").join(id).join("config.json");
        let mut config: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        config["host_session"] = json!(session);
        fs::write(path, config.to_string()).unwrap();
    }

    /// Codex's own record of a thread, as the hook reads it to tell a subagent's.
    fn rollout(&self, thread: &str, thread_source: &str) {
        self.rollout_from(thread, thread_source, "cli");
    }

    /// A rollout whose record says who started the thread: `cli` (the TUI), `exec`
    /// (`codex exec`), `vscode` (the app server).
    fn rollout_from(&self, thread: &str, thread_source: &str, source: &str) {
        self.record(json!({"id": thread, "thread_source": thread_source, "source": source}));
    }

    /// An older Codex's record, which says whose thread it is but not what started it.
    fn old_rollout(&self, thread: &str) {
        self.record(json!({"id": thread, "thread_source": "user"}));
    }

    fn record(&self, payload: Value) {
        let dir = self.root.join("codex/sessions/2026/10/29");
        fs::create_dir_all(&dir).unwrap();
        let thread = payload["id"].as_str().unwrap().to_string();
        let meta = json!({"type": "session_meta", "payload": payload});
        fs::write(dir.join(format!("rollout-2026-10-29T10-00-00-{thread}.jsonl")), meta.to_string() + "\n").unwrap();
    }
}

#[test]
fn a_report_from_another_session_in_the_pane_is_dropped_and_noted() {
    let mut f = Fixture::new();
    f.room("one", false, false);
    f.bind("one:codex", "codex-main");
    // a thread of the user's own whose record does not say what started it (an older
    // Codex): the agent's after /new, or a `codex exec` run from the pane; the user tells
    f.old_rollout("stray");
    f.notify_inputs(Some("one:codex"), "stray", &["say hi"]);
    assert!(f.events("one").is_empty());
    let strays: Value = serde_json::from_str(&fs::read_to_string(f.root.join("rooms/one/stray-reviewer.json")).unwrap()).unwrap();
    assert_eq!((strays[0]["session"].as_str(), strays[0]["said"].as_str()), (Some("stray"), Some("say hi")));
    f.notify_inputs(Some("one:codex"), "codex-main", &["review this"]);
    assert_eq!(f.events("one").len(), 2);
}

#[test]
fn an_unbound_codex_pane_takes_its_own_threads_and_binds_the_first() {
    let mut f = Fixture::new();
    f.room("one", false, false);
    f.rollout("sub", "subagent");
    f.rollout("main", "user");
    // Codex titles a new thread in a thread of its own, which finishes before the first real turn
    f.notify_inputs(Some("one:codex"), "title", &["Generate a concise, single-line task title for this conversation"]);
    assert!(f.pane_map("one:codex")["session"].is_null());
    // the agent's subagent finishes first: not the agent's own thread
    f.notify_inputs(Some("one:codex"), "sub", &["task"]);
    assert!(f.events("one").is_empty());
    assert!(f.pane_map("one:codex")["session"].is_null());
    // a `codex exec` run from the pane before the agent's first turn: its record calls the
    // thread the user's, but its source says what ran it; it binds nothing
    f.rollout_from("probe", "user", "exec");
    f.notify_inputs(Some("one:codex"), "probe", &["probe"]);
    assert!(f.events("one").is_empty());
    assert!(f.pane_map("one:codex")["session"].is_null(), "the pane took a codex exec thread");
    f.notify_inputs(Some("one:codex"), "main", &["review this"]);
    assert_eq!(f.events("one").len(), 2, "the agent's first real turn was dropped");
    assert_eq!(f.pane_map("one:codex")["session"], "main");
    // bound now: another thread of the user's own whose record cannot place it is a stray; a
    // subagent's, a `codex exec`'s or a catch-up is not
    f.old_rollout("other");
    f.notify_inputs(Some("one:codex"), "other", &["x"]);
    f.notify_inputs(Some("one:codex"), "sub", &["task"]);
    f.notify_inputs(Some("one:codex"), "probe", &["probe"]);
    f.notify_inputs(Some("one:codex"), "catch", &["Write a brief catch-up for a user returning to this task. Return JSON"]);
    assert_eq!(f.events("one").len(), 2);
    let strays: Value = serde_json::from_str(&fs::read_to_string(f.root.join("rooms/one/stray-reviewer.json")).unwrap()).unwrap();
    let ids: Vec<&str> = strays.as_array().unwrap().iter().map(|s| s["session"].as_str().unwrap()).collect();
    assert_eq!(ids, ["other"]);
}

#[test]
fn the_agents_own_new_conversation_takes_the_pane() {
    let mut f = Fixture::new();
    f.room("one", false, false);
    // Claude Code's /clear names the conversation it starts: the pane takes it, the room is told
    f.claude_pane("one", "old");
    f.claude("one:claude", "claude-session", json!({"session_id": "new", "source": "clear"}));
    assert_eq!(f.pane_map("one:claude")["session"], "new");
    let events = f.events("one");
    assert_eq!(events.len(), 1);
    assert_eq!((events[0]["kind"].as_str(), events[0]["role"].as_str(), events[0]["session"].as_str()), (Some("rebound"), Some("host"), Some("new")));
    // the other copy of the hook (the plugin's, the room's) finds it done
    f.claude("one:claude", "claude-session", json!({"session_id": "new", "source": "clear"}));
    assert_eq!(f.events("one").len(), 1);
    // the new conversation's first prompt reaches the room; the old conversation is another's now
    f.claude("one:claude", "claude-prompt", json!({"session_id": "new", "prompt_id": "p1", "prompt": "now the median"}));
    f.claude("one:claude", "claude-prompt", json!({"session_id": "old", "prompt_id": "p0", "prompt": "late"}));
    assert_eq!(f.events("one").len(), 2);
    // a `claude -p` run from the pane starts as `startup`, and takes nothing
    f.claude("one:claude", "claude-session", json!({"session_id": "probe", "source": "startup"}));
    assert_eq!(f.pane_map("one:claude")["session"], "new");

    // a thread Codex's TUI started, reported from the pane's own process: the agent's after /new
    f.bind("one:codex", "main");
    f.rollout("main", "user");
    f.rollout("next", "user");
    f.rollout_from("probe", "user", "exec");
    f.notify_inputs(Some("one:codex"), "probe", &["probe"]);
    assert_eq!(f.pane_map("one:codex")["session"], "main", "a `codex exec` run from the pane took it");
    f.notify_inputs(Some("one:codex"), "next", &["first message"]);
    assert_eq!(f.pane_map("one:codex")["session"], "next");
    let events = f.events("one");
    let kinds: Vec<&str> = events[2..].iter().map(|e| e["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["rebound", "user", "assistant"], "the new thread's first turn reaches the room too");
    assert_eq!((events[2]["session"].as_str(), events[3]["text"].as_str()), (Some("next"), Some("first message")));
    // back to the first thread (/resume): the pane follows again, and nothing waits for /rebind
    f.notify_inputs(Some("one:codex"), "main", &["review this"]);
    assert_eq!(f.pane_map("one:codex")["session"], "main");
    assert!(!f.root.join("rooms/one/stray-reviewer.json").exists());
    // a Claude started in Codex's pane later is not the room's Codex, /clear or not
    f.claude("one:codex", "claude-session", json!({"session_id": "other-agent", "source": "clear"}));
    assert_eq!(f.pane_map("one:codex")["session"], "main");
    let log = fs::read_to_string(f.root.join("hook.log")).unwrap_or_default();
    assert!(log.contains("session other-agent is not the room's agent"), "{log}");
}

#[test]
fn a_daemon_codex_host_on_a_new_thread_is_noted_for_the_room() {
    let mut f = Fixture::new();
    f.room("one", true, false);
    f.set_host_session("one", "main");
    f.rollout("main", "user");
    f.rollout("after-new", "user");
    f.rollout("sub", "subagent");
    // the user types /new in the Codex host, which runs on the app-server daemon
    f.notify_inputs(None, "after-new", &["next request"]);
    assert!(f.events("one").is_empty());
    let strays: Value = serde_json::from_str(&fs::read_to_string(f.root.join("rooms/one/stray-host.json")).unwrap()).unwrap();
    assert_eq!((strays[0]["session"].as_str(), strays[0]["said"].as_str()), (Some("after-new"), Some("next request")));
    // the host's subagents report from the same folder, and are no candidates
    f.notify_inputs(None, "sub", &["task"]);
    assert_eq!(strays.as_array().unwrap().len(), 1);
    let again: Value = serde_json::from_str(&fs::read_to_string(f.root.join("rooms/one/stray-host.json")).unwrap()).unwrap();
    assert_eq!(again.as_array().unwrap().len(), 1);
}

#[test]
fn a_daemon_notify_must_come_from_the_thread_the_room_was_bound_to() {
    let mut f = Fixture::new();
    f.room("one", true, false);
    f.set_host_session("one", "codex-main");
    f.notify_inputs(None, "other", &["x"]);
    assert!(f.events("one").is_empty());
    f.notify_inputs(None, "codex-main", &["x"]);
    assert_eq!(f.events("one").len(), 2);
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn legacy_reviewers_do_not_hide_the_original_codex_session() {
    for swapped in [false, true] {
        let mut f = Fixture::new();
        f.room("old1", false, false);
        f.room("old2", false, true);
        f.room("current", true, swapped);
        f.notify(None);
        assert!(f.events("old1").is_empty() && f.events("old2").is_empty());
        let events = f.events("current");
        assert_eq!(events.len(), 2, "both the request and answer must reach the current room");
        assert_eq!(events[0]["kind"], "user");
        assert_eq!(events[1]["kind"], "assistant");
        for ev in events {
            assert_eq!(ev["role"], if swapped { "reviewer" } else { "host" });
            assert_eq!(ev["session"], "test-thread");
            // both halves of the notify name the turn, so the room can tie them together
            assert_eq!(ev["turn"], "test-turn");
        }
    }
}

#[test]
fn codex_messages_after_the_last_report_belong_to_this_turn() {
    let mut f = Fixture::new();
    f.room("one", true, false);
    f.notify_inputs(None, "t", &["a"]);
    // a turn that ended without a notify left "b"; "c" was sent while this turn ran
    f.notify_inputs(None, "t", &["a", "b", "c"]);
    // a shorter list is another thread's: only its last message is this turn's
    f.notify_inputs(None, "t", &["z"]);
    let texts: Vec<String> =
        f.events("one").iter().map(|e| format!("{}:{}", e["kind"].as_str().unwrap(), e["text"].as_str().unwrap())).collect();
    assert_eq!(
        texts,
        ["user:a", "assistant:test answer", "user:b", "user:c", "assistant:test answer", "user:z", "assistant:test answer"]
    );
}

#[test]
fn messages_answered_while_the_room_was_closed_are_not_this_turns() {
    let mut f = Fixture::new();
    f.room("one", true, false);
    f.notify_inputs(None, "t", &["a"]);
    // Ctrl+Q closes the room view and unmaps its panes; the user goes on with Codex
    let map = f.map_path("one:codex");
    let saved = fs::read_to_string(&map).unwrap();
    fs::remove_file(&map).unwrap();
    f.notify_inputs(None, "t", &["a", "b"]);
    // the room opens again on the same Codex: it maps the pane and starts the counts over
    fs::write(&map, saved).unwrap();
    fs::remove_dir_all(f.root.join("rooms").join("one").join("inputs")).unwrap();
    f.notify_inputs(None, "t", &["a", "b", "c"]);
    let users: Vec<String> =
        f.events("one").iter().filter(|e| e["kind"] == "user").map(|e| e["text"].as_str().unwrap().to_string()).collect();
    assert_eq!(users, ["a", "c"], "b was asked and answered while no room listened");
}

#[test]
fn a_thread_rewritten_since_the_last_report_counts_from_its_last_message() {
    let mut f = Fixture::new();
    f.room("one", true, false);
    f.notify_inputs(None, "t", &["a", "b"]);
    // the list grew, but the messages counted last time are not the ones there now
    f.notify_inputs(None, "t", &["x", "y", "z"]);
    let users: Vec<String> =
        f.events("one").iter().filter(|e| e["kind"] == "user").map(|e| e["text"].as_str().unwrap().to_string()).collect();
    assert_eq!(users, ["b", "z"]);
}

#[test]
fn a_room_mapped_before_the_per_server_folders_still_hears_its_agents() {
    let mut f = Fixture::new();
    f.room("one", false, false);
    // the room was opened by the previous build, which wrote flat maps: panes/<pane>.json
    let new = f.map_path("one:codex");
    let flat = f.root.join("panes").join("one_codex.json");
    fs::rename(&new, &flat).unwrap();
    f.rollout("main", "user");
    f.notify_inputs(Some("one:codex"), "main", &["review this"]);
    assert_eq!(f.events("one").len(), 2, "every turn of a room opened before the upgrade is lost, silently");
    // binding the pane moved its map into the folder
    assert!(new.is_file() && !flat.exists());
    assert_eq!(f.pane_map("one:codex")["session"], "main");
}

#[test]
fn codex_internal_turns_are_not_reported() {
    let mut f = Fixture::new();
    f.room("one", true, false);
    f.notify_inputs(None, "t-title", &["Generate a concise, single-line task title for this thread"]);
    f.notify_inputs(None, "t-catch-up", &["Write a brief catch-up for a user returning to this task. Return JSON"]);
    assert!(f.events("one").is_empty());
}

#[test]
fn a_dropped_notify_is_explained_in_the_hook_log() {
    let mut f = Fixture::new();
    f.room("one", true, false);
    f.room("two", true, false);
    f.notify(None);
    let log = fs::read_to_string(f.root.join("hook.log")).unwrap_or_default();
    assert!(log.contains("codex-notify dropped") && log.contains("ambiguous"), "{log}");
}

#[test]
fn ambiguous_original_sessions_do_not_leak_into_another_room() {
    let mut f = Fixture::new();
    f.room("one", true, false);
    f.room("two", true, false);
    f.notify(None);
    assert!(f.events("one").is_empty() && f.events("two").is_empty());
}

#[test]
fn a_lone_legacy_reviewer_is_not_mistaken_for_the_daemon_session() {
    let mut f = Fixture::new();
    f.room("old", false, false);
    f.notify(None);
    assert!(f.events("old").is_empty());
}

#[test]
fn a_reviewer_with_its_own_pane_still_reports_to_its_room() {
    let mut f = Fixture::new();
    f.room("old", false, false);
    f.notify(Some("old:codex"));
    let events = f.events("old");
    assert_eq!(events.len(), 2);
    assert_eq!(events[1]["role"], "reviewer");
}
