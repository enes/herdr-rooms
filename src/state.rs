//! On-disk state shared by the setup popup, the room and the agent hooks.
//!
//! Layout under `$HERDR_ROOMS_HOME`, by default the state folder herdr makes for the
//! plugin (`~/.local/state/herdr/plugins/herdr-rooms`):
//!   rooms/<id>/config.json     one room's settings
//!   rooms/<id>/events.jsonl    messages the hooks captured, one JSON per line
//!   panes/<pane key>.json      which room (and role) a pane belongs to

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// The plugin's state folder. herdr hands its plugin processes this folder as
/// `HERDR_PLUGIN_STATE_DIR`, but the hooks run inside the agents, which do not get it, so
/// every process works the path out the same way instead.
pub fn home() -> PathBuf {
    let set = |name| std::env::var_os(name).filter(|p| !p.is_empty()).map(PathBuf::from);
    if let Some(p) = set("HERDR_ROOMS_HOME") {
        return p;
    }
    let state = set("XDG_STATE_HOME")
        .unwrap_or_else(|| set("HOME").unwrap_or_else(|| PathBuf::from("/tmp")).join(".local/state"));
    state.join("herdr/plugins/herdr-rooms")
}

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Host,
    Reviewer,
}

impl Role {
    pub fn other(self) -> Role {
        match self {
            Role::Host => Role::Reviewer,
            Role::Reviewer => Role::Host,
        }
    }
}

/// `host_*` and `reviewer_*` describe the two agents as the room opened: the session it
/// was opened from, and the agent it started. `swapped` says their roles are switched.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RoomConfig {
    pub id: String,
    pub cwd: String,
    #[serde(default)]
    pub topic: String,
    pub host_pane: String,
    pub host_kind: String,
    pub reviewer_kind: String,
    #[serde(default)]
    pub reviewer_model: Option<String>,
    #[serde(default)]
    pub reviewer_effort: Option<String>,
    pub reviewer_read_only: bool,
    pub max_rounds: u32,
    #[serde(default)]
    pub reviewer_pane: Option<String>,
    #[serde(default)]
    pub reviewer_name: Option<String>,
    /// `@` names the user picked for the agents in the room (beside `@host`, `@reviewer`).
    #[serde(default)]
    pub host_nick: Option<String>,
    #[serde(default)]
    pub reviewer_nick: Option<String>,
    /// `/swap` switched the roles: the agent the room started hosts, and the session it
    /// was opened from reviews.
    #[serde(default)]
    pub swapped: bool,
    /// The started agent's session, to resume it when it restarts for a new role. The
    /// room gives Claude its id at start; a Codex's thread is learned from the echo of the
    /// room's first prompt.
    #[serde(default)]
    pub reviewer_session: Option<String>,
    /// The session the room was opened from: the one that ran `open`, found by the token
    /// `open` printed. Its hooks report for the host pane; no other session's do.
    #[serde(default)]
    pub host_session: Option<String>,
    /// The process showing the room, while one does: its pane maps are stale once it is gone.
    #[serde(default)]
    pub room_pid: Option<u32>,
}

pub fn room_dir(id: &str) -> PathBuf {
    home().join("rooms").join(id)
}

pub fn events_path(id: &str) -> PathBuf {
    room_dir(id).join("events.jsonl")
}

impl RoomConfig {
    pub fn load(id: &str) -> Result<RoomConfig> {
        let path = room_dir(id).join("config.json");
        let text = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        Ok(serde_json::from_str(&text)?)
    }

    pub fn save(&self) -> Result<()> {
        let dir = room_dir(&self.id);
        fs::create_dir_all(&dir)?;
        fs::write(dir.join("config.json"), serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    /// The role of the agent the room started: the reviewer until the roles are switched.
    pub fn started_role(&self) -> Role {
        if self.swapped {
            Role::Host
        } else {
            Role::Reviewer
        }
    }

    pub fn kind(&self, role: Role) -> &str {
        if role == self.started_role() {
            &self.reviewer_kind
        } else {
            &self.host_kind
        }
    }

    /// The `@` name the user picked for the agent in `role`; a name stays with its agent.
    pub fn nick(&self, role: Role) -> Option<&String> {
        if role == self.started_role() {
            self.reviewer_nick.as_ref()
        } else {
            self.host_nick.as_ref()
        }
    }

    /// The session bound to the agent in `role`, once the room knows it.
    pub fn session(&self, role: Role) -> Option<&str> {
        if role == self.started_role() {
            self.reviewer_session.as_deref()
        } else {
            self.host_session.as_deref()
        }
    }

    /// The pane of the agent in `role`; the started agent has none until the room splits one.
    pub fn pane(&self, role: Role) -> Option<String> {
        if role == self.started_role() {
            self.reviewer_pane.clone()
        } else {
            Some(self.host_pane.clone())
        }
    }

    /// Reopening just the room view reattaches the existing agent sessions, including
    /// the maps removed when the previous view closed.
    pub fn map_panes(&self) -> Result<()> {
        map_pane(&self.host_pane, &self.id, self.started_role().other(), false, self.host_session.as_deref())?;
        if let Some(pane) = &self.reviewer_pane {
            map_pane(pane, &self.id, self.started_role(), true, self.reviewer_session.as_deref())?;
        }
        Ok(())
    }

    /// Binds the agent in `role` to `session`: the started agent's, or the one the room
    /// was opened from.
    pub fn set_session(&mut self, role: Role, session: &str) {
        if role == self.started_role() {
            self.reviewer_session = Some(session.to_string());
        } else {
            self.host_session = Some(session.to_string());
        }
    }
}

// ------------------------------------------------------------ pane map

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PaneMap {
    pub id: String,
    pub role: Role,
    /// Socket of the herdr server the pane lives on; pane ids repeat across servers.
    #[serde(default)]
    pub socket: String,
    /// The room started this agent; false for the session the room was opened from.
    #[serde(default)]
    pub started: bool,
    /// The session that reports for this pane, once known: Claude Code's session id or
    /// Codex's thread id. A hook takes no other session's report for the pane, as a process
    /// started from the pane (a `claude -p`) and an agent's subagents inherit its variables.
    #[serde(default)]
    pub session: Option<String>,
}

/// Pane maps live in a folder per herdr server: pane ids repeat across servers.
fn pane_file(pane: &str, socket: &str) -> PathBuf {
    home().join("panes").join(socket_key(socket)).join(format!("{}.json", pane.replace([':', '/'], "_")))
}

/// Where the builds before the per-server folders kept a pane's map. A room such a build
/// opened keeps hearing its agents through these until a room start moves them into the
/// folders; an upgrade must not cut an open room off.
fn legacy_pane_file(pane: &str) -> PathBuf {
    home().join("panes").join(format!("{}.json", pane.replace([':', '/'], "_")))
}

fn socket_key(socket: &str) -> String {
    if socket.is_empty() {
        "local".into()
    } else {
        socket.replace('/', "_")
    }
}

/// The pane id a map file is for: `w1C_p1.json` was `w1C:p1`.
fn pane_of_file(path: &std::path::Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    Some(stem.replacen('_', ":", 1))
}

/// The herdr server this process runs in.
pub fn socket_path() -> String {
    std::env::var("HERDR_SOCKET_PATH").unwrap_or_default()
}

pub fn map_pane(pane: &str, id: &str, role: Role, started: bool, session: Option<&str>) -> Result<()> {
    let path = pane_file(pane, &socket_path());
    fs::create_dir_all(path.parent().unwrap())?;
    let map = PaneMap { id: id.to_string(), role, socket: socket_path(), started, session: session.map(String::from) };
    fs::write(path, serde_json::to_string(&map)?)?;
    Ok(())
}

/// Binds a mapped pane to `session`: the first report of an agent's own the hook took. A
/// map still in the old flat layout moves into its folder on the way.
pub fn bind_pane(pane: &str, session: &str) -> Result<()> {
    let path = pane_file(pane, &socket_path());
    let legacy = legacy_pane_file(pane);
    let text = fs::read_to_string(&path).or_else(|_| fs::read_to_string(&legacy))?;
    let mut map: PaneMap = serde_json::from_str(&text)?;
    map.session = Some(session.to_string());
    fs::create_dir_all(path.parent().unwrap())?;
    fs::write(path, serde_json::to_string(&map)?)?;
    let _ = fs::remove_file(legacy);
    Ok(())
}

/// Removes the pane maps `alive` does not vouch for (its arguments: the pane id and its
/// map), and maps written in the old flat layout, which nothing reads any more. A room
/// that crashed, or a herdr that quit, leaves maps behind, and herdr gives new panes the
/// old ids. Returns the panes whose maps went.
pub fn sweep_panes(alive: impl Fn(&str, &PaneMap) -> bool) -> Vec<String> {
    sweep_panes_in(&home().join("panes"), alive)
}

fn sweep_panes_in(panes: &std::path::Path, alive: impl Fn(&str, &PaneMap) -> bool) -> Vec<String> {
    let mut gone = vec![];
    // Maps in the old flat layout move into their server's folder first, and then go or
    // stay by the same rules as the rest; one its folder already has a newer map for goes.
    for flat in fs::read_dir(panes).into_iter().flatten().flatten().map(|e| e.path()).filter(|p| p.is_file()) {
        let socket = fs::read_to_string(&flat).ok().and_then(|t| serde_json::from_str::<PaneMap>(&t).ok()).map(|m| m.socket);
        match (socket, flat.file_name()) {
            (Some(socket), Some(name)) if !panes.join(socket_key(&socket)).join(name).exists() => {
                let dir = panes.join(socket_key(&socket));
                let _ = fs::create_dir_all(&dir);
                let _ = fs::rename(&flat, dir.join(name));
            }
            _ => {
                let _ = fs::remove_file(&flat);
                gone.extend(pane_of_file(&flat));
            }
        }
    }
    let Ok(servers) = fs::read_dir(panes) else { return gone };
    for server in servers.flatten() {
        let path = server.path();
        if path.is_file() {
            continue;
        }
        let Ok(files) = fs::read_dir(&path) else { continue };
        for file in files.flatten() {
            let path = file.path();
            let Some(pane) = pane_of_file(&path) else { continue };
            let map = fs::read_to_string(&path).ok().and_then(|t| serde_json::from_str::<PaneMap>(&t).ok());
            if map.is_none_or(|m| !alive(&pane, &m)) {
                let _ = fs::remove_file(&path);
                gone.push(pane);
            }
        }
    }
    gone
}

// ------------------------------------------------------------ stray sessions

/// A session that reported for a pane without being the one bound to it, with what it
/// said first: the room shows it beside the id, so the user can tell the agent's new
/// conversation (after `/new` or `/clear`) from a `claude -p` or `codex exec` run there.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Stray {
    pub session: String,
    pub said: String,
    pub ts: u64,
}

fn stray_file(id: &str, role: Role) -> PathBuf {
    room_dir(id).join(format!("stray-{}.json", match role {
        Role::Host => "host",
        Role::Reviewer => "reviewer",
    }))
}

/// Notes a stray session of `role`'s pane, once, with the first thing it said; the eight
/// latest are kept.
pub fn note_stray(id: &str, role: Role, session: &str, said: &str) {
    let mut list = strays(id, role);
    if list.iter().any(|s| s.session == session) {
        return;
    }
    list.push(Stray { session: session.to_string(), said: said.chars().take(160).collect(), ts: now_ms() });
    if list.len() > 8 {
        list.remove(0);
    }
    let _ = fs::create_dir_all(room_dir(id));
    if let Ok(json) = serde_json::to_string(&list) {
        let _ = fs::write(stray_file(id, role), json);
    }
}

pub fn strays(id: &str, role: Role) -> Vec<Stray> {
    fs::read_to_string(stray_file(id, role)).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

pub fn clear_strays(id: &str, role: Role) {
    let _ = fs::remove_file(stray_file(id, role));
}

pub fn unmap_pane(pane: &str, id: &str) {
    if lookup_pane(pane).is_some_and(|m| m.id == id) {
        let _ = fs::remove_file(pane_file(pane, &socket_path()));
    }
}

pub fn lookup_pane(pane: &str) -> Option<PaneMap> {
    lookup_pane_on(pane, &socket_path())
}

/// `lookup_pane` for a pane of the herdr server at `socket`.
pub fn lookup_pane_on(pane: &str, socket: &str) -> Option<PaneMap> {
    let text = fs::read_to_string(pane_file(pane, socket)).or_else(|_| fs::read_to_string(legacy_pane_file(pane))).ok()?;
    let map: PaneMap = serde_json::from_str(&text).ok()?;
    if !map.socket.is_empty() && !socket.is_empty() && map.socket != socket {
        return None;
    }
    Some(map)
}

/// Whether any pane is in a room: lets a hook that must search for its pane skip the search.
pub fn any_pane_mapped() -> bool {
    let Ok(servers) = fs::read_dir(home().join("panes")) else { return false };
    servers.flatten().any(|s| s.path().is_file() || fs::read_dir(s.path()).is_ok_and(|mut files| files.next().is_some()))
}

// ------------------------------------------------------------ events

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EventKind {
    User,
    Assistant,
    /// The agent's context was compacted: what the room told it may be gone from it.
    Compacted,
    /// The agent started a new conversation in its pane (`/clear`, `/new`), and its hook
    /// bound the pane to it (`session`); the room follows it, as `/rebind` would.
    Rebound,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Event {
    pub ts: u64,
    pub role: Role,
    pub kind: EventKind,
    pub text: String,
    /// Background work still running when the agent stopped: it wakes the agent again
    /// later, so this answer is not the end of the turn.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub background: u32,
    /// The agent's session (Claude's session id, Codex's thread id): the room resumes it
    /// when it restarts an agent for a new role.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// The turn this event belongs to: Claude Code's `prompt_id`, which the prompt's hook
    /// and the Stop that answers it share; Codex's `turn-id`, on both halves of a notify.
    /// The moderator ties an answer to its prompt by it, whatever order the reports come
    /// in, and the reader drops a second report of the same turn however late it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn: Option<String>,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

/// A line in `hook.log` under the state folder: what a hook did with a report it did not
/// write as an event, so a room that goes quiet can be explained afterwards. The file is
/// started over once it grows past a megabyte.
pub fn hook_log(line: &str) {
    let path = home().join("hook.log");
    let _ = fs::create_dir_all(home());
    if fs::metadata(&path).is_ok_and(|m| m.len() > 1_000_000) {
        let _ = fs::write(&path, "");
    }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{} {line}", now_ms());
    }
}

/// How many user messages of a Codex thread the hook has reported so far, and the digest
/// of the last one. Codex's notify lists every message of the thread, and the ones after
/// this count began the turn it reports, as long as the counted ones are still there: a
/// list rewritten at the same length (a compaction) would not match. Kept per room, and
/// started over when the room opens.
pub fn codex_inputs_seen(id: &str, thread: &str) -> Option<(usize, u64)> {
    let text = fs::read_to_string(room_dir(id).join("inputs").join(safe_name(thread))).ok()?;
    let mut parts = text.split_whitespace();
    let n = parts.next()?.parse().ok()?;
    let last = u64::from_str_radix(parts.next()?, 16).ok()?;
    Some((n, last))
}

pub fn set_codex_inputs_seen(id: &str, thread: &str, n: usize, last: u64) {
    let dir = room_dir(id).join("inputs");
    let _ = fs::create_dir_all(&dir);
    let _ = fs::write(dir.join(safe_name(thread)), format!("{n} {last:016x}"));
}

/// A stable digest of a message, the same in every process.
pub fn digest(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

/// A thread id as a file name: its letters, digits and dashes.
fn safe_name(id: &str) -> String {
    let name: String = id.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').collect();
    if name.is_empty() {
        "unknown".into()
    } else {
        name
    }
}

pub fn append_event(id: &str, event: &Event) -> Result<()> {
    let path = events_path(id);
    fs::create_dir_all(path.parent().unwrap())?;
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    // One write per line: hooks of both agents append at once (a Claude reviewer's two
    // hooks fire together), and separate writes for a line and its newline can interleave.
    let mut line = serde_json::to_string(event)?;
    line.push('\n');
    file.write_all(line.as_bytes())?;
    Ok(())
}

/// Drops an event that repeats one already seen. A Claude agent can report each turn
/// twice: through this plugin's installed hooks and through the ones the room gives it.
///
/// A report that names its turn repeats another when the turn, the kind and the words are
/// the same, however late the second copy comes: the same words in another turn are
/// another message, and a prompt and its answer share a turn but differ in kind. The
/// words stay in the key so that a session a hook made continue (a Stop that was blocked)
/// keeps its later, different answer. Without a turn id, a repeat is the same words within
/// a few seconds; both reports come from one stop, so they also agree on `background` and
/// the session, and the same words with other background work, or from another session
/// (a second `/clear`), are a later report.
#[derive(Default)]
pub struct Dedup {
    recent: Vec<Event>,
    seen: std::collections::VecDeque<(Role, Option<String>, String, EventKind, String, u32)>,
}

impl Dedup {
    const WINDOW_MS: u64 = 15_000;
    /// Turn keys kept; the oldest go first.
    const KEEP: usize = 256;

    pub fn fresh(&mut self, ev: &Event) -> bool {
        if let Some(turn) = &ev.turn {
            let key = (ev.role, ev.session.clone(), turn.clone(), ev.kind, ev.text.clone(), ev.background);
            if self.seen.contains(&key) {
                return false;
            }
            if self.seen.len() >= Self::KEEP {
                self.seen.pop_front();
            }
            self.seen.push_back(key);
            return true;
        }
        self.recent.retain(|r| ev.ts.saturating_sub(r.ts) <= Self::WINDOW_MS);
        let repeats = |r: &Event| {
            r.role == ev.role && r.kind == ev.kind && r.text == ev.text && r.background == ev.background && r.session == ev.session
        };
        if self.recent.iter().any(repeats) {
            return false;
        }
        self.recent.push(ev.clone());
        true
    }
}

/// Tails events.jsonl, returning only whole lines appended since the last poll.
pub struct EventReader {
    path: PathBuf,
    offset: u64,
    partial: String,
    dedup: Dedup,
}

impl EventReader {
    /// Starts at the current end of the file: a reopened room does not replay old turns.
    pub fn new(id: &str) -> EventReader {
        let path = events_path(id);
        let offset = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        EventReader { path, offset, partial: String::new(), dedup: Dedup::default() }
    }

    pub fn poll(&mut self) -> Vec<Event> {
        let Ok(mut file) = fs::File::open(&self.path) else { return vec![] };
        if file.seek(SeekFrom::Start(self.offset)).is_err() {
            return vec![];
        }
        let mut buf = String::new();
        let Ok(n) = file.read_to_string(&mut buf) else { return vec![] };
        self.offset += n as u64;
        self.partial.push_str(&buf);
        let mut events = vec![];
        while let Some(nl) = self.partial.find('\n') {
            let line: String = self.partial.drain(..=nl).collect();
            if let Ok(ev) = serde_json::from_str::<Event>(line.trim()) {
                if self.dedup.fresh(&ev) {
                    events.push(ev);
                }
            }
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(ts: u64, role: Role, text: &str) -> Event {
        Event { ts, role, kind: EventKind::Assistant, text: text.into(), background: 0, session: None, turn: None }
    }

    #[test]
    fn a_report_with_a_turn_id_repeats_by_turn_kind_and_words() {
        let mut d = Dedup::default();
        let turn = |ts, kind, text: &str, turn: &str| Event {
            kind,
            turn: Some(turn.into()),
            session: Some("s".into()),
            ..ev(ts, Role::Reviewer, text)
        };
        // a prompt and its answer share a turn: both kept
        assert!(d.fresh(&turn(1_000, EventKind::User, "review this", "t1")));
        assert!(d.fresh(&turn(1_000, EventKind::Assistant, "VERDICT: AGREE", "t1")));
        // the second hook's copies, even long after the window: dropped
        assert!(!d.fresh(&turn(90_000, EventKind::User, "review this", "t1")));
        assert!(!d.fresh(&turn(90_000, EventKind::Assistant, "VERDICT: AGREE", "t1")));
        // the same words in another turn are another message
        assert!(d.fresh(&turn(90_100, EventKind::Assistant, "VERDICT: AGREE", "t2")));
        // the same turn going on with other words (a blocked stop that continued) is kept
        assert!(d.fresh(&turn(90_200, EventKind::Assistant, "VERDICT: AGREE, and tests", "t2")));
        // a report without an id still goes by the words and the window
        assert!(d.fresh(&ev(90_300, Role::Reviewer, "VERDICT: AGREE")));
        assert!(!d.fresh(&ev(90_400, Role::Reviewer, "VERDICT: AGREE")));
        // and by the session: a second `/clear` soon after names another conversation
        let rebound = |ts, session: &str| Event { kind: EventKind::Rebound, session: Some(session.into()), ..ev(ts, Role::Host, "/clear") };
        assert!(d.fresh(&rebound(90_500, "c1")));
        assert!(!d.fresh(&rebound(90_600, "c1")));
        assert!(d.fresh(&rebound(90_700, "c2")));
    }

    #[test]
    fn stale_pane_maps_are_swept() {
        let panes = std::env::temp_dir().join(format!("herdr-rooms-sweep-{}", std::process::id())).join("panes");
        let _ = fs::remove_dir_all(&panes);
        let map = |socket: &str, pane: &str, id: &str| {
            let dir = panes.join(socket_key(socket));
            fs::create_dir_all(&dir).unwrap();
            let json = serde_json::to_string(&PaneMap { id: id.into(), role: Role::Host, socket: socket.into(), started: false, session: None }).unwrap();
            fs::write(dir.join(format!("{}.json", pane.replace(':', "_"))), json).unwrap();
        };
        map("/srv/a.sock", "w1:p1", "room-live");
        map("/srv/a.sock", "w1:p2", "room-live");
        map("/srv/a.sock", "w9:p1", "room-gone");
        // a map of another server's pane, and maps in the old flat layout: one of a room
        // still open, one of a room gone, one unreadable
        map("/srv/b.sock", "w1:p1", "room-b");
        fs::write(panes.join("w1_p3.json"), r#"{"id":"room-old","role":"host","socket":"/srv/a.sock"}"#).unwrap();
        fs::write(panes.join("w1_p4.json"), r#"{"id":"room-live","role":"host","socket":"/srv/a.sock"}"#).unwrap();
        fs::write(panes.join("w1_p5.json"), "not json").unwrap();
        let mut gone = sweep_panes_in(&panes, |pane, map| map.id == "room-live" && pane != "w1:p2" || map.socket == "/srv/b.sock");
        gone.sort();
        assert_eq!(gone, ["w1:p2", "w1:p3", "w1:p5", "w9:p1"]);
        let left = |socket: &str, pane: &str| panes.join(socket_key(socket)).join(format!("{}.json", pane.replace(':', "_"))).is_file();
        assert!(left("/srv/a.sock", "w1:p1") && !left("/srv/a.sock", "w1:p2") && !left("/srv/a.sock", "w9:p1"));
        assert!(left("/srv/b.sock", "w1:p1") && !left("/srv/a.sock", "w1:p3"));
        // the open room's flat map moved into its server's folder, and no flat file is left
        assert!(left("/srv/a.sock", "w1:p4"));
        assert!(fs::read_dir(&panes).unwrap().flatten().all(|e| e.path().is_dir()));
    }

    #[test]
    fn dedup_drops_a_double_report_only() {
        let mut d = Dedup::default();
        assert!(d.fresh(&ev(1_000, Role::Reviewer, "VERDICT: AGREE")));
        assert!(!d.fresh(&ev(1_050, Role::Reviewer, "VERDICT: AGREE")));
        // the same words from the other agent, or much later, are new messages
        assert!(d.fresh(&ev(1_100, Role::Host, "VERDICT: AGREE")));
        assert!(d.fresh(&ev(60_000, Role::Reviewer, "VERDICT: AGREE")));

        // the same words once the background work is done end the turn: not a repeat
        let waiting = |ts, n| Event { background: n, ..ev(ts, Role::Host, "waiting") };
        assert!(d.fresh(&waiting(70_000, 1)));
        assert!(!d.fresh(&waiting(70_050, 1)));
        assert!(d.fresh(&waiting(72_000, 0)));
    }
}
