use std::collections::BTreeMap;

use serde_json::{Map, Value, json};
use tempfile::{TempDir, tempdir};
use wt_core::{
    VerifyStage, WatchChange, WorktreeEventKind,
    watch::{
        Condition, LocalClock, PrFacts, PrLife, RowFacts, WATCHES_MAX, WATCHES_PER_SLUG_MAX, Watch,
        WatchAction, WatchActions, WatchFacts, evaluate, parse,
    },
};

use crate::{
    NewWatch, RemovedWorktree, RepositoryIdentity, Store, UnblockDelivery, WatchRefusal,
    WatchSelector, WorkStatusRecord,
};

/// 2026-11-24 12:00:00 UTC.
const NOON: i64 = 1_795_521_600_000;
const CREATED: &str = "2026-11-01T00:00:00Z";
const SHA: &str = "0123abcdef0123abcdef0123abcdef0123abcdef";

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

fn then(set: impl FnOnce(&mut WatchActions)) -> WatchActions {
    let mut then = WatchActions::default();
    set(&mut then);
    then
}

fn notify() -> WatchActions {
    then(|then| then.notify = Some(String::new()))
}

fn wake() -> WatchActions {
    then(|then| then.wake = Some("test it".into()))
}

fn new(slug: &str, conditions: &[&str], then: WatchActions) -> NewWatch {
    let clock = LocalClock {
        now_ms: NOON,
        utc_offset_minutes: 0,
    };
    NewWatch {
        slug: slug.into(),
        by: slug.into(),
        utc_offset_minutes: 0,
        when: conditions
            .iter()
            .map(|arg| parse(arg, clock).unwrap())
            .collect(),
        then,
    }
}

fn add(store: &mut Store, slug: &str, conditions: &[&str], then: WatchActions) -> Watch {
    store
        .add_watch(new(slug, conditions, then), NOON)
        .unwrap()
        .unwrap()
}

fn status(blocked_on: Option<&str>) -> WorkStatusRecord {
    WorkStatusRecord {
        state: "ready".into(),
        at: "2026-11-24T12:00:00Z".into(),
        note: None,
        risk: None,
        sha: None,
        by: None,
        blocked_on: blocked_on.map(str::to_owned),
        verify_after_merge: None,
        verify_in: None,
        extra: Map::new(),
    }
}

fn removed(slug: &str, landed: &str) -> RemovedWorktree {
    RemovedWorktree {
        slug: slug.into(),
        branch: format!("feature/{slug}"),
        removed_at: "2026-11-24T12:00:01Z".into(),
        work: None,
        automations_paused: None,
        extra: [("landedOnAtRemoval".to_owned(), json!(landed))]
            .into_iter()
            .collect(),
    }
}

fn watch_events(store: &Store, slug: &str) -> Vec<(WatchChange, String, Option<String>)> {
    store
        .read_worktree_events(slug, 500)
        .unwrap()
        .into_iter()
        .filter_map(|event| match event.event {
            WorktreeEventKind::Watch {
                change,
                what,
                reason,
                by: _,
                then: _,
                id: _,
            } => Some((change, what, reason)),
            _ => None,
        })
        .collect()
}

fn changes(store: &Store, slug: &str) -> Vec<WatchChange> {
    watch_events(store, slug)
        .into_iter()
        .map(|(change, ..)| change)
        .collect()
}

fn raw(store: &Store) -> Value {
    serde_json::from_str(&store.read_repository_state_json().unwrap().unwrap()).unwrap()
}

fn live(slugs: &[(&str, &str)]) -> BTreeMap<String, Option<String>> {
    slugs
        .iter()
        .map(|(slug, created)| ((*slug).to_owned(), Some((*created).to_owned())))
        .collect()
}

fn pr_merged(number: u64) -> WatchFacts {
    WatchFacts {
        prs: [(
            number,
            PrFacts {
                life: Some(PrLife::Merged),
                ..PrFacts::default()
            },
        )]
        .into(),
        ..WatchFacts::default()
    }
}

#[test]
fn a_watch_is_stored_with_an_id_and_its_owners_incarnation() {
    let (_temp, mut store) = store();
    let watch = add(
        &mut store,
        "one",
        &["#3806:production", "release:staging+1h"],
        wake(),
    );
    assert_eq!(watch.id.len(), 6);
    assert!(
        watch
            .id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
    );
    assert_eq!(watch.incarnation.as_deref(), Some(CREATED));
    assert_eq!((watch.created_at_ms, watch.met_at_ms), (NOON, None));
    assert_eq!(
        store.list_watches(None).unwrap(),
        std::slice::from_ref(&watch)
    );
    assert_eq!(
        store.list_watches(Some("one")).unwrap(),
        std::slice::from_ref(&watch)
    );
    assert_eq!(store.list_watches(Some("two")).unwrap(), []);
    assert_eq!(
        raw(&store)["watches"][0]["when"],
        json!([{"cond": "#3806:production"}, {"cond": "release:staging", "delayMs": 3_600_000}])
    );
    assert_eq!(
        watch_events(&store, "one"),
        [(
            WatchChange::Set,
            "#3806 in production and 1h after next staging release".to_owned(),
            None
        )]
    );
    // A worktree with no recorded creation still gets a watch.
    let unrecorded = add(&mut store, "old", &["merged"], notify());
    assert_eq!(unrecorded.incarnation, None);
    assert_ne!(unrecorded.id, watch.id);
}

#[test]
fn a_watch_with_nothing_to_wait_for_or_to_do_is_refused() {
    let (_temp, mut store) = store();
    assert_eq!(
        store.add_watch(new("one", &[], notify()), NOON).unwrap(),
        Err(WatchRefusal::NoConditions)
    );
    assert_eq!(
        store
            .add_watch(new("one", &["merged"], WatchActions::default()), NOON)
            .unwrap(),
        Err(WatchRefusal::NoActions)
    );
    assert!(raw(&store)["watches"].is_null());
}

#[test]
fn watches_are_capped_per_worktree_and_per_repository() {
    let (_temp, mut store) = store();
    // Each tells the person something else: the same watch twice is one.
    let telling = |index: usize| then(|then| then.notify = Some(format!("news {index}")));
    for index in 0..WATCHES_PER_SLUG_MAX {
        add(&mut store, "one", &["merged"], telling(index));
    }
    let refusal = store
        .add_watch(new("one", &["merged"], notify()), NOON)
        .unwrap()
        .unwrap_err();
    assert_eq!(refusal, WatchRefusal::TooManyForSlug { slug: "one".into() });
    assert!(refusal.to_string().contains("one already has 20 watches"));
    for slug in 1..WATCHES_MAX / WATCHES_PER_SLUG_MAX {
        for index in 0..WATCHES_PER_SLUG_MAX {
            add(
                &mut store,
                &format!("slug-{slug}"),
                &["merged"],
                telling(index),
            );
        }
    }
    assert_eq!(
        store
            .add_watch(new("fresh", &["merged"], notify()), NOON)
            .unwrap(),
        Err(WatchRefusal::TooMany)
    );
    let watches = store.list_watches(None).unwrap();
    assert_eq!(watches.len(), WATCHES_MAX);
    let ids: std::collections::BTreeSet<_> = watches.iter().map(|watch| &watch.id).collect();
    assert_eq!(ids.len(), WATCHES_MAX);
    // Cancelling makes room.
    let first = [watches[0].id.clone()];
    assert_eq!(
        store
            .cancel_watches(WatchSelector::Ids(&first), None)
            .unwrap()
            .len(),
        1
    );
    add(&mut store, "fresh", &["merged"], notify());
}

#[test]
fn keys_and_entries_from_a_newer_build_survive_every_write() {
    let (_temp, mut store) = store();
    let unreadable = json!({"id": "zzzzzz", "slug": "one", "shape": "of the future"});
    store
        .write_repository_state_json(
            &json!({
                "version": 99,
                "slugs": {"one": {"createdAt": CREATED}},
                "watches": [
                    {
                        "id": "aaaaaa", "slug": "one", "incarnation": CREATED, "by": "one",
                        "createdAtMs": NOON, "utcOffsetMinutes": 0, "futureTop": {"x": 1},
                        "when": [{"cond": "#7", "futureCond": 2}, {"cond": "moon:full"}],
                        "then": {"notify": "", "wake": "go", "futureAction": true}
                    },
                    unreadable
                ]
            })
            .to_string(),
        )
        .unwrap();
    let watch = store.list_watches(None).unwrap().remove(0);
    assert_eq!(store.list_watches(None).unwrap().len(), 1);
    assert_eq!(watch.when[1].cond, Condition::Unknown("moon:full".into()));

    let stamps = evaluate(&watch, &pr_merged(7), NOON).stamps.unwrap();
    assert!(store.apply_evaluation(&[stamps]).unwrap());
    assert!(
        store
            .complete_action("aaaaaa", WatchAction::Notify)
            .unwrap()
    );
    add(&mut store, "one", &["merged"], notify());
    store.set_branch_tip("trunk", "new-sha").unwrap();

    let state = raw(&store);
    assert_eq!(state["version"], 99);
    assert_eq!(state["watches"].as_array().unwrap().len(), 3);
    let kept = &state["watches"][0];
    assert_eq!(kept["futureTop"]["x"], 1);
    assert_eq!(
        kept["when"][0],
        json!({"cond": "#7:merged", "futureCond": 2, "firstHeldAtMs": NOON})
    );
    assert_eq!(kept["when"][1], json!({"cond": "moon:full"}));
    assert_eq!(kept["then"], json!({"wake": "go", "futureAction": true}));
    assert_eq!(state["watches"][1], unreadable);

    // Only an explicit cancel of its id removes what this build cannot read.
    assert!(
        !store
            .complete_action("zzzzzz", WatchAction::Notify)
            .unwrap()
    );
    store
        .drop_orphans(&BTreeMap::new(), &BTreeMap::new(), NOON)
        .unwrap();
    assert_eq!(raw(&store)["watches"], json!([unreadable]));
    let ids = ["zzzzzz".to_owned()];
    assert_eq!(
        store
            .cancel_watches(WatchSelector::Ids(&ids), None)
            .unwrap(),
        []
    );
    assert_eq!(raw(&store)["watches"], json!([]));
}

#[test]
fn a_build_that_predates_watches_does_not_lose_them() {
    let (_temp, mut store) = store();
    add(&mut store, "one", &["#7:production+1h"], wake());
    let before = raw(&store)["watches"].clone();
    // Writes that know nothing of watches, and a whole-state read and write
    // back as an older build's file-based path did.
    store.set_branch_tip("trunk", "sha").unwrap();
    store.set_slug_dev_port("one", Some(4500)).unwrap();
    store.reap_wt_state(&["one".to_owned()].into()).unwrap();
    store.reap_wt_state(&Default::default()).unwrap();
    let state = store.read_wt_state().unwrap();
    assert_eq!(state["watches"], before);
    store
        .write_repository_state_json(&state.to_string())
        .unwrap();
    store.set_branch_tip("trunk", "other").unwrap();
    assert_eq!(raw(&store)["watches"], before);
    assert_eq!(store.list_watches(None).unwrap().len(), 1);
}

#[test]
fn storing_an_evaluation_twice_writes_once() {
    let (_temp, mut store) = store();
    let watch = add(&mut store, "one", &["#7"], notify());
    assert!(!store.apply_evaluation(&[]).unwrap());
    let stamps = evaluate(&watch, &pr_merged(7), NOON + 5).stamps.unwrap();
    assert!(
        store
            .apply_evaluation(std::slice::from_ref(&stamps))
            .unwrap()
    );
    let stored = store.list_watches(None).unwrap().remove(0);
    assert_eq!(stored.met_at_ms, Some(NOON + 5));
    assert_eq!(stored.when[0].first_held_at_ms, Some(NOON + 5));
    assert!(
        !store
            .apply_evaluation(std::slice::from_ref(&stamps))
            .unwrap()
    );
    // A met watch is never met again, at another time or by a stale pass.
    let later = evaluate(&watch, &pr_merged(7), NOON + 9).stamps.unwrap();
    store.apply_evaluation(&[later]).unwrap();
    assert_eq!(
        store.list_watches(None).unwrap()[0].met_at_ms,
        Some(NOON + 5)
    );
    assert_eq!(evaluate(&stored, &pr_merged(7), NOON + 9).stamps, None);
    assert_eq!(
        changes(&store, "one"),
        [WatchChange::Set, WatchChange::Held]
    );

    // Stamps for a watch that is gone write nothing.
    store
        .cancel_watches(WatchSelector::Ids(&[watch.id]), None)
        .unwrap();
    assert!(!store.apply_evaluation(&[stamps]).unwrap());
}

#[test]
fn a_probe_error_is_written_when_it_changes_and_not_again() {
    let (_temp, mut store) = store();
    let watch = add(&mut store, "one", &["check:secret=FOO"], notify());
    let key = wt_core::watch::CheckKey {
        slug: "one".into(),
        id: "secret".into(),
        arg: Some("FOO".into()),
    };
    let failing = WatchFacts {
        checks: [(key, Err("exit 2".to_owned()))].into(),
        ..WatchFacts::default()
    };
    let stamps = evaluate(&watch, &failing, NOON).stamps.unwrap();
    assert!(store.apply_evaluation(&[stamps]).unwrap());
    let stored = store.list_watches(None).unwrap().remove(0);
    assert_eq!(stored.last_error.as_deref(), Some("exit 2"));
    assert_eq!(evaluate(&stored, &failing, NOON + 60_000).stamps, None);
}

#[test]
fn a_finished_action_is_no_longer_owed_and_the_last_one_ends_the_watch() {
    let (_temp, mut store) = store();
    store
        .set_slug_work_status("one", Some(&status(Some("after #7"))))
        .unwrap();
    let watch = add(
        &mut store,
        "one",
        &["#7"],
        then(|then| {
            then.unblock = Some("after #7".into());
            then.notify = Some("it merged".into());
            then.wake = Some("test it".into());
        }),
    );
    assert!(!store.complete_action("nosuch", WatchAction::Wake).unwrap());
    assert!(
        store
            .complete_action(&watch.id, WatchAction::Unblock)
            .unwrap()
    );
    assert!(
        !store
            .complete_action(&watch.id, WatchAction::Unblock)
            .unwrap()
    );
    let left = store.list_watches(None).unwrap().remove(0);
    assert_eq!(
        left.then.remaining().collect::<Vec<_>>(),
        [WatchAction::Notify, WatchAction::Wake]
    );
    assert!(store.complete_action(&watch.id, WatchAction::Wake).unwrap());
    assert!(
        store
            .complete_action(&watch.id, WatchAction::Notify)
            .unwrap()
    );
    assert_eq!(store.list_watches(None).unwrap(), []);
    assert!(
        !store
            .complete_action(&watch.id, WatchAction::Notify)
            .unwrap()
    );
    assert_eq!(
        changes(&store, "one"),
        [
            WatchChange::Set,
            WatchChange::ClearedGate,
            WatchChange::Woken,
            WatchChange::Notified
        ]
    );
}

#[test]
fn a_gate_watch_lives_and_dies_with_the_gate_text_it_witnessed() {
    let (_temp, mut store) = store();
    let gate = |text: &str| {
        then(|then| {
            then.unblock = Some(text.into());
            then.wake = Some("go on".into());
        })
    };
    // No gate, or another gate: nothing to clear.
    assert_eq!(
        store
            .add_watch(new("one", &["#7"], gate("after #7")), NOON)
            .unwrap(),
        Err(WatchRefusal::GateChanged { current: None })
    );
    store
        .set_slug_work_status("one", Some(&status(Some("after #7"))))
        .unwrap();
    assert_eq!(
        store
            .add_watch(new("one", &["#7"], gate("after #8")), NOON)
            .unwrap(),
        Err(WatchRefusal::GateChanged {
            current: Some("after #7".into())
        })
    );
    let reminder = add(&mut store, "one", &["#7"], notify());
    let first = add(&mut store, "one", &["#7"], gate("after #7"));
    // The gate has one watch: setting it again replaces the pending one.
    let second = add(&mut store, "one", &["#7:production"], gate("after #7"));
    let ids = |store: &mut Store| {
        store
            .list_watches(Some("one"))
            .unwrap()
            .into_iter()
            .map(|watch| watch.id)
            .collect::<Vec<_>>()
    };
    assert_eq!(ids(&mut store), [reminder.id.clone(), second.id.clone()]);
    assert_ne!(first.id, second.id);

    // Re-asserting the same gate keeps its watch.
    let mut same = status(Some("after #7"));
    same.note = Some("still waiting".into());
    store.set_slug_work_status("one", Some(&same)).unwrap();
    assert_eq!(ids(&mut store).len(), 2);

    // A different gate cancels it; other watches are not the gate's.
    store
        .set_slug_work_status("one", Some(&status(Some("after #9"))))
        .unwrap();
    assert_eq!(ids(&mut store), std::slice::from_ref(&reminder.id));

    // Clearing the gate cancels it.
    let third = add(&mut store, "one", &["#9"], gate("after #9"));
    store
        .update_slug_work_status("one", |_| Ok::<_, ()>(Some(status(None))))
        .unwrap()
        .unwrap();
    assert_eq!(ids(&mut store), std::slice::from_ref(&reminder.id));

    // Clearing the status cancels it.
    store
        .set_slug_work_status("one", Some(&status(Some("after #9"))))
        .unwrap();
    add(&mut store, "one", &["#9"], gate("after #9"));
    assert!(store.clear_slug_work_status("one", None).unwrap());
    assert_eq!(ids(&mut store), std::slice::from_ref(&reminder.id));

    // So does a met watch not yet delivered, wake included: left behind,
    // it would clear a later gate that reads the same.
    let met_gate = |store: &mut Store| {
        store
            .set_slug_work_status("one", Some(&status(Some("after #9"))))
            .unwrap();
        let met = add(store, "one", &["#9"], gate("after #9"));
        let stamps = evaluate(&met, &pr_merged(9), NOON).stamps.unwrap();
        store.apply_evaluation(&[stamps]).unwrap();
        met
    };
    met_gate(&mut store);
    store
        .set_slug_work_status("one", Some(&status(None)))
        .unwrap();
    assert_eq!(ids(&mut store), std::slice::from_ref(&reminder.id));
    assert_eq!(changes(&store, "one").last(), Some(&WatchChange::Cancelled));

    // A build that cancels no watches cleared the gate, and it is set again
    // with the same words: the met watch witnessed the earlier gate.
    let met = met_gate(&mut store);
    let mut state = store.read_wt_state().unwrap();
    state["slugs"]["one"]["work"]
        .as_object_mut()
        .unwrap()
        .remove("blockedOn");
    store
        .write_repository_state_json(&state.to_string())
        .unwrap();
    assert_eq!(ids(&mut store), [reminder.id.clone(), met.id.clone()]);
    let mut again = status(Some("after #9"));
    again.note = Some("set again".into());
    let write = store
        .update_slug_work_status("one", |_| Ok::<_, ()>(Some(again)))
        .unwrap()
        .unwrap();
    assert_eq!(
        write
            .cancelled_watches
            .iter()
            .map(|watch| (watch.id.as_str(), watch.then.wake.as_deref()))
            .collect::<Vec<_>>(),
        [(met.id.as_str(), Some("go on"))]
    );
    assert_eq!(ids(&mut store), std::slice::from_ref(&reminder.id));
    store
        .set_slug_work_status("one", Some(&status(None)))
        .unwrap();

    // The explicit cancel takes the gate's watch whatever its state.
    assert_eq!(
        store
            .cancel_watches(WatchSelector::Gate { slug: "one" }, Some("one"))
            .unwrap(),
        []
    );
    store
        .set_slug_work_status("one", Some(&status(Some("g"))))
        .unwrap();
    let last = add(&mut store, "one", &["#9"], gate("g"));
    assert_eq!(
        store
            .cancel_watches(WatchSelector::Gate { slug: "one" }, Some("one"))
            .unwrap(),
        [last]
    );
    let _ = third;
}

#[test]
fn watches_are_cancelled_by_id_or_by_who_set_them() {
    let (_temp, mut store) = store();
    let mine = add(&mut store, "one", &["merged"], notify());
    let mut human = new("one", &["green"], notify());
    human.by = "human".into();
    let human = store.add_watch(human, NOON).unwrap().unwrap();
    let other = add(&mut store, "two", &["merged"], notify());
    assert_eq!(
        store
            .cancel_watches(
                WatchSelector::SetBy {
                    slug: "one",
                    by: "one"
                },
                Some("one")
            )
            .unwrap(),
        [mine]
    );
    let ids = [human.id.clone(), other.id.clone(), "nosuch".to_owned()];
    assert_eq!(
        store
            .cancel_watches(WatchSelector::Ids(&ids), None)
            .unwrap(),
        [human, other]
    );
    assert_eq!(
        store
            .cancel_watches(WatchSelector::Ids(&ids), None)
            .unwrap(),
        []
    );
    assert_eq!(
        changes(&store, "one"),
        [
            WatchChange::Set,
            WatchChange::Set,
            WatchChange::Cancelled,
            WatchChange::Cancelled
        ]
    );
}

#[test]
fn a_watch_does_not_pass_to_the_next_worktree_with_its_slug() {
    let (_temp, mut store) = store();
    let old = add(&mut store, "one", &["merged"], wake());
    let unrecorded = add(&mut store, "old", &["merged"], wake());
    // The same incarnation, and a worktree whose creation is still not
    // recorded, keep their watches.
    let mut same = live(&[("one", CREATED)]);
    same.insert("old".to_owned(), None);
    assert_eq!(
        store.drop_orphans(&same, &BTreeMap::new(), NOON).unwrap(),
        []
    );
    assert_eq!(store.list_watches(None).unwrap().len(), 2);

    // A worktree created later has a recorded creation: it is not the
    // unrecorded one, whatever the slug.
    let reborn = live(&[
        ("one", "2026-11-24T13:00:00Z"),
        ("old", "2026-11-24T13:00:00Z"),
    ]);
    let dropped = store.drop_orphans(&reborn, &BTreeMap::new(), NOON).unwrap();
    assert_eq!(dropped, [old, unrecorded]);
    assert_eq!(store.list_watches(None).unwrap(), []);
    // The new worktree's history does not report the old one's watch.
    assert_eq!(changes(&store, "one"), [WatchChange::Set]);
    assert_eq!(changes(&store, "old"), [WatchChange::Set]);
    assert_eq!(
        store.drop_orphans(&reborn, &BTreeMap::new(), NOON).unwrap(),
        []
    );
}

#[test]
fn removal_keeps_what_tells_the_person_about_a_landing() {
    let (_temp, mut store) = store();
    let tells = add(
        &mut store,
        "one",
        &["merged", "production+1h", "#7"],
        notify(),
    );
    let wakes = add(&mut store, "one", &["merged"], wake());
    let stuck = add(&mut store, "one", &["green"], notify());

    // Recording the landed removal holds the owner's own "merged" for good,
    // while the row may still be on the board.
    store
        .record_removed_worktrees(&[removed("one", "base")], NOON + 1_000)
        .unwrap();
    let after_record = store.list_watches(None).unwrap();
    assert_eq!(after_record.len(), 3);
    for watch in &after_record[..2] {
        assert!(watch.when[0].latched && !watch.owner_gone);
        assert_eq!(watch.when[0].first_held_at_ms, Some(NOON + 1_000));
    }
    assert!(!after_record[2].when[0].latched);

    // The row is gone: only the watch that tells the person goes on, on the
    // landing commit.
    let commits: BTreeMap<_, _> = [("one".to_owned(), SHA.to_owned())].into();
    let dropped = store
        .drop_orphans(&BTreeMap::new(), &commits, NOON + 2_000)
        .unwrap();
    assert_eq!(
        dropped.iter().map(|watch| &watch.id).collect::<Vec<_>>(),
        [&wakes.id, &stuck.id]
    );
    let kept = store.list_watches(None).unwrap();
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].id, tells.id);
    assert!(kept[0].owner_gone);
    assert_eq!(
        kept[0].when[1].cond,
        Condition::Commit {
            oid: SHA.into(),
            stage: VerifyStage::Production
        }
    );
    assert_eq!(kept[0].when[2], tells.when[2]);
    let reasons: Vec<_> = watch_events(&store, "one")
        .into_iter()
        .filter(|(change, ..)| *change == WatchChange::Dropped)
        .map(|(_, what, reason)| (what, reason.unwrap()))
        .collect();
    assert_eq!(
        reasons,
        [
            ("merged".to_owned(), "its worktree was removed".to_owned()),
            (
                "PR checks green".to_owned(),
                "its worktree was removed and can no longer become green".to_owned()
            ),
        ]
    );

    // It survives later passes and a new worktree taking the slug, and is
    // judged without the owner's row.
    let reborn = live(&[("one", "2026-11-25T00:00:00Z")]);
    assert_eq!(
        store.drop_orphans(&reborn, &BTreeMap::new(), NOON).unwrap(),
        []
    );
    let facts = WatchFacts {
        rows: [(
            "one".to_owned(),
            RowFacts {
                exists: true,
                ..RowFacts::default()
            },
        )]
        .into(),
        commits: [((SHA.to_owned(), VerifyStage::Production), true)].into(),
        ..pr_merged(7)
    };
    let at = NOON + 2 * 3_600_000;
    store
        .apply_evaluation(&[evaluate(&kept[0], &facts, at).stamps.unwrap()])
        .unwrap();
    let held = store.list_watches(None).unwrap().remove(0);
    store
        .apply_evaluation(&[evaluate(&held, &facts, at + 3_600_000).stamps.unwrap()])
        .unwrap();
    assert!(store.list_watches(None).unwrap()[0].is_met());
    assert!(
        store
            .complete_action(&tells.id, WatchAction::Notify)
            .unwrap()
    );
    assert_eq!(store.list_watches(None).unwrap(), []);
}

#[test]
fn a_removal_that_did_not_land_or_has_no_landing_commit_drops_the_watch() {
    let (_temp, mut store) = store();
    add(&mut store, "one", &["production"], notify());
    store.record_slug_created("two", CREATED).unwrap();
    add(&mut store, "two", &["merged"], notify());
    store.record_slug_created("three", CREATED).unwrap();
    let kept = add(&mut store, "three", &["merged"], notify());
    store
        .record_removed_worktrees(
            &[
                removed("one", "base"),
                removed("two", "unlanded"),
                removed("three", "production"),
            ],
            NOON + 1_000,
        )
        .unwrap();
    assert!(!store.list_watches(Some("two")).unwrap()[0].when[0].latched);
    let dropped = store
        .drop_orphans(&BTreeMap::new(), &BTreeMap::new(), NOON + 2_000)
        .unwrap();
    assert_eq!(
        dropped
            .iter()
            .map(|watch| watch.slug.as_str())
            .collect::<Vec<_>>(),
        ["one", "two"]
    );
    assert_eq!(
        watch_events(&store, "one").last().unwrap().2.as_deref(),
        Some("its worktree was removed and no landing commit is known")
    );
    assert_eq!(
        watch_events(&store, "two").last().unwrap().2.as_deref(),
        Some("its worktree was removed before landing")
    );
    // A landed "merged" needs no commit.
    let left = store.list_watches(None).unwrap();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].id, kept.id);
    assert!(left[0].owner_gone && left[0].when[0].latched);
}

#[test]
fn an_earlier_incarnations_removal_record_says_nothing_about_a_later_watch() {
    let (_temp, mut store) = store();
    // Removed and landed before the watch below was set.
    let mut earlier = removed("one", "base");
    earlier.removed_at = "2026-11-24T11:00:00Z".into();
    store
        .record_removed_worktrees(&[earlier.clone()], NOON - 3_600_000)
        .unwrap();
    add(&mut store, "one", &["merged"], notify());
    // The old record is written again, as a confirmation does.
    store
        .record_removed_worktrees(&[earlier], NOON + 1_000)
        .unwrap();
    assert!(!store.list_watches(None).unwrap()[0].when[0].latched);
    let dropped = store
        .drop_orphans(&BTreeMap::new(), &BTreeMap::new(), NOON + 2_000)
        .unwrap();
    assert_eq!(dropped.len(), 1);
}

#[test]
fn clearing_the_gate_is_one_write_and_a_changed_gate_takes_the_watch_with_it() {
    let (_temp, mut store) = store();
    store
        .set_slug_work_status("one", Some(&status(Some("after #7"))))
        .unwrap();
    let both = || {
        then(|then| {
            then.unblock = Some("after #7".into());
            then.wake = Some("test it".into());
        })
    };
    let gate = |store: &mut Store| {
        store.read_wt_state().unwrap()["slugs"]["one"]["work"]["blockedOn"]
            .as_str()
            .map(str::to_owned)
    };
    let watch = add(&mut store, "one", &["#7"], both());
    assert_eq!(crate::cleared_gate(&watch), None);
    // The gate is cleared with the record that it was, and the watch keeps
    // its wake through its own write.
    assert_eq!(
        store.deliver_unblock(&watch.id).unwrap(),
        UnblockDelivery::Cleared
    );
    assert_eq!(gate(&mut store), None);
    let left = store.list_watches(None).unwrap().remove(0);
    assert_eq!(crate::cleared_gate(&left), Some("after #7"));
    assert_eq!(
        left.then.remaining().collect::<Vec<_>>(),
        [WatchAction::Wake]
    );
    // A second delivery (a retried fire, or the pass that finds the fire
    // in the ledger) owes nothing, even with the same gate set again.
    store
        .set_slug_work_status("one", Some(&status(Some("after #7"))))
        .unwrap();
    assert_eq!(
        store.deliver_unblock(&watch.id).unwrap(),
        UnblockDelivery::NotOwed
    );
    assert_eq!(gate(&mut store).as_deref(), Some("after #7"));
    assert_eq!(
        store.deliver_unblock("nosuch").unwrap(),
        UnblockDelivery::NotOwed
    );
    store.complete_action(&watch.id, WatchAction::Wake).unwrap();

    // A build that cancels no watches rewrote the gate: it stays, and the
    // watch goes whole, because the gate its wake was to follow is gone.
    let watch = add(&mut store, "one", &["#7"], both());
    let mut state = store.read_wt_state().unwrap();
    state["slugs"]["one"]["work"]["blockedOn"] = "after #7 and #8".into();
    store
        .write_repository_state_json(&state.to_string())
        .unwrap();
    assert_eq!(
        store.deliver_unblock(&watch.id).unwrap(),
        UnblockDelivery::GateChanged
    );
    assert_eq!(gate(&mut store).as_deref(), Some("after #7 and #8"));
    assert_eq!(store.list_watches(None).unwrap(), []);
    assert_eq!(
        watch_events(&store, "one").last().unwrap(),
        &(
            WatchChange::Dropped,
            watch.summary(),
            Some("the gate it was to clear was cleared or changed".to_owned())
        )
    );
}

#[test]
fn setting_the_same_watch_again_returns_the_one_already_set() {
    let (_temp, mut store) = store();
    let first = add(&mut store, "one", &["#7", "17:00"], wake());
    assert_eq!(
        store
            .add_watch(new("one", &["pr:7:merged", "17:00"], wake()), NOON + 5)
            .unwrap(),
        Err(WatchRefusal::AlreadySet {
            watch: Box::new(first)
        })
    );
    // Another prompt, another condition or another setter is another watch.
    add(
        &mut store,
        "one",
        &["#7", "17:00"],
        then(|then| then.wake = Some("something else".into())),
    );
    add(&mut store, "one", &["#7"], wake());
    let managers = NewWatch {
        by: "manager".into(),
        ..new("one", &["#7", "17:00"], wake())
    };
    store.add_watch(managers, NOON).unwrap().unwrap();
    assert_eq!(store.list_watches(None).unwrap().len(), 4);
}

#[test]
fn a_gate_watch_set_again_without_a_wake_keeps_the_one_it_had() {
    let (_temp, mut store) = store();
    store
        .set_slug_work_status("one", Some(&status(Some("after #7"))))
        .unwrap();
    let gate = |wake: Option<&str>| {
        then(|then| {
            then.unblock = Some("after #7".into());
            then.wake = wake.map(str::to_owned);
        })
    };
    add(&mut store, "one", &["#7"], gate(Some("verify it")));
    let again = add(&mut store, "one", &["#7:production"], gate(None));
    assert_eq!(again.then.wake.as_deref(), Some("verify it"));
    // A wake given now replaces it.
    let replaced = add(&mut store, "one", &["#7:production"], gate(Some("other")));
    assert_eq!(replaced.then.wake.as_deref(), Some("other"));
    assert_eq!(store.list_watches(None).unwrap(), [replaced]);
}

#[test]
fn a_menu_sets_and_cancels_only_on_the_worktree_it_showed() {
    let (_temp, mut store) = store();
    let human = |conditions: &[&str]| NewWatch {
        by: "human".into(),
        ..new("one", conditions, notify())
    };
    // The menu showed another worktree with this slug, or one never recorded.
    for stale in [Some("2026-01-01T00:00:00Z"), None] {
        assert_eq!(
            store
                .add_watch_seen(human(&["ready"]), NOON, stale)
                .unwrap(),
            Err(WatchRefusal::OwnerChanged)
        );
    }
    let watch = store
        .add_watch_seen(human(&["ready"]), NOON, Some(CREATED))
        .unwrap()
        .unwrap();
    assert_eq!(watch.incarnation.as_deref(), Some(CREATED));
    assert_eq!(
        store
            .add_watch_seen(human(&["ready"]), NOON, Some(CREATED))
            .unwrap(),
        Err(WatchRefusal::AlreadySet {
            watch: Box::new(watch.clone())
        })
    );
    // An agent waiting for the same thing is not the person's watch.
    let agents = add(&mut store, "one", &["ready"], notify());
    for (id, by, incarnation) in [
        (&agents.id, "human", Some(CREATED)),
        (&watch.id, "one", Some(CREATED)),
        (&watch.id, "human", None),
    ] {
        assert_eq!(
            store.cancel_watch_seen(id, "one", by, incarnation).unwrap(),
            None
        );
    }
    let cancelled = store
        .cancel_watch_seen(&watch.id, "one", "human", Some(CREATED))
        .unwrap();
    assert_eq!(cancelled.map(|watch| watch.id), Some(watch.id.clone()));
    assert_eq!(
        store
            .cancel_watch_seen(&watch.id, "one", "human", Some(CREATED))
            .unwrap(),
        None
    );
    assert_eq!(
        changes(&store, "one"),
        [WatchChange::Set, WatchChange::Set, WatchChange::Cancelled]
    );
    // The person is not named, and the event says what the watch will do.
    let events = store.read_worktree_events("one", 500).unwrap();
    let set = events
        .iter()
        .find(|event| matches!(event.event, WorktreeEventKind::Watch { .. }))
        .unwrap();
    assert!(matches!(
        &set.event,
        WorktreeEventKind::Watch { then, by: None, .. } if then == &["notify".to_owned()]
    ));
}

/// The engine stamps a landed row's commit onto the watch that follows it.
/// A process started after that, which never saw the row landed, still
/// carries the watch past the worktree's removal.
#[test]
fn a_landing_commit_stamped_before_a_restart_carries_the_watch_past_removal() {
    use wt_core::watch::WatchStamps;
    let temp = tempdir().unwrap();
    let path = temp.path().join("wt.sqlite");
    let open = || Store::open(&path, RepositoryIdentity::new("repo", "/repo")).unwrap();
    let mut store = open();
    store.record_slug_created("one", CREATED).unwrap();
    let tells = add(&mut store, "one", &["production"], notify());
    let stamp = WatchStamps {
        landing_commit: Some(SHA.into()),
        evaluated_at_ms: Some(NOON),
        ..WatchStamps::new(&tells.id)
    };
    assert!(
        store
            .apply_evaluation(std::slice::from_ref(&stamp))
            .unwrap()
    );
    // Written only when it changes.
    assert!(!store.apply_evaluation(&[stamp]).unwrap());
    assert_eq!(raw(&store)["watches"][0]["landingCommit"], SHA);
    assert_eq!(raw(&store)["watches"][0]["evaluatedAtMs"], NOON);
    drop(store);

    let mut store = open();
    store
        .record_removed_worktrees(&[removed("one", "base")], NOON + 1_000)
        .unwrap();
    // This process knows no landing commits of its own.
    assert_eq!(
        store
            .drop_orphans(&BTreeMap::new(), &BTreeMap::new(), NOON + 2_000)
            .unwrap(),
        []
    );
    let kept = store.list_watches(None).unwrap();
    assert_eq!(kept.len(), 1);
    assert!(kept[0].owner_gone);
    assert_eq!(kept[0].evaluated_at_ms(), Some(NOON));
    assert_eq!(
        kept[0].when[0].cond,
        Condition::Commit {
            oid: SHA.into(),
            stage: VerifyStage::Production
        }
    );
}
