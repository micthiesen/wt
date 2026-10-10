//! Dev-server events have a separate, batched lane. They never invalidate Git,
//! GitHub, or run the potentially expensive user health command.
use std::{path::PathBuf, sync::Arc, time::Duration};

use notify::{RecursiveMode, Watcher};
use wt_dev::{DevServerStatus, DevStatusRow, DevWorktree};
use wt_runtime::{
    RefreshPolicy, SourceHandle, SourceSnapshot, SourceState, TaskScope, source_channel,
    start_source,
};
use wt_tui::Board;

use crate::context::AppContext;

const BACKSTOP: Duration = Duration::from_secs(15);

pub fn overlay(
    scope: &TaskScope,
    context: &AppContext,
    local: SourceHandle<Board>,
    board: SourceHandle<Board>,
) -> SourceHandle<Board> {
    if context.config.dev_server.is_none() {
        return board;
    }
    let dev = start_source(
        scope,
        RefreshPolicy {
            debounce: Duration::from_millis(50),
            minimum_interval: Duration::from_millis(500),
        },
        {
            let context = context.clone();
            let local = local.clone();
            move |cancel| {
                let context = context.clone();
                let rows = worktrees(&local.snapshot());
                async move {
                    Ok::<_, anyhow::Error>(
                        crate::dev::service(&context)?
                            .status_all(&rows, &cancel)
                            .await?
                            .worktrees,
                    )
                }
            }
        },
    );
    watch(
        scope,
        context.config.paths.cache_root.join("dev"),
        dev.clone(),
    );
    project(scope, local, board, dev)
}

fn worktrees(snapshot: &SourceSnapshot<Board>) -> Vec<DevWorktree> {
    snapshot
        .data
        .iter()
        .flat_map(|board| &board.rows)
        .map(|row| DevWorktree {
            slug: row.slug.clone(),
            path: PathBuf::from(&row.path),
            branch: row.branch.clone(),
        })
        .collect()
}

fn project(
    scope: &TaskScope,
    local: SourceHandle<Board>,
    board: SourceHandle<Board>,
    dev: SourceHandle<Vec<DevStatusRow>>,
) -> SourceHandle<Board> {
    let (source, mut publisher) = source_channel();
    let cancellation = scope.token();
    scope.spawn(async move {
        let mut local_updates = local.subscribe();
        let mut board_updates = board.subscribe();
        let mut dev_updates = dev.subscribe();
        let mut inventory = Vec::new();
        // The upstream may already have published before this task subscribed.
        local_updates.mark_changed();
        board_updates.mark_changed();
        loop {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => break,
                request = publisher.requested() => {
                    if request.is_none() { break; }
                    board.refresh();
                    dev.refresh();
                    continue;
                }
                changed = local_updates.changed() => {
                    if changed.is_err() { break; }
                    let next = worktrees(&local_updates.borrow_and_update()).into_iter()
                        .map(|row| (row.slug, row.path, row.branch)).collect::<Vec<_>>();
                    if next != inventory {
                        inventory = next;
                        dev.refresh();
                    }
                    continue;
                }
                changed = board_updates.changed() => {
                    if changed.is_err() { break; }
                    board_updates.borrow_and_update();
                }
                changed = dev_updates.changed() => {
                    if changed.is_err() { break; }
                    dev_updates.borrow_and_update();
                }
            }
            publisher.publish(compose(
                board_updates.borrow().clone(),
                dev_updates.borrow().clone(),
            ));
        }
    });
    source
}

fn watch(scope: &TaskScope, directory: PathBuf, source: SourceHandle<Vec<DevStatusRow>>) {
    let cancellation = scope.token();
    scope.spawn(async move {
        let callback = source.clone();
        let watcher = tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(&directory)?;
            let mut watcher =
                notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
                    match event {
                        Ok(event) if matches!(event.kind, notify::EventKind::Access(_)) => {}
                        // Streaming log output must not turn into status polling.
                        Ok(event)
                            if event
                                .paths
                                .iter()
                                .any(|path| path.extension().is_some_and(|ext| ext == "log")) => {}
                        _ => {
                            callback.refresh();
                        }
                    }
                })?;
            watcher.watch(&directory, RecursiveMode::Recursive)?;
            Ok::<_, anyhow::Error>(watcher)
        })
        .await;
        if !matches!(watcher, Ok(Ok(_))) {
            tracing::warn!(
                ?watcher,
                "dev status watcher unavailable; using refresh backstop"
            );
        }
        let mut interval =
            tokio::time::interval_at(tokio::time::Instant::now() + BACKSTOP, BACKSTOP);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => break,
                _ = interval.tick() => { source.refresh(); }
            }
        }
        drop(watcher);
    });
}

fn compose(
    mut board: SourceSnapshot<Board>,
    dev: SourceSnapshot<Vec<DevStatusRow>>,
) -> SourceSnapshot<Board> {
    let Some(data) = board.data.as_ref() else {
        return board;
    };
    let mut data = data.as_ref().clone();
    let error = match &dev.state {
        SourceState::Failed(error) => Some(error.as_ref()),
        _ => None,
    };
    for row in &mut data.rows {
        let status = dev
            .data
            .as_ref()
            .and_then(|rows| rows.iter().find(|status| status.slug == row.slug));
        if let Some(status) = status.and_then(|row| row.status.as_ref()) {
            row.details.push(label(status));
            row.dev_url = status.url.as_deref().map(wt_core::sanitize_terminal_text);
        }
        if let Some(error) = error.or_else(|| status.and_then(|row| row.error.as_deref())) {
            row.details
                .push(format!("Dev: {}", wt_core::sanitize_terminal_text(error)));
        }
    }
    board.data = Some(Arc::new(data));
    board
}

fn label(status: &DevServerStatus) -> String {
    let mut text = if status.crashed {
        "Dev: crashed".to_owned()
    } else if status.starting {
        "Dev: starting".to_owned()
    } else if status.running {
        "Dev: running".to_owned()
    } else if let Some(waiting) = status.waiting {
        format!("Dev: queued #{}", waiting.rank + 1)
    } else {
        "Dev: stopped".to_owned()
    };
    if (status.running || status.starting)
        && let Some(url) = &status.url
    {
        text.push_str(&format!(" · {}", wt_core::sanitize_terminal_text(url)));
    }
    if status.rebased_since == Some(true) {
        text.push_str(" · needs reset after rebase");
    }
    if let Some(restarts) = &status.restarts {
        text.push_str(&format!(
            " · {} restarts (exit {})",
            restarts.count, restarts.last_exit
        ));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use wt_tui::BoardRow;

    fn snapshot<T>(data: T) -> SourceSnapshot<T> {
        SourceSnapshot {
            data: Some(Arc::new(data)),
            state: SourceState::Ready,
            updated_at: Some(tokio::time::Instant::now()),
            revision: 0,
        }
    }

    fn board() -> Board {
        Board {
            rows: vec![BoardRow {
                key: "one".into(),
                slug: "one".into(),
                path: "/tmp/one".into(),
                branch: "feature".into(),
                title: "Original".into(),
                ..BoardRow::default()
            }],
            ..Board::default()
        }
    }

    fn dev() -> Vec<DevStatusRow> {
        vec![DevStatusRow {
            slug: "one".into(),
            status: Some(DevServerStatus {
                running: true,
                starting: false,
                crashed: false,
                port: Some(3000),
                url: Some("http://localhost:3000".into()),
                since: None,
                waiting: None,
                rebased_since: Some(true),
                restarts: None,
            }),
            error: None,
        }]
    }

    #[test]
    fn failed_dev_fetch_keeps_last_good_status_visible_and_reports_the_error() {
        let mut status = snapshot(dev());
        status.state = SourceState::Failed("tmux unavailable".into());
        let result = compose(snapshot(board()), status);
        let row = &result.data.as_ref().unwrap().rows[0];
        assert_eq!(row.dev_url.as_deref(), Some("http://localhost:3000"));
        assert!(
            row.details
                .iter()
                .any(|line| line.contains("needs reset after rebase"))
        );
        assert!(
            row.details
                .iter()
                .any(|line| line == "Dev: tmux unavailable")
        );
        assert!(!row.details.iter().any(|line| line.contains("stopped")));
    }

    #[tokio::test(start_paused = true)]
    async fn dev_updates_do_not_refresh_upstream_and_title_updates_do_not_refetch_dev() {
        let scope = TaskScope::new();
        let (local, mut local_writer) = source_channel();
        let (upstream, mut upstream_writer) = source_channel();
        let (status, mut status_writer) = source_channel();
        // Exercise startup after the input already contains data.
        local_writer.publish(snapshot(board()));
        upstream_writer.publish(snapshot(board()));
        let output = project(&scope, local, upstream, status);
        assert_eq!(status_writer.requested().await, Some(()));
        status_writer.publish(snapshot(dev()));
        tokio::time::timeout(
            Duration::from_secs(1),
            output
                .subscribe()
                .wait_for(|s| s.data.as_ref().is_some_and(|b| b.rows[0].dev_url.is_some())),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(1), local_writer.requested())
                .await
                .is_err()
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(1), upstream_writer.requested())
                .await
                .is_err()
        );
        let mut renamed = board();
        renamed.rows[0].title = "Renamed".into();
        local_writer.publish(snapshot(renamed.clone()));
        upstream_writer.publish(snapshot(renamed));
        tokio::time::timeout(
            Duration::from_secs(1),
            output.subscribe().wait_for(|s| {
                s.data
                    .as_ref()
                    .is_some_and(|b| b.rows[0].title == "Renamed")
            }),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(1), status_writer.requested())
                .await
                .is_err()
        );
        output.refresh();
        assert_eq!(upstream_writer.requested().await, Some(()));
        assert_eq!(status_writer.requested().await, Some(()));
        scope.shutdown(Duration::from_secs(1)).await.unwrap();
    }
}
