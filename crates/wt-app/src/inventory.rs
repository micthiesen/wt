use std::collections::BTreeSet;

use serde_json::Value;
use wt_config::Config;
use wt_core::parse_work_status;
use wt_tui::{Board, BoardRow};
use wt_vcs::WorktreeSnapshot;

pub fn board(
    config: &Config,
    inventory: &[WorktreeSnapshot],
    state: &Value,
    archived: &BTreeSet<String>,
) -> Board {
    let mut rows = Vec::with_capacity(inventory.len());
    for snapshot in inventory
        .iter()
        .filter(|snapshot| !snapshot.worktree.is_main)
    {
        let target = &snapshot.worktree.target;
        let stored = &state["slugs"][target.slug()];
        let work = parse_work_status(&stored["work"]);
        let title = stored["manualTitle"]
            .as_str()
            .map(str::trim)
            .filter(|title| !title.is_empty())
            .unwrap_or(target.slug());
        let mut details = Vec::new();
        let mut badge = String::new();
        if stored["automationsPaused"].as_bool() == Some(true) {
            badge.push_str("  auto paused");
            details.push("Automations paused for this worktree".into());
        }
        rows.push(BoardRow {
            key: wt_core::worktree_target_key(target),
            base_branch: state["slugs"][target.slug()]["baseBranch"]
                .as_str()
                .map(str::to_owned),
            slug: clean_text(target.slug()),
            title: clean_text(title),
            branch: clean_text(&target.branch),
            path: clean_text(&target.path),
            badge,
            work_rank: wt_core::work_record_rank(work.as_ref()),
            work: work.as_ref().map(|record| {
                let mut record = record.clone();
                record.note = record.note.as_deref().map(clean_text);
                record.blocked_on = record.blocked_on.as_deref().map(clean_text);
                record.verify_after_merge = record.verify_after_merge.as_deref().map(clean_text);
                wt_tui::WorkPresentation {
                    record: Some(record.clone()),
                    effective_state: Some(record.state),
                    blocked: wt_core::is_gated(Some(&record)),
                    stale: record
                        .sha
                        .as_deref()
                        .zip(
                            snapshot
                                .status
                                .as_ref()
                                .and_then(|status| status.head_sha.as_deref())
                                .or(snapshot.worktree.head_sha.as_deref()),
                        )
                        .map(|(recorded, current)| recorded != current),
                    ..wt_tui::WorkPresentation::default()
                }
            }),
            git: wt_tui::GitPresentation {
                head_sha: snapshot
                    .status
                    .as_ref()
                    .and_then(|status| status.head_sha.clone())
                    .or_else(|| snapshot.worktree.head_sha.clone()),
                error: snapshot.error.as_deref().map(clean_text),
                tracked_changes: snapshot
                    .status
                    .as_ref()
                    .map(|status| status.tracked_changes),
                untracked_files: snapshot
                    .status
                    .as_ref()
                    .map(|status| status.untracked_files),
                upstream: snapshot
                    .status
                    .as_ref()
                    .and_then(|status| status.upstream.as_deref())
                    .map(clean_text),
                ahead: snapshot.status.as_ref().and_then(|status| status.ahead),
                behind: snapshot.status.as_ref().and_then(|status| status.behind),
                ..wt_tui::GitPresentation::default()
            },
            automations_paused: stored["automationsPaused"].as_bool() == Some(true),
            verify_steps: work
                .as_ref()
                .and_then(|work| work.verify_after_merge.as_deref())
                .map(clean_text),
            needs_attention: work.is_some_and(|work| {
                matches!(
                    work.state,
                    wt_core::WorkState::NeedsHuman
                        | wt_core::WorkState::NeedsTesting
                        | wt_core::WorkState::Ready
                )
            }),
            details,
            archived: archived.contains(target.slug()),
            issue_id: issue_id(&stored["issueId"], target.slug()),
            issue_url: issue_id(&stored["issueId"], target.slug())
                .filter(|id| !id.starts_with("GH-"))
                .and_then(|id| {
                    config
                        .issue_tracker
                        .as_ref()?
                        .url_template
                        .as_ref()
                        .map(|template| template.replace("{id}", &id))
                }),
            ..BoardRow::default()
        });
    }
    let mut board = Board {
        name: clean_text(&config.repo_id),
        automations_paused: state["automationsPaused"].as_bool().unwrap_or(false),
        full_width_activity: config.ui.activity_pane == wt_config::ActivityPane::FullWidth,
        rows,
        ..Board::default()
    };
    crate::board_layout::prepare(&mut board, state, &config.branch.base, config.ui.sort);
    board
}

/// Strip terminal controls while preserving text and line structure. Log and
/// external-service fields must never be interpreted as terminal instructions.
fn clean_text(text: &str) -> String {
    wt_core::sanitize_terminal_text(text)
}

fn issue_id(stored: &serde_json::Value, slug: &str) -> Option<String> {
    crate::issue_identity::resolve(slug, stored.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn issue_resolution_keeps_asserted_none_distinct_from_absence() {
        assert_eq!(
            issue_id(&serde_json::Value::Null, "eng-123-fix"),
            Some("ENG-123".into())
        );
        assert_eq!(issue_id(&serde_json::json!(""), "eng-123-fix"), None);
        assert_eq!(issue_id(&serde_json::json!("   "), "eng-123-fix"), None);
        assert_eq!(
            issue_id(&serde_json::json!("other-2"), "eng-123-fix"),
            Some("OTHER-2".into())
        );
    }
}
