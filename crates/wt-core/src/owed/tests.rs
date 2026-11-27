use serde_json::json;

use super::*;
use crate::{
    Check, StageDeploys, WorkState,
    watch::{Delay, WatchActions, parse},
};

/// 2026-11-24 12:00:00 UTC.
const NOON: i64 = 1_795_521_600_000;
const AT: &str = "2026-11-24T11:00:00.000Z";
const UTC: LocalClock = LocalClock {
    now_ms: NOON,
    utc_offset_minutes: 0,
};
const HOUR_MS: i64 = 3_600_000;
const BORN: &str = "2026-11-01T00:00:00Z";

fn wake(id: &str, conditions: &[&str], prompt: &str) -> Watch {
    Watch {
        id: id.into(),
        slug: "one".into(),
        incarnation: Some(BORN.into()),
        by: "one".into(),
        created_at_ms: NOON - HOUR_MS,
        utc_offset_minutes: 0,
        when: conditions
            .iter()
            .map(|arg| parse(arg, UTC).unwrap_or_else(|error| panic!("{error}")))
            .collect(),
        then: WatchActions {
            wake: Some(prompt.into()),
            ..WatchActions::default()
        },
        met_at_ms: None,
        last_error: None,
        owner_gone: false,
        extra: Map::new(),
    }
}

fn due_step(id: &str, what: &str, due_at_ms: i64) -> OwedRecord {
    let mut record =
        OwedRecord::from_wake(&wake(id, &["staging"], what), due_at_ms, false).unwrap();
    record.when_text = "on staging".into();
    record
}

fn ready() -> WorkStatusRecord {
    WorkStatusRecord::new(WorkState::Ready, AT)
}

fn brief(items: &[OwedItem]) -> Vec<String> {
    items
        .iter()
        .map(|item| {
            format!(
                "{} {} {} ({})",
                item.state.as_str(),
                item.kind.as_str(),
                item.headline(),
                item.when_words(UTC)
            )
        })
        .collect()
}

const STAGED: Landing = Landing {
    base: true,
    production: false,
    deploys: StageDeploys::UNPROBED,
};

#[test]
fn nothing_is_owed_by_a_row_with_no_status_watch_or_step() {
    assert_eq!(owed_items(None, &Landing::NONE, &[], &[], NOON), []);
    assert!(!owes_anything(None, &[], &[]));
    assert_eq!(owed_phase(&[], false), None);
    assert_eq!(owed_phase(&[], true), Some(OwedPhase::Done));
    assert!(owed_hazards(None, &[], &[], UTC).is_empty());
}

#[test]
fn the_list_is_ordered_due_then_gate_then_waiting_by_what_it_waits_for() {
    let mut record = ready()
        .with_check(VerifyStage::Staging, "open the page")
        .with_check(VerifyStage::Production, "watch the logs\nfor an hour");
    record.blocked_on = Some("the prod secret is missing".into());
    let mut gate = wake("gate22", &["pr:3806:production"], "merge it");
    gate.then.unblock = record.blocked_on.clone();
    let mut tells = wake("tell22", &["merged"], "");
    tells.then = WatchActions {
        notify: Some(String::new()),
        ..WatchActions::default()
    };
    let watches = [
        wake("time22", &["17:00"], "rerun the renewal check"),
        wake("soon22", &["13:00"], "look at the queue"),
        wake("prod22", &["production"], "retire the flag"),
        wake("else22", &["check:vault=X"], "rotate the key"),
        tells,
        gate,
    ];
    let due = [
        due_step("late22", "open the follow-up PR", NOON - HOUR_MS),
        due_step(
            "erly22",
            "confirm prod logs\nthen report",
            NOON - 2 * HOUR_MS,
        ),
    ];
    // Before the row's own work lands: nothing of a check is due, and the
    // gate still holds the merge.
    let items = owed_items(Some(&record), &Landing::NONE, &watches, &due, NOON);
    assert_eq!(
        brief(&items),
        [
            "due step confirm prod logs (due since 10:00)",
            "due step open the follow-up PR (due since 11:00)",
            "waiting gate the prod secret is missing (clears when #3806 in production)",
            "waiting check check: open the page (in staging)",
            "waiting check check: watch the logs (in production)",
            "waiting step retire the flag (when in production)",
            "waiting step rotate the key (when check vault=X)",
            "waiting step look at the queue (at 13:00)",
            "waiting step rerun the renewal check (at 17:00)",
        ]
    );
    assert_eq!(owed_phase(&items, false), None);
    assert_eq!(items[2].done_target(), None);
    assert_eq!(items[2].id.as_deref(), Some("gate22"));
    // The wake that follows the gate is the gate's item, not a step.
    assert_eq!(
        items
            .iter()
            .filter(|item| item.id.as_deref() == Some("gate22"))
            .count(),
        1
    );
    // Landed: the staging check is due, and the gate is no longer owed.
    // The wake set to follow it is the step that remains of it.
    let items = owed_items(Some(&record), &STAGED, &watches, &due, NOON);
    assert_eq!(
        brief(&items),
        [
            "due check check: open the page (due in staging)",
            "due step confirm prod logs (due since 10:00)",
            "due step open the follow-up PR (due since 11:00)",
            "waiting check check: watch the logs (in production)",
            "waiting step retire the flag (when in production)",
            "waiting step rotate the key (when check vault=X)",
            "waiting step merge it (when #3806 in production)",
            "waiting step look at the queue (at 13:00)",
            "waiting step rerun the renewal check (at 17:00)",
        ]
    );
    assert_eq!(owed_phase(&items, true), Some(OwedPhase::Due));
    // What wt done takes: a stage for a check, an id for a due step.
    assert_eq!(items[0].done_target().as_deref(), Some("staging"));
    assert_eq!(items[1].done_target().as_deref(), Some("erly22"));
    assert_eq!(items[4].done_target(), None);
    assert_eq!(items[4].stage, Some(VerifyStage::Production));
    assert_eq!(items[1].by.as_deref(), Some("one"));
    // A watch that only tells the person is not owed.
    assert!(
        items
            .iter()
            .all(|item| item.id.as_deref() != Some("tell22"))
    );
}

#[test]
fn a_gate_nobody_watches_waits_for_a_person_and_holds_no_cleanup() {
    let mut record = ready();
    record.blocked_on = Some("needs a decision".into());
    let items = owed_items(Some(&record), &Landing::NONE, &[], &[], NOON);
    assert_eq!(
        brief(&items),
        ["waiting gate needs a decision (until a person clears it)"]
    );
    assert_eq!(items[0].when, OwedWhen::Person);
    assert!(!owes_anything(Some(&record), &[], &[]));
    assert!(owed_hazards(Some(&record), &[], &[], UTC).is_empty());
    // Once the row's own work has landed the merge it held back has
    // happened: the gate is not owed, and the phase agrees with cleanup.
    let landed = owed_items(Some(&record), &STAGED, &[], &[], NOON);
    assert_eq!(landed, []);
    assert_eq!(owed_phase(&landed, true), Some(OwedPhase::Done));
    // A verified or dropped record has no gate and owes no check.
    record.state = WorkState::Verified;
    record.checks.set(VerifyStage::Staging, Check::new("stale"));
    record.checks_mirror = Some(VerifyStage::Staging);
    assert_eq!(owed_items(Some(&record), &STAGED, &[], &[], NOON), []);
}

#[test]
fn a_check_reads_by_where_it_stands() {
    let record = ready()
        .with_check(VerifyStage::Staging, "open the page")
        .with_check(VerifyStage::Production, "watch the logs");
    let words = |landing: Landing, record: &WorkStatusRecord, now_ms: i64| {
        owed_items(Some(record), &landing, &[], &[], now_ms)
            .iter()
            .map(|item| {
                item.when_words(LocalClock {
                    now_ms,
                    utc_offset_minutes: 0,
                })
            })
            .collect::<Vec<_>>()
    };
    // Not landed: owed, dormant.
    assert_eq!(
        words(Landing::NONE, &record, NOON),
        ["in staging", "in production"]
    );
    let probed = Landing::new(true, true, true).with_deploys(StageDeploys {
        staging: Some(false),
        production: Some(true),
    });
    assert_eq!(
        words(probed, &record, NOON),
        ["due in production", "awaiting staging deploy"]
    );
    let old = NOON + 3 * 24 * HOUR_MS;
    assert_eq!(
        words(Landing::new(true, true, true), &record, old),
        ["overdue in staging", "overdue in production"]
    );
    let mut later = record.clone();
    later.defer_check(VerifyStage::Staging, Some(NOON + 30 * HOUR_MS));
    let items = owed_items(Some(&later), &STAGED, &[], &[], NOON);
    // Staging has the change, so it waits for the time and sorts last.
    assert_eq!(
        brief(&items),
        [
            "waiting check check: watch the logs (in production)",
            "waiting check check: open the page (in staging, not before 18:00 tomorrow)",
        ]
    );
    assert_eq!(owed_phase(&items, true), Some(OwedPhase::Waiting));
    assert!(owes_anything(Some(&later), &[], &[]));
}

#[test]
fn cleanup_is_held_by_checks_pending_wakes_and_due_steps_alike() {
    let check = ready().with_check(VerifyStage::Production, "watch the logs");
    assert!(owes_anything(Some(&check), &[], &[]));
    let waking = [wake("time22", &["17:00"], "rerun the renewal check")];
    assert!(owes_anything(None, &waking, &[]));
    let due = [due_step("late22", "open the follow-up PR", NOON - HOUR_MS)];
    assert!(owes_anything(None, &[], &due));
    assert_eq!(
        owed_hazards(Some(&check), &waking, &due, UTC),
        [
            "still owes: open the follow-up PR (due since 11:00); check: watch the logs (in production); rerun the renewal check (at 17:00)"
        ]
    );
    let mut later = check.clone();
    later.defer_check(VerifyStage::Production, Some(NOON + 5 * HOUR_MS));
    assert_eq!(
        owed_hazards(Some(&later), &[], &[], UTC),
        ["still owes: check: watch the logs (in production, not before 17:00)"]
    );
    // A watch that only tells the person, or whose owner is gone, is not owed.
    let mut tells = wake("tell22", &["merged"], "");
    tells.then = WatchActions {
        notify: Some(String::new()),
        ..WatchActions::default()
    };
    let mut orphan = wake("gone22", &["17:00"], "nobody to wake");
    orphan.owner_gone = true;
    assert!(!owes_anything(None, &[tells.clone(), orphan.clone()], &[]));
    assert!(owed_hazards(None, &[tells, orphan], &[], UTC).is_empty());
    // A gate whose watch wakes afterwards is a wake still owed.
    let mut gated = ready();
    gated.blocked_on = Some("the release".into());
    let mut gate = wake("gate22", &["release:production"], "ship it");
    gate.then.unblock = gated.blocked_on.clone();
    assert!(owes_anything(
        Some(&gated),
        std::slice::from_ref(&gate),
        &[]
    ));
    // Removal would lose the wake, so the hazard names it; the gate holds
    // nothing. The text is the same before and after landing.
    assert_eq!(
        owed_hazards(Some(&gated), std::slice::from_ref(&gate), &[], UTC),
        ["still owes: ship it (when next production release)"]
    );
    // Before landing the wake is part of the gate's item. After it the gate
    // is gone from the list and the wake is the step that is left, so a
    // landed row is cleanable exactly when its list is empty.
    let before = owed_items(
        Some(&gated),
        &Landing::NONE,
        std::slice::from_ref(&gate),
        &[],
        NOON,
    );
    assert_eq!(
        brief(&before),
        ["waiting gate the release (clears when next production release)"]
    );
    let after = owed_items(
        Some(&gated),
        &STAGED,
        std::slice::from_ref(&gate),
        &[],
        NOON,
    );
    assert_eq!(
        brief(&after),
        ["waiting step ship it (when next production release)"]
    );
    assert_eq!(owed_phase(&after, true), Some(OwedPhase::Waiting));
    // A gate watch that wakes nobody holds nothing once the row landed.
    gate.then.wake = None;
    let after = owed_items(
        Some(&gated),
        &STAGED,
        std::slice::from_ref(&gate),
        &[],
        NOON,
    );
    assert_eq!(after, []);
    assert!(!owes_anything(Some(&gated), &[gate], &[]));
}

#[test]
fn only_a_wake_delivered_before_landing_closes_on_delivery() {
    assert!(OwnerLanding::Landed.owes());
    assert!(OwnerLanding::Unknown.owes());
    assert!(!OwnerLanding::NotLanded.owes());
}

#[test]
fn a_due_step_keeps_what_its_wake_said_and_keys_it_does_not_know() {
    let mut watch = wake("abc234", &["staging"], "confirm prod logs");
    watch.set_before_release(true);
    watch.met_at_ms = Some(NOON);
    let step = OwedRecord::from_wake(&watch, NOON + 5, true).unwrap();
    assert_eq!(
        serde_json::to_value(&step).unwrap(),
        json!({
            "id": "abc234", "slug": "one", "incarnation": BORN, "by": "one",
            "what": "confirm prod logs", "whenText": "on staging", "dueAtMs": NOON + 5,
            "utcOffsetMinutes": 0, "unconfirmed": true, "beforeRelease": true,
        })
    );
    let stored = json!({
        "id": "abc234", "slug": "one", "by": "one", "what": "x", "whenText": "merged",
        "dueAtMs": 1, "fromALaterBuild": {"kept": true},
    });
    let read: OwedRecord = serde_json::from_value(stored.clone()).unwrap();
    assert!(read.owned_by(None) && !read.owned_by(Some(BORN)));
    let mut written = serde_json::to_value(&read).unwrap();
    written.as_object_mut().unwrap().remove("utcOffsetMinutes");
    assert_eq!(written, stored);
    let mut notify_only = watch;
    notify_only.then.wake = None;
    assert_eq!(OwedRecord::from_wake(&notify_only, NOON, false), None);
    let rows = [step.clone(), due_step("other2", "x", NOON)];
    assert_eq!(row_due(&rows, "one", Some(BORN)).len(), 2);
    assert!(row_due(&rows, "one", Some("2026-11-20T00:00:00Z")).is_empty());
    assert!(row_due(&rows, "two", Some(BORN)).is_empty());
}

#[test]
fn an_item_and_a_landed_row_are_filed_by_what_they_wait_for() {
    const DAY_MS: i64 = 24 * HOUR_MS;
    let record = ready()
        .with_check(VerifyStage::Staging, "open the page")
        .with_check(VerifyStage::Production, "watch the logs");
    // Its stage holds, so its delay is running: a known time.
    let mut running = wake("run22", &["production+7d"], "retire the old path");
    running.when[0].first_held_at_ms = Some(NOON - DAY_MS);
    let mut commit = wake("comm22", &["merged"], "compare error rates");
    commit.when = vec![WatchCondition::new(
        Condition::Commit {
            oid: "0123456789abcdef".into(),
            stage: VerifyStage::Production,
        },
        Delay::parse("1h"),
    )];
    let watches = [
        wake("time22", &["17:00"], "rerun the renewal check"),
        running,
        wake("prod22", &["production"], "retire the flag"),
        wake("pull22", &["pr:3889:production"], "drop the shim"),
        commit,
        wake("rels22", &["release:staging"], "smoke the release"),
        wake("mrge22", &["pr:12:merged"], "rebase"),
        wake(
            "wtre22",
            &["wt:a-rather-long-worktree-slug:merged"],
            "rebase again",
        ),
        wake("prob22", &["check:vault=X"], "rotate the key"),
    ];
    let due = [due_step("late22", "open the follow-up PR", NOON - HOUR_MS)];
    let items = owed_items(Some(&record), &STAGED, &watches, &due, NOON);
    let filed = |id: &str| {
        let item = items
            .iter()
            .find(|item| item.id.as_deref() == Some(id))
            .unwrap_or_else(|| panic!("{id}"));
        (OwedBlock::of_item(item), item.next_at_ms)
    };
    assert_eq!(OwedBlock::of_item(&items[0]), OwedBlock::Due);
    assert_eq!(filed("late22"), (OwedBlock::Due, None));
    // A deploy that has not happened: the row's own stage, another pull
    // request's or commit's, the stage's next release.
    let production_check = items
        .iter()
        .find(|item| item.kind == OwedKind::Check && item.state == OwedState::Waiting)
        .unwrap();
    assert_eq!(
        OwedBlock::of_item(production_check),
        OwedBlock::WaitingProduction
    );
    for id in ["prod22", "pull22", "comm22"] {
        assert_eq!(filed(id), (OwedBlock::WaitingProduction, None), "{id}");
    }
    assert_eq!(filed("rels22"), (OwedBlock::WaitingStaging, None));
    // A known time: a time, or a delay running on a condition that holds.
    assert_eq!(
        filed("time22"),
        (OwedBlock::Scheduled, Some(NOON + 5 * HOUR_MS))
    );
    assert_eq!(
        filed("run22"),
        (OwedBlock::Scheduled, Some(NOON + 6 * DAY_MS))
    );
    for id in ["mrge22", "wtre22", "prob22"] {
        assert_eq!(filed(id), (OwedBlock::WaitingOther, None), "{id}");
    }
    // A check put off on a stage that has the change is a known time too.
    let mut put_off = ready();
    put_off.set_check(VerifyStage::Staging, "open the page");
    assert!(put_off.defer_check(VerifyStage::Staging, Some(NOON + HOUR_MS)));
    let deferred = owed_items(Some(&put_off), &STAGED, &[], &[], NOON);
    assert_eq!(OwedBlock::of_item(&deferred[0]), OwedBlock::Scheduled);
    assert_eq!(deferred[0].next_at_ms, Some(NOON + HOUR_MS));
    // A row is filed under the most urgent thing any open item waits on.
    let row = |ids: &[&str]| {
        let own: Vec<Watch> = watches
            .iter()
            .filter(|watch| ids.contains(&watch.id.as_str()))
            .cloned()
            .collect();
        let items = owed_items(None, &STAGED, &own, &[], NOON);
        OwedBlock::of_row(owed_phase(&items, true).unwrap(), &items)
    };
    assert_eq!(row(&[]), OwedBlock::Done);
    assert_eq!(row(&["time22"]), OwedBlock::Scheduled);
    assert_eq!(row(&["time22", "prob22"]), OwedBlock::Scheduled);
    assert_eq!(row(&["time22", "pull22"]), OwedBlock::WaitingProduction);
    assert_eq!(row(&["pull22", "rels22"]), OwedBlock::WaitingStaging);
    assert_eq!(row(&["mrge22"]), OwedBlock::WaitingOther);
    assert_eq!(OwedBlock::of_row(OwedPhase::Due, &items), OwedBlock::Due);
    // Every block has its own key and reads back from it.
    for block in OwedBlock::ALL {
        assert_eq!(OwedBlock::from_key(block.key()), Some(block));
        assert!(block.key().starts_with("\0owed:"));
    }
    assert_eq!(
        OwedBlock::ALL.map(OwedBlock::title),
        [
            "Due",
            "Waiting for staging",
            "Waiting for production",
            "Scheduled",
            "Waiting on other work",
            "Done"
        ]
    );
    // What a landed row waits for next, as the board and `wt clean` say it.
    let waits = |ids: &[&str]| {
        let own: Vec<Watch> = watches
            .iter()
            .filter(|watch| ids.contains(&watch.id.as_str()))
            .cloned()
            .collect();
        let items = owed_items(None, &STAGED, &own, &[], NOON);
        OwedBlock::row_waits_for(owed_phase(&items, true).unwrap(), &items, UTC)
    };
    assert_eq!(waits(&[]), "done");
    assert_eq!(waits(&["time22"]), "17:00");
    assert_eq!(waits(&["pull22", "rels22"]), "staging");
    assert_eq!(waits(&["time22", "pull22"]), "production");
    assert_eq!(waits(&["mrge22"]), "other work");
    assert_eq!(
        OwedBlock::row_waits_for(OwedPhase::Waiting, &deferred, UTC),
        "13:00"
    );
    let mut due_check = ready();
    due_check.set_check(VerifyStage::Staging, "open the page");
    let due_items = owed_items(Some(&due_check), &STAGED, &[], &[], NOON);
    assert_eq!(
        OwedBlock::row_waits_for(owed_phase(&due_items, true).unwrap(), &due_items, UTC),
        "due staging"
    );
    assert_eq!(OwedBlock::Scheduled.waits_for(None, None), "scheduled");
    // The fewest words that name each condition.
    let labels: Vec<String> = watches
        .iter()
        .map(|watch| watch.when[0].label(UTC))
        .collect();
    assert_eq!(
        labels,
        [
            "17:00",
            "production +7d",
            "production",
            "#3889 production",
            "0123456 production +1h",
            "staging release",
            "#12 merged",
            "a-rather-lo… merged",
            "check vault",
        ]
    );
}

#[test]
fn an_instant_and_a_countdown_read_in_the_fewest_words() {
    use crate::watch::{countdown, terse_instant};
    const DAY_MS: i64 = 24 * HOUR_MS;
    // NOON is a Tuesday.
    for (at_ms, expected) in [
        (NOON + 5 * HOUR_MS, "17:00"),
        (NOON - 2 * HOUR_MS, "10:00"),
        (NOON + 2 * DAY_MS + 6 * HOUR_MS + 12 * 60_000, "Thu 18:12"),
        (NOON - DAY_MS, "Mon 12:00"),
        (NOON + 6 * DAY_MS, "Mon 12:00"),
        (NOON + 8 * DAY_MS - 3 * HOUR_MS + 24 * 60_000, "Dec 2 09:24"),
    ] {
        assert_eq!(terse_instant(at_ms, UTC), expected);
    }
    // The reader's day, not UTC's.
    let tokyo = LocalClock {
        now_ms: NOON,
        utc_offset_minutes: 9 * 60,
    };
    assert_eq!(terse_instant(NOON + 4 * HOUR_MS, tokyo), "Wed 01:00");
    for (ms, expected) in [
        (6 * DAY_MS + 23 * HOUR_MS, "6d"),
        (DAY_MS + 21 * HOUR_MS + 5 * 60_000, "1d 21h"),
        (2 * DAY_MS, "2d"),
        (2 * HOUR_MS + 5 * 60_000, "2h 5m"),
        (3 * HOUR_MS, "3h"),
        (40 * 60_000, "40m"),
        (20_000, "under 1m"),
    ] {
        assert_eq!(countdown(ms), expected);
    }
}
