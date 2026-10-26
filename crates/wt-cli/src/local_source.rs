//! Git facts and durable presentation state have independent invalidation.
//! Editing a title or status must not respawn Git across the whole fleet.
use std::{collections::BTreeSet, sync::Arc, time::Duration};

use serde_json::Value;
use wt_config::Config;
use wt_core::Domain;
use wt_runtime::{
    DerivedPublisher, DiskCache, LeafHandle, Persistence, RefreshPolicy, SourceHandle,
    SourceSnapshot, SourceSpec, SourceState, TaskScope, derived_channel, start_source,
};
use wt_tui::Board;
use wt_vcs::WorktreeSnapshot;

use crate::{freshness, inventory};
use wt_app_core::context::AppContext;

pub type Metadata = (Value, BTreeSet<String>);

pub struct LocalSources {
    pub board: SourceHandle<Board>,
    pub metadata: LeafHandle<Metadata>,
    pub git: LeafHandle<Vec<WorktreeSnapshot>>,
    pub edits: SourceHandle<std::collections::BTreeMap<String, i64>>,
}

pub fn start(scope: &TaskScope, context: &AppContext) -> LocalSources {
    let git = start_source(scope, git_spec(context), {
        let repository = context.repository.clone();
        move |cancel| {
            let repository = repository.clone();
            async move { repository.inventory_status(&cancel).await }
        }
    });
    let metadata = start_source(scope, metadata_spec(), {
        let context = context.clone();
        move |_cancel| {
            let context = context.clone();
            async move {
                let state = context
                    .database
                    .call(|store| Ok(store.read_wt_state()?))
                    .await?;
                crate::commands::_destroy::reconcile_removals(&context, &state).await?;
                context
                    .database
                    .call(|store| Ok((store.read_wt_state()?, store.read_archived_keys()?)))
                    .await
            }
        }
    });
    let board = project(scope, context.config.clone(), git.view(), metadata.view());
    let edits = freshness::start(
        scope,
        context.config.clone(),
        board.clone(),
        git.view(),
        metadata.view(),
    );
    LocalSources {
        board,
        metadata,
        git,
        edits,
    }
}

/// The Git inventory survives restarts as restored display data; only a
/// live scan can confirm it.
fn git_spec(context: &AppContext) -> SourceSpec<Vec<WorktreeSnapshot>> {
    SourceSpec {
        name: "git",
        domain: Domain::Git,
        persistence: Persistence::Disk(DiskCache::new(&context.caches)),
        policy: RefreshPolicy {
            debounce: Duration::from_millis(25),
            minimum_interval: Duration::from_millis(100),
        },
    }
}

fn metadata_spec() -> SourceSpec<Metadata> {
    SourceSpec {
        name: "metadata",
        domain: Domain::Metadata,
        persistence: Persistence::Never("authoritative SQLite; read directly"),
        policy: RefreshPolicy {
            debounce: Duration::from_millis(15),
            minimum_interval: Duration::from_millis(50),
        },
    }
}

fn project(
    scope: &TaskScope,
    config: Arc<Config>,
    git: SourceHandle<Vec<WorktreeSnapshot>>,
    metadata: SourceHandle<Metadata>,
) -> SourceHandle<Board> {
    let (source, publisher) = derived_channel(scope, "local-board");
    let cancellation = scope.token();
    scope.spawn(run_projection(
        cancellation,
        publisher,
        config,
        git,
        metadata,
    ));
    source
}

async fn run_projection(
    cancellation: tokio_util::sync::CancellationToken,
    mut publisher: DerivedPublisher<Board>,
    config: Arc<Config>,
    git: SourceHandle<Vec<WorktreeSnapshot>>,
    metadata: SourceHandle<Metadata>,
) {
    let mut git_updates = git.subscribe();
    let mut metadata_updates = metadata.subscribe();
    git_updates.mark_changed();
    metadata_updates.mark_changed();
    loop {
        tokio::select! {
            biased;
            _ = cancellation.cancelled() => break,
            request = publisher.requested() => {
                if request.is_none() { break; }
                git.refresh(); metadata.refresh(); continue;
            }
            changed = git_updates.changed() => {
                if changed.is_err() { break; }
                git_updates.borrow_and_update();
            }
            changed = metadata_updates.changed() => {
                if changed.is_err() { break; }
                metadata_updates.borrow_and_update();
            }
        }
        let facts = git_updates.borrow().clone();
        let state = metadata_updates.borrow().clone();
        let config = config.clone();
        let failure = facts.derive(None).with_input(&state);
        let snapshot =
            match tokio::task::spawn_blocking(move || compose(&config, facts, state)).await {
                Ok(snapshot) if !cancellation.is_cancelled() => snapshot,
                Ok(_) => break,
                Err(error) => {
                    failure.with_state(SourceState::Failed(format!("local board: {error}").into()))
                }
            };
        publisher.publish_if_changed(snapshot);
    }
}

fn compose(
    config: &Config,
    git: SourceSnapshot<Vec<WorktreeSnapshot>>,
    metadata: SourceSnapshot<Metadata>,
) -> SourceSnapshot<Board> {
    let state = SourceState::combine([("Git", git.state()), ("State", metadata.state())]);
    let data = git
        .data()
        .zip(metadata.data())
        .map(|(git, metadata)| Arc::new(inventory::board(config, git, &metadata.0, &metadata.1)));
    git.derive(data).with_input(&metadata).with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wt_runtime::{Stamp, leaf_channel};

    #[tokio::test]
    async fn metadata_changes_reuse_git_facts_while_git_is_busy_or_failed() {
        let fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        let scope = TaskScope::new();
        let (git, mut git_publisher) = leaf_channel(&scope, git_spec(&fixture.ctx));
        let (metadata, mut metadata_publisher) = leaf_channel(&scope, metadata_spec());
        let facts = fixture
            .ctx
            .repository
            .inventory_status(&fixture.ctx.cancellation)
            .await
            .unwrap();
        git_publisher.publish_live(facts.clone(), Stamp::now());
        let mut state = serde_json::json!({"slugs":{}});
        metadata_publisher.publish_live((state.clone(), BTreeSet::new()), Stamp::now());
        let source = project(
            &scope,
            fixture.ctx.config.clone(),
            git.view(),
            metadata.view(),
        );
        let mut updates = source.subscribe();
        tokio::time::timeout(
            Duration::from_secs(2),
            updates.wait_for(|s| s.data().is_some()),
        )
        .await
        .unwrap()
        .unwrap();
        git_publisher.set_state(SourceState::Refreshing);
        state["slugs"]["one"] = serde_json::json!({"manualTitle":"Edited during refresh"});
        metadata_publisher.publish_live((state.clone(), BTreeSet::new()), Stamp::now());
        tokio::time::timeout(
            Duration::from_secs(2),
            updates.wait_for(|s| {
                s.data()
                    .is_some_and(|b| b.rows.iter().any(|r| r.title == "Edited during refresh"))
            }),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(30), git_publisher.requested())
                .await
                .is_err()
        );
        git_publisher.set_state(SourceState::Failed("fixture Git failure".into()));
        state["slugs"]["one"]["manualTitle"] = "Edited after failure".into();
        metadata_publisher.publish_live((state, BTreeSet::new()), Stamp::now());
        tokio::time::timeout(
            Duration::from_secs(2),
            updates.wait_for(|s| {
                matches!(s.state(), SourceState::Failed(_))
                    && s.data()
                        .is_some_and(|b| b.rows.iter().any(|r| r.title == "Edited after failure"))
            }),
        )
        .await
        .unwrap()
        .unwrap();
        // Explicit refresh remains an all-source operation.
        source.refresh();
        assert_eq!(git_publisher.requested().await, Some(()));
        assert_eq!(metadata_publisher.requested().await, Some(()));
        scope.shutdown(Duration::from_secs(1)).await.unwrap();
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn restored_git_inventory_publishes_a_restored_board_before_any_fetch() {
        let mut fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        let caches = tempfile::tempdir().unwrap();
        fixture.ctx.caches = wt_runtime::CacheStore::new(caches.path().to_path_buf(), "test");
        let facts = fixture
            .ctx
            .repository
            .inventory_status(&fixture.ctx.cancellation)
            .await
            .unwrap();
        // A previous process saved its live inventory.
        {
            let scope = TaskScope::new();
            let (_git, publisher) = leaf_channel(&scope, git_spec(&fixture.ctx));
            publisher.publish_live(facts.clone(), Stamp::now());
            let path = caches.path().join("git.json");
            tokio::time::timeout(Duration::from_secs(5), async {
                while !path.exists() {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("git inventory cache written");
            scope.shutdown(Duration::from_secs(2)).await.unwrap();
        }
        // This process has not fetched Git at all; its publisher never runs.
        let scope = TaskScope::new();
        let (git, _git_publisher) = leaf_channel(&scope, git_spec(&fixture.ctx));
        let (metadata, metadata_publisher) = leaf_channel(&scope, metadata_spec());
        metadata_publisher.publish_live(
            (serde_json::json!({"slugs":{}}), BTreeSet::new()),
            Stamp::now(),
        );
        let source = project(
            &scope,
            fixture.ctx.config.clone(),
            git.view(),
            metadata.view(),
        );
        let mut updates = source.subscribe();
        let board = tokio::time::timeout(
            Duration::from_secs(5),
            updates.wait_for(|s| s.data().is_some()),
        )
        .await
        .unwrap()
        .unwrap()
        .clone();
        assert!(
            board.is_restored(),
            "the board says its Git facts are restored"
        );
        assert!(!board.domains()[&Domain::Git].is_live());
        assert!(board.domains()[&Domain::Metadata].is_live());
        assert_eq!(*board.state(), SourceState::Refreshing);
        assert!(
            board
                .data()
                .unwrap()
                .rows
                .iter()
                .any(|row| row.slug == "one")
        );
        assert!(
            git.confirmed().is_none(),
            "restored Git facts never authorize actions"
        );
        scope.shutdown(Duration::from_secs(1)).await.unwrap();
        fixture.close().await.unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn git_inventory_meets_the_persisted_source_contract() {
        let row = |slug: &str, branch: &str, main: bool, dirty: bool| {
            let path = if main {
                "/repo/main".to_owned()
            } else {
                format!("/repo/worktrees/{slug}")
            };
            let git_dir = if main {
                "/repo/main/.git".to_owned()
            } else {
                format!("/repo/main/.git/worktrees/{slug}")
            };
            serde_json::json!({
                "worktree": {
                    "target": {
                        "ref": {"kind": "local", "slug": slug},
                        "slug": slug,
                        "branch": branch,
                        "path": path,
                        "stage": "stage-7692c3ad35",
                        "location": {"kind": "local"}
                    },
                    "isMain": main,
                    "headSha": "a980b25659467cf7e44cb1db69e0118b049ece62",
                    "detached": false,
                    "kind": "linkedWorktree",
                    "locked": false,
                    "prunable": false,
                    "gitDir": git_dir,
                    "commonDir": "/repo/main/.git"
                },
                "status": {
                    "branch": branch,
                    "headSha": "a980b25659467cf7e44cb1db69e0118b049ece62",
                    "upstream": if main { None } else { Some(format!("origin/{branch}")) },
                    "ahead": if main { None } else { Some(2) },
                    "behind": if main { None } else { Some(0) },
                    "trackedChanges": u32::from(dirty),
                    "untrackedFiles": 0,
                    "dirty": dirty
                },
                "error": null
            })
        };
        let facts: Vec<WorktreeSnapshot> = serde_json::from_value(serde_json::json!([
            row("main", "main", true, false),
            row("one", "feature/one", false, true),
            row("two", "feature/two", false, false),
        ]))
        .unwrap();
        wt_runtime::conformance::persisted_source("git", Domain::Git, facts).await;
    }
}
