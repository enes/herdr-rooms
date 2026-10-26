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
}

// ------------------------------------------------------------ pane map

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PaneMap {
    pub id: String,
    pub role: Role,
    /// Socket of the herdr server the pane lives on; pane ids repeat across servers.
    #[serde(default)]
    pub socket: String,
}

fn pane_file(pane: &str) -> PathBuf {
    home().join("panes").join(format!("{}.json", pane.replace([':', '/'], "_")))
}

fn socket() -> String {
    std::env::var("HERDR_SOCKET_PATH").unwrap_or_default()
}

pub fn map_pane(pane: &str, id: &str, role: Role) -> Result<()> {
    let path = pane_file(pane);
    fs::create_dir_all(path.parent().unwrap())?;
    let map = PaneMap { id: id.to_string(), role, socket: socket() };
    fs::write(path, serde_json::to_string(&map)?)?;
    Ok(())
}

pub fn unmap_pane(pane: &str, id: &str) {
    if lookup_pane(pane).is_some_and(|m| m.id == id) {
        let _ = fs::remove_file(pane_file(pane));
    }
}

pub fn lookup_pane(pane: &str) -> Option<PaneMap> {
    lookup_pane_on(pane, &socket())
}

/// `lookup_pane` for a pane of the herdr server at `socket`.
pub fn lookup_pane_on(pane: &str, socket: &str) -> Option<PaneMap> {
    let map: PaneMap = serde_json::from_str(&fs::read_to_string(pane_file(pane)).ok()?).ok()?;
    if !map.socket.is_empty() && !socket.is_empty() && map.socket != socket {
        return None;
    }
    Some(map)
}

/// Whether any pane is in a room: lets a hook that must search for its pane skip the search.
pub fn any_pane_mapped() -> bool {
    fs::read_dir(home().join("panes")).is_ok_and(|mut d| d.next().is_some())
}

// ------------------------------------------------------------ events

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EventKind {
    User,
    Assistant,
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
}

fn is_zero(n: &u32) -> bool {
    *n == 0
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

/// Drops an event that repeats one seen moments ago. A Claude reviewer can report each
/// turn twice: through this plugin's installed hooks and through the ones the room gives it.
#[derive(Default)]
pub struct Dedup {
    recent: Vec<Event>,
}

impl Dedup {
    const WINDOW_MS: u64 = 15_000;

    pub fn fresh(&mut self, ev: &Event) -> bool {
        self.recent.retain(|r| ev.ts.saturating_sub(r.ts) <= Self::WINDOW_MS);
        if self.recent.iter().any(|r| r.role == ev.role && r.kind == ev.kind && r.text == ev.text) {
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
        Event { ts, role, kind: EventKind::Assistant, text: text.into(), background: 0 }
    }

    #[test]
    fn dedup_drops_a_double_report_only() {
        let mut d = Dedup::default();
        assert!(d.fresh(&ev(1_000, Role::Reviewer, "VERDICT: AGREE")));
        assert!(!d.fresh(&ev(1_050, Role::Reviewer, "VERDICT: AGREE")));
        // the same words from the other agent, or much later, are new messages
        assert!(d.fresh(&ev(1_100, Role::Host, "VERDICT: AGREE")));
        assert!(d.fresh(&ev(60_000, Role::Reviewer, "VERDICT: AGREE")));
    }
}
