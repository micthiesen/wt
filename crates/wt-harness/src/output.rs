//! Shared bounded output-tail values and formatting helpers for live harnesses.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HarnessOutputKind {
    Info,
    User,
    Assistant,
    Thinking,
    Tool,
    ToolOk,
    ToolError,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessOutputLine {
    pub id: u64,
    pub timestamp_ms: i64,
    pub kind: HarnessOutputKind,
    pub text: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HarnessOutputTarget {
    pub slug: String,
    pub cwd: std::path::PathBuf,
    pub session_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HarnessOutputUpdate {
    pub slug: String,
    pub session_id: String,
    /// Clear the old visible tail before appending this update.
    pub reset: bool,
    pub append: Vec<HarnessOutputLine>,
}

pub(crate) const MAX_LINES_PER_BLOCK: usize = 8;
pub(crate) const MAX_LINE_CHARS: usize = 240;

pub(crate) fn text_lines(
    text: &str,
    kind: HarnessOutputKind,
    timestamp_ms: i64,
    next_id: &mut impl FnMut() -> u64,
    prefix: &str,
) -> Vec<HarnessOutputLine> {
    let pieces = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>();
    let shown = pieces.len().min(MAX_LINES_PER_BLOCK);
    let mut lines = Vec::with_capacity(shown + usize::from(pieces.len() > shown));
    for (index, piece) in pieces.iter().take(shown).enumerate() {
        let lead = if index == 0 {
            prefix
        } else if prefix.is_empty() {
            ""
        } else {
            "  "
        };
        lines.push(one_line(
            &format!("{lead}{piece}"),
            kind,
            timestamp_ms,
            next_id,
        ));
    }
    let hidden = pieces.len().saturating_sub(shown);
    if hidden > 0 {
        lines.push(one_line(
            &format!(
                "  …{hidden} more line{}",
                if hidden == 1 { "" } else { "s" }
            ),
            HarnessOutputKind::Info,
            timestamp_ms,
            next_id,
        ));
    }
    lines
}

pub(crate) fn one_line(
    text: &str,
    kind: HarnessOutputKind,
    timestamp_ms: i64,
    next_id: &mut impl FnMut() -> u64,
) -> HarnessOutputLine {
    let trimmed = text.trim_end();
    let clipped = if trimmed.chars().count() > MAX_LINE_CHARS {
        let mut value = trimmed.chars().take(MAX_LINE_CHARS - 1).collect::<String>();
        value.push('…');
        value
    } else {
        trimmed.to_owned()
    };
    HarnessOutputLine {
        id: next_id(),
        timestamp_ms,
        kind,
        text: clipped,
    }
}
