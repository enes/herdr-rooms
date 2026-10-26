//! `/room` → `herdr-rooms open` → the setup popup (`herdr-rooms setup`):
//! pick the reviewer, its model and the rules, then open the room under the host pane.

use crate::state::{self, RoomConfig, Role};
use crate::{herdr, ui};
use anyhow::{anyhow, Result};
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, MouseButton, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::layout::{Constraint, Layout, Margin, Position, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};
use ratatui::Frame;
use std::path::PathBuf;

/// Opens the popup over the calling pane (the agent the user typed `/room` in).
pub fn open() -> Result<()> {
    let host = match std::env::var("HERDR_PANE_ID").ok().filter(|s| !s.is_empty()).or_else(context_pane) {
        Some(pane) => pane,
        None => codex_daemon_pane()?,
    };
    herdr::open_plugin_pane("setup", &["--env", &format!("HOST_PANE={host}"), "--focus"])?;
    println!("Room popup opened.");
    Ok(())
}

/// `!room` in a Codex session on Codex's app-server daemon runs without the
/// pane's variables: take the Codex agent in this folder, the focused one if several.
fn codex_daemon_pane() -> Result<String> {
    let cwd = std::env::current_dir()?;
    let mut found = herdr::agents_in("codex", &cwd);
    if found.len() > 1 {
        found.retain(|a| a.focused);
    }
    match found.as_slice() {
        [one] => {
            // Later herdr calls, the popup's included, go to the server that agent is on.
            std::env::set_var("HERDR_SOCKET_PATH", &one.socket);
            Ok(one.pane.clone())
        }
        [] => Err(anyhow!("not inside a herdr pane: start herdr, run your agent in one of its panes, then try again")),
        _ => Err(anyhow!("several Codex agents work in {}: focus the one to open the room from and retry", cwd.display())),
    }
}

fn context_pane() -> Option<String> {
    let ctx: serde_json::Value = serde_json::from_str(&std::env::var("HERDR_PLUGIN_CONTEXT_JSON").ok()?).ok()?;
    ctx["focused_pane_id"].as_str().filter(|s| !s.is_empty()).map(String::from)
}

struct Candidate {
    kind: &'static str,
    label: &'static str,
    ready: bool,
    version: String,
    note: String,
}

pub(crate) fn on_path(bin: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
}

/// `codex-cli 0.164.0` → `v0.164.0`, `2.1.310 (Claude Code)` → `v2.1.310`.
fn version(bin: &str) -> String {
    let out = std::process::Command::new(bin).arg("--version").output().ok().filter(|o| o.status.success());
    let text = out.map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
    let number = text.split_whitespace().find(|w| w.starts_with(|c: char| c.is_ascii_digit()));
    number.map(|v| format!("v{v}")).unwrap_or_default()
}

fn detect() -> Vec<Candidate> {
    let (codex, claude) = (on_path("codex"), on_path("claude"));
    // Three short CLI calls, side by side so the popup opens without a wait.
    let (codex_logged_in, codex_version, claude_version) = std::thread::scope(|s| {
        let login = s.spawn(|| {
            codex
                && std::process::Command::new("codex").args(["login", "status"]).output().is_ok_and(|o| o.status.success())
        });
        let codex_version = s.spawn(|| if codex { version("codex") } else { String::new() });
        let claude_version = if claude { version("claude") } else { String::new() };
        (login.join().unwrap_or(false), codex_version.join().unwrap_or_default(), claude_version)
    });
    vec![
        Candidate {
            kind: "codex",
            label: "Codex",
            ready: codex_logged_in,
            version: codex_version,
            note: match (codex, codex_logged_in) {
                (false, _) => "✗ not installed".into(),
                (true, false) => "✗ run: codex login".into(),
                (true, true) => "✓ logged in".into(),
            },
        },
        Candidate {
            kind: "claude",
            label: "Claude Code",
            ready: claude,
            version: claude_version,
            note: if claude { "new session".into() } else { "✗ not installed".into() },
        },
    ]
}

fn models(kind: &str) -> Vec<String> {
    let mut list = vec!["default".to_string()];
    match kind {
        "codex" => {
            let path = std::env::var_os("CODEX_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".codex"))
                .join("models_cache.json");
            if let Ok(v) = std::fs::read_to_string(path).and_then(|t| Ok(serde_json::from_str::<serde_json::Value>(&t)?)) {
                for m in v["models"].as_array().into_iter().flatten() {
                    if let Some(slug) = m["slug"].as_str() {
                        list.push(slug.to_string());
                    }
                }
            }
        }
        "claude" => list.extend(["opus", "sonnet", "haiku"].map(String::from)),
        _ => {}
    }
    list
}

fn efforts(kind: &str) -> Vec<String> {
    let mut list = vec!["default", "low", "medium", "high", "xhigh"];
    if kind == "claude" {
        list.push("max");
    }
    list.into_iter().map(String::from).collect()
}

const TOPIC: usize = 0;
const REVIEWER: usize = 1;
const MODEL: usize = 2;
const EFFORT: usize = 3;
const ROUNDS: usize = 4;
const PERMS: usize = 5;
const HOST_NICK: usize = 6;
const REVIEWER_NICK: usize = 7;
const BUTTONS: usize = 8;

/// What a left click on a spot of the popup does; `draw` maps the spots.
#[derive(Clone, Copy)]
enum Hit {
    Focus(usize),
    Reviewer(usize),
    /// A ◂ / ▸ arrow beside a focused field's value.
    Step(usize, i32),
    Perms(bool),
    /// The model or effort box: opens its list.
    List(usize),
    /// An option of the open list.
    Pick(usize),
    /// Anywhere outside the open list.
    Dismiss,
    Cancel,
    Open,
}

enum Click {
    Close,
    Open,
}

/// The open model or effort list: its field and the highlighted option.
#[derive(Clone, Copy)]
struct Dropdown {
    field: usize,
    cursor: usize,
}

struct Form {
    host_pane: String,
    host: Result<herdr::AgentInfo, String>,
    topic: String,
    candidates: Vec<Candidate>,
    reviewer: usize,
    models: Vec<String>,
    model: usize,
    efforts: Vec<String>,
    effort: usize,
    rounds: u32,
    read_only: bool,
    host_nick: String,
    reviewer_nick: String,
    focus: usize,
    open_button: bool,
    dropdown: Option<Dropdown>,
    error: Option<String>,
}

impl Form {
    fn new(host_pane: String) -> Form {
        let host = herdr::agent_get(&host_pane).map_err(|e| e.to_string()).and_then(|a| {
            if a.kind.is_empty() {
                Err("no recognized agent in this pane".into())
            } else {
                Ok(a)
            }
        });
        let candidates = detect();
        let host_kind = host.as_ref().map(|a| a.kind.clone()).unwrap_or_default();
        let reviewer = candidates
            .iter()
            .position(|c| c.ready && c.kind != host_kind)
            .or_else(|| candidates.iter().position(|c| c.ready))
            .unwrap_or(0);
        let mut form = Form {
            host_pane,
            host,
            topic: String::new(),
            candidates,
            reviewer,
            models: vec![],
            model: 0,
            efforts: vec![],
            effort: 0,
            rounds: 3,
            read_only: true,
            host_nick: String::new(),
            reviewer_nick: String::new(),
            focus: TOPIC,
            open_button: true,
            dropdown: None,
            error: None,
        };
        form.reset_options();
        form
    }

    fn kind(&self) -> &'static str {
        self.candidates[self.reviewer].kind
    }

    fn host_kind(&self) -> &str {
        self.host.as_ref().map(|a| a.kind.as_str()).unwrap_or_default()
    }

    fn editing_text(&self) -> bool {
        matches!(self.focus, TOPIC | HOST_NICK | REVIEWER_NICK)
    }

    /// The text field under focus, if any.
    fn text_field(&mut self) -> Option<&mut String> {
        match self.focus {
            TOPIC => Some(&mut self.topic),
            HOST_NICK => Some(&mut self.host_nick),
            REVIEWER_NICK => Some(&mut self.reviewer_nick),
            _ => None,
        }
    }

    /// The picked `@` names, `None` where left empty.
    fn nicks(&self) -> Result<(Option<String>, Option<String>)> {
        let pick = |n: &str| (!n.is_empty()).then(|| n.to_lowercase());
        let (host, reviewer) = (pick(&self.host_nick), pick(&self.reviewer_nick));
        for n in [&host, &reviewer].into_iter().flatten() {
            if crate::review::RESERVED_NICKS.contains(&n.as_str()) {
                return Err(anyhow!("@{n} is taken: it always means a role or both agents"));
            }
        }
        let default = |role| crate::review::default_nick(role, self.host_kind(), self.kind());
        let host_final = host.clone().unwrap_or_else(|| default(Role::Host));
        let reviewer_final = reviewer.clone().unwrap_or_else(|| default(Role::Reviewer));
        if host_final == reviewer_final {
            return Err(anyhow!("both agents would be @{host_final}: pick two different names"));
        }
        Ok((host, reviewer))
    }

    fn reset_options(&mut self) {
        self.models = models(self.kind());
        self.efforts = efforts(self.kind());
        self.model = 0;
        self.effort = 0;
    }

    fn change(&mut self, delta: i32) {
        let step = |i: usize, n: usize| ((i as i32 + delta).rem_euclid(n as i32)) as usize;
        match self.focus {
            REVIEWER => {
                self.reviewer = step(self.reviewer, self.candidates.len());
                self.reset_options();
            }
            MODEL => self.model = step(self.model, self.models.len()),
            EFFORT => self.effort = step(self.effort, self.efforts.len()),
            ROUNDS => self.rounds = (self.rounds as i32 + delta).clamp(1, 10) as u32,
            PERMS => self.read_only = !self.read_only,
            BUTTONS => self.open_button = !self.open_button,
            _ => {}
        }
    }

    fn options(&self, field: usize) -> &[String] {
        if field == MODEL {
            &self.models
        } else {
            &self.efforts
        }
    }

    fn open_list(&mut self, field: usize) {
        let cursor = if field == MODEL { self.model } else { self.effort };
        (self.focus, self.dropdown) = (field, Some(Dropdown { field, cursor }));
    }

    fn pick(&mut self, option: usize) {
        match self.dropdown.take() {
            Some(Dropdown { field: MODEL, .. }) => self.model = option,
            Some(_) => self.effort = option,
            None => {}
        }
    }

    /// Up, down, Enter and Esc while a list is open. Returns false for any other key.
    fn list_key(&mut self, code: KeyCode) -> bool {
        let Some(Dropdown { field, cursor }) = self.dropdown else { return false };
        let n = self.options(field).len();
        let moved = |cursor| Some(Dropdown { field, cursor });
        match code {
            KeyCode::Up | KeyCode::BackTab => self.dropdown = moved((cursor + n - 1) % n),
            KeyCode::Down | KeyCode::Tab => self.dropdown = moved((cursor + 1) % n),
            KeyCode::Enter | KeyCode::Char(' ') => self.pick(cursor),
            KeyCode::Esc => self.dropdown = None,
            _ => return false,
        }
        true
    }

    /// A left click on a spot `draw` reported.
    fn click(&mut self, hit: Hit) -> Option<Click> {
        match hit {
            Hit::Focus(field) => self.focus = field,
            Hit::List(field) => self.open_list(field),
            Hit::Pick(option) => self.pick(option),
            Hit::Dismiss => self.dropdown = None,
            Hit::Reviewer(i) => {
                self.focus = REVIEWER;
                if self.reviewer != i {
                    self.reviewer = i;
                    self.reset_options();
                }
            }
            Hit::Step(field, delta) => {
                self.focus = field;
                self.change(delta);
            }
            Hit::Perms(read_only) => (self.focus, self.read_only) = (PERMS, read_only),
            Hit::Cancel => return Some(Click::Close),
            Hit::Open => {
                (self.focus, self.open_button) = (BUTTONS, true);
                return Some(Click::Open);
            }
        }
        None
    }

    fn confirm(&mut self) -> Result<()> {
        let host = self.host.as_ref().map_err(|e| anyhow!("{e}"))?;
        let cand = &self.candidates[self.reviewer];
        if !cand.ready {
            return Err(anyhow!("{} is unavailable: {}", cand.label, cand.note));
        }
        let pick = |list: &[String], i: usize| (i > 0).then(|| list[i].clone());
        let (host_nick, reviewer_nick) = self.nicks()?;
        let room = RoomConfig {
            id: format!("{}", state::now_ms()),
            cwd: host.cwd.clone(),
            topic: self.topic.trim().to_string(),
            host_pane: self.host_pane.clone(),
            host_kind: host.kind.clone(),
            reviewer_kind: cand.kind.to_string(),
            reviewer_model: pick(&self.models, self.model),
            reviewer_effort: pick(&self.efforts, self.effort),
            reviewer_read_only: self.read_only,
            max_rounds: self.rounds,
            reviewer_pane: None,
            reviewer_name: None,
            host_nick,
            reviewer_nick,
        };
        room.save()?;
        state::map_pane(&room.host_pane, &room.id, Role::Host)?;
        // The room splits the host pane down; the reviewer later splits the host to the
        // right, so the room ends up as a full-width strip under both agents.
        herdr::open_plugin_pane(
            "room",
            &[
                "--placement",
                "split",
                "--target-pane",
                &room.host_pane,
                "--direction",
                "down",
                "--cwd",
                &room.cwd,
                "--env",
                &format!("ROOM_ID={}", room.id),
                "--no-focus",
            ],
        )
    }
}

pub fn run() -> Result<()> {
    let host_pane = std::env::var("HOST_PANE")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(context_pane)
        .ok_or_else(|| anyhow!("no host pane given (HOST_PANE is unset)"))?;
    let mut form = Form::new(host_pane);
    let mut terminal = ratatui::init();
    let _ = execute!(std::io::stdout(), EnableMouseCapture);
    let result = (|| -> Result<()> {
        let mut hits = vec![];
        loop {
            terminal.draw(|f| hits = draw(f, &form))?;
            let key = match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => key,
                Event::Mouse(m) if m.kind == MouseEventKind::Down(MouseButton::Left) => {
                    // Later regions sit on top of earlier ones: an arrow inside a box wins over the box.
                    let at = Position::new(m.column, m.row);
                    let Some(&(_, hit)) = hits.iter().rev().find(|(area, _)| area.contains(at)) else { continue };
                    match form.click(hit) {
                        Some(Click::Close) => return Ok(()),
                        Some(Click::Open) => match form.confirm() {
                            Ok(()) => return Ok(()),
                            Err(e) => form.error = Some(e.to_string()),
                        },
                        None => {}
                    }
                    continue;
                }
                _ => continue,
            };
            if form.list_key(key.code) {
                continue;
            }
            // An open list takes every key it knows; any other key closes it, then acts as usual.
            form.dropdown = None;
            match key.code {
                KeyCode::Enter | KeyCode::Char(' ') if matches!(form.focus, MODEL | EFFORT) => form.open_list(form.focus),
                KeyCode::Esc => return Ok(()),
                KeyCode::Up | KeyCode::BackTab => form.focus = form.focus.saturating_sub(1),
                KeyCode::Down | KeyCode::Tab => form.focus = (form.focus + 1).min(BUTTONS),
                // The two name boxes sit side by side.
                KeyCode::Left if form.focus == REVIEWER_NICK => form.focus = HOST_NICK,
                KeyCode::Right if form.focus == HOST_NICK => form.focus = REVIEWER_NICK,
                KeyCode::Left => form.change(-1),
                KeyCode::Right => form.change(1),
                KeyCode::Char(' ') if !form.editing_text() => form.change(1),
                KeyCode::Enter => {
                    if form.focus == BUTTONS && !form.open_button {
                        return Ok(());
                    }
                    match form.confirm() {
                        Ok(()) => return Ok(()),
                        Err(e) => form.error = Some(e.to_string()),
                    }
                }
                KeyCode::Backspace => {
                    if let Some(field) = form.text_field() {
                        field.pop();
                    }
                }
                KeyCode::Char(c) => {
                    let nick = form.focus != TOPIC;
                    if let Some(field) = form.text_field() {
                        // A name is one word: the room reads `@` names up to the first other character.
                        if !nick || c.is_alphanumeric() || c == '-' {
                            field.push(c);
                        }
                    }
                }
                _ => {}
            }
        }
    })();
    let _ = execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}

fn agent_label(kind: &str) -> String {
    match kind {
        "claude" => "Claude Code".into(),
        other => crate::review::display_name(other),
    }
}

/// A row with `left` at its start and `right` flush against its end.
fn spread(left: Vec<Span<'static>>, right: Vec<Span<'static>>, width: u16) -> Line<'static> {
    let used: usize = left.iter().chain(right.iter()).map(Span::width).sum();
    let mut spans = left;
    spans.push(Span::raw(" ".repeat((width as usize).saturating_sub(used).max(1))));
    spans.extend(right);
    Line::from(spans)
}

/// A field's label: small capitals, blue while the field has the focus.
fn label(text: &str, focused: bool) -> Span<'static> {
    let style = if focused { Style::new().fg(ui::CODEX).bold() } else { Style::new().fg(ui::MUTED) };
    Span::styled(text.to_uppercase(), style)
}

/// A rounded outline on the card; returns its inside, padded a column. Outlines stay unfilled:
/// a terminal draws a border mid-cell, so a filled box would show a band of fill outside it.
fn boxed(f: &mut Frame, area: Rect, border: Color, title: Option<Span<'static>>) -> Rect {
    let mut block = Block::bordered().border_type(BorderType::Rounded).border_style(Style::new().fg(border));
    if let Some(title) = title {
        block = block.title(title);
    }
    let inside = block.inner(area);
    f.render_widget(block, area);
    inside.inner(Margin::new(1, 0))
}

/// The end of `text` that fits in `width` columns: a long entry scrolls left as it grows.
fn tail(text: &str, width: u16) -> String {
    let chars: Vec<char> = text.chars().collect();
    chars[chars.len().saturating_sub(width as usize)..].iter().collect()
}

/// Where spans of these widths sit when `spread` sets them flush against the end of `row`.
fn flush_right(row: Rect, spans: &[Span]) -> Vec<Rect> {
    let widths: Vec<u16> = spans.iter().map(|s| s.width() as u16).collect();
    let mut x = row.right().saturating_sub(widths.iter().sum());
    widths
        .into_iter()
        .map(|width| {
            let at = Rect { x, width, height: 1, ..row };
            x += width;
            at
        })
        .collect()
}

/// The popup as the design canvas draws it: a card, fields in boxes, the picked reviewer in
/// a highlighted box, paired fields side by side, the buttons under a rule. Returns the
/// spots a click acts on, the topmost last.
fn draw(f: &mut Frame, form: &Form) -> Vec<(Rect, Hit)> {
    let mut hits = vec![];
    let full = f.area();
    f.render_widget(Block::new().style(Style::new().bg(ui::CARD).fg(ui::TEXT)), full);
    // herdr keeps a column right of the pane inside its frame: one column less here evens the sides.
    let area = Rect { x: full.x + 2, width: full.width.saturating_sub(3), ..full };
    let width = area.width;
    let gap = Constraint::Length(1);
    let [_, header, top_rule, _, topic_label, topic, _, host_bar, _, reviewer_label, reviewers, _, pair_labels, pair, _, grid, _, names_label, names, error, bottom_rule, footer, _] =
        Layout::vertical([
            gap,
            Constraint::Length(1),
            Constraint::Length(1),
            gap,
            Constraint::Length(1),
            Constraint::Length(3),
            gap,
            Constraint::Length(1),
            gap,
            Constraint::Length(1),
            Constraint::Length(3 * form.candidates.len() as u16),
            gap,
            Constraint::Length(1),
            Constraint::Length(3),
            gap,
            Constraint::Length(2),
            gap,
            Constraint::Length(1),
            Constraint::Length(3),
            Constraint::Fill(1),
            Constraint::Length(1),
            Constraint::Length(1),
            gap,
        ])
        .areas(area);
    let columns = |row: Rect| Layout::horizontal([Constraint::Fill(1), Constraint::Fill(1)]).spacing(2).areas::<2>(row);
    let muted = Style::new().fg(ui::MUTED);
    let accent = Style::new().fg(ui::CODEX);
    let border = |focused: bool| if focused { ui::CODEX } else { ui::LINE };
    let rule = |f: &mut Frame, row: Rect| {
        let row = Rect { x: full.x, width: full.width, ..row };
        f.render_widget(Paragraph::new("─".repeat(row.width as usize)).style(Style::new().fg(ui::LINE)), row);
    };
    let radio = |picked: bool| Span::styled(if picked { "(●) " } else { "( ) " }, if picked { accent } else { muted });
    let mut cursor = None;

    let title = vec![
        Span::styled("● ", Style::new().fg(ui::CLAUDE)),
        Span::styled("● ", accent),
        Span::styled("Review room", Style::new().bold()),
    ];
    f.render_widget(spread(title, vec![Span::styled("esc · close", muted)], width), header);
    rule(f, top_rule);

    let on = form.focus == TOPIC;
    f.render_widget(Line::from(vec![label("Topic", on), Span::styled("  optional", muted)]), topic_label);
    let inside = boxed(f, topic, border(on), None);
    hits.push((topic, Hit::Focus(TOPIC)));
    let shown = tail(&form.topic, inside.width.saturating_sub(1));
    if form.topic.is_empty() {
        f.render_widget(Span::styled("a label for the room; not sent to your agent", muted), inside);
    } else {
        f.render_widget(Span::raw(shown.clone()), inside);
    }
    if on {
        cursor = Some(Position::new(inside.x + shown.chars().count() as u16, inside.y));
    }

    f.render_widget(Block::new().style(Style::new().bg(ui::FIELD)), host_bar);
    let inside = host_bar.inner(Margin::new(1, 0));
    let host_line = match &form.host {
        Ok(a) => spread(
            vec![label("Host · this pane", false)],
            vec![Span::styled("● ", Style::new().fg(ui::agent_color(&a.kind))), Span::raw(agent_label(&a.kind))],
            inside.width,
        ),
        Err(e) => Line::styled(format!("✗ host: {e}"), Style::new().fg(ui::ERR)),
    };
    f.render_widget(host_line, inside);

    f.render_widget(label("Reviewer", form.focus == REVIEWER), reviewer_label);
    let version_width = form.candidates.iter().map(|c| c.version.chars().count()).max().unwrap_or(0);
    let note_width = form.candidates.iter().map(|c| c.note.chars().count()).max().unwrap_or(0);
    for (i, c) in form.candidates.iter().enumerate() {
        let picked = i == form.reviewer;
        let area = Rect { y: reviewers.y + 3 * i as u16, height: 3, ..reviewers };
        let inside = boxed(f, area, border(picked), None);
        hits.push((area, Hit::Reviewer(i)));
        let name = if !c.ready { muted } else if picked { Style::new().bold() } else { Style::new() };
        let left = vec![
            radio(picked),
            Span::styled("● ", Style::new().fg(if c.ready { ui::agent_color(c.kind) } else { ui::MUTED })),
            Span::styled(c.label, name),
        ];
        let note = if c.note.starts_with('✓') { Style::new().fg(ui::OK) } else { muted };
        let right = vec![
            Span::styled(format!("{:<version_width$}", c.version), muted),
            Span::raw("   "),
            Span::styled(format!("{:>note_width$}", c.note), note),
        ];
        f.render_widget(spread(left, right, inside.width), inside);
    }

    let [left, right] = columns(pair_labels);
    f.render_widget(label("Model", form.focus == MODEL), left);
    f.render_widget(label("Effort", form.focus == EFFORT), right);
    let [left, right] = columns(pair);
    let lists = [(left, MODEL), (right, EFFORT)];
    for (area, value, field) in [(left, &form.models[form.model], MODEL), (right, &form.efforts[form.effort], EFFORT)] {
        let on = form.focus == field;
        let inside = boxed(f, area, border(on), None);
        hits.push((area, Hit::List(field)));
        let arrows = if on {
            vec![Span::styled("◂", accent), Span::raw(" "), Span::styled("▸", accent)]
        } else {
            vec![Span::styled("▾", muted)]
        };
        if on {
            let at = flush_right(inside, &arrows);
            hits.extend([(at[0], Hit::Step(field, -1)), (at[2], Hit::Step(field, 1))]);
        }
        f.render_widget(spread(vec![Span::raw(value.clone())], arrows, inside.width), inside);
    }

    // The round limit on the left, the reviewer's permissions on the right: each a label over its value.
    let [left, right] = columns(grid);
    let row = |area: Rect, i: u16| Rect { y: area.y + i, height: 1, ..area };
    hits.extend([
        (row(left, 0), Hit::Focus(ROUNDS)),
        (row(left, 1), Hit::Focus(ROUNDS)),
        (row(right, 0), Hit::Focus(PERMS)),
        (row(right, 1), Hit::Focus(PERMS)),
    ]);
    let on = form.focus == ROUNDS;
    let pill_style = Style::new().bg(ui::FIELD).fg(if on { ui::CODEX } else { ui::TEXT }).bold();
    let pill: Vec<Span> = if on {
        [" ", "◂", " ", &form.rounds.to_string(), " ", "▸", " "].map(|s| Span::styled(s.to_string(), pill_style)).to_vec()
    } else {
        vec![Span::styled(format!(" {} ", form.rounds), pill_style)]
    };
    if on {
        let at = flush_right(row(left, 1), &pill);
        hits.extend([(at[1], Hit::Step(ROUNDS, -1)), (at[5], Hit::Step(ROUNDS, 1))]);
    }
    let on_perms = form.focus == PERMS;
    let segment = |text: &'static str, active: bool, color: Color| {
        let style = if active { Style::new().bg(color).fg(ui::INK).bold() } else { Style::new().bg(ui::FIELD).fg(ui::MUTED) };
        Span::styled(format!(" {text} "), style)
    };
    let mut toggle = vec![segment("read-only", form.read_only, ui::OK), segment("can write", !form.read_only, ui::WARN)];
    if on_perms {
        toggle.insert(0, Span::styled("◂ ", accent));
        toggle.push(Span::styled(" ▸", accent));
    }
    let at = flush_right(row(right, 1), &toggle);
    let first = if on_perms { 1 } else { 0 };
    hits.extend([(at[first], Hit::Perms(true)), (at[first + 1], Hit::Perms(false))]);
    let rounds = vec![spread(vec![label("Round limit", on)], vec![], left.width), spread(vec![], pill, left.width)];
    f.render_widget(Paragraph::new(rounds), left);
    let perms = vec![spread(vec![label("Reviewer permissions", on_perms)], vec![], right.width), spread(vec![], toggle, right.width)];
    f.render_widget(Paragraph::new(perms), right);

    let on = matches!(form.focus, HOST_NICK | REVIEWER_NICK);
    let hint = Span::styled("  optional · @host and @reviewer always work", muted);
    f.render_widget(Line::from(vec![label("Names in the room", on), hint]), names_label);
    let [left, right] = columns(names);
    let default = |role| crate::review::default_nick(role, form.host_kind(), form.kind());
    for (area, word, value, role, field) in [
        (left, "host", &form.host_nick, Role::Host, HOST_NICK),
        (right, "reviewer", &form.reviewer_nick, Role::Reviewer, REVIEWER_NICK),
    ] {
        let on = form.focus == field;
        let title = Span::styled(format!(" {word} "), if on { accent.bold() } else { muted });
        let inside = boxed(f, area, border(on), Some(title));
        hits.push((area, Hit::Focus(field)));
        let shown = tail(value, inside.width.saturating_sub(2));
        let text = if value.is_empty() { Span::styled(default(role), muted) } else { Span::raw(shown.clone()) };
        f.render_widget(Line::from(vec![Span::styled("@", muted), text]), inside);
        if on {
            cursor = Some(Position::new(inside.x + 1 + shown.chars().count() as u16, inside.y));
        }
    }

    let keys = match form.focus {
        _ if form.dropdown.is_some() => "↑↓ choose · ⏎ pick · esc close",
        MODEL | EFFORT => "⏎ list · ←→ change · ↑↓ move",
        HOST_NICK | REVIEWER_NICK => "type a name · ←→ other box · ↑↓ move",
        _ => "↑↓ move · ←→ change · ⏎ open room",
    };
    let keys = Span::styled(keys, muted);
    let keys = match &form.error {
        Some(err) if error.height > 0 => {
            let row = Rect { y: error.bottom() - 1, height: 1, ..error };
            f.render_widget(Span::styled(format!("✗ {err}"), Style::new().fg(ui::ERR)), row);
            keys
        }
        Some(err) => Span::styled(format!("✗ {err}"), Style::new().fg(ui::ERR)),
        None => keys,
    };
    rule(f, bottom_rule);
    let on_cancel = form.focus == BUTTONS && !form.open_button;
    let cancel = if on_cancel { Style::new().bg(ui::BORDER).fg(ui::TEXT).bold() } else { Style::new().bg(ui::LINE).fg(ui::MUTED) };
    let open = if on_cancel { Style::new().bg(ui::LINE).fg(ui::CODEX) } else { Style::new().bg(ui::CODEX).fg(ui::INK).bold() };
    let buttons = vec![Span::styled(" Cancel ", cancel), Span::raw("  "), Span::styled(" Open room ⏎ ", open)];
    let at = flush_right(footer, &buttons);
    hits.extend([(at[0], Hit::Cancel), (at[2], Hit::Open)]);
    f.render_widget(spread(vec![keys], buttons, width), footer);

    if let Some(Dropdown { field, cursor: highlighted }) = form.dropdown {
        let anchor = lists.iter().find(|(_, list)| *list == field).map_or(area, |(area, _)| *area);
        draw_list(f, &mut hits, full, anchor, form.options(field), field_value(form, field), highlighted);
    } else if let Some(at) = cursor {
        f.set_cursor_position(at);
    }
    hits
}

fn field_value(form: &Form, field: usize) -> usize {
    if field == MODEL {
        form.model
    } else {
        form.effort
    }
}

/// An open list under its box (over it when there is more room above), scrolled to keep the
/// highlighted option in view. A click anywhere else closes it.
fn draw_list(f: &mut Frame, hits: &mut Vec<(Rect, Hit)>, full: Rect, anchor: Rect, options: &[String], current: usize, highlighted: usize) {
    let below = full.bottom().saturating_sub(anchor.bottom());
    let above = anchor.y.saturating_sub(full.y);
    let height = (options.len() as u16 + 2).min(below.max(above));
    if height < 3 {
        return;
    }
    let y = if height <= below { anchor.bottom() } else { anchor.y - height };
    let area = Rect { y, height, ..anchor };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(ui::CODEX))
        .style(Style::new().bg(ui::CARD));
    let inside = block.inner(area);
    f.render_widget(Clear, area);
    f.render_widget(block, area);

    hits.push((full, Hit::Dismiss));
    let rows = inside.height as usize;
    let first = highlighted.saturating_sub(rows.saturating_sub(1)).min(options.len().saturating_sub(rows));
    for (row, (i, name)) in options.iter().enumerate().skip(first).take(rows).enumerate() {
        let line = Rect { y: inside.y + row as u16, height: 1, ..inside };
        let style = if i == highlighted { Style::new().bg(ui::CODEX).fg(ui::INK).bold() } else { Style::new() };
        let mark = if i == current { Span::styled("✓", if i == highlighted { style } else { Style::new().fg(ui::OK) }) } else { Span::raw("") };
        let text = spread(vec![Span::raw(format!(" {name}"))], vec![mark, Span::raw(" ")], line.width);
        f.render_widget(Paragraph::new(text).style(style), line);
        hits.push((line, Hit::Pick(i)));
    }
}
