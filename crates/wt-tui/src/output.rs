//! Prepared output only. Cursor and viewport updates never ask a source to read.
use crate::{Interaction, Model, PickerAction, PickerOption, model::PickerPrompt};
use ratatui::{
    style::{Color, Style},
    text::Line,
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
                })
                .collect(),
        });
    }

    pub(crate) fn output_view(
        &mut self,
        height: usize,
        width: usize,
    ) -> (String, Vec<Line<'static>>) {
        let target = self.output.target.clone();
        let (title, lines, identity, count) = match &target {
            OutputTarget::Attention => {
                let mut lines = Vec::new();
                let mut marked = false;
                for event in &self.board.attention {
                    if self.board.attention_seen_ms > 0
                        && !marked
                        && event.at_ms > self.board.attention_seen_ms
                    {
                        lines.push(Line::styled(
                            format!(
                                "── seen {}",
                                self.display_time(self.board.attention_seen_ms)
                            ),
                            Style::new().fg(Color::DarkGray),
                        ));
                        marked = true;
                    }
                    let text = format!(
                        "{} {}: {}",
                        self.display_time(event.at_ms),
                        event.source,
                        event.text
                    );
                    lines.push(
                        if self.board.attention_seen_ms > 0
                            && event.at_ms <= self.board.attention_seen_ms
                        {
                            Line::styled(text, Style::new().fg(Color::DarkGray))
                        } else {
                            Line::from(text)
                        },
                    );
                }
                if self.board.attention_seen_ms > 0 && !marked && !self.board.attention.is_empty() {
                    lines.push(Line::styled(
                        format!(
                            "── seen {}",
                            self.display_time(self.board.attention_seen_ms)
                        ),
                        Style::new().fg(Color::DarkGray),
                    ));
                }
                ("Attention".to_owned(), lines, "attention".into(), 0)
            }
            OutputTarget::Activity => (
                "All activity".to_owned(),
                self.board
                    .activity
                    .iter()
                    .map(|event| {
                        Line::from(format!(
                            "{} {} [{}] {}",
                            self.display_time(event.at_ms),
                            event.level,
                            event.source,
                            event.text
                        ))
                    })
                    .collect(),
                "activity".into(),
                0,
            ),
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
                        row.map(|row| row.slug.as_str()).unwrap_or("No worktree"),
                        row.map(|row| row.key.clone()).unwrap_or_default(),
                    ),
                };
                let count = sessions.len() + logs.len();
                let stream = if self.output.identity != identity {
                    0
                } else {
                    self.output.stream.min(count.saturating_sub(1))
                };
                if let Some(session) = sessions.get(stream) {
                    (
                        format!(
                            "{label} · {} / {} · {}",
                            session.harness, session.name, session.state
                        ),
                        session.output.iter().cloned().map(Line::from).collect(),
                        identity,
                        count,
                    )
                } else if let Some(log) = stream
                    .checked_sub(sessions.len())
                    .and_then(|index| logs.get(index))
                {
                    (
                        format!("{label} · {}", log.title),
                        log.lines.iter().cloned().map(Line::from).collect(),
                        identity,
                        count,
                    )
                } else {
                    (
                        format!("{label} output"),
                        vec![Line::from("No output yet")],
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
        let lines = lines
            .into_iter()
            .flat_map(|line| {
                let style = line.style;
                let text = line
                    .spans
                    .into_iter()
                    .map(|span| span.content.into_owned())
                    .collect::<String>();
                wrap_visual_lines(&text, style, width)
            })
            .collect::<Vec<_>>();
        let top = self.output.viewport(lines.len(), height);
        let hint = if self.output.top.is_some() {
            " · scrolled"
        } else {
            ""
        };
        (
            format!("{title}{hint} · ' source · [ ] stream"),
            lines.into_iter().skip(top).take(height).collect(),
        )
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
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].style.fg, Some(Color::DarkGray));
        assert!(lines[1].spans[0].content.starts_with("── seen "));
        assert_eq!(lines[0].spans[0].content, "00:00:01 old: handled");
        assert!(lines[1].spans[0].content.starts_with("── seen 00:00:02"));
        assert_eq!(lines[2].spans[0].content, "00:00:03 new: unread");
        assert_eq!(lines[2].style.fg, None);
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
        assert_eq!(lines[0].spans[0].content, "00:00:01Z test: new");
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
        assert!(visible[1].spans[0].content.starts_with("  "));
        model.output.scroll(true);
        let (_, scrolled) = model.output_view(2, 24);
        assert!(scrolled[0].spans[0].content.starts_with("  "));
        assert_ne!(visible[0].spans[0].content, scrolled[0].spans[0].content);
    }
}
