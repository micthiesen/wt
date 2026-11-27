//! Each worktree's recorded history: the leaf that publishes it to the board
//! and the observer that records what changes outside wt.
//!
//! The log lives in SQLite (`wt-store`). wt's own writes append to it where
//! they happen. Pull request state, checks, reviews, the merge queue,
//! conflicts and landings are not written by wt, so the host that owns the
//! sources watches the confirmed GitHub and Git facts and appends what
//! changed. What has already been recorded is read from the log itself, in
//! the appending transaction, so a restart or a second host on the same
//! database records each fact once.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use wt_app_core::{
    context::AppContext,
    worktree_history::{
        BOARD_EVENTS_PER_ROW, ObservedChecks, ObservedPr, ObservedRow, PrPhase, status_transition,
        transitions,
    },
};
use wt_core::{Domain, LandedOn, ReviewChange, WorktreeEvent};
use wt_github::{GithubData, PrChecks, PrReview, PullRequest};
use wt_runtime::{
    Confirmed, Fetched, LeafHandle, Persistence, RefreshPolicy, SourceHandle, SourceSnapshot,
    SourceSpec, SourceState, Stamp, TaskScope, derived_channel, start_skippable_source,
};
use wt_tui::{Board, LandingKind};
use wt_vcs::WorktreeSnapshot;

use crate::local_source::Metadata;

/// Recorded events by local worktree slug, oldest first.
pub type RowHistory = BTreeMap<String, Vec<WorktreeEvent>>;

/// How often events past the retention age are swept from slugs that are
/// never written again.
const PRUNE_EVERY: Duration = Duration::from_secs(60 * 60);

fn spec() -> SourceSpec<RowHistory> {
    SourceSpec {
        name: "row-history",
        domain: Domain::History,
        persistence: Persistence::Never("authoritative SQLite; read directly"),
        policy: RefreshPolicy {
            debounce: Duration::from_millis(15),
            minimum_interval: Duration::from_millis(50),
        },
    }
}

/// Publish the newest events of every live local worktree and of every
/// removed one the state still remembers, in one query. A removed slug reads
/// as its last incarnation, ending at its removal.
///
/// It reads again whenever the metadata leaf does: that leaf is refreshed by
/// the state database's file watcher and backstop, so an append by another
/// process (`wt status` in an agent's shell) arrives the same way a status
/// does. The slugs come from the Git inventory and the removal ledger, so the
/// read waits until both are confirmed.
///
/// A read that fails publishes an empty history and warns. The timeline is
/// then blank; nothing else on the board depends on this lane.
pub fn start(
    scope: &TaskScope,
    context: &AppContext,
    git: LeafHandle<Vec<WorktreeSnapshot>>,
    metadata: LeafHandle<Metadata>,
) -> LeafHandle<RowHistory> {
    // What the last publish reflects: the log's revision and the slugs read.
    let published = Arc::new(Mutex::new(None::<(i64, Vec<String>)>));
    let history = start_skippable_source(scope, spec(), {
        let context = context.clone();
        let git = git.clone();
        let metadata = metadata.clone();
        move |_cancel| {
            let context = context.clone();
            let inputs = git.confirmed().zip(metadata.confirmed());
            let published = published.clone();
            async move {
                let Some((inventory, metadata)) = inputs else {
                    return Ok::<_, String>(None);
                };
                let slugs = history_slugs(&inventory, &metadata.0);
                let observed = Stamp::now();
                let known = published.lock().ok().and_then(|known| known.clone());
                let read = context
                    .database
                    .call(move |store| {
                        let revision = store.worktree_events_revision()?;
                        if known.is_some_and(|(seen, read)| seen == revision && read == slugs) {
                            return Ok(None);
                        }
                        let mut events =
                            store.read_worktree_events_for(&slugs, BOARD_EVENTS_PER_ROW)?;
                        for event in events.values_mut().flatten() {
                            wt_app_core::worktree_history::sanitize(&mut event.event);
                        }
                        Ok(Some((revision, slugs, events)))
                    })
                    .await;
                let mut published = published.lock().map_err(|_| "row history lock poisoned")?;
                match read {
                    Ok(Some((revision, slugs, events))) => {
                        *published = Some((revision, slugs));
                        Ok(Some(Fetched::new(events, observed)))
                    }
                    Ok(None) => Ok(None),
                    Err(error) => {
                        // This lane shares the History domain with the
                        // removed-rows pane, so it never reports failure
                        // there. Forgetting what was published makes the
                        // next read publish again.
                        if published.take().is_some() {
                            tracing::warn!(error = %format!("{error:#}"), "could not read worktree history; timelines are empty");
                        }
                        Ok(Some(Fetched::new(RowHistory::new(), observed)))
                    }
                }
            }
        }
    });
    let cancellation = scope.token();
    let refresh = history.view();
    let git = git.view();
    let metadata = metadata.view();
    scope.spawn(async move {
        let mut metadata_updates = metadata.subscribe();
        let mut git_updates = git.subscribe();
        metadata_updates.mark_changed();
        git_updates.mark_changed();
        loop {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => break,
                changed = metadata_updates.changed() => {
                    if changed.is_err() { break; }
                    metadata_updates.borrow_and_update();
                }
                changed = git_updates.changed() => {
                    if changed.is_err() { break; }
                    git_updates.borrow_and_update();
                }
            }
            refresh.refresh();
        }
    });
    history
}

/// Attach each local row's history under its row key: live rows, and the
/// removed rows the history pane lists. Rows of other hosts keep whatever
/// their own host published: a slug names a different worktree on every
/// host.
pub fn overlay(
    scope: &TaskScope,
    board: SourceHandle<Board>,
    history: SourceHandle<RowHistory>,
) -> SourceHandle<Board> {
    let (output, mut publisher) = derived_channel(scope, "row-history-board");
    let cancellation = scope.token();
    scope.spawn(async move {
        let mut boards = board.subscribe();
        let mut histories = history.subscribe();
        boards.mark_changed();
        histories.mark_changed();
        loop {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => break,
                request = publisher.requested() => {
                    if request.is_none() { break; }
                    board.refresh();
                    history.refresh();
                    continue;
                }
                changed = boards.changed() => if changed.is_err() { break; },
                changed = histories.changed() => if changed.is_err() { break; },
            }
            let base = boards.borrow_and_update().clone();
            let history_snapshot = histories.borrow_and_update().clone();
            publisher.publish_if_changed(attach(base, &history_snapshot));
        }
    });
    output
}

fn attach(
    base: SourceSnapshot<Board>,
    history: &SourceSnapshot<RowHistory>,
) -> SourceSnapshot<Board> {
    // A lane that failed outright (a panicked read) has nothing to show and
    // must not lend its state to the board's History domain.
    if matches!(history.state(), SourceState::Failed(_)) {
        return base;
    }
    let (Some(board), Some(events)) = (base.data(), history.data()) else {
        return base;
    };
    let live = board
        .rows
        .iter()
        .filter(|row| local_worktree(row))
        .map(|row| (&row.key, &row.slug));
    let removed = board
        .removed_history
        .rows
        .iter()
        .filter(|row| row.host.is_none() && !wt_core::is_remote_worktree_ledger_key(&row.key))
        .map(|row| (&row.key, &row.slug));
    let row_history: BTreeMap<_, _> = live
        .chain(removed)
        .filter_map(|(key, slug)| Some((key.clone(), events.get(slug)?.clone())))
        .collect();
    if row_history.is_empty() {
        return base;
    }
    let mut board = board.as_ref().clone();
    board.row_history = row_history;
    base.with_data(Some(Arc::new(board))).with_input(history)
}

/// Every slug whose history the board can show: live local worktrees and the
/// removals the state still holds. A slug is one or the other on the board;
/// the removed-history source drops a removal whose slug is live again.
fn history_slugs(inventory: &[WorktreeSnapshot], state: &serde_json::Value) -> Vec<String> {
    let live = inventory
        .iter()
        .filter(|row| {
            !row.worktree.is_main
                && matches!(
                    row.worktree.target.location(),
                    wt_core::WorktreeLocation::Local
                )
        })
        .map(|row| row.worktree.target.slug().to_owned());
    let removed = state
        .get("removed")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.get("slug")?.as_str().map(str::to_owned));
    live.chain(removed)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn local_worktree(row: &wt_tui::BoardRow) -> bool {
    !row.is_main
        && !row.pending_creation
        && row.host.is_none()
        && !row.slug.is_empty()
        && !wt_core::is_remote_worktree_ledger_key(&row.key)
}

/// Record what changes outside wt. `board` must carry the Git presentation
/// facts; `history` is refreshed after an append so the row shows it without
/// waiting for the file watcher.
///
/// Runs in the host process only, off the input path. Events are written
/// from confirmed inputs alone (see [`observations`]).
pub fn observe(
    scope: &TaskScope,
    context: &AppContext,
    board: SourceHandle<Board>,
    github: LeafHandle<GithubData>,
    metadata: LeafHandle<Metadata>,
    history: SourceHandle<RowHistory>,
) {
    let cancellation = scope.token();
    let context = context.clone();
    scope.spawn(async move {
        let mut boards = board.subscribe();
        let mut githubs = github.subscribe();
        let mut histories = history.subscribe();
        let mut states = metadata.subscribe();
        boards.mark_changed();
        githubs.mark_changed();
        states.mark_changed();
        let mut prune = tokio::time::interval(PRUNE_EVERY);
        prune.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // Facts this process has already reconciled with the log. It only
        // saves reads: the log decides what is new, so losing this at a
        // restart, or another host recording first, adds nothing twice.
        let mut reconciled = BTreeMap::<String, ObservedRow>::new();
        // When each slug's published history says it was created. A slug
        // that is created again has a new, empty history, so what was
        // reconciled against the old one no longer counts.
        let mut incarnations = BTreeMap::<String, u64>::new();
        let mut history_open = true;
        loop {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => break,
                _ = prune.tick() => {
                    let now = wt_app_core::feeds::epoch_ms();
                    let pruned = context
                        .database
                        .call(move |store| Ok(store.prune_worktree_events(now)?))
                        .await;
                    if let Err(error) = pruned {
                        tracing::warn!(%error, "could not prune worktree history");
                    }
                    continue;
                }
                changed = boards.changed() => if changed.is_err() { break; },
                changed = githubs.changed() => if changed.is_err() { break; },
                changed = states.changed() => if changed.is_err() { break; },
                // The history lane ending is not a reason to stop recording.
                changed = histories.changed(), if history_open => history_open = changed.is_ok(),
            }
            if let Some(published) = histories.borrow_and_update().data() {
                let created = created_at(published);
                reconciled.retain(|slug, _| incarnations.get(slug) == created.get(slug));
                incarnations = created;
            }
            let board_snapshot = boards.borrow_and_update().clone();
            let github_snapshot = githubs.borrow_and_update().clone();
            let confirmed = github.confirm(&github_snapshot).filter(|_| {
                // A failed fetch keeps its last data; that is not a new read.
                matches!(github_snapshot.state(), SourceState::Ready)
            });
            let bot = context.config.review_bot.name.clone();
            // Choosing a branch's pull request stats the checkout.
            let observed = tokio::task::spawn_blocking(move || {
                observations(&board_snapshot, confirmed.as_ref(), &bot)
            })
            .await;
            let Ok(mut observed) = observed else { continue };
            // The stored status, for statuses written without an event (see
            // `status_transition`). This snapshot only says a status may
            // have changed; the store compares the one it reads itself.
            let state_snapshot = states.borrow_and_update().clone();
            if let Some(state) = metadata
                .confirm(&state_snapshot)
                .filter(|_| matches!(state_snapshot.state(), SourceState::Ready))
            {
                for (slug, row) in &mut observed {
                    row.work = Some(
                        state
                            .0
                            .get("slugs")
                            .and_then(|slugs| slugs.get(slug)?.get("work"))
                            .filter(|work| !work.is_null())
                            .map(ToString::to_string),
                    );
                }
            }
            reconciled.retain(|slug, _| observed.contains_key(slug));
            // An unknown fact keeps the last known one, so a fetch gap
            // (GitHub refreshing or failed, then back) is not a change on
            // every row.
            let changed: BTreeMap<String, ObservedRow> = observed
                .into_iter()
                .map(|(slug, row)| match reconciled.get(&slug) {
                    Some(last) => (slug, row.or_last_known(last)),
                    None => (slug, row),
                })
                .filter(|(slug, row)| reconciled.get(slug) != Some(row))
                .collect();
            if changed.is_empty() {
                continue;
            }
            let at_ms = wt_app_core::feeds::epoch_ms();
            let facts = changed.clone();
            let appended = context
                .database
                .call(move |store| {
                    let slugs: Vec<String> = facts.keys().cloned().collect();
                    Ok(
                        store.reconcile_worktree_history(at_ms, &slugs, |slug, events, work| {
                            let Some(row) = facts.get(slug) else {
                                return Vec::new();
                            };
                            // The status first: it carries its own, earlier
                            // time, and the rest is dated now.
                            let status = row
                                .work
                                .as_ref()
                                .and_then(|_| status_transition(events, work, at_ms));
                            status
                                .into_iter()
                                .chain(
                                    transitions(events, row)
                                        .into_iter()
                                        .map(|event| (None, event)),
                                )
                                .collect()
                        })?,
                    )
                })
                .await;
            match appended {
                Ok(appended) => {
                    reconciled.extend(changed);
                    if appended > 0 {
                        history.refresh();
                    }
                }
                Err(error) => tracing::warn!(%error, "could not record observed worktree history"),
            }
        }
    });
}

/// The time of each slug's `created` event, where its published history
/// still begins with one.
fn created_at(history: &RowHistory) -> BTreeMap<String, u64> {
    history
        .iter()
        .filter_map(|(slug, events)| {
            let first = events.first()?;
            matches!(first.event, wt_core::WorktreeEventKind::Created { .. })
                .then(|| (slug.clone(), first.at_ms))
        })
        .collect()
}

/// What the current snapshots prove about each local worktree, by slug.
///
/// Restored cache data is display-only and proves nothing, so:
/// - no row is observed until the board's Git inventory and metadata are
///   live;
/// - pull request facts need a confirmed, ready GitHub read that covered the
///   row's branch. A branch a read did not cover is unknown, never "no PR";
/// - conflict and landing facts need live Git presentation facts.
fn observations(
    board: &SourceSnapshot<Board>,
    github: Option<&Confirmed<GithubData>>,
    review_bot: &str,
) -> BTreeMap<String, ObservedRow> {
    let Some(data) = board
        .data()
        .filter(|_| crate::sources::rows_confirmed(board))
    else {
        return BTreeMap::new();
    };
    let git_facts = board
        .domains()
        .get(&Domain::Presentation)
        .is_some_and(|meta| meta.is_live() && !matches!(meta.state(), SourceState::Failed(_)));
    data.rows
        .iter()
        .filter(|row| local_worktree(row))
        .map(|row| {
            let pr = github
                .filter(|data| data.covered_branches.contains(&row.branch))
                .and_then(|data| {
                    let pr = wt_github::pick_pr_for_worktree(
                        Some(&row.branch),
                        Path::new(&row.path),
                        &data.prs,
                    )?;
                    // Absence from one page of a longer queue proves nothing.
                    let queued = if data.merge_queue.contains_key(&row.branch) {
                        Some(true)
                    } else {
                        data.merge_queue_complete.then_some(false)
                    };
                    Some(observed_pr(pr, queued, review_bot))
                });
            let landed = git_facts.then_some(row.git.landed_on).flatten();
            let observed = ObservedRow {
                pr,
                conflicted: git_facts
                    .then(|| {
                        row.git
                            .base_conflicts
                            .as_ref()
                            .map(|files| !files.is_empty())
                    })
                    .flatten(),
                landed: landed.map(|landed| match landed {
                    LandingKind::Base => LandedOn::Base,
                    LandingKind::Production => LandedOn::Production,
                }),
                landed_head: landed.and(row.git.head_sha.clone()),
                landed_at_ms: landed.and(row.git.landed_at_ms),
                work: None,
            };
            (row.slug.clone(), observed)
        })
        .collect()
}

fn observed_pr(pr: &PullRequest, queued: Option<bool>, review_bot: &str) -> ObservedPr {
    let phase = match pr.state.as_str() {
        "MERGED" => PrPhase::Merged,
        "CLOSED" => PrPhase::Closed,
        _ if pr.is_draft => PrPhase::Draft,
        _ => PrPhase::Open,
    };
    ObservedPr {
        number: pr.number,
        phase,
        checks: match pr.checks {
            PrChecks::Pass => Some(ObservedChecks::Passing),
            PrChecks::Fail => Some(ObservedChecks::Failing(pr.failed_checks.clone())),
            PrChecks::Pending | PrChecks::None => None,
        },
        // A verdict on an older head, or one still being produced, is not
        // a verdict on this one.
        bot_review: pr
            .review_bot
            .as_ref()
            .filter(|bot| bot.stale != Some(true))
            .and_then(|bot| match bot.state.as_str() {
                "unresolved" => Some(ReviewChange::Findings {
                    count: bot.unresolved,
                }),
                "clean" => Some(ReviewChange::Clean),
                _ => None,
            })
            .map(|verdict| (review_bot.to_owned(), verdict)),
        people_review: match pr.review {
            PrReview::Approved => Some(ReviewChange::Approved),
            PrReview::ChangesRequested => Some(ReviewChange::ChangesRequested),
            PrReview::Pending | PrReview::Unrequested | PrReview::None => None,
        },
        queued,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wt_core::{ChecksChange, PrChange, WorktreeEventKind};
    use wt_runtime::{LeafPublisher, leaf_channel};
    use wt_tui::BoardRow;

    fn pull_request(number: u64, checks: &str) -> PullRequest {
        serde_json::from_value(serde_json::json!({
            "number": number, "url": format!("https://github.com/o/r/pull/{number}"),
            "headRefName": "feature/one", "baseRefName": "main", "title": "A PR",
            "isDraft": false, "state": "OPEN", "checks": checks, "failedChecks": ["tests"],
            "review": "pending", "reviewRequests": 0, "requestedReviewers": [],
            "suggestedReviewers": [], "comments": [], "unresolvedThreads": 0,
            "unresolvedThreadsTotal": 0
        }))
        .unwrap()
    }

    fn github(number: u64, checks: &str) -> GithubData {
        GithubData {
            prs: [("feature/one".into(), pull_request(number, checks))].into(),
            covered_branches: ["feature/one".into()].into(),
            ..GithubData::default()
        }
    }

    fn row(conflicts: Option<Vec<String>>) -> BoardRow {
        let mut row = BoardRow {
            key: "one".into(),
            slug: "one".into(),
            branch: "feature/one".into(),
            path: "/tmp/nonexistent-wt-history-fixture".into(),
            ..BoardRow::default()
        };
        row.git.base_conflicts = conflicts;
        row
    }

    fn live_board(rows: Vec<BoardRow>) -> SourceSnapshot<Board> {
        SourceSnapshot::test_live(
            Some(Board {
                rows,
                ..Board::default()
            }),
            SourceState::Ready,
            Domain::Git,
        )
        .test_also_live(Domain::Metadata)
        .test_also_live(Domain::Presentation)
    }

    fn github_spec() -> SourceSpec<GithubData> {
        SourceSpec {
            name: "github",
            domain: Domain::Github,
            persistence: Persistence::Never("test fixture"),
            policy: RefreshPolicy::default(),
        }
    }

    async fn events(context: &AppContext, slug: &'static str) -> Vec<WorktreeEventKind> {
        context
            .database
            .call(move |store| Ok(store.read_worktree_events(slug, 500)?))
            .await
            .unwrap()
            .into_iter()
            .map(|event| event.event)
            .collect()
    }

    async fn wait_for_events(
        context: &AppContext,
        slug: &'static str,
        done: impl Fn(&[WorktreeEventKind]) -> bool,
    ) -> Vec<WorktreeEventKind> {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let seen = events(context, slug).await;
                if done(&seen) {
                    return seen;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the observer records the events")
    }

    #[test]
    fn only_confirmed_inputs_are_observed() {
        let data = Confirmed::for_test(github(7, "fail"));
        let conflicted = || vec![row(Some(vec!["src/lib.rs".into()]))];
        let full = observations(&live_board(conflicted()), Some(&data), "Reviewer");
        assert_eq!(full["one"].pr.as_ref().map(|pr| pr.number), Some(7));
        assert_eq!(full["one"].conflicted, Some(true));

        // A board restored from cache names rows that may no longer exist.
        let restored = SourceSnapshot::test_restored(
            Board {
                rows: conflicted(),
                ..Board::default()
            },
            Domain::Git,
            1,
        )
        .test_also_live(Domain::Metadata)
        .test_also_live(Domain::Presentation);
        assert!(observations(&restored, Some(&data), "Reviewer").is_empty());

        // Without a confirmed GitHub read the pull request is unknown.
        let no_github = observations(&live_board(conflicted()), None, "Reviewer");
        assert_eq!(no_github["one"].pr, None);
        assert_eq!(no_github["one"].conflicted, Some(true));

        // A read that did not cover the branch is not proof of anything.
        let mut uncovered = github(7, "fail");
        uncovered.covered_branches.clear();
        let uncovered = Confirmed::for_test(uncovered);
        assert_eq!(
            observations(&live_board(conflicted()), Some(&uncovered), "Reviewer")["one"].pr,
            None
        );

        // Restored Git facts say nothing about conflicts or landings.
        let stale_facts = SourceSnapshot::test_live(
            Some(Board {
                rows: conflicted(),
                ..Board::default()
            }),
            SourceState::Ready,
            Domain::Git,
        )
        .test_also_live(Domain::Metadata);
        let stale = observations(&stale_facts, Some(&data), "Reviewer");
        assert_eq!(stale["one"].conflicted, None);
        assert!(stale["one"].pr.is_some());

        // Remote rows belong to their own host's log; the main checkout and
        // a row that is still being created have none.
        let mut remote = row(None);
        remote.host = Some("builder".into());
        let mut main = row(None);
        main.is_main = true;
        let mut pending = row(None);
        pending.pending_creation = true;
        assert!(
            observations(
                &live_board(vec![remote, main, pending]),
                Some(&data),
                "Reviewer"
            )
            .is_empty()
        );
    }

    struct Observer {
        scope: TaskScope,
        board: wt_runtime::DerivedPublisher<Board>,
        github: LeafPublisher<GithubData>,
        metadata: LeafPublisher<Metadata>,
    }

    impl Observer {
        /// What the metadata leaf publishes after reading the state.
        async fn read_state(&self, context: &AppContext) {
            let state = context
                .database
                .call(|store| Ok(store.read_wt_state()?))
                .await
                .unwrap();
            self.metadata
                .publish_live((state, BTreeSet::new()), Stamp::now());
        }
    }

    /// A status as a build without the event log writes it: the state
    /// changes and no event is appended.
    async fn old_build_writes(context: &AppContext, slug: &'static str, work: serde_json::Value) {
        context
            .database
            .call(move |store| {
                let mut state = store.read_wt_state()?;
                if work.is_null() {
                    state["slugs"][slug]
                        .as_object_mut()
                        .map(|entry| entry.remove("work"));
                } else {
                    state["slugs"][slug]["work"] = work;
                }
                store.write_repository_state_json(&state.to_string())?;
                Ok(())
            })
            .await
            .unwrap();
    }

    fn observer(context: &AppContext) -> Observer {
        let scope = TaskScope::new();
        let (board, board_publisher) = derived_channel(&scope, "board");
        let (github, github_publisher) = leaf_channel(&scope, github_spec());
        let history = SourceHandle::constant(SourceSnapshot::default());
        let (metadata, metadata_publisher) = leaf_channel(
            &scope,
            SourceSpec::<Metadata> {
                name: "metadata",
                domain: Domain::Metadata,
                persistence: Persistence::Never("test fixture"),
                policy: RefreshPolicy::default(),
            },
        );
        observe(&scope, context, board, github, metadata, history);
        Observer {
            metadata: metadata_publisher,
            scope,
            board: board_publisher,
            github: github_publisher,
        }
    }

    #[tokio::test]
    async fn the_observer_records_each_fact_once_across_a_restart_and_a_second_host() {
        let fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        let context = &fixture.ctx;
        let first = observer(context);
        first.board.publish(live_board(vec![row(Some(Vec::new()))]));
        first.github.publish_live(github(7, "fail"), Stamp::now());
        let baseline = wait_for_events(context, "one", |events| events.len() == 2).await;
        assert_eq!(
            baseline,
            [
                WorktreeEventKind::Pr {
                    number: 7,
                    change: PrChange::Opened
                },
                WorktreeEventKind::Checks {
                    state: ChecksChange::Failing,
                    failed: vec!["tests".into()]
                },
            ]
        );
        // Republishing the same facts is not news.
        first.github.publish_live(github(7, "fail"), Stamp::now());
        first.board.publish(live_board(vec![row(Some(Vec::new()))]));

        // A second host on the same database, and then a restart of the
        // first, start with no memory and see the same facts.
        let second = observer(context);
        second
            .board
            .publish(live_board(vec![row(Some(Vec::new()))]));
        second.github.publish_live(github(7, "fail"), Stamp::now());
        first.scope.shutdown(Duration::from_secs(2)).await.unwrap();
        let restarted = observer(context);
        restarted
            .board
            .publish(live_board(vec![row(Some(Vec::new()))]));
        restarted
            .github
            .publish_live(github(7, "fail"), Stamp::now());

        // Both now see the checks pass; exactly one of them records it.
        second.github.publish_live(github(7, "pass"), Stamp::now());
        restarted
            .github
            .publish_live(github(7, "pass"), Stamp::now());
        let recovered = wait_for_events(context, "one", |events| events.len() >= 3).await;
        tokio::time::sleep(Duration::from_millis(150)).await;
        let settled = events(context, "one").await;
        assert_eq!(settled, recovered, "nothing is recorded twice");
        assert_eq!(settled.len(), 3);
        assert_eq!(
            settled[2],
            WorktreeEventKind::Checks {
                state: ChecksChange::Passing,
                failed: Vec::new()
            }
        );
        second.scope.shutdown(Duration::from_secs(2)).await.unwrap();
        restarted
            .scope
            .shutdown(Duration::from_secs(2))
            .await
            .unwrap();
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn a_failed_or_restored_read_records_nothing() {
        let fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        let context = &fixture.ctx;
        let observer = observer(context);
        observer
            .github
            .publish_live(github(7, "fail"), Stamp::now());
        // The fetch then fails: its last data stays visible but is no read.
        observer
            .github
            .set_state(SourceState::Failed("offline".into()));
        let restored = SourceSnapshot::test_restored(
            Board {
                rows: vec![row(Some(vec!["src/lib.rs".into()]))],
                ..Board::default()
            },
            Domain::Git,
            1,
        )
        .test_also_live(Domain::Metadata)
        .test_also_live(Domain::Presentation);
        observer.board.publish(restored);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(events(context, "one").await.is_empty());
        // The same rows confirmed live, with GitHub still failed: only the
        // Git fact is recorded.
        observer
            .board
            .publish(live_board(vec![row(Some(vec!["src/lib.rs".into()]))]));
        let seen = wait_for_events(context, "one", |events| !events.is_empty()).await;
        assert_eq!(seen, [WorktreeEventKind::Conflict { resolved: false }]);
        observer
            .scope
            .shutdown(Duration::from_secs(2))
            .await
            .unwrap();
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn the_source_republishes_after_another_connection_appends() {
        let fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        let context = &fixture.ctx;
        let scope = TaskScope::new();
        let (git, git_publisher) = leaf_channel(
            &scope,
            SourceSpec {
                name: "git",
                domain: Domain::Git,
                persistence: Persistence::Never("test fixture"),
                policy: RefreshPolicy::default(),
            },
        );
        let (metadata, metadata_publisher) = leaf_channel(
            &scope,
            SourceSpec::<Metadata> {
                name: "metadata",
                domain: Domain::Metadata,
                persistence: Persistence::Never("test fixture"),
                policy: RefreshPolicy::default(),
            },
        );
        let history = start(&scope, context, git.clone(), metadata.clone());
        let (boards, board_publisher) = derived_channel(&scope, "board");
        let board = overlay(&scope, boards, history.view());
        let mut remote = row(None);
        remote.key = wt_core::remote_worktree_ledger_key("builder", "one");
        remote.host = Some("builder".into());
        board_publisher.publish(live_board(vec![row(None), remote]));
        let publish_metadata = || {
            metadata_publisher.publish_live(
                (serde_json::json!({"slugs": {}}), BTreeSet::new()),
                Stamp::now(),
            );
        };
        publish_metadata();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            history.snapshot().data().is_none(),
            "nothing is read for slugs the Git scan has not confirmed"
        );
        let inventory = context
            .repository
            .inventory_status(&context.cancellation)
            .await
            .unwrap();
        assert!(
            inventory
                .iter()
                .any(|row| row.worktree.target.slug() == "one")
        );
        git_publisher.publish_live(inventory, Stamp::now());
        let mut updates = board.subscribe();
        // Another process (an agent's `wt status`) writes through its own
        // connection; the state watcher then refreshes the metadata leaf.
        let mut other = wt_store::Store::open(
            &context.config.paths.state_db,
            wt_store::RepositoryIdentity::new(
                &context.config.repo_id,
                context.config.repo_path.to_string_lossy(),
            ),
        )
        .unwrap();
        for (number, expected) in [(7, 1), (8, 2)] {
            other
                .append_worktree_event(
                    "one",
                    number,
                    &WorktreeEventKind::Pr {
                        number,
                        change: PrChange::Opened,
                    },
                )
                .unwrap();
            publish_metadata();
            let seen = tokio::time::timeout(
                Duration::from_secs(5),
                updates.wait_for(|snapshot| {
                    snapshot.data().is_some_and(|board| {
                        board.row_history.get("one").map(Vec::len) == Some(expected)
                    })
                }),
            )
            .await
            .expect("the board carries the appended event")
            .unwrap()
            .clone();
            let board = seen.data().unwrap();
            assert_eq!(
                board.row_history.len(),
                1,
                "another host's row with the same slug gets no local history"
            );
            assert_eq!(board.row_history["one"].last().unwrap().at_ms, number);
        }
        assert!(history.confirmed().is_some(), "read live, never restored");
        scope.shutdown(Duration::from_secs(2)).await.unwrap();
        fixture.close().await.unwrap();
    }

    #[test]
    fn a_pull_request_missing_from_a_full_queue_page_is_not_known_to_have_left() {
        let mut data = github(7, "pass");
        let queued = |data: &GithubData| {
            observations(
                &live_board(vec![row(None)]),
                Some(&Confirmed::for_test(data.clone())),
                "Reviewer",
            )["one"]
                .pr
                .as_ref()
                .unwrap()
                .queued
        };
        assert_eq!(
            queued(&data),
            None,
            "the read does not say it saw the whole queue"
        );
        data.merge_queue_complete = true;
        assert_eq!(queued(&data), Some(false));
    }

    #[test]
    fn a_failed_history_lane_leaves_the_board_and_its_domains_untouched() {
        let board = live_board(vec![row(None)]);
        let events: RowHistory = [(
            "one".to_owned(),
            vec![WorktreeEvent {
                at_ms: 1,
                event: WorktreeEventKind::Conflict { resolved: false },
            }],
        )]
        .into();
        let ready = SourceSnapshot::test_live(Some(events), SourceState::Ready, Domain::History);
        let attached = attach(board.clone(), &ready);
        assert_eq!(attached.data().unwrap().row_history.len(), 1);
        let failed = ready.with_state(SourceState::Failed("panicked".into()));
        let untouched = attach(board, &failed);
        assert!(untouched.data().unwrap().row_history.is_empty());
        assert!(!untouched.domains().contains_key(&Domain::History));
        assert_eq!(*untouched.state(), SourceState::Ready);
    }

    /// The real local lanes: Git inventory, metadata, the removal ledger and
    /// its pane rows, then this module's leaf and overlay.
    #[tokio::test]
    async fn a_removed_rows_last_history_reaches_the_board_under_that_rows_key() {
        let fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        let context = &fixture.ctx;
        let removed_at = time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap();
        context
            .database
            .call(move |store| {
                // An earlier worktree used the slug too; only the last one
                // is the removed row's history.
                store.append_worktree_event(
                    "gone",
                    1,
                    &WorktreeEventKind::Created { base: None },
                )?;
                store.append_worktree_event(
                    "gone",
                    2,
                    &WorktreeEventKind::Removed { landed: false },
                )?;
                store.record_slug_created("gone", &removed_at)?;
                store.record_removed_worktrees(
                    &[wt_store::RemovedWorktree {
                        slug: "gone".into(),
                        branch: "feature/gone".into(),
                        removed_at: removed_at.clone(),
                        work: None,
                        automations_paused: None,
                        extra: Default::default(),
                    }],
                    wt_app_core::feeds::epoch_ms() as i64,
                )?;
                store.append_worktree_event(
                    "one",
                    wt_app_core::feeds::epoch_ms(),
                    &WorktreeEventKind::Conflict { resolved: false },
                )?;
                Ok(())
            })
            .await
            .unwrap();
        let scope = TaskScope::new();
        let local = crate::local_source::start(&scope, context);
        let removed =
            crate::history_source::start(&scope, context, local.metadata.view(), local.git.view());
        let board = crate::history_source::overlay(&scope, local.board.clone(), removed.snapshot);
        let history = start(&scope, context, local.git.clone(), local.metadata.clone());
        let board = overlay(&scope, board, history.view());
        board.refresh();
        let mut updates = board.subscribe();
        let seen = tokio::time::timeout(
            Duration::from_secs(10),
            updates.wait_for(|snapshot| {
                snapshot
                    .data()
                    .is_some_and(|board| board.row_history.len() == 2)
            }),
        )
        .await
        .expect("live and removed rows both carry history")
        .unwrap()
        .clone();
        let board = seen.data().unwrap();
        let removed_row = board
            .removed_history
            .rows
            .iter()
            .find(|row| row.slug == "gone")
            .expect("the removal is listed");
        let timeline: Vec<_> = board.row_history[&removed_row.key]
            .iter()
            .map(|event| &event.event)
            .collect();
        assert_eq!(
            timeline,
            [
                &WorktreeEventKind::Created { base: None },
                &WorktreeEventKind::Removed { landed: false }
            ],
            "its last incarnation, ending at the removal"
        );
        let live_row = board.rows.iter().find(|row| row.slug == "one").unwrap();
        assert_eq!(board.row_history[&live_row.key].len(), 1);
        scope.shutdown(Duration::from_secs(5)).await.unwrap();
        fixture.close().await.unwrap();
    }

    fn iso(ms: u64) -> String {
        time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000)
            .unwrap()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap()
    }

    async fn timed(context: &AppContext, slug: &'static str) -> Vec<WorktreeEvent> {
        context
            .database
            .call(move |store| Ok(store.read_worktree_events(slug, 500)?))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_status_written_by_an_older_build_is_recorded_once_with_its_time_and_author() {
        let fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        let context = &fixture.ctx;
        let now = wt_app_core::feeds::epoch_ms();
        let asserted = now - 3_600_000;
        old_build_writes(
            context,
            "one",
            serde_json::json!({
                "state": "ready", "at": iso(asserted), "by": "one", "risk": "low",
                "note": "all checks green\nsecond line"
            }),
        )
        .await;
        // Two hosts on the database see the same state at once.
        let first = observer(context);
        let second = observer(context);
        for host in [&first, &second] {
            host.board.publish(live_board(vec![row(None)]));
            host.read_state(context).await;
        }
        let recorded = wait_for_events(context, "one", |events| !events.is_empty()).await;
        assert_eq!(
            recorded,
            [WorktreeEventKind::Status {
                from: None,
                to: wt_core::WorkState::Ready,
                by: Some("one".into()),
                risk: Some(wt_core::WorkRisk::Low),
                note: Some("all checks green".into()),
                blocked_on: None,
                verify_in: None,
            }]
        );
        assert_eq!(
            timed(context, "one").await[0].at_ms,
            asserted,
            "dated when the status was asserted, not when wt noticed"
        );
        // Seen again, and by a restarted host: nothing new.
        first.read_state(context).await;
        let restarted = observer(context);
        restarted.board.publish(live_board(vec![row(None)]));
        restarted.read_state(context).await;
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(events(context, "one").await.len(), 1);

        // The older build clears it. Nothing says who or when.
        old_build_writes(context, "one", serde_json::Value::Null).await;
        for host in [&first, &second, &restarted] {
            host.read_state(context).await;
        }
        let cleared = wait_for_events(context, "one", |events| events.len() >= 2).await;
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(
            events(context, "one").await,
            cleared,
            "one event for one clear"
        );
        assert_eq!(
            cleared[1],
            WorktreeEventKind::StatusCleared {
                from: Some(wt_core::WorkState::Ready),
                by: None
            }
        );
        for host in [first, second, restarted] {
            host.scope.shutdown(Duration::from_secs(2)).await.unwrap();
        }
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn a_status_that_already_has_its_event_is_not_recorded_again() {
        let fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        let context = &fixture.ctx;
        let record = |state: &str| wt_store::WorkStatusRecord {
            state: state.into(),
            at: iso(wt_app_core::feeds::epoch_ms()),
            note: None,
            risk: None,
            sha: None,
            by: Some("one".into()),
            blocked_on: None,
            verify_after_merge: None,
            verify_in: None,
            extra: Default::default(),
        };
        let working = record("working");
        context
            .database
            .call(move |store| Ok(store.set_slug_work_status("one", Some(&working))?))
            .await
            .unwrap();
        let host = observer(context);
        host.board.publish(live_board(vec![row(None)]));
        host.read_state(context).await;
        // The host still holds that snapshot when this build's `wt status`
        // writes the next status together with its event. The stale snapshot
        // must not be compared with the newer log.
        let stale = context
            .database
            .call(|store| Ok(store.read_wt_state()?))
            .await
            .unwrap();
        let ready = record("ready");
        context
            .database
            .call(move |store| Ok(store.set_slug_work_status("one", Some(&ready))?))
            .await
            .unwrap();
        let mut renoted = stale;
        renoted["slugs"]["one"]["work"]["note"] = "a snapshot from before the write".into();
        host.metadata
            .publish_live((renoted, BTreeSet::new()), Stamp::now());
        tokio::time::sleep(Duration::from_millis(200)).await;
        host.read_state(context).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        let recorded = events(context, "one").await;
        assert_eq!(recorded.len(), 2, "{recorded:?}");
        assert!(matches!(
            recorded[1],
            WorktreeEventKind::Status {
                from: Some(wt_core::WorkState::Working),
                to: wt_core::WorkState::Ready,
                ..
            }
        ));
        host.scope.shutdown(Duration::from_secs(2)).await.unwrap();
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn an_observed_status_never_sorts_before_the_creation_or_the_history_it_follows() {
        let fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        let context = &fixture.ctx;
        let now = wt_app_core::feeds::epoch_ms();
        // The worktree was created a minute ago and already has a conflict
        // on record; the status claims an earlier time than both.
        let created = iso(now - 60_000);
        context
            .database
            .call(move |store| {
                store.record_slug_created("one", &created)?;
                store.append_worktree_event(
                    "one",
                    now - 30_000,
                    &WorktreeEventKind::Conflict { resolved: false },
                )?;
                Ok(())
            })
            .await
            .unwrap();
        old_build_writes(
            context,
            "one",
            serde_json::json!({"state": "working", "at": iso(now - 600_000)}),
        )
        .await;
        let host = observer(context);
        host.board.publish(live_board(vec![row(None)]));
        host.read_state(context).await;
        wait_for_events(context, "one", |events| events.len() == 3).await;
        let history = timed(context, "one").await;
        assert!(matches!(
            history[0].event,
            WorktreeEventKind::Created { .. }
        ));
        assert!(matches!(history[2].event, WorktreeEventKind::Status { .. }));
        assert_eq!(
            history[2].at_ms,
            now - 30_000,
            "the later of its own time and the newest event before it"
        );
        host.scope.shutdown(Duration::from_secs(2)).await.unwrap();
        fixture.close().await.unwrap();
    }
}
