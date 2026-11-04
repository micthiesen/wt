//! Cleanup review from the prepared board. Opening this menu does no I/O.
use std::collections::BTreeSet;

use ratatui::style::Color;

use crate::{Board, BoardRow, CleanupTarget, ConfirmAction, UiModal, theme};

/// Title of the overlay shown when nothing can be cleaned.
pub(crate) const LOG_TITLE: &str = "Clean worktrees";
const NOT_LANDED: &str = "landing of its own work is not proven";
const HOST_UNAVAILABLE: &str = "host is unavailable";
const CHECKOUT_UNAVAILABLE: &str = "checkout is unavailable";
const OPERATION_IN_PROGRESS: &str = "operation in progress";
const GIT_UNAVAILABLE: &str = "Git state is unavailable";
const CHANGES_LOADING: &str = "checkout changes are still loading";

/// What the open cleanup menu decided for a row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CleanupMark {
    /// Confirming removes it.
    Remove,
    /// Its work landed, but something still holds the checkout.
    Held,
    /// wt cannot tell yet.
    Unknown,
    /// Its work has not landed.
    Unlanded,
}

impl CleanupMark {
    pub(crate) fn from_reason(reason: &str) -> Self {
        match reason {
            NOT_LANDED => Self::Unlanded,
            HOST_UNAVAILABLE
            | CHECKOUT_UNAVAILABLE
            | OPERATION_IN_PROGRESS
            | GIT_UNAVAILABLE
            | CHANGES_LOADING => Self::Unknown,
            _ => Self::Held,
        }
    }

    /// The color of a kept row's title and of its reason in the menu.
    pub(crate) fn color(self) -> Color {
        match self {
            Self::Remove => theme::OK,
            Self::Held => theme::WARN,
            Self::Unknown => theme::INFO,
            Self::Unlanded => theme::FG_DIM,
        }
    }
}

/// The open cleanup menu's decisions, for painting the list behind it.
pub(crate) struct CleanupPreview<'a> {
    targets: BTreeSet<&'a str>,
}

impl<'a> CleanupPreview<'a> {
    pub(crate) fn new(targets: impl IntoIterator<Item = &'a str>) -> Self {
        Self {
            targets: targets.into_iter().collect(),
        }
    }

    /// The displayed targets are what confirming acts on, so they decide
    /// `Remove` even after a refresh. A row that became cleanable since the
    /// menu opened is not one of them, and reads as unknown until reopened.
    pub(crate) fn mark(&self, board: &Board, row: &BoardRow) -> Option<CleanupMark> {
        if row.key.starts_with('\0') {
            return None;
        }
        if self.targets.contains(row.key.as_str()) {
            return Some(CleanupMark::Remove);
        }
        Some(hold_reason(board, row).map_or(CleanupMark::Unknown, CleanupMark::from_reason))
    }
}

pub(crate) fn menu(board: &Board) -> UiModal {
    let mut targets = Vec::new();
    let mut lines = vec!["From background results. Removal checks current state again.".into()];
    let mut kept = Vec::new();
    let mut removed = Vec::new();
    for (host, error) in &board.host_errors {
        let host = host_label(board, host);
        lines.push(format!("{host}: unavailable: {error}"));
    }
    for row in &board.rows {
        // Temporary creation rows have no checkout to review.
        if row.key.starts_with('\0') {
            continue;
        }
        let label = match &row.host {
            Some(host) => format!("{}: ", host_label(board, host)),
            None => String::new(),
        };
        let name = if row.slug.is_empty() {
            &row.key
        } else {
            &row.slug
        };
        if let Some(reason) = hold_reason(board, row) {
            kept.push(format!("{label}Keep {name}: {reason}"));
        } else {
            removed.push(format!("{label}Remove {name}"));
            targets.push(CleanupTarget {
                key: row.key.clone(),
                path: row.path.clone(),
                branch: row.branch.clone(),
                head: row.git.head_sha.clone().expect("candidate HEAD is known"),
            });
        }
    }
    if !removed.is_empty() {
        lines.push(String::new());
        lines.push(format!("Ready to remove · {}", removed.len()));
        lines.extend(removed);
    }
    if !kept.is_empty() {
        lines.push(String::new());
        lines.push(format!("Keeping · {}", kept.len()));
        lines.extend(kept);
    }
    if targets.is_empty() {
        lines.insert(0, "Nothing safe to clean in the background results.".into());
        return UiModal::Log {
            title: LOG_TITLE.into(),
            lines,
            close_key: Some('c'),
            refresh: None,
        };
    }
    UiModal::Confirm {
        title: format!(
            "Clean {} landed worktree{}?",
            targets.len(),
            if targets.len() == 1 { "" } else { "s" }
        ),
        lines,
        action: ConfirmAction::CleanupTargets { targets },
        cancel_key: Some('c'),
    }
}

fn host_label<'a>(board: &'a Board, host: &'a str) -> &'a str {
    board
        .hosts
        .iter()
        .find(|choice| choice.id.as_deref() == Some(host))
        .map_or(host, |choice| choice.label.as_str())
}

fn hold_reason(board: &Board, row: &BoardRow) -> Option<&'static str> {
    let host_down = row
        .host
        .as_ref()
        .is_some_and(|host| board.host_errors.contains_key(host));
    if host_down {
        Some(HOST_UNAVAILABLE)
    } else {
        keep_reason(row)
    }
}

fn keep_reason(row: &BoardRow) -> Option<&'static str> {
    checkout_hold_reason(row, true)
}

/// Why a row's checkout cannot be removed from the board alone. Shared by the
/// `c` cleanup menu and the `d` remove menu. Cleanup also needs proof that the
/// row's own work landed; the remove menu does not, because the host plans
/// unpushed work itself and returns its hazards for review.
pub(crate) fn checkout_hold_reason(row: &BoardRow, landing_required: bool) -> Option<&'static str> {
    if row.path_missing || row.path.is_empty() {
        Some(CHECKOUT_UNAVAILABLE)
    } else if row.busy.is_some() {
        Some(OPERATION_IN_PROGRESS)
    } else if row.git.error.is_some() || row.git.head_sha.as_deref().is_none_or(str::is_empty) {
        Some(GIT_UNAVAILABLE)
    } else if landing_required && row.git.landed_on.is_none() {
        Some(NOT_LANDED)
    } else if landing_required
        && row.work.as_ref().is_some_and(|work| {
            matches!(
                work.effective_state,
                Some(wt_core::WorkState::Working | wt_core::WorkState::NeedsHuman)
            )
        })
    {
        // Whatever Git says, an agent still at work (or waiting on the
        // human) has not finished with this checkout.
        Some("work is still in progress")
    } else if row.work.as_ref().is_some_and(|work| work.verification_owed) {
        Some("post-merge verification still owed")
    } else if row.git.rebasing || !row.git.conflict_files.is_empty() {
        Some("rebase or merge is in progress")
    } else if row.git.tracked_changes.is_none() || row.git.untracked_files.is_none() {
        Some(CHANGES_LOADING)
    } else if row.git.tracked_changes != Some(0) || row.git.untracked_files != Some(0) {
        Some("uncommitted changes")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GitPresentation, LandingKind, WorkPresentation};

    fn landed(slug: &str) -> BoardRow {
        BoardRow {
            key: slug.into(),
            slug: slug.into(),
            path: format!("/worktrees/{slug}"),
            branch: format!("feature/{slug}"),
            git: GitPresentation {
                head_sha: Some("head".into()),
                landed_on: Some(LandingKind::Base),
                tracked_changes: Some(0),
                untracked_files: Some(0),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn marks_follow_the_displayed_targets_and_each_rows_reason() {
        let mut dirty = landed("dirty");
        dirty.git.untracked_files = Some(1);
        let mut unlanded = landed("unlanded");
        unlanded.git.landed_on = None;
        let mut loading = landed("loading");
        loading.git.tracked_changes = None;
        let mut down = landed("down");
        down.host = Some("builder".into());
        // Its checkout changed after the menu opened; it stays a target.
        let mut target = landed("target");
        target.git.untracked_files = Some(1);
        let fresh = landed("fresh");
        let mut temporary = landed("creating");
        temporary.key = "\0creating".into();
        let board = Board {
            host_errors: [("builder".to_owned(), "down".to_owned())].into(),
            ..Default::default()
        };
        let preview = CleanupPreview::new(["target"]);
        let mark = |row: &BoardRow| preview.mark(&board, row);
        assert_eq!(mark(&target), Some(CleanupMark::Remove));
        assert_eq!(mark(&dirty), Some(CleanupMark::Held));
        assert_eq!(mark(&unlanded), Some(CleanupMark::Unlanded));
        assert_eq!(mark(&loading), Some(CleanupMark::Unknown));
        assert_eq!(mark(&down), Some(CleanupMark::Unknown));
        assert_eq!(mark(&fresh), Some(CleanupMark::Unknown));
        assert_eq!(mark(&temporary), None);
    }

    #[test]
    fn work_in_progress_is_never_offered_for_cleanup() {
        for state in [wt_core::WorkState::Working, wt_core::WorkState::NeedsHuman] {
            let mut row = landed("busy");
            row.work = Some(WorkPresentation {
                effective_state: Some(state),
                ..Default::default()
            });
            assert_eq!(keep_reason(&row), Some("work is still in progress"));
        }
        let mut ready = landed("done");
        ready.work = Some(WorkPresentation {
            effective_state: Some(wt_core::WorkState::Ready),
            ..Default::default()
        });
        assert_eq!(keep_reason(&ready), None);
    }

    #[test]
    fn menu_uses_archived_and_remote_rows_from_the_board() {
        let mut archived = landed("archived");
        archived.archived = true;
        let mut remote = landed("same");
        remote.host = Some("builder".into());
        remote.key = wt_core::remote_worktree_ledger_key("builder", "same");
        let board = Board {
            rows: vec![archived, remote],
            ..Default::default()
        };
        let UiModal::Confirm {
            action: ConfirmAction::CleanupTargets { targets },
            lines,
            ..
        } = menu(&board)
        else {
            panic!("prepared candidates must open confirmation");
        };
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].key, "archived");
        assert_eq!(
            targets[1].key,
            wt_core::remote_worktree_ledger_key("builder", "same")
        );
        assert!(lines.contains(&"builder: Remove same".into()));
    }

    #[test]
    fn no_candidates_still_opens_a_persistent_menu_with_keep_reasons() {
        let mut debt = landed("debt");
        debt.work = Some(WorkPresentation {
            verification_owed: true,
            ..Default::default()
        });
        let mut dirty = landed("dirty");
        dirty.git.untracked_files = Some(1);
        let mut loading = landed("loading");
        loading.git.tracked_changes = None;
        let mut unlanded = landed("unlanded");
        unlanded.git.landed_on = None;
        let board = Board {
            rows: vec![debt, dirty, loading, unlanded],
            ..Default::default()
        };
        let UiModal::Log {
            lines,
            close_key,
            refresh,
            ..
        } = menu(&board)
        else {
            panic!("keep reasons must remain visible");
        };
        assert!(lines.contains(&"Keep debt: post-merge verification still owed".into()));
        assert!(lines.contains(&"Keep dirty: uncommitted changes".into()));
        assert!(lines.contains(&"Keep loading: checkout changes are still loading".into()));
        assert!(lines.contains(&"Keep unlanded: landing of its own work is not proven".into()));
        assert_eq!(close_key, Some('c'));
        assert!(refresh.is_none());
    }

    #[test]
    fn cached_rows_from_a_failed_host_are_kept_without_waiting_for_it() {
        let mut row = landed("one");
        row.host = Some("builder".into());
        row.key = wt_core::remote_worktree_ledger_key("builder", "one");
        let board = Board {
            rows: vec![row],
            host_errors: [("builder".into(), "Permission denied".into())].into(),
            ..Default::default()
        };
        let UiModal::Log { lines, .. } = menu(&board) else {
            panic!("failed hosts cannot nominate removal");
        };
        assert!(lines.contains(&"builder: unavailable: Permission denied".into()));
        assert!(lines.contains(&"builder: Keep one: host is unavailable".into()));
    }

    #[test]
    fn managed_host_labels_never_replace_cleanup_target_identity() {
        let host =
            "michael@10.10.1.90 [config: ~/.local/share/wt/managed/code-cozee-dev/config.toml]";
        let mut row = landed("done");
        row.host = Some(host.into());
        row.key = wt_core::remote_worktree_ledger_key(host, "done");
        let key = row.key.clone();
        let board = Board {
            hosts: vec![crate::HostChoice {
                id: Some(host.into()),
                label: "MaxBook".into(),
                sign_in_needed: Vec::new(),
            }],
            rows: vec![row],
            ..Default::default()
        };
        let UiModal::Confirm {
            lines,
            action: ConfirmAction::CleanupTargets { targets },
            ..
        } = menu(&board)
        else {
            panic!("candidate expected")
        };
        assert!(lines.contains(&"MaxBook: Remove done".into()));
        assert!(lines.iter().all(|line| !line.contains("config.toml")));
        assert_eq!(targets[0].key, key);
    }
}
