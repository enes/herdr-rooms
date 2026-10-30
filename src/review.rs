//! The moderator of a review room, free of herdr and the terminal: events in, the
//! room's log and outgoing prompts out.

use crate::state::{RoomConfig, Event, EventKind, Role};

pub const USER_ROOM_TAG: &str = "[User · room]:";
// Recognize prompts from older room processes, but never emit this forwarding tag.
pub const USER_AS_REVIEWER_TAG: &str = "[User · as reviewer]:";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Both,
    Host,
    Reviewer,
}

impl Target {
    pub fn next(self) -> Target {
        match self {
            Target::Both => Target::Host,
            Target::Host => Target::Reviewer,
            Target::Reviewer => Target::Both,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// No request yet: waiting for the user to ask the host something.
    WaitingUser,
    HostWorking,
    ReviewerWorking,
    Agreed,
    NoConsensus,
    /// The user ended the review; the room only logs from here on.
    Ended,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Agree,
    Disagree,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Author {
    User,
    UserRoom(Target),
    Host,
    Reviewer,
    System,
}

#[derive(Clone, Debug)]
pub struct Msg {
    pub ts: u64,
    pub author: Author,
    pub text: String,
    pub verdict: Option<Verdict>,
    /// The moderator's `swapped` when the message was written: the agents in its roles
    /// then are the other way round now if it differs.
    pub swapped: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outgoing {
    pub to: Role,
    pub text: String,
}

pub fn display_name(kind: &str) -> String {
    match kind {
        "claude" => "Claude".into(),
        "codex" => "Codex".into(),
        other => {
            let mut c = other.chars();
            c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
        }
    }
}

/// Room `@` words that always mean a role, or both agents.
pub const RESERVED_NICKS: [&str; 4] = ["host", "reviewer", "all", "both"];

/// Typed alone in the room: ends the review and closes the room and the agent it started.
pub const EXIT_COMMAND: &str = "/exit";

/// Typed alone in the room: switches the roles, once both agents are done.
pub const SWAP_COMMAND: &str = "/swap";

/// Typed alone in the room: binds an agent's new session (after `/new` or `/clear`) to
/// its pane, so its turns reach the room again.
pub const REBIND_COMMAND: &str = "/rebind";

/// Typed alone in the room: sends the host's last answer that was only logged (one a
/// background job woke it for, or one the room could not tie to a prompt) to review.
pub const REVIEW_COMMAND: &str = "/review";

/// Starts the message that gives an agent its role after a switch. The standing rule of
/// a read-only room's agent follows the latest one, so a switch never contradicts it.
pub const ROLE_LINE: &str = "(room) Role:";

/// Every prompt the room writes ends with this. The room's own words are English, so
/// without it an agent would answer them in English whatever language the user writes in.
const IN_USER_LANGUAGE: &str = "Reply in the language the user writes in.";

/// Starts the note in a room message for one agent alone, whose reply is only logged.
const ALONE_NOTE: &str = "Only you got this message";

/// What a prompt an agent got asks of it: how the room treats the reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TurnKind {
    /// A review prompt, or the host's turn on its objections: of which review, and which
    /// round of it, as the prompt's trailer says.
    Review(u64, u64),
    /// The user's request to the host, typed or a room message for both: its answer is reviewed.
    Request,
    /// A message whose reply is only logged: a room message for one agent, the reviewer's
    /// copy of one for both, a role notice.
    Side,
    /// Typed into the reviewer's pane.
    Private,
    /// The agent's own background work reporting back: the answer goes on from the turn
    /// that started the work.
    Continue,
}

/// A prompt an agent has not answered yet, by the turn id its hooks report for it.
#[derive(Clone, Debug)]
struct OpenTurn {
    id: String,
    /// The session whose hook reported it: an answer closes only its own session's prompts.
    session: Option<String>,
    /// The room typed it, and the room types only into an idle agent: it began a turn.
    by_room: bool,
    kind: TurnKind,
    /// The room already said its turn ended with no report: it stays for attribution (a
    /// message typed later joins it), but it no longer makes the room expect a report.
    lost: bool,
}

/// The turn whose answer left background work running, with what it answers: the answers
/// that work brings later go on from it.
#[derive(Clone, Debug)]
struct Continuing {
    id: String,
    session: Option<String>,
    kinds: Vec<TurnKind>,
}

/// What an answer is to the room, from the prompts it answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Attribution {
    /// The review, or the host's answer that goes on to review.
    Public,
    /// Only logged.
    Side,
    /// A review of an earlier request than the current one: only logged.
    Stale,
    /// The answer names a turn the room saw no prompt for: only logged, never relayed.
    Unknown,
    /// The agent's own background work woke it, and the room has no turn to go on from:
    /// only logged.
    Woken,
}

/// A `$name` naming an installed skill, in text typed into Codex, is a skill mention: it
/// loads the skill, and the agent runs it instead of reading. Text relayed to a Codex gets
/// a full-width `＄` before those names, and only those: `$TMPDIR` or `$x:expr` in code
/// stay as they are. Says whether anything changed, so the prompt can explain the `＄`.
fn escape_for(kind: &str, text: &str, skills: &[String]) -> (String, bool) {
    if kind != "codex" || skills.is_empty() || !text.contains('$') {
        return (text.to_string(), false);
    }
    let mut out = String::with_capacity(text.len() + 8);
    let mut changed = false;
    let mut rest = text;
    while let Some(at) = rest.find('$') {
        out.push_str(&rest[..at]);
        let after = &rest[at + 1..];
        let name_len = after.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-')).unwrap_or(after.len());
        if name_len > 0 && skills.iter().any(|s| s == &after[..name_len]) {
            out.push('＄');
            changed = true;
        } else {
            out.push('$');
        }
        rest = after;
    }
    out.push_str(rest);
    (out, changed)
}

/// The names of the skills a Codex on this machine has: `room`, which this plugin installs,
/// the folders under `$CODEX_HOME/skills`, and Codex's own under `skills/.system`.
fn codex_skill_names() -> Vec<String> {
    let skills = crate::bind::codex_home().join("skills");
    let mut names = vec!["room".to_string()];
    for dir in [skills.clone(), skills.join(".system")] {
        let Ok(entries) = std::fs::read_dir(dir) else { continue };
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(String::from) else { continue };
            if entry.path().is_dir() && !name.starts_with('.') && !names.contains(&name) {
                names.push(name);
            }
        }
    }
    names
}

/// What a prompt says of a `＄` it had to put in.
const ESCAPED_NOTE: &str = " A ＄ in the text above stands for $: the room keeps skill names from running.";

/// An answer to a request, or to the current round of the current review, is public; one
/// to an earlier round or review is stale; anything else stays in the room. A message typed
/// into the reviewer's pane mid-review joins it: the reply is the review (the user's choice
/// for this room).
fn attribution(kinds: &[TurnKind], current: (u64, u64)) -> Attribution {
    if kinds.contains(&TurnKind::Request) {
        return Attribution::Public;
    }
    let reviews = kinds.iter().filter_map(|k| if let TurnKind::Review(seq, round) = k { Some((*seq, *round)) } else { None });
    let mut reviews = reviews.peekable();
    if reviews.peek().is_none() {
        return Attribution::Side;
    }
    if reviews.any(|key| key == current) {
        Attribution::Public
    } else {
        Attribution::Stale
    }
}

/// The review and the round a review prompt's trailer names (`round 2/3 · #r7`), so the
/// reply's echo says which turn of which review it belongs to. The trailer comes after the
/// forwarded text, which may quote an earlier one, so the last marks count.
fn review_key(text: &str) -> Option<(u64, u64)> {
    const ROUND: &str = "(review room · round ";
    const MARK: &str = "· #r";
    let number = |after: &str| -> Option<u64> {
        let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
        digits.parse().ok()
    };
    let seq = number(&text[text.rfind(MARK)? + MARK.len()..])?;
    let round = number(&text[text.rfind(ROUND)? + ROUND.len()..])?;
    Some((seq, round))
}

/// The index of `role` in arrays kept per role: the host's, then the reviewer's.
fn index(role: Role) -> usize {
    match role {
        Role::Host => 0,
        Role::Reviewer => 1,
    }
}

/// One way to finish the word being typed in the room.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Completion {
    /// The word that replaces the one being typed: `@codex`, `/exit`.
    pub word: String,
    /// What it stands for, shown beside it.
    pub about: String,
}

/// The `@` name an agent gets when the user picks none: its own name, or its role when
/// both agents are the same kind.
pub fn default_nick(role: Role, host_kind: &str, reviewer_kind: &str) -> String {
    let (kind, role_word) = match role {
        Role::Host => (host_kind, "host"),
        Role::Reviewer => (reviewer_kind, "reviewer"),
    };
    if host_kind == reviewer_kind {
        role_word.into()
    } else {
        display_name(kind).to_lowercase()
    }
}

/// Claude Code's prompt when background work an agent started (a monitor, a background
/// shell) reports back: the agent's own work going on, not a message from the user.
pub fn is_task_notification(text: &str) -> bool {
    text.trim_start().starts_with("<task-notification>")
}

/// An agent's internal prompt to itself (Codex titling a thread, writing a catch-up): the
/// hook drops these turns; one that got through is nobody's message.
pub fn is_agent_internal(text: &str) -> bool {
    let t = text.trim_start();
    crate::hook::CODEX_INTERNAL_PROMPTS.iter().any(|p| t.starts_with(p))
}

/// Claude Code hands a pasted prompt to its hooks wrapped in `<pasted_content id="…">` tags.
pub fn unwrap_paste(text: &str) -> String {
    let t = text.trim();
    let Some(rest) = t.strip_prefix("<pasted_content") else { return t.to_string() };
    let Some(open_end) = rest.find('>') else { return t.to_string() };
    let body = &rest[open_end + 1..];
    let body = body.rfind("</pasted_content").map_or(body, |close| &body[..close]);
    body.trim().to_string()
}

/// The reviewer's last `VERDICT:` line, tolerant of markdown around and inside it
/// (`**VERDICT:** AGREE`, `**Verdict**: agree`, `## VERDICT: AGREE`, `> VERDICT: AGREE`).
pub fn parse_verdict(text: &str) -> Verdict {
    for line in text.lines().rev() {
        let plain: String = line.chars().filter(|c| !matches!(c, '*' | '`' | '_')).collect();
        let plain = plain.trim_start_matches(|c: char| matches!(c, '#' | '>' | '-') || c.is_whitespace()).to_uppercase();
        let Some(rest) = plain.strip_prefix("VERDICT") else { continue };
        let Some(rest) = rest.trim_start().strip_prefix(':') else { continue };
        let rest = rest.trim_start();
        if rest.starts_with("DISAGREE") {
            return Verdict::Disagree;
        }
        if rest.starts_with("AGREE") {
            return Verdict::Agree;
        }
    }
    Verdict::Unknown
}

pub struct Moderator {
    pub host_name: String,
    pub reviewer_name: String,
    pub max_rounds: u32,
    pub reviewer_read_only: bool,
    /// `@` names the user picked in the popup, lowercased.
    pub host_nick: Option<String>,
    pub reviewer_nick: Option<String>,
    host_kind: String,
    reviewer_kind: String,
    /// Reviews completed in the current review.
    pub round: u32,
    pub phase: Phase,
    pub paused: bool,
    pub user_prompt: String,
    pub log: Vec<Msg>,
    pub outbox: Vec<Outgoing>,
    /// Flips at each switch of the roles.
    pub swapped: bool,
    /// Background tasks the host's last answer said it is still waiting on.
    pub host_waiting: u32,
    /// The host has not answered the user's latest request yet: a message the user adds
    /// meanwhile (Claude Code hands it to the running turn) joins that request.
    request_open: bool,
    /// The host's answers so far in a turn that background work keeps alive.
    host_partial: Vec<String>,
    /// The reviewer's answers so far in a review that background work keeps alive.
    reviewer_partial: Vec<String>,
    /// Reviews started so far. Each review prompt carries its number, so a reply that
    /// comes after a later request started is told from the current review's.
    review_seq: u64,
    /// Prompts each role's agent has not answered yet, by the turn ids its hooks report:
    /// the host's, then the reviewer's. An answer is tied to its prompt by that id alone.
    open: [Vec<OpenTurn>; 2],
    /// Per role, the turn whose answer left background work running.
    continuing: [Option<Continuing>; 2],
    /// Per role, the agent's last answer left background work running, so its turn is not over.
    busy: [bool; 2],
    /// Prompts held back while paused, sent in order on resume.
    held: Vec<Outgoing>,
    /// The host's answers since its last reviewed one that were only logged: a background
    /// job woke it for them, or the room could not tie them to a prompt. `/review` sends
    /// them to review.
    unreviewed: Vec<String>,
    /// Skill names a Codex would run on seeing `$name` in a prompt.
    codex_skills: Vec<String>,
}

impl Moderator {
    pub fn new(d: &RoomConfig) -> Moderator {
        Moderator {
            host_name: display_name(d.kind(Role::Host)),
            reviewer_name: display_name(d.kind(Role::Reviewer)),
            max_rounds: d.max_rounds.max(1),
            reviewer_read_only: d.reviewer_read_only,
            host_nick: d.nick(Role::Host).map(|n| n.to_lowercase()),
            reviewer_nick: d.nick(Role::Reviewer).map(|n| n.to_lowercase()),
            host_kind: d.kind(Role::Host).into(),
            reviewer_kind: d.kind(Role::Reviewer).into(),
            round: 0,
            phase: Phase::WaitingUser,
            paused: false,
            user_prompt: String::new(),
            log: vec![],
            outbox: vec![],
            swapped: false,
            host_waiting: 0,
            request_open: false,
            host_partial: vec![],
            reviewer_partial: vec![],
            review_seq: 0,
            open: [vec![], vec![]],
            continuing: [None, None],
            busy: [false, false],
            held: vec![],
            unreviewed: vec![],
            codex_skills: codex_skill_names(),
        }
    }

    /// `text` as the agent in `role` may read it, and the note its prompt then carries.
    fn relayed(&self, role: Role, text: &str) -> (String, &'static str) {
        let (text, changed) = escape_for(self.kind(role), text, &self.codex_skills);
        (text, if changed { ESCAPED_NOTE } else { "" })
    }

    /// `role`'s turn goes on until its background work reports back. The room types
    /// nothing into it until then: a prompt typed in mid-turn would leave its later
    /// answers with no telling which conversation they belong to.
    pub fn busy(&self, role: Role) -> bool {
        self.busy[index(role)]
    }

    pub fn name(&self, role: Role) -> &str {
        match role {
            Role::Host => &self.host_name,
            Role::Reviewer => &self.reviewer_name,
        }
    }

    pub fn kind(&self, role: Role) -> &str {
        match role {
            Role::Host => &self.host_kind,
            Role::Reviewer => &self.reviewer_kind,
        }
    }

    fn host_tag(&self) -> String {
        format!("[{} · host]:", self.host_name)
    }

    fn reviewer_tag(&self) -> String {
        format!("[{} · reviewer]:", self.reviewer_name)
    }

    /// Starts the notice that tells the host how a review ended; its reply stays in the room.
    fn verdict_tag(&self) -> String {
        format!("[{} · reviewer · verdict]:", self.reviewer_name)
    }

    /// Prompts the room itself typed into an agent come back through that agent's hook.
    fn is_injected(&self, text: &str) -> bool {
        let t = text.trim_start();
        [
            self.host_tag(),
            self.reviewer_tag(),
            self.verdict_tag(),
            USER_ROOM_TAG.into(),
            USER_AS_REVIEWER_TAG.into(),
            ROLE_LINE.into(),
        ]
        .iter()
        .any(|tag| t.starts_with(tag.as_str()))
    }

    /// Whether `ev` is an agent's echo of a prompt the room typed into it: the one kind of
    /// event that is certainly from the session the room talks to, not from an agent's
    /// internal turn in a thread of its own.
    pub fn is_room_prompt(&self, ev: &Event) -> bool {
        ev.kind == EventKind::User && self.is_injected(&unwrap_paste(&ev.text))
    }

    /// The key of the review turn the room is on now: the current review, its next round.
    fn current_turn(&self) -> (u64, u64) {
        (self.review_seq, u64::from(self.round) + 1)
    }

    /// What a prompt of the room's asks of the agent in `role`, from its text as the
    /// agent's hook echoes it.
    fn echo_kind(&self, role: Role, text: &str) -> TurnKind {
        let t = text.trim_start();
        if t.starts_with(&self.verdict_tag()) {
            return TurnKind::Side;
        }
        if t.starts_with(&self.host_tag()) || t.starts_with(&self.reviewer_tag()) {
            let (seq, round) = review_key(t).unwrap_or(self.current_turn());
            return TurnKind::Review(seq, round);
        }
        if t.starts_with(USER_ROOM_TAG) {
            // The host's answer to a message for both goes on to review; the reviewer's
            // reply to any room message, and a reply to a message for one agent, is logged.
            return if role == Role::Host && !t.contains(ALONE_NOTE) { TurnKind::Request } else { TurnKind::Side };
        }
        if t.starts_with(USER_AS_REVIEWER_TAG) {
            return TurnKind::Request;
        }
        TurnKind::Side
    }

    /// Notes a prompt the agent in `role` got, by the turn id its hook reported. A turn can
    /// hold several prompts (Codex reports every message that joined one), each once.
    fn open_turn(&mut self, role: Role, ev: &Event, kind: TurnKind, by_room: bool) {
        let Some(id) = &ev.turn else { return };
        let open = &mut self.open[index(role)];
        if open.iter().any(|t| &t.id == id && t.session == ev.session && t.kind == kind) {
            return;
        }
        if open.len() >= 32 {
            open.remove(0);
        }
        open.push(OpenTurn { id: id.clone(), session: ev.session.clone(), by_room, kind, lost: false });
    }

    /// What an answer replies to, by the turn it names, among the prompts its own session
    /// got. For Claude Code, which hands a message typed mid-turn to the running turn and
    /// stops once under the later prompt's id, that prompt and the older ones back to the
    /// room's latest: the room types only into an idle agent, so its prompt began the turn,
    /// and anything older ended without a Stop (Esc, an error) and is not answered here.
    /// For an agent that queues messages and answers each in its own turn, the prompts of
    /// that turn alone. An answer that leaves background work running closes nothing yet:
    /// the answers that work brings later go on from it. An answer that names no turn, or
    /// one the room saw no prompt for, is unknown: the order of events is no evidence.
    fn answered(&mut self, role: Role, ev: &Event) -> Attribution {
        let Some(id) = ev.turn.as_ref() else { return Attribution::Unknown };
        let current = self.current_turn();
        let merges = self.kind(role) == "claude";
        let i = index(role);
        let mine = |t: &OpenTurn| t.session == ev.session;
        let Some(first) = self.open[i].iter().position(|t| &t.id == id && mine(t)) else { return Attribution::Unknown };
        let last = self.open[i].iter().rposition(|t| &t.id == id && mine(t)).unwrap_or(first);
        // Codex lists a turn's prompts together, those its failed turns left included: there
        // too, the room's latest prompt began the turn.
        let start = if merges { 0 } else { first };
        let from = self.open[i][start..=last].iter().rposition(|t| t.by_room && mine(t)).map_or(start, |p| start + p);
        let turn: Vec<&OpenTurn> = self.open[i][from..=last].iter().filter(|t| mine(t)).collect();
        let closing: Vec<String> = turn.iter().map(|t| t.id.clone()).collect();
        let mut kinds: Vec<TurnKind> = turn.iter().map(|t| t.kind).collect();
        // A notification of the agent's own background work belongs to the turn it woke;
        // alone, it goes on from the answer that left the work running.
        kinds.retain(|k| *k != TurnKind::Continue);
        let continued = kinds.is_empty();
        if continued {
            match &self.continuing[i] {
                Some(c) if c.session == ev.session => kinds = c.kinds.clone(),
                _ => return Attribution::Woken,
            }
        }
        // The record of the turn that left work running goes only with the answer that
        // ends that turn: the notification's, or one that closes the turn's own prompt. An
        // independent answer in between leaves it, and the prompt, in place.
        let closes_started = self.continuing[i]
            .as_ref()
            .is_some_and(|c| c.session == ev.session && (continued || closing.contains(&c.id)));
        let close = |open: &mut Vec<OpenTurn>| open.retain(|t| !(closing.contains(&t.id) && t.session == ev.session));
        if ev.background == 0 {
            close(&mut self.open[i]);
            if closes_started {
                if let Some(c) = self.continuing[i].take() {
                    self.open[i].retain(|t| !(t.id == c.id && t.session == c.session));
                }
            }
        } else if continued {
            // The notification is answered; the prompt that started the work stays open.
            close(&mut self.open[i]);
        } else {
            self.continuing[i] = Some(Continuing { id: id.clone(), session: ev.session.clone(), kinds: kinds.clone() });
        }
        attribution(&kinds, current)
    }

    /// An answer that could not be tied to a prompt: the user hears why it does not count.
    fn unattributed(&mut self, role: Role, ev: &Event, how: Attribution) {
        let name = self.name(role).to_string();
        let how_to = if role == Role::Host { " To have it reviewed, type /review here." } else { "" };
        self.system(if how == Attribution::Woken {
            format!("A background job woke {name}; its answer is logged only.{how_to}")
        } else if ev.turn.is_none() {
            format!(
                "⚠ {name}'s hooks report no turn id, so its answers cannot be tied to prompts and are logged only: \
                 this needs a Claude Code with prompt_id or a Codex with turn-id in its hooks."
            )
        } else {
            format!("⚠ {name}'s answer names no prompt the room saw: logged, not counted.{how_to}")
        });
    }

    /// `/review`: the host's answers that were only logged since its last reviewed one go
    /// to review as its answer to the current request.
    pub fn review_unreviewed(&mut self) {
        if self.unreviewed.is_empty() {
            self.system("Nothing to review: no answer of the host's went unreviewed since this room view opened.");
            return;
        }
        match self.phase {
            Phase::Ended => {
                self.system("The review is ended; a new request starts a new one.");
                return;
            }
            // The host is on a request: those answers go to review with its answer to it,
            // which must not be taken from it.
            Phase::HostWorking => {
                let mut earlier = std::mem::take(&mut self.unreviewed);
                earlier.append(&mut self.host_partial);
                self.host_partial = earlier;
                self.system("The host is answering a request: its unreviewed answers go to review with that answer.");
                return;
            }
            Phase::ReviewerWorking => {
                self.system("A review is running: type /review again once it ends.");
                return;
            }
            Phase::WaitingUser | Phase::Agreed | Phase::NoConsensus => {}
        }
        let answer = std::mem::take(&mut self.unreviewed).join("\n\n");
        self.round = 0;
        self.host_waiting = 0;
        self.host_partial.clear();
        self.reviewer_partial.clear();
        self.review_seq += 1;
        let text = self.to_reviewer(&answer);
        self.phase = Phase::ReviewerWorking;
        self.send(Outgoing { to: Role::Reviewer, text });
    }

    /// The agent in `role` had its context compacted: what the room told it, its role
    /// first of all, may be gone, so the room says it again. The reply stays in the room.
    fn compacted(&mut self, role: Role) {
        let name = self.name(role).to_string();
        self.system(format!("{name}'s context was compacted; the room reminds it of its role."));
        let text = self.role_reminder(role, "Your context was just compacted");
        self.send(Outgoing { to: role, text });
    }

    /// The agent in `role` is on another conversation of its own (`how` it got there: a
    /// hook saw it, or `/rebind` named it), and its pane and the room follow it to
    /// `session`: nothing the old conversation owed can come any more, and the new one has
    /// not heard of the room, so the room tells it its role. The reply stays in the room.
    pub fn followed(&mut self, role: Role, how: &str, session: &str) {
        let waiting = self.turn_lost(role);
        let name = self.name(role).to_string();
        let mut text = format!("{name} is on another conversation ({how}); the room follows it to session {session} and tells it its role.");
        if waiting {
            text.push_str(" The review was waiting on the old one: send the request again, or Ctrl+E ends the review.");
        }
        self.system(text);
        let text = self.role_reminder(role, "You are on a new conversation");
        self.send(Outgoing { to: role, text });
    }

    fn role_reminder(&self, role: Role, what: &str) -> String {
        let (word, other, other_role) = match role {
            Role::Host => ("host", &self.reviewer_name, "reviewer"),
            Role::Reviewer => ("reviewer", &self.host_name, "host"),
        };
        format!(
            "{ROLE_LINE} {word}. {what}; this is a reminder, not a change: in this review room \
             you are the {word}, and {other} is the {other_role}. Acknowledge in one short line. {IN_USER_LANGUAGE}"
        )
    }

    pub fn system(&mut self, text: impl Into<String>) {
        self.push(Author::System, text.into(), None);
    }

    fn push(&mut self, author: Author, text: String, verdict: Option<Verdict>) {
        self.log.push(Msg { ts: crate::state::now_ms(), author, text, verdict, swapped: self.swapped });
    }

    /// Hands `out` to the room, which types it into its agent behind every prompt before
    /// it; while paused, holds it.
    fn send(&mut self, out: Outgoing) {
        if self.paused {
            self.held.push(out);
        } else {
            self.outbox.push(out);
        }
    }

    fn start_review(&mut self, prompt: &str) {
        self.round = 0;
        self.user_prompt = prompt.to_string();
        self.phase = Phase::HostWorking;
        self.request_open = true;
        self.host_waiting = 0;
        self.host_partial.clear();
        self.reviewer_partial.clear();
        self.review_seq += 1;
    }

    /// Whether the agent in `role` has a prompt the room saw begin a turn and no answer
    /// to it yet, one the room has not already given up on.
    pub fn has_open_turn(&self, role: Role) -> bool {
        self.open[index(role)].iter().any(|t| !t.lost)
    }

    /// Whether the reviewer is on the current review: its prompt was delivered and not
    /// answered yet.
    fn reviewing(&self) -> bool {
        let (seq, round) = self.current_turn();
        self.open[index(Role::Reviewer)].iter().any(|t| t.kind == TurnKind::Review(seq, round))
    }

    pub fn on_event(&mut self, mut ev: Event) {
        match ev.kind {
            EventKind::User => ev.text = unwrap_paste(&ev.text),
            EventKind::Assistant => self.busy[index(ev.role)] = ev.background > 0,
            EventKind::Compacted => {
                self.compacted(ev.role);
                return;
            }
            EventKind::Rebound => {
                self.followed(ev.role, &ev.text, ev.session.as_deref().unwrap_or("?"));
                return;
            }
        }
        match (ev.role, ev.kind) {
            (Role::Host, EventKind::User) => {
                if is_agent_internal(&ev.text) {
                    return;
                }
                if is_task_notification(&ev.text) {
                    self.open_turn(Role::Host, &ev, TurnKind::Continue, false);
                    return;
                }
                if self.is_injected(&ev.text) {
                    let kind = self.echo_kind(Role::Host, &ev.text);
                    self.open_turn(Role::Host, &ev, kind, true);
                    return;
                }
                self.open_turn(Role::Host, &ev, TurnKind::Request, false);
                self.push(Author::User, ev.text.clone(), None);
                if self.phase == Phase::Ended {
                    return;
                }
                if self.request_open && self.phase == Phase::HostWorking {
                    self.user_prompt = format!("{}\n\n{}", self.user_prompt.trim(), ev.text.trim());
                } else {
                    self.start_review(&ev.text);
                }
            }
            (Role::Host, EventKind::Assistant) => {
                let attributed = self.answered(Role::Host, &ev);
                self.push(Author::Host, ev.text.clone(), None);
                if matches!(attributed, Attribution::Unknown | Attribution::Woken) {
                    self.unreviewed.push(ev.text.trim().to_string());
                    self.unattributed(Role::Host, &ev, attributed);
                }
                if attributed != Attribution::Public {
                    // A reply to a message for the host alone stays in the room, and so do the
                    // answers its background work brings later.
                    return;
                }
                if self.phase != Phase::HostWorking {
                    return;
                }
                self.host_partial.push(ev.text.trim().to_string());
                if ev.background > 0 {
                    // The host paused for its background work and comes back on its own.
                    if self.host_waiting == 0 {
                        let (host, n) = (&self.host_name, ev.background);
                        self.system(format!(
                            "⏳ {host} is waiting on {n} background task(s); the review starts when its turn ends."
                        ));
                    }
                    self.host_waiting = ev.background;
                    return;
                }
                self.host_waiting = 0;
                self.request_open = false;
                self.unreviewed.clear();
                let answer = std::mem::take(&mut self.host_partial).join("\n\n");
                let text = self.to_reviewer(&answer);
                self.phase = Phase::ReviewerWorking;
                self.reviewer_partial.clear();
                self.send(Outgoing { to: Role::Reviewer, text });
            }
            (Role::Reviewer, EventKind::User) => {
                if is_agent_internal(&ev.text) {
                    return;
                }
                if is_task_notification(&ev.text) {
                    self.open_turn(Role::Reviewer, &ev, TurnKind::Continue, false);
                    return;
                }
                if self.is_injected(&ev.text) {
                    let kind = self.echo_kind(Role::Reviewer, &ev.text);
                    self.open_turn(Role::Reviewer, &ev, kind, true);
                    return;
                }
                // Typed into the reviewer's pane. While it reviews, the message joins the
                // review and the reply goes to the host; otherwise the exchange is private.
                let reviewing = self.phase == Phase::ReviewerWorking && self.reviewing();
                self.open_turn(Role::Reviewer, &ev, TurnKind::Private, false);
                self.push(Author::UserRoom(Target::Reviewer), ev.text.clone(), None);
                if reviewing {
                    let reviewer = self.reviewer_name.clone();
                    self.system(format!("Your message joins {reviewer}'s review in progress; the reply goes to the host."));
                }
            }
            (_, EventKind::Compacted | EventKind::Rebound) => {}
            (Role::Reviewer, EventKind::Assistant) => {
                let attributed = self.answered(Role::Reviewer, &ev);
                if attributed != Attribution::Public || self.phase != Phase::ReviewerWorking {
                    // A side reply (to the user, say), not a review: no verdict badge unless it gave one.
                    let verdict = parse_verdict(&ev.text);
                    let shown = (verdict != Verdict::Unknown).then_some(verdict);
                    self.push(Author::Reviewer, ev.text.clone(), shown);
                    match attributed {
                        Attribution::Stale => {
                            let reviewer = self.reviewer_name.clone();
                            self.system(format!("⏱ {reviewer}'s review of an earlier round or request came late: logged, not counted."));
                        }
                        Attribution::Unknown | Attribution::Woken => self.unattributed(Role::Reviewer, &ev, attributed),
                        _ => {}
                    }
                    return;
                }
                self.reviewer_partial.push(ev.text.trim().to_string());
                if ev.background > 0 {
                    // The reviewer comes back on its own; its verdict is the one its turn ends with.
                    self.push(Author::Reviewer, ev.text.clone(), None);
                    if self.reviewer_partial.len() == 1 {
                        let (reviewer, n) = (&self.reviewer_name, ev.background);
                        self.system(format!(
                            "⏳ {reviewer} is waiting on {n} background task(s); the review goes on when its turn ends."
                        ));
                    }
                    return;
                }
                let review = std::mem::take(&mut self.reviewer_partial).join("\n\n");
                let verdict = parse_verdict(&review);
                self.push(Author::Reviewer, ev.text.clone(), Some(verdict));
                self.round += 1;
                if verdict == Verdict::Agree {
                    self.phase = Phase::Agreed;
                    let n = self.round;
                    self.system(format!("✓ AGREED · {n} round(s)"));
                    // The host hears how the review ended, as it hears every objection.
                    let text = self.to_host_verdict(&review, true);
                    self.send(Outgoing { to: Role::Host, text });
                } else if self.round >= self.max_rounds {
                    self.phase = Phase::NoConsensus;
                    let (n, max) = (self.round, self.max_rounds);
                    self.system(format!(
                        "⚖ NO CONSENSUS · {n}/{max} rounds — your call. Type in the room; your decision goes to both."
                    ));
                    let text = self.to_host_verdict(&review, false);
                    self.send(Outgoing { to: Role::Host, text });
                } else {
                    let text = self.to_host(&review);
                    self.phase = Phase::HostWorking;
                    self.send(Outgoing { to: Role::Host, text });
                }
            }
        }
    }

    /// The `@` name the room shows for an agent: the one picked in the popup, or the default.
    pub fn nick(&self, role: Role) -> String {
        let picked = match role {
            Role::Host => &self.host_nick,
            Role::Reviewer => &self.reviewer_nick,
        };
        picked.clone().unwrap_or_else(|| default_nick(role, &self.host_kind, &self.reviewer_kind))
    }

    /// Who a room message addresses with `@`: `@host` / `@reviewer`, a picked name, an
    /// agent's own name (`@codex`, also with a suffix like `@codex's`), or `@all`.
    /// `None` names no one.
    pub fn mentioned(&self, text: &str) -> Option<Target> {
        let (host, reviewer) = (self.host_name.to_lowercase(), self.reviewer_name.to_lowercase());
        let (host_nick, reviewer_nick) = (self.nick(Role::Host), self.nick(Role::Reviewer));
        let (mut to_host, mut to_reviewer) = (false, false);
        for word in text.split_whitespace() {
            let Some(rest) = word.strip_prefix('@') else { continue };
            let name: String = rest.chars().take_while(|c| c.is_alphanumeric() || *c == '-').collect();
            let name = name.to_lowercase();
            if name == "all" || name == "both" {
                (to_host, to_reviewer) = (true, true);
            }
            to_host |= name == "host" || name == host || name == host_nick;
            to_reviewer |= name == "reviewer" || name == reviewer || name == reviewer_nick;
        }
        match (to_host, to_reviewer) {
            (true, true) => Some(Target::Both),
            (true, false) => Some(Target::Host),
            (false, true) => Some(Target::Reviewer),
            (false, false) => None,
        }
    }

    /// What the word at the end of the room's input can become: an `@` name that starts
    /// with what was typed, or a `/` command when the word is all the input. A word that
    /// is already complete offers nothing.
    pub fn completions(&self, input: &str) -> Vec<Completion> {
        if input.is_empty() || input.ends_with(char::is_whitespace) {
            return vec![];
        }
        let typed = input.rsplit(char::is_whitespace).next().unwrap_or_default();
        let word = typed.to_lowercase();
        let offers = if typed.starts_with('/') && typed.len() == input.len() {
            let swap = format!("switch roles: {} hosts, {} reviews", self.reviewer_name, self.host_name);
            vec![
                Completion { word: EXIT_COMMAND.into(), about: "end the review, close the room and the agent it started".into() },
                Completion { word: SWAP_COMMAND.into(), about: swap },
                Completion { word: REBIND_COMMAND.into(), about: "list, or bind by id, an agent's new session after /new or /clear".into() },
                Completion { word: REVIEW_COMMAND.into(), about: "send the host's last unreviewed answer to review".into() },
            ]
        } else if word.starts_with('@') {
            self.mention_offers()
        } else {
            return vec![];
        };
        offers.into_iter().filter(|c| c.word.starts_with(&word) && c.word != word).collect()
    }

    /// Every `@` word the room understands, the picked names first, each once.
    fn mention_offers(&self) -> Vec<Completion> {
        let (host, reviewer) = (&self.host_name, &self.reviewer_name);
        let shared = host.eq_ignore_ascii_case(reviewer);
        let hosting = format!("{host} · host");
        let review = format!("{reviewer} · reviewer");
        let own_name = |name: &String, about: &String| Completion {
            word: format!("@{}", name.to_lowercase()),
            about: if shared { "both agents".into() } else { about.clone() },
        };
        let mut offers = vec![
            Completion { word: format!("@{}", self.nick(Role::Host)), about: hosting.clone() },
            Completion { word: format!("@{}", self.nick(Role::Reviewer)), about: review.clone() },
            Completion { word: "@host".into(), about: hosting.clone() },
            Completion { word: "@reviewer".into(), about: review.clone() },
            own_name(host, &hosting),
            own_name(reviewer, &review),
            Completion { word: "@all".into(), about: "both agents".into() },
        ];
        let mut seen = std::collections::HashSet::new();
        offers.retain(|c| seen.insert(c.word.clone()));
        offers
    }

    pub fn on_room_input(&mut self, target: Target, text: &str) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        self.push(Author::UserRoom(target), text.to_string(), None);
        let alone = |role| Outgoing { to: role, text: self.room_text(role, text, true) };
        match target {
            // A message for one agent alone is a side talk: the other agent sees neither it
            // nor the reply, and a review in progress goes on as if it never happened.
            Target::Host => self.send(alone(Role::Host)),
            Target::Reviewer => self.send(alone(Role::Reviewer)),
            Target::Both => {
                let (to_host, to_reviewer) =
                    (self.room_text(Role::Host, text, false), self.room_text(Role::Reviewer, text, false));
                // Once the user ended the review, room messages still reach the agents but
                // start nothing. Otherwise a message for both is the request the review that
                // follows is of, unless a review is running, which it steers.
                match self.phase {
                    Phase::Ended => {}
                    Phase::WaitingUser | Phase::Agreed | Phase::NoConsensus => self.start_review(text),
                    Phase::HostWorking | Phase::ReviewerWorking => self.phase = Phase::HostWorking,
                }
                // The host's answer goes on to the reviewer; the reviewer's own reply is only logged.
                self.send(Outgoing { to: Role::Reviewer, text: to_reviewer });
                self.send(Outgoing { to: Role::Host, text: to_host });
            }
        }
    }

    /// A room message as `role` gets it. One that calls agents by `@` name also says
    /// which name is whose, so each agent knows when it is the one spoken to; one for
    /// `role` `alone` says that its reply goes to the user only.
    fn room_text(&self, role: Role, text: &str, alone: bool) -> String {
        let (other, other_name, other_role) = match role {
            Role::Host => (Role::Reviewer, &self.reviewer_name, "reviewer"),
            Role::Reviewer => (Role::Host, &self.host_name, "host"),
        };
        let mut note = String::new();
        if self.mentioned(text).is_some() {
            let (me, them) = (self.nick(role), self.nick(other));
            note = format!("In this room you are @{me}; {other_name} is @{them}. ");
        }
        if alone {
            note.push_str(&format!(
                "{ALONE_NOTE}, and your reply goes to the user alone; the {other_role} sees neither. "
            ));
        }
        let (text, escaped) = self.relayed(role, text);
        format!("{USER_ROOM_TAG} {text}\n\n---\n(room) {note}{IN_USER_LANGUAGE}{escaped}")
    }

    pub fn toggle_pause(&mut self) {
        self.paused = !self.paused;
        if !self.paused {
            self.outbox.extend(std::mem::take(&mut self.held));
        }
    }

    pub fn end(&mut self) {
        self.phase = Phase::Ended;
        self.held.clear();
        self.open = [vec![], vec![]];
        // Nothing waits on an agent's background work any more: a prompt typed in from
        // here on must not be held for a turn that may never report back.
        self.busy = [false, false];
        self.continuing = [None, None];
        self.system("Review ended. The room only keeps the log from here on.");
    }

    /// The agent in `role` finished a turn and no hook reported it (an error ended the
    /// turn, the hooks did not run, or another session's did): nothing of that turn will
    /// come, so nothing waits on it. Says whether the review was waiting on that turn.
    pub fn turn_lost(&mut self, role: Role) -> bool {
        self.busy[index(role)] = false;
        self.continuing[index(role)] = None;
        self.open[index(role)].iter_mut().for_each(|t| t.lost = true);
        match role {
            Role::Host => self.phase == Phase::HostWorking,
            Role::Reviewer => self.phase == Phase::ReviewerWorking,
        }
    }

    /// Whether the roles may switch, as far as the moderator knows: no review running, no
    /// prompt held back by a pause, and no background work that brings an agent back for
    /// more of its turn. The room also waits for both agents and their prompts.
    pub fn can_swap(&self) -> bool {
        !matches!(self.phase, Phase::HostWorking | Phase::ReviewerWorking)
            && self.held.is_empty()
            && !self.busy[0]
            && !self.busy[1]
    }

    /// Switches the roles: each agent keeps its names and takes the other's role, the
    /// user's next request starts a new review, and each agent is told its new role. The
    /// room calls it only once `can_swap` holds and both agents are done, so the queues,
    /// which stay with their roles, have nothing in them that belongs to the other agent.
    pub fn swap_roles(&mut self) {
        std::mem::swap(&mut self.host_name, &mut self.reviewer_name);
        std::mem::swap(&mut self.host_nick, &mut self.reviewer_nick);
        std::mem::swap(&mut self.host_kind, &mut self.reviewer_kind);
        self.swapped = !self.swapped;
        self.round = 0;
        self.phase = Phase::WaitingUser;
        self.user_prompt.clear();
        self.request_open = false;
        self.host_waiting = 0;
        self.host_partial.clear();
        self.reviewer_partial.clear();
        self.open = [vec![], vec![]];
        self.continuing = [None, None];
        for role in [Role::Host, Role::Reviewer] {
            let text = self.role_notice(role);
            // Like a side talk: the acknowledgement stays in the room.
            self.send(Outgoing { to: role, text });
        }
    }

    /// What the switch means for the agent now in `role`.
    fn role_notice(&self, role: Role) -> String {
        match role {
            Role::Host => format!(
                "{ROLE_LINE} host. The user switched the roles in this review room: you are now the host, \
                 the agent the user works with, and {} reviews your answers and code changes. \
                 Acknowledge in one short line. {IN_USER_LANGUAGE}",
                self.reviewer_name
            ),
            Role::Reviewer => {
                let hands = if self.reviewer_read_only {
                    "Do not create, edit or delete any file: show fixes as diffs in your reply"
                } else {
                    "You may edit files, but list every change you make so the host sees it"
                };
                format!(
                    "{ROLE_LINE} reviewer. The user switched the roles in this review room: you are now the reviewer, \
                     and {} hosts. The room sends you its answers to review. {hands}. \
                     Acknowledge in one short line. {IN_USER_LANGUAGE}",
                    self.host_name
                )
            }
        }
    }

    /// The review prompt. Its trailer numbers the review (`#r`), so the reply's echo says
    /// which review it belongs to.
    fn to_reviewer(&self, host_text: &str) -> String {
        let (host, n, max, seq) = (&self.host_name, self.round + 1, self.max_rounds, self.review_seq);
        let (host_text, escaped) = self.relayed(Role::Reviewer, host_text.trim());
        let mut s = format!("{} {}\n\n---\n", self.host_tag(), host_text);
        if self.round == 0 {
            let request = if self.user_prompt.trim().is_empty() { "(unknown)" } else { self.user_prompt.trim() };
            let hands = if self.reviewer_read_only {
                "do not modify any file"
            } else {
                "you may edit files, but list every change you make so the host sees it"
            };
            s.push_str(&format!(
                "(review room · round {n}/{max} · #r{seq}) You are the reviewer in a review room; {host} is the host, the agent the user works with.\n\
                 The user's request: \"{request}\"\n\
                 Check the host's answer and its code changes (git diff, the files) yourself; {hands}.\n\
                 Object only to points that change the outcome: bugs, missed cases, security, wrong facts, bad design. No style nitpicks.\n\
                 Number your objections O1, O2, … and keep the same numbers in later rounds.\n\
                 Withdraw an objection when the host convinces you; never agree just to end the review.\n\
                 If the only open point is a decision for the user and the host has already asked the user, AGREE.\n"
            ));
        } else {
            s.push_str(&format!(
                "(review room · round {n}/{max} · #r{seq}) Above is the host's reply to your objections. Re-check the code if needed.\n"
            ));
        }
        s.push_str(&format!("{IN_USER_LANGUAGE}{escaped}\nThe LAST line of your reply, in English whatever the language, must be exactly one of:\nVERDICT: AGREE\nVERDICT: DISAGREE"));
        s
    }

    /// How the review ended, for the host: the reviewer's last words and the outcome. The
    /// host's reply, if any, stays in the room.
    fn to_host_verdict(&self, reviewer_text: &str, agreed: bool) -> String {
        let (rev, seq) = (&self.reviewer_name, self.review_seq);
        let outcome = if agreed {
            format!("{rev} agreed: the review of this request is over. Nothing more is asked of you")
        } else {
            let max = self.max_rounds;
            format!("{rev} still objects after {max} rounds: the user decides from here. Nothing more is asked of you")
        };
        let (text, escaped) = self.relayed(Role::Host, reviewer_text.trim());
        format!(
            "{} {text}\n\n---\n(review room · #r{seq}) {outcome}; reply in one line at most. {IN_USER_LANGUAGE}{escaped}",
            self.verdict_tag()
        )
    }

    /// The reviewer's objections for the host. Its trailer numbers the round the host's
    /// reply begins, which is what the reply's echo must carry.
    fn to_host(&self, reviewer_text: &str) -> String {
        let (rev, done, n, max, seq) = (&self.reviewer_name, self.round, self.round + 1, self.max_rounds, self.review_seq);
        let (text, escaped) = self.relayed(Role::Host, reviewer_text.trim());
        format!(
            "{} {text}\n\n---\n(review room · round {n}/{max} · #r{seq}) {rev}, as reviewer, raised the objections above in round {done}; \
             your reply begins round {n}. Weigh each one: where it is right, fix it; where it is wrong, push back with concrete \
             evidence (code, test output). Do not concede just to end the review. Your reply goes back to {rev}. {IN_USER_LANGUAGE}{escaped}",
            self.reviewer_tag()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(max_rounds: u32) -> RoomConfig {
        RoomConfig {
            id: "t".into(),
            cwd: "/".into(),
            topic: String::new(),
            host_pane: "w1:p1".into(),
            host_kind: "claude".into(),
            reviewer_kind: "codex".into(),
            reviewer_model: None,
            reviewer_effort: None,
            reviewer_read_only: true,
            max_rounds,
            reviewer_pane: Some("w1:p2".into()),
            reviewer_name: Some("rev".into()),
            host_nick: None,
            reviewer_nick: None,
            swapped: false,
            reviewer_session: None,
            host_session: None,
            room_pid: None,
        }
    }

    /// An event as an agent that reports no turn ids would write it.
    fn ev(role: Role, kind: EventKind, text: &str) -> Event {
        Event { ts: 0, role, kind, text: text.into(), background: 0, session: None, turn: None }
    }

    /// `ev` as an agent that names its turns reports it.
    fn turn(ev: Event, id: &str) -> Event {
        Event { turn: Some(id.into()), ..ev }
    }

    /// The agent in `role` reports a message typed into its pane, under turn `id`.
    fn typed(e: &mut Moderator, role: Role, text: &str, id: &str) {
        e.on_event(turn(ev(role, EventKind::User, text), id));
    }

    /// An agent's hook echoes a prompt the room typed into it, under turn `id`.
    fn got(e: &mut Moderator, out: &Outgoing, id: &str) {
        e.on_event(turn(ev(out.to, EventKind::User, &out.text), id));
    }

    /// The agent in `role` answers turn `id`.
    fn answer(e: &mut Moderator, role: Role, text: &str, id: &str) {
        e.on_event(turn(ev(role, EventKind::Assistant, text), id));
    }

    /// An answer on turn `id` that leaves `n` background tasks running.
    fn waiting(role: Role, text: &str, n: u32, id: &str) -> Event {
        Event { background: n, ..turn(ev(role, EventKind::Assistant, text), id) }
    }

    fn take(e: &mut Moderator) -> Vec<Outgoing> {
        std::mem::take(&mut e.outbox)
    }

    #[test]
    fn verdict_parsing() {
        assert_eq!(parse_verdict("ok\nVERDICT: AGREE"), Verdict::Agree);
        assert_eq!(parse_verdict("O1 bug\n**VERDICT: DISAGREE**"), Verdict::Disagree);
        assert_eq!(parse_verdict("`VERDICT: agree`\n"), Verdict::Agree);
        assert_eq!(parse_verdict("no verdict here"), Verdict::Unknown);
        assert_eq!(parse_verdict("VERDICT: AGREE\nlater\nVERDICT: DISAGREE"), Verdict::Disagree);
        // markdown inside the line, as models often write it
        assert_eq!(parse_verdict("**VERDICT:** AGREE"), Verdict::Agree);
        assert_eq!(parse_verdict("**Verdict**: disagree"), Verdict::Disagree);
        assert_eq!(parse_verdict("## VERDICT: AGREE"), Verdict::Agree);
        assert_eq!(parse_verdict("> VERDICT: `AGREE`"), Verdict::Agree);
        assert_eq!(parse_verdict("- VERDICT : DISAGREE"), Verdict::Disagree);
        assert_eq!(parse_verdict("VERDICTS: AGREE"), Verdict::Unknown);
    }

    #[test]
    fn review_keys_are_read_from_the_trailer() {
        assert_eq!(review_key("[Claude · host]: x\n\n---\n(review room · round 1/3 · #r7) You are"), Some((7, 1)));
        // a quoted earlier trailer in the forwarded text does not mislead
        assert_eq!(
            review_key("[Codex · reviewer]: see (review room · round 2/3 · #r3)\n\n---\n(review room · round 3/3 · #r4) Codex"),
            Some((4, 3))
        );
        assert_eq!(review_key("[User · room]: hi"), None);
    }

    #[test]
    fn pause_holds_every_message_in_order() {
        let mut e = Moderator::new(&config(3));
        typed(&mut e, Role::Host, "x", "u1");
        e.toggle_pause();
        answer(&mut e, Role::Host, "answer", "u1");
        e.on_room_input(Target::Host, "also add logging");
        e.on_room_input(Target::Both, "hold on");
        assert!(take(&mut e).is_empty());
        e.toggle_pause();
        let to: Vec<Role> = take(&mut e).iter().map(|o| o.to).collect();
        assert_eq!(to, [Role::Reviewer, Role::Host, Role::Reviewer, Role::Host]);
    }

    #[test]
    fn an_ended_review_stays_ended() {
        let mut e = Moderator::new(&config(3));
        typed(&mut e, Role::Host, "x", "u1");
        e.end();
        // room messages still reach the agents, but start no review
        e.on_room_input(Target::Host, "one more thing");
        assert_eq!(take(&mut e).len(), 1);
        typed(&mut e, Role::Reviewer, "typed in the reviewer pane", "p1");
        answer(&mut e, Role::Host, "done", "u1");
        e.on_room_input(Target::Reviewer, "and you?");
        take(&mut e);
        answer(&mut e, Role::Reviewer, "fine\nVERDICT: DISAGREE", "p1");
        assert!(take(&mut e).is_empty());
        assert_eq!((e.phase, e.round), (Phase::Ended, 0));
    }

    #[test]
    fn a_reply_to_a_message_for_both_is_not_a_review() {
        let mut e = Moderator::new(&config(3));
        e.on_room_input(Target::Both, "what do you think of the cache?");
        let out = take(&mut e);
        let to_host = out.iter().find(|o| o.to == Role::Host).unwrap();
        let to_reviewer = out.iter().find(|o| o.to == Role::Reviewer).unwrap();
        // the host answers first: its answer goes on to the reviewer
        got(&mut e, to_host, "h1");
        answer(&mut e, Role::Host, "the cache is fine", "h1");
        let review = take(&mut e);
        assert_eq!(review[0].to, Role::Reviewer);
        assert_eq!(e.phase, Phase::ReviewerWorking);
        // the reviewer's reply to the room message is only logged, even with a verdict line
        got(&mut e, to_reviewer, "r1");
        answer(&mut e, Role::Reviewer, "looks fine to me\nVERDICT: DISAGREE", "r1");
        assert_eq!((e.phase, e.round), (Phase::ReviewerWorking, 0));
        assert!(take(&mut e).is_empty());
        // its next turn, the review, counts
        got(&mut e, &review[0], "r2");
        answer(&mut e, Role::Reviewer, "ok\nVERDICT: AGREE", "r2");
        assert_eq!((e.phase, e.round), (Phase::Agreed, 1));
    }

    #[test]
    fn a_message_for_both_is_the_request_of_the_review_it_starts() {
        let mut e = Moderator::new(&config(3));
        typed(&mut e, Role::Host, "fix avg", "u1");
        answer(&mut e, Role::Host, "fixed avg", "u1");
        let review = take(&mut e);
        got(&mut e, &review[0], "r1");
        answer(&mut e, Role::Reviewer, "fine\nVERDICT: AGREE", "r1");
        assert_eq!((e.phase, e.round), (Phase::Agreed, 1));
        take(&mut e);
        // after a finished review, a message for both starts the next one, of that message
        e.on_room_input(Target::Both, "now the median?");
        assert_eq!((e.phase, e.round, e.user_prompt.as_str()), (Phase::HostWorking, 0, "now the median?"));
        let out = take(&mut e);
        got(&mut e, out.iter().find(|o| o.to == Role::Host).unwrap(), "h1");
        answer(&mut e, Role::Host, "added median", "h1");
        let review = take(&mut e);
        assert!(review[0].text.contains("The user's request: \"now the median?\"") && !review[0].text.contains("fix avg"));
        assert!(review[0].text.contains("round 1/3 · #r2)"));
        // a message for both while that review runs steers it: the request stays
        got(&mut e, &review[0], "r2");
        e.on_room_input(Target::Both, "look at the tests too");
        assert_eq!((e.phase, e.user_prompt.as_str()), (Phase::HostWorking, "now the median?"));
        let out = take(&mut e);
        got(&mut e, out.iter().find(|o| o.to == Role::Host).unwrap(), "h2");
        answer(&mut e, Role::Host, "tests added", "h2");
        let review = take(&mut e);
        assert!(review[0].text.contains("The user's request: \"now the median?\"") && review[0].text.contains("#r2)"));
        // and from a room that has had no request yet, the message is the request
        let mut e = Moderator::new(&config(3));
        e.on_room_input(Target::Both, "is the cache safe?");
        assert_eq!((e.phase, e.user_prompt.as_str()), (Phase::HostWorking, "is the cache safe?"));
    }

    #[test]
    fn a_prompt_pasted_twice_counts_once() {
        // the paste stalled, so the room typed the review again: Codex answers both turns
        let mut e = Moderator::new(&config(3));
        typed(&mut e, Role::Host, "x", "u1");
        answer(&mut e, Role::Host, "answer", "u1");
        let review = take(&mut e);
        got(&mut e, &review[0], "r1");
        got(&mut e, &review[0], "r2");
        answer(&mut e, Role::Reviewer, "O1\nVERDICT: DISAGREE", "r1");
        assert_eq!((e.phase, e.round), (Phase::HostWorking, 1));
        assert_eq!(take(&mut e).len(), 1);
        answer(&mut e, Role::Reviewer, "O1\nVERDICT: DISAGREE", "r2");
        assert_eq!((e.phase, e.round), (Phase::HostWorking, 1));
        assert!(take(&mut e).is_empty());
    }

    #[test]
    fn a_prompt_that_never_arrived_leaves_no_mark() {
        let mut e = Moderator::new(&config(3));
        e.on_room_input(Target::Both, "wait");
        let out = take(&mut e);
        let to_host = out.iter().find(|o| o.to == Role::Host).unwrap();
        got(&mut e, to_host, "h1");
        answer(&mut e, Role::Host, "waiting", "h1");
        let review = take(&mut e);
        assert_eq!(review[0].to, Role::Reviewer);
        // the reviewer never took its copy of the room message; the review reaches it
        got(&mut e, &review[0], "r1");
        answer(&mut e, Role::Reviewer, "done\nVERDICT: AGREE", "r1");
        assert_eq!((e.phase, e.round), (Phase::Agreed, 1));
    }

    #[test]
    fn a_message_for_one_agent_stays_in_the_room() {
        let mut e = Moderator::new(&config(3));
        e.on_room_input(Target::Host, "@claude what does avg do?");
        let out = take(&mut e);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].to, Role::Host);
        assert!(out[0].text.ends_with(
            "In this room you are @claude; Codex is @codex. Only you got this message, and your reply goes \
             to the user alone; the reviewer sees neither. Reply in the language the user writes in."
        ));
        got(&mut e, &out[0], "h1");
        answer(&mut e, Role::Host, "it averages", "h1");
        assert!(take(&mut e).is_empty());
        assert_eq!(e.phase, Phase::WaitingUser);
        assert_eq!(e.log.last().unwrap().author, Author::Host);

        e.on_room_input(Target::Reviewer, "is avg safe?");
        let out = take(&mut e);
        assert_eq!((out.len(), out[0].to), (1, Role::Reviewer));
        assert!(out[0].text.contains("the host sees neither"));
        got(&mut e, &out[0], "r1");
        answer(&mut e, Role::Reviewer, "no, it divides by zero\nVERDICT: DISAGREE", "r1");
        assert!(take(&mut e).is_empty());
        assert_eq!((e.phase, e.round), (Phase::WaitingUser, 0));
    }

    #[test]
    fn a_side_talk_mid_review_leaves_the_review_alone() {
        let mut e = Moderator::new(&config(3));
        typed(&mut e, Role::Host, "x", "u1");
        answer(&mut e, Role::Host, "answer", "u1");
        let review = take(&mut e);
        got(&mut e, &review[0], "r1");
        // the user asks each agent something on the side while the reviewer reviews
        e.on_room_input(Target::Host, "why this way?");
        e.on_room_input(Target::Reviewer, "anything urgent?");
        let side = take(&mut e);
        assert_eq!(side.len(), 2);
        assert_eq!(e.phase, Phase::ReviewerWorking);
        // the review comes first and goes to the host behind the side question
        answer(&mut e, Role::Reviewer, "O1\nVERDICT: DISAGREE", "r1");
        let objections = take(&mut e);
        assert_eq!(objections[0].to, Role::Host);
        assert_eq!((e.phase, e.round), (Phase::HostWorking, 1));
        got(&mut e, &side[1], "r2");
        answer(&mut e, Role::Reviewer, "only O1", "r2");
        got(&mut e, &side[0], "h1");
        answer(&mut e, Role::Host, "because it is simpler", "h1");
        assert!(take(&mut e).is_empty());
        assert_eq!((e.phase, e.round), (Phase::HostWorking, 1));
        // the host's reply to the objections goes on to review
        got(&mut e, &objections[0], "h2");
        answer(&mut e, Role::Host, "fixed O1", "h2");
        assert!(take(&mut e)[0].text.starts_with("[Claude · host]: fixed O1"));
        assert_eq!(e.phase, Phase::ReviewerWorking);
    }

    #[test]
    fn a_side_talk_lasts_until_its_background_work_ends() {
        let mut e = Moderator::new(&config(3));
        e.on_room_input(Target::Host, "run the slow tests");
        let out = take(&mut e);
        got(&mut e, &out[0], "h1");
        e.on_event(waiting(Role::Host, "started them", 1, "h1"));
        answer(&mut e, Role::Host, "all green", "h1");
        assert!(take(&mut e).is_empty());
        // the user's next request to the host is reviewed again
        typed(&mut e, Role::Host, "now fix avg", "u1");
        answer(&mut e, Role::Host, "fixed", "u1");
        assert_eq!(take(&mut e)[0].to, Role::Reviewer);
    }

    #[test]
    fn a_host_side_talk_and_a_review_with_background_work_stay_apart() {
        let mut e = Moderator::new(&config(3));
        let leaked = |out: &[Outgoing]| out.iter().any(|o| o.text.contains("SIDE"));
        typed(&mut e, Role::Host, "review this", "u1");
        e.on_event(waiting(Role::Host, "reviews are running", 1, "u1"));
        // the room holds the side question while the review's background work runs
        e.on_room_input(Target::Host, "how long?");
        let side = take(&mut e);
        assert_eq!(side.len(), 1);
        assert!(e.busy(Role::Host));
        answer(&mut e, Role::Host, "they are in", "u1");
        let review = take(&mut e);
        assert!(review[0].text.starts_with("[Claude · host]: reviews are running\n\nthey are in"));
        assert!(!e.busy(Role::Host));
        // the side question starts background work of its own
        got(&mut e, &side[0], "h1");
        e.on_event(waiting(Role::Host, "SIDE started a check", 1, "h1"));
        assert!(e.busy(Role::Host));
        // the objections wait in the room until that work reports back, which stays in the room
        got(&mut e, &review[0], "r1");
        answer(&mut e, Role::Reviewer, "O1\nVERDICT: DISAGREE", "r1");
        let objections = take(&mut e);
        assert_eq!(objections[0].to, Role::Host);
        answer(&mut e, Role::Host, "SIDE check done", "h1");
        assert!(take(&mut e).is_empty());
        assert_eq!((e.phase, e.round), (Phase::HostWorking, 1));
        got(&mut e, &objections[0], "h2");
        answer(&mut e, Role::Host, "fixed O1", "h2");
        let out = take(&mut e);
        assert!(out[0].text.starts_with("[Claude · host]: fixed O1\n\n---") && !leaked(&out));
    }

    #[test]
    fn a_reviewer_side_talk_with_background_work_is_not_a_review() {
        let mut e = Moderator::new(&config(3));
        e.on_room_input(Target::Reviewer, "profile avg in the background");
        let out = take(&mut e);
        got(&mut e, &out[0], "r1");
        e.on_event(waiting(Role::Reviewer, "started", 1, "r1"));
        assert!(e.busy(Role::Reviewer));
        // a review is asked for before that work reports back; the room holds it
        typed(&mut e, Role::Host, "x", "u1");
        answer(&mut e, Role::Host, "answer", "u1");
        let review = take(&mut e);
        assert_eq!(review[0].to, Role::Reviewer);
        assert_eq!(e.phase, Phase::ReviewerWorking);
        // the profile comes in: still the side talk, not the review
        answer(&mut e, Role::Reviewer, "avg is slow\nVERDICT: DISAGREE", "r1");
        assert!(take(&mut e).is_empty());
        assert_eq!((e.phase, e.round), (Phase::ReviewerWorking, 0));
        assert!(!e.busy(Role::Reviewer));
        got(&mut e, &review[0], "r2");
        answer(&mut e, Role::Reviewer, "fine\nVERDICT: AGREE", "r2");
        assert_eq!((e.phase, e.round), (Phase::Agreed, 1));
    }

    #[test]
    fn the_same_words_after_background_work_end_the_turn() {
        // as the room reads them: through the repeat filter, then the moderator
        let mut seen = crate::state::Dedup::default();
        let mut e = Moderator::new(&config(3));
        e.on_room_input(Target::Host, "check it in the background");
        let out = take(&mut e);
        got(&mut e, &out[0], "h1");
        for (ts, n) in [(1_000, 1), (1_050, 1), (3_000, 0)] {
            let report = Event { ts, ..waiting(Role::Host, "on it", n, "h1") };
            if seen.fresh(&report) {
                e.on_event(report);
            }
        }
        assert!(!e.busy(Role::Host));
        assert_eq!(e.log.iter().filter(|m| m.author == Author::Host).count(), 2);
    }

    #[test]
    fn background_work_holds_the_reviewers_verdict_until_its_turn_ends() {
        let mut e = Moderator::new(&config(3));
        typed(&mut e, Role::Host, "x", "u1");
        answer(&mut e, Role::Host, "answer", "u1");
        let review = take(&mut e);
        got(&mut e, &review[0], "r1");
        e.on_event(waiting(Role::Reviewer, "two checks are running", 2, "r1"));
        e.on_event(waiting(Role::Reviewer, "one is in", 1, "r1"));
        assert!(take(&mut e).is_empty());
        assert_eq!((e.phase, e.round), (Phase::ReviewerWorking, 0));
        assert_eq!(e.log.iter().filter(|m| m.author == Author::System).count(), 1);
        answer(&mut e, Role::Reviewer, "O1 avg divides by zero\nVERDICT: DISAGREE", "r1");
        let out = take(&mut e);
        assert!(out[0].text.starts_with("[Codex · reviewer]: two checks are running\n\none is in\n\nO1"));
        assert_eq!((e.phase, e.round), (Phase::HostWorking, 1));
    }

    #[test]
    fn messages_for_both_sent_mid_review_are_told_apart() {
        let mut e = Moderator::new(&config(3));
        typed(&mut e, Role::Host, "x", "u1");
        answer(&mut e, Role::Host, "answer", "u1");
        let review = take(&mut e);
        assert_eq!(review[0].to, Role::Reviewer);
        got(&mut e, &review[0], "r1");
        // two identical messages for both while the reviewer is still reviewing
        e.on_room_input(Target::Both, "wait");
        e.on_room_input(Target::Both, "wait");
        let both = take(&mut e);
        let copies: Vec<&Outgoing> = both.iter().filter(|o| o.to == Role::Reviewer).collect();
        let to_host: Vec<&Outgoing> = both.iter().filter(|o| o.to == Role::Host).collect();
        assert_eq!(copies.len(), 2);
        // the review in progress ends; the messages for both set the host working, so it is only logged
        answer(&mut e, Role::Reviewer, "O1\nVERDICT: DISAGREE", "r1");
        got(&mut e, to_host[0], "h1");
        answer(&mut e, Role::Host, "ok, waiting", "h1");
        let review = take(&mut e);
        assert_eq!(review[0].to, Role::Reviewer);
        assert_eq!(e.phase, Phase::ReviewerWorking);
        for (i, copy) in copies.iter().enumerate() {
            let id = format!("r{}", i + 2);
            got(&mut e, copy, &id);
            answer(&mut e, Role::Reviewer, "sure\nVERDICT: AGREE", &id);
            assert_eq!((e.phase, e.round), (Phase::ReviewerWorking, 0));
        }
        got(&mut e, &review[0], "r4");
        answer(&mut e, Role::Reviewer, "O2\nVERDICT: DISAGREE", "r4");
        assert_eq!((e.phase, e.round), (Phase::HostWorking, 1));
    }

    #[test]
    fn disagree_then_agree() {
        let mut e = Moderator::new(&config(3));
        typed(&mut e, Role::Host, "make avg handle an empty list", "u1");
        assert_eq!(e.phase, Phase::HostWorking);
        answer(&mut e, Role::Host, "added it", "u1");
        let out = take(&mut e);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].to, Role::Reviewer);
        assert!(out[0].text.starts_with("[Claude · host]: added it"));
        assert!(out[0].text.contains("make avg handle an empty list"));

        // the injected prompt echoes back through the reviewer's hook and starts nothing
        got(&mut e, &out[0], "r1");
        assert_eq!(e.phase, Phase::ReviewerWorking);
        answer(&mut e, Role::Reviewer, "O1 an empty list crashes\nVERDICT: DISAGREE", "r1");
        let out = take(&mut e);
        assert_eq!(out[0].to, Role::Host);
        assert!(out[0].text.starts_with("[Codex · reviewer]: O1"));
        assert_eq!(e.phase, Phase::HostWorking);

        // Claude Code wraps pasted prompts before its hooks see them
        let wrapped = format!("\n\n<pasted_content id=\"b199\">\n{}\n</pasted_content id=\"b199\">", out[0].text);
        typed(&mut e, Role::Host, &wrapped, "h1");
        assert_eq!((e.phase, e.round), (Phase::HostWorking, 1));
        answer(&mut e, Role::Host, "you are right, fixed", "h1");
        let out = take(&mut e);
        assert!(out[0].text.contains("round 2/3"));
        got(&mut e, &out[0], "r2");
        answer(&mut e, Role::Reviewer, "good\nVERDICT: AGREE", "r2");
        assert_eq!(e.phase, Phase::Agreed);
        assert_eq!(e.round, 2);
        // the host hears how it ended; its reply stays in the room
        let notice = take(&mut e);
        assert_eq!((notice.len(), notice[0].to), (1, Role::Host));
        assert!(notice[0].text.starts_with("[Codex · reviewer · verdict]: good") && notice[0].text.contains("Codex agreed"));
        got(&mut e, &notice[0], "v1");
        answer(&mut e, Role::Host, "great", "v1");
        assert!(take(&mut e).is_empty());
        assert_eq!(e.phase, Phase::Agreed);
        // the user's next request starts a new review
        typed(&mut e, Role::Host, "now write the tests", "u2");
        assert_eq!((e.phase, e.round), (Phase::HostWorking, 0));
    }

    #[test]
    fn the_host_hears_a_verdict_even_while_a_new_request_runs() {
        let mut e = Moderator::new(&config(3));
        typed(&mut e, Role::Host, "fix avg", "u1");
        answer(&mut e, Role::Host, "fixed avg", "u1");
        let review = take(&mut e);
        got(&mut e, &review[0], "r1");
        answer(&mut e, Role::Reviewer, "fine\nVERDICT: AGREE", "r1");
        let notice = take(&mut e);
        // the user asks for the next thing before the host has read the notice
        typed(&mut e, Role::Host, "now the median", "u2");
        got(&mut e, &notice[0], "v1");
        answer(&mut e, Role::Host, "noted", "v1");
        assert!(take(&mut e).is_empty(), "the reply to the notice is not the answer to the new request");
        assert_eq!((e.phase, e.round, e.user_prompt.as_str()), (Phase::HostWorking, 0, "now the median"));
        answer(&mut e, Role::Host, "added median", "u2");
        assert!(take(&mut e)[0].text.starts_with("[Claude · host]: added median"));
    }

    #[test]
    fn ending_the_review_frees_an_agent_held_by_background_work() {
        let mut e = Moderator::new(&config(3));
        typed(&mut e, Role::Host, "fix avg", "u1");
        e.on_event(waiting(Role::Host, "running the tests", 1, "u1"));
        assert!(e.busy(Role::Host));
        e.end();
        assert!(!e.busy(Role::Host) && e.can_swap());
        // a turn the hooks never reported frees the agent too, and says whether the review
        // waited on it (a Claude reviewer: its echo came at the start of the turn)
        let mut d = config(3);
        d.reviewer_kind = "claude".into();
        let mut e = Moderator::new(&d);
        typed(&mut e, Role::Host, "fix median", "u2");
        answer(&mut e, Role::Host, "fixed median", "u2");
        let review = take(&mut e);
        got(&mut e, &review[0], "r1");
        e.on_event(waiting(Role::Reviewer, "checking", 1, "r1"));
        assert!(e.busy(Role::Reviewer) && e.has_open_turn(Role::Reviewer));
        assert!(e.turn_lost(Role::Reviewer) && !e.busy(Role::Reviewer));
        assert!(!e.turn_lost(Role::Host));
        // the lost prompt no longer makes the room expect a report, but a message typed
        // into the pane later still joins its turn
        assert!(!e.has_open_turn(Role::Reviewer));
        typed(&mut e, Role::Reviewer, "still there?", "p1");
        assert!(e.has_open_turn(Role::Reviewer));
        answer(&mut e, Role::Reviewer, "O1 late\nVERDICT: DISAGREE", "p1");
        assert_eq!((e.phase, e.round), (Phase::HostWorking, 1));
    }

    #[test]
    fn background_work_holds_the_review_until_the_turn_ends() {
        let mut e = Moderator::new(&config(3));
        typed(&mut e, Role::Host, "review this code", "u1");
        e.on_event(waiting(Role::Host, "three reviews are running", 3, "u1"));
        e.on_event(waiting(Role::Host, "first one found a bug", 2, "u1"));
        assert!(take(&mut e).is_empty());
        assert_eq!((e.phase, e.host_waiting), (Phase::HostWorking, 2));
        // one notice for the wait, not one per wake-up
        assert_eq!(e.log.iter().filter(|m| m.author == Author::System).count(), 1);

        answer(&mut e, Role::Host, "all three are in", "u1");
        let out = take(&mut e);
        assert_eq!(out.len(), 1);
        assert!(out[0].text.starts_with("[Claude · host]: three reviews are running\n\nfirst one found a bug\n\nall three are in"));
        assert_eq!((e.phase, e.host_waiting), (Phase::ReviewerWorking, 0));

        // the next round forwards the host's reply alone
        got(&mut e, &out[0], "r1");
        answer(&mut e, Role::Reviewer, "O1\nVERDICT: DISAGREE", "r1");
        let objections = take(&mut e);
        got(&mut e, &objections[0], "h1");
        answer(&mut e, Role::Host, "fixed", "h1");
        assert!(take(&mut e)[0].text.starts_with("[Claude · host]: fixed\n\n---"));
    }

    #[test]
    fn first_review_states_the_reviewer_permissions() {
        let mut e = Moderator::new(&config(3));
        typed(&mut e, Role::Host, "x", "u1");
        answer(&mut e, Role::Host, "answer", "u1");
        let text = &take(&mut e)[0].text;
        assert!(text.contains("do not modify any file") && text.contains("Reply in the language the user writes in."));

        let mut d = config(3);
        d.reviewer_read_only = false;
        let mut e = Moderator::new(&d);
        typed(&mut e, Role::Host, "x", "u1");
        answer(&mut e, Role::Host, "answer", "u1");
        let text = &take(&mut e)[0].text;
        assert!(text.contains("you may edit files") && !text.contains("do not modify"));
    }

    #[test]
    fn round_limit_hands_the_call_to_the_user() {
        let mut e = Moderator::new(&config(2));
        typed(&mut e, Role::Host, "x", "u1");
        answer(&mut e, Role::Host, "answer", "u1");
        for n in 1..=2 {
            let review = take(&mut e);
            let id = format!("r{n}");
            got(&mut e, &review[0], &id);
            answer(&mut e, Role::Reviewer, "no\nVERDICT: DISAGREE", &id);
            if n == 1 {
                let objections = take(&mut e);
                got(&mut e, &objections[0], "h1");
                answer(&mut e, Role::Host, "answer", "h1");
            }
        }
        assert_eq!(e.phase, Phase::NoConsensus);
        assert_eq!(e.log.last().unwrap().author, Author::System);
        // the host hears that the rounds ran out
        let notice = take(&mut e);
        assert!(notice[0].to == Role::Host && notice[0].text.contains("still objects after 2 rounds"));
        // a later request starts a new review
        typed(&mut e, Role::Host, "another task", "u2");
        assert_eq!((e.phase, e.round), (Phase::HostWorking, 0));
        answer(&mut e, Role::Host, "done", "u2");
        assert_eq!(take(&mut e)[0].to, Role::Reviewer);
    }

    #[test]
    fn direct_reviewer_messages_and_replies_are_private_in_either_role_assignment() {
        for swapped in [false, true] {
            let mut d = config(3);
            d.swapped = swapped;
            let mut e = Moderator::new(&d);
            for (i, prompt) in ["PRIVATE question", "PRIVATE follow-up", "PRIVATE stop"].iter().enumerate() {
                let id = format!("p{i}");
                typed(&mut e, Role::Reviewer, prompt, &id);
                assert!(take(&mut e).is_empty(), "a reviewer-pane message must not reach the host");
                answer(&mut e, Role::Reviewer, "PRIVATE answer\nVERDICT: DISAGREE", &id);
                assert!(take(&mut e).is_empty());
                assert_eq!((e.phase, e.round), (Phase::WaitingUser, 0));
            }
            // A later public request still gets reviewed normally.
            typed(&mut e, Role::Host, "fix avg", "u1");
            answer(&mut e, Role::Host, "fixed avg", "u1");
            let out = take(&mut e);
            assert_eq!(out.len(), 1);
            assert!(!out[0].text.contains("PRIVATE"));
            got(&mut e, &out[0], "r1");
            answer(&mut e, Role::Reviewer, "good\nVERDICT: AGREE", "r1");
            assert_eq!((e.phase, e.round), (Phase::Agreed, 1));
        }
    }

    #[test]
    fn reviewer_private_followups_stay_private_while_a_review_waits() {
        let mut e = Moderator::new(&config(3));
        e.on_room_input(Target::Reviewer, "PRIVATE question");
        let out = take(&mut e);
        got(&mut e, &out[0], "r1");
        e.on_event(waiting(Role::Reviewer, "PRIVATE working", 1, "r1"));
        typed(&mut e, Role::Host, "fix avg", "u1");
        answer(&mut e, Role::Host, "fixed avg", "u1");
        let review = take(&mut e);
        assert_eq!(e.phase, Phase::ReviewerWorking);
        typed(&mut e, Role::Reviewer, "PRIVATE stop", "p1");
        assert!(take(&mut e).is_empty());
        answer(&mut e, Role::Reviewer, "PRIVATE stopped\nVERDICT: DISAGREE", "p1");
        assert!(!e.busy(Role::Reviewer));
        assert!(take(&mut e).is_empty());
        assert_eq!((e.phase, e.round), (Phase::ReviewerWorking, 0));
        got(&mut e, &review[0], "r2");
        answer(&mut e, Role::Reviewer, "O1 empty list\nVERDICT: DISAGREE", "r2");
        let out = take(&mut e);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].to, Role::Host);
        assert!(!out[0].text.contains("PRIVATE"));
    }

    #[test]
    fn a_message_typed_mid_review_joins_the_review() {
        // Claude Code hands a message typed mid-turn to the running turn
        let mut d = config(3);
        d.reviewer_kind = "claude".into();
        let mut e = Moderator::new(&d);
        typed(&mut e, Role::Host, "fix avg", "u1");
        answer(&mut e, Role::Host, "fixed avg", "u1");
        let review = take(&mut e);
        got(&mut e, &review[0], "r1");
        e.on_event(waiting(Role::Reviewer, "incomplete review", 1, "r1"));
        typed(&mut e, Role::Reviewer, "also check the tests", "p1");
        assert!(take(&mut e).is_empty());
        assert!(e.log.last().unwrap().text.contains("joins"));
        e.on_event(waiting(Role::Reviewer, "checking the tests", 1, "p1"));
        assert!(take(&mut e).is_empty());
        answer(&mut e, Role::Reviewer, "O1 no test for []\nVERDICT: DISAGREE", "p1");
        // the reply is the review, and it goes to the host whole
        let out = take(&mut e);
        assert_eq!((e.phase, e.round), (Phase::HostWorking, 1));
        assert_eq!((out.len(), out[0].to), (1, Role::Host));
        assert!(out[0].text.contains("checking the tests") && out[0].text.contains("O1 no test"));
    }

    #[test]
    fn a_message_merged_into_the_review_turn_makes_one_public_answer() {
        let mut d = config(3);
        d.reviewer_kind = "claude".into();
        let mut e = Moderator::new(&d);
        typed(&mut e, Role::Host, "fix avg", "u1");
        answer(&mut e, Role::Host, "fixed avg", "u1");
        let review = take(&mut e);
        got(&mut e, &review[0], "r");
        typed(&mut e, Role::Reviewer, "and the tests?", "p");
        // Claude Code answers both in one turn, under the later prompt's id
        answer(&mut e, Role::Reviewer, "O1 no tests\nVERDICT: DISAGREE", "p");
        let out = take(&mut e);
        assert_eq!((e.phase, e.round), (Phase::HostWorking, 1));
        assert_eq!(out[0].to, Role::Host);
        // the host's reply names the objections prompt and goes on to review
        got(&mut e, &out[0], "h");
        answer(&mut e, Role::Host, "fixed, with tests", "h");
        assert_eq!(take(&mut e)[0].to, Role::Reviewer);
        assert_eq!(e.phase, Phase::ReviewerWorking);
    }

    #[test]
    fn an_ended_review_takes_no_late_answer() {
        let mut e = Moderator::new(&config(3));
        typed(&mut e, Role::Host, "fix avg", "u1");
        answer(&mut e, Role::Host, "fixed avg", "u1");
        let review = take(&mut e);
        got(&mut e, &review[0], "r1");
        typed(&mut e, Role::Reviewer, "PRIVATE question", "p1");
        assert!(take(&mut e).is_empty());
        e.end();
        answer(&mut e, Role::Reviewer, "PRIVATE answer\nVERDICT: AGREE", "p1");
        answer(&mut e, Role::Reviewer, "fine\nVERDICT: AGREE", "r1");
        assert!(take(&mut e).is_empty());
        assert_eq!((e.phase, e.round), (Phase::Ended, 0));
    }

    #[test]
    fn answers_follow_the_prompts_they_name_not_their_order() {
        let mut e = Moderator::new(&config(3));
        // the user talks to the idle reviewer, which will answer late
        typed(&mut e, Role::Reviewer, "PRIVATE what do you make of avg?", "p");
        assert!(take(&mut e).is_empty());
        // meanwhile a request is answered and its review delivered
        typed(&mut e, Role::Host, "fix avg", "u1");
        answer(&mut e, Role::Host, "fixed avg", "u1");
        let review = take(&mut e);
        got(&mut e, &review[0], "r");
        // the private answer comes after the review's delivery, but names its own prompt
        answer(&mut e, Role::Reviewer, "PRIVATE avg looks slow\nVERDICT: DISAGREE", "p");
        assert!(take(&mut e).is_empty());
        assert_eq!((e.phase, e.round), (Phase::ReviewerWorking, 0));
        // the review's answer counts
        answer(&mut e, Role::Reviewer, "fine\nVERDICT: AGREE", "r");
        assert_eq!((e.phase, e.round), (Phase::Agreed, 1));
    }

    #[test]
    fn an_answer_without_a_turn_id_is_never_relayed() {
        // an agent that reports no turn ids: the order of events is no evidence
        let mut e = Moderator::new(&config(3));
        e.on_event(ev(Role::Reviewer, EventKind::User, "PRIVATE is avg slow?"));
        typed(&mut e, Role::Host, "fix avg", "u1");
        answer(&mut e, Role::Host, "fixed avg", "u1");
        let review = take(&mut e);
        got(&mut e, &review[0], "r1");
        // the private reply comes late, after the review's delivery, without an id
        e.on_event(ev(Role::Reviewer, EventKind::Assistant, "PRIVATE yes\nVERDICT: AGREE"));
        assert!(take(&mut e).is_empty());
        assert_eq!((e.phase, e.round), (Phase::ReviewerWorking, 0));
        assert!(e.log.last().unwrap().text.contains("report no turn id"));
        // nor is the host's id-less answer forwarded
        e.on_event(ev(Role::Host, EventKind::Assistant, "an afterthought"));
        assert!(take(&mut e).is_empty());
        answer(&mut e, Role::Reviewer, "fine\nVERDICT: AGREE", "r1");
        assert_eq!((e.phase, e.round), (Phase::Agreed, 1));
    }

    #[test]
    fn a_review_of_an_earlier_request_arriving_late_is_not_counted() {
        let mut e = Moderator::new(&config(3));
        typed(&mut e, Role::Host, "fix avg", "u1");
        answer(&mut e, Role::Host, "fixed avg", "u1");
        let first = take(&mut e);
        assert!(first[0].text.contains("· #r1)"));
        got(&mut e, &first[0], "a");
        // a new request is answered while the reviewer still works on the first
        typed(&mut e, Role::Host, "now the median", "u2");
        answer(&mut e, Role::Host, "added median", "u2");
        let second = take(&mut e);
        assert!(second[0].text.contains("· #r2)"));
        assert_eq!(e.phase, Phase::ReviewerWorking);
        // the first review arrives: it names the first prompt, so it is not the median's review
        answer(&mut e, Role::Reviewer, "avg is fine\nVERDICT: AGREE", "a");
        assert!(take(&mut e).is_empty());
        assert_eq!((e.phase, e.round), (Phase::ReviewerWorking, 0));
        assert!(e.log.last().unwrap().text.contains("came late"));
        got(&mut e, &second[0], "b");
        answer(&mut e, Role::Reviewer, "O1 median of []\nVERDICT: DISAGREE", "b");
        assert_eq!((e.phase, e.round), (Phase::HostWorking, 1));
        assert!(take(&mut e)[0].text.contains("O1 median"));
    }

    #[test]
    fn an_answer_naming_an_unknown_turn_is_not_counted() {
        let mut e = Moderator::new(&config(3));
        typed(&mut e, Role::Host, "fix avg", "u1");
        answer(&mut e, Role::Host, "fixed avg", "u1");
        take(&mut e);
        // the room saw no echo of the prompt this answers: it may be a private reply whose
        // echo was lost, so it is only logged, and the user hears why
        answer(&mut e, Role::Reviewer, "fine\nVERDICT: AGREE", "unseen");
        assert!(take(&mut e).is_empty());
        assert_eq!((e.phase, e.round), (Phase::ReviewerWorking, 0));
        assert!(e.log.last().unwrap().text.contains("names no prompt"));
        // the same for the host's answer
        answer(&mut e, Role::Host, "by the way", "unseen-too");
        assert!(take(&mut e).is_empty());
        assert_eq!(e.phase, Phase::ReviewerWorking);
        // a room message for the host alone, answered under its own id, stays in the room
        e.on_room_input(Target::Host, "why avg?");
        let out = take(&mut e);
        got(&mut e, &out[0], "s");
        answer(&mut e, Role::Host, "because", "s");
        assert!(take(&mut e).is_empty());
    }

    #[test]
    fn a_late_final_answer_after_background_work_keeps_its_prompt() {
        let mut e = Moderator::new(&config(3));
        typed(&mut e, Role::Host, "fix avg", "u1");
        answer(&mut e, Role::Host, "fixed avg", "u1");
        let first = take(&mut e);
        got(&mut e, &first[0], "a");
        // the reviewer stops with background work running: its prompt stays open
        e.on_event(waiting(Role::Reviewer, "checking in the background", 1, "a"));
        // meanwhile a new request is answered and its review queued
        typed(&mut e, Role::Host, "now the median", "u2");
        answer(&mut e, Role::Host, "added median", "u2");
        let second = take(&mut e);
        // the first review ends under the same id: an earlier request's, not the median's
        answer(&mut e, Role::Reviewer, "avg is fine\nVERDICT: AGREE", "a");
        assert!(take(&mut e).is_empty());
        assert_eq!((e.phase, e.round), (Phase::ReviewerWorking, 0));
        got(&mut e, &second[0], "b");
        answer(&mut e, Role::Reviewer, "O1 median of []\nVERDICT: DISAGREE", "b");
        assert_eq!((e.phase, e.round), (Phase::HostWorking, 1));
        assert!(take(&mut e)[0].text.contains("O1 median"));
    }

    #[test]
    fn background_work_reporting_back_goes_on_from_its_turn() {
        let mut d = config(3);
        d.reviewer_kind = "claude".into();
        let mut e = Moderator::new(&d);
        let note = "<task-notification>\n<task-id>b1</task-id>\n<status>completed</status>\n</task-notification>";
        typed(&mut e, Role::Host, "fix avg", "u1");
        answer(&mut e, Role::Host, "fixed avg", "u1");
        let review = take(&mut e);
        got(&mut e, &review[0], "a");
        e.on_event(waiting(Role::Reviewer, "running the tests", 1, "a"));
        // Claude Code wakes the reviewer with a notification, a prompt of its own
        typed(&mut e, Role::Reviewer, note, "n");
        answer(&mut e, Role::Reviewer, "O1 a test fails\nVERDICT: DISAGREE", "n");
        let out = take(&mut e);
        assert_eq!((e.phase, e.round), (Phase::HostWorking, 1));
        assert!(out[0].text.starts_with("[Claude · reviewer]: running the tests\n\nO1 a test fails"));
        // nothing of the review's turn is left open
        assert!(e.open[1].is_empty() && e.continuing[1].is_none());
    }

    #[test]
    fn a_notification_alone_goes_on_from_the_answer_that_started_the_work() {
        // Codex-style bookkeeping: a notification turn with nothing else open continues the
        // turn whose answer left the work running, and leaves that prompt open until the end
        let mut e = Moderator::new(&config(3));
        let note = "<task-notification>\n<task-id>b1</task-id>\n<status>completed</status>\n</task-notification>";
        typed(&mut e, Role::Host, "fix avg", "u1");
        answer(&mut e, Role::Host, "fixed avg", "u1");
        let review = take(&mut e);
        got(&mut e, &review[0], "a");
        e.on_event(waiting(Role::Reviewer, "running the tests", 2, "a"));
        typed(&mut e, Role::Reviewer, note, "n1");
        e.on_event(waiting(Role::Reviewer, "one is in", 1, "n1"));
        assert_eq!(e.open[1].len(), 1, "the notification is answered; the review's prompt stays open");
        typed(&mut e, Role::Reviewer, note, "n2");
        answer(&mut e, Role::Reviewer, "O1 a test fails\nVERDICT: DISAGREE", "n2");
        let out = take(&mut e);
        assert_eq!((e.phase, e.round), (Phase::HostWorking, 1));
        assert!(out[0].text.starts_with("[Codex · reviewer]: running the tests\n\none is in\n\nO1 a test fails"));
        assert!(e.open[1].is_empty() && e.continuing[1].is_none());
        // a notification with nothing to go on from: logged, with a note that says so
        typed(&mut e, Role::Reviewer, note, "n3");
        answer(&mut e, Role::Reviewer, "stray", "n3");
        assert!(e.log.last().unwrap().text.contains("A background job woke Codex"));
    }

    #[test]
    fn an_independent_answer_leaves_a_turn_with_background_work_open() {
        let mut e = Moderator::new(&config(3));
        typed(&mut e, Role::Host, "fix avg", "u1");
        answer(&mut e, Role::Host, "fixed avg", "u1");
        let review = take(&mut e);
        got(&mut e, &review[0], "a");
        e.on_event(waiting(Role::Reviewer, "checking in the background", 1, "a"));
        // the user asks the reviewer something private; Codex answers it in a turn of its own
        typed(&mut e, Role::Reviewer, "PRIVATE how long?", "p");
        answer(&mut e, Role::Reviewer, "PRIVATE a minute", "p");
        assert!(take(&mut e).is_empty());
        assert_eq!(e.open[1].len(), 1, "the review's prompt stays open");
        assert!(e.continuing[1].is_some(), "and so does the record of its background work");
        // the review ends under its own id: it still counts, with its earlier part
        answer(&mut e, Role::Reviewer, "O1 avg of []\nVERDICT: DISAGREE", "a");
        let out = take(&mut e);
        assert_eq!((e.phase, e.round), (Phase::HostWorking, 1));
        assert!(out[0].text.starts_with("[Codex · reviewer]: checking in the background\n\nO1 avg of []"));
        assert!(e.open[1].is_empty() && e.continuing[1].is_none());
    }

    #[test]
    fn text_relayed_to_codex_cannot_mention_its_skills() {
        // the host is Claude, the reviewer Codex: what goes to Codex loses the `$` before
        // its installed skills' names, and nothing else
        let mut e = Moderator::new(&config(3));
        e.codex_skills = vec!["room".into(), "deploy".into()];
        typed(&mut e, Role::Host, "x", "u1");
        answer(&mut e, Role::Host, "run the $room skill with $TMPDIR and $x:expr, not $1, $deploy-x or a lone $", "u1");
        let review = take(&mut e);
        assert!(review[0].text.contains("run the ＄room skill with $TMPDIR and $x:expr, not $1, $deploy-x or a lone $"));
        assert!(review[0].text.contains("A ＄ in the text above stands for $"));
        // the room keeps the host's words as they were
        assert!(e.log.iter().any(|m| m.author == Author::Host && m.text.contains("$room")));
        // what goes to Claude keeps its dollars, and gets no note
        got(&mut e, &review[0], "r1");
        answer(&mut e, Role::Reviewer, "O1 use $room\nVERDICT: DISAGREE", "r1");
        let objections = take(&mut e);
        assert!(objections[0].text.contains("O1 use $room") && !objections[0].text.contains("stands for $"));
        assert!(objections[0].text.contains("your reply begins round 2"));
        e.on_room_input(Target::Both, "look at $room and $deploy");
        let out = take(&mut e);
        let to_codex = &out.iter().find(|o| o.to == Role::Reviewer).unwrap().text;
        assert!(to_codex.contains("look at ＄room and ＄deploy") && to_codex.ends_with("skill names from running."));
        assert!(out.iter().find(|o| o.to == Role::Host).unwrap().text.contains("look at $room and $deploy"));
        // text with no skill name in it is left alone, note included
        let skills = vec!["room".to_string()];
        assert_eq!(escape_for("codex", "$ROOM $rooms $room_ $_x $9 $ $$room", &skills), ("$ROOM $rooms $room_ $_x $9 $ $＄room".to_string(), true));
        assert_eq!(escape_for("codex", "no mention of $HOME", &skills), ("no mention of $HOME".to_string(), false));
        assert_eq!(escape_for("claude", "$room", &skills), ("$room".to_string(), false));
    }

    #[test]
    fn a_compaction_gets_the_agent_its_role_again() {
        let mut e = Moderator::new(&config(3));
        typed(&mut e, Role::Host, "fix avg", "u1");
        answer(&mut e, Role::Host, "fixed avg", "u1");
        take(&mut e);
        e.on_event(Event { kind: EventKind::Compacted, ..ev(Role::Reviewer, EventKind::User, "compact") });
        let out = take(&mut e);
        assert_eq!((out.len(), out[0].to), (1, Role::Reviewer));
        assert!(out[0].text.starts_with("(room) Role: reviewer.") && out[0].text.contains("reminder, not a change"));
        assert!(e.log.last().unwrap().text.contains("context was compacted"));
        // the reminder's echo starts nothing and the acknowledgement stays in the room
        got(&mut e, &out[0], "c1");
        answer(&mut e, Role::Reviewer, "ok, still reviewing", "c1");
        assert!(take(&mut e).is_empty());
        assert_eq!((e.phase, e.round), (Phase::ReviewerWorking, 0));
    }

    #[test]
    fn an_answer_a_background_job_woke_goes_to_review_on_request() {
        let mut e = Moderator::new(&config(3));
        let note = "<task-notification>\n<task-id>b1</task-id>\n<status>completed</status>\n</task-notification>";
        // the user asks for the work at a later time; the host sets a timer and answers
        typed(&mut e, Role::Host, "start step 5 at 14:41", "u1");
        answer(&mut e, Role::Host, "ok, at 14:41", "u1");
        let review = take(&mut e);
        got(&mut e, &review[0], "r1");
        answer(&mut e, Role::Reviewer, "fine\nVERDICT: AGREE", "r1");
        take(&mut e);
        // the timer wakes the host; the room has no turn of the host's to go on from
        typed(&mut e, Role::Host, note, "n1");
        answer(&mut e, Role::Host, "here is the work", "n1");
        assert!(take(&mut e).is_empty());
        let last = e.log.last().unwrap().text.clone();
        assert!(last.contains("A background job woke Claude") && last.contains("type /review"), "{last}");
        e.review_unreviewed();
        let out = take(&mut e);
        assert_eq!((out.len(), out[0].to), (1, Role::Reviewer));
        assert!(out[0].text.starts_with("[Claude · host]: here is the work"));
        assert!(out[0].text.contains("The user's request: \"start step 5 at 14:41\"") && out[0].text.contains("#r2)"));
        assert_eq!((e.phase, e.round), (Phase::ReviewerWorking, 0));
        got(&mut e, &out[0], "r2");
        answer(&mut e, Role::Reviewer, "good\nVERDICT: AGREE", "r2");
        assert_eq!((e.phase, e.round), (Phase::Agreed, 1));
        take(&mut e);
        // nothing is left to review a second time
        e.review_unreviewed();
        assert!(take(&mut e).is_empty());
        assert!(e.log.last().unwrap().text.contains("Nothing to review"));
    }

    #[test]
    fn review_does_not_swallow_the_answer_to_an_open_request() {
        // the host's answer came into a room opened during its turn: unreviewed. Then the
        // user types a request into the host's pane, and /review is typed in the room while
        // the host is still on that request
        let mut d = config(3);
        d.reviewer_kind = "claude".into();
        let mut e = Moderator::new(&d);
        answer(&mut e, Role::Host, "Düzelttim ve oda yeniden açıldı", "x0");
        typed(&mut e, Role::Host, "Sen tetikle", "u1");
        e.review_unreviewed();
        assert!(take(&mut e).is_empty());
        assert!(e.log.last().unwrap().text.contains("go to review with that answer"));
        // the host answers the open request: both go to review together, as one answer
        answer(&mut e, Role::Host, "5. adım raporu: …", "u1");
        let out = take(&mut e);
        assert_eq!((out.len(), out[0].to), (1, Role::Reviewer));
        assert!(out[0].text.starts_with("[Claude · host]: Düzelttim ve oda yeniden açıldı\n\n5. adım raporu: …"));
        assert_eq!(e.phase, Phase::ReviewerWorking);
        // another unreviewed answer while the review runs: /review waits for the review to end
        answer(&mut e, Role::Host, "a stray note", "x1");
        e.review_unreviewed();
        assert!(take(&mut e).is_empty() && e.log.last().unwrap().text.contains("type /review again once it ends"));
    }

    #[test]
    fn a_turn_that_ended_without_a_stop_is_not_merged_into_the_next() {
        // a Claude reviewer's review turn ends without a Stop (Esc, an API error, a usage limit)
        let mut d = config(3);
        d.reviewer_kind = "claude".into();
        let mut e = Moderator::new(&d);
        typed(&mut e, Role::Host, "fix avg", "u1");
        answer(&mut e, Role::Host, "fixed avg", "u1");
        let review = take(&mut e);
        got(&mut e, &review[0], "r1");
        // the user asks the reviewer alone whether it is stuck
        e.on_room_input(Target::Reviewer, "are you stuck?");
        let side = take(&mut e);
        assert!(side[0].text.contains("the host sees neither"));
        got(&mut e, &side[0], "s1");
        answer(&mut e, Role::Reviewer, "PRIVATE no, the API failed", "s1");
        assert!(take(&mut e).is_empty());
        assert_eq!((e.phase, e.round), (Phase::ReviewerWorking, 0));

        // the same on the host's side: a request interrupted with Esc, then a message for the host alone
        let mut e = Moderator::new(&config(3));
        typed(&mut e, Role::Host, "fix avg", "u1");
        e.on_room_input(Target::Host, "@claude what does avg do?");
        let side = take(&mut e);
        got(&mut e, &side[0], "h1");
        answer(&mut e, Role::Host, "SECRET it averages", "h1");
        assert!(take(&mut e).is_empty());
    }

    #[test]
    fn another_session_in_the_same_pane_answers_nothing_of_the_rooms() {
        // a `claude -p` run from the reviewer pane inherits HERDR_PANE_ID and reports under its own session
        let mut d = config(3);
        d.reviewer_kind = "claude".into();
        let mut e = Moderator::new(&d);
        typed(&mut e, Role::Host, "fix avg", "u1");
        answer(&mut e, Role::Host, "fixed avg", "u1");
        let review = take(&mut e);
        got(&mut e, &review[0], "r1");
        let probe = |e: &mut Moderator, kind, text: &str| {
            e.on_event(Event { session: Some("probe".into()), ..turn(ev(Role::Reviewer, kind, text), "x1") })
        };
        probe(&mut e, EventKind::User, "say hi");
        probe(&mut e, EventKind::Assistant, "hi");
        assert!(take(&mut e).is_empty());
        assert_eq!((e.phase, e.round), (Phase::ReviewerWorking, 0));
        // the review itself still counts
        answer(&mut e, Role::Reviewer, "fine\nVERDICT: AGREE", "r1");
        assert_eq!((e.phase, e.round), (Phase::Agreed, 1));
    }

    #[test]
    fn a_codex_internal_turn_is_not_the_hosts_answer() {
        // the hook drops Codex's internal turns; one that got through, in a thread of its
        // own, is neither the user's request nor the host's answer
        let mut d = config(3);
        (d.host_kind, d.reviewer_kind) = ("codex".into(), "claude".into());
        let mut e = Moderator::new(&d);
        typed(&mut e, Role::Host, "fix avg", "u1");
        let other = |ev: Event| Event { session: Some("catch-up-thread".into()), ..ev };
        let catch_up = "Write a brief catch-up for a user returning to this task. Return JSON with summary and nullable next_action.";
        e.on_event(other(turn(ev(Role::Host, EventKind::User, catch_up), "c1")));
        e.on_event(other(turn(ev(Role::Host, EventKind::Assistant, "{\"summary\":\"...\"}"), "c1")));
        assert!(take(&mut e).is_empty());
        assert_eq!(e.user_prompt, "fix avg");
        answer(&mut e, Role::Host, "fixed avg", "u1");
        let out = take(&mut e);
        assert!(out[0].text.starts_with("[Codex · host]: fixed avg"));
    }

    #[test]
    fn a_codex_side_talk_after_a_failed_review_turn_stays_in_the_room() {
        // the review turn hit the usage limit (no notify); the user asks Codex alone whether
        // it is stuck; the next notify lists both, and the hook reports both under its id
        let mut e = Moderator::new(&config(3));
        typed(&mut e, Role::Host, "fix avg", "u1");
        answer(&mut e, Role::Host, "fixed avg", "u1");
        let review = take(&mut e);
        e.on_room_input(Target::Reviewer, "are you stuck?");
        let side = take(&mut e);
        assert!(side[0].text.contains("the host sees neither"));
        got(&mut e, &review[0], "t2");
        got(&mut e, &side[0], "t2");
        answer(&mut e, Role::Reviewer, "PRIVATE no, the API failed", "t2");
        assert!(take(&mut e).is_empty());
        assert_eq!((e.phase, e.round), (Phase::ReviewerWorking, 0));
        assert!(e.open[1].is_empty(), "the failed turn's prompt is closed with the turn that listed it");
    }

    #[test]
    fn messages_that_joined_a_codex_turn_are_answered_together() {
        // the review turn ended without a notify (a usage limit); next morning the user typed
        // into Codex's pane and Codex answered with the review. The hook reports both inputs
        // under the turn that answered them, and the reply is the review.
        let mut e = Moderator::new(&config(3));
        typed(&mut e, Role::Host, "fix avg", "u1");
        answer(&mut e, Role::Host, "fixed avg", "u1");
        let review = take(&mut e);
        got(&mut e, &review[0], "t2");
        typed(&mut e, Role::Reviewer, "yoorumla bro", "t2");
        answer(&mut e, Role::Reviewer, "O1 avg of []\nVERDICT: DISAGREE", "t2");
        assert_eq!((e.phase, e.round), (Phase::HostWorking, 1));
        assert_eq!(take(&mut e)[0].to, Role::Host);
        assert!(e.open[1].is_empty());
    }

    #[test]
    fn a_duplicate_of_an_earlier_round_is_not_the_next_round() {
        // the paste stalled and the room typed the review twice; Codex answers both in turn,
        // and the host is quicker with its reply than Codex with the duplicate
        let mut e = Moderator::new(&config(3));
        typed(&mut e, Role::Host, "fix avg", "u1");
        answer(&mut e, Role::Host, "fixed avg", "u1");
        let first = take(&mut e);
        got(&mut e, &first[0], "r1");
        answer(&mut e, Role::Reviewer, "O1 empty list\nVERDICT: DISAGREE", "r1");
        let objections = take(&mut e);
        got(&mut e, &objections[0], "h1");
        answer(&mut e, Role::Host, "fixed O1", "h1");
        let second = take(&mut e);
        assert!(second[0].text.contains("round 2/3"));
        // the duplicate of round 1's prompt is answered now: an earlier round, not this one
        got(&mut e, &first[0], "r2");
        answer(&mut e, Role::Reviewer, "O1 empty list\nVERDICT: DISAGREE", "r2");
        assert!(take(&mut e).is_empty());
        assert_eq!((e.phase, e.round), (Phase::ReviewerWorking, 1));
        got(&mut e, &second[0], "r3");
        answer(&mut e, Role::Reviewer, "fixed\nVERDICT: AGREE", "r3");
        assert_eq!((e.phase, e.round), (Phase::Agreed, 2));
    }

    #[test]
    fn an_agent_that_queues_messages_keeps_each_prompt_until_it_answers_it() {
        // Codex answers each message in its own turn: an answer closes its own prompt only
        let mut e = Moderator::new(&config(3));
        typed(&mut e, Role::Reviewer, "PRIVATE is avg safe?", "p");
        typed(&mut e, Role::Host, "fix avg", "u1");
        answer(&mut e, Role::Host, "fixed avg", "u1");
        let review = take(&mut e);
        got(&mut e, &review[0], "r");
        // the review is answered first; the private prompt stays open
        answer(&mut e, Role::Reviewer, "fine\nVERDICT: AGREE", "r");
        assert_eq!((e.phase, e.round), (Phase::Agreed, 1));
        take(&mut e);
        assert_eq!(e.open[1].len(), 1);
        answer(&mut e, Role::Reviewer, "PRIVATE no, it divides by zero", "p");
        assert!(take(&mut e).is_empty());
        assert!(e.open[1].is_empty());
        assert!(!e.log.last().unwrap().text.contains("names no prompt"));
    }

    #[test]
    fn mentions_pick_the_recipient() {
        let e = Moderator::new(&config(3));
        assert_eq!((e.nick(Role::Host), e.nick(Role::Reviewer)), ("claude".into(), "codex".into()));
        assert_eq!(e.mentioned("@Claude stop arguing"), Some(Target::Host));
        assert_eq!(e.mentioned("ask @codex's opinion"), Some(Target::Reviewer));
        assert_eq!(e.mentioned("@claude @codex both of you"), Some(Target::Both));
        assert_eq!(e.mentioned("@all stop"), Some(Target::Both));
        assert_eq!(e.mentioned("@reviewer look"), Some(Target::Reviewer));
        assert_eq!(e.mentioned("let claude and codex talk"), None);
        assert_eq!(e.mentioned("mail@claude.ai"), None);

        // two Claudes: the roles tell them apart, the shared name reaches both
        let mut d = config(3);
        d.reviewer_kind = "claude".into();
        let e = Moderator::new(&d);
        assert_eq!((e.nick(Role::Host), e.nick(Role::Reviewer)), ("host".into(), "reviewer".into()));
        assert_eq!(e.mentioned("@host fix it"), Some(Target::Host));
        assert_eq!(e.mentioned("@claude"), Some(Target::Both));

        // picked names sit beside the roles and the agents' own names
        d.host_nick = Some("Ada".into());
        d.reviewer_nick = Some("max".into());
        let mut e = Moderator::new(&d);
        assert_eq!((e.nick(Role::Host), e.nick(Role::Reviewer)), ("ada".into(), "max".into()));
        assert_eq!(e.mentioned("ask @max's view"), Some(Target::Reviewer));
        assert_eq!(e.mentioned("@ADA"), Some(Target::Host));
        assert_eq!(e.mentioned("@reviewer"), Some(Target::Reviewer));
        e.on_room_input(Target::Both, "@max take a look");
        let out = take(&mut e);
        let to_reviewer = &out.iter().find(|o| o.to == Role::Reviewer).unwrap().text;
        assert!(to_reviewer.starts_with("[User · room]: @max take a look"));
        assert!(to_reviewer.ends_with("In this room you are @max; Claude is @ada. Reply in the language the user writes in."));
    }

    #[test]
    fn completions_offer_names_and_commands() {
        let words = |e: &Moderator, input: &str| e.completions(input).into_iter().map(|c| c.word).collect::<Vec<_>>();
        let e = Moderator::new(&config(3));
        assert_eq!(words(&e, "@ho"), ["@host"]);
        assert_eq!(words(&e, "look @C"), ["@claude", "@codex"]);
        assert_eq!(words(&e, "@"), ["@claude", "@codex", "@host", "@reviewer", "@all"]);
        // a finished word offers nothing
        assert!(words(&e, "@codex").is_empty());
        assert!(words(&e, "@codex ").is_empty());
        // commands only as the whole input
        assert_eq!(words(&e, "/ex"), ["/exit"]);
        assert_eq!(words(&e, "/"), ["/exit", "/swap", "/rebind", "/review"]);
        assert_eq!(words(&e, "/rev"), ["/review"]);
        assert_eq!(e.completions("/s")[0].about, "switch roles: Codex hosts, Claude reviews");
        assert!(words(&e, "look /ex").is_empty());

        let mut d = config(3);
        d.reviewer_kind = "claude".into();
        let e = Moderator::new(&d);
        assert_eq!(words(&e, "@"), ["@host", "@reviewer", "@claude", "@all"]);
        assert_eq!(e.completions("@cl")[0].about, "both agents");
        d.host_nick = Some("ada".into());
        d.reviewer_nick = Some("max".into());
        let e = Moderator::new(&d);
        assert_eq!(words(&e, "@"), ["@ada", "@max", "@host", "@reviewer", "@claude", "@all"]);
    }

    #[test]
    fn room_message_to_both_and_pause() {
        let mut e = Moderator::new(&config(3));
        e.on_room_input(Target::Both, "refresh later");
        let out = take(&mut e);
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|o| o.text == "[User · room]: refresh later\n\n---\n(room) Reply in the language the user writes in."));
        got(&mut e, out.iter().find(|o| o.to == Role::Host).unwrap(), "h1");
        e.toggle_pause();
        answer(&mut e, Role::Host, "ok", "h1");
        assert!(take(&mut e).is_empty());
        e.toggle_pause();
        assert_eq!(take(&mut e)[0].to, Role::Reviewer);
    }

    #[test]
    fn task_notifications_are_not_the_user() {
        let mut d = config(3);
        d.reviewer_kind = "claude".into();
        let mut e = Moderator::new(&d);
        let note = "<task-notification>\n<task-id>b1</task-id>\n<status>completed</status>\n</task-notification>";
        typed(&mut e, Role::Host, "make avg handle []", "u1");
        // the host's own background work reports back mid-turn, as a prompt of its own
        typed(&mut e, Role::Host, note, "n1");
        assert_eq!((e.phase, e.user_prompt.as_str()), (Phase::HostWorking, "make avg handle []"));
        assert_eq!(e.log.iter().filter(|m| m.author == Author::User).count(), 1);
        answer(&mut e, Role::Host, "done", "n1");
        let out = take(&mut e);
        assert!(out[0].text.contains("The user's request: \"make avg handle []\"") && !out[0].text.contains("task-notification"));
        // nor is a reviewer's notification the user speaking as the reviewer
        got(&mut e, &out[0], "r1");
        typed(&mut e, Role::Reviewer, note, "n2");
        assert!(take(&mut e).is_empty());
        assert_eq!(e.phase, Phase::ReviewerWorking);
        // and one after the review starts nothing
        answer(&mut e, Role::Reviewer, "ok\nVERDICT: AGREE", "n2");
        typed(&mut e, Role::Host, note, "n3");
        assert_eq!((e.phase, e.round), (Phase::Agreed, 1));
    }

    #[test]
    fn a_message_added_mid_turn_joins_the_request() {
        let mut e = Moderator::new(&config(3));
        typed(&mut e, Role::Host, "fix avg", "u1");
        typed(&mut e, Role::Host, "and add a test", "u2");
        assert_eq!((e.phase, e.round), (Phase::HostWorking, 0));
        assert_eq!(e.log.iter().filter(|m| m.author == Author::User).count(), 2);
        answer(&mut e, Role::Host, "fixed, with a test", "u2");
        let review = take(&mut e);
        assert!(review[0].text.contains("The user's request: \"fix avg\n\nand add a test\""));
        // once the host has answered, the user's next message is a request of its own
        got(&mut e, &review[0], "r1");
        answer(&mut e, Role::Reviewer, "O1\nVERDICT: DISAGREE", "r1");
        take(&mut e);
        typed(&mut e, Role::Host, "now the median", "u3");
        assert_eq!((e.round, e.user_prompt.as_str()), (0, "now the median"));
    }

    #[test]
    fn switching_roles_swaps_the_agents_and_starts_afresh() {
        let mut e = Moderator::new(&config(3));
        typed(&mut e, Role::Host, "make avg handle []", "u1");
        answer(&mut e, Role::Host, "done", "u1");
        let review = take(&mut e);
        got(&mut e, &review[0], "r1");
        answer(&mut e, Role::Reviewer, "ok\nVERDICT: AGREE", "r1");
        assert_eq!((e.phase, e.round), (Phase::Agreed, 1));
        take(&mut e);

        e.swap_roles();
        assert_eq!((e.name(Role::Host), e.name(Role::Reviewer)), ("Codex", "Claude"));
        assert_eq!((e.kind(Role::Host), e.kind(Role::Reviewer)), ("codex", "claude"));
        assert_eq!((e.nick(Role::Host), e.nick(Role::Reviewer)), ("codex".into(), "claude".into()));
        assert_eq!(e.mentioned("@codex fix it"), Some(Target::Host));
        assert_eq!(e.mentioned("@reviewer look"), Some(Target::Reviewer));
        assert_eq!((e.phase, e.round), (Phase::WaitingUser, 0));
        // each agent hears its new role; the echo starts nothing and the replies stay in the room
        let out = take(&mut e);
        assert_eq!(out.iter().map(|o| o.to).collect::<Vec<_>>(), [Role::Host, Role::Reviewer]);
        assert!(out[0].text.starts_with("(room) Role: host.") && out[0].text.contains("Claude reviews"));
        assert!(out[1].text.starts_with("(room) Role: reviewer.") && out[1].text.contains("Do not create, edit or delete"));
        got(&mut e, &out[0], "h1");
        answer(&mut e, Role::Host, "ok, hosting", "h1");
        got(&mut e, &out[1], "r2");
        answer(&mut e, Role::Reviewer, "ok, reviewing", "r2");
        assert!(take(&mut e).is_empty());
        assert_eq!(e.phase, Phase::WaitingUser);
        // messages written before the switch keep the agents that wrote them
        let by_claude = e.log.iter().find(|m| m.text == "done").unwrap();
        assert!(by_claude.author == Author::Host && by_claude.swapped != e.swapped);

        // the user's next request to Codex is reviewed by Claude
        typed(&mut e, Role::Host, "add a median", "u2");
        answer(&mut e, Role::Host, "added median", "u2");
        let out = take(&mut e);
        assert_eq!(out[0].to, Role::Reviewer);
        assert!(out[0].text.starts_with("[Codex · host]: added median"));
        assert!(out[0].text.contains("Codex is the host") && out[0].text.contains("add a median"));
        got(&mut e, &out[0], "r3");
        answer(&mut e, Role::Reviewer, "O1 empty list\nVERDICT: DISAGREE", "r3");
        let out = take(&mut e);
        assert!(out[0].to == Role::Host && out[0].text.starts_with("[Claude · reviewer]: O1"));
        assert_eq!((e.phase, e.round), (Phase::HostWorking, 1));
    }

    #[test]
    fn picked_names_stay_with_their_agents() {
        let mut d = config(3);
        d.host_nick = Some("ada".into());
        d.reviewer_nick = Some("max".into());
        let mut e = Moderator::new(&d);
        e.swap_roles();
        assert_eq!((e.nick(Role::Host), e.nick(Role::Reviewer)), ("max".into(), "ada".into()));
        assert_eq!(e.mentioned("@ada look"), Some(Target::Reviewer));
        // a room that opens switched, as its config says
        d.swapped = true;
        let e = Moderator::new(&d);
        assert_eq!((e.name(Role::Host), e.nick(Role::Host)), ("Codex", "max".into()));
        assert_eq!((e.name(Role::Reviewer), e.nick(Role::Reviewer)), ("Claude", "ada".into()));
    }

    #[test]
    fn a_switch_waits_until_nothing_is_in_progress() {
        let mut e = Moderator::new(&config(3));
        assert!(e.can_swap());
        typed(&mut e, Role::Host, "x", "u1");
        assert!(!e.can_swap(), "the host is working");
        answer(&mut e, Role::Host, "answer", "u1");
        assert!(!e.can_swap(), "the reviewer is reviewing");
        let review = take(&mut e);
        got(&mut e, &review[0], "r1");
        answer(&mut e, Role::Reviewer, "fine\nVERDICT: AGREE", "r1");
        assert!(e.can_swap());
        // a prompt held back by a pause
        e.toggle_pause();
        e.on_room_input(Target::Host, "one more thing");
        assert!(!e.can_swap());
        e.toggle_pause();
        assert!(e.can_swap(), "released: the room waits for its delivery itself");
        let side = take(&mut e);
        // background work that brings an agent back for more of its turn
        got(&mut e, &side[0], "h1");
        e.on_event(waiting(Role::Host, "started a check", 1, "h1"));
        assert!(!e.can_swap());
        answer(&mut e, Role::Host, "check done", "h1");
        assert!(e.can_swap());
        // an ended review does not hold a switch, and the switch reviews again
        typed(&mut e, Role::Host, "y", "u2");
        e.end();
        assert!(e.can_swap());
        e.swap_roles();
        take(&mut e);
        typed(&mut e, Role::Host, "z", "u3");
        answer(&mut e, Role::Host, "done", "u3");
        assert_eq!(take(&mut e)[0].to, Role::Reviewer);
    }

    /// Plays the room around a moderator: hands each prompt to the agent it reaches, which
    /// changes role at each switch, and lets each agent answer its prompts in order, each
    /// under a turn id of its own as the agents' hooks report them.
    struct Play {
        e: Moderator,
        /// Prompts Claude (who opened the room) got, then Codex (whom the room started).
        claude: Vec<String>,
        codex: Vec<String>,
        /// Per role: the prompts not yet answered.
        waiting: [std::collections::VecDeque<String>; 2],
        turns: u64,
    }

    impl Play {
        fn new() -> Play {
            Play { e: Moderator::new(&config(3)), claude: vec![], codex: vec![], waiting: Default::default(), turns: 0 }
        }

        fn id(&mut self) -> String {
            self.turns += 1;
            format!("t{}", self.turns)
        }

        fn flush(&mut self) {
            for o in take(&mut self.e) {
                let i = (o.to == Role::Reviewer) as usize;
                self.waiting[i].push_back(o.text.clone());
                if (o.to == Role::Host) != self.e.swapped {
                    self.claude.push(o.text);
                } else {
                    self.codex.push(o.text);
                }
            }
        }

        fn room(&mut self, to: Target, text: &str) {
            self.e.on_room_input(to, text);
            self.flush();
        }

        /// The agent in `role` takes the oldest prompt it got, and answers it.
        fn answer(&mut self, role: Role, text: &str) {
            let prompt = self.waiting[(role == Role::Reviewer) as usize].pop_front().expect("a prompt to answer");
            let id = self.id();
            self.e.on_event(turn(ev(role, EventKind::User, &prompt), &id));
            self.e.on_event(turn(ev(role, EventKind::Assistant, text), &id));
            self.flush();
        }

        /// The user asks the host in its own pane, and the host answers.
        fn request(&mut self, text: &str, answer: &str) {
            let id = self.id();
            self.e.on_event(turn(ev(Role::Host, EventKind::User, text), &id));
            self.e.on_event(turn(ev(Role::Host, EventKind::Assistant, answer), &id));
            self.flush();
        }

        fn swap(&mut self) {
            self.e.swap_roles();
            self.flush();
            self.answer(Role::Host, "ok, hosting");
            self.answer(Role::Reviewer, "ok, reviewing");
        }
    }

    #[test]
    fn private_messages_stay_private_across_switches() {
        let mut p = Play::new();
        // Claude hosts: a private question each, and a message for both
        p.room(Target::Host, "SECRET-H-Q is avg slow?");
        p.answer(Role::Host, "SECRET-H-A yes");
        p.room(Target::Reviewer, "SECRET-R-Q any bugs?");
        p.answer(Role::Reviewer, "SECRET-R-A one\nVERDICT: DISAGREE");
        p.room(Target::Both, "BOTH-Q thoughts on the cache?");
        p.answer(Role::Reviewer, "BOTH-R-A the cache leaks");
        p.answer(Role::Host, "the cache is fine");
        p.answer(Role::Reviewer, "fine\nVERDICT: AGREE");
        assert_eq!(p.e.phase, Phase::Agreed);
        // the host hears the verdict each time, and its reply stays in the room
        p.answer(Role::Host, "noted");

        // Codex hosts: a private question each, then a request and its review both ways
        p.swap();
        p.room(Target::Host, "SECRET2-H-Q why this way?");
        p.answer(Role::Host, "SECRET2-H-A simpler");
        p.room(Target::Reviewer, "SECRET2-R-Q anything urgent?");
        p.answer(Role::Reviewer, "SECRET2-R-A no");
        p.request("fix avg", "fixed avg");
        p.answer(Role::Reviewer, "O1 empty list\nVERDICT: DISAGREE");
        p.answer(Role::Host, "fixed O1");
        p.answer(Role::Reviewer, "good\nVERDICT: AGREE");
        assert_eq!((p.e.phase, p.e.round), (Phase::Agreed, 2));
        p.answer(Role::Host, "noted");

        // and back: Claude hosts again
        p.swap();
        p.request("add tests", "added tests");
        p.answer(Role::Reviewer, "O1 no edge case\nVERDICT: DISAGREE");
        p.answer(Role::Host, "added it");
        p.answer(Role::Reviewer, "good\nVERDICT: AGREE");
        assert_eq!((p.e.phase, p.e.round), (Phase::Agreed, 2));
        p.answer(Role::Host, "noted");
        assert!(p.waiting.iter().all(|w| w.is_empty()));

        let has = |got: &[String], words: &[&str]| got.iter().any(|t| words.iter().any(|w| t.contains(w)));
        // what each agent was told and answered alone, the reviewer's reply to a message for both included
        assert!(!has(&p.codex, &["SECRET-H", "SECRET2-R"]));
        assert!(!has(&p.claude, &["SECRET-R", "SECRET2-H", "BOTH-R-A"]));
        // each private question did reach its own agent, and the reviews did go both ways
        assert!(has(&p.claude, &["SECRET-H-Q"]) && has(&p.claude, &["SECRET2-R-Q"]));
        assert!(has(&p.codex, &["SECRET-R-Q"]) && has(&p.codex, &["SECRET2-H-Q"]));
        assert!(has(&p.claude, &["[Codex · host]: fixed avg"]) && has(&p.codex, &["[Claude · reviewer]: O1 empty list"]));
        assert!(has(&p.codex, &["[Claude · host]: added tests"]) && has(&p.claude, &["[Codex · reviewer]: O1 no edge case"]));
    }
}
