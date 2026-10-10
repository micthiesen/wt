//! Temporary lifecycle feedback. Source snapshots remain the authority.
use std::collections::BTreeMap;

use crate::{Board, BoardRow, BoardSection, SessionTarget, UiAction, UiReply};

#[derive(Default)]
pub(crate) struct LifecycleFeedback {
    next_id: u64,
    operations: BTreeMap<u64, Operation>,
}

enum Operation {
    Create {
        key: String,
        host: Option<String>,
        input: String,
        resolved: Option<String>,
        open_agent: bool,
    },
    Remove {
        keys: Vec<String>,
        accepted: bool,
    },
}

#[derive(Default)]
pub(crate) struct Completion {
    pub selected_create: bool,
    pub changed: bool,
    pub action: Option<UiAction>,
}

impl LifecycleFeedback {
    pub fn next_id(&mut self) -> u64 {
        self.next_id = self.next_id.wrapping_add(1);
        self.next_id
    }

    pub fn begin(&mut self, id: u64, action: &UiAction) -> Option<String> {
        let (host, action) = match action {
            UiAction::OnHost { host, action } => (host.clone(), action.as_ref()),
            action => (None, action),
        };
        let operation = match action {
            UiAction::Create { input } => Operation::Create {
                key: format!("\0creating:{id}"),
                host,
                input: input.clone(),
                resolved: None,
                open_agent: false,
            },
            UiAction::Remove { key, .. } => Operation::Remove {
                keys: vec![key.clone()],
                accepted: false,
            },
            _ => return None,
        };
        let selection = match &operation {
            Operation::Create { key, .. } => Some(key.clone()),
            _ => None,
        };
        self.operations.insert(id, operation);
        selection
    }

    pub fn contains(&self, id: u64) -> bool {
        self.operations.contains_key(&id)
    }

    pub fn is_placeholder(&self, key: &str) -> bool {
        self.operations.values().any(|operation| matches!(operation, Operation::Create { key: pending, .. } if pending == key))
    }

    pub fn request_agent(&mut self, key: &str) -> Option<UiAction> {
        for operation in self.operations.values_mut() {
            if let Operation::Create {
                key: pending,
                host,
                resolved,
                open_agent,
                ..
            } = operation
                && pending == key
            {
                *open_agent = true;
                if let Some(key) = resolved {
                    return Some(on_host(
                        host.clone(),
                        UiAction::Session {
                            key: Some(key.clone()),
                            target: SessionTarget::Harness,
                        },
                    ));
                }
            }
        }
        None
    }

    pub fn finish(&mut self, reply: &UiReply, selected: Option<&str>) -> Completion {
        let Some(id) = reply.operation_id else {
            return Completion::default();
        };
        let Some(operation) = self.operations.get_mut(&id) else {
            return Completion::default();
        };
        let mut completion = Completion {
            changed: true,
            selected_create: matches!(operation, Operation::Create { key, .. } if selected == Some(key.as_str())),
            ..Default::default()
        };
        let discard = if reply.failed || reply.modal.is_some() {
            true
        } else {
            match operation {
                Operation::Create {
                    key,
                    host,
                    resolved,
                    open_agent,
                    ..
                } => {
                    completion.selected_create = selected == Some(key.as_str());
                    *resolved = reply.select_when_visible.clone();
                    if completion.selected_create
                        && *open_agent
                        && let Some(key) = resolved
                    {
                        completion.action = Some(on_host(
                            host.clone(),
                            UiAction::Session {
                                key: Some(key.clone()),
                                target: SessionTarget::Harness,
                            },
                        ));
                    }
                    resolved.is_none()
                }
                Operation::Remove { accepted, .. } => {
                    *accepted = true;
                    false
                }
            }
        };
        if discard {
            self.operations.remove(&id);
        }
        completion
    }

    /// Whether any operation still needs projecting onto snapshots.
    pub fn is_empty(&self) -> bool {
        self.operations.is_empty()
    }

    pub fn project(&mut self, board: &mut Board) {
        self.operations.retain(|_, operation| match operation {
            Operation::Create {
                resolved: Some(key),
                ..
            } => !board.rows.iter().any(|row| &row.key == key),
            Operation::Create { .. } => true,
            Operation::Remove { keys, accepted } => {
                !*accepted
                    || keys.iter().any(|key| {
                        board
                            .rows
                            .iter()
                            .any(|row| &row.key == key && !row.archived)
                    })
            }
        });
        for operation in self.operations.values() {
            match operation {
                Operation::Create {
                    key,
                    host,
                    input,
                    resolved,
                    ..
                } => {
                    let index = board.rows.len();
                    board.rows.push(BoardRow {
                        key: key.clone(),
                        host: host.clone(),
                        title: input.clone(),
                        busy: Some(crate::BusyView {
                            op: "init".into(),
                            label: if resolved.is_some() {
                                "ready · updating inventory"
                            } else {
                                "creating"
                            }
                            .into(),
                            ..Default::default()
                        }),
                        detail_groups: vec![crate::PreparedDetailGroup {
                            id: "git".into(),
                            label: "git".into(),
                            ..Default::default()
                        }],
                        badge: if resolved.is_some() {
                            "ready · updating inventory"
                        } else {
                            "creating · F12 to open when ready"
                        }
                        .into(),
                        details: vec![
                            if resolved.is_some() {
                                "Checkout ready. Waiting for inventory."
                            } else {
                                "Creating checkout. F12 will open the agent when ready."
                            }
                            .into(),
                        ],
                        ..Default::default()
                    });
                    let inbox = section(board, "\0inbox", "Inbox");
                    board.sections[inbox].rows.push(index);
                    board.sections[inbox].folded = false;
                }
                Operation::Remove { keys, .. } => {
                    for key in keys {
                        if let Some(index) = board.rows.iter().position(|row| &row.key == key) {
                            for section in &mut board.sections {
                                section.rows.retain(|row| *row != index);
                            }
                            board.rows[index].archived = true;
                            board.rows[index].badge = "removing".into();
                            board.rows[index].busy = Some(crate::BusyView {
                                op: "remove".into(),
                                label: "removing".into(),
                                ..Default::default()
                            });
                            let archive = section(board, "\0archived", "Archived");
                            board.sections[archive].rows.push(index);
                        }
                    }
                }
            }
        }
    }
}

fn section(board: &mut Board, key: &str, title: &str) -> usize {
    if let Some(index) = board.sections.iter().position(|section| section.key == key) {
        return index;
    }
    board.sections.push(BoardSection {
        key: key.into(),
        title: title.into(),
        folded: key == "\0archived",
        ..Default::default()
    });
    board.sections.len() - 1
}

fn on_host(host: Option<String>, action: UiAction) -> UiAction {
    UiAction::OnHost {
        host,
        action: Box::new(action),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn board() -> Board {
        Board {
            rows: vec![BoardRow {
                key: "old".into(),
                ..Default::default()
            }],
            sections: vec![BoardSection {
                key: "\0inbox".into(),
                rows: vec![0],
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn creation_survives_stale_snapshots_and_opens_the_captured_host() {
        let mut feedback = LifecycleFeedback::default();
        let action = on_host(
            Some("remote".into()),
            UiAction::Create {
                input: "new --base staging".into(),
            },
        );
        let key = feedback.begin(1, &action).unwrap();
        let source = board();
        let mut projected = source.clone();
        feedback.project(&mut projected);
        assert_eq!(projected.rows.len(), 2);
        assert_eq!(source.rows.len(), 1);
        assert!(feedback.request_agent(&key).is_none());
        assert!(
            !feedback
                .finish(
                    &UiReply {
                        operation_id: Some(2),
                        failed: true,
                        ..Default::default()
                    },
                    Some(&key)
                )
                .changed
        );
        let completion = feedback.finish(
            &UiReply {
                operation_id: Some(1),
                select_when_visible: Some("remote/new".into()),
                ..Default::default()
            },
            Some(&key),
        );
        assert!(completion.selected_create);
        assert_eq!(
            completion.action,
            Some(on_host(
                Some("remote".into()),
                UiAction::Session {
                    key: Some("remote/new".into()),
                    target: SessionTarget::Harness
                }
            ))
        );
        let mut stale = source;
        feedback.project(&mut stale);
        assert_eq!(
            stale.rows.last().unwrap().badge,
            "ready · updating inventory"
        );
        let mut fresh = board();
        fresh.rows.push(BoardRow {
            key: "remote/new".into(),
            ..Default::default()
        });
        feedback.project(&mut fresh);
        assert_eq!(fresh.rows.len(), 2);
        assert!(!feedback.contains(1));
    }

    #[test]
    fn failed_creation_removes_only_its_own_placeholder() {
        let mut feedback = LifecycleFeedback::default();
        let action = UiAction::Create {
            input: "same".into(),
        };
        let first = feedback.begin(1, &action).unwrap();
        let second = feedback.begin(2, &action).unwrap();
        assert_ne!(first, second);
        feedback.finish(
            &UiReply {
                operation_id: Some(1),
                failed: true,
                ..Default::default()
            },
            Some(&first),
        );
        let mut projected = board();
        feedback.project(&mut projected);
        assert_eq!(projected.rows.len(), 2);
        assert_eq!(projected.rows.last().unwrap().key, second);
    }

    #[test]
    fn removal_moves_the_row_and_failure_restores_the_latest_source() {
        let mut feedback = LifecycleFeedback::default();
        let revision = crate::RemovalRevision {
            key: "old".into(),
            path: "path".into(),
            branch: "branch".into(),
            head: "head".into(),
            digest: "digest".into(),
            hazards: Vec::new(),
            published_base: None,
        };
        feedback.begin(
            1,
            &UiAction::Remove {
                key: "old".into(),
                force: false,
                revision,
            },
        );
        let source = board();
        let mut projected = source.clone();
        feedback.project(&mut projected);
        assert!(projected.rows[0].archived);
        assert!(projected.sections[0].rows.is_empty());
        assert_eq!(projected.sections.last().unwrap().rows, [0]);
        feedback.finish(
            &UiReply {
                operation_id: Some(1),
                failed: true,
                ..Default::default()
            },
            None,
        );
        let mut latest = source;
        latest.rows[0].title = "New title".into();
        feedback.project(&mut latest);
        assert!(!latest.rows[0].archived);
        assert_eq!(latest.rows[0].title, "New title");
        assert_eq!(latest.sections[0].rows, [0]);
    }
}
