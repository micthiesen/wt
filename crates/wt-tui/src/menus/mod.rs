//! Menus built from the presented board.
//!
//! A menu shows what the board already knows, opens without a host round
//! trip, and rebuilds while it stays open as fresher snapshots arrive. Its
//! commit carries what the user saw (a PR pin, a live slot identity, a
//! removal revision, a status claim), and the owning host checks that witness
//! before writing. A mismatch answers with a stale reply; the model reopens
//! the same menu from the newer board instead of applying the old view.
//!
//! Content only a host can fetch (a CI log, a live terminal) is not a menu
//! of choices: its overlay opens at the keypress with a loading body, owns
//! the keys, and is filled by the host's reply.
//!
//! [`UiAction::menu`] is the single declaration of where each menu comes
//! from. It has no wildcard arm: a new preparation must choose a board
//! builder or name the host-only content it fetches.

mod base;
pub(crate) mod notify;
pub(crate) mod palette;
mod remove;
mod section;
mod status;

mod confirms;
pub(crate) mod github;
pub(crate) mod sessions;

pub use confirms::new_worktree_prompt;
pub(crate) use section::NEW_SECTION;

use crate::{Board, SessionTarget, UiAction, UiModal};

/// Where a menu request is answered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Menu {
    /// Not a menu request.
    NotMenu,
    /// Built from the board without asking the host.
    Local(LocalMenu),
    /// Content only the owning host can fetch. Its overlay opens at the
    /// keypress and takes the keys; the request's reply fills it.
    Overlay(LoadingOverlay),
    /// A read the owning host makes as a step of another request. No key
    /// opens one, so nothing waits on it with an empty screen.
    Host { reason: &'static str },
}

/// The overlay shown while its content is fetched.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadingOverlay {
    pub title: String,
    /// An extra key that closes it, the one the filled overlay will have.
    pub close_key: Option<char>,
}

/// A menu built from the board.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LocalMenu {
    /// Open this modal. `warm` is an optional read-only request sent in the
    /// background so a later rebuild can show fresher data.
    Modal {
        modal: Box<UiModal>,
        warm: Option<Box<UiAction>>,
        /// The host that answers the menu's commits, for a menu about another
        /// host's data that its commit cannot route by worktree key.
        host: Option<String>,
    },
    /// Nothing to choose from; show this notice instead.
    Notice(String),
    /// The board has no answer yet: say it and ask the sources to read
    /// again, instead of holding the key until a host has waited for them.
    NotYet(String),
    /// The board's answer may be behind what the person knows, and one
    /// narrow read would tell: say it and send that read in the background,
    /// without re-reading everything else.
    Ask { text: String, read: Box<UiAction> },
}

impl LocalMenu {
    pub(crate) fn modal(modal: UiModal) -> Self {
        Self::Modal {
            modal: Box::new(modal),
            warm: None,
            host: None,
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
            // The host is already chosen, so the name prompt opens for it.
            A::OnHost { host, action } => match &**action {
                A::PrepareCreate { initial, placement } => Menu::Local(LocalMenu::Modal {
                    modal: Box::new(new_worktree_prompt(initial, placement.clone())),
                    warm: None,
                    host: host.clone(),
                }),
                // A row is named by its key, which carries its host; a
                // request that names another host's rows by their own keys
                // cannot be answered from this board.
                action => match action.menu(board) {
                    Menu::Local(_) => Menu::Host {
                        reason: "names rows by another host's keys",
                    },
                    menu => menu,
                },
            },
            A::PrepareCreate { initial, placement } => {
                confirms::create(board, initial, placement.clone())
            }
            A::ResolveCreation { .. } => Menu::Host {
                reason: "parses the request against the host's branches",
            },
            A::PrepareHardRefresh => Menu::Local(confirms::hard_refresh()),
            A::PrepareRestoreRemoved { key } => Menu::Local(confirms::restore_removed(board, key)),
            A::PrepareGithub { key, ship } => Menu::Local(github::menu(board, key, *ship)),
            A::PrepareReviewers { key } => Menu::Local(github::reviewers(board, key)),
            A::PrepareReviewCheckout {
                url,
                updated_at,
                branch,
                host,
            } => Menu::Local(confirms::review_checkout(
                board,
                url,
                updated_at,
                branch,
                host.as_deref(),
            )),
            // The CI log is fetched from GitHub when the board shows a
            // failing check.
            A::GithubFailedChecks { key } => github::failed_checks(board, key),
            A::PrepareActions { surface } => palette::menu(board, surface),
            // The dev server's terminal output is read live from its host.
            A::PrepareDevLogs { key } => Menu::Overlay(LoadingOverlay {
                title: match board.rows.iter().find(|row| row.key == *key) {
                    Some(row) => format!("{} dev logs", row.slug),
                    None => "dev logs".into(),
                },
                close_key: Some('l'),
            }),
            A::PrepareSection { key } => Menu::Local(section::menu(board, key)),
            A::PrepareDeleteSection { section } => {
                Menu::Local(section::delete_confirm(board, section))
            }
            A::PrepareRemove { key } => remove::menu(board, key),
            A::PrepareStatus { key } => Menu::Local(status::menu(board, key)),
            A::PrepareBase { key } => base::menu(board, key),
            A::PrepareNotify { key } => Menu::Local(notify::menu(board, key)),
            A::PrepareSessions { key, target } => {
                Menu::Local(sessions::sessions_menu(board, key.as_deref(), *target))
            }
            A::PrepareHarnesses { key } => Menu::Local(sessions::harness_menu(board, key)),
            A::PrepareStopTerminal { key, target } => {
                Menu::Local(sessions::stop_terminal_menu(board, key, *target))
            }
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
            | A::DeleteSection { .. }
            | A::CyclePrimary
            | A::SetTitle { .. }
            | A::Copy { .. }
            | A::Create { .. }
            | A::OpenEditor { .. }
            | A::Remove { .. }
            | A::DismissFailedRemoval { .. }
            | A::CleanupTargets { .. }
            | A::ToggleArchive { .. }
            | A::SetStatus { .. }
            | A::SetBase { .. }
            | A::SetNotify { .. }
            | A::SetIssueOverride { .. }
            | A::OpenUrl { .. }
            | A::Session { .. }
            | A::SelectSession { .. }
            | A::KillSession { .. }
            | A::EnterHarness { .. }
            | A::StopTerminal { .. }
            | A::StopSession { .. }
            | A::DiscoverSessions { .. }
            | A::ReadPullRequest { .. } => Menu::NotMenu,
        }
    }

    /// Whether the board answers this request, so a host must never be asked.
    /// Asked of the empty board: a request a host ever answers says so there.
    pub fn is_board_menu(&self) -> bool {
        matches!(self.menu(&Board::default()), Menu::Local(_))
    }
}

/// The display policy of the host a row lives on: a remote row's menus read
/// its own host's configuration, not this machine's.
pub(crate) fn row_display<'a>(board: &'a Board, row: &crate::BoardRow) -> &'a crate::DisplayPolicy {
    row.host
        .as_ref()
        .and_then(|host| board.host_display.get(host))
        .unwrap_or(&board.display)
}

/// What to say when a remote row's host has not sent what a menu needs.
pub(crate) fn host_unknown(board: &Board, row: &crate::BoardRow, what: &str) -> LocalMenu {
    match row
        .host
        .as_ref()
        .and_then(|host| board.host_errors.get(host))
    {
        Some(error) => LocalMenu::Notice(format!("{what} unknown: {error}")),
        None => LocalMenu::NotYet(format!("{what} not loaded yet")),
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
            UiAction::ResolveCreation {
                input: "one".into(),
            },
            UiAction::GithubFailedChecks { key: "one".into() },
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
