//! Prepared output only. Cursor and viewport updates never ask a source to read.
use crate::{Interaction, Model, PickerAction, PickerOption, model::PickerPrompt};

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

    pub(crate) fn output_view(&mut self, height: usize) -> (String, Vec<String>) {
        let target = self.output.target.clone();
        let (title, lines, identity, count) = match &target {
            OutputTarget::Attention => (
                "Attention".to_owned(),
                self.board.attention.clone(),
                "attention".into(),
                0,
            ),
            OutputTarget::Activity => (
                "All activity".to_owned(),
                self.board.activity.clone(),
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
                        session.output.clone(),
                        identity,
                        count,
                    )
                } else if let Some(log) = stream
                    .checked_sub(sessions.len())
                    .and_then(|index| logs.get(index))
                {
                    (
                        format!("{label} · {}", log.title),
                        log.lines.clone(),
                        identity,
                        count,
                    )
                } else {
                    (
                        format!("{label} output"),
                        vec!["No output yet".into()],
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
            .iter()
            .flat_map(|line| line.lines().map(str::to_owned))
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    #[test]
    fn scrolling_stays_put_on_append_and_refollows_at_bottom() {
        let mut model = Model {
            board: Arc::new(crate::Board {
                attention: (0..20).map(|n| n.to_string()).collect(),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(model.output_view(4).1, ["16", "17", "18", "19"]);
        model.output.scroll(true);
        assert_eq!(model.output_view(4).1[0], "13");
        Arc::make_mut(&mut model.board).attention.push("20".into());
        assert_eq!(model.output_view(4).1[0], "13");
        model.output.scroll(false);
        model.output_view(4);
        model.output.scroll(false);
        assert_eq!(model.output_view(4).1, ["17", "18", "19", "20"]);
        assert!(model.output.top.is_none());
    }
}
