use rusqlite::Connection;
use serde_json::{Map, json};
use tempfile::tempdir;
use wt_core::{PrChange, WorkRisk, WorkState, WorktreeEvent, WorktreeEventKind};

use crate::{
    RemovedWorktree, RepositoryIdentity, Store, WORKTREE_EVENT_MAX_AGE_MS,
    WORKTREE_EVENTS_PER_SLUG, WorkStatusRecord,
};

fn identity() -> RepositoryIdentity {
    RepositoryIdentity::new("repo", "/repo")
}

fn status(state: &str) -> WorkStatusRecord {
    WorkStatusRecord {
        state: state.into(),
        at: "2026-11-01T00:00:00Z".into(),
        note: None,
        risk: None,
        sha: None,
        by: None,
        blocked_on: None,
        verify_after_merge: None,
        verify_in: None,
        extra: Map::new(),
    }
}

fn kinds(store: &Store, slug: &str) -> Vec<WorktreeEventKind> {
    store
        .read_worktree_events(slug, 500)
        .unwrap()
        .into_iter()
        .map(|event| event.event)
        .collect()
}

fn opened(number: u64) -> WorktreeEventKind {
    WorktreeEventKind::Pr {
        number,
        change: PrChange::Opened,
    }
}

/// The schema exactly as builds before the event log created it.
fn version_one_database(path: &std::path::Path) {
    let db = Connection::open(path).unwrap();
    db.execute_batch(
        "CREATE TABLE schema_migrations (version INTEGER PRIMARY KEY, applied_at INTEGER NOT NULL);
         CREATE TABLE repositories (
             repo_id TEXT PRIMARY KEY, repo_path TEXT NOT NULL UNIQUE,
             created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);
         CREATE TABLE repository_state (
             repo_id TEXT PRIMARY KEY REFERENCES repositories(repo_id) ON DELETE CASCADE,
             data TEXT NOT NULL, updated_at INTEGER NOT NULL);
         CREATE TABLE archived_worktrees (
             repo_id TEXT NOT NULL REFERENCES repositories(repo_id) ON DELETE CASCADE,
             worktree_key TEXT NOT NULL, archived_at INTEGER NOT NULL,
             PRIMARY KEY (repo_id, worktree_key));
         INSERT INTO schema_migrations VALUES (1, 1);
         INSERT INTO repositories VALUES ('repo', '/repo', 1, 1);",
    )
    .unwrap();
    db.execute(
        "INSERT INTO repository_state VALUES ('repo', ?1, 1)",
        [json!({"version": crate::CURRENT_WT_STATE_VERSION, "slugs": {"one": {"work": {"state": "working", "at": "1"}}}}).to_string()],
    )
    .unwrap();
}

#[test]
fn a_version_one_database_gains_the_log_and_still_opens_for_an_older_build() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("state.sqlite");
    version_one_database(&path);

    // Before any newer writer has opened it, a read-only open has no table
    // to read and cannot add one.
    let reader = Store::open_read_only(&path, identity()).unwrap();
    assert!(reader.read_worktree_events("one", 80).unwrap().is_empty());
    assert_eq!(reader.worktree_events_revision().unwrap(), 0);
    drop(reader);

    let mut store = Store::open(&path, identity()).unwrap();
    assert_eq!(
        store.read_wt_state().unwrap()["slugs"]["one"]["work"]["state"],
        "working",
        "the migration leaves existing state alone"
    );
    store
        .set_slug_work_status("one", Some(&status("ready")))
        .unwrap();
    assert_eq!(kinds(&store, "one").len(), 1);
    drop(store);

    // What a build that knows only version 1 does on open: it finds its
    // migration row, runs no DDL, and reads and writes the state row.
    let db = Connection::open(&path).unwrap();
    let applied: bool = db
        .query_row(
            "SELECT 1 FROM schema_migrations WHERE version = 1",
            [],
            |_| Ok(true),
        )
        .unwrap();
    assert!(applied);
    let newest: i64 = db
        .query_row("SELECT MAX(version) FROM schema_migrations", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert!(newest >= 1, "the older read-only check still passes");
    db.execute(
        "UPDATE repository_state SET data = ?1 WHERE repo_id = 'repo'",
        [json!({"version": crate::CURRENT_WT_STATE_VERSION, "slugs": {}}).to_string()],
    )
    .unwrap();
    drop(db);

    // Its write did not disturb the log, and a second migration is a no-op.
    let store = Store::open(&path, identity()).unwrap();
    assert_eq!(kinds(&store, "one").len(), 1);
    let versions: i64 = Connection::open(&path)
        .unwrap()
        .query_row("SELECT COUNT(*) FROM schema_migrations", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(versions, 2);
}

#[test]
fn a_status_change_is_one_event_and_a_note_only_reassertion_is_none() {
    let temp = tempdir().unwrap();
    let mut store = Store::open(temp.path().join("state.sqlite"), identity()).unwrap();
    let mut working = status("working");
    working.by = Some("one".into());
    working.note = Some("first line\nsecond line".into());
    assert!(store.set_slug_work_status("one", Some(&working)).unwrap());
    assert_eq!(
        kinds(&store, "one"),
        vec![WorktreeEventKind::Status {
            from: None,
            to: WorkState::Working,
            by: Some("one".into()),
            risk: None,
            note: Some("first line".into()),
            blocked_on: None,
            verify_in: None,
        }]
    );

    // Same claim: nothing is written at all.
    assert!(!store.set_slug_work_status("one", Some(&working)).unwrap());
    // A new note changes the stored record but is not a transition.
    let mut renoted = working.clone();
    renoted.note = Some("still at it".into());
    assert!(store.set_slug_work_status("one", Some(&renoted)).unwrap());
    assert_eq!(kinds(&store, "one").len(), 1);

    // Risk, the merge gate and the owed check each count without a state change.
    let mut ready = status("ready");
    ready.by = Some("human".into());
    ready.risk = Some("low".into());
    store
        .update_slug_work_status("one", |_| Ok::<_, ()>(Some(ready.clone())))
        .unwrap()
        .unwrap();
    let mut gated = ready.clone();
    gated.blocked_on = Some("deploy".into());
    store.set_slug_work_status("one", Some(&gated)).unwrap();
    let mut owed = gated.clone();
    owed.verify_after_merge = Some("check the page".into());
    owed.verify_in = Some("production".into());
    store.set_slug_work_status("one", Some(&owed)).unwrap();
    let events = kinds(&store, "one");
    assert_eq!(events.len(), 4);
    assert_eq!(
        events[1],
        WorktreeEventKind::Status {
            from: Some(WorkState::Working),
            to: WorkState::Ready,
            // The person in the TUI is recorded as no author.
            by: None,
            risk: Some(WorkRisk::Low),
            note: None,
            blocked_on: None,
            verify_in: None,
        }
    );
    assert!(matches!(
        &events[3],
        WorktreeEventKind::Status { blocked_on: Some(gate), verify_in: Some(stage), .. }
            if gate == "deploy" && stage == "production"
    ));

    assert!(
        store
            .clear_slug_work_status("one", Some("manager"))
            .unwrap()
    );
    assert!(
        !store
            .clear_slug_work_status("one", Some("manager"))
            .unwrap()
    );
    assert_eq!(
        kinds(&store, "one").last(),
        Some(&WorktreeEventKind::StatusCleared {
            from: Some(WorkState::Ready),
            by: Some("manager".into()),
        })
    );
    // The check the cleared status owed was dropped with it, and says so.
    assert_eq!(kinds(&store, "one").len(), 6);
}

#[test]
fn a_refused_status_update_records_nothing() {
    let temp = tempdir().unwrap();
    let mut store = Store::open(temp.path().join("state.sqlite"), identity()).unwrap();
    let refused = store
        .update_slug_work_status("one", |_| Err::<Option<WorkStatusRecord>, _>("stale"))
        .unwrap();
    assert_eq!(refused, Err("stale"));
    assert!(kinds(&store, "one").is_empty());
}

fn removal(slug: &str, removed_at: &str, landed: bool) -> RemovedWorktree {
    let mut extra = Map::new();
    if landed {
        extra.insert("landedOnAtRemoval".into(), json!("base"));
    }
    RemovedWorktree {
        slug: slug.into(),
        branch: format!("feature/{slug}"),
        removed_at: removed_at.into(),
        work: None,
        automations_paused: None,
        extra,
    }
}

#[test]
fn a_recreated_slug_starts_a_fresh_history_and_a_removed_one_keeps_its_last() {
    let temp = tempdir().unwrap();
    let mut store = Store::open(temp.path().join("state.sqlite"), identity()).unwrap();
    let now = crate::store::now_ms();
    let iso = |ms: i64| {
        time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000)
            .unwrap()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap()
    };
    store
        .set_slug_base("one", Some(("main", Some("abc"))))
        .unwrap();
    assert!(store.record_slug_created("one", &iso(now - 10)).unwrap());
    assert!(!store.record_slug_created("one", &iso(now)).unwrap());
    store
        .set_slug_work_status("one", Some(&status("working")))
        .unwrap();
    // The removal is recorded twice while it confirms; it is one event.
    store
        .record_removed_worktrees(&[removal("one", &iso(now), true)], now)
        .unwrap();
    store
        .record_removed_worktrees(&[removal("one", &iso(now), true)], now)
        .unwrap();
    let first = kinds(&store, "one");
    assert_eq!(
        first.first(),
        Some(&WorktreeEventKind::Created {
            base: Some("main".into())
        })
    );
    assert_eq!(
        first.last(),
        Some(&WorktreeEventKind::Removed { landed: true })
    );
    assert_eq!(first.len(), 3, "created, status, removed: {first:?}");

    // Creation resets the slug entry, then records the new incarnation.
    store.clear_slug_state("one").unwrap();
    assert_eq!(
        kinds(&store, "one"),
        first,
        "a removed slug reads as its last incarnation"
    );
    store.record_slug_created("one", &iso(now + 5)).unwrap();
    store
        .set_slug_work_status("one", Some(&status("review")))
        .unwrap();
    let second = kinds(&store, "one");
    assert_eq!(second.len(), 2, "{second:?}");
    assert_eq!(second[0], WorktreeEventKind::Created { base: None });
    assert!(matches!(
        second[1],
        WorktreeEventKind::Status {
            from: None,
            to: WorkState::Review,
            ..
        }
    ));
}

#[test]
fn an_incarnation_created_by_an_older_build_does_not_continue_the_previous_history() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("state.sqlite");
    let mut store = Store::open(&path, identity()).unwrap();
    let now = crate::store::now_ms();
    let iso = |ms: i64| {
        time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000)
            .unwrap()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap()
    };
    let at = |ms: i64| u64::try_from(ms).unwrap();
    // An earlier incarnation, created and removed a while ago.
    store
        .append_worktree_event(
            "one",
            at(now - 9_000),
            &WorktreeEventKind::Created { base: None },
        )
        .unwrap();
    store
        .append_worktree_event("one", at(now - 8_000), &opened(1))
        .unwrap();
    store
        .append_worktree_event(
            "one",
            at(now - 7_000),
            &WorktreeEventKind::Removed { landed: false },
        )
        .unwrap();
    // An older build recreates the slug: it writes createdAt and no event.
    let created = now - 5_000;
    Connection::open(&path)
        .unwrap()
        .execute(
            "INSERT OR REPLACE INTO repository_state VALUES ('repo', ?1, 1)",
            [json!({"version": crate::CURRENT_WT_STATE_VERSION, "slugs": {"one": {"createdAt": iso(created), "baseBranch": "main"}}}).to_string()],
        )
        .unwrap();
    // The host observes the new incarnation before any status is written.
    store
        .append_worktree_event("one", at(now - 3_000), &opened(2))
        .unwrap();
    store
        .set_slug_work_status("one", Some(&status("working")))
        .unwrap();
    let events = store.read_worktree_events("one", 80).unwrap();
    assert_eq!(
        events.iter().map(|event| &event.event).collect::<Vec<_>>(),
        [
            &WorktreeEventKind::Created {
                base: Some("main".into())
            },
            &opened(2),
            &WorktreeEventKind::Status {
                from: None,
                to: WorkState::Working,
                by: None,
                risk: None,
                note: None,
                blocked_on: None,
                verify_in: None,
            },
        ],
        "the late creation sorts before what was already observed, and after the earlier incarnation"
    );
    assert_eq!(events[0].at_ms, at(created), "the recorded creation time");
    // A later status write does not open the incarnation a second time.
    store
        .set_slug_work_status("one", Some(&status("ready")))
        .unwrap();
    assert_eq!(kinds(&store, "one").len(), 4);
    // Its removal is recorded although the log's previous lifecycle event
    // for the slug, before this incarnation, was already a removal.
    store
        .record_removed_worktrees(&[removal("one", &iso(now), false)], now)
        .unwrap();
    assert_eq!(
        kinds(&store, "one").last(),
        Some(&WorktreeEventKind::Removed { landed: false })
    );
    assert_eq!(kinds(&store, "one").len(), 5);
}

#[test]
fn a_worktree_that_predates_the_log_gains_its_creation_once_and_only_within_retention() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("state.sqlite");
    let mut store = Store::open(&path, identity()).unwrap();
    let now = crate::store::now_ms();
    let iso = |ms: i64| {
        time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000)
            .unwrap()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap()
    };
    Connection::open(&path)
        .unwrap()
        .execute(
            "INSERT INTO repository_state VALUES ('repo', ?1, 1)",
            [
                json!({"version": crate::CURRENT_WT_STATE_VERSION, "slugs": {
                    "recent": {"createdAt": iso(now - 60_000)},
                    "ancient": {"createdAt": iso(now - WORKTREE_EVENT_MAX_AGE_MS - 60_000)},
                }})
                .to_string(),
            ],
        )
        .unwrap();
    for slug in ["recent", "ancient"] {
        store
            .set_slug_work_status(slug, Some(&status("working")))
            .unwrap();
        store
            .set_slug_work_status(slug, Some(&status("ready")))
            .unwrap();
    }
    let recent = kinds(&store, "recent");
    assert_eq!(recent.len(), 3, "{recent:?}");
    assert_eq!(recent[0], WorktreeEventKind::Created { base: None });
    // Its creation is older than anything the log keeps.
    assert_eq!(kinds(&store, "ancient").len(), 2);
}

/// An observer's rule: record the PR it sees unless the history already
/// ends with it.
fn observe(number: u64) -> impl FnMut(&str, &[WorktreeEvent]) -> Vec<WorktreeEventKind> {
    move |_, history| {
        let known = history
            .iter()
            .rev()
            .find(|event| matches!(event.event, WorktreeEventKind::Pr { .. }))
            .is_some_and(|event| event.event == opened(number));
        if known {
            Vec::new()
        } else {
            vec![opened(number)]
        }
    }
}

#[test]
fn an_observation_is_recorded_once_across_a_restart_and_two_writers() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("state.sqlite");
    let slugs = vec!["one".to_owned(), "two".to_owned()];
    let mut first = Store::open(&path, identity()).unwrap();
    let mut second = Store::open(&path, identity()).unwrap();
    assert_eq!(
        first
            .reconcile_worktree_events(10, &slugs, observe(7))
            .unwrap(),
        2
    );
    assert_eq!(
        second
            .reconcile_worktree_events(11, &slugs, observe(7))
            .unwrap(),
        0
    );
    drop(first);
    // A restarted host has no memory of what it recorded.
    let mut restarted = Store::open(&path, identity()).unwrap();
    assert_eq!(
        restarted
            .reconcile_worktree_events(12, &slugs, observe(7))
            .unwrap(),
        0
    );
    assert_eq!(kinds(&restarted, "one"), vec![opened(7)]);
    assert_eq!(kinds(&restarted, "two"), vec![opened(7)]);

    // Both hosts decided to record from the same stale read. The decision
    // is taken again under the write lock, so the loser adds nothing.
    let revision = restarted.worktree_events_revision().unwrap();
    let mut raced = false;
    let appended = second
        .reconcile_worktree_events(13, &slugs[..1], |slug, history| {
            if !raced {
                raced = true;
                restarted
                    .reconcile_worktree_events(13, &["one".to_owned()], observe(8))
                    .unwrap();
            }
            observe(8)(slug, history)
        })
        .unwrap();
    assert_eq!(appended, 0);
    assert_eq!(kinds(&second, "one"), vec![opened(7), opened(8)]);
    assert!(second.worktree_events_revision().unwrap() > revision);
}

#[test]
fn an_unchanged_observation_does_not_write_to_the_database() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("state.sqlite");
    let mut store = Store::open(&path, identity()).unwrap();
    let slugs = vec!["one".to_owned()];
    store
        .reconcile_worktree_events(10, &slugs, observe(7))
        .unwrap();
    // Another connection holds the write lock; a no-op observation must not
    // wait for it, because every board change runs one.
    let blocker = Connection::open(&path).unwrap();
    blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
    let started = std::time::Instant::now();
    assert_eq!(
        store
            .reconcile_worktree_events(11, &slugs, observe(7))
            .unwrap(),
        0
    );
    assert!(started.elapsed() < std::time::Duration::from_secs(1));
    blocker.execute_batch("ROLLBACK").unwrap();
}

#[test]
fn appends_cap_each_slug_and_drop_events_past_the_retention_age() {
    let temp = tempdir().unwrap();
    let mut store = Store::open(temp.path().join("state.sqlite"), identity()).unwrap();
    let now = 10 * WORKTREE_EVENT_MAX_AGE_MS as u64;
    let cap = WORKTREE_EVENTS_PER_SLUG as u64;
    for number in 0..cap + 20 {
        store
            .append_worktree_event("busy", now + number, &opened(number))
            .unwrap();
    }
    store
        .append_worktree_event("quiet", now, &opened(1))
        .unwrap();
    let busy = store.read_worktree_events("busy", usize::MAX).unwrap();
    assert_eq!(busy.len() as u64, cap);
    assert_eq!(busy[0].event, opened(20), "the oldest are dropped first");
    assert_eq!(busy.last().unwrap().event, opened(cap + 19));
    assert_eq!(store.read_worktree_events("quiet", 80).unwrap().len(), 1);

    // An old event goes when its slug is next written.
    let old = now - WORKTREE_EVENT_MAX_AGE_MS as u64 - 1;
    store
        .append_worktree_event("aged", old, &opened(1))
        .unwrap();
    store
        .append_worktree_event("idle", old, &opened(1))
        .unwrap();
    store
        .append_worktree_event("aged", now, &opened(2))
        .unwrap();
    assert_eq!(kinds(&store, "aged"), vec![opened(2)]);
    // A slug that is never written again is reached by the sweep.
    assert_eq!(kinds(&store, "idle"), vec![opened(1)]);
    assert_eq!(store.prune_worktree_events(now).unwrap(), 1);
    assert!(kinds(&store, "idle").is_empty());
    assert_eq!(store.read_worktree_events("quiet", 80).unwrap().len(), 1);
}

#[test]
fn reads_return_the_newest_events_oldest_first_for_many_slugs_at_once() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("state.sqlite");
    let mut store = Store::open(&path, identity()).unwrap();
    let mut other = Store::open(&path, RepositoryIdentity::new("other", "/other")).unwrap();
    for number in 1..=5 {
        store
            .append_worktree_event("one", number, &opened(number))
            .unwrap();
    }
    store.append_worktree_event("two", 9, &opened(9)).unwrap();
    other.append_worktree_event("one", 1, &opened(99)).unwrap();
    let slugs = ["one", "two", "absent"].map(str::to_owned);
    let events = store.read_worktree_events_for(&slugs, 2).unwrap();
    assert_eq!(events.len(), 2, "a slug with no events is absent");
    assert_eq!(
        events["one"],
        vec![
            WorktreeEvent {
                at_ms: 4,
                event: opened(4)
            },
            WorktreeEvent {
                at_ms: 5,
                event: opened(5)
            },
        ]
    );
    assert_eq!(events["two"].len(), 1);
    // A row this build cannot parse is one unknown line, not an error.
    Connection::open(&path)
        .unwrap()
        .execute(
            "INSERT INTO worktree_events (repo_id, slug, at_ms, kind, data) VALUES ('repo', 'two', 10, 'teleported', '{\"kind\":\"teleported\"}')",
            [],
        )
        .unwrap();
    assert_eq!(
        kinds(&store, "two").last(),
        Some(&WorktreeEventKind::Unknown)
    );
}

fn iso(ms: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000)
        .unwrap()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap()
}

#[test]
fn a_history_write_that_fails_never_costs_the_state_write() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("state.sqlite");
    let mut store = Store::open(&path, identity()).unwrap();
    let now = crate::store::now_ms();
    store
        .set_slug_work_status("one", Some(&status("working")))
        .unwrap();
    // A later build changes the table under this still-running session.
    Connection::open(&path)
        .unwrap()
        .execute_batch("DROP TABLE worktree_events")
        .unwrap();

    assert!(
        store
            .set_slug_work_status("one", Some(&status("ready")))
            .unwrap()
    );
    let written = store
        .update_slug_work_status("one", |_| Ok::<_, ()>(Some(status("review"))))
        .unwrap()
        .unwrap();
    assert!(written.changed);
    assert!(store.record_slug_created("two", &iso(now)).unwrap());
    let mut owed = status("ready");
    owed.verify_after_merge = Some("check the page".into());
    store.set_slug_work_status("two", Some(&owed)).unwrap();
    store
        .record_removed_worktrees(&[removal("two", &iso(now), true)], now)
        .unwrap();
    assert!(matches!(
        store
            .settle_removed_verification("two", &status("verified"))
            .unwrap(),
        crate::RemovedSettle::Settled { .. }
    ));
    assert!(store.clear_slug_work_status("one", None).unwrap());

    let state = store.read_wt_state().unwrap();
    assert!(state["slugs"]["one"]["work"].is_null(), "the clear landed");
    assert_eq!(state["slugs"]["two"]["createdAt"], iso(now));
    let removed = store.read_removed_worktrees().unwrap();
    assert_eq!(
        removed.len(),
        1,
        "the removal record survives without its event"
    );
    assert_eq!(removed[0].work.as_ref().unwrap().state, "verified");
    assert!(
        kinds(&store, "one").is_empty(),
        "a missing table reads as no history"
    );
}

#[test]
fn a_failed_history_write_rolls_back_only_its_own_rows() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("state.sqlite");
    let mut store = Store::open(&path, identity()).unwrap();
    let now = crate::store::now_ms();
    store
        .append_worktree_event("one", u64::try_from(now).unwrap() - 10, &opened(1))
        .unwrap();
    // The status insert is refused after the backfilled creation went in.
    Connection::open(&path)
        .unwrap()
        .execute_batch(
            "INSERT INTO repository_state VALUES ('repo', '{\"version\":99,\"slugs\":{\"one\":{\"createdAt\":\"CREATED\"}}}', 1);
             CREATE TRIGGER refuse_status BEFORE INSERT ON worktree_events
             WHEN NEW.kind = 'status' BEGIN SELECT RAISE(ABORT, 'refused'); END;"
                .replace("CREATED", &iso(now - 1_000))
                .as_str(),
        )
        .unwrap();
    assert!(
        store
            .set_slug_work_status("one", Some(&status("ready")))
            .unwrap()
    );
    assert_eq!(
        store.read_wt_state().unwrap()["slugs"]["one"]["work"]["state"],
        "ready"
    );
    assert_eq!(
        kinds(&store, "one"),
        vec![opened(1)],
        "neither half of the refused history write stays"
    );
}

#[test]
fn what_happens_while_a_worktree_is_being_created_belongs_to_the_new_incarnation() {
    let temp = tempdir().unwrap();
    let mut store = Store::open(temp.path().join("state.sqlite"), identity()).unwrap();
    let now = crate::store::now_ms();
    let at = |ms: i64| u64::try_from(ms).unwrap();
    // The slug's earlier incarnation.
    store
        .append_worktree_event(
            "one",
            at(now - 9_000),
            &WorktreeEventKind::Created { base: None },
        )
        .unwrap();
    store
        .append_worktree_event(
            "one",
            at(now - 8_000),
            &WorktreeEventKind::Removed { landed: true },
        )
        .unwrap();
    // Creation begins: the slug entry is reset. The install then runs while
    // the row is already on the board.
    let began = now - 2_000;
    store.clear_slug_state("one").unwrap();
    store
        .set_slug_base("one", Some(("main", Some("abc"))))
        .unwrap();
    store
        .append_worktree_event("one", at(now - 1_000), &opened(7))
        .unwrap();
    let mut owed = status("ready");
    owed.verify_after_merge = Some("check the page".into());
    store.set_slug_work_status("one", Some(&owed)).unwrap();
    assert!(
        store
            .record_slug_created_since("one", &iso(now), began)
            .unwrap()
    );
    let events = store.read_worktree_events("one", 80).unwrap();
    assert_eq!(events.len(), 3, "{events:?}");
    assert_eq!(
        (events[0].at_ms, &events[0].event),
        (
            at(began),
            &WorktreeEventKind::Created {
                base: Some("main".into())
            }
        )
    );
    assert_eq!(events[1].event, opened(7));
    assert!(matches!(
        events[2].event,
        WorktreeEventKind::Status {
            to: WorkState::Ready,
            ..
        }
    ));
    // The creation is matched to this incarnation, not opened a second time,
    // although its event predates createdAt.
    store
        .set_slug_work_status("one", Some(&status("verified")))
        .unwrap();
    // The verified status and the check it closed.
    assert_eq!(kinds(&store, "one").len(), 5);
    assert_eq!(
        kinds(&store, "one")
            .iter()
            .filter(|event| matches!(event, WorktreeEventKind::Created { .. }))
            .count(),
        1
    );
}

#[test]
fn stored_text_and_check_names_are_bounded() {
    let temp = tempdir().unwrap();
    let mut store = Store::open(temp.path().join("state.sqlite"), identity()).unwrap();
    let long = "x".repeat(4_000);
    let mut gated = status("ready");
    gated.by = Some(long.clone());
    gated.blocked_on = Some(format!("{long}\nsecond line"));
    gated.verify_after_merge = Some("steps".into());
    gated.verify_in = Some(long.clone());
    store.set_slug_work_status("one", Some(&gated)).unwrap();
    store
        .append_worktree_event(
            "one",
            u64::try_from(crate::store::now_ms()).unwrap(),
            &WorktreeEventKind::Checks {
                state: wt_core::ChecksChange::Failing,
                failed: (1..=9).map(|n| format!("check-{n}-{long}")).collect(),
            },
        )
        .unwrap();
    let now = u64::try_from(crate::store::now_ms()).unwrap();
    store
        .append_worktree_event(
            "one",
            now,
            &WorktreeEventKind::Automation {
                rule: long.clone(),
                outcome: wt_core::AutomationOutcomeKind::Failed,
                reason: Some(long.clone()),
            },
        )
        .unwrap();
    store
        .append_worktree_event(
            "one",
            now,
            &WorktreeEventKind::Restack {
                ok: true,
                onto: Some(long.clone()),
            },
        )
        .unwrap();
    let stored = serde_json::to_string(&store.read_worktree_events("one", 80).unwrap()).unwrap();
    let longest = stored
        .split('"')
        .map(|part| part.chars().count())
        .max()
        .unwrap();
    assert!(
        longest <= wt_core::EVENT_TEXT_MAX,
        "a {longest}-character string was stored"
    );
    let events = kinds(&store, "one");
    let WorktreeEventKind::Checks { failed, .. } = &events[1] else {
        panic!("{events:?}");
    };
    assert_eq!(failed.len(), crate::WORKTREE_EVENT_CHECKS_MAX + 1);
    assert_eq!(failed.last().unwrap(), "+3 more");
    // Bounding what was already bounded changes nothing.
    let mut again = events[1].clone();
    crate::bound_worktree_event(&mut again);
    assert_eq!(again, events[1]);
}

#[test]
fn a_slug_at_its_cap_is_not_given_a_backdated_creation() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("state.sqlite");
    let mut store = Store::open(&path, identity()).unwrap();
    let now = crate::store::now_ms();
    for number in 0..WORKTREE_EVENTS_PER_SLUG {
        store
            .append_worktree_event(
                "one",
                u64::try_from(now - 1_000 + number).unwrap(),
                &opened(1),
            )
            .unwrap();
    }
    Connection::open(&path)
        .unwrap()
        .execute(
            "INSERT INTO repository_state VALUES ('repo', ?1, 1)",
            [json!({"version": crate::CURRENT_WT_STATE_VERSION, "slugs": {"one": {"createdAt": iso(now - 60_000)}}}).to_string()],
        )
        .unwrap();
    let before = store.worktree_events_revision().unwrap();
    store
        .set_slug_work_status("one", Some(&status("working")))
        .unwrap();
    assert_eq!(
        store.worktree_events_revision().unwrap(),
        before + 1,
        "only the status was inserted"
    );
    let events = kinds(&store, "one");
    assert_eq!(events.len() as i64, WORKTREE_EVENTS_PER_SLUG);
    assert!(matches!(
        events.last(),
        Some(WorktreeEventKind::Status { .. })
    ));
}

#[test]
fn settling_a_removed_rows_check_never_writes_into_a_recreated_slugs_history() {
    let temp = tempdir().unwrap();
    let mut store = Store::open(temp.path().join("state.sqlite"), identity()).unwrap();
    let now = crate::store::now_ms();
    let mut owed = status("ready");
    owed.verify_after_merge = Some("check the page".into());
    for slug in ["gone", "back"] {
        store.record_slug_created(slug, &iso(now - 5_000)).unwrap();
        store.set_slug_work_status(slug, Some(&owed)).unwrap();
        store
            .record_removed_worktrees(&[removal(slug, &iso(now - 4_000), true)], now)
            .unwrap();
    }
    // "back" is created again while its removed row still owes the check.
    // Creation normally drops that row; a failed restore can leave both.
    store.clear_slug_state("back").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    store
        .record_slug_created("back", &iso(crate::store::now_ms()))
        .unwrap();
    for slug in ["gone", "back"] {
        assert!(matches!(
            store
                .settle_removed_verification(slug, &status("verified"))
                .unwrap(),
            crate::RemovedSettle::Settled { .. }
        ));
    }
    assert!(matches!(
        kinds(&store, "gone").last(),
        Some(WorktreeEventKind::Status {
            to: WorkState::Verified,
            ..
        })
    ));
    assert_eq!(
        kinds(&store, "back"),
        vec![WorktreeEventKind::Created { base: None }],
        "the live worktree's history says nothing about the removed one's check"
    );
}

#[test]
fn a_status_decision_reads_the_status_under_the_write_lock_and_dates_events_in_order() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("state.sqlite");
    let mut first = Store::open(&path, identity()).unwrap();
    let mut second = Store::open(&path, identity()).unwrap();
    let slugs = vec!["one".to_owned()];
    let base = u64::try_from(crate::store::now_ms()).unwrap() - 10_000;
    // Record the stored state unless the log's last status already says it.
    fn decide(
        events: &[WorktreeEvent],
        work: Option<&WorkStatusRecord>,
        at: u64,
    ) -> Vec<(Option<u64>, WorktreeEventKind)> {
        let logged = events.iter().rev().find_map(|event| match &event.event {
            WorktreeEventKind::Status { to, .. } => Some(*to),
            _ => None,
        });
        match work.and_then(|work| crate::work_status_event(logged, work)) {
            Some(WorktreeEventKind::Status { to, .. }) if Some(to) == logged => Vec::new(),
            Some(event) => vec![(Some(at), event)],
            None => Vec::new(),
        }
    }
    let state = |work: &str| {
        json!({"version": crate::CURRENT_WT_STATE_VERSION, "slugs": {"one": {"work": {"state": work, "at": "1"}}}})
            .to_string()
    };
    // An older build wrote the status: state only.
    first.write_repository_state_json(&state("ready")).unwrap();
    first
        .append_worktree_event("one", base + 1_000, &opened(1))
        .unwrap();
    // Two hosts decide from the same read; the loser decides again under
    // the lock and finds the event there.
    let mut raced = false;
    let appended = first
        .reconcile_worktree_history(base + 5_000, &slugs, |_, events, work| {
            if !raced {
                raced = true;
                let won = second
                    .reconcile_worktree_history(
                        base + 5_000,
                        &["one".to_owned()],
                        |_, events, work| decide(events, work, base + 400),
                    )
                    .unwrap();
                assert_eq!(won, 1);
            }
            decide(events, work, base + 400)
        })
        .unwrap();
    assert_eq!(appended, 0);
    let events = first.read_worktree_events("one", 80).unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(
        events[1].at_ms,
        base + 1_000,
        "its own time is earlier than the history it follows, so it takes the later one"
    );
    // This build then writes a status with its event; an observer holding
    // the earlier status must decide against what is stored now.
    second
        .set_slug_work_status("one", Some(&status("review")))
        .unwrap();
    let again = first
        .reconcile_worktree_history(base + 20_000, &slugs, |_, events, work| {
            decide(events, work, base + 400)
        })
        .unwrap();
    assert_eq!(again, 0);
    assert_eq!(kinds(&first, "one").len(), 3);
    // An event dated in the future is recorded as of now.
    second
        .write_repository_state_json(&state("verified"))
        .unwrap();
    let now = base + 30_000;
    first
        .reconcile_worktree_history(now, &slugs, |_, events, work| decide(events, work, now * 2))
        .unwrap();
    assert_eq!(first.read_worktree_events("one", 80).unwrap()[3].at_ms, now);
}

#[test]
fn an_incarnation_never_includes_lifecycle_events_inserted_before_its_creation() {
    let temp = tempdir().unwrap();
    let mut store = Store::open(temp.path().join("state.sqlite"), identity()).unwrap();
    let t = u64::try_from(crate::store::now_ms()).unwrap() - 60_000;
    let created = WorktreeEventKind::Created { base: None };
    let removed = WorktreeEventKind::Removed { landed: true };
    // The new creation is dated in the same millisecond as the previous
    // removal ("same"), before it ("earlier"), and after it ("later").
    for (slug, recreated_at) in [("same", t + 10), ("earlier", t + 5), ("later", t + 15)] {
        let mut add = |at: u64, event: &WorktreeEventKind| {
            store.append_worktree_event(slug, at, event).unwrap();
        };
        add(t, &created);
        add(t + 1, &opened(1));
        add(t + 8, &opened(2));
        add(t + 10, &removed);
        add(recreated_at, &created);
        add(t + 20, &opened(3));
    }
    let slugs = ["same", "earlier", "later"].map(str::to_owned);
    let expected = vec![created.clone(), opened(3)];
    // The board's read of many slugs, the single read behind wt history,
    // and the read a reconcile decides against all agree.
    let many = store.read_worktree_events_for(&slugs, 80).unwrap();
    for slug in &slugs {
        let many: Vec<_> = many[slug].iter().map(|event| event.event.clone()).collect();
        assert_eq!(many, expected, "{slug}");
        assert_eq!(kinds(&store, slug), expected, "{slug}");
    }
    let mut decided_against = Vec::new();
    store
        .reconcile_worktree_events(t + 30, &slugs, |_, history| {
            decided_against.push(
                history
                    .iter()
                    .map(|event| event.event.clone())
                    .collect::<Vec<_>>(),
            );
            Vec::new()
        })
        .unwrap();
    assert_eq!(decided_against, vec![expected.clone(); 3]);

    // A removal is the last word of its own incarnation even when its clock
    // reads earlier than the creation's, and is recorded once.
    let now = crate::store::now_ms();
    store
        .record_removed_worktrees(&[removal("earlier", &iso(now), false)], now)
        .unwrap();
    store
        .append_worktree_event("same", t + 9, &WorktreeEventKind::Removed { landed: false })
        .unwrap();
    assert_eq!(
        kinds(&store, "same"),
        vec![
            WorktreeEventKind::Removed { landed: false },
            created.clone(),
            opened(3)
        ],
        "inserted after the creation, so it is this incarnation's"
    );
    assert_eq!(
        kinds(&store, "earlier").last(),
        Some(&WorktreeEventKind::Removed { landed: false })
    );
    assert_eq!(kinds(&store, "earlier").len(), 3);

    // A creation recorded late still comes before what was observed while
    // the worktree was being created.
    store
        .append_worktree_event("late", t + 50, &opened(9))
        .unwrap();
    store
        .append_worktree_event("late", t + 40, &created)
        .unwrap();
    assert_eq!(kinds(&store, "late"), vec![created, opened(9)]);
}
