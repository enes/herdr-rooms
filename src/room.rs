//! The room pane: starts the reviewer, relays messages between the two agents
//! and shows the conversation with who said what.

use crate::review::{
    display_name, Author, Completion, Moderator, Phase, Target, Verdict, EXIT_COMMAND, REBIND_COMMAND, REVIEW_COMMAND, SWAP_COMMAND,
};
use crate::state::{self, RoomConfig, EventReader, Role};
use crate::{herdr, ui};
use anyhow::{Context, Result};
use chrono::TimeZone;
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event, KeyCode, KeyEventKind,
    KeyModifiers, MouseEventKind,
};
use std::cell::Cell;
use ratatui::crossterm::execute;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

type SharedPane = Arc<Mutex<Option<String>>>;
/// The moderator's `busy` for one agent, for its sender to read.
type Busy = Arc<AtomicBool>;

enum Worker {
    Status(Role, String),
    /// This role's sender is about to type a prompt: the turn it starts is one a hook must
    /// report, however soon it ends.
    Typing(Role),
    /// This role's sender is done with a prompt: delivered, or given up on.
    Sent(Role),
    Blocked(Role),
    /// The room split a pane for the agent it starts; `session` is the conversation in it.
    Started { pane: String, name: String, session: Option<String> },
    Info(String),
    Warn(String),
}

pub fn run() -> Result<()> {
    let id = std::env::var("ROOM_ID").context("ROOM_ID is unset: /room opens the room")?;
    let mut config = RoomConfig::load(&id)?;
    sweep_stale_maps();
    config.room_pid = Some(std::process::id());
    config.save()?;
    // A room that opens again knows nothing of the turns since it closed: Codex's input
    // counts start over, so the first notify reports only its last message.
    let _ = std::fs::remove_dir_all(state::room_dir(&id).join("inputs"));
    config.map_panes()?;
    let (tx, rx) = mpsc::channel();
    let panes: [SharedPane; 2] = [Role::Host, Role::Reviewer].map(|role| Arc::new(Mutex::new(config.pane(role))));
    let (host_busy, reviewer_busy) = (Busy::default(), Busy::default());
    let host_q = spawn_sender(Role::Host, panes[0].clone(), host_busy.clone(), tx.clone());
    let reviewer_q = spawn_sender(Role::Reviewer, panes[1].clone(), reviewer_busy.clone(), tx.clone());
    spawn_status_poller(panes.clone(), tx.clone());
    if config.reviewer_pane.is_none() {
        let (d, pane, tx) = (config.clone(), panes[slot(config.started_role())].clone(), tx.clone());
        thread::spawn(move || start_agent(d, pane, tx));
    }

    let mut room = Room::new(config, panes, host_q, reviewer_q, [host_busy, reviewer_busy], tx, rx);
    let mut terminal = ratatui::init();
    // Without it a pasted line break arrives as Enter, and each line went out as its own
    // message. The mouse is captured for the wheel to scroll the log.
    let _ = execute!(std::io::stdout(), EnableBracketedPaste, EnableMouseCapture);
    let result = room.run_loop(&mut terminal);
    let _ = execute!(std::io::stdout(), DisableMouseCapture, DisableBracketedPaste);
    ratatui::restore();
    room.close();
    if room.exit_all {
        // `/exit`: the pane of the agent the room started goes, then this one (herdr would
        // close it as we exit anyway). The session the room was opened from stays.
        if let Some(p) = room.config.reviewer_pane.clone() {
            let _ = herdr::close_pane(&p);
        }
        if let Ok(own) = std::env::var("HERDR_PANE_ID") {
            let _ = herdr::close_pane(&own);
        }
    }
    result
}

/// Removes pane maps nothing vouches for any more: a room that crashed, or a herdr that
/// quit, leaves maps behind, and herdr gives new panes the old ids, so a hook in a new
/// agent would report to an old room. A map stays while the room that wrote it still
/// runs, or while it belongs to another herdr server this process cannot ask.
fn sweep_stale_maps() {
    let live = herdr::pane_ids().ok();
    let socket = state::socket_path();
    let gone = state::sweep_panes(|pane, map| {
        if map.socket != socket {
            return true;
        }
        let room = RoomConfig::load(&map.id).ok();
        // A room opened before rooms recorded their process cannot be asked: its maps stay
        // while their panes do.
        let shown = room.as_ref().is_some_and(|r| r.room_pid.is_none_or(process_alive));
        let pane_there = live.as_ref().is_none_or(|l| l.iter().any(|p| p == pane));
        room.is_some() && shown && pane_there
    });
    if !gone.is_empty() {
        state::hook_log(&format!("room start swept stale pane maps: {}", gone.join(", ")));
    }
}

fn process_alive(pid: u32) -> bool {
    std::process::Command::new("kill").args(["-0", &pid.to_string()]).output().is_ok_and(|o| o.status.success())
}

/// How long the room waits for a hook to report a turn it saw end, before it says so.
const REPORT_TIMEOUT: Duration = Duration::from_secs(20);

// ------------------------------------------------------------ background work

/// The index of `role` in arrays kept per role: the host's, then the reviewer's.
fn slot(role: Role) -> usize {
    match role {
        Role::Host => 0,
        Role::Reviewer => 1,
    }
}

fn role_word(role: Role) -> &'static str {
    match role {
        Role::Host => "host",
        Role::Reviewer => "reviewer",
    }
}

/// What the agent the room runs needs to start: a pane mapped to the room, and its command
/// line.
struct Launch {
    pane: String,
    name: String,
    session: Option<String>,
    args: Vec<String>,
}

/// Everything that can fail before the agent the room runs starts, in the role it has now:
/// its command line, then a pane split to the right of the host's. With `resume` it
/// continues its conversation, `d.reviewer_session`.
fn prepare(d: &RoomConfig, resume: bool) -> Result<Launch> {
    // Claude takes the id the room gives it; a new Codex reports its own with its first turn.
    let session = match (resume, d.reviewer_kind.as_str()) {
        (true, _) => d.reviewer_session.clone(),
        (false, "claude") => Some(new_uuid()),
        (false, _) => None,
    };
    let args = agent_args(d, session.as_deref(), resume)?;
    let pane = herdr::pane_split(&d.host_pane, "right", &d.cwd).context("could not open a pane")?;
    let name = format!("{}-{}-{}", d.reviewer_kind, role_word(d.started_role()), &d.id[d.id.len().saturating_sub(4)..]);
    let _ = state::map_pane(&pane, &d.id, d.started_role(), true, session.as_deref());
    Ok(Launch { pane, name, session, args })
}

/// Starts the prepared agent. Its sender gets the pane once the agent is up, so no prompt
/// is typed into a pane that is still starting; a pane without an agent fails the prompts
/// typed into it, and the room says so.
fn launch(d: &RoomConfig, l: Launch, pane: &SharedPane, tx: &Sender<Worker>) {
    let (who, role) = (display_name(&d.reviewer_kind), role_word(d.started_role()));
    let _ = tx.send(Worker::Started { pane: l.pane.clone(), name: l.name.clone(), session: l.session });
    let started = herdr::agent_start(&l.name, &d.reviewer_kind, &l.pane, &l.args);
    *pane.lock().unwrap() = Some(l.pane);
    match started {
        Ok(()) => {
            let _ = tx.send(Worker::Info(format!("{who} is ready as {role} ({}).", protection(d))));
        }
        Err(e) if herdr::error_code(&e) == Some("agent_not_ready") => {
            let _ = tx.send(Worker::Warn(format!(
                "{who} is waiting for approval at startup (folder trust, update…). Answer it in its pane."
            )));
        }
        Err(e) => {
            let _ = tx.send(Worker::Warn(format!("Could not start {who}: {e}")));
        }
    }
}

/// Starts the agent the room runs, in a pane to the right of the host's.
fn start_agent(d: RoomConfig, pane: SharedPane, tx: Sender<Worker>) {
    let (who, role) = (display_name(&d.reviewer_kind), role_word(d.started_role()));
    let _ = tx.send(Worker::Info(format!("Starting {who} as {role}…")));
    match prepare(&d, false) {
        Ok(l) => launch(&d, l, &pane, &tx),
        Err(e) => {
            let _ = tx.send(Worker::Warn(format!("Could not start {who}: {e:#}")));
        }
    }
}

/// Starts the agent the room runs again, prepared for the role it has after a switch, so
/// its permissions follow its role. Its old pane, whose agent is idle, closes first.
fn restart_agent(d: RoomConfig, l: Launch, pane: SharedPane, old: String, tx: Sender<Worker>) {
    state::unmap_pane(&old, &d.id);
    let _ = herdr::close_pane(&old);
    launch(&d, l, &pane, &tx);
}

/// A random version-4 UUID for `claude --session-id`, from the random keys std seeds its
/// hash maps with.
fn new_uuid() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut bytes = [0u8; 16];
    for (i, half) in bytes.chunks_mut(8).enumerate() {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(state::now_ms());
        h.write_usize(i);
        half.copy_from_slice(&h.finish().to_be_bytes());
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..])
}

fn exe() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("herdr-rooms"))
}

/// Standing orders for the agent a read-only room starts, given as its system (Claude) or
/// developer (Codex) instructions so they hold in every turn. They follow the role in the
/// room's latest `(room) Role:` message: a resumed Codex keeps the developer instructions
/// it started with, and a switch must not contradict them. The sandboxes enforce them.
const CLAUDE_READ_ONLY_RULE: &str = "You are in a review room. Your role is the one named in the room's latest \"(room) Role:\" message; until one comes, you are the reviewer. After a context compaction the room repeats your role; until it does, keep the last role you were given. As the reviewer, the repository is read-only for you: never create, edit, move or delete files in it, and do not run commands that write to it (sed -i, output redirection into files, git checkout/reset/stash/commit, formatters, installers, code generators). Read files, inspect git state and diffs, and run checks that only read. If a check must write (a build, a test cache, a scratch file), point it at a fresh directory under $TMPDIR. When a fix is needed, show it as a diff in your reply instead of applying it. While you review, your shell is sandboxed: writes to the repository fail with Operation not permitted.\n";
/// Kept short: herdr types the agent's command line into a shell, and a long line can be
/// cut off before its Enter arrives.
const CODEX_READ_ONLY_RULE: &str = "Your role in this review room is the one in its latest (room) Role: message, reviewer until one comes; after a compaction the room repeats it, keep the last one until then. As reviewer you are read-only: never create, edit or delete files or run commands that write; your sandbox blocks every write, $TMPDIR included, and you cannot leave it, so run checks inline (python3 -c) and show fixes as diffs.";

/// A TOML basic string, for `-c key=value` (JSON string escapes are valid TOML).
fn toml_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_default()
}

/// The repository the host works in: the whole git work tree when there is one.
fn repo_root(cwd: &str) -> String {
    std::process::Command::new("git")
        .args(["-C", cwd, "rev-parse", "--show-toplevel"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|root| !root.is_empty())
        .unwrap_or_else(|| cwd.to_string())
}

/// Whether the agent the room started is held to read-only now: only while it reviews.
/// As host it gets what a reviewer that may write gets.
fn read_only_now(d: &RoomConfig) -> bool {
    d.reviewer_read_only && d.started_role() == Role::Reviewer
}

/// How the agent the room started is held back, for the room's "ready" line.
fn protection(d: &RoomConfig) -> &'static str {
    match (read_only_now(d), d.reviewer_kind.as_str()) {
        (false, _) => "can write to the repository",
        (true, "codex") => "read-only: OS sandbox, no escalation",
        (true, "claude") => "read-only: sandboxed shell, no edit tools, no MCP",
        (true, _) => "read-only",
    }
}

/// Native CLI arguments for the agent the room starts, in the role it has now, including
/// the hook that reports its turns. `session` is the conversation it resumes, or with
/// `resume` false the id a new Claude takes. Claude's settings and system prompt go into
/// files in the room's folder, so the command line herdr types stays short.
fn agent_args(d: &RoomConfig, session: Option<&str>, resume: bool) -> Result<Vec<String>> {
    let exe = exe().display().to_string();
    let read_only = read_only_now(d);
    let dir = state::room_dir(&d.id);
    let mut args: Vec<String> = vec![];
    match d.reviewer_kind.as_str() {
        "codex" => {
            if let (true, Some(id)) = (resume, session) {
                args.extend(["resume".into(), id.into()]);
            }
            args.extend(["-c".into(), "check_for_update_on_startup=false".into()]);
            args.extend(["-c".into(), format!("notify=[{},\"hook\",\"codex-notify\"]", toml_str(&exe))]);
            if read_only {
                // `-a never`: the model cannot ask to run outside the read-only sandbox.
                args.extend(["-s".into(), "read-only".into(), "-a".into(), "never".into()]);
            } else {
                args.extend(["-s".into(), "workspace-write".into()]);
            }
            if d.reviewer_read_only {
                args.extend(["-c".into(), format!("developer_instructions={}", toml_str(CODEX_READ_ONLY_RULE))]);
            }
            if let Some(m) = &d.reviewer_model {
                args.extend(["-m".into(), m.clone()]);
            }
            if let Some(e) = &d.reviewer_effort {
                args.extend(["-c".into(), format!("model_reasoning_effort={}", toml_str(e))]);
            }
        }
        "claude" => {
            match (resume, session) {
                (true, Some(id)) => args.extend(["--resume".into(), id.into()]),
                (false, Some(id)) => args.extend(["--session-id".into(), id.into()]),
                _ => {}
            }
            let hook = |kind: &str| serde_json::json!([{ "hooks": [{ "type": "command", "command": format!("'{exe}' hook {kind}") }] }]);
            // SessionStart after a compaction, when the room reminds the agent of its role, and
            // after `/clear`, when the room follows the agent to its new conversation.
            let on_session_start = serde_json::json!([{
                "matcher": "compact|clear",
                "hooks": [{ "type": "command", "command": format!("'{exe}' hook claude-session") }]
            }]);
            let mut settings = serde_json::json!({
                "hooks": { "UserPromptSubmit": hook("claude-prompt"), "Stop": hook("claude-stop"), "SessionStart": on_session_start }
            });
            if read_only {
                // Bash runs in the OS sandbox with the repository write-denied. No
                // dangerouslyDisableSandbox escape, and no unsandboxed fallback: if the
                // sandbox cannot start, Claude exits instead.
                settings["sandbox"] = serde_json::json!({
                    "enabled": true,
                    "failIfUnavailable": true,
                    "autoAllowBashIfSandboxed": true,
                    "allowUnsandboxedCommands": false,
                    "filesystem": { "denyWrite": [repo_root(&d.cwd)] },
                });
            }
            std::fs::create_dir_all(&dir)?;
            let settings_file = dir.join("agent-settings.json");
            std::fs::write(&settings_file, serde_json::to_string_pretty(&settings)?)?;
            args.extend(["--settings".into(), settings_file.display().to_string()]);
            if read_only {
                // Plan mode would stop at a plan-approval dialog. Instead: no edit tools, no
                // MCP servers (they run outside the sandbox), and the rule in the system prompt.
                args.extend(["--disallowedTools".into(), "Edit,Write,NotebookEdit".into()]);
                args.push("--strict-mcp-config".into());
            }
            if d.reviewer_read_only {
                let rules_file = dir.join("agent-rules.md");
                std::fs::write(&rules_file, CLAUDE_READ_ONLY_RULE)?;
                args.extend(["--append-system-prompt-file".into(), rules_file.display().to_string()]);
            }
            if let Some(m) = &d.reviewer_model {
                args.extend(["--model".into(), m.clone()]);
            }
            if let Some(e) = &d.reviewer_effort {
                args.extend(["--effort".into(), e.clone()]);
            }
        }
        _ => {}
    }
    Ok(args)
}

/// One queue per role: prompts go out one at a time, each once the agent in that role is
/// ready.
fn spawn_sender(role: Role, pane: SharedPane, busy: Busy, tx: Sender<Worker>) -> Sender<String> {
    let (qtx, qrx) = mpsc::channel::<String>();
    thread::spawn(move || {
        for text in qrx {
            deliver(role, &pane, &busy, &text, &tx);
            let _ = tx.send(Worker::Sent(role));
        }
    });
    qtx
}

/// Types `text` into the agent once it has settled at its prompt, and makes sure a turn
/// really starts: an agent that is still booting can swallow input without a trace.
fn deliver(role: Role, pane: &SharedPane, busy: &Busy, text: &str, tx: &Sender<Worker>) {
    let who = match role {
        Role::Host => "The host",
        Role::Reviewer => "The reviewer",
    };
    let (mut warned, mut waited, mut failures, mut tries, mut idle_polls) = (false, false, 0, 0, 0);
    loop {
        thread::sleep(Duration::from_millis(700));
        let Some(target) = pane.lock().unwrap().clone() else { continue };
        let status = match herdr::agent_get(&target) {
            Ok(a) => a.status,
            Err(e) => {
                failures += 1;
                if failures == 10 {
                    let _ = tx.send(Worker::Warn(format!("{who} cannot be reached: {e}")));
                    return;
                }
                continue;
            }
        };
        match status.as_str() {
            // Waiting at its prompt, but its background work will wake it for more of this turn.
            "idle" | "done" if busy.load(Ordering::Relaxed) => {
                idle_polls = 0;
                if !waited {
                    let _ = tx.send(Worker::Info(format!(
                        "{who} has background work running; the message goes once that work reports back."
                    )));
                    waited = true;
                }
                continue;
            }
            "idle" | "done" => idle_polls += 1,
            "blocked" => {
                idle_polls = 0;
                if !warned {
                    let _ = tx.send(Worker::Blocked(role));
                    warned = true;
                }
                continue;
            }
            _ => {
                idle_polls = 0;
                continue;
            }
        }
        // Settled: ready on two polls in a row.
        if idle_polls < 2 {
            continue;
        }
        idle_polls = 0;
        tries += 1;
        // `agent prompt --wait` returns once the turn is over when it is short (an error
        // ends it at once): the room must know of the prompt before then.
        let _ = tx.send(Worker::Typing(role));
        match herdr::agent_prompt_started(&target, text) {
            Ok(true) => break,
            Ok(false) => {
                if herdr::agent_submit(&target) {
                    break;
                }
                if tries == 3 {
                    let _ = tx.send(Worker::Warn(format!(
                        "{who} did not pick up a message after 3 tries. It is in the room log; resend it from here."
                    )));
                    return;
                }
            }
            Err(e) if herdr::error_code(&e) == Some("agent_blocked") => {
                if !warned {
                    let _ = tx.send(Worker::Blocked(role));
                    warned = true;
                }
            }
            Err(e) => {
                failures += 1;
                if failures == 10 {
                    let _ = tx.send(Worker::Warn(format!("Could not deliver a message to {}: {e}", who.to_lowercase())));
                    return;
                }
            }
        }
    }
}

fn spawn_status_poller(panes: [SharedPane; 2], tx: Sender<Worker>) {
    thread::spawn(move || loop {
        for (role, pane) in [Role::Host, Role::Reviewer].into_iter().zip(&panes) {
            let target = pane.lock().unwrap().clone();
            if let Some(target) = target {
                let status = herdr::agent_get(&target).map(|a| a.status).unwrap_or_else(|_| "gone".into());
                if tx.send(Worker::Status(role, status)).is_err() {
                    return;
                }
            }
        }
        thread::sleep(Duration::from_secs(1));
    });
}

// ------------------------------------------------------------ the room

struct Room {
    config: RoomConfig,
    moderator: Moderator,
    reader: EventReader,
    /// The host's pane, then the reviewer's: where each role's sender types and what the
    /// status poller reads. A switch swaps them; an agent that restarts has none meanwhile.
    panes: [SharedPane; 2],
    host_q: Sender<String>,
    reviewer_q: Sender<String>,
    /// Shared with the senders: the host's, then the reviewer's.
    busy: [Busy; 2],
    /// Prompts handed to each role's sender that it is not done with yet.
    in_flight: [u32; 2],
    /// Status polls in a row that found each role's agent at its prompt.
    settled: [u32; 2],
    /// `/swap` was typed: the roles switch once nothing is in progress.
    swap_pending: bool,
    /// Per role, the stray sessions the room has already told the user about.
    announced_stray: [Vec<String>; 2],
    /// Per role, since when a turn the status poller saw end awaits its hook's report.
    awaiting_report: [Option<Instant>; 2],
    /// Per role, the room delivered a prompt and no report has come since: the next turn
    /// to end is one a hook must report.
    delivered_unreported: [bool; 2],
    /// The agent the room started has had a turn in its conversation, which a restart
    /// must keep.
    started_spoke: bool,
    tx: Sender<Worker>,
    rx: Receiver<Worker>,
    host_status: String,
    reviewer_status: String,
    target: Target,
    input: String,
    /// Where the next character goes, in characters from the start of `input`.
    cursor: usize,
    /// The highlighted completion, while the word being typed has some.
    pick: usize,
    /// The input as it was when Esc hid its completions: they stay hidden until it changes.
    dismissed: Option<String>,
    /// `/exit` was typed: close the reviewer's pane and the room on the way out.
    exit_all: bool,
    /// Ctrl+Q: close the room, keep the reviewer.
    quit: bool,
    /// Lines the log is scrolled up from its end.
    scroll_back: u16,
    /// How far up the log can go, as the last draw measured it.
    scroll_max: Cell<u16>,
    last_phase: Phase,
    /// Per role, a Codex thread's record the room watches for compactions, and how much of
    /// it it has read.
    rollout: [Option<(PathBuf, u64)>; 2],
}

impl Room {
    fn new(
        config: RoomConfig,
        panes: [SharedPane; 2],
        host_q: Sender<String>,
        reviewer_q: Sender<String>,
        busy: [Busy; 2],
        tx: Sender<Worker>,
        rx: Receiver<Worker>,
    ) -> Room {
        let mut moderator = Moderator::new(&config);
        let (host, reviewer) = (moderator.host_name.clone(), moderator.reviewer_name.clone());
        moderator.system(format!(
            "Room open. {host} hosts, {reviewer} reviews every request, up to {} rounds each.",
            config.max_rounds
        ));
        Room {
            reader: EventReader::new(&config.id),
            // A room that reopens on a running agent cannot tell whether it has spoken.
            started_spoke: config.reviewer_pane.is_some(),
            config,
            moderator,
            panes,
            host_q,
            reviewer_q,
            busy,
            in_flight: [0; 2],
            settled: [0; 2],
            swap_pending: false,
            announced_stray: [vec![], vec![]],
            awaiting_report: [None, None],
            delivered_unreported: [false, false],
            tx,
            rx,
            host_status: String::new(),
            reviewer_status: String::new(),
            target: Target::Both,
            input: String::new(),
            cursor: 0,
            pick: 0,
            dismissed: None,
            exit_all: false,
            quit: false,
            scroll_back: 0,
            scroll_max: Cell::new(0),
            last_phase: Phase::WaitingUser,
            rollout: [None, None],
        }
    }

    fn close(&self) {
        state::unmap_pane(&self.config.host_pane, &self.config.id);
        if let Some(p) = &self.config.reviewer_pane {
            state::unmap_pane(p, &self.config.id);
        }
    }

    fn run_loop(&mut self, terminal: &mut ratatui::DefaultTerminal) -> Result<()> {
        loop {
            self.pump();
            terminal.draw(|f| self.draw(f))?;
            if !event::poll(Duration::from_millis(150))? {
                continue;
            }
            let key = match event::read()? {
                Event::Key(key) => key,
                Event::Paste(text) => {
                    self.insert(&text.replace("\r\n", "\n").replace('\r', "\n"));
                    continue;
                }
                Event::Mouse(m) => {
                    self.wheel(m.kind);
                    continue;
                }
                _ => continue,
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            // While the word being typed has completions, the list takes ↑ ↓ Tab Enter Esc.
            let offers = self.offers();
            if !offers.is_empty() && !key.modifiers.contains(KeyModifiers::SHIFT) {
                let n = offers.len();
                match key.code {
                    KeyCode::Up => self.pick = (self.pick + n - 1) % n,
                    KeyCode::Down => self.pick = (self.pick + 1) % n,
                    KeyCode::Tab | KeyCode::Enter => self.complete(&offers[self.pick.min(n - 1)]),
                    KeyCode::Esc => self.dismissed = Some(self.input.clone()),
                    _ => self.key(key.code, key.modifiers),
                }
            } else {
                self.key(key.code, key.modifiers);
            }
            if self.exit_all || self.quit {
                return Ok(());
            }
        }
    }

    fn key(&mut self, code: KeyCode, mods: KeyModifiers) {
        let (ctrl, alt, shift) = (
            mods.contains(KeyModifiers::CONTROL),
            mods.contains(KeyModifiers::ALT),
            mods.contains(KeyModifiers::SHIFT),
        );
        let len = self.input.chars().count();
        match code {
            KeyCode::Char('q') | KeyCode::Char('c') if ctrl => self.quit = true,
            KeyCode::Char('p') if ctrl => self.moderator.toggle_pause(),
            KeyCode::Char('e') if ctrl => self.moderator.end(),
            KeyCode::Tab => self.target = self.target.next(),
            // The log: a page, a line, its top, its end (the box's keys when it is empty).
            KeyCode::PageUp => self.scroll(10),
            KeyCode::PageDown => self.scroll(-10),
            KeyCode::Up if shift => self.scroll(1),
            KeyCode::Down if shift => self.scroll(-1),
            KeyCode::Home if self.input.is_empty() => self.scroll_back = self.scroll_max.get(),
            KeyCode::End if self.input.is_empty() => self.scroll_back = 0,
            // The input box: a cursor that moves by character or word.
            KeyCode::Home => self.cursor = 0,
            KeyCode::End => self.cursor = len,
            KeyCode::Left if alt => self.cursor = self.word_left(),
            KeyCode::Right if alt => self.cursor = self.word_right(),
            KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Right => self.cursor = (self.cursor + 1).min(len),
            KeyCode::Esc => self.clear_input(),
            KeyCode::Backspace => self.backspace(),
            KeyCode::Delete => self.delete(),
            KeyCode::Enter if self.input.trim() == EXIT_COMMAND => {
                self.moderator.end();
                self.exit_all = true;
            }
            KeyCode::Enter if self.input.trim() == SWAP_COMMAND => {
                self.clear_input();
                self.request_swap();
            }
            KeyCode::Enter if self.input.trim() == REVIEW_COMMAND => {
                self.clear_input();
                self.moderator.review_unreviewed();
            }
            KeyCode::Enter if self.input.trim().strip_prefix(REBIND_COMMAND).is_some_and(|rest| rest.is_empty() || rest.starts_with(' ')) => {
                let which = self.input.trim()[REBIND_COMMAND.len()..].trim().to_string();
                self.clear_input();
                self.rebind(&which);
            }
            KeyCode::Enter => {
                let text = std::mem::take(&mut self.input);
                self.cursor = 0;
                self.pick = 0;
                let to = self.moderator.mentioned(&text).unwrap_or(self.target);
                self.moderator.on_room_input(to, &text);
                self.scroll_back = 0;
            }
            KeyCode::Char(c) if !ctrl && !alt => self.insert(&c.to_string()),
            _ => {}
        }
    }

    /// The mouse wheel scrolls the log.
    fn wheel(&mut self, kind: MouseEventKind) {
        match kind {
            MouseEventKind::ScrollUp => self.scroll(3),
            MouseEventKind::ScrollDown => self.scroll(-3),
            _ => {}
        }
    }

    /// Scrolls the log `lines` up (down when negative), within what there is to see.
    fn scroll(&mut self, lines: i32) {
        let back = (i32::from(self.scroll_back) + lines).max(0) as u16;
        self.scroll_back = back.min(self.scroll_max.get());
    }

    /// The byte at which character `i` of the input starts (its end for `i` past the last).
    fn byte_at(&self, i: usize) -> usize {
        self.input.char_indices().nth(i).map_or(self.input.len(), |(b, _)| b)
    }

    fn insert(&mut self, text: &str) {
        let at = self.byte_at(self.cursor);
        self.input.insert_str(at, text);
        self.cursor += text.chars().count();
        self.pick = 0;
    }

    fn backspace(&mut self) {
        if self.cursor > 0 {
            let (from, to) = (self.byte_at(self.cursor - 1), self.byte_at(self.cursor));
            self.input.replace_range(from..to, "");
            self.cursor -= 1;
            self.pick = 0;
        }
    }

    fn delete(&mut self) {
        if self.cursor < self.input.chars().count() {
            let (from, to) = (self.byte_at(self.cursor), self.byte_at(self.cursor + 1));
            self.input.replace_range(from..to, "");
            self.pick = 0;
        }
    }

    fn clear_input(&mut self) {
        self.input.clear();
        self.cursor = 0;
        self.pick = 0;
    }

    /// The start of the word before the cursor.
    fn word_left(&self) -> usize {
        let chars: Vec<char> = self.input.chars().collect();
        let mut i = self.cursor.min(chars.len());
        while i > 0 && chars[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !chars[i - 1].is_whitespace() {
            i -= 1;
        }
        i
    }

    /// The end of the word after the cursor.
    fn word_right(&self) -> usize {
        let chars: Vec<char> = self.input.chars().collect();
        let mut i = self.cursor.min(chars.len());
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        while i < chars.len() && !chars[i].is_whitespace() {
            i += 1;
        }
        i
    }

    /// Completions for the word before the cursor, unless Esc hid them for this input.
    fn offers(&self) -> Vec<Completion> {
        if self.dismissed.as_deref() == Some(self.input.as_str()) {
            return vec![];
        }
        self.moderator.completions(&self.input[..self.byte_at(self.cursor)])
    }

    /// Puts `c` in place of the word before the cursor, with a space after it.
    fn complete(&mut self, c: &Completion) {
        let end = self.byte_at(self.cursor);
        let start = self.input[..end].char_indices().rev().find(|(_, ch)| ch.is_whitespace()).map_or(0, |(i, ch)| i + ch.len_utf8());
        let replacement = format!("{} ", c.word);
        self.input.replace_range(start..end, &replacement);
        self.cursor = self.input[..start].chars().count() + replacement.chars().count();
        self.pick = 0;
    }

    /// Moves worker news onto the log, events in, and prompts out.
    fn pump(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Worker::Status(role, s) => {
                    let settled = &mut self.settled[slot(role)];
                    *settled = if matches!(s.as_str(), "idle" | "done") { *settled + 1 } else { 0 };
                    let was = match role {
                        Role::Host => std::mem::replace(&mut self.host_status, s.clone()),
                        Role::Reviewer => std::mem::replace(&mut self.reviewer_status, s.clone()),
                    };
                    // A turn ended: its hook should report it shortly, when it was a turn at
                    // all. A `!` command, Claude Code's bash mode or `/compact` also take an
                    // agent through working and back, and no hook reports those; so the room
                    // expects a report only for a prompt it delivered, or a prompt it saw
                    // begin a turn (a message typed into a Claude Code pane).
                    let expected = self.delivered_unreported[slot(role)] || self.moderator.has_open_turn(role);
                    if was == "working" && matches!(s.as_str(), "idle" | "done") && expected {
                        self.awaiting_report[slot(role)] = Some(Instant::now());
                    }
                    self.check_stray(role);
                    self.watch_rollout(role);
                }
                Worker::Typing(role) => self.delivered_unreported[slot(role)] = true,
                Worker::Sent(role) => self.in_flight[slot(role)] = self.in_flight[slot(role)].saturating_sub(1),
                Worker::Started { pane, name, session } => {
                    self.config.reviewer_pane = Some(pane);
                    self.config.reviewer_name = Some(name);
                    self.config.reviewer_session = session;
                    let _ = self.config.save();
                }
                Worker::Blocked(role) => {
                    let name = self.name(role);
                    self.moderator.system(format!("⚠ {name} needs approval: answer it in its pane; the message goes through after."));
                }
                Worker::Info(s) => self.moderator.system(s),
                Worker::Warn(s) => self.moderator.system(format!("⚠ {s}")),
            }
        }
        for ev in self.reader.poll() {
            self.awaiting_report[slot(ev.role)] = None;
            self.delivered_unreported[slot(ev.role)] = false;
            if ev.kind == state::EventKind::Rebound {
                // The agent is on another conversation and its hook bound the pane to it:
                // the room binds its own record of the session too, once (a Claude agent's
                // two hooks both report it).
                match ev.session.as_deref() {
                    Some(session) if self.config.session(ev.role) != Some(session) => self.bind_session(ev.role, session),
                    _ => continue,
                }
            } else if ev.role == self.config.started_role() {
                // Any turn of its session (any turn at all, until the room knows the session)
                // is a conversation a restart must keep; only an echo of the room's own prompt
                // names the session for certain, as a Codex internal turn (a thread's title,
                // a catch-up) runs in a thread of its own.
                if self.config.reviewer_session.is_none() || ev.session == self.config.reviewer_session {
                    self.started_spoke = true;
                }
                if self.moderator.is_room_prompt(&ev) {
                    self.started_turn(&ev);
                }
            }
            self.moderator.on_event(ev);
            self.scroll_back = 0;
        }
        for role in [Role::Host, Role::Reviewer] {
            if self.awaiting_report[slot(role)].is_some_and(|since| since.elapsed() >= REPORT_TIMEOUT) {
                self.awaiting_report[slot(role)] = None;
                self.report_missing(role);
            }
        }
        for (role, busy) in [Role::Host, Role::Reviewer].into_iter().zip(&self.busy) {
            busy.store(self.moderator.busy(role), Ordering::Relaxed);
        }
        for out in std::mem::take(&mut self.moderator.outbox) {
            let q = match out.to {
                Role::Host => &self.host_q,
                Role::Reviewer => &self.reviewer_q,
            };
            let _ = q.send(out.text);
            self.in_flight[slot(out.to)] += 1;
        }
        if self.swap_pending && self.swap_ready() {
            self.swap();
        }
        if self.moderator.phase != self.last_phase {
            self.last_phase = self.moderator.phase;
            let rounds = self.moderator.round;
            let body = match self.moderator.phase {
                Phase::Agreed => Some(format!("agreed · {rounds} round(s)")),
                Phase::NoConsensus => Some(format!("no consensus · {rounds} rounds · your call")),
                _ => None,
            };
            if let Some(body) = body {
                thread::spawn(move || herdr::notify("Rooms", &body));
            }
        }
    }

    fn name(&self, role: Role) -> String {
        self.moderator.name(role).to_string()
    }

    /// Notes a prompt of the room's reaching the agent the room started: its hooks name the
    /// session to resume.
    fn started_turn(&mut self, ev: &state::Event) {
        if ev.session.is_some() && ev.session != self.config.reviewer_session {
            self.config.reviewer_session = ev.session.clone();
            let _ = self.config.save();
            // The pane is that session's: its hooks report for it and no other's do.
            if let Some(p) = &self.config.reviewer_pane {
                let _ = state::map_pane(p, &self.config.id, self.config.started_role(), true, ev.session.as_deref());
            }
        }
    }

    /// The agent in `role` finished a turn and no hook reported it: the room says so, with
    /// what Codex's own record says ended the turn when it can tell, and nothing waits on
    /// that turn any more.
    fn report_missing(&mut self, role: Role) {
        let waiting = self.moderator.turn_lost(role);
        let name = self.name(role);
        let mut text = format!(
            "⚠ {name} finished a turn, but no hook reported it within {}s: the turn may have ended in an error or been \
             interrupted (Esc, a `!` command, /compact), its hooks may not be running, or another session's hooks \
             reported instead (see hook.log).",
            REPORT_TIMEOUT.as_secs()
        );
        let session = if role == self.config.started_role() { &self.config.reviewer_session } else { &self.config.host_session };
        if self.moderator.kind(role) == "codex" {
            if let Some(error) = session.as_deref().and_then(|t| crate::bind::codex_last_error(&crate::bind::codex_home(), t)) {
                text.push_str(&format!(" Codex says: {error}"));
            }
        }
        if waiting {
            text.push_str(" The review was waiting on that turn: send the request again, or Ctrl+E ends the review.");
        }
        self.moderator.system(text);
    }

    /// Codex keeps its own record of a thread, and a `compacted` line in it is the one
    /// sign of a compaction the room gets from a Codex (Claude Code has a hook for it).
    /// Read once a second from where the last read ended; the record's past is not news.
    fn watch_rollout(&mut self, role: Role) {
        if self.moderator.kind(role) != "codex" {
            return;
        }
        let session = if role == self.config.started_role() { &self.config.reviewer_session } else { &self.config.host_session };
        let Some(thread) = session.clone() else { return };
        let i = slot(role);
        let path = match &self.rollout[i] {
            Some((path, _)) if path.to_string_lossy().contains(&thread) => path.clone(),
            _ => {
                let Some(path) = crate::bind::codex_rollout(&crate::bind::codex_home(), &thread) else { return };
                let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                self.rollout[i] = Some((path, len));
                return;
            }
        };
        let Ok(len) = std::fs::metadata(&path).map(|m| m.len()) else { return };
        let seen = self.rollout[i].as_ref().map_or(0, |(_, seen)| *seen);
        if len <= seen {
            if len < seen {
                // The record was replaced: start over from its end.
                self.rollout[i] = Some((path, len));
            }
            return;
        }
        use std::io::{Read, Seek, SeekFrom};
        let mut tail = vec![];
        let Ok(mut file) = std::fs::File::open(&path) else { return };
        if file.seek(SeekFrom::Start(seen)).is_err() || file.read_to_end(&mut tail).is_err() {
            return;
        }
        // Whole lines only: a record (a compaction's carries the history it kept, so it is
        // long) can be half written when the room looks. Codex writes each record as
        // {"timestamp":…,"ordinal":…,"type":…,"payload":…}.
        let whole = tail.iter().rposition(|&b| b == b'\n').map_or(0, |at| at + 1);
        let compacted = String::from_utf8_lossy(&tail[..whole])
            .lines()
            .any(|l| serde_json::from_str::<serde_json::Value>(l).is_ok_and(|v| v["type"] == "compacted"));
        self.rollout[i] = Some((path, seen + whole as u64));
        if compacted {
            self.moderator.on_event(state::Event {
                ts: state::now_ms(),
                role,
                kind: state::EventKind::Compacted,
                text: "compact".into(),
                background: 0,
                session: Some(thread),
                turn: None,
            });
        }
    }

    /// Sessions other than the pane's reported for `role`'s pane: each is shown once, with
    /// what it said first, so the user can tell the agent's new conversation (after `/new`
    /// or `/clear`) from a `claude -p` or `codex exec` run there, and `/rebind <id>` it.
    fn check_stray(&mut self, role: Role) {
        for stray in state::strays(&self.config.id, role) {
            if self.announced_stray[slot(role)].contains(&stray.session) {
                continue;
            }
            let name = self.name(role);
            let said: String = stray.said.lines().next().unwrap_or_default().chars().take(60).collect();
            self.moderator.system(format!(
                "⚠ Another session reported for {name}'s pane: {} (it said: \"{said}\"). Its turns are dropped. \
                 If it is {name}'s new conversation (/new, /clear), type /rebind {}.",
                stray.session, stray.session
            ));
            self.announced_stray[slot(role)].push(stray.session);
        }
    }

    /// `/rebind <id>`: the stray session named becomes the one bound to its pane. Alone,
    /// `/rebind` only lists the candidates, each with what it said first: a stray may be a
    /// `claude -p` run from the pane rather than the agent's new conversation, and binding
    /// that would leave the room deaf, so the user picks by id.
    fn rebind(&mut self, which: &str) {
        let id = self.config.id.clone();
        let candidates: Vec<(Role, state::Stray)> =
            [Role::Host, Role::Reviewer].into_iter().flat_map(|r| state::strays(&id, r).into_iter().map(move |s| (r, s))).collect();
        if which.is_empty() {
            if candidates.is_empty() {
                self.moderator.system(
                    "Nothing to rebind: no other session has reported for either pane. A Codex on Codex's daemon that \
                     started a new thread is noted once that thread reports from this folder; if it never does, open a new room from it with !room.",
                );
            } else {
                let list: Vec<String> = candidates
                    .iter()
                    .map(|(r, s)| format!("/rebind {} ({} · {}: \"{}\")", s.session, self.name(*r), role_word(*r), s.said.lines().next().unwrap_or_default()))
                    .collect();
                self.moderator.system(format!("Sessions that reported without being bound; pick one by id: {}", list.join(" · ")));
            }
            return;
        }
        let pick = match candidates.iter().find(|(_, s)| s.session == which) {
            Some(found) => found.clone(),
            None => {
                self.moderator.system(format!("No stray session {which}; /rebind alone lists them."));
                return;
            }
        };
        let (role, stray) = pick;
        self.bind_session(role, &stray.session);
        // A conversation the room could not follow on its own: it has not heard of the
        // room either (a Codex's `/new` on Codex's daemon, a `claude` restarted).
        self.moderator.followed(role, "/rebind", &stray.session);
    }

    /// Binds the agent in `role` to `session`: the room's record of it (a reopened room maps
    /// the pane by it, `/swap` resumes it, Codex's record is read by it), the pane's map, and
    /// no stray left to offer for that pane.
    fn bind_session(&mut self, role: Role, session: &str) {
        let id = self.config.id.clone();
        self.config.set_session(role, session);
        let _ = self.config.save();
        if let Some(pane) = self.config.pane(role) {
            let started = role == self.config.started_role();
            let _ = state::map_pane(&pane, &id, role, started, Some(session));
        }
        state::clear_strays(&id, role);
        self.announced_stray[slot(role)].clear();
    }

    /// Whether the roles can switch now: nothing in progress for the moderator, every
    /// prompt done with, and both agents up and settled at their prompts.
    fn swap_ready(&self) -> bool {
        self.moderator.can_swap()
            && self.in_flight == [0; 2]
            && self.settled.iter().all(|&n| n >= 2)
            && self.panes.iter().all(|p| p.lock().unwrap().is_some())
    }

    /// `/swap`: switches the roles now if nothing is in progress, or once it is over;
    /// typed again while it waits, cancels it.
    fn request_swap(&mut self) {
        if self.swap_pending {
            self.swap_pending = false;
            self.moderator.system("⇄ Role switch cancelled.");
        } else if self.swap_ready() {
            self.swap();
        } else {
            self.swap_pending = true;
            self.moderator.system(
                "⇄ The roles switch once both agents are done and no message waits to be sent. \
                 Ctrl+E ends a review now; /swap again cancels.",
            );
        }
    }

    /// Switches the roles. When the room is read-only, the agent it started restarts on
    /// its own conversation, so that its permissions follow its new role; the session the
    /// room was opened from is never restarted. What can fail comes first: if it does, the
    /// roles, the pane maps and the permissions all stay as they were.
    fn swap(&mut self) {
        self.swap_pending = false;
        let restart = self.config.reviewer_read_only;
        let who = display_name(&self.config.reviewer_kind);
        if restart && self.started_spoke && self.config.reviewer_session.is_none() {
            self.moderator.system(format!(
                "⚠ Can't switch roles: the room doesn't know {who}'s session, and restarting it would lose its conversation."
            ));
            return;
        }
        let mut next = self.config.clone();
        next.swapped = !next.swapped;
        // Before its first turn there is nothing to keep: it starts afresh.
        let resume = self.started_spoke;
        let launch = match restart.then(|| prepare(&next, resume)) {
            None => None,
            Some(Ok(l)) => Some(l),
            Some(Err(e)) => {
                let role = role_word(next.started_role());
                self.moderator.system(format!(
                    "⚠ Can't switch roles: {who} could not be prepared as {role} ({e:#}). The roles stay as they were."
                ));
                return;
            }
        };
        if let Some(l) = &launch {
            next.reviewer_pane = Some(l.pane.clone());
            next.reviewer_name = Some(l.name.clone());
            next.reviewer_session = l.session.clone();
        }
        let old = self.config.reviewer_pane.clone();
        self.config = next;
        self.moderator.swap_roles();
        let _ = self.config.save();
        {
            let (mut host, mut reviewer) = (self.panes[0].lock().unwrap(), self.panes[1].lock().unwrap());
            std::mem::swap(&mut *host, &mut *reviewer);
        }
        std::mem::swap(&mut self.host_status, &mut self.reviewer_status);
        self.settled = [0; 2];

        let (id, started) = (self.config.id.clone(), self.config.started_role());
        let _ = state::map_pane(&self.config.host_pane, &id, started.other(), false, self.config.host_session.as_deref());
        let (host, reviewer) = (self.name(Role::Host), self.name(Role::Reviewer));
        let mut note = format!("⇄ Roles switched: {host} hosts, {reviewer} reviews. Give {host} requests in its pane or here.");
        if restart && started == Role::Host {
            note.push_str(&format!(
                " {reviewer} reviews in your own session, where the room asks for read-only but cannot enforce it: \
                 it can only sandbox an agent it starts."
            ));
        }
        self.moderator.system(note);

        let pane = self.panes[slot(started)].clone();
        let (Some(l), Some(old)) = (launch, old) else {
            if let Some(p) = &self.config.reviewer_pane {
                let _ = state::map_pane(p, &id, started, true, self.config.reviewer_session.as_deref());
            }
            return;
        };
        let role = role_word(started);
        self.moderator.system(if resume {
            format!("Restarting {who} as {role}; its conversation resumes…")
        } else {
            format!("Starting {who} as {role}…")
        });
        // Its sender waits for the new pane.
        *pane.lock().unwrap() = None;
        match started {
            Role::Host => self.host_status.clear(),
            Role::Reviewer => self.reviewer_status.clear(),
        }
        let (d, tx) = (self.config.clone(), self.tx.clone());
        thread::spawn(move || restart_agent(d, l, pane, old, tx));
    }

    fn draw(&self, f: &mut Frame) {
        let [header, status, body, input, help] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(3),
            Constraint::Length(1),
        ])
        .areas(f.area());

        let e = &self.moderator;
        let (phase_text, phase_color) = match (e.paused, e.phase) {
            (true, _) => ("⏸ paused".to_string(), ui::WARN),
            (_, Phase::WaitingUser) => ("ask your agent something".to_string(), ui::MUTED),
            (_, Phase::HostWorking) if e.host_waiting > 0 => (
                format!("{} is waiting on {} background task(s)…", e.host_name, e.host_waiting),
                ui::agent_color(e.kind(Role::Host)),
            ),
            (_, Phase::HostWorking) => (format!("{} is working…", e.host_name), ui::agent_color(e.kind(Role::Host))),
            (_, Phase::ReviewerWorking) => (format!("{} is reviewing…", e.reviewer_name), ui::agent_color(e.kind(Role::Reviewer))),
            (_, Phase::Agreed) => ("✓ agreed".to_string(), ui::OK),
            (_, Phase::NoConsensus) => ("⚖ your call".to_string(), ui::WARN),
            (_, Phase::Ended) => ("ended".to_string(), ui::MUTED),
        };
        // The topic is the room's label; without one the header shows the current request.
        let topic = if self.config.topic.is_empty() { e.user_prompt.clone() } else { self.config.topic.clone() };
        let topic: String = topic.lines().next().unwrap_or_default().chars().take(60).collect();
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(" ROOM ", Style::new().bold().fg(Color::Black).bg(ui::MUTED)),
                Span::raw(" "),
                Span::styled(topic, Style::new().fg(ui::MUTED)),
                Span::raw("   "),
                Span::styled(format!("Round {}/{}", e.round, e.max_rounds), Style::new().bold()),
                Span::raw("   "),
                Span::styled(phase_text, Style::new().fg(phase_color)),
                Span::styled(if self.swap_pending { "   ⇄ switching roles when done" } else { "" }, Style::new().fg(ui::WARN)),
            ])),
            header,
        );

        let (hs, hc) = ui::status_label(&self.host_status);
        let (rs, rc) = ui::status_label(&self.reviewer_status);
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(" ● ", Style::new().fg(ui::agent_color(e.kind(Role::Host)))),
                Span::raw(format!("{} · host ", e.host_name)),
                Span::styled(format!("@{}  ", e.nick(Role::Host)), Style::new().fg(ui::USER)),
                Span::styled(hs, Style::new().fg(hc)),
                Span::styled("    ● ", Style::new().fg(ui::agent_color(e.kind(Role::Reviewer)))),
                Span::raw(format!("{} · reviewer ", e.reviewer_name)),
                Span::styled(format!("@{}  ", e.nick(Role::Reviewer)), Style::new().fg(ui::USER)),
                Span::styled(rs, Style::new().fg(rc)),
            ])),
            status,
        );

        let lines = self.message_lines();
        let mut block = Block::new().borders(Borders::TOP | Borders::BOTTOM).border_style(Style::new().fg(ui::BORDER));
        let para = Paragraph::new(lines).wrap(Wrap { trim: false });
        let total = para.line_count(body.width) as u16;
        let inner_height = body.height.saturating_sub(2);
        let max_back = total.saturating_sub(inner_height);
        self.scroll_max.set(max_back);
        let back = self.scroll_back.min(max_back);
        if back > 0 {
            block = block.title(Span::styled(format!(" ↑ {back} lines back · End for the latest "), Style::new().fg(ui::WARN)));
        }
        let top = max_back.saturating_sub(back);
        f.render_widget(para.block(block).scroll((top, 0)), body);

        let name = |t: Target| match t {
            Target::Both => "both".to_string(),
            Target::Host => format!("only {}", e.host_name),
            Target::Reviewer => format!("only {}", e.reviewer_name),
        };
        // An @name in the draft overrides the Tab choice for that message.
        let title = match e.mentioned(&self.input) {
            Some(t) => format!(" To: {} (@) ", name(t)),
            None => format!(" To: {} (Tab, or @{} / @{}) ", name(self.target), e.nick(Role::Host), e.nick(Role::Reviewer)),
        };
        let input_block =
            Block::bordered().border_style(Style::new().fg(ui::USER)).title(Span::styled(title, Style::new().fg(ui::USER)));
        let width = input.width.saturating_sub(4).max(1) as usize;
        // A pasted message keeps its line breaks; the one-line box shows them as ↵, and
        // shows the stretch around the cursor.
        let chars: Vec<char> = self.input.chars().map(|c| if c == '\n' { '↵' } else { c }).collect();
        let start = (self.cursor + 1).saturating_sub(width);
        let shown: String = chars.iter().skip(start).take(width).collect();
        f.render_widget(Paragraph::new(format!("> {shown}")).block(input_block), input);
        f.set_cursor_position(Position::new(input.x + 3 + (self.cursor - start) as u16, input.y + 1));
        self.draw_offers(f, input);

        f.render_widget(
            Paragraph::new(Line::styled(
                " Enter send · Tab or @name recipient · /swap roles · /review last answer · /rebind after /new · /exit close all · Ctrl+P pause · Ctrl+E end · wheel/PgUp scroll · Ctrl+Q close room",
                Style::new().fg(ui::MUTED),
            )),
            help,
        );
    }

    /// The completions for the word being typed, in a list just above the input box.
    fn draw_offers(&self, f: &mut Frame, input: Rect) {
        let offers = self.offers();
        if offers.is_empty() {
            return;
        }
        let pick = self.pick.min(offers.len() - 1);
        let rows = offers.len().min(6);
        let first = pick.saturating_sub(rows - 1);
        let word_width = offers.iter().map(|c| c.word.chars().count()).max().unwrap_or(0);
        let about_width = offers.iter().map(|c| c.about.chars().count()).max().unwrap_or(0);
        let width = ((word_width + about_width + 5) as u16 + 2).min(input.width.saturating_sub(2));
        let height = rows as u16 + 2;
        let area = Rect { x: input.x + 2, y: input.y.saturating_sub(height), width, height };
        let lines: Vec<Line> = offers
            .iter()
            .enumerate()
            .skip(first)
            .take(rows)
            .map(|(i, c)| {
                let (word, about) = if i == pick {
                    let on = Style::new().bg(ui::USER).fg(ui::INK);
                    (on.bold(), on)
                } else {
                    (Style::new().fg(ui::USER), Style::new().fg(ui::MUTED))
                };
                let pad = " ".repeat(word_width - c.word.chars().count() + 2);
                // fills the row out to the block's inner width, so the highlight spans it
                let about_pad = " ".repeat((width as usize).saturating_sub(word_width + c.about.chars().count() + 6));
                Line::from(vec![
                    Span::styled(format!(" {}{pad}", c.word), word),
                    Span::styled(format!("{}{about_pad} ", c.about), about),
                ])
            })
            .collect();
        let block = Block::bordered()
            .border_style(Style::new().fg(ui::USER))
            .title(Span::styled(" ↑↓ · Tab ", Style::new().fg(ui::MUTED)))
            .style(Style::new().bg(Color::Reset));
        f.render_widget(Clear, area);
        f.render_widget(Paragraph::new(lines).block(block), area);
    }

    fn message_lines(&self) -> Vec<Line<'static>> {
        let e = &self.moderator;
        let mut lines = vec![];
        for m in &e.log {
            let time = chrono::Local
                .timestamp_millis_opt(m.ts as i64)
                .single()
                .map(|t| t.format("%H:%M").to_string())
                .unwrap_or_default();
            // The agent that had `role` when the message was written: the roles may have switched since.
            let then = |role: Role| if m.swapped == e.swapped { role } else { role.other() };
            let agent = |role: Role| {
                let a = then(role);
                (format!("{} · {} @{}", e.name(a).to_uppercase(), role_word(role), e.nick(a)), ui::agent_color(e.kind(a)))
            };
            let (who, color) = match &m.author {
                Author::User => ("YOU".to_string(), ui::USER),
                Author::UserRoom(Target::Both) => ("YOU → both".to_string(), ui::USER),
                Author::UserRoom(Target::Host) => (format!("YOU → {}", e.name(then(Role::Host))), ui::USER),
                Author::UserRoom(Target::Reviewer) => (format!("YOU → {}", e.name(then(Role::Reviewer))), ui::USER),
                Author::Host => agent(Role::Host),
                Author::Reviewer => agent(Role::Reviewer),
                Author::System => ("•".to_string(), ui::MUTED),
            };
            if m.author == Author::System {
                let color = match m.text.chars().next() {
                    Some('✓') => ui::OK,
                    Some('⚖') | Some('⚠') => ui::WARN,
                    _ => ui::MUTED,
                };
                lines.push(Line::from(vec![
                    Span::styled(format!(" {time}  "), Style::new().fg(ui::MUTED)),
                    Span::styled(m.text.clone(), Style::new().fg(color)),
                ]));
                lines.push(Line::raw(""));
                continue;
            }
            let mut head = vec![
                Span::styled(format!(" {time}  "), Style::new().fg(ui::MUTED)),
                Span::styled(who, Style::new().fg(color).bold()),
            ];
            match m.verdict {
                Some(Verdict::Agree) => head.push(Span::styled("  ✓ AGREES", Style::new().fg(ui::OK).bold())),
                Some(Verdict::Disagree) => head.push(Span::styled("  ✗ DISAGREES", Style::new().fg(ui::ERR).bold())),
                Some(Verdict::Unknown) => head.push(Span::styled("  ? no verdict line", Style::new().fg(ui::WARN))),
                None => {}
            }
            lines.push(Line::from(head));
            for text_line in m.text.trim().lines() {
                lines.push(Line::raw(format!("        {text_line}")));
            }
            lines.push(Line::raw(""));
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Event, EventKind};

    fn home() -> PathBuf {
        std::env::temp_dir().join(format!("herdr-rooms-test-{}", std::process::id()))
    }

    /// A room with its own panes and id, under a scratch state folder. herdr is a stand-in
    /// that writes down each call: it splits panes, except those of a room whose tag starts
    /// with `x`, and fails everything else, so no test reaches a real pane.
    fn config(tag: &str, kind: &str, read_only: bool) -> RoomConfig {
        let home = home();
        std::fs::create_dir_all(&home).unwrap();
        let fake = home.join("herdr");
        if !fake.exists() {
            use std::os::unix::fs::PermissionsExt;
            let draft = home.join(format!("herdr.{tag}"));
            let script = format!(
                "#!/bin/sh\necho \"$*\" >> '{}'\ncase \"$1 $2 $3\" in\n  \"pane split x\"*) exit 1 ;;\n  \"pane split \"*) printf '{{\"result\":{{\"pane\":{{\"pane_id\":\"%s-new\"}}}}}}' \"$3\"; exit 0 ;;\nesac\nexit 1\n",
                home.join("herdr-calls").display()
            );
            std::fs::write(&draft, script).unwrap();
            std::fs::set_permissions(&draft, std::fs::Permissions::from_mode(0o755)).unwrap();
            std::fs::rename(&draft, &fake).unwrap();
        }
        std::env::set_var("HERDR_ROOMS_HOME", &home);
        std::env::set_var("HERDR_BIN_PATH", &fake);
        RoomConfig {
            id: format!("1792870204{tag}"),
            cwd: "/tmp".into(),
            topic: String::new(),
            host_pane: format!("{tag}:p1"),
            host_kind: "claude".into(),
            reviewer_kind: kind.into(),
            reviewer_model: None,
            reviewer_effort: None,
            reviewer_read_only: read_only,
            max_rounds: 3,
            reviewer_pane: Some(format!("{tag}:p2")),
            reviewer_name: None,
            host_nick: None,
            reviewer_nick: None,
            swapped: false,
            reviewer_session: None,
            host_session: None,
            room_pid: None,
        }
    }

    fn has(args: &[String], run: &[&str]) -> bool {
        args.windows(run.len()).any(|w| w.iter().zip(run).all(|(a, b)| a == b))
    }

    #[test]
    fn a_codex_agent_is_read_only_while_it_reviews() {
        let mut d = config("ca", "codex", true);
        let args = agent_args(&d, None, false).unwrap();
        assert!(has(&args, &["-s", "read-only", "-a", "never"]));
        assert!(args.iter().any(|a| a.starts_with("developer_instructions=") && a.contains("(room) Role:")));
        // as host: the sandbox of a reviewer that may write, and the same conversation
        d.swapped = true;
        let args = agent_args(&d, Some("01a17dab"), true).unwrap();
        assert_eq!(args[..2], ["resume", "01a17dab"]);
        assert!(has(&args, &["-s", "workspace-write"]));
        assert!(!args.iter().any(|a| a == "-a" || a == "read-only"));
        assert!(args.iter().any(|a| a.starts_with("notify=")));
        // a reviewer again: read-only again
        d.swapped = false;
        let args = agent_args(&d, Some("01a17dab"), true).unwrap();
        assert!(args[..2] == ["resume", "01a17dab"] && has(&args, &["-s", "read-only", "-a", "never"]));
    }

    #[test]
    fn a_claude_agent_is_read_only_while_it_reviews() {
        let mut d = config("cl", "claude", true);
        let settings = |args: &[String]| -> serde_json::Value {
            let at = args.iter().position(|a| a == "--settings").unwrap();
            serde_json::from_str(&std::fs::read_to_string(&args[at + 1]).unwrap()).unwrap()
        };
        let id = new_uuid();
        let args = agent_args(&d, Some(&id), false).unwrap();
        assert!(has(&args, &["--session-id", &id]));
        assert!(has(&args, &["--disallowedTools", "Edit,Write,NotebookEdit"]) && has(&args, &["--strict-mcp-config"]));
        assert_eq!(settings(&args)["sandbox"]["allowUnsandboxedCommands"], false);
        assert!(has(&args, &["--append-system-prompt-file"]));
        // as host: your own permissions and MCP servers, the same conversation, the role-aware rule
        d.swapped = true;
        let args = agent_args(&d, Some(&id), true).unwrap();
        assert!(has(&args, &["--resume", &id]) && !has(&args, &["--session-id"]));
        assert!(!has(&args, &["--disallowedTools"]) && !has(&args, &["--strict-mcp-config"]));
        let s = settings(&args);
        assert!(s.get("sandbox").is_none() && s["hooks"]["Stop"].is_array());
        assert!(has(&args, &["--append-system-prompt-file"]));
        let rules = std::fs::read_to_string(state::room_dir(&d.id).join("agent-rules.md")).unwrap();
        assert!(rules.contains("(room) Role:"));
    }

    #[test]
    fn a_room_that_may_write_gives_both_roles_the_same_arguments() {
        for (tag, kind) in [("wc", "codex"), ("wl", "claude")] {
            let mut d = config(tag, kind, false);
            let reviewing = agent_args(&d, Some("s"), true).unwrap();
            d.swapped = true;
            assert_eq!(agent_args(&d, Some("s"), true).unwrap(), reviewing);
        }
    }

    #[test]
    fn session_ids_are_v4_uuids() {
        let (a, b) = (new_uuid(), new_uuid());
        assert_ne!(a, b);
        let parts: Vec<usize> = a.split('-').map(str::len).collect();
        assert_eq!(parts, [8, 4, 4, 4, 12]);
        assert!(a.chars().all(|c| c == '-' || c.is_ascii_hexdigit()));
        assert_eq!(&a[14..15], "4");
        assert!(matches!(&a[19..20], "8" | "9" | "a" | "b"));
    }

    fn room(d: RoomConfig) -> (Room, [Receiver<String>; 2]) {
        let (tx, rx) = mpsc::channel();
        let ((host_q, host_rx), (reviewer_q, reviewer_rx)) = (mpsc::channel(), mpsc::channel());
        let panes = [Role::Host, Role::Reviewer].map(|role| Arc::new(Mutex::new(d.pane(role))));
        (Room::new(d, panes, host_q, reviewer_q, Default::default(), tx, rx), [host_rx, reviewer_rx])
    }

    /// Both agents show at their prompts twice, as the status poller reports them.
    fn settle(room: &mut Room) {
        for _ in 0..2 {
            for role in [Role::Host, Role::Reviewer] {
                room.tx.send(Worker::Status(role, "idle".into())).unwrap();
            }
        }
        room.pump();
    }

    /// Pumps the room until its log has `text`, as a restart reports from its thread.
    fn logged(room: &mut Room, text: &str) -> bool {
        for _ in 0..60 {
            room.pump();
            if room.moderator.log.iter().any(|m| m.text.contains(text)) {
                return true;
            }
            thread::sleep(Duration::from_millis(50));
        }
        false
    }

    fn pane(room: &Room, role: Role) -> Option<String> {
        room.panes[slot(role)].lock().unwrap().clone()
    }

    #[test]
    fn reopening_the_room_restores_maps_for_existing_agents() {
        let mut d = config("reopen", "codex", true);
        d.host_session = Some("claude-own".into());
        d.reviewer_session = Some("01a17dab".into());
        for swapped in [false, true] {
            d.swapped = swapped;
            d.map_panes().unwrap();
            let host = state::lookup_pane(&d.host_pane).unwrap();
            let started = state::lookup_pane(d.reviewer_pane.as_ref().unwrap()).unwrap();
            assert_eq!((host.id.as_str(), host.role, host.started), (d.id.as_str(), d.started_role().other(), false));
            assert_eq!((started.id.as_str(), started.role, started.started), (d.id.as_str(), d.started_role(), true));
            // each pane is its own agent's session, whatever the roles
            assert_eq!((host.session.as_deref(), started.session.as_deref()), (Some("claude-own"), Some("01a17dab")));
            state::unmap_pane(&d.host_pane, &d.id);
            state::unmap_pane(d.reviewer_pane.as_ref().unwrap(), &d.id);
        }
    }

    #[test]
    fn a_turn_nobody_reported_is_announced_and_frees_the_agent() {
        let (mut room, _queues) = room(config("lost", "codex", true));
        room.config.reviewer_session = Some("t-lost".into());
        // Codex's own record says what ended the turn
        // one CODEX_HOME for every test of this process: the variable is process-wide
        let codex_home = home().join("codex");
        let dir = codex_home.join("sessions/2026/10/29");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("rollout-2026-10-29T10-00-00-t-lost.jsonl"),
            "{\"type\":\"session_meta\",\"payload\":{\"id\":\"t-lost\",\"thread_source\":\"user\"}}\n\
             {\"type\":\"event_msg\",\"payload\":{\"type\":\"task_complete\",\"last_agent_message\":null,\"error\":{\"message\":\"You’ve hit your usage limit.\"}}}\n",
        )
        .unwrap();
        std::env::set_var("CODEX_HOME", &codex_home);
        // a request is answered and its review delivered; the reviewer works, then goes idle
        let turn = |kind, text: &str| Event {
            ts: state::now_ms(),
            role: Role::Host,
            kind,
            text: text.into(),
            background: 0,
            session: Some("claude-own".into()),
            turn: Some("u1".into()),
        };
        state::append_event(&room.config.id, &turn(EventKind::User, "fix avg")).unwrap();
        state::append_event(&room.config.id, &turn(EventKind::Assistant, "fixed avg")).unwrap();
        room.pump();
        assert_eq!(room.moderator.phase, Phase::ReviewerWorking);
        // the reviewer runs a `!` command of its own before the review reaches it: no report
        // is expected for that
        for status in ["working", "idle"] {
            room.tx.send(Worker::Status(Role::Reviewer, status.into())).unwrap();
        }
        room.pump();
        assert!(room.awaiting_report[1].is_none(), "nothing was delivered: no report expected");
        // the review is typed in; the turn that ends next must be reported, even one so
        // short (a usage-limit error) that it is over before `agent prompt --wait` returns
        room.tx.send(Worker::Typing(Role::Reviewer)).unwrap();
        for status in ["working", "idle"] {
            room.tx.send(Worker::Status(Role::Reviewer, status.into())).unwrap();
        }
        room.tx.send(Worker::Sent(Role::Reviewer)).unwrap();
        room.pump();
        assert!(room.awaiting_report[1].is_some(), "a turn ended: its report is awaited");
        // the report never comes
        room.awaiting_report[1] = Some(Instant::now() - REPORT_TIMEOUT - Duration::from_secs(1));
        room.pump();
        let notice = room.moderator.log.last().unwrap().text.clone();
        assert!(notice.contains("no hook reported it") && notice.contains("Codex says: You’ve hit your usage limit."), "{notice}");
        assert!(notice.contains("The review was waiting on that turn"));
        assert!(room.awaiting_report[1].is_none() && !room.moderator.busy(Role::Reviewer));
        // a prompt typed into a Claude Code pane opens a turn the room saw begin: its end is
        // expected to be reported, and a report that comes in time is not announced
        state::append_event(&room.config.id, &turn(EventKind::User, "and the median")).unwrap();
        room.pump();
        for status in ["working", "idle"] {
            room.tx.send(Worker::Status(Role::Host, status.into())).unwrap();
        }
        room.pump();
        assert!(room.awaiting_report[0].is_some());
        state::append_event(&room.config.id, &turn(EventKind::Assistant, "added median")).unwrap();
        room.pump();
        assert!(room.awaiting_report[0].is_none());
        room.close();
    }

    #[test]
    fn the_input_box_edits_at_the_cursor() {
        let (mut room, _queues) = room(config("cursor", "codex", true));
        let none = KeyModifiers::NONE;
        for c in "ab cd".chars() {
            room.key(KeyCode::Char(c), none);
        }
        assert_eq!((room.input.as_str(), room.cursor), ("ab cd", 5));
        room.key(KeyCode::Left, none);
        room.key(KeyCode::Left, none);
        room.key(KeyCode::Char('x'), none);
        assert_eq!((room.input.as_str(), room.cursor), ("ab xcd", 4));
        room.key(KeyCode::Backspace, none);
        assert_eq!((room.input.as_str(), room.cursor), ("ab cd", 3));
        room.key(KeyCode::Delete, none);
        assert_eq!((room.input.as_str(), room.cursor), ("ab d", 3));
        room.key(KeyCode::Home, none);
        room.key(KeyCode::Char('é'), none);
        assert_eq!((room.input.as_str(), room.cursor), ("éab d", 1));
        room.key(KeyCode::Right, KeyModifiers::ALT);
        assert_eq!(room.cursor, 3);
        room.key(KeyCode::Left, KeyModifiers::ALT);
        assert_eq!(room.cursor, 0);
        room.key(KeyCode::End, none);
        assert_eq!(room.cursor, 5);
        room.insert("\nmore");
        assert_eq!((room.input.as_str(), room.cursor), ("éab d\nmore", 10));
        room.key(KeyCode::Esc, none);
        assert_eq!((room.input.as_str(), room.cursor), ("", 0));
        // a completion replaces the word before the cursor
        for c in "hi @ho tail".chars() {
            room.key(KeyCode::Char(c), none);
        }
        for _ in 0..5 {
            room.key(KeyCode::Left, none);
        }
        let offers = room.offers();
        assert_eq!(offers[0].word, "@host");
        room.complete(&offers[0]);
        assert_eq!((room.input.as_str(), room.cursor), ("hi @host  tail", 9));
    }

    #[test]
    fn the_log_scrolls_with_the_wheel_and_keys() {
        let (mut room, _queues) = room(config("scroll", "codex", true));
        room.scroll_max.set(50);
        room.wheel(MouseEventKind::ScrollUp);
        assert_eq!(room.scroll_back, 3);
        room.key(KeyCode::Up, KeyModifiers::SHIFT);
        assert_eq!(room.scroll_back, 4);
        room.key(KeyCode::Home, KeyModifiers::NONE);
        assert_eq!(room.scroll_back, 50);
        room.key(KeyCode::PageUp, KeyModifiers::NONE);
        assert_eq!(room.scroll_back, 50, "no further than the log's top");
        room.wheel(MouseEventKind::ScrollDown);
        assert_eq!(room.scroll_back, 47);
        room.key(KeyCode::End, KeyModifiers::NONE);
        assert_eq!(room.scroll_back, 0);
        // with text in the box, Home and End move the cursor instead
        room.key(KeyCode::Char('x'), KeyModifiers::NONE);
        room.key(KeyCode::Home, KeyModifiers::NONE);
        assert_eq!((room.scroll_back, room.cursor), (0, 0));
    }

    #[test]
    fn a_codex_compaction_is_read_from_its_record() {
        let (mut room, _queues) = room(config("compact", "codex", true));
        room.config.reviewer_session = Some("t-compact".into());
        let codex_home = home().join("codex");
        let dir = codex_home.join("sessions/2026/10/29");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rollout-2026-10-29T10-00-00-t-compact.jsonl");
        // Codex 0.165 writes its records as {"timestamp":…,"ordinal":…,"type":…,"payload":…}
        let record = |ordinal: u32, kind: &str, payload: &str| {
            format!("{{\"timestamp\":\"2026-10-29T10:37:14.873Z\",\"ordinal\":{ordinal},\"type\":\"{kind}\",\"payload\":{payload}}}")
        };
        let meta = record(0, "session_meta", "{\"id\":\"t-compact\",\"thread_source\":\"user\"}");
        let old = record(1, "compacted", "{\"message\":\"old\"}");
        std::fs::write(&path, format!("{meta}\n{old}\n")).unwrap();
        std::env::set_var("CODEX_HOME", &codex_home);
        // the first look only notes where the record ends: an old compaction is not news
        room.tx.send(Worker::Status(Role::Reviewer, "idle".into())).unwrap();
        room.pump();
        assert!(room.moderator.outbox.is_empty());
        // a compaction now, written in two parts: nothing until the line is whole
        let mut file = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        use std::io::Write;
        let line = record(1068, "compacted", "{\"message\":\"\"}");
        let (head, rest) = line.split_at(40);
        write!(file, "{head}").unwrap();
        file.flush().unwrap();
        room.tx.send(Worker::Status(Role::Reviewer, "idle".into())).unwrap();
        room.pump();
        assert!(queues_text(&_queues[1]).is_empty(), "a half-written record is not read");
        writeln!(file, "{rest}").unwrap();
        room.tx.send(Worker::Status(Role::Reviewer, "idle".into())).unwrap();
        room.pump();
        room.pump();
        let sent = queues_text(&_queues[1]);
        assert!(sent.iter().any(|t| t.starts_with("(room) Role: reviewer.") && t.contains("reminder")), "{sent:?}");
        // and once only
        room.tx.send(Worker::Status(Role::Reviewer, "idle".into())).unwrap();
        room.pump();
        assert!(queues_text(&_queues[1]).is_empty());
        room.close();
    }

    fn queues_text(q: &Receiver<String>) -> Vec<String> {
        q.try_iter().collect()
    }

    #[test]
    fn a_turn_reported_lost_does_not_arm_the_watchdog_forever() {
        let (mut room, _queues) = room(config("lostonce", "codex", true));
        settle(&mut room);
        // the user types into the Claude host's pane and presses Esc: no Stop comes
        let ev = Event {
            ts: state::now_ms(),
            role: Role::Host,
            kind: EventKind::User,
            text: "fix avg".into(),
            background: 0,
            session: Some("claude-own".into()),
            turn: Some("u1".into()),
        };
        state::append_event(&room.config.id, &ev).unwrap();
        room.pump();
        for status in ["working", "idle"] {
            room.tx.send(Worker::Status(Role::Host, status.into())).unwrap();
        }
        room.pump();
        room.awaiting_report[0] = Some(Instant::now() - REPORT_TIMEOUT - Duration::from_secs(1));
        room.pump();
        assert!(room.moderator.log.last().unwrap().text.contains("no hook reported it"));
        // later the user runs a `!` command in the host's pane: no turn, no hook, no alarm
        for status in ["working", "idle"] {
            room.tx.send(Worker::Status(Role::Host, status.into())).unwrap();
        }
        room.pump();
        assert!(room.awaiting_report[0].is_none(), "every later `!` command or /compact is announced as a lost turn");
        // the lost prompt stays for attribution: a message typed later still joins it
        assert!(!room.moderator.has_open_turn(Role::Host));
        room.close();
    }

    #[test]
    fn stray_sessions_are_announced_once_each_and_rebound_by_id() {
        let (mut room, _queues) = room(config("stray", "codex", true));
        room.config.reviewer_session = Some("01a17dab".into());
        let notices = |room: &Room, text: &str| room.moderator.log.iter().filter(|m| m.text.contains(text)).count();
        // the hook saw another thread report for Codex's pane: after /new, say
        state::note_stray(&room.config.id, Role::Reviewer, "01a18fff", "now the median");
        room.tx.send(Worker::Status(Role::Reviewer, "idle".into())).unwrap();
        room.tx.send(Worker::Status(Role::Reviewer, "idle".into())).unwrap();
        room.pump();
        assert_eq!(notices(&room, "01a18fff (it said: \"now the median\")"), 1, "the room says so once");
        // a `codex exec` run from the pane reports too: a second candidate, told apart by what it said
        state::note_stray(&room.config.id, Role::Reviewer, "01a19000", "list the files");
        room.tx.send(Worker::Status(Role::Reviewer, "idle".into())).unwrap();
        room.pump();
        assert_eq!((notices(&room, "01a18fff (it said"), notices(&room, "01a19000 (it said")), (1, 1));
        // /rebind alone lists the candidates and binds nothing
        room.rebind("");
        assert!(room.moderator.log.last().unwrap().text.contains("pick one by id: /rebind 01a18fff"));
        assert_eq!(room.config.reviewer_session.as_deref(), Some("01a17dab"));
        room.rebind("01a1zzzz");
        assert!(room.moderator.log.last().unwrap().text.contains("No stray session 01a1zzzz"));
        room.rebind("01a18fff");
        assert_eq!(room.config.reviewer_session.as_deref(), Some("01a18fff"));
        assert_eq!(RoomConfig::load(&room.config.id).unwrap().reviewer_session.as_deref(), Some("01a18fff"));
        assert_eq!(state::lookup_pane("stray:p2").unwrap().session.as_deref(), Some("01a18fff"));
        assert!(state::strays(&room.config.id, Role::Reviewer).is_empty());
        assert!(room.moderator.log.last().unwrap().text.contains("follows it to session 01a18fff"));
        // the new conversation has not heard of the room: it is told its role
        room.pump();
        let sent = queues_text(&_queues[1]);
        assert!(sent.iter().any(|t| t.starts_with("(room) Role: reviewer.") && t.contains("You are on a new conversation")), "{sent:?}");
        room.rebind("");
        assert!(room.moderator.log.last().unwrap().text.contains("Nothing to rebind"));
        // even a single candidate is only listed, never bound without its id: it may be a probe
        state::note_stray(&room.config.id, Role::Host, "claude-new", "hi again");
        room.rebind("");
        assert_eq!(room.config.host_session, None);
        assert!(room.moderator.log.last().unwrap().text.contains("pick one by id: /rebind claude-new"));
        room.rebind("claude-new");
        assert_eq!(room.config.host_session.as_deref(), Some("claude-new"));
        room.close();
    }

    #[test]
    fn the_room_follows_an_agent_to_its_new_conversation() {
        let (mut room, queues) = room(config("follow", "claude", true));
        room.config.reviewer_session = Some("c-old".into());
        state::note_stray(&room.config.id, Role::Reviewer, "c-probe", "list the files");
        // a request is answered and its review delivered: the review waits on the reviewer
        let turn = |kind, text: &str| Event {
            ts: state::now_ms(),
            role: Role::Host,
            kind,
            text: text.into(),
            background: 0,
            session: Some("claude-own".into()),
            turn: Some("u1".into()),
        };
        state::append_event(&room.config.id, &turn(EventKind::User, "fix avg")).unwrap();
        state::append_event(&room.config.id, &turn(EventKind::Assistant, "fixed avg")).unwrap();
        room.pump();
        assert_eq!(room.moderator.phase, Phase::ReviewerWorking);
        // the user types /clear into the reviewer's pane; its hooks report the new
        // conversation, the second copy well after the first
        let rebound = Event {
            ts: state::now_ms(),
            role: Role::Reviewer,
            kind: EventKind::Rebound,
            text: "/clear".into(),
            background: 0,
            session: Some("c-new".into()),
            turn: None,
        };
        state::append_event(&room.config.id, &rebound).unwrap();
        state::append_event(&room.config.id, &Event { ts: rebound.ts + 20_000, ..rebound.clone() }).unwrap();
        room.pump();
        assert_eq!(room.config.reviewer_session.as_deref(), Some("c-new"));
        assert_eq!(RoomConfig::load(&room.config.id).unwrap().reviewer_session.as_deref(), Some("c-new"));
        assert_eq!(state::lookup_pane("follow:p2").unwrap().session.as_deref(), Some("c-new"));
        assert!(state::strays(&room.config.id, Role::Reviewer).is_empty(), "nothing is left for /rebind");
        let notes: Vec<&str> =
            room.moderator.log.iter().map(|m| m.text.as_str()).filter(|t| t.contains("is on another conversation")).collect();
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].contains("(/clear)") && notes[0].contains("c-new") && notes[0].contains("The review was waiting on the old one"));
        assert!(!room.moderator.has_open_turn(Role::Reviewer), "the old conversation answers nothing any more");
        // the new conversation hears its role
        room.pump();
        let sent = queues_text(&queues[1]);
        assert!(sent.iter().any(|t| t.starts_with("(room) Role: reviewer.") && t.contains("You are on a new conversation")), "{sent:?}");
        room.close();
    }

    #[test]
    fn a_switch_waits_for_both_agents_and_their_prompts() {
        let (mut room, queues) = room(config("ws", "codex", false));
        room.request_swap();
        assert!(room.swap_pending && !room.config.swapped, "the agents have not shown they are idle");
        // a message on its way to the reviewer holds the switch
        room.moderator.on_room_input(Target::Reviewer, "one more thing");
        room.pump();
        settle(&mut room);
        assert!(!room.config.swapped);
        room.tx.send(Worker::Sent(Role::Reviewer)).unwrap();
        room.pump();
        assert!(room.config.swapped && !room.swap_pending);
        assert_eq!((pane(&room, Role::Host), pane(&room, Role::Reviewer)), (Some("ws:p2".into()), Some("ws:p1".into())));
        assert_eq!((room.name(Role::Host), room.name(Role::Reviewer)), ("Codex".into(), "Claude".into()));
        // the hooks report each agent under its new role from its next turn
        let map = |p: &str| state::lookup_pane(p).map(|m| (m.role, m.started));
        assert_eq!(map("ws:p2"), Some((Role::Host, true)));
        assert_eq!(map("ws:p1"), Some((Role::Reviewer, false)));
        // a room that may write restarts nobody; each agent hears its role
        room.pump();
        assert!(queues[0].try_iter().any(|t| t.starts_with("(room) Role: host.")));
        assert!(queues[1].try_iter().any(|t| t.starts_with("(room) Role: reviewer.")));
        assert!(!room.moderator.log.iter().any(|m| m.text.contains("Restarting") || m.text.contains("Starting")));
        // and the switch back waits again until both are settled
        room.request_swap();
        assert!(room.swap_pending);
        room.request_swap();
        assert!(!room.swap_pending && room.config.swapped, "typed again, /swap cancels");
        room.close();
    }

    #[test]
    fn a_switch_that_would_lose_a_conversation_is_refused() {
        let (mut room, queues) = room(config("rf", "codex", true));
        // Codex has had turns, but no turn told the room its session
        assert!(room.started_spoke && room.config.reviewer_session.is_none());
        settle(&mut room);
        room.request_swap();
        assert!(room.moderator.log.last().unwrap().text.starts_with("⚠ Can't switch roles"));
        assert!(!room.config.swapped && !room.swap_pending);
        assert_eq!((room.name(Role::Host), room.name(Role::Reviewer)), ("Claude".into(), "Codex".into()));
        assert_eq!(pane(&room, Role::Reviewer), Some("rf:p2".into()));
        room.pump();
        assert!(queues.iter().all(|q| q.try_recv().is_err()), "nothing is sent to either agent");
    }

    /// The herdr calls the tests made that name `pane`.
    fn calls_on(pane: &str) -> Vec<String> {
        let calls = std::fs::read_to_string(home().join("herdr-calls")).unwrap_or_default();
        calls.lines().filter(|l| l.split_whitespace().any(|w| w == pane)).map(String::from).collect()
    }

    #[test]
    fn an_agent_that_never_spoke_restarts_afresh() {
        let (mut room, _queues) = room(config("fr", "codex", true));
        room.started_spoke = false;
        settle(&mut room);
        room.request_swap();
        assert!(room.config.swapped);
        assert!(logged(&mut room, "Starting Codex as host…"));
        assert!(logged(&mut room, "Could not start Codex"));
        let start = calls_on("fr:p1-new").into_iter().find(|c| c.starts_with("agent start")).unwrap();
        assert!(!start.contains(" resume ") && start.contains("-s workspace-write"));
        assert!(calls_on("fr:p2").iter().any(|c| c.starts_with("pane close")));
    }

    /// A room whose split fails, its agents mapped: the room's own `started_role` decides
    /// which agent hosts.
    fn failing_room(tag: &str, swapped: bool) -> (Room, [Receiver<String>; 2]) {
        let mut d = config(tag, "codex", true);
        d.swapped = swapped;
        d.reviewer_session = Some("01a17dab".into());
        let started = d.started_role();
        state::map_pane(&d.host_pane, &d.id, started.other(), false, None).unwrap();
        state::map_pane(d.reviewer_pane.as_deref().unwrap(), &d.id, started, true, d.reviewer_session.as_deref()).unwrap();
        let (mut room, queues) = room(d);
        settle(&mut room);
        (room, queues)
    }

    /// Everything a failed switch must leave as it was: the roles, the maps the hooks read,
    /// the panes the senders type into, and the agents, none of them closed or messaged.
    fn assert_unswitched(room: &mut Room, queues: &[Receiver<String>; 2], swapped: bool, codex: Role) {
        let tag = room.config.host_pane.split(':').next().unwrap().to_string();
        let (claude_pane, codex_pane) = (format!("{tag}:p1"), format!("{tag}:p2"));
        assert!(room.moderator.log.last().unwrap().text.starts_with("⚠ Can't switch roles: Codex could not be prepared"));
        assert!(room.moderator.log.last().unwrap().text.ends_with("The roles stay as they were."));
        assert_eq!((room.config.swapped, RoomConfig::load(&room.config.id).map(|c| c.swapped).ok()), (swapped, None));
        assert_eq!(room.name(codex), "Codex");
        assert_eq!(pane(room, codex), Some(codex_pane.clone()));
        assert_eq!(pane(room, codex.other()), Some(claude_pane.clone()));
        assert_eq!(state::lookup_pane(&codex_pane).map(|m| (m.role, m.started)), Some((codex, true)));
        assert_eq!(state::lookup_pane(&claude_pane).map(|m| (m.role, m.started)), Some((codex.other(), false)));
        assert!(!calls_on(&codex_pane).iter().any(|c| c.starts_with("pane close")));
        room.pump();
        assert!(queues.iter().all(|q| q.try_recv().is_err()) && room.in_flight == [0; 2]);
        assert!(room.swap_ready(), "and /swap can be tried again");
    }

    #[test]
    fn a_switch_to_host_that_cannot_open_a_pane_changes_nothing() {
        let (mut room, queues) = failing_room("xa", false);
        room.request_swap();
        assert!(calls_on("xa:p1").iter().any(|c| c.starts_with("pane split xa:p1")));
        assert_unswitched(&mut room, &queues, false, Role::Reviewer);
    }

    #[test]
    fn a_switch_back_to_reviewer_that_cannot_open_a_pane_changes_nothing() {
        // Codex hosts and may write; were it to review on in its pane, it could still write
        let (mut room, queues) = failing_room("xb", true);
        assert_eq!(room.name(Role::Host), "Codex");
        room.request_swap();
        assert!(calls_on("xb:p1").iter().any(|c| c.starts_with("pane split xb:p1")));
        assert_unswitched(&mut room, &queues, true, Role::Host);
    }

    #[test]
    fn a_private_turn_is_a_turn_of_the_started_agent() {
        let (mut room, _queues) = room(config("pt", "claude", true));
        room.started_spoke = false;
        room.config.reviewer_session = Some("claude-started".into());
        let private = |kind, session: &str| Event {
            ts: state::now_ms(),
            role: Role::Reviewer,
            kind,
            text: "read the codebase first".into(),
            background: 0,
            session: Some(session.into()),
            turn: Some("p1".into()),
        };
        // another session in the pane (a `claude -p` probe) is no conversation of the agent's
        for kind in [EventKind::User, EventKind::Assistant] {
            state::append_event(&room.config.id, &private(kind, "probe")).unwrap();
        }
        room.pump();
        assert!(!room.started_spoke);
        // the user talks to the room's Claude in its pane before any request: a restart must keep it
        for kind in [EventKind::User, EventKind::Assistant] {
            state::append_event(&room.config.id, &private(kind, "claude-started")).unwrap();
        }
        room.pump();
        assert!(room.started_spoke, "a /swap now would restart Claude on a new session and lose this conversation");
    }

    #[test]
    fn an_agent_that_spoke_resumes_its_session() {
        let (mut room, _queues) = room(config("rs", "codex", true));
        // Codex's echo of a prompt the room typed names its thread
        let turn = Event {
            ts: state::now_ms(),
            role: Role::Reviewer,
            kind: EventKind::User,
            text: "[Claude · host]: looks fine\n\n---\n(review room · round 1/3 · #r1) You are the reviewer".into(),
            background: 0,
            session: Some("01a17dab-133e".into()),
            turn: None,
        };
        state::append_event(&room.config.id, &turn).unwrap();
        room.pump();
        assert_eq!(room.config.reviewer_session.as_deref(), Some("01a17dab-133e"));
        // a turn of the session the room was opened from names nothing of Codex's
        state::append_event(&room.config.id, &Event { role: Role::Host, session: Some("claude-own".into()), ..turn.clone() }).unwrap();
        // nor does a Codex internal turn in a thread of its own (a title, a catch-up)
        let internal = Event {
            kind: EventKind::Assistant,
            text: "{\"summary\":\"...\"}".into(),
            session: Some("01a17dff-catch-up".into()),
            ..turn.clone()
        };
        state::append_event(&room.config.id, &internal).unwrap();
        room.pump();
        assert_eq!(room.config.reviewer_session.as_deref(), Some("01a17dab-133e"));
        settle(&mut room);
        room.request_swap();
        assert!(room.config.swapped);
        assert!(logged(&mut room, "Restarting Codex as host; its conversation resumes…"));
        let saved = RoomConfig::load(&room.config.id).unwrap();
        assert!(saved.swapped && saved.reviewer_session.as_deref() == Some("01a17dab-133e"));
        assert_eq!(saved.reviewer_pane.as_deref(), Some("rs:p1-new"));
        // the old pane closes, and Codex starts in the new one on its thread, able to write
        assert!(logged(&mut room, "Could not start Codex"));
        assert!(calls_on("rs:p2").iter().any(|c| c.starts_with("pane close")));
        let start = calls_on("rs:p1-new").into_iter().find(|c| c.starts_with("agent start")).unwrap();
        assert!(start.contains("-- resume 01a17dab-133e") && start.contains("-s workspace-write"));
        assert_eq!(state::lookup_pane("rs:p1-new").map(|m| (m.role, m.started)), Some((Role::Host, true)));
        assert!(state::lookup_pane("rs:p2").is_none());
    }
}
