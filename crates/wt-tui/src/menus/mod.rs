//! Menus built from the presented board.
//!
//! A menu shows what the board already knows, opens without a host round
//! trip, and rebuilds while it stays open as fresher snapshots arrive. Its
//! commit carries what the user saw (a PR pin, a live slot identity, a
//! removal revision, a status claim), and the owning host checks that witness
//! before writing. A mismatch answers with a stale reply; the model reopens
//! the same menu from the newer board instead of applying the old view.
//!
//! [`UiAction::menu`] is the single declaration of where each menu comes
//! from. It has no wildcard arm: a new preparation must choose a board
//! builder or name the host-only data it needs.

mod status;

use crate::{Board, SessionTarget, UiAction, UiModal};

/// Where a menu request is answered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Menu {
    /// Not a menu request.
    NotMenu,
    /// Built from the board without asking the host.
    Local(LocalMenu),
    /// Needs data only the owning host can read. The model requests it and
    /// holds typed keys until it opens.
    Host { reason: &'static str },
}

/// A menu built from the board.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LocalMenu {
    /// Open this modal. `warm` is an optional read-only request sent in the
    /// background so a later rebuild can show fresher data.
    Modal {
        modal: Box<UiModal>,
        warm: Option<Box<UiAction>>,
    },
    /// Nothing to choose from; show this notice instead.
    Notice(String),
}

impl LocalMenu {
    pub(crate) fn modal(modal: UiModal) -> Self {
        Self::Modal {
            modal: Box::new(modal),
            warm: None,
        }
    }
}

/// The [`Board::session_menus`] key for a session picker target.
pub fn session_menu_key(key: Option<&str>, target: SessionTarget) -> String {
    match (target, key) {
        (SessionTarget::Harness, Some(key)) => key.to_owned(),
        (target, _) => format!("\0{target:?}"),
    }
}

impl UiAction {
    /// Where this action's menu comes from. Builders are pure functions of
    /// the board and must answer every board, including an empty one.
    pub fn menu(&self, board: &Board) -> Menu {
        use UiAction as A;
        match self {
            A::OnHost { action, .. } => match action.menu(board) {
                // A host-picked menu stays host-built; its rows are not on
                // this board.
                Menu::Local(_) => Menu::Host {
                    reason: "the menu was requested for a picked host",
                },
                menu => menu,
            },
            A::PrepareCreate { .. } => Menu::Host {
                reason: "offers host choices when remote hosts are configured",
            },
            A::ResolveCreation { .. } => Menu::Host {
                reason: "parses the request against the host's branches",
            },
            A::PrepareHardRefresh => Menu::Host {
                reason: "confirms clearing the host's derived caches",
            },
            A::PrepareRestoreRemoved { .. } => Menu::Host {
                reason: "reads the host's removal ledger",
            },
            A::PrepareGithub { .. } => Menu::Host {
                reason: "reads confirmed pull request data",
            },
            A::PrepareReviewers { .. } => Menu::Host {
                reason: "lists repository contributors from GitHub",
            },
            A::PrepareReviewCheckout { .. } => Menu::Host {
                reason: "reads the host's review requests",
            },
            A::GithubFailedChecks { .. } => Menu::Host {
                reason: "fetches the failed CI log from GitHub",
            },
            A::PrepareActions { .. } | A::PrepareAction { .. } => Menu::Host {
                reason: "reads action definitions, running actions and checkout state",
            },
            A::PrepareSection { .. } => Menu::Host {
                reason: "reads the manual section order",
            },
            A::PrepareRemove { .. } => Menu::Host {
                reason: "plans removal hazards from Git",
            },
            A::PrepareCleanup => Menu::Host {
                reason: "plans landing evidence on every host; `c` builds from the board",
            },
            A::PrepareStatus { key } => Menu::Local(status::menu(board, key)),
            A::PrepareBase { .. } => Menu::Host {
                reason: "reads recorded fork bases",
            },
            A::PrepareSessions { .. } => Menu::Host {
                reason: "reads conversation history and live tmux slots",
            },
            A::PrepareHarnesses { .. } => Menu::Host {
                reason: "reads the persisted primary harness",
            },
            A::PrepareStopTerminal { .. } => Menu::Host {
                reason: "reads live tmux terminals",
            },
            A::CreateResolved { .. }
            | A::ManagedSignIn { .. }
            | A::HardRefresh
            | A::SetAttentionSeen { .. }
            | A::ToggleAutomations { .. }
            | A::CancelAutomations
            | A::SetHistoryActive { .. }
            | A::SetPerf { .. }
            | A::RestoreRemoved { .. }
            | A::ToggleRemovedAutomations { .. }
            | A::OpenLink { .. }
            | A::OpenPrLink { .. }
            | A::OpenPrDefault { .. }
            | A::OpenSlotEditor { .. }
            | A::PerfInvestigate { .. }
            | A::Restack { .. }
            | A::ReviewCheckout { .. }
            | A::DismissReviewRequest { .. }
            | A::SubmitReviewers { .. }
            | A::GithubMarkReady { .. }
            | A::GithubSetAutoMerge { .. }
            | A::GithubShip { .. }
            | A::KillAction { .. }
            | A::RunAction { .. }
            | A::FoldSection { .. }
            | A::MoveSection { .. }
            | A::Reorder { .. }
            | A::RenameSection { .. }
            | A::CyclePrimary
            | A::SetTitle { .. }
            | A::GenerateTitle { .. }
            | A::Copy { .. }
            | A::Create { .. }
            | A::OpenEditor { .. }
            | A::Remove { .. }
            | A::Cleanup { .. }
            | A::CleanupTargets { .. }
            | A::ToggleArchive { .. }
            | A::SetStatus { .. }
            | A::SetBase { .. }
            | A::SetIssueOverride { .. }
            | A::OpenUrl { .. }
            | A::Session { .. }
            | A::SelectSession { .. }
            | A::KillSession { .. }
            | A::EnterHarness { .. }
            | A::StopTerminal { .. }
            | A::StopSession { .. } => Menu::NotMenu,
        }
    }

    /// Whether the board answers this request, so a host must never be asked.
    pub fn is_board_menu(&self) -> bool {
        matches!(self.menu(&Board::default()), Menu::Local(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_preparation_declares_a_menu_source() {
        // Spot-check the declaration agrees with `is_preparation`.
        for action in [
            UiAction::PrepareHardRefresh,
            UiAction::PrepareCleanup,
            UiAction::PrepareSessions {
                key: Some("one".into()),
                target: SessionTarget::Harness,
            },
        ] {
            assert!(action.is_preparation());
            assert_ne!(action.menu(&Board::default()), Menu::NotMenu);
        }
        assert_eq!(
            UiAction::CyclePrimary.menu(&Board::default()),
            Menu::NotMenu
        );
    }
}
