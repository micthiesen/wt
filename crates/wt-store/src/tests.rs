use std::{collections::BTreeSet, fs, thread, time::Duration};

use rusqlite::Connection;
use serde_json::{Map, json};
use tempfile::tempdir;

use crate::{
    CURRENT_WT_STATE_VERSION, LayoutChanged, MergeEdge, RemovedWorktree, RepositoryIdentity,
    SeenToggle, ShownSection, ShownUnit, Store, StoreError, TitleSource, TitleWrite,
    WorkStatusRecord, WtState, migrate_wt_state,
};

fn identity(id: &str, path: &str) -> RepositoryIdentity {
    RepositoryIdentity::new(id, path)
}

#[test]
fn removal_survives_restart_and_only_its_current_job_can_finish() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("state.sqlite");
    let mut store = Store::open(&path, identity("repo", "/repo")).unwrap();
    assert!(
        store
            .begin_removal("one", "first", "/one", "feature/one")
            .unwrap()
    );
    assert!(
        !store
            .begin_removal("one", "duplicate", "/one", "feature/one")
            .unwrap()
    );
    store
        .finish_removal("one", "first", Some("permission denied"))
        .unwrap();
    drop(store);

    let mut store = Store::open(&path, identity("repo", "/repo")).unwrap();
    store.reap_wt_state(&BTreeSet::new()).unwrap();
    assert_eq!(
        store.read_wt_state().unwrap()["removals"]["one"]["error"],
        "permission denied"
    );
    assert!(
        store
            .begin_removal("one", "retry", "/one", "feature/one")
            .unwrap()
    );
    assert!(!store.finish_removal("one", "first", None).unwrap());
    assert!(
        !store
            .finish_removal("one", "first", Some("old error"))
            .unwrap()
    );
    assert!(store.read_wt_state().unwrap()["removals"]["one"]["error"].is_null());
    assert!(store.finish_removal("one", "retry", None).unwrap());
    assert!(store.read_wt_state().unwrap()["removals"]["one"].is_null());
}

#[test]
fn failed_removal_clears_only_for_the_reviewed_job_while_its_checkout_is_absent() {
    use crate::ClearFailedRemoval::{Changed, CheckoutPresent, Cleared};
    let temp = tempdir().unwrap();
    let mut store =
        Store::open(temp.path().join("state.sqlite"), identity("repo", "/repo")).unwrap();
    assert!(
        store
            .begin_removal("one", "first", "/one", "feature/one")
            .unwrap()
    );
    let entry = |store: &mut Store| store.read_wt_state().unwrap()["removals"]["one"].clone();
    // An active attempt is never dismissed.
    let clear = |store: &mut Store, job: &str, path: &str, branch: &str, absent: bool| {
        store
            .clear_failed_removal("one", job, path, branch, || absent)
            .unwrap()
    };
    assert_eq!(
        clear(&mut store, "first", "/one", "feature/one", true),
        Changed
    );
    assert!(entry(&mut store)["error"].is_null());
    store
        .finish_removal("one", "first", Some("permission denied"))
        .unwrap();
    // Another job, path or branch than the one reviewed writes nothing.
    assert_eq!(
        clear(&mut store, "older", "/one", "feature/one", true),
        Changed
    );
    assert_eq!(
        clear(&mut store, "first", "/two", "feature/one", true),
        Changed
    );
    assert_eq!(
        clear(&mut store, "first", "/one", "feature/two", true),
        Changed
    );
    // A checkout that exists again keeps the record for a reviewed retry.
    assert_eq!(
        clear(&mut store, "first", "/one", "feature/one", false),
        CheckoutPresent
    );
    assert_eq!(entry(&mut store)["error"], "permission denied");
    assert_eq!(
        clear(&mut store, "first", "/one", "feature/one", true),
        Cleared
    );
    assert!(entry(&mut store).is_null());
    assert_eq!(
        clear(&mut store, "first", "/one", "feature/one", true),
        Changed
    );
}

#[test]
fn fork_base_write_checks_fresh_anchor_and_cycle_in_one_transaction() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("state.sqlite");
    let mut first = Store::open(&path, identity("repo", "/repo")).unwrap();
    let mut second = Store::open(&path, identity("repo", "/repo")).unwrap();
    let branches = [
        ("feature/one".into(), "one".into()),
        ("feature/two".into(), "two".into()),
    ]
    .into();
    assert_eq!(
        first
            .set_slug_base_checked("one", (None, None), "feature/two", "parent-tip", &branches)
            .unwrap(),
        crate::BaseUpdate::Updated
    );
    assert_eq!(
        second
            .set_slug_base_checked("two", (None, None), "feature/one", "other-tip", &branches)
            .unwrap(),
        crate::BaseUpdate::Cycle
    );
    assert_eq!(
        second
            .set_slug_base_checked("one", (None, None), "main", "stale", &branches)
            .unwrap(),
        crate::BaseUpdate::Stale
    );
    assert_eq!(
        first.read_slug_state("one").unwrap().unwrap()["baseSha"],
        "parent-tip"
    );
    assert!(second.read_slug_state("two").unwrap().is_none());
}

#[test]
fn archive_changes_are_independent_idempotent_and_repository_scoped() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("state.sqlite");
    let mut first = Store::open(&path, identity("repo-a", "/a")).unwrap();
    let mut second = Store::open(&path, identity("repo-a", "/a")).unwrap();
    let mut foreign = Store::open(&path, identity("repo-b", "/b")).unwrap();
    assert!(first.set_archived("one", true).unwrap());
    assert!(second.set_archived("two", true).unwrap());
    assert!(!first.set_archived("one", true).unwrap());
    assert!(foreign.set_archived("one", true).unwrap());
    assert!(first.set_archived("one", false).unwrap());
    assert!(!first.set_archived("one", false).unwrap());
    assert_eq!(
        second.read_archived_keys().unwrap(),
        BTreeSet::from(["two".into()])
    );
    assert_eq!(
        foreign.read_archived_keys().unwrap(),
        BTreeSet::from(["one".into()])
    );
    assert_eq!(first.reap_archived_keys(&BTreeSet::new()).unwrap(), 1);
    assert_eq!(
        foreign.read_archived_keys().unwrap(),
        BTreeSet::from(["one".into()])
    );
}

#[test]
fn legacy_schema_is_created_and_state_round_trips_losslessly() {
    let temp = tempdir().unwrap();
    let db_path = temp.path().join("state/wt.sqlite");
    let mut store = Store::open(&db_path, identity("repo-a", "/repos/a")).unwrap();
    let state = json!({
        "version": 17,
        "slugs": {"a": {"section": null, "order": 1, "newField": [1, 2]}},
        "unknownFutureRoot": {"nested": true}
    });
    let encoded = serde_json::to_string(&state).unwrap();
    store.write_repository_state_json(&encoded).unwrap();
    assert_eq!(
        store.read_repository_state_json().unwrap().as_deref(),
        Some(encoded.as_str())
    );
    assert!(store.has_repository_state().unwrap());
}

#[test]
fn repositories_are_isolated_and_identity_collisions_fail_closed() {
    let temp = tempdir().unwrap();
    let db_path = temp.path().join("wt.sqlite");
    let mut one = Store::open(&db_path, identity("repo-a", "/repos/a")).unwrap();
    let mut two = Store::open(&db_path, identity("repo-b", "/repos/b")).unwrap();
    one.write_repository_state_json(r#"{"slugs":{"same":{"section":"one"}}}"#)
        .unwrap();
    two.write_repository_state_json(r#"{"slugs":{"same":{"section":"two"}}}"#)
        .unwrap();
    assert!(
        one.read_repository_state_json()
            .unwrap()
            .unwrap()
            .contains("one")
    );
    assert!(
        two.read_repository_state_json()
            .unwrap()
            .unwrap()
            .contains("two")
    );

    assert!(matches!(
        Store::open(&db_path, identity("repo-a", "/repos/other")),
        Err(StoreError::RepositoryCollision { .. })
    ));
    assert!(matches!(
        Store::open(&db_path, identity("repo-c", "/repos/a")),
        Err(StoreError::PathCollision(_))
    ));
}

#[test]
fn concurrent_opens_serialize_schema_initialization_and_registration() {
    let temp = tempdir().unwrap();
    let db_path = temp.path().join("wt.sqlite");
    let workers = 8;
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(workers));
    let joins = (0..workers)
        .map(|_| {
            let barrier = barrier.clone();
            let db_path = db_path.clone();
            thread::spawn(move || {
                barrier.wait();
                // The schema race happens on the first open. More rounds only
                // add contention, and SQLite's busy wait is not fair: on a
                // loaded runner one opener can starve past its 3s timeout.
                for _ in 0..10 {
                    drop(Store::open(&db_path, identity("shared", "/repos/shared")).unwrap());
                }
            })
        })
        .collect::<Vec<_>>();
    for join in joins {
        join.join().unwrap();
    }
    let store = Store::open_read_only(&db_path, identity("shared", "/repos/shared")).unwrap();
    assert!(!store.has_repository_state().unwrap());
}

#[test]
fn readonly_open_does_not_create_or_touch_current_database() {
    let temp = tempdir().unwrap();
    let missing = temp.path().join("missing/wt.sqlite");
    let readonly = Store::open_read_only(&missing, identity("repo-a", "/repos/a")).unwrap();
    assert_eq!(readonly.read_repository_state_json().unwrap(), None);
    assert!(!missing.exists());

    let db_path = temp.path().join("existing.sqlite");
    let mut writer = Store::open(&db_path, identity("repo-a", "/repos/a")).unwrap();
    writer.write_repository_state_json("{}").unwrap();
    let before: i64 = Connection::open(&db_path)
        .unwrap()
        .query_row(
            "SELECT updated_at FROM repositories WHERE repo_id = 'repo-a'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    thread::sleep(Duration::from_millis(2));
    let readonly = Store::open_read_only(&db_path, identity("repo-a", "/repos/a")).unwrap();
    assert_eq!(
        readonly.read_repository_state_json().unwrap().as_deref(),
        Some("{}")
    );
    assert!(matches!(readonly.read_repository_state_json(), Ok(Some(_))));
    let after: i64 = Connection::open(&db_path)
        .unwrap()
        .query_row(
            "SELECT updated_at FROM repositories WHERE repo_id = 'repo-a'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(before, after);
    assert!(matches!(
        Store::open_read_only(&db_path, identity("repo-a", "/repos/wrong")),
        Err(StoreError::RepositoryCollision { .. })
    ));
}

#[test]
fn current_state_migration_check_does_not_touch_repository_timestamps() {
    let temp = tempdir().unwrap();
    let db_path = temp.path().join("wt.sqlite");
    let mut store = Store::open(&db_path, identity("repo-a", "/repos/a")).unwrap();
    let current = format!(r#"{{"version":{CURRENT_WT_STATE_VERSION},"keep":true}}"#);
    store.write_repository_state_json(&current).unwrap();
    let before: i64 = Connection::open(&db_path)
        .unwrap()
        .query_row(
            "SELECT updated_at FROM repositories WHERE repo_id = 'repo-a'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    thread::sleep(Duration::from_millis(2));
    assert_eq!(
        store
            .read_repository_state_json_migrated()
            .unwrap()
            .as_deref(),
        Some(current.as_str())
    );
    let after: i64 = Connection::open(&db_path)
        .unwrap()
        .query_row(
            "SELECT updated_at FROM repositories WHERE repo_id = 'repo-a'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(before, after);
}

#[test]
fn archive_writes_replace_and_import_is_transactional() {
    let temp = tempdir().unwrap();
    let db_path = temp.path().join("wt.sqlite");
    let mut store = Store::open(&db_path, identity("repo-a", "/repos/a")).unwrap();
    let initial = BTreeSet::from(["old".to_owned()]);
    store.replace_archived_keys(&initial).unwrap();
    assert_eq!(store.read_archived_keys().unwrap(), initial);

    let external = Connection::open(&db_path).unwrap();
    external
        .execute_batch(
            "CREATE TRIGGER reject_archive BEFORE INSERT ON archived_worktrees \
             BEGIN SELECT RAISE(ABORT, 'fixture failure'); END;",
        )
        .unwrap();
    let new_archives = BTreeSet::from(["new".to_owned()]);
    assert!(
        store
            .import_repository_snapshot(r#"{"replacement":true}"#, &new_archives)
            .is_err()
    );
    assert_eq!(store.read_repository_state_json().unwrap(), None);
    assert_eq!(store.read_archived_keys().unwrap(), initial);
    assert!(
        store
            .merge_repository_snapshot(&new_archives, |current| {
                assert!(current.is_none());
                Ok(json!({"replacement": true}))
            })
            .is_err()
    );
    assert_eq!(store.read_repository_state_json().unwrap(), None);
    assert_eq!(store.read_archived_keys().unwrap(), initial);
}

#[test]
fn snapshot_merge_reads_under_the_write_lock_and_preserves_concurrent_imports() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("wt.sqlite");
    let mut reader = Store::open(&path, identity("repo", "/repos/one")).unwrap();
    reader
        .import_repository_snapshot(
            &json!({"future": 42, "counter": 0}).to_string(),
            &BTreeSet::new(),
        )
        .unwrap();
    let writers = (0..4)
        .map(|_| Store::open(&path, identity("repo", "/repos/one")).unwrap())
        .collect::<Vec<_>>();
    let start = std::sync::Arc::new(std::sync::Barrier::new(writers.len()));
    let tasks = writers
        .into_iter()
        .enumerate()
        .map(|(index, mut store)| {
            let start = start.clone();
            thread::spawn(move || {
                start.wait();
                for revision in 0..20 {
                    let key = format!("{index}-{revision}");
                    store
                        .merge_repository_snapshot(&BTreeSet::from([key.clone()]), |state| {
                            let mut state = state.unwrap();
                            state["counter"] = json!(state["counter"].as_u64().unwrap() + 1);
                            state["slugs"][&key] = json!({"imported": true});
                            Ok(state)
                        })
                        .unwrap();
                }
            })
        })
        .collect::<Vec<_>>();
    for task in tasks {
        task.join().unwrap();
    }
    let state = reader.read_wt_state().unwrap();
    assert_eq!(state["counter"], 80);
    assert_eq!(state["future"], 42);
    assert_eq!(state["slugs"].as_object().unwrap().len(), 80);
    assert_eq!(reader.read_archived_keys().unwrap().len(), 80);
}

#[test]
fn concurrent_repository_updates_keep_each_snapshot_and_archive_atomic() {
    let temp = tempdir().unwrap();
    let db_path = temp.path().join("wt.sqlite");
    let _seed = Store::open(&db_path, identity("seed", "/repos/seed")).unwrap();
    let mut writers = Vec::new();
    for index in 0..4 {
        writers.push(
            Store::open(
                &db_path,
                identity(&format!("repo-{index}"), &format!("/repos/{index}")),
            )
            .unwrap(),
        );
    }
    let joins = writers
        .into_iter()
        .enumerate()
        .map(|(index, mut store)| {
            thread::spawn(move || {
                let mut keys = BTreeSet::new();
                for revision in 0..20 {
                    let key = format!("{index}-{revision}");
                    keys.insert(key);
                    let state = json!({"writer": index, "revision": revision});
                    store
                        .import_repository_snapshot(&state.to_string(), &keys)
                        .unwrap();
                }
            })
        })
        .collect::<Vec<_>>();
    for join in joins {
        join.join().unwrap();
    }
    for index in 0..4 {
        let store = Store::open_read_only(
            &db_path,
            identity(&format!("repo-{index}"), &format!("/repos/{index}")),
        )
        .unwrap();
        let state: serde_json::Value =
            serde_json::from_str(&store.read_repository_state_json().unwrap().unwrap()).unwrap();
        assert_eq!(state["writer"], index);
        assert_eq!(state["revision"], 19);
        assert_eq!(store.read_archived_keys().unwrap().len(), 20);
    }
}

#[test]
fn wtstate_migrations_preserve_unknown_fields_and_do_not_downgrade() {
    let migrated = migrate_wt_state(json!({
        "version": 15,
        "slugs": {"task": {"baseSha": "abc", "future": 7}},
        "futureRoot": {"value": "kept"}
    }));
    assert_eq!(migrated.from, 15);
    assert_eq!(migrated.to, CURRENT_WT_STATE_VERSION);
    assert_eq!(migrated.value["version"], CURRENT_WT_STATE_VERSION);
    assert_eq!(migrated.value["slugs"]["task"]["future"], 7);
    assert_eq!(migrated.value["futureRoot"]["value"], "kept");
    assert_eq!(migrated.value["reviewRequestDismissals"], json!([]));

    let newer = json!({"version": 22, "future": {"field": true}});
    let result = migrate_wt_state(newer.clone());
    assert_eq!(result.from, 22);
    assert_eq!(result.to, 22);
    assert_eq!(result.value, newer);
}

#[test]
fn first_read_migrates_old_payload_once_but_preserves_newer_payloads() {
    let temp = tempdir().unwrap();
    let db_path = temp.path().join("wt.sqlite");
    let mut store = Store::open(&db_path, identity("repo-a", "/repos/a")).unwrap();
    store
        .write_repository_state_json(r#"{"version":15,"unknown":{"x":1}}"#)
        .unwrap();
    let migrated: serde_json::Value = serde_json::from_str(
        &store
            .read_repository_state_json_migrated()
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(migrated["version"], CURRENT_WT_STATE_VERSION);
    assert_eq!(migrated["reviewRequestDismissals"], json!([]));
    assert_eq!(migrated["unknown"]["x"], 1);

    let newer = r#"{"version":99,"unknownFuture":{"keep":true}}"#;
    store.write_repository_state_json(newer).unwrap();
    assert_eq!(
        store
            .read_repository_state_json_migrated()
            .unwrap()
            .as_deref(),
        Some(newer)
    );
}

#[test]
fn foreign_repository_scan_is_read_only_and_ignores_non_wt_files() {
    let temp = tempdir().unwrap();
    let db_path = temp.path().join("source.sqlite");
    let mut store = Store::open(&db_path, identity("foreign", "/old/repo")).unwrap();
    store
        .write_repository_state_json(r#"{"version":17}"#)
        .unwrap();
    store
        .replace_archived_keys(&BTreeSet::from(["gone".to_owned()]))
        .unwrap();
    let rows = Store::read_foreign_repository_rows(&db_path).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].repo_id, "foreign");
    assert_eq!(rows[0].repo_path, "/old/repo");
    assert_eq!(rows[0].archived, BTreeSet::from(["gone".to_owned()]));

    let junk = temp.path().join("not-a-db.sqlite");
    fs::write(&junk, "not sqlite").unwrap();
    assert!(
        Store::read_foreign_repository_rows(junk)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn title_cas_counts_same_text_edit_and_rejects_delayed_result() {
    let temp = tempdir().unwrap();
    let mut store = Store::open(
        temp.path().join("wt.sqlite"),
        identity("repo-a", "/repos/a"),
    )
    .unwrap();
    store
        .write_repository_state_json(r#"{"version":17,"futureRoot":true}"#)
        .unwrap();
    let user = |store: &mut Store, title: &str, revision| {
        store
            .set_slug_title("task", title, TitleSource::User, revision)
            .unwrap()
    };
    assert_eq!(
        user(&mut store, " Pinned title ", Some(0)),
        TitleWrite::Saved
    );
    assert_eq!(user(&mut store, "Pinned title", Some(1)), TitleWrite::Saved);
    assert_eq!(
        user(&mut store, "Delayed title", Some(1)),
        TitleWrite::Stale
    );
    let state: serde_json::Value =
        serde_json::from_str(&store.read_repository_state_json().unwrap().unwrap()).unwrap();
    assert_eq!(state["slugs"]["task"]["manualTitle"], "Pinned title");
    assert_eq!(state["slugs"]["task"]["titleSource"], "user");
    assert_eq!(state["slugs"]["task"]["manualTitleRevision"], 2);
    assert_eq!(state["futureRoot"], true);
    assert!(!store.clear_slug_manual_title("task", 1).unwrap());
    assert!(store.clear_slug_manual_title("task", 2).unwrap());
    let state = store.read_wt_state().unwrap();
    assert!(state["slugs"]["task"]["manualTitle"].is_null());
    assert!(state["slugs"]["task"]["titleSource"].is_null());
    assert_eq!(state["slugs"]["task"]["manualTitleRevision"], 3);
    assert_eq!(state["futureRoot"], true);
}

#[test]
fn title_sources_only_climb_and_a_user_rename_is_final() {
    let temp = tempdir().unwrap();
    let mut store = Store::open(temp.path().join("wt.sqlite"), identity("repo", "/repo")).unwrap();
    let mut set = |title: &str, source| {
        let outcome = store.set_slug_title("task", title, source, None).unwrap();
        let state = store.read_wt_state().unwrap();
        (outcome, crate::stored_title(&state, "task").unwrap())
    };
    // Created from an issue whose title lookup failed, then seeded by it.
    assert_eq!(
        set("COZ-1234", TitleSource::Fallback),
        (
            TitleWrite::Saved,
            ("COZ-1234".into(), TitleSource::Fallback)
        )
    );
    assert_eq!(
        set("Calendar renders twice", TitleSource::Issue),
        (
            TitleWrite::Saved,
            ("Calendar renders twice".into(), TitleSource::Issue)
        )
    );
    // A late fallback cannot undo the issue title.
    assert_eq!(
        set("COZ-1234", TitleSource::Fallback),
        (
            TitleWrite::Outranked(TitleSource::Issue),
            ("Calendar renders twice".into(), TitleSource::Issue)
        )
    );
    // The agent renames as often as the outcome changes; a later issue seed
    // is refused.
    assert_eq!(
        set(
            "Render the calendar once per week change",
            TitleSource::Agent
        )
        .0,
        TitleWrite::Saved
    );
    assert_eq!(
        set("Render the calendar once", TitleSource::Agent).0,
        TitleWrite::Saved
    );
    assert_eq!(
        set("Calendar renders twice", TitleSource::Issue).0,
        TitleWrite::Outranked(TitleSource::Agent)
    );
    // The user can always rename, and nothing else replaces it afterwards.
    assert_eq!(set("Calendar fix", TitleSource::User).0, TitleWrite::Saved);
    assert_eq!(
        set("Calendar fix, take two", TitleSource::User).0,
        TitleWrite::Saved
    );
    for source in [
        TitleSource::Agent,
        TitleSource::Issue,
        TitleSource::Fallback,
    ] {
        assert_eq!(
            set("Not this", source),
            (
                TitleWrite::Outranked(TitleSource::User),
                ("Calendar fix, take two".into(), TitleSource::User)
            )
        );
    }
    // The next PR in the worktree is new work, so its title replaces even a
    // person's, and the rename rules then apply to the new title.
    assert_eq!(
        store
            .reset_slug_title("task", "Next PR", TitleSource::Agent)
            .unwrap(),
        TitleWrite::Saved
    );
    let state = store.read_wt_state().unwrap();
    assert_eq!(
        crate::stored_title(&state, "task").unwrap(),
        ("Next PR".into(), TitleSource::Agent)
    );
}

#[test]
fn reparenting_preserves_squash_safe_base_anchor() {
    let temp = tempdir().unwrap();
    let mut store = Store::open(
        temp.path().join("wt.sqlite"),
        identity("repo-a", "/repos/a"),
    )
    .unwrap();
    store
        .write_repository_state_json(
            r#"{"version":17,"slugs":{"parent":{"baseBranch":"grandparent"},"child":{"baseBranch":"parent","baseSha":"anchor-123"},"other":{"baseBranch":"parent","baseSha":"anchor-456"}}}"#,
        )
        .unwrap();
    let affected = store
        .reparent_base_references("parent", "trunk", Some("parent"))
        .unwrap();
    assert_eq!(affected, vec!["child", "other"]);
    let state: serde_json::Value =
        serde_json::from_str(&store.read_repository_state_json().unwrap().unwrap()).unwrap();
    assert_eq!(state["slugs"]["child"]["baseBranch"], "grandparent");
    assert_eq!(state["slugs"]["child"]["baseSha"], "anchor-123");
    assert_eq!(state["slugs"]["other"]["baseSha"], "anchor-456");
}

#[test]
fn typed_record_updates_keep_unknown_nested_fields() {
    let temp = tempdir().unwrap();
    let mut store = Store::open(
        temp.path().join("wt.sqlite"),
        identity("repo-a", "/repos/a"),
    )
    .unwrap();
    store
        .write_repository_state_json(
            r#"{"version":17,"slugs":{"task":{"examined":{"sha":"old","verdict":"keep","futureExamined":1}}},"edges":[{"from":"a","to":"b","kind":"before","strength":"blocks","at":"old","by":"agent","futureEdge":2}],"reviewRequestDismissals":[{"url":"https://example.test/pr/1","updatedAt":"old","dismissedAt":"old","futureDismissal":3}] }"#,
        )
        .unwrap();

    store
        .set_slug_examined(
            "task",
            Some(json!({"sha":"new","verdict":"keep","at":"now"})),
        )
        .unwrap();
    let edge = MergeEdge {
        from: "a".to_owned(),
        to: "b".to_owned(),
        kind: "before".to_owned(),
        strength: "blocks".to_owned(),
        at: "now".to_owned(),
        by: "agent".to_owned(),
        why: None,
        from_sha: None,
        to_sha: None,
        extra: Map::new(),
    };
    store.set_merge_edge(&edge).unwrap();
    store
        .add_review_request_dismissal(&crate::ReviewRequestDismissal {
            url: "https://example.test/pr/1".to_owned(),
            updated_at: "new".to_owned(),
            dismissed_at: "new".to_owned(),
            extra: Map::new(),
        })
        .unwrap();

    let state: serde_json::Value =
        serde_json::from_str(&store.read_repository_state_json().unwrap().unwrap()).unwrap();
    assert_eq!(state["slugs"]["task"]["examined"]["futureExamined"], 1);
    assert_eq!(state["edges"][0]["futureEdge"], 2);
    assert_eq!(state["reviewRequestDismissals"][0]["futureDismissal"], 3);
}

#[test]
fn work_status_amends_verification_steps_and_keeps_idempotent_assertion_time() {
    let temp = tempdir().unwrap();
    let mut store = Store::open(
        temp.path().join("wt.sqlite"),
        identity("repo-a", "/repos/a"),
    )
    .unwrap();
    let status = |at: &str, verify_after_merge: &str| WorkStatusRecord {
        state: "ready".to_owned(),
        at: at.to_owned(),
        note: None,
        risk: Some("low".to_owned()),
        sha: Some("head".to_owned()),
        by: None,
        blocked_on: None,
        verify_after_merge: Some(verify_after_merge.to_owned()),
        verify_in: None,
        extra: Map::new(),
    };
    assert!(
        store
            .set_slug_work_status("task", Some(&status("t1", "check deployed PR")))
            .unwrap()
    );
    assert!(
        !store
            .set_slug_work_status("task", Some(&status("t2", "check deployed PR")))
            .unwrap()
    );
    assert!(
        store
            .set_slug_work_status("task", Some(&status("t1", "check exact context")))
            .unwrap()
    );
    let state: serde_json::Value =
        serde_json::from_str(&store.read_repository_state_json().unwrap().unwrap()).unwrap();
    assert_eq!(
        state["slugs"]["task"]["work"]["verifyAfterMerge"],
        "check exact context"
    );
    assert_eq!(state["slugs"]["task"]["work"]["at"], "t1");
}

#[test]
fn work_status_reassertion_and_amend_preserve_unknown_fields() {
    let temp = tempdir().unwrap();
    let mut store = Store::open(
        temp.path().join("wt.sqlite"),
        identity("repo-a", "/repos/a"),
    )
    .unwrap();
    store
        .write_repository_state_json(
            r#"{"version":17,"slugs":{"task":{"work":{"state":"ready","at":"original","risk":"low","sha":"head","verifyAfterMerge":"deploy","futureStatus":{"x":1}}}}}"#,
        )
        .unwrap();
    let status = |at: &str, verify_after_merge: &str| WorkStatusRecord {
        state: "ready".to_owned(),
        at: at.to_owned(),
        note: None,
        risk: Some("low".to_owned()),
        sha: Some("head".to_owned()),
        by: None,
        blocked_on: None,
        verify_after_merge: Some(verify_after_merge.to_owned()),
        verify_in: None,
        extra: Map::new(),
    };
    assert!(
        !store
            .set_slug_work_status("task", Some(&status("reasserted", "deploy")))
            .unwrap()
    );
    let unchanged: serde_json::Value =
        serde_json::from_str(&store.read_repository_state_json().unwrap().unwrap()).unwrap();
    assert_eq!(unchanged["slugs"]["task"]["work"]["at"], "original");
    assert_eq!(unchanged["slugs"]["task"]["work"]["futureStatus"]["x"], 1);

    assert!(
        store
            .set_slug_work_status("task", Some(&status("amended", "new deploy")))
            .unwrap()
    );
    let amended: serde_json::Value =
        serde_json::from_str(&store.read_repository_state_json().unwrap().unwrap()).unwrap();
    assert_eq!(
        amended["slugs"]["task"]["work"]["verifyAfterMerge"],
        "new deploy"
    );
    assert_eq!(amended["slugs"]["task"]["work"]["futureStatus"]["x"], 1);
}

#[test]
fn removal_revision_proof_overrides_historical_pr_state() {
    let mut row: RemovedWorktree = serde_json::from_value(json!({
        "slug": "feature", "branch": "feature/one", "removedAt": "2026-10-09T00:00:00Z",
        "prState": "MERGED", "gitState": "merged"
    }))
    .unwrap();
    assert!(crate::is_merged_removal(&row));
    for (proof, expected) in [
        ("unlanded", false),
        ("unknown-future-proof", false),
        ("base", true),
        ("production", true),
    ] {
        row.extra.insert("landedOnAtRemoval".into(), json!(proof));
        assert_eq!(crate::is_merged_removal(&row), expected, "{proof}");
    }
}

#[test]
fn removed_history_keeps_verification_obligation_and_pause_across_minimal_confirm() {
    let temp = tempdir().unwrap();
    let mut store = Store::open(
        temp.path().join("wt.sqlite"),
        identity("repo-a", "/repos/a"),
    )
    .unwrap();
    let work = WorkStatusRecord {
        state: "ready".to_owned(),
        at: "2026-10-01T00:00:00Z".to_owned(),
        note: None,
        risk: Some("low".to_owned()),
        sha: None,
        by: None,
        blocked_on: None,
        verify_after_merge: Some("check staging".to_owned()),
        verify_in: None,
        extra: Map::new(),
    };
    let rich = RemovedWorktree {
        slug: "task".to_owned(),
        branch: "feature/task".to_owned(),
        removed_at: "2026-10-08T00:00:00Z".to_owned(),
        work: Some(work),
        automations_paused: Some(true),
        extra: Map::from_iter([
            ("prState".to_owned(), json!("MERGED")),
            ("landedOnAtRemoval".to_owned(), json!("base")),
            ("prMergeCommitOid".to_owned(), json!("merge-sha")),
        ]),
    };
    store
        .record_removed_worktrees(&[rich], 1_792_000_000_000)
        .unwrap();
    let minimal = RemovedWorktree {
        slug: "task".to_owned(),
        branch: "feature/task".to_owned(),
        removed_at: "2026-10-09T00:00:00Z".to_owned(),
        work: None,
        automations_paused: None,
        extra: Map::new(),
    };
    store
        .record_removed_worktrees(&[minimal], 1_792_000_000_000)
        .unwrap();
    let state: serde_json::Value =
        serde_json::from_str(&store.read_repository_state_json().unwrap().unwrap()).unwrap();
    let entry = &state["removed"][0];
    assert_eq!(entry["work"]["verifyAfterMerge"], "check staging");
    assert_eq!(entry["automationsPaused"], true);
    assert_eq!(entry["landedOnAtRemoval"], "base");
    assert_eq!(entry["prMergeCommitOid"], "merge-sha");
}

#[test]
fn removed_history_follows_an_unpause_made_while_the_removal_was_pending() {
    let temp = tempdir().unwrap();
    let mut store = Store::open(
        temp.path().join("wt.sqlite"),
        identity("repo-a", "/repos/a"),
    )
    .unwrap();
    let removal = |removed_at: &str| RemovedWorktree {
        slug: "task".to_owned(),
        branch: "feature/task".to_owned(),
        removed_at: removed_at.to_owned(),
        work: None,
        automations_paused: None,
        extra: Map::new(),
    };
    assert!(store.toggle_slug_automations_paused("task").unwrap());
    store
        .record_removed_worktrees(&[removal("2026-10-08T00:00:00Z")], 1_792_000_000_000)
        .unwrap();
    assert_eq!(
        store.read_removed_worktrees().unwrap()[0].automations_paused,
        Some(true)
    );
    // The slug is still live; the user unpauses before the removal confirms.
    assert!(!store.toggle_slug_automations_paused("task").unwrap());
    store
        .record_removed_worktrees(&[removal("2026-10-09T00:00:00Z")], 1_792_000_000_000)
        .unwrap();
    assert_eq!(
        store.read_removed_worktrees().unwrap()[0].automations_paused,
        None
    );
}

#[test]
fn failed_restore_reinserts_exact_history_only_when_no_newer_removal_exists() {
    let temp = tempdir().unwrap();
    let mut store = Store::open(
        temp.path().join("wt.sqlite"),
        identity("repo-a", "/repos/a"),
    )
    .unwrap();
    // Restores prune by the wall clock, so the history must be recent.
    let now = time::OffsetDateTime::now_utc()
        .replace_nanosecond(0)
        .unwrap();
    let at = |ago: time::Duration| {
        (now - ago)
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap()
    };
    let old = RemovedWorktree {
        slug: "task".to_owned(),
        branch: "feature/task".to_owned(),
        removed_at: at(time::Duration::hours(1)),
        work: None,
        automations_paused: Some(true),
        extra: Map::from_iter([("futureField".to_owned(), json!({"preserve": true}))]),
    };
    assert!(
        store
            .restore_removed_worktree_if_absent("task", &old)
            .unwrap()
    );
    assert_eq!(store.read_removed_worktrees().unwrap(), vec![old.clone()]);
    assert!(store.clear_removed_worktree("task").unwrap());

    let newer = RemovedWorktree {
        removed_at: at(time::Duration::minutes(59)),
        ..old.clone()
    };
    store
        .record_removed_worktrees(std::slice::from_ref(&newer), now.unix_timestamp() * 1000)
        .unwrap();
    assert!(
        !store
            .restore_removed_worktree_if_absent("task", &old)
            .unwrap()
    );
    assert_eq!(store.read_removed_worktrees().unwrap(), vec![newer]);
}

#[test]
fn mixed_host_ordering_stays_in_controller_layout_and_preserves_unknown_fields() {
    let temp = tempdir().unwrap();
    let mut store = Store::open(
        temp.path().join("wt.sqlite"),
        identity("repo-a", "/repos/a"),
    )
    .unwrap();
    let remote = "@remote/server/one";
    store.write_repository_state_json(r#"{"slugs":{"local":{"order":1,"manualTitle":"Keep"}},"remoteLayouts":{"@remote/server/one":{"order":2,"future":42}}}"#).unwrap();
    let shown = units(&["local", remote]);
    assert_eq!(
        store
            .swap_shown_units(as_shown(None, &shown), "local", remote)
            .unwrap(),
        Ok(())
    );
    let state = store.read_wt_state().unwrap();
    assert!(state["slugs"].get(remote).is_none());
    assert_eq!(state["slugs"]["local"]["manualTitle"], "Keep");
    assert_eq!(state["remoteLayouts"][remote]["future"], 42);
    assert!(
        state["remoteLayouts"][remote]["order"].as_f64().unwrap()
            < state["slugs"]["local"]["order"].as_f64().unwrap()
    );
    store.place_slug(remote, Some("Next"), true).unwrap();
    let state = store.read_wt_state().unwrap();
    assert_eq!(state["remoteLayouts"][remote]["section"], "Next");
    assert!(state["slugs"].get(remote).is_none());
}

/// The stored orders of `keys`, sorted as the board draws explicit orders.
fn by_stored_order(store: &mut Store, keys: &[&str]) -> Vec<String> {
    let state = store.read_wt_state().unwrap();
    let mut placed: Vec<(f64, String)> = keys
        .iter()
        .map(|key| {
            let collection = if key.starts_with("@remote/") {
                "remoteLayouts"
            } else {
                "slugs"
            };
            (
                state[collection][*key]["order"]
                    .as_f64()
                    .unwrap_or_else(|| panic!("{key} has no stored order")),
                (*key).to_owned(),
            )
        })
        .collect();
    placed.sort_by(|a, b| a.0.total_cmp(&b.0));
    placed.into_iter().map(|(_, key)| key).collect()
}

fn keys(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

/// Units as manual sort shows them: one band, each drawn by its stored order.
fn units(names: &[&str]) -> Vec<ShownUnit> {
    banded(&names.iter().map(|name| (*name, 0)).collect::<Vec<_>>())
}

/// Units with the status the board draws them under.
fn banded(units: &[(&str, u8)]) -> Vec<ShownUnit> {
    units
        .iter()
        .map(|(key, band)| ShownUnit {
            key: (*key).to_owned(),
            band: *band,
            anchored: false,
        })
        .collect()
}

fn as_shown<'a>(name: Option<&'a str>, drawn: &'a [ShownUnit]) -> ShownSection<'a> {
    ShownSection { name, drawn }
}

#[test]
fn a_swap_moves_exactly_the_pair_shown_and_refuses_a_layout_that_changed() {
    let temp = tempdir().unwrap();
    let mut store = Store::open(
        temp.path().join("wt.sqlite"),
        identity("repo-a", "/repos/a"),
    )
    .unwrap();
    store
        .write_repository_state_json(
            r#"{"slugs":{
                "a":{"section":"S","order":0},"b":{"section":"S","order":1},
                "c":{"section":"S","order":2,"manualTitle":"Keep"},"d":{"section":"S","order":3}}}"#,
        )
        .unwrap();
    let all = ["a", "b", "c", "d"];
    let shown = units(&all);
    assert_eq!(
        store
            .swap_shown_units(as_shown(Some("S"), &shown), "a", "b")
            .unwrap(),
        Ok(())
    );
    assert_eq!(by_stored_order(&mut store, &all), ["b", "a", "c", "d"]);
    // Neighbours with orders of their own trade them; the rest is untouched.
    let state = store.read_wt_state().unwrap();
    assert_eq!(state["slugs"]["c"]["order"], 2.0);
    assert_eq!(state["slugs"]["d"]["order"], 3.0);
    assert_eq!(state["slugs"]["c"]["manualTitle"], "Keep");

    // The same press sent again from the layout it no longer describes (a
    // host that has not re-read its own write) must not swap the pair back.
    assert_eq!(
        store
            .swap_shown_units(as_shown(Some("S"), &shown), "a", "b")
            .unwrap(),
        Err(LayoutChanged::Order("a".into(), "b".into()))
    );
    assert_eq!(by_stored_order(&mut store, &all), ["b", "a", "c", "d"]);
    // The next press carries what the first one left, and goes one further.
    let shown = units(&["b", "a", "c", "d"]);
    assert_eq!(
        store
            .swap_shown_units(as_shown(Some("S"), &shown), "a", "c")
            .unwrap(),
        Ok(())
    );
    assert_eq!(by_stored_order(&mut store, &all), ["b", "c", "a", "d"]);

    // Another writer reorders the section: the witness no longer holds.
    let theirs = units(&["b", "c", "a", "d"]);
    store
        .swap_shown_units(as_shown(Some("S"), &theirs), "d", "a")
        .unwrap()
        .unwrap();
    assert_eq!(
        store
            .swap_shown_units(as_shown(Some("S"), &theirs), "a", "d")
            .unwrap(),
        Err(LayoutChanged::Order("a".into(), "d".into()))
    );
    // Or files one of its rows elsewhere.
    store
        .move_worktrees_to_section(&["c".to_owned()], Some("T"))
        .unwrap();
    let shown = units(&["b", "c", "d", "a"]);
    assert_eq!(
        store
            .swap_shown_units(as_shown(Some("S"), &shown), "d", "a")
            .unwrap(),
        Err(LayoutChanged::Filed("c".into()))
    );
    assert_eq!(
        by_stored_order(&mut store, &["b", "d", "a"]),
        ["b", "d", "a"]
    );
}

#[test]
fn the_first_reorder_stores_the_order_drawn_including_creation_anchors() {
    let temp = tempdir().unwrap();
    let mut store = Store::open(
        temp.path().join("wt.sqlite"),
        identity("repo-a", "/repos/a"),
    )
    .unwrap();
    // Manual sort draws: the never-ordered rows first, then the ordered
    // ones, with a new row behind the row it was created after.
    store
        .write_repository_state_json(
            r#"{"slugs":{
                "top":{"section":"S","order":null,"creationAfter":null},
                "x":{"section":"S"},
                "y":{"section":"S","order":5},
                "new":{"section":"S","order":null,"creationAfter":"y"},
                "z":{"section":"S","order":6},
                "landed":{"section":"S","order":5.5}}}"#,
        )
        .unwrap();
    let all = ["top", "x", "y", "new", "z"];
    let shown = units(&all);
    assert_eq!(
        store
            .swap_shown_units(as_shown(Some("S"), &shown), "x", "y")
            .unwrap(),
        Ok(())
    );
    // Every unit has an order now, so the anchor no longer places "new":
    // it stays where it was drawn.
    assert_eq!(
        by_stored_order(&mut store, &all),
        ["top", "y", "x", "new", "z"]
    );
    // A row filed here and drawn elsewhere stays between the two it was
    // stored between, although the caller was not shown it.
    assert_eq!(
        by_stored_order(&mut store, &["y", "landed", "z"]),
        ["y", "landed", "z"]
    );
    // So does one filed after the caller looked.
    store
        .write_repository_state_json(
            r#"{"slugs":{
                "a":{"section":"S"},"b":{"section":"S"},
                "x":{"section":"S","order":7},"since":{"section":"S","order":8}}}"#,
        )
        .unwrap();
    let shown = units(&["a", "b", "x"]);
    store
        .swap_shown_units(as_shown(Some("S"), &shown), "a", "b")
        .unwrap()
        .unwrap();
    assert_eq!(
        by_stored_order(&mut store, &["b", "a", "x", "since"]),
        ["b", "a", "x", "since"]
    );

    // Under status sort the two can have units of another status between
    // them in manual order. The moved unit takes the other's place and the
    // rest keep their order; only the units of one status are compared.
    store
        .write_repository_state_json(
            r#"{"slugs":{
                "p":{"section":"S","order":1},"other":{"section":"S","order":2},
                "q":{"section":"S","order":3},"late":{"section":"S","order":0}}}"#,
        )
        .unwrap();
    let drawn = banded(&[("p", 0), ("q", 0), ("late", 1), ("other", 1)]);
    assert_eq!(
        store
            .swap_shown_units(as_shown(Some("S"), &drawn), "p", "q")
            .unwrap(),
        Ok(())
    );
    assert_eq!(
        by_stored_order(&mut store, &["late", "p", "other", "q"]),
        ["late", "other", "q", "p"]
    );
    // Manual sort draws by stored order alone, so there the whole list is
    // the witness and this one was never the stored order.
    let drawn = units(&["p", "q", "late", "other"]);
    assert_eq!(
        store
            .swap_shown_units(as_shown(Some("S"), &drawn), "p", "q")
            .unwrap(),
        Err(LayoutChanged::Order("p".into(), "q".into()))
    );
}

#[test]
fn a_unit_crosses_to_the_edge_of_the_section_shown_or_is_refused() {
    let temp = tempdir().unwrap();
    let mut store = Store::open(
        temp.path().join("wt.sqlite"),
        identity("repo-a", "/repos/a"),
    )
    .unwrap();
    let remote = "@remote/server/one";
    store
        .write_repository_state_json(
            r#"{"sectionsOrder":["S","T","Empty"],
                "slugs":{
                "root":{"section":"S","order":0},"child":{"section":"S","order":1},
                "c":{"section":"T"},"d":{"section":"T","order":3}},
                "remoteLayouts":{"@remote/server/one":{"section":"S","order":2,"future":42}}}"#,
        )
        .unwrap();
    fn section<'a>(name: &'a str, drawn: &'a [ShownUnit]) -> ShownSection<'a> {
        as_shown(Some(name), drawn)
    }
    let unit = keys(&["root", "child"]);
    let (here, there) = (&units(&["root", remote]), &units(&["c", "d"]));
    // "c" was never ordered and is drawn first, so the top is before it.
    assert_eq!(
        store
            .move_shown_unit(&unit, "root", section("S", here), section("T", there), true)
            .unwrap(),
        Ok(unit.clone())
    );
    assert_eq!(
        by_stored_order(&mut store, &["root", "c", "d"]),
        ["root", "c", "d"]
    );
    let state = store.read_wt_state().unwrap();
    assert_eq!(state["slugs"]["child"]["section"], "T");

    // Back up: the bottom of S, behind the remote row filed there.
    let (back, rest) = (&units(&["root", "c", "d"]), &units(&[remote]));
    assert_eq!(
        store
            .move_shown_unit(&unit, "root", section("T", back), section("S", rest), false)
            .unwrap(),
        Ok(unit.clone())
    );
    let state = store.read_wt_state().unwrap();
    assert!(
        state["slugs"]["root"]["order"].as_f64().unwrap()
            > state["remoteLayouts"][remote]["order"].as_f64().unwrap()
    );
    assert_eq!(state["remoteLayouts"][remote]["future"], 42);
    assert!(state["slugs"].get(remote).is_none());

    // Refused: the row left the section it was shown in, the destination
    // is gone, or either section is not laid out as shown.
    let before = store.read_wt_state().unwrap();
    let none: &[ShownUnit] = &[];
    let leaving = &units(&[remote, "root"]);
    assert_eq!(
        store
            .move_shown_unit(
                &unit,
                "root",
                section("T", none),
                section("Empty", none),
                true
            )
            .unwrap(),
        Err(LayoutChanged::Filed("root".into()))
    );
    assert_eq!(
        store
            .move_shown_unit(
                &unit,
                "root",
                section("S", leaving),
                section("Gone", none),
                true
            )
            .unwrap(),
        Err(LayoutChanged::Section("Gone".into()))
    );
    assert_eq!(
        store
            .move_shown_unit(
                &unit,
                "root",
                section("S", here),
                section("Empty", none),
                true
            )
            .unwrap(),
        Err(LayoutChanged::Order("root".into(), remote.into()))
    );
    store
        .swap_shown_units(section("T", there), "c", "d")
        .unwrap()
        .unwrap();
    assert_eq!(
        store
            .move_shown_unit(
                &unit,
                "root",
                section("S", leaving),
                section("T", there),
                true
            )
            .unwrap(),
        Err(LayoutChanged::Order("c".into(), "d".into()))
    );
    assert_eq!(
        store.read_wt_state().unwrap()["slugs"]["root"],
        before["slugs"]["root"]
    );
    // An empty named section takes the unit, and the Inbox is `None`.
    assert!(
        store
            .move_shown_unit(
                &unit,
                "root",
                section("S", leaving),
                section("Empty", none),
                true
            )
            .unwrap()
            .is_ok()
    );
    let last = keys(&["d"]);
    let (leaving, inbox) = (&units(&["d", "c"]), as_shown(None, none));
    assert!(
        store
            .move_shown_unit(&last, "d", section("T", leaving), inbox, false)
            .unwrap()
            .is_ok()
    );
    let state = store.read_wt_state().unwrap();
    assert_eq!(state["slugs"]["root"]["section"], "Empty");
    assert!(state["slugs"]["d"]["section"].is_null());
}

#[test]
fn under_status_sort_a_press_stores_the_section_as_drawn_with_one_unit_moved() {
    let temp = tempdir().unwrap();
    let mut store = Store::open(
        temp.path().join("wt.sqlite"),
        identity("repo-a", "/repos/a"),
    )
    .unwrap();
    // "a" was created from the board and has a status: status sort draws it
    // by that, first of its status because it was never ordered, and not
    // behind "p", which a row still at its creation position would follow.
    let state = r#"{"slugs":{
        "p":{"section":"S","order":5},
        "q":{"section":"S","order":1},
        "a":{"section":"S","order":null,"creationAfter":"p"},
        "z":{"section":"S","order":2}}}"#;
    let drawn = banded(&[("p", 0), ("a", 1), ("q", 1), ("z", 1)]);
    // The orders stored must draw each status in the order shown, with only
    // the pressed unit moved.
    let status_one =
        |store: &mut Store| -> Vec<String> { by_stored_order(store, &["a", "q", "z"]) };
    for (unit, past, expected) in [
        ("a", "q", ["q", "a", "z"]),
        ("q", "a", ["q", "a", "z"]),
        ("q", "z", ["a", "z", "q"]),
        ("z", "q", ["a", "z", "q"]),
    ] {
        store.write_repository_state_json(state).unwrap();
        assert_eq!(
            store
                .swap_shown_units(as_shown(Some("S"), &drawn), unit, past)
                .unwrap(),
            Ok(()),
            "{unit} past {past}"
        );
        assert_eq!(status_one(&mut store), expected, "{unit} past {past}");
    }
    // The manual order across statuses, which status sort does not draw,
    // is kept: "p" stays behind every unit it was stored behind.
    assert_eq!(
        by_stored_order(&mut store, &["a", "z", "q", "p"]),
        ["a", "z", "q", "p"]
    );
}

#[test]
fn a_unit_at_its_creation_position_keeps_no_order_and_follows_what_is_drawn_before_it() {
    let temp = tempdir().unwrap();
    let mut store = Store::open(
        temp.path().join("wt.sqlite"),
        identity("repo-a", "/repos/a"),
    )
    .unwrap();
    let state = r#"{"sectionsOrder":["S","T"],"slugs":{
        "x":{"section":"S","order":1},
        "new":{"section":"S","order":null,"creationAfter":"x"},
        "y":{"section":"S","order":2},
        "t":{"section":"T","order":1}}}"#;
    store.write_repository_state_json(state).unwrap();
    let anchored = |key: &str| ShownUnit {
        key: key.to_owned(),
        band: 5,
        anchored: true,
    };
    let placed = |key: &str| ShownUnit {
        key: key.to_owned(),
        band: 2,
        anchored: false,
    };
    let anchor = |store: &mut Store, key: &str| {
        let state = store.read_wt_state().unwrap();
        assert!(state["slugs"][key]["order"].is_null(), "{key} has an order");
        state["slugs"][key]["creationAfter"].clone()
    };
    // The new row passes "y": no order is written, it follows "y" now.
    let drawn = [placed("x"), anchored("new"), placed("y")];
    assert_eq!(
        store
            .swap_shown_units(as_shown(Some("S"), &drawn), "new", "y")
            .unwrap(),
        Ok(())
    );
    assert_eq!(anchor(&mut store, "new"), "y");
    assert_eq!(by_stored_order(&mut store, &["x", "y"]), ["x", "y"]);
    // "y" passes it back: the row follows what is drawn before it, "x".
    let drawn = [placed("x"), placed("y"), anchored("new")];
    assert_eq!(
        store
            .swap_shown_units(as_shown(Some("S"), &drawn), "y", "new")
            .unwrap(),
        Ok(())
    );
    assert_eq!(anchor(&mut store, "new"), "x");
    // The unit it follows leaves the section: it follows the one before,
    // and here that is none.
    let drawn = [placed("x"), anchored("new"), placed("y")];
    let there = [placed("t")];
    assert!(
        store
            .move_shown_unit(
                &keys(&["x"]),
                "x",
                as_shown(Some("S"), &drawn),
                as_shown(Some("T"), &there),
                false
            )
            .unwrap()
            .is_ok()
    );
    assert!(anchor(&mut store, "new").is_null());
    assert_eq!(by_stored_order(&mut store, &["t", "x"]), ["t", "x"]);
    // Shown at its creation position by a board that is behind the write
    // which gave it an order: it is drawn by that order, and moves by it.
    store
        .write_repository_state_json(
            r#"{"slugs":{"x":{"section":"S","order":1},"new":{"section":"S","order":3,"creationAfter":"x"}}}"#,
        )
        .unwrap();
    let drawn = [
        ShownUnit {
            band: 5,
            ..placed("x")
        },
        anchored("new"),
    ];
    assert_eq!(
        store
            .swap_shown_units(as_shown(Some("S"), &drawn), "x", "new")
            .unwrap(),
        Ok(())
    );
    assert_eq!(by_stored_order(&mut store, &["x", "new"]), ["new", "x"]);
}

#[test]
fn a_press_leaves_the_statuses_it_did_not_move_within_in_their_stored_order() {
    let temp = tempdir().unwrap();
    let mut store = Store::open(
        temp.path().join("wt.sqlite"),
        identity("repo-a", "/repos/a"),
    )
    .unwrap();
    // The caller lists "y" before "x", as its board drew them. Since then a
    // status changed or another writer reordered them: stored, "x" is first.
    store
        .write_repository_state_json(
            r#"{"sectionsOrder":["S","T"],"slugs":{
                "a":{"section":"S","order":1},"b":{"section":"S"},
                "x":{"section":"S","order":2},"y":{"section":"S","order":5},
                "never":{"section":"S"},
                "t":{"section":"T"},"u":{"section":"T","order":4},"v":{"section":"T","order":1}}}"#,
        )
        .unwrap();
    let drawn = banded(&[("b", 0), ("a", 0), ("y", 1), ("never", 1), ("x", 1)]);
    // "b" was never ordered, so the section is numbered.
    assert_eq!(
        store
            .swap_shown_units(as_shown(Some("S"), &drawn), "b", "a")
            .unwrap(),
        Ok(())
    );
    assert_eq!(by_stored_order(&mut store, &["a", "b"]), ["a", "b"]);
    // Not in the order listed: in the order stored, a row never ordered
    // first, as the board draws it.
    assert_eq!(
        by_stored_order(&mut store, &["never", "x", "y"]),
        ["never", "x", "y"]
    );
    // The same for the section a unit enters and the one it leaves.
    let leaving = banded(&[("a", 0), ("b", 0), ("y", 1), ("x", 1)]);
    let entering = banded(&[("t", 0), ("u", 1), ("v", 1)]);
    assert!(
        store
            .move_shown_unit(
                &keys(&["a"]),
                "a",
                as_shown(Some("S"), &leaving),
                as_shown(Some("T"), &entering),
                true
            )
            .unwrap()
            .is_ok()
    );
    assert_eq!(
        by_stored_order(&mut store, &["a", "t", "v", "u"]),
        ["a", "t", "v", "u"]
    );
    assert_eq!(by_stored_order(&mut store, &["x", "y"]), ["x", "y"]);
}

#[test]
fn a_row_that_leaves_its_section_tells_the_rows_created_after_it_where_it_was() {
    let temp = tempdir().unwrap();
    let mut store = Store::open(
        temp.path().join("wt.sqlite"),
        identity("repo-a", "/repos/a"),
    )
    .unwrap();
    let remote = "@remote/server/far";
    let state = r#"{"sectionsOrder":["S","T"],"slugs":{
        "x":{"section":"S","order":4},
        "new":{"section":"S","order":null,"creationAfter":"x"},
        "newer":{"section":"S","order":null,"creationAfter":"new"},
        "ordered":{"section":"S","order":9,"creationAfter":"x"},
        "elsewhere":{"section":"T","order":null,"creationAfter":"x"}},
        "remoteLayouts":{"@remote/server/far":{"section":"S","order":null,"creationAfter":"x"}}}"#;
    let slot = json!({ "order": 4.0, "key": "x" });
    type Leave = fn(&mut Store);
    let leaves: [Leave; 4] = [
        |store| {
            store
                .move_worktrees_to_section(&["x".to_owned()], Some("T"))
                .unwrap();
        },
        |store| store.place_slug("x", None, true).unwrap(),
        |store| {
            store.clear_slug_state("x").unwrap();
        },
        |store| {
            let live = ["new", "newer", "ordered", "elsewhere"].map(str::to_owned);
            store.reap_wt_state(&live.into()).unwrap();
        },
    ];
    for leave in leaves {
        store.write_repository_state_json(state).unwrap();
        leave(&mut store);
        let after = store.read_wt_state().unwrap();
        assert_eq!(after["slugs"]["new"]["creationSlot"], slot);
        assert_eq!(after["remoteLayouts"][remote]["creationSlot"], slot);
        // Not rows it does not place: one with an order of its own, one
        // filed in another section, one that follows another row.
        for other in ["ordered", "elsewhere", "newer"] {
            assert!(
                after["slugs"][other].get("creationSlot").is_none(),
                "{other}"
            );
        }
    }
    // A row at its creation position leaves: what follows it follows what
    // it followed, and where that was.
    store.write_repository_state_json(state).unwrap();
    store
        .move_worktrees_to_section(&["x".to_owned()], Some("T"))
        .unwrap();
    store
        .move_worktrees_to_section(&["new".to_owned()], Some("T"))
        .unwrap();
    let after = store.read_wt_state().unwrap();
    assert_eq!(after["slugs"]["newer"]["creationAfter"], "x");
    assert_eq!(after["slugs"]["newer"]["creationSlot"], slot);
    // A press restates what the row follows, and the slot is done with.
    let drawn = [
        ShownUnit {
            key: remote.to_owned(),
            band: 0,
            anchored: true,
        },
        ShownUnit {
            key: "newer".to_owned(),
            band: 0,
            anchored: true,
        },
    ];
    store
        .swap_shown_units(as_shown(Some("S"), &drawn), "newer", remote)
        .unwrap()
        .unwrap();
    let after = store.read_wt_state().unwrap();
    assert!(after["slugs"]["newer"].get("creationSlot").is_none());
    assert!(after["slugs"]["newer"]["creationAfter"].is_null());
    assert_eq!(after["remoteLayouts"][remote]["creationAfter"], "newer");
    // A deleted section's rows keep their order in the Inbox, and a row at
    // its creation position goes on following its row.
    store.write_repository_state_json(state).unwrap();
    store.place_slug("first", Some("S"), true).unwrap();
    store.remove_section("S").unwrap();
    let after = store.read_wt_state().unwrap();
    assert_eq!(
        by_stored_order(&mut store, &["first", "x", "ordered"]),
        ["first", "x", "ordered"]
    );
    assert_eq!(after["slugs"]["new"]["creationAfter"], "x");
    assert!(after["slugs"]["new"]["order"].is_null());
    assert!(after["slugs"]["new"]["section"].is_null());
}

#[test]
fn a_section_moves_past_the_one_shown_or_is_refused() {
    let temp = tempdir().unwrap();
    let mut store = Store::open(
        temp.path().join("wt.sqlite"),
        identity("repo-a", "/repos/a"),
    )
    .unwrap();
    store
        .write_repository_state_json(r#"{"sectionsOrder":["One","Two","Three"]}"#)
        .unwrap();
    let order = |store: &mut Store| -> Vec<String> {
        store.read_wt_state().unwrap()["sectionsOrder"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|name| name.as_str())
            .filter(|name| !name.starts_with('\0'))
            .map(str::to_owned)
            .collect()
    };
    let shown = keys(&["One", "Two", "Three"]);
    assert_eq!(
        store.move_group_past("One", "Two", false, &shown).unwrap(),
        Ok(true)
    );
    assert_eq!(order(&mut store), ["Two", "One", "Three"]);
    // The same press from the order it no longer describes.
    assert_eq!(
        store.move_group_past("One", "Two", false, &shown).unwrap(),
        Err(LayoutChanged::Order("One".into(), "Two".into()))
    );
    let shown = keys(&["Two", "One", "Three"]);
    assert_eq!(
        store
            .move_group_past("One", "Three", false, &shown)
            .unwrap(),
        Ok(true)
    );
    assert_eq!(order(&mut store), ["Two", "Three", "One"]);
    store.remove_section("Three").unwrap();
    let shown = keys(&["Two", "Three", "One"]);
    assert_eq!(
        store.move_group_past("One", "Three", true, &shown).unwrap(),
        Err(LayoutChanged::Section("Three".into()))
    );
    assert_eq!(order(&mut store), ["Two", "One"]);
}

#[test]
fn merge_edges_upsert_by_ordered_pair_and_prune_dead_endpoints() {
    let temp = tempdir().unwrap();
    let mut store = Store::open(
        temp.path().join("wt.sqlite"),
        identity("repo-a", "/repos/a"),
    )
    .unwrap();
    let edge = |why: &str| MergeEdge {
        from: "a".to_owned(),
        to: "b".to_owned(),
        kind: "before".to_owned(),
        strength: "blocks".to_owned(),
        at: "t".to_owned(),
        by: "fleet".to_owned(),
        why: Some(why.to_owned()),
        from_sha: Some("a1".to_owned()),
        to_sha: Some("b1".to_owned()),
        extra: Map::new(),
    };
    store.set_merge_edge(&edge("first reason")).unwrap();
    store.set_merge_edge(&edge("updated reason")).unwrap();
    let state: serde_json::Value =
        serde_json::from_str(&store.read_repository_state_json().unwrap().unwrap()).unwrap();
    assert_eq!(state["edges"].as_array().unwrap().len(), 1);
    assert_eq!(state["edges"][0]["why"], "updated reason");
    assert!(
        store
            .prune_merge_edges(&BTreeSet::from(["a".to_owned()]))
            .unwrap()
    );
    assert!(!store.remove_merge_edge("a", "b").unwrap());
}

#[test]
fn section_layout_mutations_keep_controller_remote_and_local_state_separate() {
    let temp = tempdir().unwrap();
    let mut store = Store::open(
        temp.path().join("wt.sqlite"),
        identity("repo-a", "/repos/a"),
    )
    .unwrap();
    store
        .write_repository_state_json(
            r#"{"version":17,"remoteLayouts":{"@remote/server/remote%2Fslug":{"section":"Old","order":4,"futureLayout":{"x":1}}}}"#,
        )
        .unwrap();
    store.place_slug("local", Some("Old"), false).unwrap();
    store
        .set_worktree_section("@remote/server/remote%2Fslug", Some("Old"))
        .unwrap();
    store.rename_section("Old", "New").unwrap();
    let state: serde_json::Value =
        serde_json::from_str(&store.read_repository_state_json().unwrap().unwrap()).unwrap();
    assert_eq!(state["slugs"]["local"]["section"], "New");
    assert_eq!(
        state["remoteLayouts"]["@remote/server/remote%2Fslug"]["section"],
        "New"
    );
    assert_eq!(
        state["remoteLayouts"]["@remote/server/remote%2Fslug"]["futureLayout"]["x"],
        1
    );
    assert_eq!(state["sectionsOrder"], json!(["\0inbox", "New"]));
    assert_eq!(store.remove_section("New").unwrap(), 2);
    let state: serde_json::Value =
        serde_json::from_str(&store.read_repository_state_json().unwrap().unwrap()).unwrap();
    assert!(state["slugs"]["local"]["section"].is_null());
    assert!(state["remoteLayouts"]["@remote/server/remote%2Fslug"]["section"].is_null());
}

#[test]
fn batch_section_move_preserves_fields_remote_ownership_and_existing_positions() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("wt.sqlite");
    let mut store = Store::open(&path, identity("repo", "/repo")).unwrap();
    store.write_repository_state_json(r#"{"slugs":{"one":{"section":"Old","future":42},"already":{"section":"New","order":5}},"remoteLayouts":{"@remote/host/two":{"section":"Old","future":[1,2]}}}"#).unwrap();
    let keys = ["one", "@remote/host/two", "already", "one"].map(str::to_owned);
    assert_eq!(
        store.move_worktrees_to_section(&keys, Some("New")).unwrap(),
        ["one", "@remote/host/two"]
    );
    let state = store.read_wt_state().unwrap();
    assert_eq!(state["slugs"]["one"]["future"], 42);
    assert_eq!(
        state["remoteLayouts"]["@remote/host/two"]["future"],
        json!([1, 2])
    );
    assert!(state["slugs"]["@remote/host/two"].is_null());
    assert_eq!(state["slugs"]["already"]["order"], 5);
    assert_eq!(state["slugs"]["one"]["order"], 6.0);
    assert_eq!(state["remoteLayouts"]["@remote/host/two"]["order"], 7.0);
    assert!(
        store
            .move_worktrees_to_section(&keys, Some("New"))
            .unwrap()
            .is_empty()
    );
    assert_eq!(store.read_wt_state().unwrap(), state);
}

#[test]
fn explicit_empty_issue_override_and_attention_watermark_keep_their_meaning() {
    let temp = tempdir().unwrap();
    let mut store = Store::open(
        temp.path().join("wt.sqlite"),
        identity("repo-a", "/repos/a"),
    )
    .unwrap();
    store.set_slug_issue_id("task", Some("")).unwrap();
    store.set_attention_seen(100).unwrap();
    store.set_attention_seen(50).unwrap();
    let state: serde_json::Value =
        serde_json::from_str(&store.read_repository_state_json().unwrap().unwrap()).unwrap();
    assert_eq!(state["slugs"]["task"]["issueId"], "");
    assert_eq!(state["attentionSeenTs"], 100);
    store.set_slug_issue_id("task", None).unwrap();
    let state: serde_json::Value =
        serde_json::from_str(&store.read_repository_state_json().unwrap().unwrap()).unwrap();
    assert!(state["slugs"]["task"].get("issueId").is_none());
}

#[test]
fn mutation_of_newer_state_keeps_its_version_and_unrecognized_fields() {
    let temp = tempdir().unwrap();
    let mut store = Store::open(
        temp.path().join("wt.sqlite"),
        identity("repo-a", "/repos/a"),
    )
    .unwrap();
    store
        .write_repository_state_json(r#"{"version":99,"future":{"unknown":true}}"#)
        .unwrap();
    store.set_branch_tip("trunk", "new-sha").unwrap();
    let state: serde_json::Value =
        serde_json::from_str(&store.read_repository_state_json().unwrap().unwrap()).unwrap();
    assert_eq!(state["version"], 99);
    assert_eq!(state["future"]["unknown"], true);
    assert_eq!(state["branchTips"]["trunk"], "new-sha");
}

#[test]
fn dev_port_claims_ignore_reservations_of_slugs_without_a_live_worktree() {
    let temp = tempdir().unwrap();
    let mut store = Store::open(
        temp.path().join("wt.sqlite"),
        identity("repo-a", "/repos/a"),
    )
    .unwrap();
    store.set_slug_dev_port("live", Some(4500)).unwrap();
    store.set_slug_dev_port("gone", Some(4501)).unwrap();
    let live: BTreeSet<String> = ["live", "new"].map(str::to_owned).into();
    assert_eq!(
        store.claim_dev_port("new", &[4500, 4501], &live).unwrap(),
        Some(4501)
    );
    assert_eq!(store.read_slug_dev_port("live").unwrap(), Some(4500));
}

#[test]
fn recording_a_removal_releases_only_that_slugs_dev_port() {
    let temp = tempdir().unwrap();
    let mut store = Store::open(
        temp.path().join("wt.sqlite"),
        identity("repo-a", "/repos/a"),
    )
    .unwrap();
    store.set_slug_dev_port("gone", Some(4500)).unwrap();
    store.set_slug_dev_port("kept", Some(4501)).unwrap();
    store
        .record_removed_worktrees(
            &[RemovedWorktree {
                slug: "gone".into(),
                branch: "feature/gone".into(),
                removed_at: "2026-10-09T00:00:00Z".into(),
                work: None,
                automations_paused: None,
                extra: Default::default(),
            }],
            1_792_000_000_000,
        )
        .unwrap();
    assert_eq!(store.read_slug_dev_port("gone").unwrap(), None);
    assert_eq!(store.read_slug_dev_port("kept").unwrap(), Some(4501));
}

#[test]
fn dev_port_claims_skip_existing_allocations_and_automation_toggles_persist() {
    let temp = tempdir().unwrap();
    let mut store = Store::open(
        temp.path().join("wt.sqlite"),
        identity("repo-a", "/repos/a"),
    )
    .unwrap();
    let live: BTreeSet<String> = ["a", "b", "c"].map(str::to_owned).into();
    assert_eq!(
        store.claim_dev_port("a", &[4500, 4501], &live).unwrap(),
        Some(4500)
    );
    assert_eq!(
        store.claim_dev_port("b", &[4500, 4501], &live).unwrap(),
        Some(4501)
    );
    assert_eq!(store.read_slug_dev_port("a").unwrap(), Some(4500));
    assert_eq!(
        store.claim_dev_port("c", &[4500, 4501], &live).unwrap(),
        None
    );
    assert!(store.toggle_slug_automations_paused("a").unwrap());
    assert!(
        store
            .toggle_stack_automations_paused("stack-root", &["a".to_owned(), "b".to_owned()])
            .unwrap()
    );
    assert!(store.toggle_global_automations_paused().unwrap());
    assert!(!store.is_section_folded("unknown").unwrap());
    let state = store.read_wt_state().unwrap();
    assert_eq!(state["automationsPaused"], true);
    assert_eq!(state["pausedStacks"], json!(["stack-root"]));
    assert_eq!(state["slugs"]["a"]["automationsPaused"], true);
    assert_eq!(state["slugs"]["b"]["automationsPaused"], true);
}

#[test]
fn witnessed_toggles_compare_and_write_in_one_transaction() {
    use crate::SeenToggle;
    let temp = tempdir().unwrap();
    let mut store = Store::open(
        temp.path().join("wt.sqlite"),
        identity("repo-a", "/repos/a"),
    )
    .unwrap();
    assert_eq!(
        store
            .toggle_global_automations_paused_seen(Some(false))
            .unwrap(),
        SeenToggle::Set(true)
    );
    // A board that still showed "running" must not resume it.
    assert_eq!(
        store
            .toggle_global_automations_paused_seen(Some(false))
            .unwrap(),
        SeenToggle::Stale { current: true }
    );
    assert_eq!(store.read_wt_state().unwrap()["automationsPaused"], true);

    assert_eq!(
        store
            .toggle_slug_automations_paused_seen("a", Some(true))
            .unwrap(),
        SeenToggle::Stale { current: false }
    );
    assert!(store.read_wt_state().unwrap()["slugs"].get("a").is_none());
    assert_eq!(
        store
            .toggle_slug_automations_paused_seen("a", Some(false))
            .unwrap(),
        SeenToggle::Set(true)
    );

    // The stack marker is unset but row "a" is paused: the user saw "paused",
    // so one write resumes the whole stack without a transient pause.
    let members = ["a".to_owned(), "b".to_owned()];
    assert_eq!(
        store
            .toggle_stack_automations_paused_seen("stack", &members, Some(("a", false)))
            .unwrap(),
        SeenToggle::Stale { current: true }
    );
    assert_eq!(
        store
            .toggle_stack_automations_paused_seen("stack", &members, Some(("a", true)))
            .unwrap(),
        SeenToggle::Set(false)
    );
    let state = store.read_wt_state().unwrap();
    assert_eq!(state["pausedStacks"], json!([]));
    assert!(state["slugs"]["a"].get("automationsPaused").is_none());
    assert!(state["slugs"]["b"].get("automationsPaused").is_none());
    assert_eq!(
        store
            .toggle_stack_automations_paused_seen("stack", &members, Some(("b", false)))
            .unwrap(),
        SeenToggle::Set(true)
    );
    let state = store.read_wt_state().unwrap();
    assert_eq!(state["pausedStacks"], json!(["stack"]));
    assert_eq!(state["slugs"]["a"]["automationsPaused"], true);
    assert_eq!(state["slugs"]["b"]["automationsPaused"], true);
}

#[test]
fn witnessed_archive_moves_layout_with_the_marker() {
    use crate::SeenToggle;
    let temp = tempdir().unwrap();
    let mut store = Store::open(
        temp.path().join("wt.sqlite"),
        identity("repo-a", "/repos/a"),
    )
    .unwrap();
    store
        .move_worktrees_to_section(&["one".to_owned()], Some("Later"))
        .unwrap();
    assert_eq!(
        store
            .toggle_archived_seen("one", Some(true), "\0archived")
            .unwrap(),
        SeenToggle::Stale { current: false }
    );
    assert!(!store.read_archived_keys().unwrap().contains("one"));
    assert!(!store.is_section_folded("\0archived").unwrap());
    assert_eq!(
        store
            .toggle_archived_seen("one", Some(false), "\0archived")
            .unwrap(),
        SeenToggle::Set(true)
    );
    assert!(store.read_archived_keys().unwrap().contains("one"));
    assert!(store.is_section_folded("\0archived").unwrap());
    assert_eq!(
        store
            .toggle_archived_seen("one", Some(false), "\0archived")
            .unwrap(),
        SeenToggle::Stale { current: true }
    );
    assert_eq!(
        store
            .toggle_archived_seen("one", None, "\0archived")
            .unwrap(),
        SeenToggle::Set(false)
    );
    assert!(store.read_archived_keys().unwrap().is_empty());
    assert_eq!(
        store.read_wt_state().unwrap()["slugs"]["one"]["section"],
        serde_json::Value::Null
    );
}

#[test]
fn new_worktree_position_preserves_neighbors_and_survives_restart() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("state.sqlite");
    let mut store = Store::open(&path, identity("repo", "/repo")).unwrap();
    store.write_repository_state_json(r#"{"sectionsOrder":["Batch"],"foldedSections":["Batch"],"slugs":{"origin":{"section":"Batch","order":7,"manualTitle":"Keep"}},"remoteLayouts":{"@remote/box/new":{"future":42}}}"#).unwrap();
    let before = store.read_wt_state().unwrap();
    assert!(
        store
            .place_new_worktree("@remote/box/new", Some("Batch"), Some("origin"))
            .unwrap()
    );
    let state = store.read_wt_state().unwrap();
    assert_eq!(state["slugs"]["origin"], before["slugs"]["origin"]);
    assert_eq!(state["remoteLayouts"]["@remote/box/new"]["future"], 42);
    assert_eq!(
        state["remoteLayouts"]["@remote/box/new"]["creationAfter"],
        "origin"
    );
    assert!(state["foldedSections"].as_array().unwrap().is_empty());
    drop(store);
    let mut store = Store::open(&path, identity("repo", "/repo")).unwrap();
    assert_eq!(store.read_wt_state().unwrap(), state);
}

#[test]
fn a_changed_creation_section_is_refused_without_writing() {
    let temp = tempdir().unwrap();
    let mut store =
        Store::open(temp.path().join("state.sqlite"), identity("repo", "/repo")).unwrap();
    store
        .write_repository_state_json(
            r#"{"sectionsOrder":["Batch","Other"],"slugs":{"origin":{"section":"Other"}}}"#,
        )
        .unwrap();
    let before = store.read_wt_state().unwrap();
    assert!(
        !store
            .place_new_worktree("new", Some("Batch"), Some("origin"))
            .unwrap()
    );
    assert!(
        !store
            .place_new_worktree("new", Some("Removed"), None)
            .unwrap()
    );
    assert_eq!(store.read_wt_state().unwrap(), before);
    assert!(store.place_new_worktree("new", None, None).unwrap());
    let state = store.read_wt_state().unwrap();
    assert!(state["slugs"]["new"]["section"].is_null());
    assert!(
        state["slugs"]["new"]
            .as_object()
            .unwrap()
            .contains_key("creationAfter")
    );
}

#[test]
fn sections_outlive_their_last_worktree_until_deleted() {
    let temp = tempdir().unwrap();
    let mut store = Store::open(
        temp.path().join("wt.sqlite"),
        identity("repo-a", "/repos/a"),
    )
    .unwrap();
    let state = |store: &mut Store| -> serde_json::Value {
        serde_json::from_str(&store.read_repository_state_json().unwrap().unwrap()).unwrap()
    };
    store.place_slug("one", Some("Batch"), false).unwrap();
    store
        .move_worktrees_to_section(&["one".into()], None)
        .unwrap();
    store.clear_slug_state("one").unwrap();
    assert_eq!(
        state(&mut store)["sectionsOrder"],
        json!(["\0inbox", "Batch"])
    );
    assert_eq!(store.remove_section("Batch").unwrap(), 0);
    assert_eq!(state(&mut store)["sectionsOrder"], json!(["\0inbox"]));
}

#[test]
fn adopting_explicit_sections_keeps_only_live_ones_once() {
    let temp = tempdir().unwrap();
    let mut store = Store::open(
        temp.path().join("wt.sqlite"),
        identity("repo-a", "/repos/a"),
    )
    .unwrap();
    store
        .write_repository_state_json(
            r#"{"version":17,"sectionsOrder":["\u0000inbox","Live","Stale","Remote","Empty","Placed","Creating"],
               "foldedSections":["Stale","Live"],
               "slugs":{"live":{"section":"Live"},"gone":{"section":"Stale"},
                        "old":{"section":"Placed","creationAfter":"x","order":3},
                        "new":{"section":"Creating","creationAfter":"x"}},
               "remoteLayouts":{"@remote/h/r":{"section":"Remote"}}}"#,
        )
        .unwrap();
    let live = std::collections::BTreeSet::from(["live".to_owned()]);
    assert!(store.adopt_explicit_sections(&live).unwrap());
    let state: serde_json::Value =
        serde_json::from_str(&store.read_repository_state_json().unwrap().unwrap()).unwrap();
    // A creation still in flight counts as live; a settled one does not.
    assert_eq!(
        state["sectionsOrder"],
        json!(["\0inbox", "Live", "Remote", "Creating"])
    );
    assert_eq!(state["foldedSections"], json!(["Live"]));
    // The stale reference is cleared so loading cannot resurrect it.
    assert!(state["slugs"]["gone"]["section"].is_null());
    // Afterwards an emptied section stays, and adoption never runs again.
    store.place_slug("live", None, false).unwrap();
    assert!(!store.adopt_explicit_sections(&live).unwrap());
    let state: serde_json::Value =
        serde_json::from_str(&store.read_repository_state_json().unwrap().unwrap()).unwrap();
    assert_eq!(
        state["sectionsOrder"],
        json!(["\0inbox", "Live", "Remote", "Creating"])
    );
}

#[test]
fn a_removal_records_a_later_production_landing_once() {
    let temp = tempdir().unwrap();
    let mut store = Store::open(
        temp.path().join("wt.sqlite"),
        identity("repo-a", "/repos/a"),
    )
    .unwrap();
    store
        .write_repository_state_json(
            r#"{"version":18,"removed":[
                {"slug":"one","branch":"b","removedAt":"t1","landedOnAtRemoval":"base"},
                {"slug":"one","branch":"b","removedAt":"t0","landedOnAtRemoval":"base"}]}"#,
        )
        .unwrap();
    let landed = [("one".to_owned(), "t1".to_owned())];
    assert!(store.record_removed_production_landings(&landed).unwrap());
    assert!(!store.record_removed_production_landings(&landed).unwrap());
    let state: serde_json::Value =
        serde_json::from_str(&store.read_repository_state_json().unwrap().unwrap()).unwrap();
    assert_eq!(state["removed"][0]["landedOn"], "production");
    // Only the exact removal: an older record of the same slug is untouched.
    assert!(state["removed"][1].get("landedOn").is_none());
}

#[test]
fn status_update_reads_and_writes_in_one_transaction() {
    use std::{convert::Infallible, sync::mpsc};

    let temp = tempdir().unwrap();
    let path = temp.path().join("state.sqlite");
    let mut first = Store::open(&path, identity("repo", "/repo")).unwrap();
    let mut second = Store::open(&path, identity("repo", "/repo")).unwrap();
    let status = |state: &str, verify_after_merge: Option<&str>| WorkStatusRecord {
        state: state.into(),
        at: "t".into(),
        note: None,
        risk: None,
        sha: None,
        by: None,
        blocked_on: None,
        verify_after_merge: verify_after_merge.map(str::to_owned),
        verify_in: verify_after_merge.map(|_| "production".to_owned()),
        extra: Map::new(),
    };
    let carried = |state: &'static str| {
        move |previous: Option<&WorkStatusRecord>| {
            let mut next = status(state, None);
            if let Some(previous) = previous {
                next.verify_after_merge = previous.verify_after_merge.clone();
                next.verify_in = previous.verify_in.clone();
            }
            Ok::<_, Infallible>(Some(next))
        }
    };

    // Another process asserts an owed check while this one is between
    // reading the previous record and writing its own.
    let owed = status("ready", Some("STEPS: 1. check production"));
    let (reading_tx, reading_rx) = mpsc::channel();
    let other = thread::spawn(move || {
        reading_rx.recv().unwrap();
        second.set_slug_work_status("task", Some(&owed)).unwrap();
    });
    first
        .update_slug_work_status("task", |previous| {
            reading_tx.send(()).unwrap();
            thread::sleep(Duration::from_millis(300));
            carried("review")(previous)
        })
        .unwrap()
        .unwrap();
    other.join().unwrap();
    assert_eq!(
        first.read_wt_state().unwrap()["slugs"]["task"]["work"]["verifyAfterMerge"],
        "STEPS: 1. check production",
        "the other writer's owed check was overwritten from a stale read"
    );

    let write = first
        .update_slug_work_status("task", carried("needs-testing"))
        .unwrap()
        .unwrap();
    assert_eq!(write.previous.unwrap().state, "ready");
    assert!(write.changed);
    let work = &first.read_wt_state().unwrap()["slugs"]["task"]["work"];
    assert_eq!(work["state"], "needs-testing");
    assert_eq!(work["verifyAfterMerge"], "STEPS: 1. check production");

    assert_eq!(
        first
            .update_slug_work_status("task", |_| Err::<Option<WorkStatusRecord>, _>("stale"))
            .unwrap(),
        Err("stale")
    );
    assert_eq!(
        first.read_wt_state().unwrap()["slugs"]["task"]["work"]["state"],
        "needs-testing"
    );
}

#[test]
fn witnessed_section_writes_compare_and_write_in_one_transaction() {
    use std::sync::mpsc;

    let temp = tempdir().unwrap();
    let path = temp.path().join("state.sqlite");
    let mut first = Store::open(&path, identity("repo", "/repo")).unwrap();
    let keys = |keys: &[&str]| keys.iter().map(|key| (*key).to_owned()).collect::<Vec<_>>();
    first
        .move_worktrees_to_section(&keys(&["task", "one"]), Some("A"))
        .unwrap();
    let section = |store: &mut Store, key: &str| {
        store.read_wt_state().unwrap()["slugs"][key]["section"]
            .as_str()
            .map(str::to_owned)
    };
    let shows = |key: &'static str, seen: Option<&'static str>| {
        move |state: &WtState| {
            (state["slugs"][key]["section"].as_str() == seen)
                .then_some(())
                .ok_or("stale")
        }
    };

    // Another process refiles the row while this one is between comparing
    // its witness and writing.
    let mut second = Store::open(&path, identity("repo", "/repo")).unwrap();
    let (checking_tx, checking_rx) = mpsc::channel();
    let other = thread::spawn(move || {
        checking_rx.recv().unwrap();
        second
            .move_worktrees_to_section(&["task".to_owned()], Some("C"))
            .unwrap();
    });
    let moved = first
        .move_worktrees_to_section_checked(&keys(&["task"]), Some("B"), |state| {
            checking_tx.send(()).unwrap();
            thread::sleep(Duration::from_millis(300));
            shows("task", Some("A"))(state)
        })
        .unwrap();
    other.join().unwrap();
    assert_eq!(moved, Ok(keys(&["task"])));
    assert_eq!(
        section(&mut first, "task").as_deref(),
        Some("C"),
        "the other writer's move was overwritten after a stale compare"
    );
    assert_eq!(
        first
            .move_worktrees_to_section_checked(&keys(&["task"]), None, shows("task", Some("B")))
            .unwrap(),
        Err("stale")
    );
    assert_eq!(section(&mut first, "task").as_deref(), Some("C"));

    // Another process files a row into the section while its deletion is
    // between comparing the members the user saw and removing it.
    let mut second = Store::open(&path, identity("repo", "/repo")).unwrap();
    let (checking_tx, checking_rx) = mpsc::channel();
    let other = thread::spawn(move || {
        checking_rx.recv().unwrap();
        second
            .move_worktrees_to_section(&["two".to_owned()], Some("A"))
            .unwrap();
    });
    let removed = first
        .remove_section_checked("A", |state| {
            checking_tx.send(()).unwrap();
            thread::sleep(Duration::from_millis(300));
            shows("two", None)(state)
        })
        .unwrap();
    other.join().unwrap();
    assert_eq!(removed, Ok(1));
    assert_eq!(section(&mut first, "one"), None);
    assert_eq!(
        section(&mut first, "two").as_deref(),
        Some("A"),
        "a row filed during the delete was sent to the Inbox unseen"
    );
    assert_eq!(
        first
            .remove_section_checked("A", shows("two", None))
            .unwrap(),
        Err("stale")
    );
    assert_eq!(section(&mut first, "two").as_deref(), Some("A"));
}

#[test]
fn removed_pause_toggle_refuses_a_flag_the_caller_did_not_see() {
    let temp = tempdir().unwrap();
    let mut store =
        Store::open(temp.path().join("state.sqlite"), identity("repo", "/repo")).unwrap();
    store
        .record_removed_worktrees(
            &[RemovedWorktree {
                slug: "gone".into(),
                branch: "feature/gone".into(),
                removed_at: "2026-10-09T00:00:00Z".into(),
                work: None,
                automations_paused: None,
                extra: Map::new(),
            }],
            1_791_540_000_000,
        )
        .unwrap();
    assert_eq!(
        store
            .toggle_removed_automations_paused_seen("gone", Some(false))
            .unwrap(),
        Some(SeenToggle::Set(true))
    );
    // The same press again, from a view that still shows it running.
    assert_eq!(
        store
            .toggle_removed_automations_paused_seen("gone", Some(false))
            .unwrap(),
        Some(SeenToggle::Stale { current: true })
    );
    assert_eq!(
        store.read_removed_worktrees().unwrap()[0].automations_paused,
        Some(true)
    );
    assert_eq!(
        store
            .toggle_removed_automations_paused_seen("absent", Some(false))
            .unwrap(),
        None
    );
}

#[test]
fn restoring_a_slug_entry_replaces_it_whole_and_leaves_other_slugs_alone() {
    let temp = tempdir().unwrap();
    let mut store =
        Store::open(temp.path().join("state.sqlite"), identity("repo", "/repo")).unwrap();
    store
        .write_repository_state_json(
            &json!({
                "version": crate::CURRENT_WT_STATE_VERSION,
                "slugs": {
                    "task": {"section": "A", "order": 2, "futureField": {"kept": true}},
                    "other": {"section": "B"}
                }
            })
            .to_string(),
        )
        .unwrap();
    let before = store.read_slug_state("task").unwrap().unwrap();

    store
        .move_worktrees_to_section(&["task".to_owned()], Some("B"))
        .unwrap();
    assert!(store.toggle_slug_automations_paused("task").unwrap());
    assert!(store.restore_slug_state("task", before.clone()).unwrap());
    assert_eq!(store.read_slug_state("task").unwrap(), Some(before.clone()));
    assert!(!store.restore_slug_state("task", before.clone()).unwrap());

    assert!(store.clear_slug_state("task").unwrap());
    assert!(store.restore_slug_state("task", before.clone()).unwrap());
    assert_eq!(store.read_slug_state("task").unwrap(), Some(before));
    assert_eq!(
        store.read_slug_state("other").unwrap(),
        Some(json!({"section": "B"}))
    );
}

#[test]
fn an_armed_merge_is_one_record_per_slug_cleared_by_cancel_end_or_removal() {
    use wt_core::{MergeArm, MergeMechanism};
    let temp = tempdir().unwrap();
    let mut store =
        Store::open(temp.path().join("state.sqlite"), identity("repo", "/repo")).unwrap();
    store
        .record_slug_created("one", "2026-11-01T00:00:00Z")
        .unwrap();
    let arm = |armed_at_ms| MergeArm {
        pr: 7,
        head: Some("abc".into()),
        armed_at_ms,
        mechanism: MergeMechanism::Queue,
        seen_armed_at_ms: None,
    };
    assert!(store.set_slug_merge_arm("one", &arm(10)).unwrap());
    assert!(!store.set_slug_merge_arm("one", &arm(10)).unwrap());
    let stored = store.read_wt_state().unwrap()["slugs"]["one"].clone();
    assert_eq!(
        stored["mergeArm"],
        json!({"pr": 7, "head": "abc", "armedAtMs": 10, "mechanism": "queue"})
    );
    // The slug's other fields are untouched.
    assert_eq!(stored["createdAt"], "2026-11-01T00:00:00Z");

    // Arming again replaces the record; whoever saw the first arming end
    // does not clear the second.
    assert!(store.set_slug_merge_arm("one", &arm(20)).unwrap());
    assert!(!store.clear_slug_merge_arm("one", Some(10)).unwrap());
    assert_eq!(
        serde_json::from_value::<MergeArm>(
            store.read_wt_state().unwrap()["slugs"]["one"]["mergeArm"].clone()
        )
        .unwrap(),
        arm(20)
    );
    assert!(store.clear_slug_merge_arm("one", Some(20)).unwrap());
    assert!(!store.clear_slug_merge_arm("one", None).unwrap());
    assert!(!store.clear_slug_merge_arm("absent", None).unwrap());
    let cleared = store.read_wt_state().unwrap()["slugs"]["one"].clone();
    assert!(cleared.get("mergeArm").is_none());
    assert_eq!(cleared["createdAt"], "2026-11-01T00:00:00Z");

    // GitHub seen holding an arming is recorded once, on that arming only.
    store.set_slug_merge_arm("one", &arm(25)).unwrap();
    assert!(!store.note_slug_merge_arm_seen("one", 20, 26).unwrap());
    assert!(!store.note_slug_merge_arm_seen("absent", 25, 26).unwrap());
    assert!(store.note_slug_merge_arm_seen("one", 25, 26).unwrap());
    assert!(!store.note_slug_merge_arm_seen("one", 25, 99).unwrap());
    assert_eq!(
        serde_json::from_value::<MergeArm>(
            store.read_wt_state().unwrap()["slugs"]["one"]["mergeArm"].clone()
        )
        .unwrap()
        .seen_armed_at_ms,
        Some(26)
    );

    // A cancel clears whatever arming there is, and removal takes the
    // record with the slug's other state.
    store.set_slug_merge_arm("one", &arm(30)).unwrap();
    assert!(store.clear_slug_merge_arm("one", None).unwrap());
    store.set_slug_merge_arm("one", &arm(40)).unwrap();
    store.reap_wt_state(&BTreeSet::new()).unwrap();
    assert!(store.read_wt_state().unwrap()["slugs"]["one"].is_null());
}

/// The metadata reader takes the state and the archived keys together. A
/// write that lands between its two statements must show in both or in
/// neither, on the read-only connection and on a writable one.
#[test]
fn state_and_archived_keys_are_read_as_one_snapshot() {
    let temp = tempdir().unwrap();
    let path = temp.path().join("state.sqlite");
    let mut writer = Store::open(&path, identity("repo", "/repo")).unwrap();
    let keys = |key: &str| BTreeSet::from([key.to_owned()]);
    let mut seen = 1;
    writer.set_attention_seen(seen).unwrap();
    writer.replace_archived_keys(&keys("at-1")).unwrap();
    for read_only in [true, false] {
        let mut reader = if read_only {
            Store::open_read_only(&path, identity("repo", "/repo")).unwrap()
        } else {
            Store::open(&path, identity("repo", "/repo")).unwrap()
        };
        let before = seen;
        seen += 1;
        let (state, archived) = reader
            .read_state_and_archived(|| {
                writer.set_attention_seen(seen).unwrap();
                writer
                    .replace_archived_keys(&keys(&format!("at-{seen}")))
                    .unwrap();
            })
            .unwrap();
        assert_eq!(state["attentionSeenTs"], before);
        assert_eq!(archived, keys(&format!("at-{before}")), "a mixed pair");
        let (state, archived) = reader.read_wt_state_and_archived_keys().unwrap();
        assert_eq!(state["attentionSeenTs"], seen);
        assert_eq!(archived, keys(&format!("at-{seen}")));
    }
}
