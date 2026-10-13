//! The user's pending requests over the source board: beginning, settling
//! and projecting them, and the separate creation progress they show.

use std::sync::Arc;

use wt_runtime::SourceSnapshot;

use super::{Board, Model, ToastTone};
use crate::{SessionTarget, UiAction, UiReply};

/// Interaction state of a pending creation, independent of worktree rows.
#[derive(Clone, Debug)]
pub(crate) struct CreateUi {
    pub host: Option<String>,
    pub resolved: Option<String>,
    pub open_agent: bool,
}

fn on_host(host: Option<String>, action: UiAction) -> UiAction {
    UiAction::OnHost {
        host,
        action: Box::new(action),
    }
}

impl Model {
    pub fn apply(&mut self, snapshot: SourceSnapshot<Board>) {
        self.source_state = snapshot.state().clone();
        self.local_domains = snapshot.domains().clone();
        if let Some(board) = snapshot.data() {
            self.source_board = board.clone();
            self.reproject(tokio::time::Instant::now());
        }
    }

    /// The latest fetch-start stamp for a host's domain. Restored data has no
    /// ordered stamp, so it can never confirm a pending change.
    pub(super) fn observed(
        &self,
        target: &wt_runtime::Target,
        domain: wt_core::Domain,
    ) -> Option<wt_runtime::Stamp> {
        let domains = match &target.host {
            None => &self.local_domains,
            Some(host) => self.source_board.host_domains.get(host)?,
        };
        domains
            .get(&domain)
            .filter(|meta| meta.is_live())
            .map(|meta| meta.observed())
    }

    /// Settle pending changes against the latest source board and present it
    /// with the remaining ones applied.
    pub(crate) fn reproject(&mut self, now: tokio::time::Instant) {
        let source = self.source_board.clone();
        let mut pending = std::mem::take(&mut self.pending);
        let expired = pending.settle(&source, |target, domain| self.observed(target, domain), now);
        self.pending = pending;
        self.creates.retain(|op, _| self.pending.contains(*op));
        if self
            .creation_focus
            .is_some_and(|id| !self.creates.contains_key(&id))
        {
            self.creation_focus = None;
        }
        for change in &expired {
            let text = crate::render::status::expired_notice(change);
            let source = change.mutation.target.key.as_deref().unwrap_or("wt");
            tracing::warn!(target: "wt_attention", event_channel = "attention", event_source = %source, event_text = %text, "pending change expired");
        }
        if let Some(first) = expired.first() {
            self.toast = Some((
                crate::render::status::expired_notice(first),
                ToastTone::Notice,
            ));
        }
        let board = if self.pending.is_empty() {
            source
        } else {
            let mut board = self.pending.project(&source);
            board.pending_views.extend(self.pending.views());
            Arc::new(board)
        };
        self.present(board);
    }

    /// When a pending change next needs re-checking against its deadline.
    pub fn next_deadline(&self) -> Option<tokio::time::Instant> {
        self.pending.next_deadline()
    }

    pub(crate) fn next_op_id(&mut self) -> u64 {
        self.next_op = self.next_op.wrapping_add(1);
        self.next_op
    }

    /// F12 while creation has focus opens the resolved checkout, or queues
    /// opening until its host returns the canonical identity.
    pub(super) fn request_agent(&mut self, id: u64) -> Option<UiAction> {
        let create = self.creates.get_mut(&id)?;
        create.open_agent = true;
        create.resolved.clone().map(|resolved| {
            on_host(
                create.host.clone(),
                UiAction::Session {
                    key: Some(resolved),
                    target: SessionTarget::Harness,
                },
            )
        })
    }

    /// Resolve what the user saw into the request the host executes: toggles
    /// become explicit directions and GitHub writes carry the PR they named.
    pub(crate) fn resolve(&self, action: UiAction) -> UiAction {
        let pin = |key: &str| {
            self.board
                .rows
                .iter()
                .find(|row| row.key == key)
                .and_then(|row| row.pr.as_ref())
                .and_then(|pr| {
                    pr.number.map(|number| crate::PrPin {
                        number,
                        head_sha: pr.head_sha.clone(),
                    })
                })
        };
        match action {
            UiAction::OnHost { host, action } => UiAction::OnHost {
                host,
                action: Box::new(self.resolve(*action)),
            },
            UiAction::PrepareAction {
                surface: crate::ActionSurface::Row { key },
                id,
                ..
            } if id == crate::AUTO_MERGE_ACTION_ID => {
                let armed = self
                    .board
                    .rows
                    .iter()
                    .find(|row| row.key == key)
                    .and_then(|row| row.pr.as_ref())
                    .is_some_and(|pr| pr.auto_merge_armed || pr.merge_queue.is_some());
                UiAction::GithubSetAutoMerge {
                    pin: pin(&key),
                    key,
                    enable: !armed,
                }
            }
            UiAction::GithubSetAutoMerge {
                key,
                enable,
                pin: None,
            } => UiAction::GithubSetAutoMerge {
                pin: pin(&key),
                key,
                enable,
            },
            UiAction::GithubMarkReady { key, pin: None } => UiAction::GithubMarkReady {
                pin: pin(&key),
                key,
            },
            UiAction::GithubShip { key, pin: None } => UiAction::GithubShip {
                pin: pin(&key),
                key,
            },
            action => action,
        }
    }

    /// Show the change an accepted request will make, before the host runs it.
    pub(crate) fn begin_action(&mut self, id: u64, action: &UiAction) {
        let crate::Effect::Mutation(mut mutation) = action.effect(&self.board) else {
            return;
        };
        if mutation.assign_creation(id) {
            self.creates.insert(
                id,
                CreateUi {
                    host: mutation.target.host.clone(),
                    resolved: None,
                    open_agent: false,
                },
            );
            self.creation_focus = Some(id);
            self.pending_selection = None;
        }
        let now = tokio::time::Instant::now();
        self.pending.begin(id, mutation, now);
        self.reproject(now);
    }

    /// Advance pending changes whose deadline passed.
    pub fn tick(&mut self, now: tokio::time::Instant) {
        if self.next_deadline().is_some_and(|deadline| deadline <= now) {
            self.reproject(now);
        }
    }

    /// Apply the host's answer to its pending change, and open the agent of
    /// a finished creation the user asked for with F12 while focused on its
    /// progress. Returns whether that creation is focused, which keeps
    /// the reply current for the user.
    pub(super) fn reply_pending(&mut self, reply: &UiReply) -> bool {
        let create = reply
            .operation_id
            .and_then(|id| self.creates.get(&id))
            .cloned();
        let selected_create =
            reply.operation_id.is_some() && self.creation_focus == reply.operation_id;
        if self.finish_pending(reply) {
            if let Some(create) = &create
                && selected_create
                && create.open_agent
                && !reply.failed
                && let Some(resolved) = &reply.select_when_visible
            {
                self.follow_up = Some(on_host(
                    create.host.clone(),
                    UiAction::Session {
                        key: Some(resolved.clone()),
                        target: SessionTarget::Harness,
                    },
                ));
            }
            self.reproject(tokio::time::Instant::now());
        }
        selected_create
    }

    /// Classify the host's answer for a pending change.
    fn finish_pending(&mut self, reply: &UiReply) -> bool {
        let Some(id) = reply.operation_id.filter(|id| self.pending.contains(*id)) else {
            return false;
        };
        let now = tokio::time::Instant::now();
        if matches!(
            reply.outcome,
            Some(crate::ReplyOutcome::Accepted(_) | crate::ReplyOutcome::Uncertain)
        ) {
            // Remote replies settle against stamps relayed by this process.
            wt_runtime::hold_for_settlement(std::time::Duration::from_secs(30));
        }
        let is_create = self.creates.contains_key(&id);
        if reply.failed || reply.modal.is_some() {
            self.pending.remove(id);
        } else {
            match reply.outcome {
                Some(crate::ReplyOutcome::Accepted(at)) => {
                    let resolved = reply.select_when_visible.clone();
                    if is_create && resolved.is_none() {
                        self.pending.remove(id);
                    } else {
                        self.pending.accept_with(id, at, now, |mutation| {
                            // The stamp names the host that executed the
                            // write. The controller runs some writes for
                            // remote rows itself (archive, sections, session
                            // entry); those are confirmed by this process's
                            // own reads, on the clock the stamp came from.
                            if at.local_instant().is_some() {
                                mutation.target.host = None;
                            }
                            mutation.on_accepted(reply);
                        });
                        if let Some(create) = self.creates.get_mut(&id) {
                            create.resolved = resolved;
                        }
                    }
                }
                Some(crate::ReplyOutcome::NoChange | crate::ReplyOutcome::Deferred) => {
                    self.pending.remove(id);
                }
                // An unclassified reply is treated as uncertain so a missing
                // classification shows up instead of silently reverting.
                Some(crate::ReplyOutcome::Uncertain) | None => {
                    self.pending.uncertain(id, now);
                }
            }
        }
        true
    }
}
