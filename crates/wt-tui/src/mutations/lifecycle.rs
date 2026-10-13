//! Creating and removing worktrees. Creation tracks progress without inventing
//! inventory rows; only the host can establish a checkout identity.
use std::time::Duration;

use wt_core::Domain;
use wt_runtime::{Deadlines, Target};

use super::{ARCHIVED_SECTION, ARCHIVED_TITLE, Family, place};
use crate::{Board, BusyView};

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

    fn apply(&self, _target: &Target, board: &mut Board) {
        match self {
            // A creation is an operation, not a worktree. Inventory may publish
            // its row before the command replies, including on remote hosts.
            // Never fabricate a second identity that needs deduplication.
            Self::Create { .. } => {}
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
            Self::Create { resolved, .. } => resolved
                .as_ref()
                .is_some_and(|key| board.rows.iter().any(|row| &row.key == key)),
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
