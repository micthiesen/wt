//! Cell-accurate text fitting. Truncation keeps the head of a string intact
//! (the distinctive part, such as an issue number and the first words of a
//! title) and marks the cut with an ellipsis.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub(crate) const ELLIPSIS: char = '…';

/// Truncate to at most `width` cells, ending in `…` when anything was cut.
pub(crate) fn truncate_end(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let cell = ch.width().unwrap_or(0);
        if used + cell > width - 1 {
            break;
        }
        out.push(ch);
        used += cell;
    }
    out.push(ELLIPSIS);
    out
}

/// Truncate to at most `width` cells by cutting the middle, so both the
/// start and the distinctive end of a path or URL stay visible.
pub(crate) fn truncate_middle(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_owned();
    }
    if width < 3 {
        return truncate_end(text, width);
    }
    let tail_width = (width - 1) / 2;
    let head = truncate_end(text, width - tail_width);
    let head = head.trim_end_matches(ELLIPSIS);
    let mut tail = Vec::new();
    let mut used = 0;
    for ch in text.chars().rev() {
        let cell = ch.width().unwrap_or(0);
        if used + cell > tail_width {
            break;
        }
        tail.push(ch);
        used += cell;
    }
    let tail: String = tail.into_iter().rev().collect();
    format!("{head}{ELLIPSIS}{tail}")
}

/// Word-wrap to `width` cells. Words longer than a line are hard-split.
/// Existing line breaks are preserved; blank lines stay blank.
pub(crate) fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    for paragraph in text.split('\n') {
        let mut line = String::new();
        let mut used = 0;
        for word in paragraph.split(' ') {
            let word_width = word.width();
            let gap = usize::from(!line.is_empty());
            if used + gap + word_width <= width {
                if gap == 1 {
                    line.push(' ');
                }
                line.push_str(word);
                used += gap + word_width;
                continue;
            }
            if !line.is_empty() {
                lines.push(std::mem::take(&mut line));
                used = 0;
            }
            if word_width <= width {
                line.push_str(word);
                used = word_width;
                continue;
            }
            for ch in word.chars() {
                let cell = ch.width().unwrap_or(0);
                if used + cell > width {
                    lines.push(std::mem::take(&mut line));
                    used = 0;
                }
                line.push(ch);
                used += cell;
            }
        }
        lines.push(line);
    }
    lines
}

/// Pad with spaces to exactly `width` cells (truncating when longer).
pub(crate) fn fit(text: &str, width: usize) -> String {
    let text = truncate_end(text, width);
    let pad = width.saturating_sub(text.width());
    format!("{text}{}", " ".repeat(pad))
}

/// Compact relative age for epoch milliseconds, such as `42s`, `5m`, `3h`,
/// `2d`, or `6w`.
pub(crate) fn age(at_ms: u64, now_ms: u64) -> String {
    let seconds = now_ms.saturating_sub(at_ms) / 1000;
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3600 => format!("{}m", seconds / 60),
        3600..86_400 => format!("{}h", seconds / 3600),
        86_400..1_209_600 => format!("{}d", seconds / 86_400),
        _ => format!("{}w", seconds / 604_800),
    }
}

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| {
            elapsed.as_millis().min(u128::from(u64::MAX)) as u64
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_keeps_the_head_and_counts_cells() {
        assert_eq!(truncate_end("ENG-12: fix the thing", 10), "ENG-12: f…");
        assert_eq!(truncate_end("short", 10), "short");
        assert_eq!(truncate_end("界面界面", 5), "界面…");
        assert_eq!(truncate_end("anything", 0), "");
        assert_eq!(fit("ab", 4), "ab  ");
        assert_eq!(truncate_middle("/very/long/path/name", 10), "/very…name");
        assert_eq!(truncate_middle("short", 10), "short");
    }

    #[test]
    fn wrapping_preserves_breaks_and_splits_long_words() {
        assert_eq!(wrap("one two three", 7), ["one two", "three"]);
        assert_eq!(wrap("a\n\nb", 5), ["a", "", "b"]);
        assert_eq!(wrap("abcdefgh", 3), ["abc", "def", "gh"]);
    }

    #[test]
    fn ages_are_compact() {
        assert_eq!(age(0, 59_000), "59s");
        assert_eq!(age(0, 3_600_000), "1h");
        assert_eq!(age(0, 2 * 86_400_000), "2d");
        assert_eq!(age(5, 0), "0s");
    }
}
