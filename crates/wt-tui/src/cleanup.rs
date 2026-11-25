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
const WORK_IN_PROGRESS: &str = "work is still in progress";
/// What nothing to remove reads as.
const NOTHING_READY: &str = "Nothing is ready to clean.";

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
    /// A row the menu leaves out (see [`surprising`]) wears no mark.
    pub(crate) fn mark(&self, board: &Board, row: &BoardRow) -> Option<CleanupMark> {
        if row.key.starts_with('\0') {
            return None;
        }
        if self.targets.contains(row.key.as_str()) {
            return Some(CleanupMark::Remove);
        }
        match hold_reason(board, row) {
            None => Some(CleanupMark::Unknown),
            Some(reason) if surprising(row, &reason) => Some(CleanupMark::from_reason(&reason)),
            Some(_) => None,
        }
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
            if surprising(row, &reason) {
                kept.push(format!("{label}Keep {name}: {reason}"));
            }
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
        if lines.len() == 1 {
            // Nothing kept and no host to report: only the verdict.
            lines.clear();
        }
        lines.insert(0, NOTHING_READY.into());
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

fn hold_reason(board: &Board, row: &BoardRow) -> Option<String> {
    let host_down = row
        .host
        .as_ref()
        .is_some_and(|host| board.host_errors.contains_key(host));
    if host_down {
        Some(HOST_UNAVAILABLE.into())
    } else {
        keep_reason(row)
    }
}

fn keep_reason(row: &BoardRow) -> Option<String> {
    checkout_hold_reason(row, true)
}

/// Whether a kept row belongs in the review: one a person could be
/// surprised to see kept, because its work landed, was dropped, or its PR
/// merged or closed, and something still holds the checkout. Work that is
/// simply unfinished or not started is not cleanup's business and is left
/// out of the list and unmarked behind it.
fn surprising(row: &BoardRow, reason: &str) -> bool {
    let dropped = row.work.as_ref().is_some_and(|work| {
        work.record
            .as_ref()
            .map(|record| record.state)
            .or(work.effective_state)
            == Some(wt_core::WorkState::Dropped)
    });
    let pr_over = row
        .pr
        .as_ref()
        .and_then(|pr| pr.state.as_deref())
        .is_some_and(|state| matches!(state, "MERGED" | "CLOSED"));
    reason != WORK_IN_PROGRESS && (row.git.landed_on.is_some() || dropped || pr_over)
}

/// The one reason a row that still owes something is kept:
/// `still owes: <headline>`, counting what else it owes. A merge gate alone
/// holds nothing.
fn owed_reason(row: &BoardRow) -> Option<String> {
    let mut holding = row.owes.iter().filter(|item| item.holds_cleanup());
    let first = holding.next()?;
    let more = holding.count();
    Some(if more == 0 {
        format!("{}: {}", wt_core::OWED_HAZARD, first.headline)
    } else {
        format!(
            "{}: {} (+{more} more)",
            wt_core::OWED_HAZARD,
            first.headline
        )
    })
}

/// Why a row's checkout cannot be removed from the board alone. Shared by the
/// `c` cleanup menu and the `d` remove menu. Cleanup also needs proof that the
/// row's own work landed; the remove menu does not, because the host plans
/// unpushed work itself and returns its hazards for review.
pub(crate) fn checkout_hold_reason(row: &BoardRow, landing_required: bool) -> Option<String> {
    let reason = if row.path_missing || row.path.is_empty() {
        CHECKOUT_UNAVAILABLE
    } else if row.busy.is_some() {
        OPERATION_IN_PROGRESS
    } else if row.git.error.is_some() || row.git.head_sha.as_deref().is_none_or(str::is_empty) {
        GIT_UNAVAILABLE
    } else if landing_required && row.git.landed_on.is_none() {
        NOT_LANDED
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
        WORK_IN_PROGRESS
    } else if let Some(owed) = owed_reason(row) {
        // The host refuses this removal; say so before it is offered.
        return Some(owed);
    } else if row.git.rebasing || !row.git.conflict_files.is_empty() {
        "rebase or merge is in progress"
    } else if row.git.tracked_changes.is_none() || row.git.untracked_files.is_none() {
        CHANGES_LOADING
    } else if row.git.tracked_changes != Some(0) || row.git.untracked_files != Some(0) {
        "uncommitted changes"
    } else {
        return None;
    };
    Some(reason.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{GitPresentation, LandingKind, OwedView, WatchActionKind, WorkPresentation};
    use wt_core::{OwedKind, OwedState, WorkState};

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

    fn unlanded(slug: &str) -> BoardRow {
        let mut row = landed(slug);
        row.git.landed_on = None;
        row
    }

    fn status(state: WorkState) -> Option<WorkPresentation> {
        Some(WorkPresentation {
            record: Some(wt_core::WorkStatusRecord::new(
                state,
                "2026-10-09T00:00:00Z",
            )),
            effective_state: Some(state),
            ..Default::default()
        })
    }

    fn pr(state: &str) -> Option<crate::PrPresentation> {
        Some(crate::PrPresentation {
            number: Some(7),
            state: Some(state.into()),
            ..Default::default()
        })
    }

    #[test]
    fn marks_follow_the_displayed_targets_and_each_rows_reason() {
        let mut dirty = landed("dirty");
        dirty.git.untracked_files = Some(1);
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
        // Dropped work that never landed is kept, and worth saying so.
        let mut dropped = unlanded("dropped");
        dropped.work = status(WorkState::Dropped);
        let board = Board {
            host_errors: [("builder".to_owned(), "down".to_owned())].into(),
            ..Default::default()
        };
        let preview = CleanupPreview::new(["target"]);
        let mark = |row: &BoardRow| preview.mark(&board, row);
        assert_eq!(mark(&target), Some(CleanupMark::Remove));
        assert_eq!(mark(&dirty), Some(CleanupMark::Held));
        assert_eq!(mark(&dropped), Some(CleanupMark::Unlanded));
        assert_eq!(mark(&loading), Some(CleanupMark::Unknown));
        assert_eq!(mark(&down), Some(CleanupMark::Unknown));
        assert_eq!(mark(&fresh), Some(CleanupMark::Unknown));
        assert_eq!(mark(&temporary), None);
    }

    #[test]
    fn unfinished_and_unstarted_work_is_left_out_and_unmarked() {
        // Nothing asserted, in progress, open PR, or landed in Git with its
        // agent still at work: none of it is cleanup's business.
        let unstarted = unlanded("unstarted");
        let mut working = unlanded("working");
        working.work = status(WorkState::Working);
        let mut open = unlanded("open");
        open.work = status(WorkState::Ready);
        open.pr = pr("OPEN");
        let mut busy = landed("busy");
        busy.work = status(WorkState::Working);
        let mut broken = unlanded("broken");
        broken.git.error = Some("boom".into());
        // These a person could expect gone: say why they stay.
        let mut dropped = unlanded("dropped");
        dropped.work = status(WorkState::Dropped);
        let mut closed = unlanded("closed");
        closed.pr = pr("CLOSED");
        let mut merged = unlanded("merged");
        merged.pr = pr("MERGED");
        let mut owing = landed("owing");
        owing.owes = vec![OwedView::new(
            OwedKind::Check,
            OwedState::Waiting,
            "open /health",
        )];
        let rows = vec![
            unstarted, working, open, busy, broken, dropped, closed, merged, owing,
        ];
        let board = Board {
            rows,
            ..Default::default()
        };
        let UiModal::Log { lines, .. } = menu(&board) else {
            panic!("nothing here can be removed");
        };
        assert_eq!(
            lines,
            [
                "Nothing is ready to clean.",
                "From background results. Removal checks current state again.",
                "",
                "Keeping · 4",
                "Keep dropped: landing of its own work is not proven",
                "Keep closed: landing of its own work is not proven",
                "Keep merged: landing of its own work is not proven",
                "Keep owing: still owes: check: open /health",
            ]
        );
        let preview = CleanupPreview::new([]);
        let marks: Vec<_> = board
            .rows
            .iter()
            .map(|row| preview.mark(&board, row))
            .collect();
        assert_eq!(
            marks,
            [
                None,
                None,
                None,
                None,
                None,
                Some(CleanupMark::Unlanded),
                Some(CleanupMark::Unlanded),
                Some(CleanupMark::Unlanded),
                Some(CleanupMark::Held),
            ]
        );
    }

    #[test]
    fn a_board_of_unfinished_work_says_only_that_nothing_is_ready() {
        let mut working = unlanded("working");
        working.work = status(WorkState::Working);
        let board = Board {
            rows: vec![unlanded("unstarted"), working],
            ..Default::default()
        };
        let UiModal::Log {
            lines,
            close_key,
            refresh,
            ..
        } = menu(&board)
        else {
            panic!("nothing here can be removed");
        };
        assert_eq!(lines, ["Nothing is ready to clean."]);
        assert_eq!(close_key, Some('c'));
        assert!(refresh.is_none());
        // With something to remove, the unfinished rows still stay out.
        let board = Board {
            rows: vec![unlanded("unstarted"), landed("done")],
            ..Default::default()
        };
        let UiModal::Confirm { lines, .. } = menu(&board) else {
            panic!("one row is ready");
        };
        assert_eq!(
            lines,
            [
                "From background results. Removal checks current state again.",
                "",
                "Ready to remove · 1",
                "Remove done",
            ]
        );
    }

    #[test]
    fn one_reason_says_what_a_landed_row_still_owes() {
        let step = |state, what: &str| OwedView::new(OwedKind::Step, state, what);
        let mut waking = landed("waking");
        waking.owes = vec![step(
            OwedState::Waiting,
            "rerun the renewal check\nthen report",
        )];
        assert_eq!(
            keep_reason(&waking).as_deref(),
            Some("still owes: rerun the renewal check")
        );
        // The remove menu says it too: the host refuses that removal.
        assert_eq!(checkout_hold_reason(&waking, false), keep_reason(&waking));
        // The most urgent item names it; the rest are counted.
        waking
            .owes
            .insert(0, step(OwedState::Due, "confirm prod logs"));
        waking.owes.push(OwedView::new(
            OwedKind::Check,
            OwedState::Waiting,
            "open /health",
        ));
        assert_eq!(
            keep_reason(&waking).as_deref(),
            Some("still owes: confirm prod logs (+2 more)")
        );
        assert_eq!(
            CleanupMark::from_reason(&keep_reason(&waking).unwrap()),
            CleanupMark::Held
        );
        // A leftover merge gate holds nothing; one whose watch also wakes
        // the session does. A notification is not owed at all.
        let mut gated = landed("gated");
        gated.owes = vec![OwedView::new(OwedKind::Gate, OwedState::Waiting, "QA")];
        assert_eq!(keep_reason(&gated), None);
        gated.owes[0].wakes = true;
        assert_eq!(keep_reason(&gated).as_deref(), Some("still owes: QA"));
        let mut telling = landed("telling");
        telling.watches = vec![crate::WatchView {
            id: "abc234".into(),
            actions: vec![WatchActionKind::Notify],
            ..Default::default()
        }];
        assert_eq!(keep_reason(&telling), None);
    }

    #[test]
    fn work_in_progress_is_never_offered_for_cleanup() {
        for state in [WorkState::Working, WorkState::NeedsHuman] {
            let mut row = landed("busy");
            row.work = Some(WorkPresentation {
                effective_state: Some(state),
                ..Default::default()
            });
            assert_eq!(keep_reason(&row).as_deref(), Some(WORK_IN_PROGRESS));
        }
        let mut ready = landed("done");
        ready.work = Some(WorkPresentation {
            effective_state: Some(WorkState::Ready),
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
        debt.owes = vec![OwedView::new(
            OwedKind::Check,
            OwedState::Due,
            "open /health",
        )];
        let mut dirty = landed("dirty");
        dirty.git.untracked_files = Some(1);
        let mut loading = landed("loading");
        loading.git.tracked_changes = None;
        let board = Board {
            rows: vec![debt, dirty, loading, unlanded("unlanded")],
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
        assert_eq!(lines[0], "Nothing is ready to clean.");
        assert!(lines.contains(&"Keeping · 3".into()));
        assert!(lines.contains(&"Keep debt: still owes: check: open /health".into()));
        assert!(lines.contains(&"Keep dirty: uncommitted changes".into()));
        assert!(lines.contains(&"Keep loading: checkout changes are still loading".into()));
        assert!(!lines.iter().any(|line| line.contains("unlanded")));
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
