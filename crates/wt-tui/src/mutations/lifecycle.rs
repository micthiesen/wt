//! Creating and removing worktrees. Creation projects one non-actionable row;
//! the host binds its exact identity before creating the checkout.
use std::time::Duration;

use wt_core::Domain;
use wt_runtime::{Deadlines, Target};

use super::{Family, INBOX_SECTION, place};
use crate::{Board, BoardRow, BusyView};

/// Label, badge and busy label of a removal.
const REMOVING: &str = "removing";
/// A checkout or a queued removal (stage teardown, a large tree) can take
/// minutes; the operation stays until inventory shows the result rather
/// than flicking back at the usual deadline.
const DEADLINE: Duration = Duration::from_secs(600);

#[derive(Clone, Debug, PartialEq)]
pub enum LifecycleChange {
    Create {
        request_id: u64,
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
            Self::Create { request_id, .. } => format!("create:{request_id}"),
            Self::Remove { keys } => format!("remove:{}", keys.join(",")),
        }
    }

    fn label(&self) -> String {
        match self {
            Self::Create {
                input,
                resolved: None,
                ..
            } => format!("creating {input}"),
            Self::Create { input, .. } => format!("waiting for {input} in inventory"),
            Self::Remove { .. } => REMOVING.into(),
        }
    }

    fn domains(&self) -> &'static [Domain] {
        &[Domain::Git, Domain::Metadata]
    }

    fn apply(&self, target: &Target, board: &mut Board) {
        match self {
            Self::Create {
                input, resolved, ..
            } => {
                let Some(key) = target.key.as_ref() else {
                    return;
                };
                if let Some(row) = board
                    .rows
                    .iter_mut()
                    .find(|row| row.key == *key && row.host == target.host)
                {
                    row.pending_creation = resolved.is_none();
                    if row.pending_creation {
                        row.busy = busy("creating", "create");
                    }
                } else {
                    let index = board.rows.len();
                    board.rows.push(BoardRow {
                        key: key.clone(),
                        host: target.host.clone(),
                        slug: input.clone(),
                        title: input.clone(),
                        pending_creation: true,
                        busy: busy("creating", "create"),
                        ..Default::default()
                    });
                    if !board.sections.is_empty() {
                        place(board, index, INBOX_SECTION, "Inbox");
                    }
                }
            }
            Self::Remove { keys } => {
                for key in keys {
                    if let Some(index) = board.rows.iter().position(|row| &row.key == key) {
                        let row = &mut board.rows[index];
                        // Durable worker state owns placement and terminal failure.
                        if row
                            .removal
                            .as_ref()
                            .is_some_and(|removal| removal.error.is_some())
                        {
                            continue;
                        }
                        let label = if row.removal.is_some() {
                            REMOVING
                        } else {
                            "checking removal"
                        };
                        row.badge = label.into();
                        row.busy = busy(label, "remove");
                    }
                }
            }
        }
    }

    fn observed(&self, target: &Target, board: &Board) -> bool {
        match self {
            Self::Create { resolved, .. } => resolved.as_ref().is_some_and(|key| {
                board
                    .rows
                    .iter()
                    .any(|row| &row.key == key && row.host == target.host)
            }),
            // The worker archives before acknowledging startup. That is
            // progress, not removal: keep showing the operation until Git
            // inventory no longer contains the checkout.
            Self::Remove { keys } => !keys
                .iter()
                .any(|key| board.rows.iter().any(|row| &row.key == key)),
        }
    }

    fn deadlines(&self) -> Option<Deadlines> {
        let deadline = match self {
            // The host waits for durable completion, including SST teardown
            // whose own timeout is twenty minutes.
            Self::Remove { .. } => Duration::from_secs(30 * 60),
            Self::Create { .. } => DEADLINE,
        };
        Some(Deadlines {
            in_flight: deadline,
            accepted: deadline,
        })
    }
}
