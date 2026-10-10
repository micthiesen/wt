//! The one palette every pane draws from. Nord-derived and deliberately
//! small, so panels feel coherent: a dark surface, three foreground weights,
//! and a handful of semantic hues. Status meaning lives in `badges`, never in
//! ad hoc colors at a call site.

use ratatui::style::{Color, Modifier, Style};

const fn rgb(hex: u32) -> Color {
    Color::Rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

pub(crate) const BG: Color = rgb(0x1b1d23);
/// Title bar, footer, and modal surfaces sit one step above the board.
pub(crate) const BG_ALT: Color = rgb(0x23262e);
pub(crate) const SELECTED_BG: Color = rgb(0x3b4252);
pub(crate) const BORDER: Color = rgb(0x3b4252);
/// Section rules: present but quieter than pane borders.
pub(crate) const BORDER_DIM: Color = rgb(0x2e3440);
pub(crate) const FG: Color = rgb(0xd8dee9);
pub(crate) const FG_DIM: Color = rgb(0x747b8a);
/// Body text that recedes below a colored header without dropping to
/// metadata gray (the work-status note is the tenant).
pub(crate) const FG_MID: Color = rgb(0xa6acb9);
pub(crate) const FG_BRIGHT: Color = rgb(0xeceff4);
pub(crate) const ACCENT: Color = rgb(0x88c0d0);
pub(crate) const ACCENT_ALT: Color = rgb(0x81a1c1);
/// "Working, but backgrounded": same cool family as accent, quieter.
pub(crate) const TEAL: Color = rgb(0x6a9b8e);
pub(crate) const OK: Color = rgb(0xa3be8c);
pub(crate) const WARN: Color = rgb(0xebcb8b);
pub(crate) const ERR: Color = rgb(0xbf616a);
pub(crate) const INFO: Color = rgb(0xb48ead);
pub(crate) const CLAUDE: Color = rgb(0xc47b3a);
pub(crate) const CODEX: Color = rgb(0x4d56d6);
/// Amber, the indigo complement, so Codex `working` contrasts in hue.
pub(crate) const CODEX_ALT: Color = rgb(0xe0a94f);
pub(crate) const OPENCODE: Color = rgb(0xa78bfa);
/// Lime, the violet complement, so OpenCode `working` reads as its own hue.
pub(crate) const OPENCODE_ALT: Color = rgb(0x9ccf6e);

pub(crate) fn fg(color: Color) -> Style {
    Style::new().fg(color)
}

pub(crate) fn dim() -> Style {
    fg(FG_DIM)
}

pub(crate) fn bold(color: Color) -> Style {
    Style::new().fg(color).add_modifier(Modifier::BOLD)
}

/// Connector color for a stack lane. Lane 0 (the spine, and every linear
/// stack) stays dim; forked sibling lanes take distinct hues that avoid the
/// ok/err status colors so a rail tint never reads as a state.
pub(crate) fn lane_color(lane: u8) -> Color {
    const PALETTE: [Color; 4] = [INFO, TEAL, ACCENT_ALT, WARN];
    if lane == 0 {
        FG_DIM
    } else {
        PALETTE[usize::from(lane - 1) % PALETTE.len()]
    }
}
