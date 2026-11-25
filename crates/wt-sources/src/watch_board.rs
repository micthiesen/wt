//! Each row's pending watches and everything it owes on the board: what the
//! worktree waits for, whether each condition holds yet, what wt will do
//! then, and the one owed list `wt_core::owed_items` derives from its
//! status, its watches and the steps that came due.
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
    OwedItem, OwedKind, OwedWhen, sanitize_terminal_text,
    watch::{Condition, LocalClock, Watch, WatchAction, WatchCondition},
};
use wt_runtime::{SourceHandle, SourceSnapshot, SourceState, TaskScope, derived_channel};
use wt_tui::{
    Board, BoardRow, OwedView, WatchActionKind, WatchConditionView, WatchTruth, WatchView,
};

use crate::local_source::Metadata;

/// How often waiting rows are prepared again when nothing else changes.
const REFRESH: Duration = Duration::from_secs(30);
/// Why conditions read unknown when this process runs no engine.
const ENGINE_OFF: &str = "automations are off (WT_AUTOMATIONS=off), so nothing evaluates it";

/// `separate_production` is whether `[branch] production` names a branch
/// other than base, as the work overlay reads a landing.
pub fn overlay(
    scope: &TaskScope,
    board: SourceHandle<Board>,
    metadata: SourceHandle<Metadata>,
    separate_production: bool,
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
            let (prepared, next) = attach(base, &state, clock, idle, separate_production);
            wake = next.map(|after| tokio::time::Instant::now() + after);
            publisher.publish_if_changed(prepared);
        }
    });
    output
}

/// Put each local worktree's watches, owed list and creation stamp on its
/// row. Returns the board and how long until it should be prepared again:
/// `None` while no row waits for anything.
fn attach(
    base: SourceSnapshot<Board>,
    metadata: &SourceSnapshot<Metadata>,
    clock: LocalClock,
    idle: Option<&str>,
    separate_production: bool,
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
    let due_steps = wt_store::stored_owed(state);
    let mut board = board.as_ref().clone();
    let mut next = None::<Duration>;
    let paused_everywhere = board.automations_paused;
    for row in board.rows.iter_mut().filter(|row| local_worktree(row)) {
        let created_at = state["slugs"][&row.slug]["createdAt"].as_str();
        let held = paused_everywhere || row.automations_paused;
        let own = wt_core::owed::row_watches(&watches, &row.slug, created_at);
        row.watches = own
            .iter()
            .map(|watch| WatchView {
                held,
                ..view(watch, clock, idle)
            })
            .collect();
        // The stored record, read as every other reader of the owed list
        // reads it: a gate is matched to its watch by its text.
        let record = wt_core::parse_work_status(&state["slugs"][&row.slug]["work"]);
        let landing = wt_core::Landing::new(
            row.git.landed_on.is_some(),
            row.git.landed_on == Some(wt_tui::LandingKind::Production),
            separate_production,
        )
        .with_deploys(row.git.deploys);
        let items = wt_core::owed_items(
            record.as_ref(),
            &landing,
            &own,
            &wt_core::owed::row_due(&due_steps, &row.slug, created_at),
            clock.now_ms,
        );
        row.owed_phase = wt_core::owed_phase(&items, landing.base);
        row.owes = items
            .iter()
            .map(|item| owed_view(item, &own, clock, idle, held))
            .collect();
        // A check put off to a time comes back when that time does.
        for at_ms in items.iter().filter_map(|item| match item.when {
            OwedWhen::Stage { not_before_ms, .. } => not_before_ms,
            _ => None,
        }) {
            if let Ok(until @ 1..) = u64::try_from(at_ms - clock.now_ms) {
                let until = Duration::from_millis(until).min(REFRESH);
                next = Some(next.map_or(until, |next| next.min(until)));
            }
        }
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
        held: false,
    }
}

/// One owed item as a row shows it. `watches` are the row's own: a gate's
/// watch and a waiting step's give the item its conditions.
pub(crate) fn owed_view(
    item: &OwedItem,
    watches: &[Watch],
    clock: LocalClock,
    idle: Option<&str>,
    paused: bool,
) -> OwedView {
    let watch = match item.when {
        OwedWhen::Conditions { .. } => item
            .id
            .as_deref()
            .and_then(|id| watches.iter().find(|watch| watch.id == id)),
        _ => None,
    };
    OwedView {
        id: item.id.as_deref().map(sanitize_terminal_text),
        kind: item.kind,
        state: item.state,
        stage: item.stage,
        what: sanitize_terminal_text(&item.what),
        headline: sanitize_terminal_text(&item.headline()),
        when: sanitize_terminal_text(&item.when_words(clock)),
        conditions: watch
            .map(|watch| {
                watch
                    .when
                    .iter()
                    .map(|cond| condition(watch, cond, clock, idle))
                    .collect()
            })
            .unwrap_or_default(),
        due_at_ms: item.due_at_ms.and_then(|at_ms| u64::try_from(at_ms).ok()),
        due_at: item
            .due_at_ms
            .map(|at_ms| sanitize_terminal_text(&wt_core::watch::describe_instant(at_ms, clock))),
        not_before_in: match item.when {
            OwedWhen::Stage {
                not_before_ms: Some(at_ms),
                ..
            } if at_ms > clock.now_ms => Some(remaining(at_ms - clock.now_ms)),
            _ => None,
        },
        by: item.by.as_deref().map(sanitize_terminal_text),
        before_release: item.before_release,
        overdue: item.overdue,
        awaiting_deploy: item.awaiting_deploy,
        unconfirmed: item.unconfirmed,
        met: watch.is_some_and(Watch::is_met),
        held: paused,
        wakes: item.kind == OwedKind::Gate && watch.is_some_and(Watch::wakes_owner),
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
        let (snapshot, next) = attach(board, &metadata, CLOCK, idle, false);
        assert!(snapshot.domains().contains_key(&wt_core::Domain::Metadata));
        (snapshot.data().unwrap().as_ref().clone(), next)
    }

    fn prepared_state(
        rows: Vec<BoardRow>,
        state: serde_json::Value,
        separate_production: bool,
    ) -> (Board, Option<Duration>) {
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
        let (snapshot, next) = attach(board, &metadata, CLOCK, None, separate_production);
        (snapshot.data().unwrap().as_ref().clone(), next)
    }

    fn landed(slug: &str, on: wt_tui::LandingKind) -> BoardRow {
        let mut row = row(slug);
        row.git.landed_on = Some(on);
        row
    }

    fn kinds(row: &BoardRow) -> Vec<(wt_core::OwedKind, wt_core::OwedState, &str)> {
        row.owes
            .iter()
            .map(|item| (item.kind, item.state, item.when.as_str()))
            .collect()
    }

    #[test]
    fn an_unlanded_row_carries_one_list_of_its_gate_checks_and_wakes() {
        use wt_core::{OwedKind, OwedState};
        let gate = watch(
            "gate22",
            "one",
            "one",
            &["#3806:production"],
            WatchActions {
                unblock: Some("prod secret".into()),
                wake: Some("Deploy it.".into()),
                ..Default::default()
            },
        );
        let timed = watch(
            "wake33",
            "one",
            "manager",
            &["+130m"],
            wake("Run it\nthen say so"),
        );
        let told = watch("tell44", "one", "human", &["merged"], notify());
        let state = json!({
            "slugs": { "one": {
                "createdAt": CREATED,
                "work": {
                    "state": "ready", "at": "2026-11-24T11:00:00Z",
                    "blockedOn": "prod secret",
                    "checks": {
                        "staging": { "steps": "Look. STEPS: 1. open /health" },
                        "production": { "steps": "check the dashboard" },
                    },
                },
            } },
            "watches": [gate, timed, told],
        });
        let (board, next) = prepared_state(vec![row("one")], state, true);
        assert_eq!(next, Some(REFRESH));
        let one = &board.rows[0];
        // Not landed: the asserted status says where the row stands.
        assert_eq!(one.owed_phase, None);
        assert_eq!(
            kinds(one),
            [
                (
                    OwedKind::Gate,
                    OwedState::Waiting,
                    "clears when #3806 in production"
                ),
                (OwedKind::Check, OwedState::Waiting, "once in staging"),
                (OwedKind::Check, OwedState::Waiting, "once in production"),
                (OwedKind::Step, OwedState::Waiting, "at 14:10"),
            ]
        );
        let [gate, staging, _, step] = one.owes.as_slice() else {
            panic!("{:?}", one.owes)
        };
        // The gate's watch is part of the gate: its wake is not a step of
        // its own.
        assert_eq!(gate.what, "prod secret");
        assert_eq!(gate.id.as_deref(), Some("gate22"));
        assert!(gate.wakes && !gate.met);
        assert_eq!(gate.conditions[0].text, "#3806 in production");
        assert_eq!(staging.stage, Some(wt_core::VerifyStage::Staging));
        assert_eq!(staging.headline, "check: Look. STEPS: 1. open /health");
        assert_eq!(step.what, "Run it then say so");
        assert_eq!(step.headline, "Run it");
        assert_eq!(step.by.as_deref(), Some("manager"));
        assert_eq!(step.conditions[0].due_in.as_deref(), Some("2h10m"));
        // What only tells the person is not owed; every watch stays listed
        // for the menus that need it.
        assert_eq!(one.watches.len(), 3);
        assert!(
            !one.owes
                .iter()
                .any(|item| item.id.as_deref() == Some("tell44"))
        );
    }

    #[test]
    fn a_landed_rows_phase_follows_what_is_due_waiting_or_done() {
        use wt_core::{OwedKind, OwedPhase, OwedState};
        let state = |deferred: bool, due: bool| {
            let mut state = json!({
                "slugs": { "one": {
                    "createdAt": CREATED,
                    "work": {
                        "state": "ready", "at": "2026-11-24T11:00:00Z",
                        "checks": {
                            "staging": { "steps": "open /health" },
                            "production": { "steps": "check the dashboard" },
                        },
                    },
                } },
                "owed": [
                    {
                        "id": "93co6q", "slug": "one", "incarnation": CREATED, "by": "one",
                        "what": "confirm prod logs", "whenText": "in production",
                        "dueAtMs": NOON - 24 * 60_000, "beforeRelease": true,
                    },
                    // Another worktree's, and an earlier worktree of this name.
                    { "id": "other1", "slug": "two", "incarnation": CREATED, "by": "two",
                      "what": "x", "whenText": "y", "dueAtMs": NOON },
                    { "id": "older1", "slug": "one", "incarnation": "2026-01-01T00:00:00Z",
                      "by": "one", "what": "x", "whenText": "y", "dueAtMs": NOON },
                ],
            });
            if deferred {
                state["slugs"]["one"]["work"]["checks"]["staging"]["notBeforeMs"] =
                    json!(NOON + 3_600_000);
            }
            if !due {
                state["owed"] = json!([]);
            }
            state
        };
        // Landed on base with a separate production branch: the step that
        // came due and the staging check are due, production still waits.
        let base = || landed("one", wt_tui::LandingKind::Base);
        let (board, next) = prepared_state(vec![base()], state(false, true), true);
        assert_eq!(next, None, "nothing here changes with time");
        let one = &board.rows[0];
        assert_eq!(one.owed_phase, Some(OwedPhase::Due));
        assert_eq!(
            kinds(one),
            [
                (OwedKind::Check, OwedState::Due, "due in staging"),
                (OwedKind::Step, OwedState::Due, "due since 11:36"),
                (OwedKind::Check, OwedState::Waiting, "once in production"),
            ]
        );
        let step = &one.owes[1];
        assert_eq!(step.id.as_deref(), Some("93co6q"));
        assert_eq!(step.due_at.as_deref(), Some("11:36"));
        assert_eq!(
            step.due_at_ms,
            Some(u64::try_from(NOON - 24 * 60_000).unwrap())
        );
        assert!(step.before_release);
        // Put off for an hour: waiting, counted down, and prepared again.
        let (board, next) = prepared_state(vec![base()], state(true, false), true);
        let one = &board.rows[0];
        assert_eq!(one.owed_phase, Some(OwedPhase::Waiting));
        // Its stage has the change, so only the time holds it back: it
        // sorts with what waits for a time.
        assert_eq!(
            kinds(one),
            [
                (OwedKind::Check, OwedState::Waiting, "once in production"),
                (
                    OwedKind::Check,
                    OwedState::Waiting,
                    "once in staging, not before 13:00"
                ),
            ]
        );
        assert_eq!(one.owes[1].not_before_in.as_deref(), Some("1h"));
        assert_eq!(next, Some(REFRESH));
        // A probed stage that is not running the change yet.
        let mut awaiting = base();
        awaiting.git.deploys.staging = Some(false);
        let (board, _) = prepared_state(vec![awaiting], state(false, false), true);
        let one = &board.rows[0];
        assert_eq!(one.owed_phase, Some(OwedPhase::Waiting));
        assert!(one.owes[0].awaiting_deploy && !one.owes[0].due());
        assert_eq!(one.owes[0].when, "awaiting staging deploy");
        // In production everything it owes is due.
        let (board, _) = prepared_state(
            vec![landed("one", wt_tui::LandingKind::Production)],
            state(false, false),
            true,
        );
        assert!(board.rows[0].owes.iter().all(OwedView::due));
        // Landed and nothing open: done, which is what cleanup removes.
        let (board, _) = prepared_state(
            vec![base()],
            json!({ "slugs": { "one": { "createdAt": CREATED } } }),
            true,
        );
        assert_eq!(board.rows[0].owed_phase, Some(OwedPhase::Done));
        assert!(board.rows[0].owes.is_empty());
        // A remote row's list is its host's to derive.
        let mut remote = base();
        remote.key = wt_core::remote_worktree_ledger_key("builder", "one");
        remote.host = Some("builder".into());
        let (board, _) = prepared_state(vec![remote], state(false, true), true);
        assert!(board.rows[0].owes.is_empty());
        assert_eq!(board.rows[0].owed_phase, None);
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
                    Some("exited 2: vault sealed")
                ),
            ]
        );
        assert_eq!(timed.by, "manager");
        // Terminal controls never reach the renderer.
        assert_eq!(timed.headline.as_deref(), Some("Run the smoke test now"));
        let due = &timed.conditions[0];
        assert_eq!(due.text, "14:10");
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
    fn a_watch_on_a_paused_row_is_marked_held() {
        let watches = [watch("tell44", "one", "human", &["merged"], notify())];
        let (board, _) = prepared(vec![row("one")], &watches, None);
        assert!(!board.rows[0].watches[0].held);
        let mut paused = row("one");
        paused.automations_paused = true;
        let (board, _) = prepared(vec![paused], &watches, None);
        assert!(board.rows[0].watches[0].held);
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
        let (snapshot, next) = attach(empty, &metadata, CLOCK, None, false);
        assert!(snapshot.data().is_none());
        assert_eq!(next, None);
    }
}
