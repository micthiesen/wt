//! Prepared output only. Cursor and viewport updates never ask a source to read.
use crate::{Interaction, Model, PickerAction, PickerOption, model::PickerPrompt};
use ratatui::{
    style::{Color, Style},
    text::Line,
};

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

    pub(crate) fn output_view(&mut self, height: usize) -> (String, Vec<Line<'static>>) {
        let target = self.output.target.clone();
        let (title, lines, identity, count) = match &target {
            OutputTarget::Attention => {
                let mut lines = Vec::new();
                let mut marked = false;
                for event in &self.board.attention {
                    if !marked && event.at_ms > self.board.attention_seen_ms {
                        lines.push(Line::styled(
                            format!("── seen {}", time_of_day(self.board.attention_seen_ms)),
                            Style::new().fg(Color::DarkGray),
                        ));
                        marked = true;
                    }
                    let text = format!("{}: {}", event.source, event.text);
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
                        format!("── seen {}", time_of_day(self.board.attention_seen_ms)),
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
                            time_of_day(event.at_ms),
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
                let line_style = line.style;
                line.spans
                    .into_iter()
                    .flat_map(|span| {
                        span.content
                            .lines()
                            .map(|content| {
                                Line::styled(content.to_owned(), line_style.patch(span.style))
                            })
                            .collect::<Vec<_>>()
                    })
                    .collect::<Vec<_>>()
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
}

fn time_of_day(at_ms: u64) -> String {
    let seconds = at_ms / 1_000 % 86_400;
    format!(
        "{:02}:{:02}:{:02}",
        seconds / 3_600,
        seconds / 60 % 60,
        seconds % 60
    )
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
        assert!(format!("{:?}", model.output_view(4).1).contains("19"));
        model.output.scroll(true);
        assert!(format!("{:?}", model.output_view(4).1[0]).contains("13"));
        Arc::make_mut(&mut model.board)
            .attention
            .push(crate::AttentionLine {
                at_ms: 20,
                source: "test".into(),
                text: "20".into(),
            });
        assert!(format!("{:?}", model.output_view(4).1[0]).contains("13"));
        model.output.scroll(false);
        model.output_view(4);
        model.output.scroll(false);
        assert!(format!("{:?}", model.output_view(4).1).contains("20"));
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
                ..Default::default()
            }),
            ..Default::default()
        };
        let lines = model.output_view(5).1;
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].style.fg, Some(Color::DarkGray));
        assert!(lines[1].spans[0].content.starts_with("── seen "));
        assert_eq!(lines[2].spans[0].content, "new: unread");
        assert_eq!(lines[2].style.fg, None);
    }
}
