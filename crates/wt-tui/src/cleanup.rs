//! Cleanup review from the prepared board. Opening this menu does no I/O.
use crate::{Board, BoardRow, CleanupTarget, ConfirmAction, UiModal};

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
        let reason = if row
            .host
            .as_ref()
            .is_some_and(|host| board.host_errors.contains_key(host))
        {
            Some("host is unavailable")
        } else {
            keep_reason(row)
        };
        if let Some(reason) = reason {
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
            title: "Clean worktrees".into(),
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

fn keep_reason(row: &BoardRow) -> Option<&'static str> {
    checkout_hold_reason(row, true)
}

/// Why a row's checkout cannot be removed from the board alone. Shared by the
/// `c` cleanup menu and the `d` remove menu. Cleanup also needs proof that the
/// row's own work landed; the remove menu does not, because the host plans
/// unpushed work itself and returns its hazards for review.
pub(crate) fn checkout_hold_reason(row: &BoardRow, landing_required: bool) -> Option<&'static str> {
    if row.path_missing || row.path.is_empty() {
        Some("checkout is unavailable")
    } else if row.busy.is_some() {
        Some("operation in progress")
    } else if row.git.error.is_some() || row.git.head_sha.as_deref().is_none_or(str::is_empty) {
        Some("Git state is unavailable")
    } else if landing_required && row.git.landed_on.is_none() {
        Some("landing of its own work is not proven")
    } else if row.work.as_ref().is_some_and(|work| work.verification_owed) {
        Some("post-merge verification still owed")
    } else if row.git.rebasing || !row.git.conflict_files.is_empty() {
        Some("rebase or merge is in progress")
    } else if row.git.tracked_changes.is_none() || row.git.untracked_files.is_none() {
        Some("checkout changes are still loading")
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
