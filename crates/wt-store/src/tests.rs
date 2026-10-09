use std::{collections::BTreeSet, fs, thread, time::Duration};

use rusqlite::Connection;
use serde_json::{Map, json};
use tempfile::tempdir;

use crate::{
    CURRENT_WT_STATE_VERSION, MergeEdge, RemovedWorktree, RepositoryIdentity, Store, StoreError,
    WorkStatusRecord, migrate_wt_state,
};

fn identity(id: &str, path: &str) -> RepositoryIdentity {
    RepositoryIdentity::new(id, path)
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
                for _ in 0..100 {
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
fn manual_title_cas_counts_same_text_edit_and_rejects_delayed_result() {
    let temp = tempdir().unwrap();
    let mut store = Store::open(
        temp.path().join("wt.sqlite"),
        identity("repo-a", "/repos/a"),
    )
    .unwrap();
    store
        .write_repository_state_json(r#"{"version":17,"futureRoot":true}"#)
        .unwrap();
    assert!(
        store
            .set_slug_manual_title("task", " Pinned title ", Some(0))
            .unwrap()
    );
    assert!(
        store
            .set_slug_manual_title("task", "Pinned title", Some(1))
            .unwrap()
    );
    assert!(
        !store
            .set_slug_manual_title("task", "Delayed title", Some(1))
            .unwrap()
    );
    let state: serde_json::Value =
        serde_json::from_str(&store.read_repository_state_json().unwrap().unwrap()).unwrap();
    assert_eq!(state["slugs"]["task"]["manualTitle"], "Pinned title");
    assert_eq!(state["slugs"]["task"]["manualTitleRevision"], 2);
    assert_eq!(state["futureRoot"], true);
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
fn dev_port_claims_skip_existing_allocations_and_automation_toggles_persist() {
    let temp = tempdir().unwrap();
    let mut store = Store::open(
        temp.path().join("wt.sqlite"),
        identity("repo-a", "/repos/a"),
    )
    .unwrap();
    assert_eq!(
        store.claim_dev_port("a", &[4500, 4501]).unwrap(),
        Some(4500)
    );
    assert_eq!(
        store.claim_dev_port("b", &[4500, 4501]).unwrap(),
        Some(4501)
    );
    assert_eq!(store.read_slug_dev_port("a").unwrap(), Some(4500));
    assert_eq!(store.claim_dev_port("c", &[4500, 4501]).unwrap(), None);
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
