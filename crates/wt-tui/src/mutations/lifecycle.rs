//! Creating and removing worktrees: placeholder rows that stand in until
//! inventory shows the result, with deadlines long enough for a checkout or
//! a queued removal.
use std::time::Duration;

use wt_core::Domain;
use wt_runtime::{Deadlines, Target};

use super::{ARCHIVED_SECTION, ARCHIVED_TITLE, Family, INBOX_SECTION, INBOX_TITLE, place};
use crate::{Board, BoardRow, BusyView, PreparedDetailGroup};

/// Label of a creation the host has not finished.
const CREATING: &str = "creating";
/// Label of a created checkout that inventory does not show yet.
const UPDATING_INVENTORY: &str = "updating inventory";
/// Badge and busy label of a created checkout that inventory does not show
/// yet.
const READY_UPDATING_INVENTORY: &str = "ready · updating inventory";
/// Label, badge and busy label of a removal.
const REMOVING: &str = "removing";
/// A checkout or a queued removal (stage teardown, a large tree) can take
/// minutes; the placeholder stays until inventory shows the result rather
/// than flicking back at the usual deadline.
const DEADLINE: Duration = Duration::from_secs(600);

#[derive(Clone, Debug, PartialEq)]
pub enum LifecycleChange {
    Create {
        placeholder: String,
        input: String,
        resolved: Option<String>,
    },
    Remove {
        keys: Vec<String>,
    },
}

fn busy(label: &str, op: &str) -> Option<BusyView> {
    Some(BusyView {
        op: op.into(),
        label: label.into(),
        ..Default::default()
    })
}

impl Family for LifecycleChange {
    fn field(&self) -> String {
        match self {
            Self::Create { placeholder, .. } => format!("create:{placeholder}"),
            Self::Remove { keys } => format!("remove:{}", keys.join(",")),
        }
    }

    fn label(&self) -> String {
        match self {
            Self::Create { resolved: None, .. } => CREATING,
            Self::Create { .. } => UPDATING_INVENTORY,
            Self::Remove { .. } => REMOVING,
        }
        .into()
    }

    fn domains(&self) -> &'static [Domain] {
        &[Domain::Git, Domain::Metadata]
    }

    fn apply(&self, target: &Target, board: &mut Board) {
        match self {
            Self::Create {
                placeholder,
                input,
                resolved,
            } => {
                if resolved
                    .as_deref()
                    .is_some_and(|key| board.rows.iter().any(|row| row.key == key))
                {
                    return;
                }
                let ready = resolved.is_some();
                let index = board.rows.len();
                board.rows.push(BoardRow {
                    key: placeholder.clone(),
                    host: target.host.clone(),
                    title: input.clone(),
                    busy: busy(
                        if ready {
                            READY_UPDATING_INVENTORY
                        } else {
                            CREATING
                        },
                        "init",
                    ),
                    detail_groups: vec![PreparedDetailGroup {
                        id: "git".into(),
                        label: "git".into(),
                        ..Default::default()
                    }],
                    badge: if ready {
                        READY_UPDATING_INVENTORY
                    } else {
                        "creating · F12 to open when ready"
                    }
                    .into(),
                    details: vec![
                        if ready {
                            "Checkout ready. Waiting for inventory."
                        } else {
                            "Creating checkout. F12 will open the agent when ready."
                        }
                        .into(),
                    ],
                    ..Default::default()
                });
                let inbox = super::section_index(board, INBOX_SECTION, INBOX_TITLE);
                board.sections[inbox].rows.push(index);
                board.sections[inbox].folded = false;
            }
            Self::Remove { keys } => {
                for key in keys {
                    if let Some(index) = board.rows.iter().position(|row| &row.key == key) {
                        let row = &mut board.rows[index];
                        row.archived = true;
                        row.badge = REMOVING.into();
                        row.busy = busy(REMOVING, "remove");
                        place(board, index, ARCHIVED_SECTION, ARCHIVED_TITLE);
                    }
                }
            }
        }
    }

    fn observed(&self, _target: &Target, board: &Board) -> bool {
        match self {
            // The placeholder stands in for the row until inventory shows
            // it. Confirmed data never contains a placeholder, so only the
            // real row can settle the creation.
            Self::Create {
                resolved,
                placeholder,
                ..
            } => board.rows.iter().any(|row| {
                Some(row.key.as_str()) == resolved.as_deref() || &row.key == placeholder
            }),
            Self::Remove { keys } => !keys.iter().any(|key| {
                board
                    .rows
                    .iter()
                    .any(|row| &row.key == key && !row.archived)
            }),
        }
    }

    fn deadlines(&self) -> Option<Deadlines> {
        Some(Deadlines {
            in_flight: DEADLINE,
            accepted: DEADLINE,
        })
    }
}
