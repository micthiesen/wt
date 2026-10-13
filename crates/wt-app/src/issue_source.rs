//! One batched tracker reader for the fleet. Identity changes never borrow a
//! previous provider's or previous inventory's answer.
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use wt_core::Domain;
use wt_platform::process::CommandSpec;
use wt_runtime::{
    DiskCache, Fetched, LeafHandle, Persistence, RefreshPolicy, SourceHandle, SourceSnapshot,
    SourceSpec, Stamp, TaskScope, derived_channel, start_skippable_source,
};
use wt_tui::Board;

use crate::{context::AppContext, issue_identity};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusBatch {
    pub ids: Vec<String>,
    pub statuses: BTreeMap<String, String>,
}

pub struct IssueSources {
    pub board: SourceHandle<Board>,
    pub statuses: LeafHandle<StatusBatch>,
}

fn statuses_spec(context: &AppContext) -> SourceSpec<StatusBatch> {
    SourceSpec {
        name: "issue-statuses",
        domain: Domain::Issues,
        persistence: Persistence::Disk(DiskCache::new(&context.caches)),
        policy: RefreshPolicy {
            debounce: Duration::from_millis(50),
            minimum_interval: Duration::from_secs(1),
        },
    }
}

pub fn start(scope: &TaskScope, context: &AppContext, board: SourceHandle<Board>) -> IssueSources {
    let tracker = context.config.issue_tracker.clone();
    let command = tracker
        .as_ref()
        .and_then(|tracker| tracker.status_command.clone());
    let prefix = tracker.as_ref().and_then(|tracker| tracker.prefix.clone());
    let enabled = command.is_some();
    let statuses = start_skippable_source(scope, statuses_spec(context), {
        let context = context.clone();
        let board = board.clone();
        let prefix = prefix.clone();
        move |cancel| {
            let context = context.clone();
            let command = command.clone();
            // The id set comes from the derived board. Restored Git or
            // metadata may name issues that no longer exist, so the reader
            // waits for both to be live (rule E2); the board's next change
            // re-arms it.
            let ids = confirmed_ids(&board.snapshot(), prefix.as_deref());
            let observed = Stamp::now();
            async move {
                let Some(ids) = ids else {
                    return Ok(None);
                };
                let Some(command) = command.filter(|_| !ids.is_empty()) else {
                    return Ok::<_, anyhow::Error>(Some(Fetched::new(
                        StatusBatch {
                            ids,
                            statuses: BTreeMap::new(),
                        },
                        observed,
                    )));
                };
                let argv = arguments(&command, &ids);
                let (program, args) = argv
                    .split_first()
                    .context("issue status reader has no executable")?;
                let mut spec = CommandSpec::new(program)
                    .args(args)
                    .cwd(&context.config.paths.main_clone);
                spec.timeout = Duration::from_secs(30);
                let output = context
                    .processes
                    .run(spec, &cancel)
                    .await
                    .context("issue status reader")?;
                if !output.status.success() {
                    bail!(
                        "issue status reader exited {}: {}",
                        output.status,
                        output.stderr_text().trim()
                    );
                }
                Ok(Some(Fetched::new(
                    StatusBatch {
                        statuses: parse(&output.stdout, &ids)?,
                        ids,
                    },
                    observed,
                )))
            }
        }
    });
    let output = project(scope, board, statuses.view(), prefix, enabled);
    IssueSources {
        board: output,
        statuses,
    }
}

fn ids(board: &SourceSnapshot<Board>, prefix: Option<&str>) -> Vec<String> {
    board
        .data()
        .into_iter()
        .flat_map(|board| &board.rows)
        .filter_map(|row| row.issue_id.as_ref())
        .filter(|id| issue_identity::is_tracker(id, prefix))
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// The id set, only when the rows it comes from were scanned live by this
/// process. `None` while Git or metadata is restored or missing.
fn confirmed_ids(board: &SourceSnapshot<Board>, prefix: Option<&str>) -> Option<Vec<String>> {
    let live = |domain| {
        board
            .domains()
            .get(&domain)
            .is_some_and(|meta| meta.is_live())
    };
    (board.data().is_some() && live(Domain::Git) && live(Domain::Metadata))
        .then(|| ids(board, prefix))
}

fn arguments(command: &[String], ids: &[String]) -> Vec<String> {
    command
        .iter()
        .flat_map(|arg| {
            if arg == "{ids}" {
                ids.to_vec()
            } else {
                vec![arg.clone()]
            }
        })
        .collect()
}

fn parse(bytes: &[u8], ids: &[String]) -> Result<BTreeMap<String, String>> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Response {
        issues: Vec<Issue>,
    }
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Issue {
        id: String,
        status: String,
    }
    let response: Response =
        serde_json::from_slice(bytes).context("issue status reader: invalid JSON response")?;
    let expected = ids.iter().collect::<BTreeSet<_>>();
    let mut statuses = BTreeMap::new();
    for Issue { id, status } in response.issues {
        if !expected.contains(&id) {
            bail!("issue status reader: unexpected issue {id:?}");
        }
        if statuses.contains_key(&id) {
            bail!("issue status reader: duplicate issue {id}");
        }
        if status.trim().is_empty() || status.chars().any(char::is_control) {
            bail!(
                "issue status reader: invalid status for {id}: expected a nonempty single-line string"
            );
        }
        statuses.insert(id, status);
    }
    let missing = ids
        .iter()
        .filter(|id| !statuses.contains_key(*id))
        .cloned()
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        bail!(
            "issue status reader: missing issues: {}",
            missing.join(", ")
        );
    }
    Ok(statuses)
}

fn project(
    scope: &TaskScope,
    board: SourceHandle<Board>,
    statuses: SourceHandle<StatusBatch>,
    prefix: Option<String>,
    enabled: bool,
) -> SourceHandle<Board> {
    let (output, mut publisher) = derived_channel(scope, "issue-board");
    let cancel = scope.token();
    scope.spawn(async move {
        let mut boards = board.subscribe();
        let mut updates = statuses.subscribe();
        boards.mark_changed();
        updates.mark_changed();
        // Displayed ids select which statuses apply; confirmed ids decide
        // when to read. Restored rows can show restored statuses, but only a
        // live inventory triggers the reader.
        let mut previous = Vec::new();
        let mut confirmed = None::<Vec<String>>;
        let backstop = Duration::from_secs(180);
        let mut interval =
            tokio::time::interval_at(tokio::time::Instant::now() + backstop, backstop);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            let readable = enabled && confirmed.as_ref().is_some_and(|ids| !ids.is_empty());
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                request = publisher.requested() => {
                    if request.is_none() { break; }
                    board.refresh();
                    if confirmed.is_some() { statuses.refresh(); }
                    continue;
                }
                changed = boards.changed() => {
                    if changed.is_err() { break; }
                    let snapshot = boards.borrow_and_update().clone();
                    previous = ids(&snapshot, prefix.as_deref());
                    let next = confirmed_ids(&snapshot, prefix.as_deref());
                    if next != confirmed {
                        confirmed = next;
                        // A confirmed empty set (or disabled tracker) still
                        // needs a live empty publication to supersede restore.
                        // The fetch closure decides whether external I/O is needed.
                        if confirmed.is_some() {
                            statuses.refresh();
                        }
                    }
                }
                changed = updates.changed() => {
                    if changed.is_err() { break; }
                    updates.borrow_and_update();
                }
                _ = interval.tick(), if readable => { statuses.refresh(); continue; }
            }
            let next = compose(boards.borrow().clone(), updates.borrow().clone(), &previous);
            publisher.publish_if_changed(next);
        }
    });
    output
}

fn compose(
    board: SourceSnapshot<Board>,
    statuses: SourceSnapshot<StatusBatch>,
    expected: &[String],
) -> SourceSnapshot<Board> {
    let Some(data) = board.data() else {
        return board;
    };
    let mut data = data.as_ref().clone();
    let matching = statuses.data().filter(|batch| batch.ids == expected);
    for row in &mut data.rows {
        let Some(id) = row.issue_id.as_ref().filter(|id| expected.contains(id)) else {
            continue;
        };
        row.issue_status = matching.and_then(|batch| batch.statuses.get(id)).cloned();
        if let wt_runtime::SourceState::Failed(error) = statuses.state() {
            row.details.push(format!(
                "Tracker: {}",
                wt_core::sanitize_terminal_text(error)
            ));
        }
    }
    let board = board.with_data(Some(Arc::new(data)));
    // No displayed tracker fields depend on this leaf for an empty id set.
    // An unrelated restored answer must not taint the worktree list.
    if expected.is_empty() {
        board
    } else {
        board.with_input(&statuses)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wt_runtime::SourceState;
    use wt_tui::BoardRow;

    #[test]
    fn rejects_partial_duplicate_extra_and_control_statuses() {
        let ids = vec!["ENG-1".into()];
        for invalid in [
            r#"{"issues":[]}"#,
            r#"{"issues":[{"id":"ENG-1","status":"Open"},{"id":"ENG-1","status":"Open"}]}"#,
            r#"{"issues":[{"id":"ENG-2","status":"Open"}]}"#,
            r#"{"issues":[{"id":"ENG-1","status":"\u001b[31m"}]}"#,
            r#"{"issues":[{"id":"ENG-1","status":"Open","extra":true}]}"#,
        ] {
            assert!(parse(invalid.as_bytes(), &ids).is_err(), "{invalid}");
        }
        assert_eq!(
            parse(br#"{"issues":[{"id":"ENG-1","status":"In Review"}]}"#, &ids).unwrap()["ENG-1"],
            "In Review"
        );
        assert_eq!(
            arguments(
                &["reader".into(), "{ids}".into(), "literal{id}".into()],
                &ids
            ),
            vec!["reader", "ENG-1", "literal{id}"]
        );
    }

    fn snapshot<T>(value: T) -> SourceSnapshot<T> {
        SourceSnapshot::test_live(Some(value), SourceState::Ready, Domain::Issues)
    }

    /// A board scanned live by this process: Git and metadata both live.
    fn live_board(board: Board) -> SourceSnapshot<Board> {
        SourceSnapshot::test_live(Some(board), SourceState::Ready, Domain::Git).with_input(
            &SourceSnapshot::<()>::test_live(None, SourceState::Ready, Domain::Metadata),
        )
    }

    fn board(id: &str) -> Board {
        Board {
            rows: vec![BoardRow {
                issue_id: Some(id.into()),
                ..BoardRow::default()
            }],
            ..Board::default()
        }
    }

    fn leaf<T: Send + Sync + 'static>(
        scope: &TaskScope,
    ) -> (LeafHandle<T>, wt_runtime::LeafPublisher<T>) {
        wt_runtime::leaf_channel(
            scope,
            SourceSpec {
                name: "test",
                domain: Domain::Issues,
                persistence: Persistence::Never("test"),
                policy: RefreshPolicy::default(),
            },
        )
    }

    #[test]
    fn changed_inventory_never_borrows_previous_answer_and_failure_keeps_matching_data() {
        let statuses = snapshot(StatusBatch {
            ids: vec!["ENG-1".into()],
            statuses: [("ENG-1".into(), "Open".into())].into(),
        })
        .with_state(SourceState::Failed("offline".into()));
        let prior = compose(
            live_board(board("ENG-1")),
            statuses.clone(),
            &["ENG-1".into()],
        );
        assert_eq!(
            prior.data().unwrap().rows[0].issue_status.as_deref(),
            Some("Open")
        );
        let changed = compose(live_board(board("ENG-2")), statuses, &["ENG-2".into()]);
        assert_eq!(changed.data().unwrap().rows[0].issue_status, None);
    }

    #[tokio::test(start_paused = true)]
    async fn title_changes_and_status_results_never_trigger_another_reader() {
        let scope = TaskScope::new();
        let (boards, mut boards_writer) = derived_channel(&scope, "test-board");
        let (statuses, mut status_writer) = leaf::<StatusBatch>(&scope);
        let output = project(&scope, boards, statuses.view(), Some("ENG".into()), true);
        boards_writer.publish(live_board(board("ENG-1")));
        assert_eq!(status_writer.requested().await, Some(()));
        status_writer.publish_live(
            StatusBatch {
                ids: vec!["ENG-1".into()],
                statuses: [("ENG-1".into(), "Open".into())].into(),
            },
            Stamp::now(),
        );
        output
            .subscribe()
            .wait_for(|s| s.data().is_some_and(|b| b.rows[0].issue_status.is_some()))
            .await
            .unwrap();
        let mut renamed = board("ENG-1");
        renamed.rows[0].title = "Renamed".into();
        boards_writer.publish(live_board(renamed));
        output
            .subscribe()
            .wait_for(|s| s.data().is_some_and(|b| b.rows[0].title == "Renamed"))
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(1), status_writer.requested())
                .await
                .is_err()
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(1), boards_writer.requested())
                .await
                .is_err()
        );
        scope.shutdown(Duration::from_secs(1)).await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn restored_rows_never_trigger_the_tracker_reader() {
        let scope = TaskScope::new();
        let (boards, boards_writer) = derived_channel(&scope, "test-board");
        let (statuses, mut status_writer) = leaf::<StatusBatch>(&scope);
        let output = project(&scope, boards, statuses.view(), Some("ENG".into()), true);
        let restored = SourceSnapshot::test_restored(board("ENG-1"), Domain::Git, 1).with_input(
            &SourceSnapshot::<()>::test_live(None, SourceState::Ready, Domain::Metadata),
        );
        assert_eq!(confirmed_ids(&restored, Some("ENG")), None);
        boards_writer.publish(restored);
        output
            .subscribe()
            .wait_for(|s| s.data().is_some())
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), status_writer.requested())
                .await
                .is_err(),
            "restored ids must not start a tracker read"
        );
        // The same ids, now scanned live, start the first read.
        boards_writer.publish(live_board(board("ENG-1")));
        assert_eq!(status_writer.requested().await, Some(()));
        assert_eq!(
            confirmed_ids(&live_board(board("ENG-1")), Some("ENG")),
            Some(vec!["ENG-1".to_owned()])
        );
        scope.shutdown(Duration::from_secs(1)).await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn confirmed_empty_and_disabled_inputs_clear_restore_and_manual_refresh_reaches_leaf() {
        for (enabled, id) in [(true, ""), (false, "ENG-1")] {
            let scope = TaskScope::new();
            let (boards, boards_writer) = derived_channel(&scope, "test-board");
            let (statuses, mut status_writer) = derived_channel(&scope, "test-statuses");
            let output = project(&scope, boards, statuses, Some("ENG".into()), enabled);
            status_writer.publish(SourceSnapshot::test_restored(
                StatusBatch::default(),
                Domain::Issues,
                1,
            ));
            boards_writer.publish(live_board(board(id)));
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(1), status_writer.requested())
                    .await
                    .expect("leaf refresh request must arrive"),
                Some(()),
                "confirmed input must supersede restored data even without external I/O"
            );
            status_writer.publish(snapshot(StatusBatch::default()));
            output
                .subscribe()
                .wait_for(|s| {
                    s.data().is_some()
                        && s.domains()
                            .get(&Domain::Issues)
                            .is_none_or(|meta| meta.is_live())
                })
                .await
                .unwrap();
            output.refresh();
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(1), status_writer.requested())
                    .await
                    .expect("leaf refresh request must arrive"),
                Some(()),
                "manual refresh must reach the leaf for confirmed input"
            );
            scope.shutdown(Duration::from_secs(1)).await.unwrap();
        }
    }

    #[test]
    fn unused_restore_is_not_a_dependency_but_matching_restore_remains_visible_until_confirmed() {
        let restored = SourceSnapshot::test_restored(
            StatusBatch {
                ids: vec!["ENG-1".into()],
                statuses: [("ENG-1".into(), "Open".into())].into(),
            },
            Domain::Issues,
            1,
        );
        let unrelated = compose(live_board(board("")), restored.clone(), &[]);
        assert!(!unrelated.domains().contains_key(&Domain::Issues));
        let matching = compose(
            live_board(board("ENG-1")),
            restored.clone(),
            &["ENG-1".into()],
        );
        assert!(!matching.domains()[&Domain::Issues].is_live());
        let failed = compose(
            live_board(board("ENG-1")),
            restored
                .clone()
                .with_state(SourceState::Failed("offline".into())),
            &["ENG-1".into()],
        );
        assert!(!failed.domains()[&Domain::Issues].is_live());
        // An unchanged successful answer confirms the leaf; equality of data
        // must never keep its restored provenance sticky.
        let live = compose(
            live_board(board("ENG-1")),
            snapshot(restored.data().unwrap().as_ref().clone()),
            &["ENG-1".into()],
        );
        assert!(live.domains()[&Domain::Issues].is_live());
        assert_eq!(
            matching.data().unwrap().rows[0].issue_status,
            live.data().unwrap().rows[0].issue_status
        );
    }

    #[tokio::test(start_paused = true)]
    async fn issue_statuses_meet_the_persisted_source_contract() {
        wt_runtime::conformance::persisted_source(
            "issue-statuses",
            Domain::Issues,
            StatusBatch {
                ids: vec!["ENG-101".into(), "ENG-102".into()],
                statuses: [
                    ("ENG-101".into(), "In Review".into()),
                    ("ENG-102".into(), "Todo".into()),
                ]
                .into(),
            },
        )
        .await;
    }
}
