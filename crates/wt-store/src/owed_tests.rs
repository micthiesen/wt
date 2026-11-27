use std::collections::BTreeMap;

use serde_json::{Map, Value, json};
use tempfile::{TempDir, tempdir};
use wt_core::{
    Landing, LeftFor, OWED_PER_SLUG_MAX, OwedRecord, OwnerLanding, VerifyStage, WatchChange,
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
        [
            "dropped: staging check: open the page",
            "dropped: production check: watch the logs",
        ]
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
fn a_status_that_stops_owing_a_check_records_what_became_of_it() {
    // Dropped work drops its checks; a cleared status does too.
    let (_temp, mut store) = store();
    store.set_slug_work_status("one", Some(&both())).unwrap();
    let mut dropped = status("dropped");
    dropped.by = None;
    store
        .update_slug_work_status("one", |_| Ok::<_, ()>(Some(dropped)))
        .unwrap()
        .unwrap();
    assert_eq!(
        owed_events(&store, "one"),
        [
            "dropped: staging check: open the page",
            "dropped: production check: watch the logs",
        ]
    );
    store.set_slug_work_status("one", Some(&both())).unwrap();
    store.clear_slug_work_status("one", None).unwrap();
    assert_eq!(owed_events(&store, "one").len(), 4);
    // An assertion that carries one check and leaves the other out
    // dropped the other; carrying both records nothing.
    let (_temp, mut store) = self::store();
    store.set_slug_work_status("one", Some(&both())).unwrap();
    let mut carried = both();
    carried.state = "review".into();
    store.set_slug_work_status("one", Some(&carried)).unwrap();
    assert!(owed_events(&store, "one").is_empty());
    let one = status("review").with_check(PRODUCTION, "watch the logs");
    store.set_slug_work_status("one", Some(&one)).unwrap();
    assert_eq!(
        owed_events(&store, "one"),
        ["dropped: staging check: open the page"]
    );
}

#[test]
fn a_status_names_every_stage_and_a_check_leaving_it_is_not_a_new_status() {
    let (_temp, mut store) = store();
    store.set_slug_work_status("one", Some(&both())).unwrap();
    let statuses = |store: &Store| -> Vec<Option<String>> {
        store
            .read_worktree_events("one", 500)
            .unwrap()
            .into_iter()
            .filter_map(|event| match event.event {
                WorktreeEventKind::Status { verify_in, .. } => Some(verify_in),
                _ => None,
            })
            .collect()
    };
    assert_eq!(
        statuses(&store),
        [Some("staging and production".to_owned())]
    );
    // Closed, put off or dropped: the check's own event says so, and the
    // status was not asserted again.
    store
        .close_checks(
            "one",
            &CheckTargets::Stages(vec![STAGING]),
            &status("verified"),
            Some("saw it"),
        )
        .unwrap();
    store
        .defer_check("one", PRODUCTION, NOON + 3_600_000, "at 13:00", None)
        .unwrap();
    store.drop_check("one", PRODUCTION, None).unwrap();
    assert_eq!(statuses(&store).len(), 1);
    assert!(crate::only_checks_left(
        Some("staging and production"),
        None
    ));
    assert!(crate::only_checks_left(
        Some("staging and production"),
        Some("production")
    ));
    assert!(!crate::only_checks_left(
        Some("staging"),
        Some("production")
    ));
    // Owing a check again is an assertion.
    let again = status("ready").with_check(PRODUCTION, "watch the logs");
    store.set_slug_work_status("one", Some(&again)).unwrap();
    assert_eq!(
        statuses(&store).last(),
        Some(&Some("production".to_owned()))
    );
    assert_eq!(statuses(&store).len(), 2);
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

fn assert_status(store: &mut Store, next: WorkStatusRecord) -> crate::WorkStatusWrite {
    store
        .update_slug_work_status("one", |previous| {
            let mut next = next;
            if let Some(previous) = previous {
                next.carry_checks(previous);
            }
            Ok::<_, ()>(Some(next))
        })
        .unwrap()
        .unwrap()
}

fn seen_due(pr: u64, since_ms: i64) -> wt_core::CheckDue {
    wt_core::CheckDue {
        slug: "one".into(),
        created_at: Some(CREATED.into()),
        stage: STAGING,
        steps: "open the page".into(),
        since_ms,
        pr: Some(pr),
    }
}

#[test]
fn a_check_is_dated_when_first_seen_due_and_again_for_a_later_pull_request() {
    let (_temp, mut store) = store();
    assert_status(
        &mut store,
        status("ready").with_check(STAGING, "open the page"),
    );
    let began = stored(&mut store).open_check(STAGING).unwrap();
    assert!(began.since_ms.is_some());
    assert_eq!(began.due_since_ms, None);

    // Another worktree's check with this slug is not this one's.
    let mut other = seen_due(1, NOON - 1_000);
    other.created_at = Some("2026-01-01T00:00:00Z".into());
    assert_eq!(store.note_checks_due(&[other], NOON).unwrap(), 0);

    assert_eq!(
        store
            .note_checks_due(&[seen_due(1, NOON - 1_000)], NOON)
            .unwrap(),
        1
    );
    let dated = stored(&mut store).open_check(STAGING).unwrap();
    assert_eq!(dated.due_since_ms, Some(NOON - 1_000));
    assert_eq!(dated.due_pr, Some(1));
    assert_eq!(dated.since_ms, began.since_ms);
    assert_eq!(
        owed_events(&store, "one"),
        ["due: staging check: open the page (wt done staging)"]
    );
    // Seen due again: nothing is written and nothing is said twice.
    assert_eq!(
        store
            .note_checks_due(&[seen_due(1, NOON + 5_000)], NOON + 5_000)
            .unwrap(),
        0
    );

    // Asserting the status again, with the same steps, keeps the date: the
    // overdue clock does not start over.
    assert_status(
        &mut store,
        status("needs-human").with_check(STAGING, "open the page"),
    );
    assert_eq!(stored(&mut store).open_check(STAGING).unwrap(), dated);

    // Due for a later pull request of the same worktree: a new round.
    assert_eq!(
        store
            .note_checks_due(&[seen_due(2, NOON + 9_000)], NOON + 9_000)
            .unwrap(),
        1
    );
    let again = stored(&mut store).open_check(STAGING).unwrap();
    assert_eq!(again.due_since_ms, Some(NOON + 9_000));
    assert_eq!(again.since_ms, Some(NOON + 9_000));
    assert_eq!(again.due_pr, Some(2));
    assert_eq!(owed_events(&store, "one").len(), 2);

    // Closed and owed again with the same steps: a new check, not yet due.
    store
        .close_checks("one", &CheckTargets::All, &status("verified"), None)
        .unwrap();
    assert_status(
        &mut store,
        status("ready").with_check(STAGING, "open the page"),
    );
    let renewed = stored(&mut store).open_check(STAGING).unwrap();
    assert_eq!(renewed.due_since_ms, None);
    assert!(
        renewed
            .since_ms
            .is_some_and(|since| Some(since) != again.since_ms)
    );
}

#[test]
fn a_removed_row_that_reached_production_owes_its_production_check_now() {
    let (_temp, mut store) = store();
    assert_status(
        &mut store,
        status("ready").with_check(PRODUCTION, "watch the logs"),
    );
    store
        .append_worktree_event(
            "one",
            u64::try_from(NOON).unwrap(),
            &WorktreeEventKind::Landed {
                on: wt_core::LandedOn::Production,
                head: None,
                landed_at_ms: None,
            },
        )
        .unwrap();
    store
        .record_removed_worktrees(&[removal(true)], NOON)
        .unwrap();
    let entry = removed(&mut store);
    assert_eq!(entry.extra["landedOn"], "production");
    let (items, landing) = crate::removed_owed_items(&entry, true, NOON);
    assert!(landing.production);
    assert_eq!(items[0].state, wt_core::OwedState::Due);

    // A record written before the landing was kept is healed from history.
    write_raw(&mut store, |state| {
        state["removed"][0]
            .as_object_mut()
            .unwrap()
            .remove("landedOn");
    });
    assert_eq!(raw(&store)["removed"][0].get("landedOn"), None);
    // Reading does not write.
    assert!(!crate::removed_landing(&removed(&mut store), true).production);
    assert_eq!(raw(&store)["removed"][0].get("landedOn"), None);
    assert_eq!(store.heal_removed_landings().unwrap(), 1);
    assert_eq!(store.heal_removed_landings().unwrap(), 0);
    let healed = removed(&mut store);
    assert!(crate::removed_landing(&healed, true).production);
    assert_eq!(raw(&store)["removed"][0]["landedOn"], "production");

    // Nobody is left to prompt, so the manager is told, once.
    let untold = crate::removed_due_untold(&healed, true, NOON);
    assert_eq!(untold.len(), 1);
    assert_eq!(untold[0].0, "production");
    assert!(
        store
            .mark_removed_due_told("one", &healed.removed_at, &[untold[0].0.clone()])
            .unwrap()
    );
    assert!(crate::removed_due_untold(&removed(&mut store), true, NOON).is_empty());
    // Put off and due again, it is told again.
    store
        .reap_wt_state(&std::collections::BTreeSet::new())
        .unwrap();
    store
        .defer_check("one", PRODUCTION, NOON + 10, "at 12:00", None)
        .unwrap()
        .unwrap();
    let entry = removed(&mut store);
    assert!(crate::removed_due_untold(&entry, true, NOON).is_empty());
    assert_eq!(crate::removed_due_untold(&entry, true, NOON + 20).len(), 1);

    // A row that only reached base still waits.
    let (_temp, mut store) = store_with_base_only();
    let entry = removed(&mut store);
    let (items, _) = crate::removed_owed_items(&entry, true, NOON);
    assert_eq!(items[0].state, wt_core::OwedState::Waiting);
}

fn store_with_base_only() -> (TempDir, Store) {
    let (temp, mut store) = store();
    assert_status(
        &mut store,
        status("ready").with_check(PRODUCTION, "watch the logs"),
    );
    store
        .record_removed_worktrees(&[removal(true)], NOON)
        .unwrap();
    (temp, store)
}

fn marks(store: &Store) -> Vec<String> {
    store
        .read_worktree_events("one", 500)
        .unwrap()
        .into_iter()
        .filter_map(|event| match event.event {
            WorktreeEventKind::Mark { text, .. } => Some(text),
            _ => None,
        })
        .collect()
}

#[test]
fn a_stamp_left_pending_for_over_a_day_is_ignored_and_dropped_by_the_next_status() {
    let (_temp, mut store) = store();
    let raw = |store: &mut Store| {
        store.read_wt_state().unwrap()["slugs"]["one"][wt_core::STAMP_PENDING_KEY].clone()
    };
    let age = |store: &mut Store, hours: i64| {
        let at = (time::OffsetDateTime::now_utc() - time::Duration::hours(hours))
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap();
        store
            .mutate_wt_state(|state| {
                state["slugs"]["one"][wt_core::STAMP_PENDING_KEY]["at"] = at.into();
                Ok(((), true))
            })
            .unwrap();
    };
    let pending =
        |store: &mut Store| crate::stored_pending_stamp(&store.read_wt_state().unwrap(), "one");

    store.set_pending_stamp("one", "check", None).unwrap();
    age(&mut store, 23);
    assert_eq!(pending(&mut store).as_deref(), Some("check"));
    age(&mut store, 25);
    assert_eq!(pending(&mut store), None);
    assert!(raw(&mut store).is_object());

    // Even a status that would not have ended the round drops it.
    assert_eq!(assert_status(&mut store, status("working")).stamped, None);
    assert!(raw(&mut store).is_null());
    assert_eq!(assert_status(&mut store, status("ready")).stamped, None);
    assert_eq!(
        crate::stored_stamp(&store.read_wt_state().unwrap(), "one"),
        None
    );
}

#[test]
fn a_stamp_is_earned_by_the_status_that_ends_the_round_and_then_stays() {
    let (_temp, mut store) = store();
    let stamp = |store: &mut Store| crate::stored_stamp(&store.read_wt_state().unwrap(), "one");
    let pending =
        |store: &mut Store| crate::stored_pending_stamp(&store.read_wt_state().unwrap(), "one");
    let at = |state: &str, sha: &str| WorkStatusRecord {
        sha: Some(sha.into()),
        ..status(state)
    };

    assert!(
        store
            .set_pending_stamp("one", "fable-review", Some("one"))
            .unwrap()
    );
    // The round is still running: review and working earn nothing.
    assert_eq!(assert_status(&mut store, at("review", "aaa")).stamped, None);
    assert_eq!(
        assert_status(&mut store, at("working", "bbb")).stamped,
        None
    );
    assert_eq!(stamp(&mut store), None);

    let earned = assert_status(&mut store, at("ready", "ccc"))
        .stamped
        .unwrap();
    assert_eq!(
        (earned.skill.as_str(), earned.sha.as_deref()),
        ("fable-review", Some("ccc"))
    );
    assert_eq!(stamp(&mut store), Some(earned.clone()));
    assert_eq!(pending(&mut store), None);
    assert_eq!(marks(&store), ["stamped fable-review"]);

    // Sticky: a later status, at a later head, carries it unchanged.
    assert_eq!(
        assert_status(&mut store, at("working", "ddd")).stamped,
        None
    );
    assert_status(&mut store, at("ready", "eee"));
    assert_eq!(stamp(&mut store), Some(earned.clone()));

    // A new round replaces it, even when the status it ends on is the one
    // already asserted.
    store.set_pending_stamp("one", "check", None).unwrap();
    let write = assert_status(&mut store, at("ready", "eee"));
    assert!(!write.changed);
    assert_eq!(write.stamped.unwrap().skill, "check");
    assert_eq!(stamp(&mut store).unwrap().sha.as_deref(), Some("eee"));

    // By hand, at once.
    let by_hand = store
        .stamp_now("one", "fable-review", Some("fff"), None)
        .unwrap()
        .unwrap();
    assert_eq!(stamp(&mut store), Some(by_hand));
    assert!(
        store
            .stamp_now("one", "not a name", None, None)
            .unwrap()
            .is_none()
    );

    // Clearing the status clears the stamp, and one not yet earned.
    store.set_pending_stamp("one", "check", None).unwrap();
    store.clear_slug_work_status("one", None).unwrap();
    assert_eq!(stamp(&mut store), None);
    assert_eq!(pending(&mut store), None);

    // With no status to carry it, a stamp by hand waits for the next one.
    assert_eq!(
        store.stamp_now("one", "check", Some("fff"), None).unwrap(),
        None
    );
    assert_eq!(pending(&mut store).as_deref(), Some("check"));
}

/// 2100-01-01 00:00:00 UTC: later than any clock a test runs under.
const FAR: i64 = 4_102_444_800_000;

fn stored_check(store: &Store, stage: &str) -> Value {
    raw(store)["slugs"]["one"]["work"]["checks"][stage].clone()
}

#[test]
fn a_check_an_earlier_pull_request_left_becomes_a_step_instead_of_being_replaced() {
    let (_temp, mut store) = store();
    let mut first = both();
    assert!(first.defer_check(PRODUCTION, Some(FAR)));
    assert_status(&mut store, first);
    // Nothing was left anywhere yet: other steps replace the check.
    let reworded = assert_status(&mut store, status("ready").with_check(STAGING, "open it"));
    assert!(reworded.displaced.is_empty());
    assert_eq!(store.list_watches(None).unwrap(), []);

    let left = store.leave_checks("one", &LeftFor::Pr(7)).unwrap();
    assert_eq!(
        left.iter().map(|(stage, _)| *stage).collect::<Vec<_>>(),
        [STAGING, PRODUCTION]
    );
    assert_eq!(stored_check(&store, "staging")["forPr"], 7);
    // A later pull request does not claim what an earlier one left.
    assert!(
        store
            .leave_checks("one", &LeftFor::Pr(8))
            .unwrap()
            .is_empty()
    );
    assert_eq!(stored_check(&store, "production")["forPr"], 7);

    // A status that only carries the checks, or drops one, displaces none.
    assert!(
        assert_status(&mut store, status("working"))
            .displaced
            .is_empty()
    );
    assert_eq!(stored_check(&store, "staging")["forPr"], 7);

    // Other steps in a stage: the next pull request's check takes the
    // stage and the earlier one becomes a wake for its own pull request.
    let write = assert_status(
        &mut store,
        status("ready")
            .with_check(STAGING, "open the other page")
            .with_check(PRODUCTION, "watch the other logs"),
    );
    assert_eq!(write.displaced.len(), 2);
    let staging = &write.displaced[0];
    assert_eq!(
        (staging.stage, &staging.left, staging.check.steps.as_str()),
        (STAGING, &LeftFor::Pr(7), "open it")
    );
    assert_eq!(
        staging
            .watch
            .when
            .iter()
            .map(|cond| cond.cond.to_string())
            .collect::<Vec<_>>(),
        ["pr:7:staging"]
    );
    assert!(staging.watch.owed_on_delivery());
    assert!(
        staging
            .watch
            .then
            .wake
            .as_deref()
            .unwrap()
            .starts_with("#7 staging check: open it\n")
    );
    // A check that was put off is still not due before its time.
    assert_eq!(
        write.displaced[1]
            .watch
            .when
            .iter()
            .map(|cond| cond.cond.to_string())
            .collect::<Vec<_>>(),
        ["pr:7:production".to_owned(), format!("at:{FAR}")]
    );
    assert_eq!(
        stored_check(&store, "staging")["steps"],
        "open the other page"
    );
    assert!(stored_check(&store, "staging").get("forPr").is_none());
    assert_eq!(raw(&store)["watches"][0]["owedOnDelivery"], true);
    assert_eq!(store.list_watches(Some("one")).unwrap().len(), 2);

    // Its pull request reaching the stage leaves it owed, although the
    // worktree's current work has not landed.
    let WakeCompletion::Due(step) = store
        .complete_wake(&staging.watch.id, OwnerLanding::NotLanded, false, NOON)
        .unwrap()
    else {
        panic!("an earlier pull request's check must stay owed");
    };
    assert_eq!(step.id, staging.watch.id);
    assert_eq!(store.list_owed(Some("one")).unwrap(), [*step]);
}

#[test]
fn a_check_that_cannot_be_kept_refuses_the_status_that_would_replace_it() {
    let (_temp, mut store) = store();
    assert_status(&mut store, both());
    store.leave_checks("one", &LeftFor::Pr(7)).unwrap();
    // Room for one more wake, where two checks need keeping.
    for n in 1..wt_core::watch::WATCHES_PER_SLUG_MAX {
        wake(&mut store, &["17:00"], &format!("step {n}"), false);
    }
    let sets = |store: &Store| {
        store
            .read_worktree_events("one", 500)
            .unwrap()
            .into_iter()
            .filter(|event| {
                matches!(
                    event.event,
                    WorktreeEventKind::Watch {
                        change: WatchChange::Set,
                        ..
                    }
                )
            })
            .count()
    };
    let (before, set_before) = (raw(&store), sets(&store));
    let refused = store
        .update_slug_work_status("one", |_| {
            Ok::<_, ()>(Some(
                status("ready")
                    .with_check(STAGING, "open the other page")
                    .with_check(PRODUCTION, "watch the other logs"),
            ))
        })
        .unwrap_err()
        .to_string();
    assert!(
        refused.contains("the staging check of #7 and the production check of #7")
            && refused.contains("wt done <stage>"),
        "{refused}"
    );
    // Neither wake was set, and the history says nothing of one.
    assert_eq!(raw(&store), before);
    assert_eq!(sets(&store), set_before);
}

fn owed_lines(store: &Store) -> Vec<String> {
    store
        .read_worktree_events("one", 500)
        .unwrap()
        .iter()
        .filter_map(|event| wt_core::owed_sentence(&event.event))
        .collect()
}

/// A displaced staging check that had been due since `NOON`, and its wake.
fn displaced_due(store: &mut Store) -> String {
    assert_status(store, status("ready").with_check(STAGING, "open the page"));
    assert_eq!(
        store
            .note_checks_due(&[seen_due(7, NOON)], NOON + 1)
            .unwrap(),
        1
    );
    store.leave_checks("one", &LeftFor::Pr(7)).unwrap();
    let write = assert_status(
        store,
        status("ready").with_check(STAGING, "open the other page"),
    );
    write.displaced[0].watch.id.clone()
}

#[test]
fn a_displaced_check_keeps_its_due_date_and_says_in_history_that_it_became_a_step() {
    let (_temp, mut store) = store();
    let id = displaced_due(&mut store);
    assert!(
        owed_lines(&store)
            .contains(&"again when #7 on staging: staging check: open the page".to_owned()),
        "{:?}",
        owed_lines(&store)
    );
    let WakeCompletion::Due(step) = store
        .complete_wake(&id, OwnerLanding::NotLanded, false, NOON + 100)
        .unwrap()
    else {
        panic!("an earlier pull request's check must stay owed");
    };
    assert_eq!(step.due_at_ms, NOON);
    // Put off, it is still that check: its new wake stays owed too.
    let later = LocalClock {
        now_ms: NOON + 200,
        utc_offset_minutes: 0,
    };
    let again = store
        .rearm_owed(
            "one",
            &id,
            vec![parse("17:00", later).unwrap()],
            "one",
            0,
            NOON + 200,
        )
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(again.owed_on_delivery());
    assert_eq!(raw(&store)["keptChecks"][0]["id"], again.id.as_str());
    // Closed by this build, nothing is kept and nothing comes back.
    let WakeCompletion::Due(_) = store
        .complete_wake(&again.id, OwnerLanding::NotLanded, false, NOON + 300)
        .unwrap()
    else {
        panic!("put off, it stays owed");
    };
    assert!(
        store
            .close_owed("one", &again.id, "saw it", Some("one"))
            .unwrap()
            .is_some()
    );
    assert_eq!(raw(&store)["keptChecks"], json!([]));
    assert!(store.restore_kept_checks(NOON + 400).unwrap().is_empty());
}

#[test]
fn a_displaced_check_an_older_build_closed_on_delivery_is_owed_again_exactly_once() {
    let (_temp, mut store) = store();
    let id = displaced_due(&mut store);
    // Writes that know nothing of kept checks, and a whole-state read and
    // write back, carry the record.
    let before = raw(&store)["keptChecks"].clone();
    assert_eq!(before[0]["id"], id.as_str());
    store.set_branch_tip("trunk", "sha").unwrap();
    let state = store.read_wt_state().unwrap();
    store
        .write_repository_state_json(&state.to_string())
        .unwrap();
    assert_eq!(raw(&store)["keptChecks"], before);
    assert!(!crate::kept_checks_lost(&raw(&store)));
    assert!(store.restore_kept_checks(NOON + 5).unwrap().is_empty());

    // A build from before displaced checks delivers the wake to a worktree
    // whose current work has not landed: it closes, and nothing is owed.
    assert!(
        store
            .complete_action(&id, wt_core::watch::WatchAction::Wake)
            .unwrap()
    );
    assert!(store.list_owed(Some("one")).unwrap().is_empty());
    assert!(crate::kept_checks_lost(&raw(&store)));

    let restored = store.restore_kept_checks(NOON + 50).unwrap();
    assert_eq!(restored.len(), 1);
    assert_eq!(
        (
            restored[0].id.as_str(),
            restored[0].due_at_ms,
            restored[0].when_text.as_str()
        ),
        (id.as_str(), NOON, "#7 on staging")
    );
    assert!(
        restored[0]
            .what
            .starts_with("#7 staging check: open the page\n")
    );
    assert_eq!(store.list_owed(Some("one")).unwrap(), restored);
    assert!(!crate::kept_checks_lost(&raw(&store)));
    assert!(store.restore_kept_checks(NOON + 60).unwrap().is_empty());
    let said: Vec<String> = owed_lines(&store)
        .into_iter()
        .filter(|line| line.contains("restored"))
        .collect();
    assert_eq!(said.len(), 1, "{said:?}");
    assert!(said[0].ends_with(&format!("(wt done {id})")), "{said:?}");

    // That older build closing the step leaves the record behind; its
    // history event is what says the step was ended on purpose.
    let mut state = raw(&store);
    state["owed"] = json!([]);
    store
        .write_repository_state_json(&state.to_string())
        .unwrap();
    store
        .append_worktree_event(
            "one",
            u64::try_from(crate::store::now_ms()).unwrap(),
            &WorktreeEventKind::Owed {
                id: Some(id.clone()),
                change: wt_core::OwedChange::Done,
                what: "#7 staging check".into(),
                evidence: Some("saw it".into()),
                by: None,
                stage: None,
                when: None,
                unconfirmed: false,
            },
        )
        .unwrap();
    assert!(crate::kept_checks_lost(&raw(&store)));
    assert!(store.restore_kept_checks(NOON + 70).unwrap().is_empty());
    assert_eq!(raw(&store)["keptChecks"], json!([]));
    assert!(store.list_owed(Some("one")).unwrap().is_empty());
}

#[test]
fn a_displaced_step_an_older_build_put_off_is_not_owed_twice() {
    let (_temp, mut store) = store();
    let id = displaced_due(&mut store);
    let WakeCompletion::Due(step) = store
        .complete_wake(&id, OwnerLanding::NotLanded, false, NOON + 100)
        .unwrap()
    else {
        panic!("an earlier pull request's check must stay owed");
    };
    // `wt done <id> --again` in a build from before kept checks: the step
    // goes, a wake with another id carries its prompt, the record stays.
    let mut state = raw(&store);
    state["owed"] = json!([]);
    store
        .write_repository_state_json(&state.to_string())
        .unwrap();
    let again = wake(&mut store, &["17:00"], &step.what, false);
    store
        .append_worktree_event(
            "one",
            u64::try_from(crate::store::now_ms()).unwrap(),
            &WorktreeEventKind::Owed {
                id: Some(id.clone()),
                change: wt_core::OwedChange::Again,
                what: "#7 staging check".into(),
                evidence: None,
                by: None,
                stage: None,
                when: Some("at 17:00".into()),
                unconfirmed: false,
            },
        )
        .unwrap();
    assert!(crate::kept_checks_lost(&raw(&store)));
    assert!(store.restore_kept_checks(NOON + 200).unwrap().is_empty());
    assert_eq!(raw(&store)["keptChecks"], json!([]));
    assert!(store.list_owed(Some("one")).unwrap().is_empty());
    assert_eq!(store.list_watches(Some("one")).unwrap()[0].id, again);
    assert!(
        !owed_lines(&store)
            .iter()
            .any(|line| line.contains("restored"))
    );
}

#[test]
fn work_that_landed_without_a_pull_request_keeps_its_check_by_its_head() {
    let (_temp, mut store) = store();
    assert_status(
        &mut store,
        status("ready").with_check(STAGING, "open the page"),
    );
    let head = "0123456789abcdef0123456789abcdef01234567";
    store
        .leave_checks("one", &LeftFor::Commit(head.into()))
        .unwrap();
    let write = assert_status(
        &mut store,
        status("ready").with_check(STAGING, "open the other page"),
    );
    let kept = &write.displaced[0];
    assert_eq!(
        kept.watch.when[0].cond.to_string(),
        format!("commit:{head}:staging")
    );
    assert!(
        kept.watch
            .then
            .wake
            .as_deref()
            .unwrap()
            .starts_with("0123456 staging check: open the page\n")
    );
}
