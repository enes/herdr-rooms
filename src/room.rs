//! The room pane: starts the reviewer, relays messages between the two agents
//! and shows the conversation with who said what.

use crate::review::{display_name, Author, Completion, Moderator, Phase, Target, Verdict, EXIT_COMMAND};
use crate::state::{self, RoomConfig, EventReader, Role};
use crate::{herdr, ui};
use anyhow::{Context, Result};
use chrono::TimeZone;
use ratatui::crossterm::event::{self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::crossterm::execute;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

type SharedPane = Arc<Mutex<Option<String>>>;

enum Worker {
    Status(Role, String),
    /// A prompt, numbered per agent, is about to be typed into this agent's pane.
    Delivering(Role, u64),
    Sent,
    Blocked(Role),
    Info(String),
    Warn(String),
}

pub fn run() -> Result<()> {
    let id = std::env::var("ROOM_ID").context("ROOM_ID is unset: /room opens the room")?;
    let config = RoomConfig::load(&id)?;
    let (tx, rx) = mpsc::channel();
    let host_pane: SharedPane = Arc::new(Mutex::new(Some(config.host_pane.clone())));
    let reviewer_pane: SharedPane = Arc::new(Mutex::new(config.reviewer_pane.clone()));
    let host_q = spawn_sender(Role::Host, host_pane.clone(), tx.clone());
    let reviewer_q = spawn_sender(Role::Reviewer, reviewer_pane.clone(), tx.clone());
    spawn_status_poller(host_pane, reviewer_pane.clone(), tx.clone());
    if config.reviewer_pane.is_none() {
        let (d, rp, tx) = (config.clone(), reviewer_pane.clone(), tx.clone());
        thread::spawn(move || start_reviewer(d, rp, tx));
    }

    let mut room = Room::new(config, reviewer_pane, host_q, reviewer_q, rx);
    let mut terminal = ratatui::init();
    // Without it a pasted line break arrives as Enter, and each line went out as its own message.
    let _ = execute!(std::io::stdout(), EnableBracketedPaste);
    let result = room.run_loop(&mut terminal);
    let _ = execute!(std::io::stdout(), DisableBracketedPaste);
    ratatui::restore();
    room.close();
    if room.exit_all {
        // `/exit`: the reviewer's pane goes, then this one (herdr would close it as we exit anyway).
        if let Some(p) = room.reviewer_pane.lock().unwrap().clone() {
            let _ = herdr::close_pane(&p);
        }
        if let Ok(own) = std::env::var("HERDR_PANE_ID") {
            let _ = herdr::close_pane(&own);
        }
    }
    result
}

// ------------------------------------------------------------ background work

/// Splits the host pane to the right and starts the reviewer agent there.
fn start_reviewer(mut d: RoomConfig, pane: SharedPane, tx: Sender<Worker>) {
    let reviewer = display_name(&d.reviewer_kind);
    let _ = tx.send(Worker::Info(format!("Starting {reviewer} as reviewer…")));
    let new_pane = match herdr::pane_split(&d.host_pane, "right", &d.cwd) {
        Ok(p) => p,
        Err(e) => {
            let _ = tx.send(Worker::Warn(format!("Could not open the reviewer pane: {e}")));
            return;
        }
    };
    let name = format!("{}-reviewer-{}", d.reviewer_kind, &d.id[d.id.len().saturating_sub(4)..]);
    d.reviewer_pane = Some(new_pane.clone());
    d.reviewer_name = Some(name.clone());
    let _ = d.save();
    let _ = state::map_pane(&new_pane, &d.id, Role::Reviewer);
    *pane.lock().unwrap() = Some(new_pane.clone());

    let args = match reviewer_args(&d) {
        Ok(args) => args,
        Err(e) => {
            let _ = tx.send(Worker::Warn(format!("Could not prepare {reviewer}: {e}")));
            return;
        }
    };
    match herdr::agent_start(&name, &d.reviewer_kind, &new_pane, &args) {
        Ok(()) => {
            let _ = tx.send(Worker::Info(format!("{reviewer} is ready ({}).", protection(&d))));
        }
        Err(e) if herdr::error_code(&e) == Some("agent_not_ready") => {
            let _ = tx.send(Worker::Warn(format!(
                "{reviewer} is waiting for approval at startup (folder trust, update…). Answer it in the reviewer pane."
            )));
        }
        Err(e) => {
            let _ = tx.send(Worker::Warn(format!("Could not start {reviewer}: {e}")));
            return;
        }
    }
}

fn exe() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("herdr-rooms"))
}

/// Standing orders for a read-only reviewer, given as its system (Claude) or developer
/// (Codex) instructions so they hold in every turn. The sandboxes are what enforce them.
const CLAUDE_READ_ONLY_RULE: &str = "You are the reviewer in a review room. The repository is read-only for you: never create, edit, move or delete files in it, and do not run commands that write to it (sed -i, output redirection into files, git checkout/reset/stash/commit, formatters, installers, code generators). Read files, inspect git state and diffs, and run checks that only read. If a check must write (a build, a test cache, a scratch file), point it at a fresh directory under $TMPDIR. When a fix is needed, show it as a diff in your reply instead of applying it. Your shell is sandboxed: writes to the repository fail with Operation not permitted.\n";
/// Kept short: herdr types the reviewer's command line into a shell, and a long line can be
/// cut off before its Enter arrives.
const CODEX_READ_ONLY_RULE: &str = "You are a read-only reviewer in a review room: never create, edit or delete files, and never run commands that write. Your sandbox blocks every write, $TMPDIR included, and you cannot ask to leave it, so run checks inline (for example python3 -c). Show fixes as diffs in your reply.";

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

/// How the reviewer is held back, for the room's "ready" line.
fn protection(d: &RoomConfig) -> &'static str {
    match (d.reviewer_read_only, d.reviewer_kind.as_str()) {
        (false, _) => "can write to the repository",
        (true, "codex") => "read-only: OS sandbox, no escalation",
        (true, "claude") => "read-only: sandboxed shell, no edit tools, no MCP",
        (true, _) => "read-only",
    }
}

/// Native CLI arguments for the reviewer, including the hook that reports its turns.
/// Claude's settings and system prompt go into files in the room's folder, so the command
/// line herdr types stays short.
fn reviewer_args(d: &RoomConfig) -> Result<Vec<String>> {
    let exe = exe().display().to_string();
    let read_only = d.reviewer_read_only;
    let dir = state::room_dir(&d.id);
    let mut args: Vec<String> = vec![];
    match d.reviewer_kind.as_str() {
        "codex" => {
            args.extend(["-c".into(), "check_for_update_on_startup=false".into()]);
            args.extend(["-c".into(), format!("notify=[{},\"hook\",\"codex-notify\"]", toml_str(&exe))]);
            if read_only {
                // `-a never`: the model cannot ask to run outside the read-only sandbox.
                args.extend(["-s".into(), "read-only".into(), "-a".into(), "never".into()]);
                args.extend(["-c".into(), format!("developer_instructions={}", toml_str(CODEX_READ_ONLY_RULE))]);
            } else {
                args.extend(["-s".into(), "workspace-write".into()]);
            }
            if let Some(m) = &d.reviewer_model {
                args.extend(["-m".into(), m.clone()]);
            }
            if let Some(e) = &d.reviewer_effort {
                args.extend(["-c".into(), format!("model_reasoning_effort={}", toml_str(e))]);
            }
        }
        "claude" => {
            let hook = |kind: &str| serde_json::json!([{ "hooks": [{ "type": "command", "command": format!("'{exe}' hook {kind}") }] }]);
            let mut settings = serde_json::json!({ "hooks": { "UserPromptSubmit": hook("claude-prompt"), "Stop": hook("claude-stop") } });
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
            let settings_file = dir.join("reviewer-settings.json");
            std::fs::write(&settings_file, serde_json::to_string_pretty(&settings)?)?;
            args.extend(["--settings".into(), settings_file.display().to_string()]);
            if read_only {
                // Plan mode would stop at a plan-approval dialog. Instead: no edit tools, no
                // MCP servers (they run outside the sandbox), and the rule in the system prompt.
                let rules_file = dir.join("reviewer-rules.md");
                std::fs::write(&rules_file, CLAUDE_READ_ONLY_RULE)?;
                args.extend(["--disallowedTools".into(), "Edit,Write,NotebookEdit".into()]);
                args.push("--strict-mcp-config".into());
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

/// One queue per agent: prompts go out one at a time, each once that agent is ready.
fn spawn_sender(role: Role, pane: SharedPane, tx: Sender<Worker>) -> Sender<String> {
    let (qtx, qrx) = mpsc::channel::<String>();
    thread::spawn(move || {
        for (seq, text) in (0..).zip(qrx) {
            deliver(role, seq, &pane, &text, &tx);
        }
    });
    qtx
}

/// Types `text` into the agent once it has settled at its prompt, and makes sure a turn
/// really starts: an agent that is still booting can swallow input without a trace.
fn deliver(role: Role, seq: u64, pane: &SharedPane, text: &str, tx: &Sender<Worker>) {
    let who = match role {
        Role::Host => "The host",
        Role::Reviewer => "The reviewer",
    };
    let (mut warned, mut failures, mut tries, mut idle_polls) = (false, 0, 0, 0);
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
        // Before each paste, so the room knows what the agent's next answer replies to; a
        // retry repeats the prompt's number, and the moderator keeps its mark.
        let _ = tx.send(Worker::Delivering(role, seq));
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
    let _ = tx.send(Worker::Sent);
}

fn spawn_status_poller(host: SharedPane, reviewer: SharedPane, tx: Sender<Worker>) {
    thread::spawn(move || loop {
        for (role, pane) in [(Role::Host, &host), (Role::Reviewer, &reviewer)] {
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
    reviewer_pane: SharedPane,
    host_q: Sender<String>,
    reviewer_q: Sender<String>,
    rx: Receiver<Worker>,
    host_status: String,
    reviewer_status: String,
    target: Target,
    input: String,
    /// The highlighted completion, while the word being typed has some.
    pick: usize,
    /// The input as it was when Esc hid its completions: they stay hidden until it changes.
    dismissed: Option<String>,
    /// `/exit` was typed: close the reviewer's pane and the room on the way out.
    exit_all: bool,
    /// Ctrl+Q: close the room, keep the reviewer.
    quit: bool,
    scroll_back: u16,
    last_phase: Phase,
}

impl Room {
    fn new(
        config: RoomConfig,
        reviewer_pane: SharedPane,
        host_q: Sender<String>,
        reviewer_q: Sender<String>,
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
            config,
            moderator,
            reviewer_pane,
            host_q,
            reviewer_q,
            rx,
            host_status: String::new(),
            reviewer_status: String::new(),
            target: Target::Both,
            input: String::new(),
            pick: 0,
            dismissed: None,
            exit_all: false,
            quit: false,
            scroll_back: 0,
            last_phase: Phase::WaitingUser,
        }
    }

    fn close(&self) {
        state::unmap_pane(&self.config.host_pane, &self.config.id);
        if let Some(p) = self.reviewer_pane.lock().unwrap().clone() {
            state::unmap_pane(&p, &self.config.id);
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
                    self.input.push_str(&text.replace("\r\n", "\n").replace('\r', "\n"));
                    self.pick = 0;
                    continue;
                }
                _ => continue,
            };
            if key.kind != KeyEventKind::Press {
                continue;
            }
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            // While the word being typed has completions, the list takes ↑ ↓ Tab Enter Esc.
            let offers = self.offers();
            if !offers.is_empty() {
                let n = offers.len();
                match key.code {
                    KeyCode::Up => self.pick = (self.pick + n - 1) % n,
                    KeyCode::Down => self.pick = (self.pick + 1) % n,
                    KeyCode::Tab | KeyCode::Enter => self.complete(&offers[self.pick.min(n - 1)]),
                    KeyCode::Esc => self.dismissed = Some(self.input.clone()),
                    _ => self.key(key.code, ctrl),
                }
            } else {
                self.key(key.code, ctrl);
            }
            if self.exit_all || self.quit {
                return Ok(());
            }
        }
    }

    fn key(&mut self, code: KeyCode, ctrl: bool) {
        match code {
            KeyCode::Char('q') | KeyCode::Char('c') if ctrl => self.quit = true,
            KeyCode::Char('p') if ctrl => self.moderator.toggle_pause(),
            KeyCode::Char('e') if ctrl => self.moderator.end(),
            KeyCode::Tab => self.target = self.target.next(),
            KeyCode::PageUp => self.scroll_back = self.scroll_back.saturating_add(10),
            KeyCode::PageDown => self.scroll_back = self.scroll_back.saturating_sub(10),
            KeyCode::End => self.scroll_back = 0,
            KeyCode::Esc => self.input.clear(),
            KeyCode::Backspace => {
                self.input.pop();
                self.pick = 0;
            }
            KeyCode::Enter if self.input.trim() == EXIT_COMMAND => {
                self.moderator.end();
                self.exit_all = true;
            }
            KeyCode::Enter => {
                let text = std::mem::take(&mut self.input);
                let to = self.moderator.mentioned(&text).unwrap_or(self.target);
                self.moderator.on_room_input(to, &text);
                self.scroll_back = 0;
            }
            KeyCode::Char(c) if !ctrl => {
                self.input.push(c);
                self.pick = 0;
            }
            _ => {}
        }
    }

    /// Completions for the word being typed, unless Esc hid them for this input.
    fn offers(&self) -> Vec<Completion> {
        if self.dismissed.as_deref() == Some(self.input.as_str()) {
            return vec![];
        }
        self.moderator.completions(&self.input)
    }

    /// Puts `c` in place of the word being typed, with a space after it.
    fn complete(&mut self, c: &Completion) {
        let start = self.input.char_indices().rev().find(|(_, ch)| ch.is_whitespace()).map_or(0, |(i, ch)| i + ch.len_utf8());
        self.input.truncate(start);
        self.input.push_str(&c.word);
        self.input.push(' ');
        self.pick = 0;
    }

    /// Moves worker news onto the log, events in, and prompts out. Worker news goes first:
    /// a `Delivering` must reach the moderator before the answer to that prompt.
    fn pump(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Worker::Status(Role::Host, s) => self.host_status = s,
                Worker::Status(Role::Reviewer, s) => self.reviewer_status = s,
                Worker::Delivering(role, seq) => self.moderator.on_delivering(role, seq),
                Worker::Sent => {}
                Worker::Blocked(role) => {
                    let name = self.name(role);
                    self.moderator.system(format!("⚠ {name} needs approval: answer it in its pane; the message goes through after."));
                }
                Worker::Info(s) => self.moderator.system(s),
                Worker::Warn(s) => self.moderator.system(format!("⚠ {s}")),
            }
        }
        for ev in self.reader.poll() {
            self.moderator.on_event(ev);
            self.scroll_back = 0;
        }
        for out in std::mem::take(&mut self.moderator.outbox) {
            let q = match out.to {
                Role::Host => &self.host_q,
                Role::Reviewer => &self.reviewer_q,
            };
            let _ = q.send(out.text);
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
        match role {
            Role::Host => self.moderator.host_name.clone(),
            Role::Reviewer => self.moderator.reviewer_name.clone(),
        }
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
                ui::agent_color(&self.config.host_kind),
            ),
            (_, Phase::HostWorking) => (format!("{} is working…", e.host_name), ui::agent_color(&self.config.host_kind)),
            (_, Phase::ReviewerWorking) => (format!("{} is reviewing…", e.reviewer_name), ui::agent_color(&self.config.reviewer_kind)),
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
            ])),
            header,
        );

        let (hs, hc) = ui::status_label(&self.host_status);
        let (rs, rc) = ui::status_label(&self.reviewer_status);
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(" ● ", Style::new().fg(ui::agent_color(&self.config.host_kind))),
                Span::raw(format!("{} · host ", e.host_name)),
                Span::styled(format!("@{}  ", e.nick(Role::Host)), Style::new().fg(ui::USER)),
                Span::styled(hs, Style::new().fg(hc)),
                Span::styled("    ● ", Style::new().fg(ui::agent_color(&self.config.reviewer_kind))),
                Span::raw(format!("{} · reviewer ", e.reviewer_name)),
                Span::styled(format!("@{}  ", e.nick(Role::Reviewer)), Style::new().fg(ui::USER)),
                Span::styled(rs, Style::new().fg(rc)),
            ])),
            status,
        );

        let lines = self.message_lines();
        let para = Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::new().borders(Borders::TOP | Borders::BOTTOM).border_style(Style::new().fg(ui::BORDER)),
        );
        let total = para.line_count(body.width) as u16;
        let top = total.saturating_sub(body.height).saturating_sub(self.scroll_back);
        f.render_widget(para.scroll((top, 0)), body);

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
        let shown: String = {
            let width = input.width.saturating_sub(4) as usize;
            // A pasted message keeps its line breaks; the one-line box shows them as ↵.
            let chars: Vec<char> = self.input.chars().map(|c| if c == '\n' { '↵' } else { c }).collect();
            chars[chars.len().saturating_sub(width)..].iter().collect()
        };
        f.render_widget(Paragraph::new(format!("> {shown}")).block(input_block), input);
        f.set_cursor_position(Position::new(input.x + 3 + shown.chars().count() as u16, input.y + 1));
        self.draw_offers(f, input);

        f.render_widget(
            Paragraph::new(Line::styled(
                " Enter send · Tab or @name recipient · /exit close all · Ctrl+P pause · Ctrl+E end · PgUp/PgDn scroll · Ctrl+Q close room",
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
        let host_color = ui::agent_color(&self.config.host_kind);
        let reviewer_color = ui::agent_color(&self.config.reviewer_kind);
        let mut lines = vec![];
        for m in &e.log {
            let time = chrono::Local
                .timestamp_millis_opt(m.ts as i64)
                .single()
                .map(|t| t.format("%H:%M").to_string())
                .unwrap_or_default();
            let (who, color) = match &m.author {
                Author::User => ("YOU".to_string(), ui::USER),
                Author::UserRoom(Target::Both) => ("YOU → both".to_string(), ui::USER),
                Author::UserRoom(Target::Host) => (format!("YOU → {}", e.host_name), ui::USER),
                Author::UserRoom(Target::Reviewer) => (format!("YOU → {}", e.reviewer_name), ui::USER),
                Author::UserAsReviewer => ("YOU · as reviewer ⚑".to_string(), ui::USER),
                Author::Host => (format!("{} · host @{}", e.host_name.to_uppercase(), e.nick(Role::Host)), host_color),
                Author::Reviewer => {
                    (format!("{} · reviewer @{}", e.reviewer_name.to_uppercase(), e.nick(Role::Reviewer)), reviewer_color)
                }
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
