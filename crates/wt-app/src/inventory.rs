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
        let manual = stored["manualTitle"]
            .as_str()
            .map(str::trim)
            .filter(|title| !title.is_empty());
        let title = manual.map_or_else(
            || crate::issue_identity::slug_title(target.slug()),
            str::to_owned,
        );
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
            is_main: snapshot.worktree.is_main,
            detached: snapshot.worktree.detached,
            slug: clean_text(target.slug()),
            title: clean_text(&title),
            title_source: if manual.is_some() {
                wt_tui::TitleSource::Manual
            } else {
                wt_tui::TitleSource::Slug
            },
            branch: clean_text(&target.branch),
            path: clean_text(&target.path),
            stage_name: Some(clean_text(&target.stage)).filter(|stage| !stage.is_empty()),
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
    apply_removals(&mut rows, state);
    let mut board = Board {
        name: clean_text(&config.repo_id),
        automations_paused: state["automationsPaused"].as_bool().unwrap_or(false),
        full_width_activity: config.ui.activity_pane == wt_config::ActivityPane::FullWidth,
        display: display_policy(config),
        rows,
        ..Board::default()
    };
    crate::board_layout::prepare(&mut board, state, &config.branch.base, config.ui.sort);
    board
}

/// Keep accepted jobs visible until the worker records success, even when
/// partial deletion removes the checkout from Git inventory.
fn apply_removals(rows: &mut Vec<BoardRow>, state: &Value) {
    let Some(removals) = state["removals"].as_object() else {
        return;
    };
    for (key, removal) in removals {
        let (Some(path), Some(branch)) = (removal["path"].as_str(), removal["branch"].as_str())
        else {
            continue;
        };
        let index = if let Some(index) = rows.iter().position(|row| row.key == *key) {
            // A reused name cannot transfer a job to another checkout.
            if rows[index].path != path || rows[index].branch != branch {
                continue;
            }
            index
        } else {
            rows.push(BoardRow {
                key: key.clone(),
                slug: clean_text(key),
                title: clean_text(&crate::issue_identity::slug_title(key)),
                path: clean_text(path),
                branch: clean_text(branch),
                archived: true,
                ..BoardRow::default()
            });
            rows.len() - 1
        };
        let row = &mut rows[index];
        let error = removal["error"].as_str().map(clean_text);
        row.removal = Some(wt_tui::RemovalView {
            error: error.clone(),
        });
        if let Some(error) = error {
            row.badge = "Removal failed".into();
            row.needs_attention = true;
            row.details.push(format!("Removal failed: {error}"));
            row.details
                .push("Press d to review a retry. Current safety checks still apply.".into());
        } else {
            row.badge = "Removing".into();
            row.busy = Some(wt_tui::BusyView {
                op: "remove".into(),
                label: "removing".into(),
                ..Default::default()
            });
        }
    }
}

/// Configuration the renderer needs, resolved once per board.
pub fn display_policy(config: &Config) -> wt_tui::DisplayPolicy {
    wt_tui::DisplayPolicy {
        hidden_badges: config.ui.hidden_badges.iter().cloned().collect(),
        review_bot_carrot: config.review_bot.login == "coderabbitai",
        review_bot_checklist: config.review_bot.unresolved_via
            == wt_config::ReviewBotMode::Checklist,
        reviewers: config.github.reviewers,
        production: config.branch.production.is_some(),
        automations: !config.automations.is_empty(),
        primary_harness: config.harness.primary.as_str().to_owned(),
        primary: Some(config.harness.primary),
        harnesses: wt_core::HarnessId::ALL
            .into_iter()
            .filter(|harness| !config.harness.hidden.contains(harness))
            .collect(),
        trunk: config.branch.base.clone(),
        default_reviewer: config
            .github
            .reviewers
            .then(|| config.github.default_reviewer.clone())
            .flatten(),
    }
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
    fn failed_removal_remains_without_inventory_and_success_clears_it() {
        let mut state = serde_json::json!({"removals": {"one": {
            "jobId": "1-a", "path": "/one", "branch": "feature/one", "error": null
        }}});
        let mut rows = Vec::new();
        apply_removals(&mut rows, &state);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].busy.as_ref().unwrap().op, "remove");

        state["removals"]["one"]["error"] = serde_json::json!("permission denied");
        let mut restarted = Vec::new();
        apply_removals(&mut restarted, &state);
        assert_eq!(
            restarted[0].removal.as_ref().unwrap().error.as_deref(),
            Some("permission denied")
        );
        assert!(restarted[0].busy.is_none());
        assert!(restarted[0].needs_attention);

        state["removals"] = serde_json::json!({});
        let mut completed = Vec::new();
        apply_removals(&mut completed, &state);
        assert!(completed.is_empty());
    }

    #[test]
    fn removal_does_not_transfer_to_a_reused_key_at_another_path() {
        let mut rows = vec![BoardRow {
            key: "one".into(),
            path: "/new".into(),
            branch: "feature/one".into(),
            ..Default::default()
        }];
        let state = serde_json::json!({"removals": {"one": {"path": "/old", "branch": "feature/one", "error": null}}});
        apply_removals(&mut rows, &state);
        assert!(rows[0].removal.is_none());
    }
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
