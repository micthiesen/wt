//! Each row's pending watches on the board: what the worktree waits for,
//! whether each condition holds yet, and what wt will do then.
//!
//! Nothing is read here. Watches live in the repository state, which the
//! metadata leaf already reads from SQLite (never persisted, refreshed by the
//! state database's file watcher), so a watch set by `wt wake` in an agent's
//! shell arrives the way a status does. This stage only prepares that state
//! for display, and prepares it again as time passes so countdowns and due
//! times stay current.
//!
//! A watch that outlived its worktree (a notification about a landing, kept
//! across the removal) has no row to sit on and is not shown; `wt watch
//! --all` lists it.

use std::{sync::Arc, time::Duration};

use wt_app_core::watch_view::{Shown, headline, shown};
use wt_core::{
    sanitize_terminal_text,
    watch::{Condition, LocalClock, Watch, WatchAction, WatchCondition},
};
use wt_runtime::{SourceHandle, SourceSnapshot, SourceState, TaskScope, derived_channel};
use wt_tui::{Board, BoardRow, WatchActionKind, WatchConditionView, WatchTruth, WatchView};

use crate::local_source::Metadata;

/// How often waiting rows are prepared again when nothing else changes.
const REFRESH: Duration = Duration::from_secs(30);
/// Why conditions read unknown when this process runs no engine.
const ENGINE_OFF: &str = "automations are off (WT_AUTOMATIONS=off), so nothing evaluates it";

pub fn overlay(
    scope: &TaskScope,
    board: SourceHandle<Board>,
    metadata: SourceHandle<Metadata>,
) -> SourceHandle<Board> {
    let (output, mut publisher) = derived_channel(scope, "watch-board");
    let cancellation = scope.token();
    let idle = (std::env::var("WT_AUTOMATIONS").as_deref() == Ok("off")).then_some(ENGINE_OFF);
    scope.spawn(async move {
        let mut boards = board.subscribe();
        let mut states = metadata.subscribe();
        boards.mark_changed();
        states.mark_changed();
        // Set only while a row waits: a quiet board is never woken by time.
        let mut wake = None::<tokio::time::Instant>;
        loop {
            let due = async {
                match wake {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => break,
                request = publisher.requested() => {
                    if request.is_none() { break; }
                    board.refresh();
                    metadata.refresh();
                    continue;
                }
                changed = boards.changed() => if changed.is_err() { break; },
                changed = states.changed() => if changed.is_err() { break; },
                () = due => {}
            }
            let base = boards.borrow_and_update().clone();
            let state = states.borrow_and_update().clone();
            let now_ms = wt_app_core::feeds::epoch_ms();
            let clock = LocalClock {
                now_ms: i64::try_from(now_ms).unwrap_or(i64::MAX),
                utc_offset_minutes: wt_app_core::display_time::local_utc_offset_minutes(now_ms)
                    .unwrap_or(0),
            };
            let (prepared, next) = attach(base, &state, clock, idle);
            wake = next.map(|after| tokio::time::Instant::now() + after);
            publisher.publish_if_changed(prepared);
        }
    });
    output
}

/// Put each local worktree's watches and creation stamp on its row. Returns
/// the board and how long until it should be prepared again: `None` while
/// no row waits for anything.
fn attach(
    base: SourceSnapshot<Board>,
    metadata: &SourceSnapshot<Metadata>,
    clock: LocalClock,
    idle: Option<&str>,
) -> (SourceSnapshot<Board>, Option<Duration>) {
    // A failed read keeps its last state; rows then show what was last known.
    let (Some(board), Some(state)) = (base.data(), metadata.data()) else {
        return (base, None);
    };
    if matches!(metadata.state(), SourceState::Failed(_)) && board.rows.is_empty() {
        return (base, None);
    }
    let state = &state.0;
    let watches = wt_store::stored_watches(state);
    let mut board = board.as_ref().clone();
    let mut next = None::<Duration>;
    for row in board.rows.iter_mut().filter(|row| local_worktree(row)) {
        let created_at = state["slugs"][&row.slug]["createdAt"].as_str();
        row.watches = watches
            .iter()
            .filter(|watch| {
                watch.slug == row.slug && !watch.owner_gone && watch.owned_by(created_at)
            })
            .map(|watch| view(watch, clock, idle))
            .collect();
        row.created_at = created_at.map(sanitize_terminal_text);
        for watch in watches.iter().filter(|watch| watch.slug == row.slug) {
            next = Some(next.map_or(REFRESH, |next| next.min(REFRESH)));
            // A time comes due between two passes: be there when it does.
            for at_ms in watch.when.iter().filter_map(|cond| instant(&cond.cond)) {
                if let Ok(until @ 1..) = u64::try_from(at_ms - clock.now_ms) {
                    let until = Duration::from_millis(until);
                    next = Some(next.map_or(until, |next| next.min(until)));
                }
            }
        }
    }
    (
        base.with_data(Some(Arc::new(board))).with_input(metadata),
        next,
    )
}

fn local_worktree(row: &BoardRow) -> bool {
    !row.is_main
        && !row.pending_creation
        && row.host.is_none()
        && !row.slug.is_empty()
        && !wt_core::is_remote_worktree_ledger_key(&row.key)
}

fn instant(cond: &Condition) -> Option<i64> {
    match cond {
        Condition::At { at_ms } => Some(*at_ms),
        _ => None,
    }
}

fn view(watch: &Watch, clock: LocalClock, idle: Option<&str>) -> WatchView {
    let actions: Vec<WatchActionKind> = watch
        .then
        .remaining()
        .map(|action| match action {
            WatchAction::Unblock => WatchActionKind::Unblock,
            WatchAction::Notify => WatchActionKind::Notify,
            WatchAction::Wake => WatchActionKind::Wake,
        })
        .collect();
    // What a reader wants beside the conditions: the prompt the session
    // gets, else what the person is told. The gate's own text is on its line.
    let text = watch
        .then
        .wake
        .as_deref()
        .or(watch.then.notify.as_deref())
        .map(|text| sanitize_terminal_text(&headline(text)))
        .filter(|text| !text.trim().is_empty());
    WatchView {
        id: sanitize_terminal_text(&watch.id),
        by: sanitize_terminal_text(&watch.by),
        conditions: watch
            .when
            .iter()
            .map(|cond| condition(watch, cond, clock, idle))
            .collect(),
        actions,
        headline: text,
        met: watch.is_met(),
    }
}

fn condition(
    watch: &Watch,
    cond: &WatchCondition,
    clock: LocalClock,
    idle: Option<&str>,
) -> WatchConditionView {
    let (state, reason) = match shown(watch, cond, clock.now_ms, idle) {
        Shown::Holds => (WatchTruth::Holds, None),
        Shown::NotYet(reason) => (WatchTruth::NotYet, reason),
        Shown::Unknown(reason) => (WatchTruth::Unknown, Some(reason)),
        // This process runs the engine: it judges the watch within moments,
        // and until then the honest reading is that nothing holds yet.
        Shown::NotEvaluated => (WatchTruth::NotYet, None),
    };
    let at_ms = instant(&cond.cond);
    WatchConditionView {
        key: sanitize_terminal_text(&cond.cond.to_string()),
        text: sanitize_terminal_text(&cond.describe(clock)),
        state,
        reason: reason.map(|reason| sanitize_terminal_text(&reason)),
        at_ms: at_ms.and_then(|at_ms| u64::try_from(at_ms).ok()),
        due_in: at_ms
            .filter(|at_ms| *at_ms > clock.now_ms)
            .map(|at_ms| remaining(at_ms - clock.now_ms)),
    }
}

/// A countdown in its two coarsest units, as the header prints rate-limit
/// resets: `1d13h`, `2h5m`, `5m`.
fn remaining(ms: i64) -> String {
    let seconds = ms / 1000;
    let (days, hours, minutes) = (
        seconds / 86_400,
        seconds % 86_400 / 3600,
        seconds % 3600 / 60,
    );
    match (days, hours, minutes) {
        (1.., 0, _) => format!("{days}d"),
        (1.., _, _) => format!("{days}d{hours}h"),
        (0, 1.., 0) => format!("{hours}h"),
        (0, 1.., _) => format!("{hours}h{minutes}m"),
        (0, 0, 0) => "under 1m".to_owned(),
        _ => format!("{minutes}m"),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use wt_core::watch::{WatchActions, parse};

    use super::*;

    /// 2026-11-24 12:00:00 UTC.
    const NOON: i64 = 1_795_521_600_000;
    const CREATED: &str = "2026-11-01T00:00:00Z";
    const CLOCK: LocalClock = LocalClock {
        now_ms: NOON,
        utc_offset_minutes: 0,
    };

    fn watch(id: &str, slug: &str, by: &str, conditions: &[&str], then: WatchActions) -> Watch {
        Watch {
            id: id.into(),
            slug: slug.into(),
            incarnation: Some(CREATED.into()),
            by: by.into(),
            created_at_ms: NOON - 60_000,
            utc_offset_minutes: 0,
            when: conditions
                .iter()
                .map(|arg| parse(arg, CLOCK).unwrap())
                .collect(),
            then,
            met_at_ms: None,
            last_error: None,
            owner_gone: false,
            extra: [("evaluatedAtMs".to_owned(), json!(NOON))]
                .into_iter()
                .collect(),
        }
    }

    fn wake(prompt: &str) -> WatchActions {
        WatchActions {
            wake: Some(prompt.into()),
            ..Default::default()
        }
    }

    fn notify() -> WatchActions {
        WatchActions {
            notify: Some(String::new()),
            ..Default::default()
        }
    }

    fn row(slug: &str) -> BoardRow {
        BoardRow {
            key: slug.into(),
            slug: slug.into(),
            ..Default::default()
        }
    }

    fn prepared(
        rows: Vec<BoardRow>,
        watches: &[Watch],
        idle: Option<&str>,
    ) -> (Board, Option<Duration>) {
        let state = json!({
            "slugs": { "one": { "createdAt": CREATED }, "two": { "createdAt": CREATED } },
            "watches": watches,
        });
        let board = SourceSnapshot::test_live(
            Some(Board {
                rows,
                ..Default::default()
            }),
            SourceState::Ready,
            wt_core::Domain::Git,
        );
        let metadata = SourceSnapshot::test_live(
            Some((state, Default::default())),
            SourceState::Ready,
            wt_core::Domain::Metadata,
        );
        let (snapshot, next) = attach(board, &metadata, CLOCK, idle);
        assert!(snapshot.domains().contains_key(&wt_core::Domain::Metadata));
        (snapshot.data().unwrap().as_ref().clone(), next)
    }

    #[test]
    fn a_row_carries_its_own_watches_with_each_conditions_state() {
        let mut gate = watch(
            "gate22",
            "one",
            "one",
            &["#3806:production", "check:secret=API_KEY"],
            WatchActions {
                unblock: Some("prod secret".into()),
                wake: Some("Deploy it.\nThen say so.".into()),
                ..Default::default()
            },
        );
        gate.when[0].first_held_at_ms = Some(NOON - 1);
        gate.last_error = Some("check secret exited 2: vault sealed".into());
        let timed = watch(
            "wake33",
            "one",
            "manager",
            &["+130m"],
            wake("Run the smoke test \u{1b}[31mnow"),
        );
        let others = watch("tell44", "two", "human", &["merged"], notify());
        let (board, next) = prepared(
            vec![row("one"), row("two"), row("three")],
            &[gate, timed, others],
            None,
        );
        assert_eq!(next, Some(REFRESH));
        let one = &board.rows[0];
        assert_eq!(one.created_at.as_deref(), Some(CREATED));
        let [gate, timed] = one.watches.as_slice() else {
            panic!("row one has two watches: {:?}", one.watches)
        };
        assert_eq!(gate.id, "gate22");
        assert_eq!(
            gate.actions,
            [WatchActionKind::Unblock, WatchActionKind::Wake]
        );
        assert_eq!(gate.headline.as_deref(), Some("Deploy it."));
        assert!(!gate.met);
        assert_eq!(
            gate.conditions
                .iter()
                .map(|cond| (cond.text.as_str(), cond.state, cond.reason.as_deref()))
                .collect::<Vec<_>>(),
            [
                ("#3806 in production", WatchTruth::Holds, None),
                (
                    "check secret=API_KEY",
                    WatchTruth::Unknown,
                    Some("check secret exited 2: vault sealed")
                ),
            ]
        );
        assert_eq!(timed.by, "manager");
        // Terminal controls never reach the renderer.
        assert_eq!(timed.headline.as_deref(), Some("Run the smoke test now"));
        let due = &timed.conditions[0];
        assert_eq!(due.text, "14:10 today");
        assert_eq!(due.at_ms, Some(u64::try_from(NOON).unwrap() + 130 * 60_000));
        assert_eq!(due.due_in.as_deref(), Some("2h10m"));
        assert_eq!(due.state, WatchTruth::NotYet);

        let two = &board.rows[1];
        assert_eq!(two.watches.len(), 1);
        assert!(two.watches[0].is_human_notify());
        assert_eq!(two.watches[0].conditions[0].key, "merged");
        assert_eq!(two.watches[0].headline, None);
        // No recorded creation, no watches: still a row a menu can witness.
        assert!(board.rows[2].watches.is_empty());
        assert_eq!(board.rows[2].created_at, None);
    }

    #[test]
    fn only_a_rows_own_incarnation_and_only_local_worktrees_show_watches() {
        let mut earlier = watch("old222", "one", "one", &["merged"], notify());
        earlier.incarnation = Some("2026-01-01T00:00:00Z".into());
        let mut outlived = watch("gone33", "one", "human", &["merged"], notify());
        outlived.owner_gone = true;
        let live = watch("live44", "one", "one", &["merged"], notify());
        let mut remote = row("one");
        remote.key = wt_core::remote_worktree_ledger_key("builder", "one");
        remote.host = Some("builder".into());
        let mut main = row("one");
        main.is_main = true;
        let (board, _) = prepared(
            vec![row("one"), remote, main],
            &[earlier, outlived, live],
            None,
        );
        let ids = |row: &BoardRow| {
            row.watches
                .iter()
                .map(|watch| watch.id.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(&board.rows[0]), ["live44"]);
        assert!(board.rows[1].watches.is_empty());
        assert!(board.rows[2].watches.is_empty());
        assert_eq!(board.rows[1].created_at, None);
    }

    #[test]
    fn time_passing_is_followed_only_while_something_waits() {
        let (_, next) = prepared(vec![row("one")], &[], None);
        assert_eq!(next, None, "a quiet board is never prepared on a timer");
        // Due in ten seconds: prepared again exactly then, not 30 s later.
        let soon = watch("soon22", "one", "one", &["+10s"], wake("go"));
        let (board, next) = prepared(vec![row("one")], std::slice::from_ref(&soon), None);
        assert_eq!(next, Some(Duration::from_secs(10)));
        assert_eq!(
            board.rows[0].watches[0].conditions[0].due_in.as_deref(),
            Some("under 1m")
        );
        // Past due and not yet marked met: holds, and the cadence resumes.
        let mut due = soon;
        due.when[0].cond = Condition::At { at_ms: NOON - 1 };
        let (board, next) = prepared(vec![row("one")], &[due.clone()], None);
        assert_eq!(next, Some(REFRESH));
        let cond = &board.rows[0].watches[0].conditions[0];
        assert_eq!(
            (cond.state, cond.due_in.as_deref()),
            (WatchTruth::Holds, None)
        );
        due.met_at_ms = Some(NOON);
        let (board, _) = prepared(vec![row("one")], &[due], None);
        assert!(board.rows[0].watches[0].met);
    }

    #[test]
    fn nothing_recorded_reads_not_yet_under_an_engine_and_unknown_without_one() {
        let mut fresh = watch("new222", "one", "one", &["green"], notify());
        fresh.extra.clear();
        let state = |idle| {
            let (board, _) = prepared(vec![row("one")], &[fresh.clone()], idle);
            let cond = board.rows[0].watches[0].conditions[0].clone();
            (cond.state, cond.reason)
        };
        assert_eq!(state(None), (WatchTruth::NotYet, None));
        assert_eq!(
            state(Some(ENGINE_OFF)),
            (WatchTruth::Unknown, Some(ENGINE_OFF.to_owned()))
        );
    }

    #[test]
    fn countdowns_use_the_two_coarsest_units() {
        let minute = 60_000;
        for (ms, expected) in [
            (59_000, "under 1m"),
            (5 * minute, "5m"),
            (2 * 60 * minute, "2h"),
            (130 * minute, "2h10m"),
            (24 * 60 * minute, "1d"),
            (37 * 60 * minute + 5 * minute, "1d13h"),
        ] {
            assert_eq!(remaining(ms), expected);
        }
    }

    #[test]
    fn a_board_or_state_not_read_yet_passes_through() {
        let empty = SourceSnapshot::<Board>::default();
        let metadata = SourceSnapshot::<Metadata>::default();
        let (snapshot, next) = attach(empty, &metadata, CLOCK, None);
        assert!(snapshot.data().is_none());
        assert_eq!(next, None);
    }
}
