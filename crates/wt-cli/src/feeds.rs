//! The board's activity and attention feeds: appending lines, keeping them
//! sorted, deduplicated and bounded, and timestamping lasting conditions.

use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

use wt_tui::{ActivityLine, AttentionLine};

pub const MAX_ACTIVITY: usize = 500;
pub const MAX_ATTENTION: usize = 200;
pub const REPEAT_WINDOW_MS: u64 = 5_000;

/// First-seen times of the status lines a derived stage adds on every
/// compose. Stages rebuild from their upstream board, so stamping a lasting
/// failure with the current time would make it new on each pass: it could
/// never be acknowledged and every pass would republish the board. A line
/// keeps its time while each compose reports it; once a compose omits it
/// (`sweep`), a recurrence is a new event.
#[derive(Clone, Debug, Default)]
pub struct FailureClock {
    since: HashMap<(&'static str, String, String), u64>,
    seen: HashSet<(&'static str, String, String)>,
}

impl FailureClock {
    /// The time to show for this line in the current compose.
    pub fn at(&mut self, channel: &'static str, source: &str, text: &str) -> u64 {
        let key = (
            channel,
            wt_core::sanitize_terminal_text(source),
            wt_core::sanitize_terminal_text(text),
        );
        let at_ms = *self.since.entry(key.clone()).or_insert_with(epoch_ms);
        self.seen.insert(key);
        at_ms
    }

    /// Report a lasting condition on the attention feed.
    pub fn attention(&mut self, board: &mut wt_tui::Board, source: &str, text: &str) {
        let at_ms = self.at("attention", source, text);
        append_attention_at(board, at_ms, source, text);
    }

    /// Report a lasting condition on the activity feed.
    pub fn activity(&mut self, board: &mut wt_tui::Board, level: &str, source: &str, text: &str) {
        let at_ms = self.at("activity", source, text);
        append_activity_at(board, at_ms, level, source, text);
    }

    /// End one compose: lines it did not report have cleared.
    pub fn sweep(&mut self) {
        let seen = std::mem::take(&mut self.seen);
        self.since.retain(|key, _| seen.contains(key));
    }
}

pub fn epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

pub fn append_activity_at(
    board: &mut wt_tui::Board,
    at_ms: u64,
    level: &str,
    source: &str,
    text: &str,
) {
    let event = ActivityLine {
        at_ms,
        level: level.into(),
        channel: "activity".into(),
        source: wt_core::sanitize_terminal_text(source),
        text: wt_core::sanitize_terminal_text(text),
    };
    if !board.activity.iter().rev().take(20).any(|previous| {
        previous.source == event.source
            && previous.text == event.text
            && event.at_ms.saturating_sub(previous.at_ms) <= 5_000
    }) {
        board.activity.push(event);
    }
    bound_feeds(board);
}

pub fn append_attention_at(board: &mut wt_tui::Board, at_ms: u64, source: &str, text: &str) {
    let source = wt_core::sanitize_terminal_text(source);
    let text = wt_core::sanitize_terminal_text(text);
    if board.attention.iter().rev().take(20).any(|previous| {
        previous.source == source
            && previous.text == text
            && at_ms.saturating_sub(previous.at_ms) <= 5_000
    }) {
        return;
    }
    board.activity.push(ActivityLine {
        at_ms,
        level: "ERROR".into(),
        channel: "attention".into(),
        source: source.clone(),
        text: text.clone(),
    });
    board.attention.push(AttentionLine {
        at_ms,
        source,
        text,
    });
    bound_feeds(board);
}

pub fn bound_feeds(board: &mut wt_tui::Board) {
    board.activity.sort_by_key(|event| event.at_ms);
    board.attention.sort_by_key(|event| event.at_ms);
    // Sorted feeds hold the same line twice when the in-memory copy and
    // its log backfill meet, or when several processes report one event.
    // A burst inside the window is one event; an attention line identical
    // to the one directly above it adds nothing whatever the gap.
    board.activity.dedup_by(|later, earlier| {
        later.channel == earlier.channel
            && later.source == earlier.source
            && later.text == earlier.text
            && later.at_ms.saturating_sub(earlier.at_ms) <= REPEAT_WINDOW_MS
    });
    collapse_repeats(&mut board.attention);
    if board.activity.len() > MAX_ACTIVITY {
        board.activity.drain(..board.activity.len() - MAX_ACTIVITY);
    }
    if board.attention.len() > MAX_ATTENTION {
        board
            .attention
            .drain(..board.attention.len() - MAX_ATTENTION);
    }
}

/// Consecutive identical attention lines collapse into the most recent one,
/// so a failure that recurs reads with its latest time, not its first.
pub fn collapse_repeats(attention: &mut Vec<AttentionLine>) {
    let mut kept: Vec<AttentionLine> = Vec::with_capacity(attention.len());
    for line in attention.drain(..) {
        match kept.last_mut() {
            Some(last) if last.source == line.source && last.text == line.text => *last = line,
            _ => kept.push(line),
        }
    }
    *attention = kept;
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn a_lasting_failure_keeps_its_first_time_until_a_compose_omits_it() {
        let mut clock = FailureClock::default();
        let compose = |clock: &mut FailureClock, failing: bool| {
            let mut board = wt_tui::Board::default();
            if failing {
                clock.attention(&mut board, "History", "History: database locked");
            }
            clock.sweep();
            board
        };
        let first = compose(&mut clock, true);
        std::thread::sleep(Duration::from_millis(3));
        // Recomposing an unchanged failure yields an identical board, so
        // `publish_if_changed` stays quiet and an acknowledgement sticks.
        assert_eq!(compose(&mut clock, true), first);
        compose(&mut clock, false);
        std::thread::sleep(Duration::from_millis(3));
        let recurred = compose(&mut clock, true);
        assert!(recurred.attention[0].at_ms > first.attention[0].at_ms);
    }
}
