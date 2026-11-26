use std::collections::BTreeMap;

use serde_json::{Map, Value, json};
use tempfile::{TempDir, tempdir};
use wt_core::{
    Landing, OWED_PER_SLUG_MAX, OwedRecord, OwnerLanding, VerifyStage, WatchChange,
    WorktreeEventKind,
    watch::{LocalClock, WatchActions, parse},
};

use crate::{
    CheckTargets, ChecksClosed, NewWatch, RemovedChecksClosed, RemovedSettle, RemovedWorktree,
    RepositoryIdentity, Store, WakeCompletion, WorkStatusRecord,
};

/// 2026-11-24 12:00:00 UTC.
const NOON: i64 = 1_795_521_600_000;
const CREATED: &str = "2026-11-01T00:00:00Z";
const STAGING: VerifyStage = VerifyStage::Staging;
const PRODUCTION: VerifyStage = VerifyStage::Production;
const STAGED: Landing = Landing {
    base: true,
    production: false,
    deploys: wt_core::StageDeploys::UNPROBED,
};

fn store() -> (TempDir, Store) {
    let temp = tempdir().unwrap();
    let mut store = Store::open(
        temp.path().join("wt.sqlite"),
        RepositoryIdentity::new("repo", "/repo"),
    )
    .unwrap();
    store.record_slug_created("one", CREATED).unwrap();
    (temp, store)
}

fn status(state: &str) -> WorkStatusRecord {
    WorkStatusRecord {
        state: state.into(),
        at: "2026-11-24T12:00:00Z".into(),
        note: None,
        risk: None,
        sha: None,
        by: Some("one".into()),
        blocked_on: None,
        verify_after_merge: None,
        verify_in: None,
        extra: Map::new(),
    }
}

fn both() -> WorkStatusRecord {
    status("ready")
        .with_check(STAGING, "open the page")
        .with_check(PRODUCTION, "watch the logs")
}

fn raw(store: &Store) -> Value {
    serde_json::from_str(&store.read_repository_state_json().unwrap().unwrap()).unwrap()
}

fn work(store: &Store) -> Value {
    raw(store)["slugs"]["one"]["work"].clone()
}

/// Write the state as another build left it.
fn write_raw(store: &mut Store, edit: impl FnOnce(&mut Value)) {
    let mut state = raw(store);
    edit(&mut state);
    store
        .write_repository_state_json(&state.to_string())
        .unwrap();
}

/// A status write by a build from before per-stage checks: it writes the
/// fields it knows and carries every other key of the stored record over.
fn old_build_writes(store: &mut Store, legacy: Value) {
    const KNOWN: [&str; 9] = [
        "state",
        "at",
        "note",
        "risk",
        "sha",
        "by",
        "blockedOn",
        "verifyAfterMerge",
        "verifyIn",
    ];
    write_raw(store, |state| {
        let mut next = legacy;
        let stored = state["slugs"]["one"]["work"]
            .as_object()
            .cloned()
            .unwrap_or_default();
        for (key, value) in stored {
            if !KNOWN.contains(&key.as_str()) {
                next[key] = value;
            }
        }
        state["slugs"]["one"]["work"] = next;
    });
}

fn stored(store: &mut Store) -> WorkStatusRecord {
    let mut seen = None;
    store
        .update_slug_work_status("one", |previous| {
            seen = previous.cloned();
            Err::<Option<WorkStatusRecord>, ()>(())
        })
        .unwrap()
        .unwrap_err();
    seen.unwrap()
}

fn stages(checks: &[(VerifyStage, wt_core::Check)]) -> Vec<(VerifyStage, &str)> {
    checks
        .iter()
        .map(|(stage, check)| (*stage, check.steps.as_str()))
        .collect()
}

fn owed_events(store: &Store, slug: &str) -> Vec<String> {
    store
        .read_worktree_events(slug, 500)
        .unwrap()
        .into_iter()
        .filter_map(|event| wt_core::owed_sentence(&event.event))
        .collect()
}

fn wake(store: &mut Store, conditions: &[&str], prompt: &str, before_release: bool) -> String {
    let clock = LocalClock {
        now_ms: NOON,
        utc_offset_minutes: 0,
    };
    let new = NewWatch {
        slug: "one".into(),
        by: "one".into(),
        utc_offset_minutes: 0,
        when: conditions
            .iter()
            .map(|arg| parse(arg, clock).unwrap())
            .collect(),
        then: WatchActions {
            wake: Some(prompt.into()),
            ..WatchActions::default()
        },
        before_release,
    };
    store.add_watch(new, NOON).unwrap().unwrap().id
}

fn due(store: &mut Store, prompt: &str, at_ms: i64) -> OwedRecord {
    let id = wake(store, &["staging"], prompt, false);
    match store
        .complete_wake(&id, OwnerLanding::Landed, false, at_ms)
        .unwrap()
    {
        WakeCompletion::Due(step) => *step,
        other => panic!("{other:?}"),
    }
}

fn removal(landed: bool) -> RemovedWorktree {
    RemovedWorktree {
        slug: "one".into(),
        branch: "feature/one".into(),
        removed_at: "2026-11-24T12:00:01Z".into(),
        work: None,
        automations_paused: None,
        extra: [(
            "landedOnAtRemoval".to_owned(),
            json!(if landed { "base" } else { "none" }),
        )]
        .into_iter()
        .collect(),
    }
}

fn removed(store: &mut Store) -> RemovedWorktree {
    store.read_removed_worktrees().unwrap().remove(0)
}

#[test]
fn checks_are_stored_per_stage_and_mirrored_for_older_builds() {
    let (_temp, mut store) = store();
    store.set_slug_work_status("one", Some(&both())).unwrap();
    let work = work(&store);
    assert_eq!(
        work["checks"],
        json!({"staging": {"steps": "open the page"}, "production": {"steps": "watch the logs"}})
    );
    assert_eq!(work["checksMirror"], "staging");
    assert_eq!(work["verifyAfterMerge"], "open the page");
    assert_eq!(stored(&mut store), both());
    // A record with nothing owed still says so, and reads back as written.
    store
        .set_slug_work_status("one", Some(&status("working")))
        .unwrap();
    assert_eq!(self::work(&store)["checks"], json!({}));
    assert!(self::work(&store).get("checksMirror").is_none());
    assert!(self::work(&store).get("verifyAfterMerge").is_none());
    assert_eq!(stored(&mut store), status("working"));
}

#[test]
fn a_record_that_sets_only_the_legacy_fields_is_stored_with_that_check() {
    let (_temp, mut store) = store();
    let mut legacy = status("ready");
    legacy.verify_after_merge = Some("watch the logs".into());
    legacy.verify_in = Some("production".into());
    store.set_slug_work_status("one", Some(&legacy)).unwrap();
    assert_eq!(
        work(&store)["checks"],
        json!({"production": {"steps": "watch the logs"}})
    );
    assert_eq!(work(&store)["checksMirror"], "production");
    assert_eq!(
        stages(&stored(&mut store).open_checks()),
        [(PRODUCTION, "watch the logs")]
    );
    // The same claim again is not a write.
    assert!(!store.set_slug_work_status("one", Some(&legacy)).unwrap());
}

#[test]
fn an_older_build_discharging_its_check_leaves_the_other_stage_owed() {
    let (_temp, mut store) = store();
    store.set_slug_work_status("one", Some(&both())).unwrap();
    // It asserts a fresh status without the one check it knew.
    old_build_writes(
        &mut store,
        json!({"state": "review", "at": "t", "by": "one"}),
    );
    let read = stored(&mut store);
    assert_eq!(
        stages(&read.open_checks()),
        [(PRODUCTION, "watch the logs")]
    );
    assert_eq!(read.verify_after_merge.as_deref(), Some("watch the logs"));
    assert_eq!(read.verify_in.as_deref(), Some("production"));
    // The next write by this build stores what was read.
    let mut next = read.clone();
    next.note = Some("still going".into());
    store.set_slug_work_status("one", Some(&next)).unwrap();
    let work = work(&store);
    assert_eq!(
        work["checks"],
        json!({"production": {"steps": "watch the logs"}})
    );
    assert_eq!(work["checksMirror"], "production");
    assert_eq!(work["verifyAfterMerge"], "watch the logs");
    // The older build could not record which check it discharged; the
    // first write by this build does.
    assert_eq!(
        owed_events(&store, "one"),
        ["done: staging check: open the page"]
    );
    // That older build now sees the production check as its own and
    // discharges it too.
    old_build_writes(&mut store, json!({"state": "review", "at": "t"}));
    assert!(stored(&mut store).open_checks().is_empty());
    assert_eq!(owed_events(&store, "one").len(), 1);
    store
        .set_slug_work_status("one", Some(&status("ready")))
        .unwrap();
    assert_eq!(
        owed_events(&store, "one"),
        [
            "done: staging check: open the page",
            "done: production check: watch the logs",
        ]
    );
}

#[test]
fn an_older_build_verifying_or_dropping_discharges_every_check() {
    for state in ["verified", "dropped"] {
        let (_temp, mut store) = store();
        store.set_slug_work_status("one", Some(&both())).unwrap();
        old_build_writes(&mut store, json!({"state": state, "at": "t"}));
        let read = stored(&mut store);
        assert!(
            read.open_checks().is_empty() && read.all_checks().is_empty(),
            "{state}"
        );
        // A fresh status by this build does not bring them back from the
        // keys the older build carried over.
        store
            .set_slug_work_status("one", Some(&status("working")))
            .unwrap();
        assert_eq!(work(&store)["checks"], json!({}), "{state}");
        assert!(work(&store).get("checksMirror").is_none());
    }
}

#[test]
fn an_older_build_setting_a_check_again_sets_that_stage_alone() {
    let (_temp, mut store) = store();
    let mut record = both();
    record.defer_check(PRODUCTION, Some(NOON));
    store.set_slug_work_status("one", Some(&record)).unwrap();
    // Carrying its check forward with a new state changes no check.
    old_build_writes(
        &mut store,
        json!({"state": "needs-testing", "at": "t", "verifyAfterMerge": "open the page"}),
    );
    assert_eq!(stored(&mut store).all_checks(), record.all_checks());
    // New steps for production replace that stage's check, not staging's.
    old_build_writes(
        &mut store,
        json!({
            "state": "ready", "at": "t",
            "verifyAfterMerge": "tail the worker", "verifyIn": "production",
        }),
    );
    let read = stored(&mut store);
    assert_eq!(
        stages(&read.open_checks()),
        [(STAGING, "open the page"), (PRODUCTION, "tail the worker")]
    );
    assert_eq!(read.open_check(PRODUCTION).unwrap().not_before_ms, None);
    // What it reads next is the first open check again.
    assert_eq!(read.verify_after_merge.as_deref(), Some("open the page"));
    assert_eq!(read.verify_in.as_deref(), Some("staging"));
}

#[test]
fn this_build_dropping_a_check_is_not_undone_by_what_was_stored() {
    let (_temp, mut store) = store();
    store.set_slug_work_status("one", Some(&both())).unwrap();
    assert_eq!(
        store
            .drop_check("one", STAGING, Some("one"))
            .unwrap()
            .unwrap()
            .steps,
        "open the page"
    );
    assert_eq!(store.drop_check("one", STAGING, Some("one")).unwrap(), None);
    assert_eq!(
        work(&store)["checks"],
        json!({"production": {"steps": "watch the logs"}})
    );
    assert_eq!(work(&store)["verifyIn"], "production");
    // Clearing every check through a record this build wrote stays cleared.
    let mut cleared = stored(&mut store);
    cleared.clear_checks();
    store.set_slug_work_status("one", Some(&cleared)).unwrap();
    assert_eq!(work(&store)["checks"], json!({}));
    assert!(stored(&mut store).open_checks().is_empty());
    assert_eq!(
        owed_events(&store, "one"),
        ["dropped: staging check: open the page"]
    );
}

#[test]
fn verified_closes_what_is_due_and_only_the_last_check_changes_the_state() {
    let (_temp, mut store) = store();
    let mut verified = status("verified");
    verified.note = Some("saw the page".into());
    let rule = |landing| CheckTargets::Verified {
        landing,
        now_ms: NOON + 1,
    };
    assert_eq!(
        store
            .close_checks("one", &rule(STAGED), &verified, None)
            .unwrap(),
        ChecksClosed::Missing
    );
    store.set_slug_work_status("one", Some(&both())).unwrap();
    let ChecksClosed::Closed {
        closed,
        remaining,
        record,
        ..
    } = store
        .close_checks("one", &rule(STAGED), &verified, Some("saw the page"))
        .unwrap()
    else {
        panic!("staging was due");
    };
    assert_eq!(stages(&closed), [(STAGING, "open the page")]);
    assert_eq!(stages(&remaining), [(PRODUCTION, "watch the logs")]);
    // The state, note and time are left; the mirror moves on.
    assert_eq!(
        (record.state.as_str(), record.note.as_deref()),
        ("ready", None)
    );
    assert_eq!(work(&store)["state"], "ready");
    assert_eq!(work(&store)["verifyAfterMerge"], "watch the logs");
    // Nothing is due now, so the earliest open check closes: the last one.
    let ChecksClosed::Closed {
        remaining, record, ..
    } = store
        .close_checks("one", &rule(STAGED), &verified, Some("logs are clean"))
        .unwrap()
    else {
        panic!("production was owed");
    };
    assert!(remaining.is_empty());
    assert_eq!(*record, verified);
    assert_eq!(work(&store)["state"], "verified");
    assert_eq!(work(&store)["checks"], json!({}));
    assert_eq!(
        store
            .close_checks("one", &rule(STAGED), &verified, None)
            .unwrap(),
        ChecksClosed::NotOwed
    );
    assert_eq!(
        owed_events(&store, "one"),
        [
            "done: staging check: open the page: saw the page",
            "done: production check: watch the logs: logs are clean",
        ]
    );
    // Both due at once close together; a named stage closes alone.
    store.set_slug_work_status("one", Some(&both())).unwrap();
    let ChecksClosed::Closed { closed, .. } = store
        .close_checks(
            "one",
            &CheckTargets::Stages(vec![PRODUCTION]),
            &verified,
            None,
        )
        .unwrap()
    else {
        panic!("production was owed");
    };
    assert_eq!(stages(&closed), [(PRODUCTION, "watch the logs")]);
    assert_eq!(
        store
            .close_checks(
                "one",
                &CheckTargets::Stages(vec![PRODUCTION]),
                &verified,
                None
            )
            .unwrap(),
        ChecksClosed::NotOwed
    );
    store.set_slug_work_status("one", Some(&both())).unwrap();
    let promoted = Landing::new(true, true, true);
    let ChecksClosed::Closed { closed, record, .. } = store
        .close_checks("one", &rule(promoted), &verified, None)
        .unwrap()
    else {
        panic!("both were due");
    };
    assert_eq!(closed.len(), 2);
    assert_eq!(record.state, "verified");
}

#[test]
fn a_check_put_off_stays_owed_and_its_history_says_until_when() {
    let (_temp, mut store) = store();
    store.set_slug_work_status("one", Some(&both())).unwrap();
    let later = NOON + 3 * 3_600_000;
    let check = store
        .defer_check("one", STAGING, later, "at 15:00", Some("one"))
        .unwrap()
        .unwrap();
    assert_eq!(check.not_before_ms, Some(later));
    assert_eq!(work(&store)["checks"]["staging"]["notBeforeMs"], later);
    let core = stored(&mut store).to_core().unwrap();
    assert!(!wt_core::verification_due_at(
        Some(&core),
        STAGED,
        later - 1
    ));
    assert!(wt_core::verification_due_at(Some(&core), STAGED, later));
    // Verified with nothing due closes the earliest open check all the same.
    assert_eq!(
        wt_core::checks_to_close(Some(&core), STAGED, NOON),
        [STAGING]
    );
    assert_eq!(
        store
            .defer_check("two", STAGING, later, "at 15:00", None)
            .unwrap(),
        None
    );
    assert_eq!(
        owed_events(&store, "one"),
        ["again at 15:00: staging check: open the page"]
    );
}

#[test]
fn removal_keeps_the_checks_beside_the_status_where_an_older_settle_leaves_them() {
    let (_temp, mut store) = store();
    store.set_slug_work_status("one", Some(&both())).unwrap();
    store
        .record_removed_worktrees(&[removal(true)], NOON + 2_000)
        .unwrap();
    let entry = raw(&store)["removed"][0].clone();
    assert_eq!(
        entry["checks"],
        json!({"staging": {"steps": "open the page"}, "production": {"steps": "watch the logs"}})
    );
    assert_eq!(entry["checksMirror"], "staging");
    // The status keeps the mirror older builds read and no checks of its own.
    assert_eq!(entry["work"]["verifyAfterMerge"], "open the page");
    assert_eq!(entry["work"]["checks"], json!({}));
    assert!(entry["work"].get("checksMirror").is_none());
    let read = removed(&mut store);
    assert_eq!(
        stages(&read.open_checks()),
        [(STAGING, "open the page"), (PRODUCTION, "watch the logs")]
    );
    assert!(crate::verification_owed_at_removal(&read) && read.owes_anything());
    // Recording the removal again changes nothing, and a caller's copy of
    // the record cannot put back what it read earlier.
    store
        .record_removed_worktrees(&[removal(true)], NOON + 3_000)
        .unwrap();
    assert_eq!(raw(&store)["removed"][0]["checks"], entry["checks"]);
    let mut stale = removal(true);
    stale.extra.insert("checks".into(), json!({}));
    stale.extra.insert("owed".into(), json!([{"id": "stale1"}]));
    store
        .record_removed_worktrees(&[stale], NOON + 4_000)
        .unwrap();
    assert_eq!(raw(&store)["removed"][0]["checks"], entry["checks"]);
    assert!(raw(&store)["removed"][0].get("owed").is_none());

    // Closing one stage leaves the row's state and the other stage.
    let mut verified = status("verified");
    verified.note = Some("saw it".into());
    let closed = store
        .close_removed_checks(
            "one",
            &CheckTargets::Stages(vec![STAGING]),
            &verified,
            Some("saw it"),
        )
        .unwrap();
    let RemovedChecksClosed::Closed { closed, remaining } = closed else {
        panic!("staging was owed");
    };
    assert_eq!(stages(&closed), [(STAGING, "open the page")]);
    assert_eq!(stages(&remaining), [(PRODUCTION, "watch the logs")]);
    let entry = raw(&store)["removed"][0].clone();
    assert_eq!(entry["work"]["state"], "ready");
    assert_eq!(entry["work"]["verifyAfterMerge"], "watch the logs");
    assert_eq!(entry["checksMirror"], "production");
    // A restore gets the status with what is still owed attached.
    let restored = removed(&mut store).work_with_checks().unwrap();
    assert_eq!(
        stages(&restored.open_checks()),
        [(PRODUCTION, "watch the logs")]
    );
    assert_eq!(restored.state, "ready");

    // An older build settles the row: it replaces the status whole, with
    // none of the keys it does not know. Every check is discharged.
    write_raw(&mut store, |state| {
        state["removed"][0]["work"] = json!({"state": "verified", "at": "t", "note": "done"});
    });
    let read = removed(&mut store);
    assert!(read.open_checks().is_empty() && !read.owes_anything());
    assert_eq!(
        store
            .close_removed_checks("one", &CheckTargets::All, &verified, None)
            .unwrap(),
        RemovedChecksClosed::NotOwed
    );
    assert_eq!(
        store
            .close_removed_checks("two", &CheckTargets::All, &verified, None)
            .unwrap(),
        RemovedChecksClosed::Missing
    );
    assert_eq!(
        owed_events(&store, "one"),
        ["done: staging check: open the page: saw it"]
    );
}

#[test]
fn recording_a_removal_again_brings_back_no_check_closed_since() {
    let (_temp, mut store) = store();
    store.set_slug_work_status("one", Some(&both())).unwrap();
    store
        .record_removed_worktrees(&[removal(true)], NOON + 2_000)
        .unwrap();
    let verified = status("verified");
    // Closed on the record: the live slug's status still names it.
    store
        .close_removed_checks(
            "one",
            &CheckTargets::Stages(vec![STAGING]),
            &verified,
            Some("saw it"),
        )
        .unwrap();
    assert_eq!(work(&store)["verifyAfterMerge"], "open the page");
    store
        .record_removed_worktrees(&[removal(true)], NOON + 3_000)
        .unwrap();
    assert_eq!(
        stages(&removed(&mut store).open_checks()),
        [(PRODUCTION, "watch the logs")]
    );
    assert_eq!(
        raw(&store)["removed"][0]["work"]["verifyAfterMerge"],
        "watch the logs"
    );
    // Closed on the live row: the record still names it.
    let closed = store
        .close_checks(
            "one",
            &CheckTargets::Stages(vec![PRODUCTION]),
            &verified,
            None,
        )
        .unwrap();
    assert!(matches!(closed, ChecksClosed::Closed { .. }), "{closed:?}");
    store
        .record_removed_worktrees(&[removal(true)], NOON + 4_000)
        .unwrap();
    assert!(!removed(&mut store).owes_anything());
    // A status asserted after the recording is a fresh claim: it replaces
    // what was recorded.
    let mut fresh = status("ready").with_check(PRODUCTION, "rerun the export");
    fresh.at = "2026-11-24T12:00:05Z".into();
    store.set_slug_work_status("one", Some(&fresh)).unwrap();
    store
        .record_removed_worktrees(&[removal(true)], NOON + 6_000)
        .unwrap();
    assert_eq!(
        stages(&removed(&mut store).open_checks()),
        [(PRODUCTION, "rerun the export")]
    );
}

#[test]
fn a_removed_row_settles_stage_by_stage_through_the_one_outcome_call() {
    let (_temp, mut store) = store();
    store.set_slug_work_status("one", Some(&both())).unwrap();
    store
        .record_removed_worktrees(&[removal(true)], NOON + 2_000)
        .unwrap();
    let verified = status("verified");
    assert_eq!(
        store.settle_removed_verification("one", &verified).unwrap(),
        RemovedSettle::Settled {
            verify_after_merge: Some("open the page".into())
        }
    );
    assert_eq!(raw(&store)["removed"][0]["work"]["state"], "ready");
    assert_eq!(
        store.settle_removed_verification("one", &verified).unwrap(),
        RemovedSettle::Settled {
            verify_after_merge: Some("watch the logs".into())
        }
    );
    let entry = raw(&store)["removed"][0].clone();
    assert_eq!(entry["work"]["state"], "verified");
    assert_eq!(entry["checks"], json!({}));
    assert_eq!(
        store.settle_removed_verification("one", &verified).unwrap(),
        RemovedSettle::NotOwed
    );
    // Dropping discharges every stage at once.
    let (_temp, mut store) = self::store();
    store.set_slug_work_status("one", Some(&both())).unwrap();
    store
        .record_removed_worktrees(&[removal(true)], NOON + 2_000)
        .unwrap();
    store
        .settle_removed_verification("one", &status("dropped"))
        .unwrap();
    assert!(removed(&mut store).open_checks().is_empty());
    assert_eq!(raw(&store)["removed"][0]["work"]["state"], "dropped");
    assert_eq!(
        owed_events(&store, "one"),
        [
            "dropped: staging check: open the page",
            "dropped: production check: watch the logs",
        ]
    );
}

#[test]
fn removal_records_from_other_builds_read_as_what_they_owe() {
    let (_temp, mut store) = store();
    write_raw(&mut store, |state| {
        state["removed"] = json!([
            // Written before per-stage checks existed.
            {"slug": "old", "branch": "b", "removedAt": "2026-11-24T12:00:01Z",
             "work": {"state": "ready", "at": "t", "verifyAfterMerge": "x", "verifyIn": "production"}},
            // Removed by an older build after this one set two checks: it
            // copied the status as it stood, checks inside.
            {"slug": "copied", "branch": "b", "removedAt": "2026-11-24T12:00:01Z",
             "work": serde_json::to_value(both()).unwrap()},
            {"slug": "bare", "branch": "b", "removedAt": "2026-11-24T12:00:01Z"},
            {"slug": "settled", "branch": "b", "removedAt": "2026-11-24T12:00:01Z",
             "work": {"state": "verified", "at": "t", "verifyAfterMerge": "x"}},
        ]);
    });
    let rows = store.read_removed_worktrees().unwrap();
    let owed = |slug: &str| {
        rows.iter()
            .find(|row| row.slug == slug)
            .unwrap()
            .open_checks()
    };
    assert_eq!(stages(&owed("old")), [(PRODUCTION, "x")]);
    assert_eq!(
        stages(&owed("copied")),
        [(STAGING, "open the page"), (PRODUCTION, "watch the logs")]
    );
    assert!(owed("bare").is_empty() && owed("settled").is_empty());
    // Closing one on the copied record moves the rest beside the status.
    let closed = store
        .close_removed_checks("copied", &CheckTargets::Earliest, &status("verified"), None)
        .unwrap();
    assert!(matches!(closed, RemovedChecksClosed::Closed { .. }));
    let entry = raw(&store)["removed"][1].clone();
    assert_eq!(
        entry["checks"],
        json!({"production": {"steps": "watch the logs"}})
    );
    assert_eq!(entry["work"]["checks"], json!({}));
    assert_eq!(entry["work"]["verifyAfterMerge"], "watch the logs");
}

#[test]
fn a_wake_delivered_after_landing_becomes_a_due_step_until_it_is_closed() {
    let (_temp, mut store) = store();
    let landed = wake(
        &mut store,
        &["staging"],
        "confirm prod logs\nthen report",
        true,
    );
    let unknown = wake(&mut store, &["17:00"], "rerun the renewal check", false);
    let early = wake(&mut store, &["pr:7"], "rebase onto it", false);
    let step = match store
        .complete_wake(&landed, OwnerLanding::Landed, false, NOON + 5)
        .unwrap()
    {
        WakeCompletion::Due(step) => *step,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        (
            step.id.as_str(),
            step.what.as_str(),
            step.when_text.as_str(),
            step.due_at_ms
        ),
        (
            landed.as_str(),
            "confirm prod logs\nthen report",
            "on staging",
            NOON + 5
        )
    );
    assert!(step.before_release && !step.unconfirmed);
    assert_eq!(step.incarnation.as_deref(), Some(CREATED));
    // Unknown landing counts as landed; an unconfirmed wake says so.
    let WakeCompletion::Due(unsure) = store
        .complete_wake(&unknown, OwnerLanding::Unknown, true, NOON + 6)
        .unwrap()
    else {
        panic!("unknown landing must be closed by hand");
    };
    assert!(unsure.unconfirmed);
    // Before landing a wake is coordination: delivery closes it.
    assert_eq!(
        store
            .complete_wake(&early, OwnerLanding::NotLanded, false, NOON + 7)
            .unwrap(),
        WakeCompletion::Closed
    );
    assert_eq!(store.list_watches(None).unwrap(), []);
    assert_eq!(
        store.list_owed(Some("one")).unwrap(),
        [step.clone(), *unsure.clone()]
    );
    assert!(store.list_owed(Some("two")).unwrap().is_empty());
    assert_eq!(raw(&store)["owed"][0]["whenText"], "on staging");
    // A second completion finds nothing owed.
    assert_eq!(
        store
            .complete_wake(&landed, OwnerLanding::Landed, false, NOON + 9)
            .unwrap(),
        WakeCompletion::NotOwed
    );
    // Due steps are recorded as due, not as woken; the early one as woken.
    let woken: Vec<WatchChange> = store
        .read_worktree_events("one", 500)
        .unwrap()
        .into_iter()
        .filter_map(|event| match event.event {
            WorktreeEventKind::Watch { change, .. } if change.is_action() => Some(change),
            _ => None,
        })
        .collect();
    assert_eq!(woken, [WatchChange::Woken]);
    assert_eq!(
        owed_events(&store, "one"),
        [
            format!("due: confirm prod logs (wt done {landed})"),
            format!(
                "due: rerun the renewal check (wt done {unknown}); wake sent, delivery not confirmed"
            ),
        ]
    );

    let closed = store
        .close_owed("one", &landed, "logs clean for an hour", Some("one"))
        .unwrap()
        .unwrap();
    assert_eq!((closed.step, closed.removed), (step, false));
    assert_eq!(
        store.close_owed("one", &landed, "again", None).unwrap(),
        None
    );
    assert_eq!(
        store.close_owed("two", &unknown, "not mine", None).unwrap(),
        None
    );
    assert_eq!(store.list_owed(None).unwrap().len(), 1);
    assert_eq!(
        owed_events(&store, "one").last().unwrap(),
        "done: confirm prod logs: logs clean for an hour"
    );
}

#[test]
fn a_wake_with_more_to_do_keeps_its_watch_and_the_existing_calls_still_close_on_delivery() {
    let (_temp, mut store) = store();
    let clock = LocalClock {
        now_ms: NOON,
        utc_offset_minutes: 0,
    };
    let new = NewWatch {
        slug: "one".into(),
        by: "one".into(),
        utc_offset_minutes: 0,
        when: vec![parse("staging", clock).unwrap()],
        then: WatchActions {
            notify: Some(String::new()),
            wake: Some("check it".into()),
            ..WatchActions::default()
        },
        before_release: false,
    };
    let id = store.add_watch(new, NOON).unwrap().unwrap().id;
    assert!(matches!(
        store
            .complete_wake(&id, OwnerLanding::Landed, false, NOON)
            .unwrap(),
        WakeCompletion::Due(_)
    ));
    let left = store.list_watches(None).unwrap();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].then.wake, None);
    let plain = wake(&mut store, &["staging"], "old path", false);
    assert!(
        store
            .complete_action(&plain, wt_core::watch::WatchAction::Wake)
            .unwrap()
    );
    let unsure = wake(&mut store, &["staging"], "old unsure path", false);
    let deliver = |store: &mut Store| {
        store
            .complete_wake(&unsure, OwnerLanding::NotLanded, true, NOON)
            .unwrap()
    };
    assert_eq!(deliver(&mut store), WakeCompletion::Closed);
    assert_eq!(deliver(&mut store), WakeCompletion::NotOwed);
    assert_eq!(store.list_owed(None).unwrap().len(), 1);
}

#[test]
fn a_due_step_set_again_is_a_wake_watch_with_the_same_prompt() {
    let (_temp, mut store) = store();
    let id = wake(&mut store, &["staging"], "confirm prod logs", true);
    store
        .complete_wake(&id, OwnerLanding::Landed, false, NOON)
        .unwrap();
    let clock = LocalClock {
        now_ms: NOON,
        utc_offset_minutes: 0,
    };
    let when = || vec![parse("17:00", clock).unwrap()];
    assert_eq!(
        store
            .rearm_owed("one", "nosuch", when(), "one", 0, NOON)
            .unwrap(),
        None
    );
    // A refusal leaves the step due.
    let refused = store
        .rearm_owed("one", &id, Vec::new(), "one", 0, NOON)
        .unwrap()
        .unwrap();
    assert_eq!(refused, Err(crate::WatchRefusal::NoConditions));
    assert_eq!(store.list_owed(None).unwrap().len(), 1);
    let watch = store
        .rearm_owed("one", &id, when(), "one", 0, NOON)
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(watch.then.wake.as_deref(), Some("confirm prod logs"));
    assert!(watch.before_release());
    assert_ne!(watch.id, id);
    assert!(store.list_owed(None).unwrap().is_empty());
    assert_eq!(store.list_watches(Some("one")).unwrap(), [watch]);
    assert_eq!(
        owed_events(&store, "one").last().unwrap(),
        "again at 17:00: confirm prod logs"
    );
}

#[test]
fn a_worktree_past_its_limit_loses_its_oldest_due_step_and_says_so() {
    let (_temp, mut store) = store();
    let first = due(&mut store, "step 0", NOON);
    for nth in 1..=OWED_PER_SLUG_MAX as i64 {
        due(&mut store, &format!("step {nth}"), NOON + nth);
    }
    let held = store.list_owed(Some("one")).unwrap();
    assert_eq!(held.len(), OWED_PER_SLUG_MAX);
    assert!(held.iter().all(|step| step.id != first.id));
    assert!(
        owed_events(&store, "one")
            .contains(&"dropped: step 0: more than 20 steps were due".to_owned())
    );
}

#[test]
fn removal_moves_due_steps_onto_the_removal_record_where_they_still_close() {
    let (_temp, mut store) = store();
    let step = due(&mut store, "confirm prod logs", NOON);
    // An entry this build cannot read is left byte for byte.
    write_raw(&mut store, |state| {
        state["owed"]
            .as_array_mut()
            .unwrap()
            .push(json!({"id": 7, "from": "later"}));
    });
    store
        .record_removed_worktrees(&[removal(true)], NOON + 2_000)
        .unwrap();
    assert_eq!(raw(&store)["owed"], json!([{"id": 7, "from": "later"}]));
    assert!(store.list_owed(None).unwrap().is_empty());
    let read = removed(&mut store);
    assert_eq!(read.owed(), std::slice::from_ref(&step));
    assert!(read.owes_anything());
    // Recording it again neither loses nor doubles the step.
    store
        .record_removed_worktrees(&[removal(true)], NOON + 3_000)
        .unwrap();
    assert_eq!(removed(&mut store).owed().len(), 1);
    // It cannot wake a session that is gone, but it can be closed.
    let clock = LocalClock {
        now_ms: NOON,
        utc_offset_minutes: 0,
    };
    let when = vec![parse("17:00", clock).unwrap()];
    assert_eq!(
        store
            .rearm_owed("one", &step.id, when, "one", 0, NOON)
            .unwrap(),
        None
    );
    let closed = store
        .close_owed("one", &step.id, "did it", None)
        .unwrap()
        .unwrap();
    assert!(closed.removed);
    assert!(removed(&mut store).owed().is_empty());
    assert_eq!(
        owed_events(&store, "one").last().unwrap(),
        "done: confirm prod logs: did it"
    );
    // The steps go when the removal record ages out.
    let (_temp, mut store) = self::store();
    due(&mut store, "confirm prod logs", NOON);
    store
        .record_removed_worktrees(&[removal(true)], NOON + 2_000)
        .unwrap();
    let mut later = removal(true);
    later.slug = "two".into();
    later.removed_at = "2027-01-01T00:00:00Z".into();
    let much_later = wt_core::parse_iso_millis("2027-01-01T00:00:00Z").unwrap();
    store
        .record_removed_worktrees(&[later], much_later)
        .unwrap();
    assert_eq!(store.read_removed_worktrees().unwrap().len(), 1);
    assert!(raw(&store).to_string().find("confirm prod logs").is_none());
}

#[test]
fn a_due_step_whose_worktree_is_gone_follows_its_removal_record_or_is_dropped() {
    let (_temp, mut store) = store();
    let kept = due(&mut store, "kept with the live row", NOON);
    let live: BTreeMap<String, Option<String>> =
        [("one".to_owned(), Some(CREATED.to_owned()))].into();
    store.drop_orphans(&live, &BTreeMap::new(), NOON).unwrap();
    assert_eq!(store.list_owed(None).unwrap(), std::slice::from_ref(&kept));

    // An older build removed the row: it recorded the removal and left the
    // step behind. Noticing the row gone moves the step onto that record.
    write_raw(&mut store, |state| {
        state["removed"] = json!([
            {"slug": "one", "branch": "b", "removedAt": "2026-11-24T12:00:01Z"},
        ]);
    });
    store
        .drop_orphans(&BTreeMap::new(), &BTreeMap::new(), NOON + 5_000)
        .unwrap();
    assert!(store.list_owed(None).unwrap().is_empty());
    assert_eq!(removed(&mut store).owed(), std::slice::from_ref(&kept));
    assert!(
        owed_events(&store, "one")
            .iter()
            .all(|event| !event.starts_with("dropped"))
    );

    // A later worktree of the same name does not inherit an earlier one's
    // step, and with no removal record to hold it the step is dropped.
    let (_temp, mut store) = self::store();
    let lost = due(&mut store, "nobody left to do it", NOON);
    let reborn: BTreeMap<String, Option<String>> =
        [("one".to_owned(), Some("2026-11-25T00:00:00Z".to_owned()))].into();
    store
        .drop_orphans(&reborn, &BTreeMap::new(), NOON + 5_000)
        .unwrap();
    assert!(store.list_owed(None).unwrap().is_empty());
    assert!(store.read_removed_worktrees().unwrap().is_empty());
    // The slug's history belongs to the later worktree: nothing is added.
    assert!(
        owed_events(&store, "one")
            .iter()
            .all(|event| !event.starts_with("dropped"))
    );
    let (_temp, mut store) = self::store();
    due(&mut store, "nobody left to do it", NOON);
    store
        .drop_orphans(&BTreeMap::new(), &BTreeMap::new(), NOON + 5_000)
        .unwrap();
    assert_eq!(
        owed_events(&store, "one").last().unwrap(),
        "dropped: nobody left to do it: its worktree was removed"
    );
    let _ = lost;
}

#[test]
fn a_restore_attaches_the_removal_records_steps_to_the_new_worktree() {
    let (_temp, mut store) = store();
    let step = due(&mut store, "confirm prod logs", NOON);
    store
        .record_removed_worktrees(&[removal(true)], NOON + 2_000)
        .unwrap();
    let record = removed(&mut store);
    store.clear_slug_state("one").unwrap();
    store.clear_removed_worktree("one").unwrap();
    store
        .record_slug_created("one", "2026-11-25T00:00:00Z")
        .unwrap();
    assert!(store.restore_owed("one", &record.owed()).unwrap());
    assert!(!store.restore_owed("one", &[]).unwrap());
    let restored = store.list_owed(Some("one")).unwrap();
    assert_eq!(restored.len(), 1);
    assert_eq!(restored[0].id, step.id);
    assert_eq!(
        restored[0].incarnation.as_deref(),
        Some("2026-11-25T00:00:00Z")
    );
    // It is the new worktree's own: the engine leaves it in place.
    let live: BTreeMap<String, Option<String>> =
        [("one".to_owned(), Some("2026-11-25T00:00:00Z".to_owned()))].into();
    store
        .drop_orphans(&live, &BTreeMap::new(), NOON + 9_000)
        .unwrap();
    assert_eq!(store.list_owed(None).unwrap().len(), 1);
}
