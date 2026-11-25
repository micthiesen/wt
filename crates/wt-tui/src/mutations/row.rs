//! Facts about a row (status, title, base, issue, automations pause) and the
//! board-wide settings shown the same way: the automations pause and the
//! primary harness.
use wt_core::Domain;
use wt_runtime::Target;

use super::{Family, target_row, target_row_mut};
use crate::{
    Board, NotifyChange, WatchActionKind, WatchConditionView, WatchView,
    menus::notify::{set_for, state_words},
};

#[derive(Clone, Debug, PartialEq)]
pub enum RowChange {
    Status {
        state: wt_core::WorkState,
    },
    Title {
        title: String,
    },
    Base {
        base: Option<String>,
    },
    Issue {
        issue_id: Option<String>,
    },
    /// Pauses one row's automations, or the board's when the target names
    /// no row.
    Automations {
        paused: bool,
    },
    /// The controller chooses the next harness; the reply names it.
    Primary {
        harness: Option<String>,
    },
    /// A watch that tells the person, set or cancelled from the notify menu.
    Notify {
        change: NotifyChange,
    },
}

impl Family for RowChange {
    fn field(&self) -> String {
        match self {
            Self::Status { .. } => "status",
            Self::Title { .. } => "title",
            Self::Base { .. } => "base",
            Self::Issue { .. } => "issue",
            Self::Automations { .. } => "automations",
            Self::Primary { .. } => "primary",
            // Each watch is its own change: two can be pending at once.
            Self::Notify { change } => return format!("notify:{}", change.value()),
        }
        .into()
    }

    fn label(&self) -> String {
        match self {
            Self::Status { state } => format!("setting {}", state.as_str()),
            Self::Title { .. } => "retitling".into(),
            Self::Base { .. } => "setting base".into(),
            Self::Issue { .. } => "setting issue".into(),
            Self::Automations { paused: true } => "pausing automations".into(),
            Self::Automations { paused: false } => "resuming automations".into(),
            Self::Primary { .. } => "switching harness".into(),
            Self::Notify {
                change: NotifyChange::Add { .. },
            } => "setting notification".into(),
            Self::Notify {
                change: NotifyChange::Cancel { .. },
            } => "cancelling notification".into(),
        }
    }

    fn domains(&self) -> &'static [Domain] {
        match self {
            Self::Primary { .. } => &[Domain::Sessions],
            _ => &[Domain::Metadata],
        }
    }

    fn apply(&self, target: &Target, board: &mut Board) {
        match self {
            Self::Automations { paused } if target.key.is_none() => {
                board.automations_paused = *paused;
                for row in &mut board.rows {
                    let held = *paused || row.automations_paused;
                    row.watches.iter_mut().for_each(|watch| watch.held = held);
                }
            }
            Self::Primary { harness } => {
                if let Some(harness) = harness {
                    board.display.primary_harness = harness.clone();
                }
            }
            _ => {
                let paused_everywhere = board.automations_paused;
                let Some(row) = target_row_mut(board, target) else {
                    return;
                };
                match self {
                    Self::Status { state } => {
                        let work = row.work.get_or_insert_with(Default::default);
                        match &mut work.record {
                            Some(record) => record.state = *state,
                            None => work.record = Some(wt_core::WorkStatusRecord::new(*state, "")),
                        }
                        work.effective_state = Some(*state);
                        work.derived = false;
                    }
                    Self::Title { title } => {
                        row.title = title.clone();
                        row.title_source = crate::TitleSource::User;
                    }
                    Self::Base { base } => row.base_branch = base.clone(),
                    Self::Issue { issue_id } => row.issue_id = issue_id.clone(),
                    Self::Automations { paused } => {
                        row.automations_paused = *paused;
                        let held = *paused || paused_everywhere;
                        row.watches.iter_mut().for_each(|watch| watch.held = held);
                    }
                    Self::Notify {
                        change: NotifyChange::Add { state },
                    } => {
                        if set_for(row, state).is_none() {
                            // No id until the host has stored it.
                            row.watches.push(WatchView {
                                by: "human".into(),
                                conditions: vec![WatchConditionView {
                                    key: state.clone(),
                                    text: state_words(state).to_owned(),
                                    ..Default::default()
                                }],
                                actions: vec![WatchActionKind::Notify],
                                ..Default::default()
                            });
                        }
                    }
                    Self::Notify {
                        change: NotifyChange::Cancel { id },
                    } => row.watches.retain(|watch| watch.id != *id),
                    Self::Primary { .. } => {}
                }
            }
        }
    }

    fn observed(&self, target: &Target, board: &Board) -> bool {
        let row = target_row(board, target);
        match self {
            Self::Status { state } => row.is_none_or(|row| {
                row.work
                    .as_ref()
                    .and_then(|work| work.record.as_ref())
                    .is_some_and(|record| record.state == *state)
            }),
            Self::Title { title } => row.is_none_or(|row| &row.title == title),
            Self::Base { base } => row.is_none_or(|row| &row.base_branch == base),
            Self::Issue { issue_id } => row.is_none_or(|row| &row.issue_id == issue_id),
            Self::Automations { paused } => match target.key {
                Some(_) => row.is_none_or(|row| row.automations_paused == *paused),
                None => board.automations_paused == *paused,
            },
            Self::Primary { harness } => harness
                .as_deref()
                .is_none_or(|h| h.eq_ignore_ascii_case(&board.display.primary_harness)),
            Self::Notify {
                change: NotifyChange::Add { state },
            } => row.is_none_or(|row| set_for(row, state).is_some()),
            Self::Notify {
                change: NotifyChange::Cancel { id },
            } => row.is_none_or(|row| row.watches.iter().all(|watch| watch.id != *id)),
        }
    }
}
