//! Shared colors: each agent keeps one color everywhere (Claude orange,
//! Codex blue), the user is purple.

use ratatui::style::Color;

pub const CLAUDE: Color = Color::Rgb(240, 149, 106);
pub const CODEX: Color = Color::Rgb(108, 182, 245);
pub const USER: Color = Color::Rgb(196, 161, 245);
pub const MUTED: Color = Color::Rgb(155, 164, 174);
pub const OK: Color = Color::Rgb(111, 208, 140);
pub const ERR: Color = Color::Rgb(242, 122, 122);
pub const WARN: Color = Color::Rgb(232, 196, 106);
pub const BORDER: Color = Color::Rgb(58, 65, 75);

// The setup popup's card, after the design canvas.
pub const CARD: Color = Color::Rgb(26, 30, 36);
pub const FIELD: Color = Color::Rgb(15, 18, 22);
pub const LINE: Color = Color::Rgb(42, 47, 54);
pub const TEXT: Color = Color::Rgb(230, 232, 235);
/// Text on a filled button or toggle.
pub const INK: Color = Color::Rgb(11, 13, 16);

pub fn agent_color(kind: &str) -> Color {
    match kind {
        "claude" => CLAUDE,
        "codex" => CODEX,
        _ => Color::White,
    }
}

pub fn status_label(status: &str) -> (&'static str, Color) {
    match status {
        "idle" => ("waiting", MUTED),
        "done" => ("done", MUTED),
        "working" => ("working", OK),
        "blocked" => ("⚠ needs approval", WARN),
        "" => ("starting…", MUTED),
        _ => ("?", MUTED),
    }
}
