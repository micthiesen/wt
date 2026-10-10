//! Joins typed host facts into the user-facing work-status and detail views.
//! These values are presentation-only and are never used to authorize work.

use std::{
    path::Path,
    sync::Arc,
    time::{Duration, SystemTime},
};

use wt_config::Config;
use wt_core::DerivedState;
use wt_github::{GithubData, PrChecks, PrReview, PullRequest};
use wt_runtime::{SourceHandle, SourceSnapshot, SourceState, TaskScope, source_channel};
use wt_tui::{Board, BoardRow, PreparedDetailGroup, WorkPresentation};

pub fn overlay(
    scope: &TaskScope,
    config: Arc<Config>,
    board: SourceHandle<Board>,
    github: SourceHandle<GithubData>,
) -> SourceHandle<Board> {
    let (source, mut publisher) = source_channel();
    let cancellation = scope.token();
    scope.spawn(async move {
        let mut board_updates = board.subscribe();
        let mut github_updates = github.subscribe();
        board_updates.mark_changed();
        github_updates.mark_changed();
        let mut age_refresh = tokio::time::interval(Duration::from_secs(60));
        let mut last = None;
        loop {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => break,
                request = publisher.requested() => {
                    if request.is_none() { break; }
                    board.refresh();
                    github.refresh();
                    continue;
                },
                changed = board_updates.changed() => if changed.is_err() { break; },
                changed = github_updates.changed() => if changed.is_err() { break; },
                _ = age_refresh.tick() => {},
            }
            let github_snapshot = github_updates.borrow_and_update().clone();
            let prepared = compose(
                board_updates.borrow_and_update().clone(),
                github_snapshot,
                &config,
                now_ms(),
            );
            let visible = (prepared.data.as_deref().cloned(), prepared.state.clone());
            if last.as_ref() != Some(&visible) {
                last = Some(visible);
                publisher.publish(prepared);
            }
        }
    });
    source
}

fn compose(
    mut source: SourceSnapshot<Board>,
    github: SourceSnapshot<GithubData>,
    config: &Config,
    now_ms: i64,
) -> SourceSnapshot<Board> {
    let Some(prepared) = source.data.as_ref() else {
        return source;
    };
    let mut board = prepared.as_ref().clone();
    let data = github.data.as_deref();
    for row in &mut board.rows {
        row.pr = data
            .and_then(|data| {
                let pr = pick_pr(row, data)?;
                let mut presentation = pr_presentation(pr);
                presentation.merge_queue = data
                    .merge_queue
                    .get(&row.branch)
                    .map(|queue| format!("#{} · {:?}", queue.position, queue.state));
                presentation.auto_merge_armed = pr.auto_merge.is_some();
                presentation.comments = pr
                    .comments
                    .iter()
                    .map(|comment| format!("{}: {}", clean(&comment.author), clean(&comment.body)))
                    .collect();
                Some(presentation)
            })
            .or_else(|| match &github.state {
                SourceState::Failed(error) => Some(wt_tui::PrPresentation {
                    error: Some(clean(error)),
                    ..Default::default()
                }),
                _ => None,
            });
        prepare_work(row, now_ms);
        row.detail_groups = detail_groups(row, config);
    }
    crate::board_layout::refresh_rollups(&mut board);
    source.data = Some(Arc::new(board));
    source
}

fn prepare_work(row: &mut BoardRow, now_ms: i64) {
    let record = row.work.as_ref().and_then(|work| work.record.as_ref());
    let asking = row
        .sessions
        .iter()
        .any(|session| session.live && session.state == "asking");
    // Only the Git facts lane may establish landing after checking the
    // non-vacuous own-commit and advertised-base proof.
    let landed = row.git.landed_on.is_some();
    let session_state = asking.then_some(DerivedState::Asking);
    let effective = wt_core::effective_work_state(record, session_state, landed);
    let Some(mut work) = row
        .work
        .clone()
        .or_else(|| asking.then_some(WorkPresentation::default()))
    else {
        return;
    };
    work.effective_state = effective.map(|state| state.state);
    work.derived = effective.is_some_and(|state| state.derived);
    work.blocked = effective.is_some_and(|state| state.blocked);
    work.age = work
        .record
        .as_ref()
        .and_then(|record| wt_core::work_age(&record.at, now_ms));
    work.stale = work
        .record
        .as_ref()
        .and_then(|record| record.sha.as_deref())
        .zip(row.git.head_sha.as_deref())
        .map(|(recorded, head)| recorded != head);
    work.verification_owed = wt_core::owes_post_merge_verification(record, landed);
    work.verification_overdue = wt_core::verification_overdue(record, landed, now_ms);
    row.needs_attention |= matches!(
        work.effective_state,
        Some(
            wt_core::WorkState::NeedsHuman
                | wt_core::WorkState::NeedsTesting
                | wt_core::WorkState::Ready
        )
    );
    row.work = Some(work);
}

fn pick_pr<'a>(row: &BoardRow, data: &'a GithubData) -> Option<&'a PullRequest> {
    if wt_core::is_remote_worktree_ledger_key(&row.key) {
        return data.prs.get(&row.branch);
    }
    wt_github::pick_pr_for_worktree(Some(&row.branch), Path::new(&row.path), &data.prs)
}

fn pr_presentation(pr: &PullRequest) -> wt_tui::PrPresentation {
    let checks = match pr.checks {
        PrChecks::Pass => "passing",
        PrChecks::Fail => "failing",
        PrChecks::Pending => "pending",
        PrChecks::None => "none",
    };
    let review = match pr.review {
        PrReview::Approved => "approved",
        PrReview::ChangesRequested => "changes requested",
        PrReview::Pending => "pending",
        PrReview::Unrequested => "not requested",
        PrReview::None => "none",
    };
    wt_tui::PrPresentation {
        head_sha: pr.head_ref_oid.clone(),
        number: Some(pr.number),
        url: Some(clean(&pr.url)),
        title: Some(clean(&pr.title)),
        state: Some(clean(&pr.state)),
        draft: pr.is_draft,
        base_branch: Some(clean(&pr.base_ref_name)),
        checks: Some(checks.into()),
        failed_checks: pr.failed_checks.iter().map(|line| clean(line)).collect(),
        review: Some(review.into()),
        reviewers: pr
            .requested_reviewers
            .iter()
            .map(|login| clean(login))
            .collect(),
        review_bot: pr
            .review_bot
            .as_ref()
            .map(|bot| format!("{} · {} unresolved", clean(&bot.state), bot.unresolved)),
        unresolved_threads: pr.unresolved_threads_total,
        merge_queue: None,
        auto_merge_armed: pr.auto_merge.is_some(),
        comments: pr
            .comments
            .iter()
            .map(|comment| format!("{}: {}", clean(&comment.author), clean(&comment.body)))
            .collect(),
        error: None,
    }
}

fn detail_groups(row: &BoardRow, config: &Config) -> Vec<PreparedDetailGroup> {
    config
        .ui
        .rows
        .iter()
        .filter_map(|id| detail_group(row, id, config))
        .collect()
}

fn detail_group(row: &BoardRow, id: &str, config: &Config) -> Option<PreparedDetailGroup> {
    let (id, label, lines, error) = match id {
        "branch" => (
            "branch",
            "Branch",
            nonempty_lines([
                Some(clean(&row.branch)),
                row.base_branch
                    .as_deref()
                    .filter(|base| !base.is_empty())
                    .map(|base| format!("base: {}", clean(base))),
            ]),
            None,
        ),
        "path" => (
            "path",
            "Path",
            (!row.path.is_empty())
                .then(|| clean(&row.path))
                .into_iter()
                .collect(),
            None,
        ),
        "issue" | "linear" => (
            "issue",
            "Issue",
            nonempty_lines([
                row.issue_id.as_deref().map(clean),
                row.issue_status.as_deref().map(clean),
                row.issue_url
                    .as_deref()
                    .or(row.github_issue_url.as_deref())
                    .map(clean),
            ]),
            None,
        ),
        "stage" => (
            "stage",
            "Stage",
            row.stage_url.as_deref().map(clean).into_iter().collect(),
            None,
        ),
        "dev" => (
            "dev",
            "Dev",
            row.dev_url.as_deref().map(clean).into_iter().collect(),
            None,
        ),
        "pr" => {
            let pr = row.pr.as_ref()?;
            let mut lines = nonempty_lines([
                pr.number.map(|number| format!("#{number}")),
                pr.title.as_deref().map(clean),
                pr.state.as_deref().map(|state| {
                    if pr.draft {
                        "draft".into()
                    } else {
                        clean(state)
                    }
                }),
                pr.url.as_deref().map(clean),
                pr.base_branch
                    .as_deref()
                    .map(|base| format!("base: {}", clean(base))),
                pr.checks
                    .as_deref()
                    .map(|checks| format!("checks: {checks}")),
                pr.review
                    .as_deref()
                    .map(|review| format!("review: {review}")),
                pr.review_bot
                    .as_deref()
                    .map(|bot| format!("review bot: {}", clean(bot))),
            ]);
            lines.extend(pr.failed_checks.iter().map(|check| clean(check)));
            if !pr.reviewers.is_empty() {
                lines.push(format!("reviewers: {}", pr.reviewers.join(", ")));
            }
            if pr.unresolved_threads > 0 {
                lines.push(format!("unresolved threads: {}", pr.unresolved_threads));
            }
            if let Some(queue) = &pr.merge_queue {
                lines.push(format!("merge queue: {}", clean(queue)));
            } else if pr.auto_merge_armed {
                lines.push("merge when ready: armed".into());
            }
            lines.extend(pr.comments.iter().map(|comment| clean(comment)));
            ("pr", "Pull request", lines, pr.error.as_deref().map(clean))
        }
        "claude" => {
            let mut lines: Vec<String> = row
                .sessions
                .iter()
                .map(|session| {
                    format!(
                        "{} {} · {}{}",
                        clean(&session.harness),
                        clean(&session.name),
                        clean(&session.state),
                        if session.queued > 0 {
                            format!(" · {} queued", session.queued)
                        } else {
                            String::new()
                        }
                    )
                })
                .collect();
            if lines.is_empty() {
                lines.push(format!(
                    "primary: {} · F12 to start",
                    harness_label(config.harness.primary)
                ));
            }
            ("claude", "AI session", lines, None)
        }
        "git" => {
            let mut lines = Vec::new();
            match (row.git.tracked_changes, row.git.untracked_files) {
                (Some(tracked), Some(untracked)) => {
                    lines.push(format!("{tracked} tracked, {untracked} untracked"));
                }
                _ => lines.push("Git status unknown".into()),
            }
            if let Some(upstream) = row.git.upstream.as_deref() {
                let counts = match (row.git.ahead, row.git.behind) {
                    (Some(ahead), Some(behind)) => format!(" · {ahead} ahead, {behind} behind"),
                    _ => String::new(),
                };
                lines.push(format!("{}{}", clean(upstream), counts));
            }
            if row.git.rebasing {
                lines.push("Rebase in progress".into());
            }
            lines.extend(
                row.git
                    .conflict_files
                    .iter()
                    .map(|path| format!("conflict: {}", clean(path))),
            );
            if let Some(landing) = row.git.landed_on {
                lines.push(format!(
                    "Landed on {}",
                    match landing {
                        wt_tui::LandingKind::Base => "base",
                        wt_tui::LandingKind::Production => "production",
                    }
                ));
            }
            ("git", "Git", lines, row.git.error.as_deref().map(clean))
        }
        _ => return None,
    };
    if lines.is_empty() && error.is_none() {
        return None;
    }
    Some(PreparedDetailGroup {
        id: id.into(),
        label: label.into(),
        lines,
        error,
    })
}

fn harness_label(harness: wt_core::HarnessId) -> &'static str {
    match harness {
        wt_core::HarnessId::Claude => "Claude",
        wt_core::HarnessId::Codex => "Codex",
        wt_core::HarnessId::Opencode => "OpenCode",
    }
}

fn nonempty_lines<const N: usize>(values: [Option<String>; N]) -> Vec<String> {
    values
        .into_iter()
        .flatten()
        .filter(|value| !value.trim().is_empty())
        .collect()
}

fn clean(text: &str) -> String {
    wt_core::sanitize_terminal_text(text)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use wt_core::{WorkRisk, WorkState, WorkStatusRecord};
    use wt_tui::BoardSection;
    use wt_tui::GitPresentation;

    fn config(rows: &[&str]) -> Config {
        let root = tempfile::tempdir().unwrap();
        let config_path = root.path().join("user.toml");
        std::fs::write(
            &config_path,
            format!(
                "[paths]\nmain_clone = {:?}\nworktree_root = {:?}\nstate_db = {:?}\ncache_db = {:?}\n[branch]\nprefix = \"fixture\"\n",
                root.path().join("main").to_string_lossy(),
                root.path().join("worktrees").to_string_lossy(),
                root.path().join("state.sqlite").to_string_lossy(),
                root.path().join("cache.sqlite").to_string_lossy(),
            ),
        )
        .unwrap();
        let options = wt_config::LoadOptions::new(
            root.path(),
            root.path(),
            [(
                "WT_CONFIG".into(),
                config_path.to_string_lossy().into_owned(),
            )]
            .into_iter()
            .collect(),
        );
        let mut config = Config::load(&options).unwrap();
        config.ui.rows = rows.iter().map(|row| (*row).to_owned()).collect();
        config
    }

    #[test]
    fn configured_detail_groups_keep_order_and_do_not_parse_text_prefixes() {
        let row = BoardRow {
            branch: "feature-1".into(),
            base_branch: Some("main".into()),
            path: "/work/one".into(),
            details: vec!["Dev: this string is not a typed dev fact".into()],
            dev_url: Some("http://localhost:8123".into()),
            ..Default::default()
        };
        let groups = detail_groups(&row, &config(&["dev", "branch", "path", "unknown"]));
        assert_eq!(
            groups
                .iter()
                .map(|group| group.id.as_str())
                .collect::<Vec<_>>(),
            ["dev", "branch", "path"]
        );
        assert_eq!(groups[0].lines, ["http://localhost:8123"]);
        assert!(
            !groups[0]
                .lines
                .iter()
                .any(|line| line.contains("not a typed"))
        );
    }

    #[test]
    fn ai_detail_uses_selected_primary_harness_when_no_session_exists() {
        let row = BoardRow::default();
        let mut config = config(&["claude"]);
        config.harness.primary = wt_core::HarnessId::Codex;
        let groups = detail_groups(&row, &config);
        assert_eq!(groups[0].label, "AI session");
        assert_eq!(groups[0].lines, ["primary: Codex · F12 to start"]);
    }

    #[test]
    fn work_view_keeps_status_absence_and_uses_exact_head_for_merged_pr_derivation() {
        let mut row = BoardRow {
            git: GitPresentation {
                head_sha: Some("head-2".into()),
                ..Default::default()
            },
            work: Some(WorkPresentation::default()),
            ..Default::default()
        };
        prepare_work(&mut row, 100_000);
        assert_eq!(row.work.as_ref().unwrap().effective_state, None);
        assert_eq!(row.work.as_ref().unwrap().stale, None);
        row.work.as_mut().unwrap().record = Some(WorkStatusRecord {
            state: WorkState::Ready,
            risk: Some(WorkRisk::High),
            at: "1970-01-01T00:00:00Z".into(),
            sha: Some("head-1".into()),
            verify_after_merge: Some("check production".into()),
            note: Some("Keep the title".into()),
            blocked_on: Some("approval".into()),
            by: None,
            extra: Default::default(),
        });
        row.git.landed_on = Some(wt_tui::LandingKind::Base);
        prepare_work(&mut row, 200_000_000);
        let work = row.work.as_ref().unwrap();
        assert_eq!(work.effective_state, Some(WorkState::NeedsTesting));
        assert!(work.derived);
        assert_eq!(work.stale, Some(true));
        assert!(work.verification_owed);
        assert!(work.verification_overdue);
        assert_eq!(work.record.as_ref().unwrap().risk, Some(WorkRisk::High));
        assert_eq!(
            work.record.as_ref().unwrap().note.as_deref(),
            Some("Keep the title")
        );
        assert_eq!(
            work.record.as_ref().unwrap().blocked_on.as_deref(),
            Some("approval")
        );
    }

    #[test]
    fn ready_gate_remains_visible_until_landing_is_proved() {
        let mut row = BoardRow {
            git: GitPresentation {
                head_sha: Some("head".into()),
                ..Default::default()
            },
            work: Some(WorkPresentation {
                record: Some(WorkStatusRecord {
                    risk: Some(WorkRisk::High),
                    blocked_on: Some("approval".into()),
                    sha: Some("head".into()),
                    ..WorkStatusRecord::new(WorkState::Ready, "2026-10-09T00:00:00Z")
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        prepare_work(&mut row, 1_800_000_000_000);
        let work = row.work.as_ref().unwrap();
        assert_eq!(work.effective_state, Some(WorkState::Ready));
        assert!(work.blocked);
        assert!(!work.verification_owed);
        assert_eq!(work.stale, Some(false));
    }

    #[test]
    fn exact_head_merged_pr_alone_does_not_prove_landing() {
        let record = WorkStatusRecord {
            verify_after_merge: Some("check production".into()),
            ..WorkStatusRecord::new(WorkState::Ready, "2026-10-08T00:00:00Z")
        };
        let mut row = BoardRow {
            git: GitPresentation {
                head_sha: Some("current".into()),
                ..Default::default()
            },
            work: Some(WorkPresentation {
                record: Some(record),
                ..Default::default()
            }),
            pr: Some(wt_tui::PrPresentation {
                state: Some("MERGED".into()),
                head_sha: Some("old".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
        prepare_work(&mut row, 1_800_000_000_000);
        assert_eq!(
            row.work.as_ref().unwrap().effective_state,
            Some(WorkState::Ready)
        );
        assert!(!row.work.as_ref().unwrap().verification_owed);
        row.pr.as_mut().unwrap().head_sha = Some("current".into());
        prepare_work(&mut row, 1_800_000_000_000);
        assert_eq!(
            row.work.as_ref().unwrap().effective_state,
            Some(WorkState::Ready)
        );
        assert!(!row.work.as_ref().unwrap().verification_owed);
        row.git.landed_on = Some(wt_tui::LandingKind::Base);
        prepare_work(&mut row, 1_800_000_000_000);
        assert_eq!(
            row.work.as_ref().unwrap().effective_state,
            Some(WorkState::NeedsTesting)
        );
        assert!(row.work.as_ref().unwrap().verification_owed);
    }

    #[test]
    fn asking_session_derives_needs_human_without_asserting_persisted_work() {
        let mut row = BoardRow {
            sessions: vec![wt_tui::SessionView {
                live: true,
                state: "asking".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        prepare_work(&mut row, 100_000);
        let work = row.work.as_ref().unwrap();
        assert_eq!(work.record, None);
        assert_eq!(work.effective_state, Some(WorkState::NeedsHuman));
        assert!(work.derived);
    }

    #[test]
    fn folded_rollups_include_state_risk_mechanical_counts_and_blocker_notes() {
        let mut board = Board {
            rows: vec![
                BoardRow {
                    slug: "ready-row".into(),
                    needs_attention: true,
                    work: Some(WorkPresentation {
                        effective_state: Some(WorkState::Ready),
                        stale: Some(true),
                        verification_owed: true,
                        verification_overdue: true,
                        record: Some(WorkStatusRecord {
                            state: WorkState::Ready,
                            risk: Some(WorkRisk::High),
                            blocked_on: Some("release approval".into()),
                            at: "2026-10-09T00:00:00Z".into(),
                            ..WorkStatusRecord::new(WorkState::Ready, "2026-10-09T00:00:00Z")
                        }),
                        ..Default::default()
                    }),
                    git: GitPresentation {
                        tracked_changes: Some(1),
                        untracked_files: Some(0),
                        ahead: Some(2),
                        behind: Some(1),
                        rebasing: true,
                        conflict_files: vec!["Cargo.toml".into()],
                        ..Default::default()
                    },
                    pr: Some(wt_tui::PrPresentation {
                        state: Some("OPEN".into()),
                        draft: true,
                        checks: Some("failing".into()),
                        merge_queue: Some("#4 · checking".into()),
                        ..Default::default()
                    }),
                    automations_paused: true,
                    ..Default::default()
                },
                BoardRow {
                    slug: "unknown-row".into(),
                    ..Default::default()
                },
            ],
            sections: vec![BoardSection {
                rows: vec![0, 1],
                ..Default::default()
            }],
            ..Default::default()
        };
        crate::board_layout::refresh_rollups(&mut board);
        let summary = &board.sections[0].rollup;
        assert_eq!(
            summary
                .states
                .iter()
                .find(|entry| entry.state == Some(WorkState::Ready))
                .unwrap()
                .count,
            1
        );
        assert_eq!(
            summary
                .states
                .iter()
                .find(|entry| entry.state.is_none())
                .unwrap()
                .count,
            1
        );
        assert_eq!(
            summary.risks,
            [wt_tui::WorkRiskCount {
                risk: WorkRisk::High,
                count: 1
            }]
        );
        assert_eq!(summary.blocked_notes, ["ready-row: release approval"]);
        assert_eq!(summary.stale_statuses, 1);
        assert_eq!(summary.verification_owed, 1);
        assert_eq!(summary.verification_overdue, 1);
        assert_eq!(summary.dirty_worktrees, Some(1));
        assert_eq!(summary.unknown_git, 1);
        assert_eq!(summary.upstream_ahead, 1);
        assert_eq!(summary.upstream_behind, 1);
        assert_eq!(summary.rebasing, 1);
        assert_eq!(summary.conflicted, 1);
        assert_eq!(summary.open_prs, 1);
        assert_eq!(summary.draft_prs, 1);
        assert_eq!(summary.queued_prs, 1);
        assert_eq!(summary.failing_checks, 1);
        assert_eq!(summary.paused_automations, 1);
        assert_eq!(summary.needs_attention, 1);
    }
}
