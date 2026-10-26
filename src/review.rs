//! The moderator of a review room, free of herdr and the terminal: events in, the
//! room's log and outgoing prompts out.

use crate::state::{RoomConfig, Event, EventKind, Role};

pub const USER_ROOM_TAG: &str = "[User · room]:";
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

impl Phase {
    pub fn is_finished(self) -> bool {
        matches!(self, Phase::Agreed | Phase::NoConsensus | Phase::Ended)
    }
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
    UserAsReviewer,
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

/// Typed alone in the room: ends the review and closes the reviewer's pane and the room.
pub const EXIT_COMMAND: &str = "/exit";

/// Every prompt the room writes ends with this. The room's own words are English, so
/// without it an agent would answer them in English whatever language the user writes in.
const IN_USER_LANGUAGE: &str = "Reply in the language the user writes in.";

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
    /// Background tasks the host's last answer said it is still waiting on.
    pub host_waiting: u32,
    /// The host's answers so far in a turn that background work keeps alive.
    host_partial: Vec<String>,
    /// Prompts held back while paused, sent in order on resume; `true` marks a side prompt.
    held: Vec<(Outgoing, bool)>,
    /// Reviewer prompts handed to the outbox so far: the next one's number in its queue.
    reviewer_sent: u64,
    /// The numbers of reviewer prompts that carry a room message sent to both agents, not
    /// yet delivered: the reviewer's reply to one is only logged, while the host's reply
    /// goes on to review.
    side_prompts: Vec<u64>,
    /// The reviewer's current turn answers one of `side_prompts`.
    reviewer_side_turn: bool,
}

impl Moderator {
    pub fn new(d: &RoomConfig) -> Moderator {
        Moderator {
            host_name: display_name(&d.host_kind),
            reviewer_name: display_name(&d.reviewer_kind),
            max_rounds: d.max_rounds.max(1),
            reviewer_read_only: d.reviewer_read_only,
            host_nick: d.host_nick.as_ref().map(|n| n.to_lowercase()),
            reviewer_nick: d.reviewer_nick.as_ref().map(|n| n.to_lowercase()),
            host_kind: d.host_kind.clone(),
            reviewer_kind: d.reviewer_kind.clone(),
            round: 0,
            phase: Phase::WaitingUser,
            paused: false,
            user_prompt: String::new(),
            log: vec![],
            outbox: vec![],
            host_waiting: 0,
            host_partial: vec![],
            held: vec![],
            reviewer_sent: 0,
            side_prompts: vec![],
            reviewer_side_turn: false,
        }
    }

    fn host_tag(&self) -> String {
        format!("[{} · host]:", self.host_name)
    }

    fn reviewer_tag(&self) -> String {
        format!("[{} · reviewer]:", self.reviewer_name)
    }

    /// Prompts the room itself typed into an agent come back through that agent's hook.
    fn is_injected(&self, text: &str) -> bool {
        let t = text.trim_start();
        [self.host_tag(), self.reviewer_tag(), USER_ROOM_TAG.into(), USER_AS_REVIEWER_TAG.into()]
            .iter()
            .any(|tag| t.starts_with(tag.as_str()))
    }

    pub fn system(&mut self, text: impl Into<String>) {
        self.push(Author::System, text.into(), None);
    }

    fn push(&mut self, author: Author, text: String, verdict: Option<Verdict>) {
        self.log.push(Msg { ts: crate::state::now_ms(), author, text, verdict });
    }

    fn send(&mut self, out: Outgoing) {
        self.queue(out, false);
    }

    fn queue(&mut self, out: Outgoing, side: bool) {
        if self.paused {
            self.held.push((out, side));
        } else {
            self.release(out, side);
        }
    }

    /// Hands `out` to the room, which queues it behind every prompt before it, so a
    /// reviewer prompt's number here is its number in the reviewer's queue.
    fn release(&mut self, out: Outgoing, side: bool) {
        if out.to == Role::Reviewer {
            if side {
                self.side_prompts.push(self.reviewer_sent);
            }
            self.reviewer_sent += 1;
        }
        self.outbox.push(out);
    }

    fn start_review(&mut self, prompt: &str) {
        self.round = 0;
        self.user_prompt = prompt.to_string();
        self.phase = Phase::HostWorking;
        self.host_waiting = 0;
        self.host_partial.clear();
    }

    pub fn on_event(&mut self, mut ev: Event) {
        if ev.kind == EventKind::User {
            ev.text = unwrap_paste(&ev.text);
        }
        match (ev.role, ev.kind) {
            (Role::Host, EventKind::User) => {
                if self.is_injected(&ev.text) {
                    return;
                }
                self.push(Author::User, ev.text.clone(), None);
                if self.phase != Phase::Ended {
                    self.start_review(&ev.text);
                }
            }
            (Role::Host, EventKind::Assistant) => {
                self.push(Author::Host, ev.text.clone(), None);
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
                let answer = std::mem::take(&mut self.host_partial).join("\n\n");
                let text = self.to_reviewer(&answer);
                self.phase = Phase::ReviewerWorking;
                self.send(Outgoing { to: Role::Reviewer, text });
            }
            (Role::Reviewer, EventKind::User) => {
                if self.is_injected(&ev.text) {
                    return;
                }
                // The user typed straight into the reviewer's pane: they speak as the reviewer.
                self.push(Author::UserAsReviewer, ev.text.clone(), None);
                if self.phase == Phase::Ended {
                    return;
                }
                if self.phase.is_finished() {
                    self.round = 0;
                }
                self.phase = Phase::HostWorking;
                let text = format!(
                    "{USER_AS_REVIEWER_TAG} {}\n\n---\n(room) The user spoke in the reviewer's place. \
                     Weigh it and reply; your reply also goes to the reviewer, {}. {IN_USER_LANGUAGE}",
                    ev.text.trim(),
                    self.reviewer_name
                );
                self.send(Outgoing { to: Role::Host, text });
            }
            (Role::Reviewer, EventKind::Assistant) => {
                let verdict = parse_verdict(&ev.text);
                let side_turn = std::mem::take(&mut self.reviewer_side_turn);
                if side_turn || self.phase != Phase::ReviewerWorking {
                    // A side reply (to the user, say), not a review: no verdict badge unless it gave one.
                    let shown = (verdict != Verdict::Unknown).then_some(verdict);
                    self.push(Author::Reviewer, ev.text.clone(), shown);
                    return;
                }
                self.push(Author::Reviewer, ev.text.clone(), Some(verdict));
                self.round += 1;
                if verdict == Verdict::Agree {
                    self.phase = Phase::Agreed;
                    let n = self.round;
                    self.system(format!("✓ AGREED · {n} round(s)"));
                } else if self.round >= self.max_rounds {
                    self.phase = Phase::NoConsensus;
                    let (n, max) = (self.round, self.max_rounds);
                    self.system(format!(
                        "⚖ NO CONSENSUS · {n}/{max} rounds — your call. Type in the room; your decision goes to both."
                    ));
                } else {
                    let text = self.to_host(&ev.text);
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
            vec![Completion { word: EXIT_COMMAND.into(), about: "end the review, close the reviewer and the room".into() }]
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
        let (to_host, to_reviewer) = (self.room_text(Role::Host, text), self.room_text(Role::Reviewer, text));
        let (phase, outgoing) = match target {
            Target::Host => (Phase::HostWorking, vec![(Outgoing { to: Role::Host, text: to_host }, false)]),
            Target::Reviewer => (Phase::ReviewerWorking, vec![(Outgoing { to: Role::Reviewer, text: to_reviewer }, false)]),
            Target::Both => {
                // The host's answer goes on to the reviewer; the reviewer's own reply is only logged.
                let both = vec![(Outgoing { to: Role::Reviewer, text: to_reviewer }, true), (Outgoing { to: Role::Host, text: to_host }, false)];
                (Phase::HostWorking, both)
            }
        };
        // Once the user ended the review, room messages still reach the agents but start nothing.
        if self.phase != Phase::Ended {
            if self.phase.is_finished() {
                self.round = 0;
            }
            self.phase = phase;
        }
        for (out, side) in outgoing {
            self.queue(out, side);
        }
    }

    /// A room message as `role` gets it. One that calls agents by `@` name also says
    /// which name is whose, so each agent knows when it is the one spoken to.
    fn room_text(&self, role: Role, text: &str) -> String {
        let mut note = String::new();
        if self.mentioned(text).is_some() {
            let (other, other_name) = match role {
                Role::Host => (Role::Reviewer, &self.reviewer_name),
                Role::Reviewer => (Role::Host, &self.host_name),
            };
            let (me, them) = (self.nick(role), self.nick(other));
            note = format!("In this room you are @{me}; {other_name} is @{them}. ");
        }
        format!("{USER_ROOM_TAG} {text}\n\n---\n(room) {note}{IN_USER_LANGUAGE}")
    }

    /// The room is about to type prompt number `seq` of `role`'s queue into its pane; it
    /// reports again on every retry. An agent gets one prompt at a time, in order, each once
    /// its previous turn has ended, so its next answer replies to this one.
    pub fn on_delivering(&mut self, role: Role, seq: u64) {
        if role != Role::Reviewer {
            return;
        }
        // Earlier prompts are past, delivered or given up; a retry keeps this one's mark.
        self.side_prompts.retain(|&s| s >= seq);
        self.reviewer_side_turn = self.side_prompts.first() == Some(&seq);
    }

    pub fn toggle_pause(&mut self) {
        self.paused = !self.paused;
        if !self.paused {
            for (out, side) in std::mem::take(&mut self.held) {
                self.release(out, side);
            }
        }
    }

    pub fn end(&mut self) {
        self.phase = Phase::Ended;
        self.held.clear();
        self.system("Review ended. The room only keeps the log from here on.");
    }

    fn to_reviewer(&self, host_text: &str) -> String {
        let (host, n, max) = (&self.host_name, self.round + 1, self.max_rounds);
        let mut s = format!("{} {}\n\n---\n", self.host_tag(), host_text.trim());
        if self.round == 0 {
            let request = if self.user_prompt.trim().is_empty() { "(unknown)" } else { self.user_prompt.trim() };
            let hands = if self.reviewer_read_only {
                "do not modify any file"
            } else {
                "you may edit files, but list every change you make so the host sees it"
            };
            s.push_str(&format!(
                "(review room · round {n}/{max}) You are the reviewer in a review room; {host} is the host, the agent the user works with.\n\
                 The user's request: \"{request}\"\n\
                 Check the host's answer and its code changes (git diff, the files) yourself; {hands}.\n\
                 Object only to points that change the outcome: bugs, missed cases, security, wrong facts, bad design. No style nitpicks.\n\
                 Number your objections O1, O2, … and keep the same numbers in later rounds.\n\
                 Withdraw an objection when the host convinces you; never agree just to end the review.\n\
                 If the only open point is a decision for the user and the host has already asked the user, AGREE.\n"
            ));
        } else {
            s.push_str(&format!(
                "(review room · round {n}/{max}) Above is the host's reply to your objections. Re-check the code if needed.\n"
            ));
        }
        s.push_str(&format!("{IN_USER_LANGUAGE}\nThe LAST line of your reply, in English whatever the language, must be exactly one of:\nVERDICT: AGREE\nVERDICT: DISAGREE"));
        s
    }

    fn to_host(&self, reviewer_text: &str) -> String {
        let (rev, n, max) = (&self.reviewer_name, self.round + 1, self.max_rounds);
        format!(
            "{} {}\n\n---\n(review room · round {n}/{max}) {rev}, as reviewer, raised the objections above. \
             Weigh each one: where it is right, fix it; where it is wrong, push back with concrete evidence (code, test output). \
             Do not concede just to end the review. Your reply goes back to {rev}. {IN_USER_LANGUAGE}",
            self.reviewer_tag(),
            reviewer_text.trim()
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
        }
    }

    fn ev(role: Role, kind: EventKind, text: &str) -> Event {
        Event { ts: 0, role, kind, text: text.into(), background: 0 }
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
    fn pause_holds_every_message_in_order() {
        let mut e = Moderator::new(&config(3));
        e.on_event(ev(Role::Host, EventKind::User, "x"));
        e.toggle_pause();
        e.on_event(ev(Role::Host, EventKind::Assistant, "answer"));
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
        e.on_event(ev(Role::Host, EventKind::User, "x"));
        e.end();
        // room messages still reach the agents, but start no review
        e.on_room_input(Target::Host, "one more thing");
        assert_eq!(take(&mut e).len(), 1);
        e.on_event(ev(Role::Reviewer, EventKind::User, "typed in the reviewer pane"));
        e.on_event(ev(Role::Host, EventKind::Assistant, "done"));
        e.on_room_input(Target::Reviewer, "and you?");
        take(&mut e);
        e.on_event(ev(Role::Reviewer, EventKind::Assistant, "fine\nVERDICT: DISAGREE"));
        assert!(take(&mut e).is_empty());
        assert_eq!((e.phase, e.round), (Phase::Ended, 0));
    }

    #[test]
    fn a_reply_to_a_message_for_both_is_not_a_review() {
        let mut e = Moderator::new(&config(3));
        e.on_room_input(Target::Both, "what do you think of the cache?");
        let out = take(&mut e);
        assert!(out.iter().any(|o| o.to == Role::Reviewer));
        e.on_delivering(Role::Reviewer, 0);
        // the host answers first: its answer goes on to the reviewer
        e.on_event(ev(Role::Host, EventKind::Assistant, "the cache is fine"));
        assert_eq!(take(&mut e)[0].to, Role::Reviewer);
        assert_eq!(e.phase, Phase::ReviewerWorking);
        // the reviewer's reply to the room message is only logged, even with a verdict line
        // and without any echo of the prompt from its hook
        e.on_event(ev(Role::Reviewer, EventKind::Assistant, "looks fine to me\nVERDICT: DISAGREE"));
        assert_eq!((e.phase, e.round), (Phase::ReviewerWorking, 0));
        assert!(take(&mut e).is_empty());
        // its next turn, the review, counts
        e.on_delivering(Role::Reviewer, 1);
        e.on_event(ev(Role::Reviewer, EventKind::Assistant, "ok\nVERDICT: AGREE"));
        assert_eq!((e.phase, e.round), (Phase::Agreed, 1));
    }

    #[test]
    fn a_retried_prompt_keeps_its_mark() {
        let mut e = Moderator::new(&config(3));
        e.on_room_input(Target::Both, "what do you think of the cache?");
        take(&mut e);
        e.on_event(ev(Role::Host, EventKind::Assistant, "the cache is fine"));
        assert_eq!(take(&mut e)[0].to, Role::Reviewer);
        // the paste stalls twice before the reviewer takes it: three reports of prompt 0
        for _ in 0..3 {
            e.on_delivering(Role::Reviewer, 0);
        }
        e.on_event(ev(Role::Reviewer, EventKind::Assistant, "looks fine\nVERDICT: DISAGREE"));
        assert_eq!((e.phase, e.round), (Phase::ReviewerWorking, 0));
        assert!(take(&mut e).is_empty());
        // a retried review prompt still counts as the review
        e.on_delivering(Role::Reviewer, 1);
        e.on_delivering(Role::Reviewer, 1);
        e.on_event(ev(Role::Reviewer, EventKind::Assistant, "O1\nVERDICT: DISAGREE"));
        assert_eq!((e.phase, e.round), (Phase::HostWorking, 1));
    }

    #[test]
    fn a_message_for_both_that_never_arrived_marks_nothing() {
        let mut e = Moderator::new(&config(3));
        e.on_room_input(Target::Both, "wait");
        take(&mut e);
        // the reviewer never takes prompt 0; the user resends the same words to it alone
        e.on_room_input(Target::Reviewer, "wait");
        take(&mut e);
        e.on_delivering(Role::Reviewer, 1);
        e.on_event(ev(Role::Reviewer, EventKind::Assistant, "done\nVERDICT: AGREE"));
        assert_eq!((e.phase, e.round), (Phase::Agreed, 1));
    }

    #[test]
    fn messages_for_both_sent_mid_review_are_told_apart() {
        let mut e = Moderator::new(&config(3));
        e.on_event(ev(Role::Host, EventKind::User, "x"));
        e.on_event(ev(Role::Host, EventKind::Assistant, "answer"));
        assert_eq!(take(&mut e)[0].to, Role::Reviewer);
        e.on_delivering(Role::Reviewer, 0);
        // two identical messages for both while the reviewer is still reviewing
        e.on_room_input(Target::Both, "wait");
        e.on_room_input(Target::Both, "wait");
        assert_eq!(take(&mut e).iter().filter(|o| o.to == Role::Reviewer).count(), 2);
        // the review in progress ends; the messages for both set the host working, so it is only logged
        e.on_event(ev(Role::Reviewer, EventKind::Assistant, "O1\nVERDICT: DISAGREE"));
        e.on_event(ev(Role::Host, EventKind::Assistant, "ok, waiting"));
        assert_eq!(take(&mut e)[0].to, Role::Reviewer);
        assert_eq!(e.phase, Phase::ReviewerWorking);
        for seq in 1..=2 {
            e.on_delivering(Role::Reviewer, seq);
            e.on_event(ev(Role::Reviewer, EventKind::Assistant, "sure\nVERDICT: AGREE"));
            assert_eq!((e.phase, e.round), (Phase::ReviewerWorking, 0));
        }
        e.on_delivering(Role::Reviewer, 3);
        e.on_event(ev(Role::Reviewer, EventKind::Assistant, "O2\nVERDICT: DISAGREE"));
        assert_eq!((e.phase, e.round), (Phase::HostWorking, 1));
    }

    #[test]
    fn disagree_then_agree() {
        let mut e = Moderator::new(&config(3));
        e.on_event(ev(Role::Host, EventKind::User, "make avg handle an empty list"));
        assert_eq!(e.phase, Phase::HostWorking);
        e.on_event(ev(Role::Host, EventKind::Assistant, "added it"));
        let out = take(&mut e);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].to, Role::Reviewer);
        assert!(out[0].text.starts_with("[Claude · host]: added it"));
        assert!(out[0].text.contains("make avg handle an empty list"));

        // the injected prompt echoes back through the reviewer's hook and is ignored
        e.on_event(ev(Role::Reviewer, EventKind::User, &out[0].text));
        assert_eq!(e.phase, Phase::ReviewerWorking);
        e.on_event(ev(Role::Reviewer, EventKind::Assistant, "O1 an empty list crashes\nVERDICT: DISAGREE"));
        let out = take(&mut e);
        assert_eq!(out[0].to, Role::Host);
        assert!(out[0].text.starts_with("[Codex · reviewer]: O1"));
        assert_eq!(e.phase, Phase::HostWorking);

        // Claude Code wraps pasted prompts before its hooks see them
        let wrapped = format!("\n\n<pasted_content id=\"b199\">\n{}\n</pasted_content id=\"b199\">", out[0].text);
        e.on_event(ev(Role::Host, EventKind::User, &wrapped));
        assert_eq!((e.phase, e.round), (Phase::HostWorking, 1));
        e.on_event(ev(Role::Host, EventKind::Assistant, "you are right, fixed"));
        let out = take(&mut e);
        assert!(out[0].text.contains("round 2/3"));
        e.on_event(ev(Role::Reviewer, EventKind::Assistant, "good\nVERDICT: AGREE"));
        assert_eq!(e.phase, Phase::Agreed);
        assert_eq!(e.round, 2);
        assert!(take(&mut e).is_empty());
        // the user's next request starts a new review
        e.on_event(ev(Role::Host, EventKind::User, "now write the tests"));
        assert_eq!((e.phase, e.round), (Phase::HostWorking, 0));
    }

    #[test]
    fn background_work_holds_the_review_until_the_turn_ends() {
        let mut e = Moderator::new(&config(3));
        e.on_event(ev(Role::Host, EventKind::User, "review this code"));
        let waiting = |text: &str, n| Event { background: n, ..ev(Role::Host, EventKind::Assistant, text) };
        e.on_event(waiting("three reviews are running", 3));
        e.on_event(waiting("first one found a bug", 2));
        assert!(take(&mut e).is_empty());
        assert_eq!((e.phase, e.host_waiting), (Phase::HostWorking, 2));
        // one notice for the wait, not one per wake-up
        assert_eq!(e.log.iter().filter(|m| m.author == Author::System).count(), 1);

        e.on_event(ev(Role::Host, EventKind::Assistant, "all three are in"));
        let out = take(&mut e);
        assert_eq!(out.len(), 1);
        assert!(out[0].text.starts_with("[Claude · host]: three reviews are running\n\nfirst one found a bug\n\nall three are in"));
        assert_eq!((e.phase, e.host_waiting), (Phase::ReviewerWorking, 0));

        // the next round forwards the host's reply alone
        e.on_event(ev(Role::Reviewer, EventKind::Assistant, "O1\nVERDICT: DISAGREE"));
        take(&mut e);
        e.on_event(ev(Role::Host, EventKind::Assistant, "fixed"));
        assert!(take(&mut e)[0].text.starts_with("[Claude · host]: fixed\n\n---"));
    }

    #[test]
    fn first_review_states_the_reviewer_permissions() {
        let mut e = Moderator::new(&config(3));
        e.on_event(ev(Role::Host, EventKind::User, "x"));
        e.on_event(ev(Role::Host, EventKind::Assistant, "answer"));
        let text = &take(&mut e)[0].text;
        assert!(text.contains("do not modify any file") && text.contains("Reply in the language the user writes in."));

        let mut d = config(3);
        d.reviewer_read_only = false;
        let mut e = Moderator::new(&d);
        e.on_event(ev(Role::Host, EventKind::User, "x"));
        e.on_event(ev(Role::Host, EventKind::Assistant, "answer"));
        let text = &take(&mut e)[0].text;
        assert!(text.contains("you may edit files") && !text.contains("do not modify"));
    }

    #[test]
    fn round_limit_hands_the_call_to_the_user() {
        let mut e = Moderator::new(&config(2));
        e.on_event(ev(Role::Host, EventKind::User, "x"));
        for _ in 0..2 {
            e.on_event(ev(Role::Host, EventKind::Assistant, "answer"));
            e.on_event(ev(Role::Reviewer, EventKind::Assistant, "no\nVERDICT: DISAGREE"));
        }
        assert_eq!(e.phase, Phase::NoConsensus);
        assert_eq!(e.log.last().unwrap().author, Author::System);
        // a later request starts a new review
        take(&mut e);
        e.on_event(ev(Role::Host, EventKind::User, "another task"));
        assert_eq!((e.phase, e.round), (Phase::HostWorking, 0));
        e.on_event(ev(Role::Host, EventKind::Assistant, "done"));
        assert_eq!(take(&mut e)[0].to, Role::Reviewer);
    }

    #[test]
    fn user_typing_in_reviewer_pane_speaks_for_the_reviewer() {
        let mut e = Moderator::new(&config(3));
        e.on_event(ev(Role::Host, EventKind::User, "x"));
        e.on_event(ev(Role::Host, EventKind::Assistant, "answer"));
        take(&mut e);
        e.on_event(ev(Role::Reviewer, EventKind::User, "logging is missing"));
        let out = take(&mut e);
        assert_eq!(out[0].to, Role::Host);
        assert!(out[0].text.starts_with(USER_AS_REVIEWER_TAG));
        assert_eq!(e.phase, Phase::HostWorking);
        // the reviewer's own answer to the user is logged, not counted as a round
        e.on_event(ev(Role::Reviewer, EventKind::Assistant, "understood"));
        assert_eq!(e.round, 0);
        assert!(take(&mut e).is_empty());
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
        e.toggle_pause();
        e.on_event(ev(Role::Host, EventKind::Assistant, "ok"));
        assert!(take(&mut e).is_empty());
        e.toggle_pause();
        assert_eq!(take(&mut e)[0].to, Role::Reviewer);
    }
}
