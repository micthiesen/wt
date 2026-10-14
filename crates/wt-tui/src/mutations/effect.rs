//! The effect of every `UiAction`, declared with an exhaustive match so a
//! new action cannot ship without deciding how it appears while pending.
use wt_core::Domain;
use wt_runtime::{Mutation, Target};

use super::{
    GithubChange, LayoutChange, LifecycleChange, MutationKind, RowChange, SETTLING_DOMAINS,
    UiMutation, row,
};
use crate::{Board, UiAction};

/// Sentinel from the section picker that opens the "new section" prompt.
pub(super) use crate::menus::NEW_SECTION;

/// How an action affects what the board shows.
#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    /// Opens a prompt, reads, or acts outside wt's own data.
    ReadOnly,
    /// Changes data the board shows; tracked until a fetch confirms it.
    Mutation(UiMutation),
}

impl Effect {
    /// What the executing host refreshes after the action.
    pub fn domains(&self) -> &'static [Domain] {
        match self {
            Self::ReadOnly => &[],
            Self::Mutation(mutation) => mutation.domains(),
        }
    }
}

impl UiAction {
    /// The effect of this action given the board the user is looking at.
    /// Direction-dependent actions (toggles) resolve against that board, so
    /// the request carries what the user saw rather than what the host's
    /// possibly older data says.
    pub fn effect(&self, board: &Board) -> Effect {
        effect(self, None, board)
    }
}

/// A change to one row, aimed at the host that row lives on.
fn on_row(
    board: &Board,
    host: &Option<String>,
    key: &str,
    kind: impl Into<MutationKind>,
) -> Effect {
    let host = row(board, key)
        .map(|row| row.host.clone())
        .unwrap_or_else(|| host.clone());
    Effect::Mutation(UiMutation {
        target: Target::row(host, key),
        kind: kind.into(),
    })
}

fn on_board(host: &Option<String>, kind: impl Into<MutationKind>) -> Effect {
    Effect::Mutation(UiMutation {
        target: Target::board(host.clone()),
        kind: kind.into(),
    })
}

fn unprojected(
    host: &Option<String>,
    key: Option<&str>,
    label: &'static str,
    domains: &'static [Domain],
) -> Effect {
    debug_assert!(
        domains
            .iter()
            .all(|domain| SETTLING_DOMAINS.contains(domain)),
        "{label} waits on a domain no source publishes"
    );
    Effect::Mutation(UiMutation {
        target: Target {
            host: host.clone(),
            key: key.map(str::to_owned),
        },
        kind: MutationKind::Unprojected { label, domains },
    })
}

fn effect(action: &UiAction, host: Option<String>, board: &Board) -> Effect {
    let host = &host;
    match action {
        UiAction::OnHost { host, action } => effect(action, host.clone(), board),
        UiAction::PrepareCreate { .. }
        | UiAction::ResolveCreation { .. }
        | UiAction::PrepareHardRefresh
        | UiAction::SetHistoryActive { .. }
        | UiAction::SetPerf { .. }
        | UiAction::PrepareRestoreRemoved { .. }
        | UiAction::OpenLink { .. }
        | UiAction::OpenPrLink { .. }
        | UiAction::OpenPrDefault { .. }
        | UiAction::OpenSlotEditor { .. }
        | UiAction::PrepareGithub { .. }
        | UiAction::PrepareReviewers { .. }
        | UiAction::PrepareReviewCheckout { .. }
        | UiAction::PrepareActions { .. }
        | UiAction::WarmPalette { .. }
        | UiAction::PrepareSection { .. }
        | UiAction::GenerateTitle { .. }
        | UiAction::Copy { .. }
        | UiAction::OpenEditor { .. }
        | UiAction::PrepareRemove { .. }
        | UiAction::PrepareCleanup
        | UiAction::PrepareStatus { .. }
        | UiAction::PrepareBase { .. }
        | UiAction::OpenUrl { .. }
        | UiAction::PrepareSessions { .. }
        | UiAction::PrepareHarnesses { .. }
        | UiAction::PrepareStopTerminal { .. }
        | UiAction::DiscoverSessions { .. } => Effect::ReadOnly,
        // A palette entry only opens its prompt; merge-when-ready is resolved
        // into `GithubSetAutoMerge` before sending (`Model::resolve`).
        UiAction::PrepareAction { .. } => Effect::ReadOnly,
        // Credentials and any resumed action are handled by the interactive
        // sign-in flow; there is no sign-in board mutation to confirm.
        UiAction::ManagedSignIn { .. } => Effect::ReadOnly,
        UiAction::HardRefresh => unprojected(host, None, "refreshing", &[]),
        UiAction::SetAttentionSeen { at_ms } => {
            on_board(host, LayoutChange::AttentionSeen { at_ms: *at_ms })
        }
        UiAction::ToggleAutomations { key: Some(key) } => {
            let paused = row(board, key).is_some_and(|row| row.automations_paused);
            on_row(board, host, key, RowChange::Automations { paused: !paused })
        }
        UiAction::ToggleAutomations { key: None } => on_board(
            host,
            RowChange::Automations {
                paused: !board.automations_paused,
            },
        ),
        UiAction::CancelAutomations => unprojected(host, None, "cancelling automations", &[]),
        UiAction::RestoreRemoved { key, .. } => unprojected(
            host,
            Some(key),
            "restoring",
            &[Domain::Git, Domain::Metadata],
        ),
        UiAction::ToggleRemovedAutomations { key } => {
            unprojected(host, Some(key), "toggling automations", &[Domain::Metadata])
        }
        UiAction::PerfInvestigate { .. } => {
            unprojected(host, None, "sending investigation", &[Domain::Sessions])
        }
        UiAction::Restack { key } => unprojected(
            host,
            Some(key),
            "restacking",
            &[Domain::Actions, Domain::Git],
        ),
        UiAction::ReviewCheckout { .. } => unprojected(
            host,
            None,
            "checking out review",
            &[Domain::Git, Domain::Metadata, Domain::Reviews],
        ),
        UiAction::DismissReviewRequest { url, .. } => {
            on_board(host, GithubChange::DismissReview { url: url.clone() })
        }
        UiAction::SubmitReviewers { key, selected, .. } => on_row(
            board,
            host,
            key,
            GithubChange::Reviewers {
                reviewers: selected.clone(),
            },
        ),
        UiAction::GithubMarkReady { key, .. } => on_row(board, host, key, GithubChange::MarkReady),
        UiAction::GithubSetAutoMerge { key, enable, .. } => on_row(
            board,
            host,
            key,
            GithubChange::AutoMerge { enable: *enable },
        ),
        UiAction::GithubShip { key, .. } => on_row(board, host, key, GithubChange::Ship),
        UiAction::GithubFailedChecks { key } => unprojected(
            host,
            Some(key),
            "sending failed checks",
            &[Domain::Sessions],
        ),
        UiAction::KillAction { .. } => {
            unprojected(host, None, "stopping action", &[Domain::Actions])
        }
        UiAction::RunAction { surface, .. } => {
            let key = match surface {
                crate::ActionSurface::Row { key } => Some(key.as_str()),
                _ => None,
            };
            unprojected(
                host,
                key,
                "starting action",
                &[Domain::Actions, Domain::Sessions],
            )
        }
        UiAction::FoldSection { key, folded } => on_board(
            host,
            LayoutChange::FoldSection {
                section: key.clone(),
                folded: *folded,
            },
        ),
        UiAction::MoveSection { section, .. } if section.as_deref() == Some(NEW_SECTION) => {
            Effect::ReadOnly
        }
        UiAction::MoveSection { key, section, .. } => on_row(
            board,
            host,
            key,
            LayoutChange::MoveSection {
                section: section.clone(),
            },
        ),
        UiAction::Reorder { key, .. } => {
            unprojected(host, key.as_deref(), "reordering", &[Domain::Metadata])
        }
        UiAction::RenameSection { old, new } => on_board(
            host,
            LayoutChange::RenameSection {
                old: old.clone(),
                new: new.clone(),
            },
        ),
        UiAction::CyclePrimary => on_board(host, RowChange::Primary { harness: None }),
        UiAction::SetTitle { key, title } => on_row(
            board,
            host,
            key,
            RowChange::Title {
                title: title.clone(),
            },
        ),
        UiAction::Create { input } => on_board(
            host,
            LifecycleChange::Create {
                request_id: 0,
                input: input.clone(),
                resolved: None,
            },
        ),
        UiAction::CreateResolved { plan } => on_board(
            host,
            LifecycleChange::Create {
                request_id: 0,
                input: plan.branch.clone(),
                resolved: Some(plan.key.clone()),
            },
        ),
        UiAction::Remove { key, .. } => on_row(
            board,
            host,
            key,
            LifecycleChange::Remove {
                keys: vec![key.clone()],
            },
        ),
        UiAction::CleanupTargets { targets } => on_board(
            host,
            LifecycleChange::Remove {
                keys: targets.iter().map(|target| target.key.clone()).collect(),
            },
        ),
        UiAction::Cleanup { revisions } => on_board(
            host,
            LifecycleChange::Remove {
                keys: revisions.iter().map(|r| r.key.clone()).collect(),
            },
        ),
        UiAction::ToggleArchive { key } => {
            let archived = row(board, key).is_some_and(|row| row.archived);
            on_row(
                board,
                host,
                key,
                LayoutChange::Archive {
                    archived: !archived,
                },
            )
        }
        UiAction::SetStatus { key, state, .. } => {
            match state.as_deref().and_then(wt_core::resolve_work_state) {
                Some(state) => on_row(board, host, key, RowChange::Status { state }),
                None => unprojected(host, Some(key), "updating status", &[Domain::Metadata]),
            }
        }
        UiAction::SetBase { key, base, .. } => {
            on_row(board, host, key, RowChange::Base { base: base.clone() })
        }
        UiAction::SetIssueOverride { key, issue_id } => on_row(
            board,
            host,
            key,
            RowChange::Issue {
                issue_id: issue_id.clone(),
            },
        ),
        // Entry hands the terminal to an interactive process, which may run
        // indefinitely. It has no host write stamp to settle against; session
        // inventory independently observes anything the process creates.
        UiAction::Session { .. }
        | UiAction::SelectSession { .. }
        | UiAction::EnterHarness { .. } => Effect::ReadOnly,
        UiAction::KillSession { .. } => {
            unprojected(host, None, "stopping session", &[Domain::Sessions])
        }
        UiAction::StopTerminal { key, .. } => {
            unprojected(host, Some(key), "stopping terminal", &[Domain::Sessions])
        }
        UiAction::StopSession { .. } => {
            unprojected(host, None, "stopping session", &[Domain::Sessions])
        }
    }
}
