//! One batched tracker reader for the fleet. Identity changes never borrow a
//! previous provider's or previous inventory's answer.
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Duration,
};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use wt_core::Domain;
use wt_runtime::{
    DiskCache, Fetched, LeafHandle, Persistence, RefreshPolicy, SourceHandle, SourceSnapshot,
    SourceSpec, Stamp, TaskScope, derived_channel, start_skippable_source,
};
use wt_tui::Board;

use wt_app_core::{context::AppContext, issue_identity, issue_tracker};

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
    let unavailable = Arc::new(Unavailable::default());
    let retry = unavailable.clone();
    let statuses = start_skippable_source(scope, statuses_spec(context), {
        let context = context.clone();
        let board = board.clone();
        let prefix = prefix.clone();
        move |cancel| {
            let context = context.clone();
            let command = command.clone();
            let unavailable = unavailable.clone();
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
                let statuses = read_with_fallback(
                    &context,
                    &command,
                    &ids,
                    READ_TIMEOUT,
                    &cancel,
                    &unavailable,
                )
                .await?;
                Ok(Some(Fetched::new(StatusBatch { statuses, ids }, observed)))
            }
        }
    });
    let output = project(scope, board, statuses.view(), prefix, enabled, retry);
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
    crate::sources::rows_confirmed(board).then(|| ids(board, prefix))
}

/// Bound on one run of the status command.
const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// How long an issue that could not be read is left alone the first time,
/// doubling with each failure in a row up to [`RETRY_CAP`]. An id the tracker
/// refuses (a deleted or inaccessible task) fails the same way on every pass,
/// and each try is a subprocess.
const RETRY_BASE: Duration = Duration::from_secs(5 * 60);
const RETRY_CAP: Duration = Duration::from_secs(60 * 60);

/// One issue the tracker could not read.
struct Failing {
    error: String,
    /// The wait before the next try.
    delay: Duration,
    retry_at: tokio::time::Instant,
    /// The longest wait this failure has been reported at.
    reported: Duration,
}

/// The issues the tracker could not read, and when each is tried again. A
/// failing issue is read on its own, off the batch it would fail, and at
/// most once per backoff step. It is reported once per step and once more
/// when it reads again, not once per pass.
#[derive(Default)]
struct Unavailable(std::sync::Mutex<BTreeMap<String, Failing>>);

impl Unavailable {
    fn seen(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Failing>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Record a failed read. Returns whether it is news: the issue's first
    /// failure, a different error, or a longer wait than any reported.
    fn failed(&self, id: &str, error: &str, now: tokio::time::Instant) -> bool {
        let mut seen = self.seen();
        // Each failure doubles the wait; the first doubles to the base.
        let failing = seen.entry(id.to_owned()).or_insert(Failing {
            error: String::new(),
            delay: RETRY_BASE / 2,
            retry_at: now,
            reported: Duration::ZERO,
        });
        failing.delay = failing.delay.saturating_mul(2).min(RETRY_CAP);
        failing.retry_at = now + failing.delay;
        let news = failing.error != error || failing.delay > failing.reported;
        failing.error = error.to_owned();
        failing.reported = failing.reported.max(failing.delay);
        news
    }

    /// Whether the issue had been failing until this read.
    fn read(&self, id: &str) -> bool {
        self.seen().remove(id).is_some()
    }

    /// Forget issues the board no longer shows.
    fn keep(&self, ids: &[String]) {
        self.seen().retain(|id, _| ids.contains(id));
    }

    /// A person asked for a refresh: try every failing issue on the next
    /// pass, and start its wait over. What was already reported is not
    /// reported again.
    fn retry_now(&self) {
        let now = tokio::time::Instant::now();
        for failing in self.seen().values_mut() {
            failing.retry_at = now;
            failing.delay = RETRY_BASE / 2;
        }
    }

    /// Split `ids` into those to read together, the failing ones due a
    /// try, and the error of one still waiting (if any is).
    fn plan(
        &self,
        ids: &[String],
        now: tokio::time::Instant,
    ) -> (Vec<String>, Vec<String>, Option<String>) {
        let seen = self.seen();
        let (mut healthy, mut due, mut waiting) = (Vec::new(), Vec::new(), None);
        for id in ids {
            match seen.get(id) {
                None => healthy.push(id.clone()),
                Some(failing) if failing.retry_at <= now => due.push(id.clone()),
                Some(failing) => waiting = waiting.or_else(|| Some(failing.error.clone())),
            }
        }
        (healthy, due, waiting)
    }

    fn note_read<'a>(&self, ids: impl IntoIterator<Item = &'a String>) {
        for id in ids {
            if self.read(id) {
                tracing::info!(issue = %id, "issue status available again");
            }
        }
    }
}

/// Read every status in as few runs as the tracker allows. A tracker may
/// refuse a whole batch over one id it cannot resolve; statuses are
/// display-only, so a refused batch is halved until the ids it cannot read
/// stand alone, and only their rows stay blank. Those ids are then left out
/// of the batch and tried on their own with a growing wait (see
/// [`Unavailable`]). A run that does not complete ends the pass.
async fn read_with_fallback(
    context: &AppContext,
    command: &[String],
    ids: &[String],
    timeout: Duration,
    cancel: &tokio_util::sync::CancellationToken,
    unavailable: &Unavailable,
) -> Result<BTreeMap<String, String>> {
    unavailable.keep(ids);
    let (healthy, due, waiting) = unavailable.plan(ids, tokio::time::Instant::now());
    let mut groups = due.into_iter().map(|id| vec![id]).collect::<Vec<_>>();
    if !healthy.is_empty() {
        groups.push(healthy);
    }
    let mut statuses = BTreeMap::new();
    let mut refused = None;
    while let Some(mut group) = groups.pop() {
        match read_statuses(context, command, &group, timeout, cancel).await {
            Ok(read) => {
                // A run that succeeded read every id it named, whether or
                // not the tracker has a status for each.
                unavailable.note_read(&group);
                statuses.extend(read);
            }
            Err(error) if cancel.is_cancelled() => return Err(refused.unwrap_or(error)),
            Err(error) if incomplete(&error) => return Err(error),
            Err(error) if group.len() == 1 => {
                let text = format!("{error:#}");
                if unavailable.failed(&group[0], &text, tokio::time::Instant::now()) {
                    tracing::warn!(issue = %group[0], error = %text, "issue status unavailable");
                }
                refused.get_or_insert(error);
            }
            Err(error) => {
                refused.get_or_insert(error);
                let rest = group.split_off(group.len() / 2);
                groups.extend([rest, group]);
            }
        }
    }
    if statuses.is_empty()
        && let Some(error) = refused.or_else(|| waiting.map(anyhow::Error::msg))
    {
        return Err(error);
    }
    Ok(statuses)
}

/// The status command did not run to completion (it could not start, hung
/// past its timeout, or its output was cut off). That says nothing about any
/// one id, so the pass fails as a whole instead of repeating it per id.
fn incomplete(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<wt_platform::process::ProcessError>()
        .is_some()
}

async fn read_statuses(
    context: &AppContext,
    command: &[String],
    ids: &[String],
    timeout: Duration,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<BTreeMap<String, String>> {
    let issues = issue_tracker::read(context, command, ids, timeout, cancel).await?;
    Ok(issues
        .into_iter()
        .map(|(id, issue)| (id, issue.status))
        .collect())
}

fn project(
    scope: &TaskScope,
    board: SourceHandle<Board>,
    statuses: SourceHandle<StatusBatch>,
    prefix: Option<String>,
    enabled: bool,
    unavailable: Arc<Unavailable>,
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
                    // An asked-for refresh reads what a quiet pass leaves alone.
                    unavailable.retry_now();
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
    #[tokio::test]
    async fn one_unresolvable_id_blanks_only_its_own_status() {
        let fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        let script = fixture.ctx.config.paths.main_clone.join("statuses.sh");
        // Refuses any request naming COZ-2, like a tracker that fails a
        // batch over one ambiguous task.
        std::fs::write(
            &script,
            "#!/bin/sh\nfor id in \"$@\"; do [ \"$id\" = COZ-2 ] && { echo 'not accessible: COZ-2' >&2; exit 1; }; done\nprintf '{\"issues\":['\nsep=''\nfor id in \"$@\"; do printf '%s{\"id\":\"%s\",\"status\":\"Done\"}' \"$sep\" \"$id\"; sep=','; done\nprintf ']}'\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let command = vec![script.to_string_lossy().into_owned(), "{ids}".into()];
        let ids = ["COZ-1".to_owned(), "COZ-2".into(), "COZ-3".into()];
        let cancel = tokio_util::sync::CancellationToken::new();
        let statuses = read_with_fallback(
            &fixture.ctx,
            &command,
            &ids,
            READ_TIMEOUT,
            &cancel,
            &Unavailable::default(),
        )
        .await
        .unwrap();
        assert_eq!(
            statuses.keys().map(String::as_str).collect::<Vec<_>>(),
            ["COZ-1", "COZ-3"]
        );
        // With nothing readable, the batch error stands.
        let only_bad = ["COZ-2".to_owned()];
        assert!(
            read_with_fallback(
                &fixture.ctx,
                &command,
                &only_bad,
                READ_TIMEOUT,
                &cancel,
                &Unavailable::default(),
            )
            .await
            .is_err()
        );
        fixture.close().await.unwrap();
    }

    /// A tracker that hangs is not one unresolvable id: the pass fails on
    /// the first run that does not complete instead of timing out once per id.
    #[tokio::test]
    async fn a_hung_tracker_fails_the_pass_after_one_run() {
        let fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        let script = fixture.ctx.config.paths.main_clone.join("hung.sh");
        let runs = fixture.ctx.config.paths.main_clone.join("runs");
        std::fs::write(
            &script,
            format!("#!/bin/sh\necho run >> {runs:?}\nexec sleep 30\n"),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let command = vec![script.to_string_lossy().into_owned(), "{ids}".into()];
        let ids = ["COZ-1".to_owned(), "COZ-2".into(), "COZ-3".into()];
        let cancel = tokio_util::sync::CancellationToken::new();
        let timeout = Duration::from_millis(300);
        assert!(
            read_with_fallback(
                &fixture.ctx,
                &command,
                &ids,
                timeout,
                &cancel,
                &Unavailable::default(),
            )
            .await
            .is_err()
        );
        assert_eq!(std::fs::read_to_string(&runs).unwrap().lines().count(), 1);
        fixture.close().await.unwrap();
    }

    /// An id the tracker refuses fails the same way on every pass. It is
    /// found once, then kept off the batch and left alone until its wait is
    /// over or a person refreshes, so a pass is one run again instead of
    /// one per issue.
    #[tokio::test]
    async fn a_refused_id_costs_one_search_and_then_no_runs_until_it_is_due() {
        let fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        let script = fixture.ctx.config.paths.main_clone.join("statuses.sh");
        let log = fixture.ctx.config.paths.main_clone.join("runs");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\necho \"$*\" >> {log:?}\nfor id in \"$@\"; do [ \"$id\" = COZ-2 ] && {{ echo 'not accessible: COZ-2' >&2; exit 1; }}; done\nprintf '{{\"issues\":['\nsep=''\nfor id in \"$@\"; do printf '%s{{\"id\":\"%s\",\"status\":\"Done\"}}' \"$sep\" \"$id\"; sep=','; done\nprintf ']}}'\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755))
            .unwrap();
        let command = vec![script.to_string_lossy().into_owned(), "{ids}".into()];
        let ids = (1..=8).map(|n| format!("COZ-{n}")).collect::<Vec<_>>();
        let cancel = tokio_util::sync::CancellationToken::new();
        let unavailable = Unavailable::default();
        let runs = || {
            let all = std::fs::read_to_string(&log).unwrap_or_default();
            std::fs::write(&log, "").unwrap();
            all.lines().map(str::to_owned).collect::<Vec<_>>()
        };
        let read = || {
            read_with_fallback(
                &fixture.ctx,
                &command,
                &ids,
                READ_TIMEOUT,
                &cancel,
                &unavailable,
            )
        };

        let first = read().await.unwrap();
        assert_eq!(first.len(), 7);
        assert!(!first.contains_key("COZ-2"));
        // Halving finds one bad id among eight in seven runs; one run per
        // id after the failed batch would be nine.
        assert_eq!(runs().len(), 7);

        // Every later pass: one batch without the refused id.
        for _ in 0..3 {
            assert_eq!(read().await.unwrap().len(), 7);
            assert_eq!(runs(), ["COZ-1 COZ-3 COZ-4 COZ-5 COZ-6 COZ-7 COZ-8"]);
        }

        // A refresh a person asked for tries it again, on its own.
        unavailable.retry_now();
        assert_eq!(read().await.unwrap().len(), 7);
        let mut asked = runs();
        asked.sort();
        assert_eq!(
            asked,
            ["COZ-1 COZ-3 COZ-4 COZ-5 COZ-6 COZ-7 COZ-8", "COZ-2"]
        );

        // With nothing else to read, its error stands without a run.
        let only_bad = ["COZ-2".to_owned()];
        let error = read_with_fallback(
            &fixture.ctx,
            &command,
            &only_bad,
            READ_TIMEOUT,
            &cancel,
            &unavailable,
        )
        .await
        .unwrap_err();
        assert!(format!("{error:#}").contains("COZ-2"), "{error:#}");
        assert!(runs().is_empty());
        fixture.close().await.unwrap();
    }

    #[test]
    fn a_failing_issue_waits_twice_as_long_each_time_and_is_reported_once_per_step() {
        let unavailable = Unavailable::default();
        let ids = ["A-1".to_owned(), "A-2".to_owned()];
        let mut now = tokio::time::Instant::now();
        let (mut reports, mut waits) = (0, Vec::new());
        for _ in 0..8 {
            reports += usize::from(unavailable.failed("A-2", "not accessible", now));
            let wait = unavailable.seen()["A-2"].delay;
            waits.push(wait.as_secs() / 60);
            // Until the wait is over it is neither batched nor tried.
            let (healthy, due, waiting) =
                unavailable.plan(&ids, now + wait - Duration::from_secs(1));
            assert_eq!(healthy, ["A-1"]);
            assert!(due.is_empty());
            assert_eq!(waiting.as_deref(), Some("not accessible"));
            now += wait;
            assert_eq!(unavailable.plan(&ids, now).1, ["A-2"]);
        }
        assert_eq!(waits, [5, 10, 20, 40, 60, 60, 60, 60]);
        assert_eq!(
            reports, 5,
            "one warning per step, none once the wait stops growing"
        );
        assert!(
            unavailable.failed("A-2", "gone", now),
            "a different error is news"
        );

        // An asked-for refresh makes it due at once and starts the wait
        // over, without reporting the same failure again.
        unavailable.retry_now();
        let now = tokio::time::Instant::now();
        assert_eq!(unavailable.plan(&ids, now).1, ["A-2"]);
        assert!(!unavailable.failed("A-2", "gone", now));
        assert_eq!(unavailable.seen()["A-2"].delay, RETRY_BASE);

        // A read clears it; a later failure is a new one.
        assert!(unavailable.read("A-2"));
        assert_eq!(unavailable.plan(&ids, now).0, ids);
        assert!(unavailable.failed("A-2", "gone", now));
        assert_eq!(unavailable.seen()["A-2"].delay, RETRY_BASE);
    }

    use super::*;
    use wt_runtime::SourceState;
    use wt_tui::BoardRow;

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
        let output = project(
            &scope,
            boards,
            statuses.view(),
            Some("ENG".into()),
            true,
            Arc::default(),
        );
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
        let output = project(
            &scope,
            boards,
            statuses.view(),
            Some("ENG".into()),
            true,
            Arc::default(),
        );
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
            let output = project(
                &scope,
                boards,
                statuses,
                Some("ENG".into()),
                enabled,
                Arc::default(),
            );
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

    #[test]
    fn an_unreadable_issue_is_news_once_per_error_and_once_when_it_reads_again() {
        let unavailable = Unavailable::default();
        let now = tokio::time::Instant::now();
        // Reported at each longer wait, then no more once it stops growing.
        let reports = |id: &str, error: &str| {
            (0..12)
                .filter(|_| unavailable.failed(id, error, now))
                .count()
        };
        assert_eq!(reports("COZ-1", "not found"), 5);
        assert_eq!(reports("COZ-1", "not found"), 0);
        assert_eq!(reports("COZ-2", "not found"), 5);
        assert_eq!(reports("COZ-1", "tracker is down"), 1);
        assert!(unavailable.read("COZ-1"));
        assert!(!unavailable.read("COZ-1"));
        assert_eq!(reports("COZ-1", "tracker is down"), 5);
        // An issue that left the board starts over if it returns.
        unavailable.keep(&["COZ-1".to_owned()]);
        assert_eq!(reports("COZ-2", "not found"), 5);
    }
}
