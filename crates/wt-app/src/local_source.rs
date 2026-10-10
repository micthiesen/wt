//! Git facts and durable presentation state have independent invalidation.
//! Editing a title or status must not respawn Git across the whole fleet.
use std::{collections::BTreeSet, sync::Arc, time::Duration};

use serde_json::Value;
use wt_config::Config;
use wt_runtime::{
    RefreshPolicy, SourceHandle, SourceSnapshot, SourceState, TaskScope, source_channel,
    start_source,
};
use wt_tui::Board;
use wt_vcs::WorktreeSnapshot;

use crate::{context::AppContext, freshness, inventory};

pub type Metadata = (Value, BTreeSet<String>);

pub struct LocalSources {
    pub board: SourceHandle<Board>,
    pub metadata: SourceHandle<Metadata>,
}

pub fn start(scope: &TaskScope, context: &AppContext) -> LocalSources {
    let git = start_source(
        scope,
        RefreshPolicy {
            debounce: Duration::from_millis(25),
            minimum_interval: Duration::from_millis(100),
        },
        {
            let repository = context.repository.clone();
            move |cancel| {
                let repository = repository.clone();
                async move { repository.inventory_status(&cancel).await }
            }
        },
    );
    let metadata = start_source(
        scope,
        RefreshPolicy {
            debounce: Duration::from_millis(15),
            minimum_interval: Duration::from_millis(50),
        },
        {
            let database = context.database.clone();
            move |_cancel| {
                let database = database.clone();
                async move {
                    database
                        .call(|store| Ok((store.read_wt_state()?, store.read_archived_keys()?)))
                        .await
                }
            }
        },
    );
    let board = project(scope, context.config.clone(), git.clone(), metadata.clone());
    freshness::start(
        scope,
        context.config.clone(),
        context.repository.clone(),
        board.clone(),
        git,
        metadata.clone(),
    );
    LocalSources { board, metadata }
}

fn project(
    scope: &TaskScope,
    config: Arc<Config>,
    git: SourceHandle<Vec<WorktreeSnapshot>>,
    metadata: SourceHandle<Metadata>,
) -> SourceHandle<Board> {
    let (source, mut publisher) = source_channel();
    let cancellation = scope.token();
    scope.spawn(async move {
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
            match tokio::task::spawn_blocking(move || compose(&config, facts, state)).await {
                Ok(snapshot) if !cancellation.is_cancelled() => publisher.publish(snapshot),
                Ok(_) => break,
                Err(error) => {
                    publisher.publish(SourceSnapshot {
                        data: None,
                        state: SourceState::Failed(format!("local board: {error}").into()),
                        updated_at: None,
                        revision: 0,
                    });
                }
            }
        }
    });
    source
}

fn compose(
    config: &Config,
    git: SourceSnapshot<Vec<WorktreeSnapshot>>,
    metadata: SourceSnapshot<Metadata>,
) -> SourceSnapshot<Board> {
    let state = match (&git.state, &metadata.state) {
        (_, SourceState::Failed(error)) => SourceState::Failed(format!("State: {error}").into()),
        (SourceState::Failed(error), _) => SourceState::Failed(format!("Git: {error}").into()),
        (SourceState::Refreshing, _) | (_, SourceState::Refreshing) => SourceState::Refreshing,
        (SourceState::Ready, SourceState::Ready) => SourceState::Ready,
        _ => SourceState::Empty,
    };
    let data = git
        .data
        .as_ref()
        .zip(metadata.data.as_ref())
        .map(|(git, metadata)| Arc::new(inventory::board(config, git, &metadata.0, &metadata.1)));
    SourceSnapshot {
        data,
        state,
        updated_at: metadata.updated_at.or(git.updated_at),
        revision: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready<T>(data: T) -> SourceSnapshot<T> {
        SourceSnapshot {
            data: Some(Arc::new(data)),
            state: SourceState::Ready,
            updated_at: Some(tokio::time::Instant::now()),
            revision: 0,
        }
    }

    #[tokio::test]
    async fn metadata_changes_reuse_git_facts_while_git_is_busy_or_failed() {
        let fixture = crate::commands::test_support::CommandFixture::new()
            .await
            .unwrap();
        let scope = TaskScope::new();
        let (git, mut git_publisher) = source_channel();
        let (metadata, mut metadata_publisher) = source_channel();
        let facts = fixture
            .ctx
            .repository
            .inventory_status(&fixture.ctx.cancellation)
            .await
            .unwrap();
        let mut git_snapshot = ready(facts);
        git_publisher.publish(git_snapshot.clone());
        let mut state = serde_json::json!({"slugs":{}});
        metadata_publisher.publish(ready((state.clone(), BTreeSet::new())));
        let source = project(&scope, fixture.ctx.config.clone(), git, metadata);
        let mut updates = source.subscribe();
        tokio::time::timeout(
            Duration::from_secs(2),
            updates.wait_for(|s| s.data.is_some()),
        )
        .await
        .unwrap()
        .unwrap();
        git_snapshot.state = SourceState::Refreshing;
        git_publisher.publish(git_snapshot.clone());
        state["slugs"]["one"] = serde_json::json!({"manualTitle":"Edited during refresh"});
        metadata_publisher.publish(ready((state.clone(), BTreeSet::new())));
        tokio::time::timeout(
            Duration::from_secs(2),
            updates.wait_for(|s| {
                s.data
                    .as_ref()
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
        git_snapshot.state = SourceState::Failed("fixture Git failure".into());
        git_publisher.publish(git_snapshot);
        state["slugs"]["one"]["manualTitle"] = "Edited after failure".into();
        metadata_publisher.publish(ready((state, BTreeSet::new())));
        tokio::time::timeout(
            Duration::from_secs(2),
            updates.wait_for(|s| {
                matches!(s.state, SourceState::Failed(_))
                    && s.data
                        .as_ref()
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
}
