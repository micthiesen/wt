//! Prepared output only. Cursor and viewport updates never ask a source to read.
use crate::{
    Interaction, Model, PickerAction, PickerOption, glyphs, model::PickerPrompt,
    render::text::truncate_end, theme,
};
use ratatui::{
    style::Style,
    text::{Line, Span},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum OutputTarget {
    #[default]
    Attention,
    Activity,
    Selected,
    Slot(String),
}

#[derive(Default)]
pub(crate) struct OutputState {
    pub target: OutputTarget,
    stream: usize,
    top: Option<usize>,
    last_top: usize,
    last_max: usize,
    identity: String,
    stream_count: usize,
}

impl OutputState {
    pub fn choose(&mut self, target: OutputTarget) {
        *self = Self {
            target,
            ..Default::default()
        };
    }
    pub fn cycle(&mut self, forward: bool) {
        if matches!(
            self.target,
            OutputTarget::Attention | OutputTarget::Activity
        ) {
            self.choose(OutputTarget::Selected);
        } else {
            let count = self.stream_count.max(1);
            self.stream = if forward {
                (self.stream + 1) % count
            } else {
                (self.stream + count - 1) % count
            };
            self.top = None;
        }
    }
    pub fn toggle_feed(&mut self) {
        self.choose(match self.target {
            OutputTarget::Attention => OutputTarget::Activity,
            _ => OutputTarget::Attention,
        });
    }
    pub fn mark_seen(&mut self) {
        self.top = None;
    }
    #[cfg(test)]
    pub fn is_scrolled(&self) -> bool {
        self.top.is_some()
    }
    pub fn scroll(&mut self, up: bool) {
        self.top = if up {
            Some(self.last_top.saturating_sub(3))
        } else {
            let next = self.last_top.saturating_add(3);
            (next < self.last_max).then_some(next)
        };
    }
    fn viewport(&mut self, lines: usize, height: usize) -> usize {
        self.last_max = lines.saturating_sub(height);
        self.last_top = self.top.map_or(self.last_max, |top| top.min(self.last_max));
        self.last_top
    }
}

impl Model {
    pub(crate) fn open_output_picker(&mut self) {
        let mut choices = vec![
            OutputTarget::Attention,
            OutputTarget::Activity,
            OutputTarget::Selected,
        ];
        let mut labels = vec![
            "Attention".to_owned(),
            "All activity".into(),
            "Selected worktree output".into(),
        ];
        let slots = self
            .board
            .slot_sessions
            .keys()
            .chain(self.board.slot_logs.keys())
            .collect::<std::collections::BTreeSet<_>>();
        for slot in slots {
            choices.push(OutputTarget::Slot(slot.clone()));
            labels.push(format!("{slot} output"));
        }
        let selected = choices
            .iter()
            .position(|choice| *choice == self.output.target)
            .unwrap_or(0);
        self.interaction = Interaction::Picker(PickerPrompt {
            action: PickerAction::Output { choices },
            title: "Output source".into(),
            selected,
            options: labels
                .into_iter()
                .enumerate()
                .map(|(i, label)| PickerOption {
                    value: Some(i.to_string()),
                    label,
                    chord: None,
                    note: None,
                    verify_after_merge: None,
                    detail: None,
                })
                .collect(),
        });
    }

    pub(crate) fn output_view(
        &mut self,
        height: usize,
        width: usize,
    ) -> (String, Vec<Line<'static>>) {
        // One cell of padding each side, as in the list.
        let width = width.saturating_sub(2).max(1);
        let target = self.output.target.clone();
        // Line indexes where each attention entry begins.
        let mut entry_starts = Vec::new();
        let (title, lines, identity, count) = match &target {
            OutputTarget::Attention => {
                let seen_ms = self.board.attention_seen_ms;
                let events = &self.board.attention;
                let columns = FeedColumns::new(
                    width,
                    events.iter().map(|event| event.source.as_str()),
                    events.first().map(|event| self.display_time(event.at_ms)),
                );
                let mut lines = Vec::new();
                let mut marked = false;
                let mut fresh = 0;
                for event in events {
                    let seen = seen_ms > 0 && event.at_ms <= seen_ms;
                    if seen_ms > 0 && !marked && !seen {
                        lines.extend(self.seen_rule(seen_ms, width, !lines.is_empty()));
                        marked = true;
                    }
                    fresh += usize::from(!seen);
                    entry_starts.push(lines.len());
                    let text = if seen {
                        theme::dim()
                    } else {
                        theme::fg(theme::FG)
                    };
                    lines.extend(columns.event(
                        &self.display_time(event.at_ms),
                        &event.source,
                        &event.text,
                        text,
                        seen,
                        true,
                    ));
                }
                if seen_ms > 0 && !marked && !events.is_empty() {
                    lines.extend(self.seen_rule(seen_ms, width, true));
                }
                if events.is_empty() {
                    lines.push(Line::from(vec![
                        Span::styled(format!("{}  ", glyphs::CHECK_PASS), theme::fg(theme::OK)),
                        Span::styled("nothing needs you", theme::dim()),
                    ]));
                }
                let title = if seen_ms > 0 && fresh > 0 {
                    format!("attention · {fresh} new")
                } else {
                    "attention".to_owned()
                };
                (title, lines, "attention".into(), 0)
            }
            OutputTarget::Activity => {
                let events = &self.board.activity;
                let columns = FeedColumns::new(
                    width,
                    events.iter().map(|event| event.source.as_str()),
                    events.first().map(|event| self.display_time(event.at_ms)),
                );
                // One row per event: the firehose is for scanning, and the
                // full text stays in the log file.
                let mut lines = events
                    .iter()
                    .flat_map(|event| {
                        columns.event(
                            &self.display_time(event.at_ms),
                            &event.source,
                            &event.text,
                            level_style(&event.level),
                            false,
                            false,
                        )
                    })
                    .collect::<Vec<_>>();
                if lines.is_empty() {
                    lines.push(Line::styled("no events yet", theme::dim()));
                }
                ("all activity".to_owned(), lines, "activity".into(), 0)
            }
            OutputTarget::Selected | OutputTarget::Slot(_) => {
                let row = self.selected_row();
                let (sessions, logs, label, identity) = match &target {
                    OutputTarget::Slot(key) => (
                        self.board
                            .slot_sessions
                            .get(key)
                            .map(Vec::as_slice)
                            .unwrap_or_default(),
                        self.board
                            .slot_logs
                            .get(key)
                            .map(Vec::as_slice)
                            .unwrap_or_default(),
                        key.as_str(),
                        key.clone(),
                    ),
                    _ => (
                        row.map(|row| row.sessions.as_slice()).unwrap_or_default(),
                        row.map(|row| row.logs.as_slice()).unwrap_or_default(),
                        row.map(|row| row.slug.as_str()).unwrap_or("no worktree"),
                        row.map(|row| row.key.clone()).unwrap_or_default(),
                    ),
                };
                let count = sessions.len() + logs.len();
                let stream = if self.output.identity != identity {
                    0
                } else {
                    self.output.stream.min(count.saturating_sub(1))
                };
                let position = if count > 1 {
                    format!(" · {}/{count}", stream + 1)
                } else {
                    String::new()
                };
                let plain = |lines: &[String]| {
                    lines
                        .iter()
                        .flat_map(|line| wrap_visual_lines(line, theme::fg(theme::FG), width))
                        .collect::<Vec<_>>()
                };
                if let Some(session) = sessions.get(stream) {
                    let mut output = Vec::with_capacity(session.output.len() + 1);
                    if let Some(summary) = session.summary.as_deref() {
                        let mut wrapped = wrap_visual_lines(
                            summary,
                            theme::fg(theme::FG_BRIGHT),
                            width.saturating_sub(3),
                        )
                        .into_iter();
                        if let Some(first) = wrapped.next() {
                            let mut spans = vec![Span::styled(
                                format!("{}  ", glyphs::TASK_COMPLETE),
                                theme::fg(theme::OK),
                            )];
                            spans.extend(first.spans);
                            output.push(Line::from(spans));
                        }
                        output.extend(wrapped.map(|line| {
                            let mut spans = vec![Span::raw("   ")];
                            spans.extend(line.spans);
                            Line::from(spans)
                        }));
                    }
                    output.extend(plain(&session.output));
                    if output.is_empty() {
                        output.push(Line::styled("no output yet", theme::dim()));
                    }
                    (
                        format!(
                            "{label} · {} / {} · {}{position}",
                            session.harness, session.name, session.state
                        ),
                        output,
                        identity,
                        count,
                    )
                } else if let Some(log) = stream
                    .checked_sub(sessions.len())
                    .and_then(|index| logs.get(index))
                {
                    let mut output = plain(&log.lines);
                    if output.is_empty() {
                        output.push(Line::styled("no output yet", theme::dim()));
                    }
                    (
                        format!("{label} · {}{position}", log.title),
                        output,
                        identity,
                        count,
                    )
                } else {
                    (
                        format!("{label} output"),
                        vec![Line::styled("no output yet", theme::dim())],
                        identity,
                        count,
                    )
                }
            }
        };
        if self.output.identity != identity {
            self.output.identity = identity;
            self.output.stream = 0;
            self.output.top = None;
        }
        self.output.stream_count = count;
        let mut top = self.output.viewport(lines.len(), height);
        // Following the newest entries, start at an entry rather than the
        // wrapped tail of one, so the top row always carries its time.
        if self.output.top.is_none()
            && let Some(start) = entry_starts.into_iter().find(|start| *start >= top)
        {
            top = start;
            self.output.last_top = start;
        }
        let hint = if self.output.top.is_some() {
            " · scrolled"
        } else {
            ""
        };
        (
            format!("{title}{hint}"),
            lines
                .into_iter()
                .skip(top)
                .take(height)
                .map(|line| {
                    let mut spans = vec![Span::raw(" ")];
                    spans.extend(line.spans);
                    Line::from(spans).style(line.style)
                })
                .collect(),
        )
    }

    /// `── seen 12:00:01 ────`: everything above is handled. A blank row
    /// above gives it air.
    fn seen_rule(&self, seen_ms: u64, width: usize, spaced: bool) -> Vec<Line<'static>> {
        let label = format!(" seen {} ", self.display_time(seen_ms));
        let trail = width.saturating_sub(2 + label.width());
        let rule = Line::from(vec![
            Span::styled("──", theme::fg(theme::BORDER)),
            Span::styled(label, theme::dim()),
            Span::styled("─".repeat(trail), theme::fg(theme::BORDER)),
        ]);
        if spaced {
            vec![Line::default(), rule]
        } else {
            vec![rule]
        }
    }

    fn display_time(&self, at_ms: u64) -> String {
        self.board
            .local_times
            .get(&at_ms)
            .cloned()
            .unwrap_or_else(|| format!("{}Z", utc_time_of_day(at_ms)))
    }
}

fn utc_time_of_day(at_ms: u64) -> String {
    let seconds = at_ms / 1_000 % 86_400;
    format!(
        "{:02}:{:02}:{:02}",
        seconds / 3_600,
        seconds / 60 % 60,
        seconds % 60
    )
}

/// Feed rows: a dim time column, a right-aligned source column sized to
/// the widest source (at most 16 cells, shrinking first on narrow panes),
/// then the message. Wrapped messages continue under a two-cell hanging
/// indent that spans the pane, so a long note is not squeezed into the
/// message column.
struct FeedColumns {
    width: usize,
    time: usize,
    source: usize,
}

const SOURCE_MAX: usize = 16;
const CONTINUATION: usize = 2;

impl FeedColumns {
    fn new<'a>(
        width: usize,
        sources: impl Iterator<Item = &'a str>,
        sample_time: Option<String>,
    ) -> Self {
        let width = width.max(1);
        let time = sample_time
            .map_or(8, |time| time.width())
            .min(width.saturating_sub(1));
        let widest = sources.map(UnicodeWidthStr::width).max().unwrap_or(0);
        // The message keeps at least 13 cells; a source column too narrow
        // to read is dropped rather than shown as an ellipsis.
        let room = width.saturating_sub(time + 1 + 13);
        let source = if room < 4 {
            0
        } else {
            widest.min(SOURCE_MAX).min(room)
        };
        Self {
            width,
            time,
            source,
        }
    }

    fn prefix(&self) -> usize {
        self.time + 1 + if self.source > 0 { self.source + 1 } else { 0 }
    }

    fn event(
        &self,
        time: &str,
        source: &str,
        text: &str,
        style: Style,
        seen: bool,
        wrap: bool,
    ) -> Vec<Line<'static>> {
        let first = self.width.saturating_sub(self.prefix()).max(1);
        let mut head = vec![Span::styled(
            format!("{} ", truncate_end(time, self.time)),
            theme::dim(),
        )];
        if self.source > 0 {
            let source_text = truncate_end(source, self.source);
            let pad = self.source.saturating_sub(source_text.width());
            // Bracketed sources are cross-cutting system events; slug
            // sources keep the brighter accent.
            let color = if seen || source.starts_with('[') {
                theme::FG_DIM
            } else {
                theme::ACCENT_ALT
            };
            head.push(Span::styled(
                format!("{}{source_text} ", " ".repeat(pad)),
                theme::fg(color),
            ));
        }
        if !wrap {
            head.push(Span::styled(truncate_end(text, first), style));
            return vec![Line::from(head)];
        }
        let rest = self.width.saturating_sub(CONTINUATION).max(1);
        let mut parts = wrap_hanging(text, first, rest).into_iter();
        head.push(Span::styled(parts.next().unwrap_or_default(), style));
        let mut lines = vec![Line::from(head)];
        lines.extend(parts.map(|part| {
            Line::from(vec![
                Span::raw(" ".repeat(CONTINUATION.min(self.width - 1))),
                Span::styled(part, style),
            ])
        }));
        lines
    }
}

fn level_style(level: &str) -> Style {
    match level.to_ascii_uppercase().as_str() {
        "ERROR" | "ERR" | "FATAL" => theme::fg(theme::ERR),
        "WARN" | "WARNING" => theme::fg(theme::WARN),
        "OK" | "SUCCESS" => theme::fg(theme::OK),
        "DEBUG" | "TRACE" | "DIM" => theme::dim(),
        _ => theme::fg(theme::FG),
    }
}

/// Word-wrap with a different width for the first line; words longer than
/// a line are hard-split.
fn wrap_hanging(text: &str, first: usize, rest: usize) -> Vec<String> {
    let mut lines = vec![String::new()];
    let mut used = 0;
    let limit = |lines: &Vec<String>| if lines.len() == 1 { first } else { rest };
    for word in text.split_whitespace() {
        let cells = word.width();
        if used > 0 && used + 1 + cells <= limit(&lines) {
            let line = lines.last_mut().expect("one line");
            line.push(' ');
            line.push_str(word);
            used += 1 + cells;
            continue;
        }
        if used > 0 {
            lines.push(String::new());
            used = 0;
        }
        for ch in word.chars() {
            let cell = UnicodeWidthChar::width(ch).unwrap_or(0);
            if used + cell > limit(&lines) && used > 0 {
                lines.push(String::new());
                used = 0;
            }
            lines.last_mut().expect("one line").push(ch);
            used += cell;
        }
    }
    lines
}

fn wrap_visual_lines(text: &str, style: Style, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut wrapped = Vec::new();
    for source in text.split('\n') {
        let mut rest = source;
        let mut continuation = false;
        loop {
            let indent = if continuation {
                2usize.min(width.saturating_sub(1))
            } else {
                0
            };
            let available = width.saturating_sub(indent).max(1);
            if UnicodeWidthStr::width(rest) <= available {
                wrapped.push(Line::styled(
                    format!("{}{}", " ".repeat(indent), rest),
                    style,
                ));
                break;
            }

            let mut cells = 0;
            let mut fit_end = 0;
            let mut word_end = None;
            for (index, character) in rest.char_indices() {
                let character_width = UnicodeWidthChar::width(character).unwrap_or(0);
                if cells + character_width > available {
                    break;
                }
                cells += character_width;
                fit_end = index + character.len_utf8();
                if character.is_whitespace() {
                    word_end = Some(fit_end);
                }
            }
            if fit_end == 0 {
                fit_end = rest.chars().next().map_or(rest.len(), char::len_utf8);
            }
            let split = word_end.filter(|end| *end > 0).unwrap_or(fit_end);
            let part = rest[..split].trim_end_matches(char::is_whitespace);
            wrapped.push(Line::styled(
                format!("{}{}", " ".repeat(indent), part),
                style,
            ));
            rest = rest[split..].trim_start_matches(char::is_whitespace);
            continuation = true;
        }
    }
    wrapped
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Line text without the pane's one-cell left padding.
    fn text(line: &Line<'_>) -> String {
        let text: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        text.strip_prefix(' ').unwrap_or(&text).to_owned()
    }

    #[test]
    fn empty_feeds_say_so_and_the_firehose_colors_levels() {
        let mut model = Model::default();
        let (title, lines) = model.output_view(5, 60);
        assert_eq!(title, "attention");
        assert_eq!(
            text(&lines[0]),
            format!("{}  nothing needs you", glyphs::CHECK_PASS)
        );
        assert_eq!(lines[0].spans[1].style.fg, Some(theme::OK));
        model.output.toggle_feed();
        let (title, lines) = model.output_view(5, 60);
        assert_eq!(title, "all activity");
        assert_eq!(text(&lines[0]), "no events yet");
        Arc::make_mut(&mut model.board).activity = vec![
            crate::ActivityLine {
                at_ms: 1_000,
                level: "ERROR".into(),
                source: "[app]".into(),
                text: "refresh failed with a long explanation that will not fit".into(),
                ..Default::default()
            },
            crate::ActivityLine {
                at_ms: 2_000,
                level: "WARN".into(),
                source: "feature-one".into(),
                text: "slow".into(),
                ..Default::default()
            },
        ];
        let lines = model.output_view(5, 50).1;
        assert_eq!(lines.len(), 2, "the firehose truncates to one row each");
        assert!(text(&lines[0]).ends_with('…'));
        assert_eq!(lines[0].spans[2].style.fg, Some(theme::FG_DIM));
        assert_eq!(lines[0].spans[3].style.fg, Some(theme::ERR));
        assert_eq!(text(&lines[1]), "00:00:02Z feature-one slow");
        assert_eq!(lines[1].spans[2].style.fg, Some(theme::ACCENT_ALT));
        assert_eq!(lines[1].spans[3].style.fg, Some(theme::WARN));
    }
    use std::sync::Arc;
    #[test]
    fn scrolling_stays_put_on_append_and_refollows_at_bottom() {
        let mut model = Model {
            board: Arc::new(crate::Board {
                attention: (0..20)
                    .map(|n| crate::AttentionLine {
                        at_ms: n,
                        source: "test".into(),
                        text: n.to_string(),
                    })
                    .collect(),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(format!("{:?}", model.output_view(4, 40).1).contains("19"));
        model.output.scroll(true);
        assert!(format!("{:?}", model.output_view(4, 40).1[0]).contains("13"));
        Arc::make_mut(&mut model.board)
            .attention
            .push(crate::AttentionLine {
                at_ms: 20,
                source: "test".into(),
                text: "20".into(),
            });
        assert!(format!("{:?}", model.output_view(4, 40).1[0]).contains("13"));
        model.output.scroll(false);
        model.output_view(4, 40);
        model.output.scroll(false);
        assert!(format!("{:?}", model.output_view(4, 40).1).contains("20"));
        assert!(model.output.top.is_none());
    }

    #[test]
    fn selected_session_output_starts_with_its_completed_summary() {
        let mut model = Model {
            board: Arc::new(crate::Board {
                rows: vec![crate::BoardRow {
                    slug: "one".into(),
                    sessions: vec![crate::SessionView {
                        harness: "Claude".into(),
                        name: "primary".into(),
                        summary: Some("Finished the review".into()),
                        output: vec!["last output line".into()],
                        ..Default::default()
                    }],
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..Default::default()
        };
        model.selected = Some(0);
        model.output.choose(OutputTarget::Selected);
        let (title, lines) = model.output_view(5, 80);
        assert!(title.contains("Claude"));
        assert_eq!(
            text(&lines[0]),
            format!("{}  Finished the review", glyphs::TASK_COMPLETE)
        );
        assert_eq!(text(&lines[1]), "last output line");
    }

    #[test]
    fn seen_watermark_dims_old_rows_and_keeps_a_timestamped_rule() {
        let mut model = Model {
            board: Arc::new(crate::Board {
                attention: vec![
                    crate::AttentionLine {
                        at_ms: 1_000,
                        source: "old".into(),
                        text: "handled".into(),
                    },
                    crate::AttentionLine {
                        at_ms: 3_000,
                        source: "new".into(),
                        text: "unread".into(),
                    },
                ],
                attention_seen_ms: 2_000,
                local_times: [
                    (1_000, "00:00:01".into()),
                    (2_000, "00:00:02".into()),
                    (3_000, "00:00:03".into()),
                ]
                .into_iter()
                .collect(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let lines = model.output_view(5, 80).1;
        assert_eq!(lines.len(), 4);
        assert_eq!(text(&lines[0]), "00:00:01 old handled");
        assert!(
            lines[0]
                .spans
                .iter()
                .skip(1)
                .all(|span| span.style.fg == Some(theme::FG_DIM))
        );
        assert!(text(&lines[1]).is_empty());
        assert!(text(&lines[2]).starts_with("── seen 00:00:02 ──"));
        assert_eq!(text(&lines[3]), "00:00:03 new unread");
        assert_eq!(lines[3].spans[2].style.fg, Some(theme::ACCENT_ALT));
        assert_eq!(lines[3].spans[3].style.fg, Some(theme::FG));
        assert_eq!(model.output_view(5, 80).0, "attention · 1 new");
    }

    #[test]
    fn unset_attention_watermark_does_not_render_a_seen_marker() {
        let mut model = Model {
            board: Arc::new(crate::Board {
                attention: vec![crate::AttentionLine {
                    at_ms: 1_000,
                    source: "test".into(),
                    text: "new".into(),
                }],
                ..Default::default()
            }),
            ..Default::default()
        };
        let lines = model.output_view(5, 80).1;
        assert_eq!(lines.len(), 1);
        assert_eq!(text(&lines[0]), "00:00:01Z test new");
    }

    #[test]
    fn following_attention_starts_at_an_entry_not_a_wrapped_tail() {
        let line = |at_ms, text: &str| crate::AttentionLine {
            at_ms,
            source: "wt".into(),
            text: text.into(),
        };
        let mut model = Model {
            board: Arc::new(crate::Board {
                attention: vec![
                    line(
                        1_000,
                        "first entry wraps across several rows of a narrow pane",
                    ),
                    line(2_000, "second"),
                ],
                ..Default::default()
            }),
            ..Default::default()
        };
        let lines = model.output_view(3, 30).1;
        assert!(
            text(&lines[0]).starts_with("00:00:02"),
            "{:?}",
            text(&lines[0])
        );
    }

    #[test]
    fn wraps_output_to_visual_rows_with_two_cell_hanging_indent() {
        let lines = wrap_visual_lines(
            "2026-10-09T12:00:00Z manager: the note has a long word boundary here",
            Style::default(),
            24,
        );
        let rendered = lines
            .iter()
            .map(|line| line.spans[0].content.as_ref())
            .collect::<Vec<_>>();
        assert!(rendered.len() > 2);
        assert!(rendered[0].starts_with("2026-10-09"));
        assert!(rendered[1].starts_with("  "));
        assert!(
            rendered
                .iter()
                .all(|line| UnicodeWidthStr::width(*line) <= 24)
        );
        assert!(rendered.join(" ").contains("long word boundary"));
    }

    #[test]
    fn scroll_offsets_count_wrapped_visual_lines() {
        let mut model = Model {
            board: Arc::new(crate::Board {
                attention: vec![crate::AttentionLine {
                    at_ms: 1,
                    source: "manager".into(),
                    text: "one two three four five six seven eight nine ten eleven twelve thirteen fourteen fifteen sixteen seventeen eighteen nineteen twenty twenty-one twenty-two twenty-three twenty-four twenty-five twenty-six twenty-seven twenty-eight twenty-nine thirty".into(),
                }],
                ..Default::default()
            }),
            ..Default::default()
        };
        let (_, visible) = model.output_view(2, 24);
        assert!(text(&visible[1]).starts_with("  "));
        model.output.scroll(true);
        let (_, scrolled) = model.output_view(2, 24);
        assert!(text(&scrolled[0]).starts_with("  "));
        assert_ne!(text(&visible[0]), text(&scrolled[0]));
    }
}
