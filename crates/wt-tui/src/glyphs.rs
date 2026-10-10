//! Nerd Font icons used across the TUI. Requires a Nerd-Font-patched terminal
//! font. Every private-use codepoint lives here so call sites use readable
//! names. Codepoint reference: <https://www.nerdfonts.com/cheat-sheet>.
//!
//! Width contract: Unicode width tables measure these codepoints as one cell,
//! while many patched fonts draw them into two. Every glyph slot therefore
//! reserves two cells (glyph plus a space), and glyph-then-text pairs use two
//! spaces, so the right half of an icon never overlaps the next character.

// Row status markers.
pub(crate) const ROCKET: &str = "\u{F427}"; // busy, non-destructive op
pub(crate) const TRASH: &str = "\u{F48E}"; // busy, destructive (remove)
pub(crate) const UNLINK: &str = "\u{F529}"; // worktree path vanished
pub(crate) const SLASH: &str = "\u{F468}"; // circle-slash: gone / dropped / gated
pub(crate) const MERGE: &str = "\u{F419}"; // landed on the configured base
pub(crate) const PRODUCTION: &str = "\u{F417}"; // landed on the production branch
pub(crate) const PENCIL: &str = "\u{F448}"; // uncommitted changes
pub(crate) const CLEAN: &str = "\u{F06C}"; // leaf: clean working tree
pub(crate) const DOT: &str = "\u{F111}"; // asserted work state
pub(crate) const DOT_OUTLINE: &str = "\u{F10C}"; // todo / unasserted / stale
pub(crate) const DOT_CIRCLE: &str = "\u{F192}"; // tracker: in progress
pub(crate) const HALF_CIRCLE: &str = "\u{F042}"; // tracker: review
pub(crate) const TASK_COMPLETE: &str = "\u{F058}";
pub(crate) const TASK_CANCELLED: &str = "\u{F057}";

// Pull requests.
pub(crate) const PR_OPEN: &str = "\u{F407}";
pub(crate) const PR_DRAFT: &str = "\u{F4DD}";
pub(crate) const PR_MERGED: &str = "\u{F4C9}";
pub(crate) const PR_CLOSED: &str = "\u{F4DC}";

// CI checks.
pub(crate) const CHECK_PASS: &str = "\u{F49E}";
pub(crate) const CHECK_FAIL: &str = "\u{F52F}";
pub(crate) const CHECK_PENDING: &str = "\u{F43A}";

// Human review: approved, suggestions, and waiting (pending or unrequested,
// told apart by color). The eye keeps review-pending off the CI clock.
pub(crate) const THUMBS_UP: &str = "\u{F164}";
pub(crate) const LIGHTBULB: &str = "\u{F0EB}";
pub(crate) const EYE: &str = "\u{F441}";

// Review bot. Color carries state; the carrot is CodeRabbit's, everything
// else gets the checklist. Never a robot: Claude's session glyph owns that.
pub(crate) const CARROT: &str = "\u{EF3B}";
pub(crate) const REVIEW_CHECKLIST: &str = "\u{F45E}";

// Rebase lifecycle: a red warning triangle for a conflict (distinct from the
// failed-check circle) and one sync glyph whose color carries the stage.
pub(crate) const CONFLICT: &str = "\u{F421}";
pub(crate) const RESTACK: &str = "\u{F46A}";

pub(crate) const MERGE_QUEUE: &str = "\u{F4DB}";
pub(crate) const BOLT: &str = "\u{F0E7}"; // environment live (stage deployed / dev running)
pub(crate) const BAN: &str = "\u{F05E}"; // environment not running
pub(crate) const COMMENT: &str = "\u{F41F}"; // headless action running
pub(crate) const REMOTE: &str = "\u{F048B}"; // SSH host

// Harnesses.
pub(crate) const CLAUDE: &str = "\u{F06A9}";
pub(crate) const CODEX: &str = "\u{F4AC}";
pub(crate) const OPENCODE: &str = "\u{F018D}";

/// Pause marker for automation state. Plain Unicode, renders everywhere.
pub(crate) const PAUSE: &str = "⏸";
