//! Prepares controller-local clock labels for timestamps shown by the TUI.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use wt_runtime::{SourceHandle, SourceSnapshot, TaskScope, source_channel};
use wt_tui::Board;

const MAX_LABELLED_TIMESTAMPS: usize = 1_024;

/// Adds local clock labels to the current board snapshot without doing timezone
/// work on the terminal input or render thread.
pub fn overlay(scope: &TaskScope, board: SourceHandle<Board>) -> SourceHandle<Board> {
    let (output, mut publisher) = source_channel();
    let cancellation = scope.token();
    scope.spawn(async move {
        let mut updates = board.subscribe();
        updates.mark_changed();
        let mut cache = BTreeMap::<u64, String>::new();
        loop {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => break,
                request = publisher.requested() => {
                    if request.is_none() { break; }
                    board.refresh();
                    updates.mark_changed();
                }
                changed = updates.changed() => {
                    if changed.is_err() { break; }
                    updates.borrow_and_update();
                }
            }

            let base = updates.borrow().clone();
            let Some(data) = base.data.as_deref() else {
                publisher.publish(base);
                continue;
            };

            let timestamps = visible_timestamps(data);
            cache.retain(|timestamp, _| timestamps.contains(timestamp));
            let missing: Vec<_> = timestamps
                .iter()
                .filter(|timestamp| !cache.contains_key(timestamp))
                .copied()
                .collect();
            if !missing.is_empty() {
                let converted = tokio::task::spawn_blocking(move || local_time_labels(missing))
                    .await
                    .unwrap_or_else(|error| {
                        tracing::warn!(%error, "prepare local display timestamps");
                        BTreeMap::new()
                    });
                cache.extend(converted);
            }

            let mut projected = data.clone();
            projected.local_times.clone_from(&cache);
            publisher.publish(SourceSnapshot {
                data: Some(Arc::new(projected)),
                state: base.state,
                updated_at: base.updated_at,
                revision: 0,
            });
        }
    });
    output
}

fn visible_timestamps(board: &Board) -> BTreeSet<u64> {
    let mut timestamps: BTreeSet<_> = board
        .attention
        .iter()
        .rev()
        .take(MAX_LABELLED_TIMESTAMPS)
        .map(|line| line.at_ms)
        .chain((board.attention_seen_ms != 0).then_some(board.attention_seen_ms))
        .filter(|timestamp| *timestamp > 0)
        .collect();

    // Preserve every visible attention timestamp and the watermark first.
    // If an unusually large attention feed exceeds the cap, keep its newest
    // entries. Then fill remaining capacity with the latest activity entries.
    if timestamps.len() > MAX_LABELLED_TIMESTAMPS {
        timestamps = timestamps
            .into_iter()
            .rev()
            .take(MAX_LABELLED_TIMESTAMPS)
            .collect();
    }
    for timestamp in board
        .activity
        .iter()
        .rev()
        .take(MAX_LABELLED_TIMESTAMPS)
        .map(|line| line.at_ms)
        .filter(|timestamp| *timestamp > 0)
    {
        if timestamps.len() == MAX_LABELLED_TIMESTAMPS {
            break;
        }
        timestamps.insert(timestamp);
    }
    timestamps
}

#[cfg(unix)]
fn local_time_labels(timestamps: Vec<u64>) -> BTreeMap<u64, String> {
    timestamps
        .into_iter()
        .filter_map(|timestamp_ms| {
            let seconds = i64::try_from(timestamp_ms / 1_000).ok()?;
            #[cfg(target_pointer_width = "64")]
            let seconds: libc::time_t = seconds;
            #[cfg(not(target_pointer_width = "64"))]
            let seconds: libc::time_t = seconds.try_into().ok()?;
            let mut broken_down = std::mem::MaybeUninit::<libc::tm>::uninit();
            // `localtime_r` writes the full `tm` value on success and uses
            // process-local timezone state without shared static storage.
            let result = unsafe { libc::localtime_r(&seconds, broken_down.as_mut_ptr()) };
            if result.is_null() {
                return None;
            }
            let broken_down = unsafe { broken_down.assume_init() };
            Some((
                timestamp_ms,
                format!(
                    "{:02}:{:02}:{:02}",
                    broken_down.tm_hour, broken_down.tm_min, broken_down.tm_sec
                ),
            ))
        })
        .collect()
}

#[cfg(not(unix))]
fn local_time_labels(_timestamps: Vec<u64>) -> BTreeMap<u64, String> {
    BTreeMap::new()
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use wt_runtime::{SourceSnapshot, SourceState, TaskScope, source_channel};
    use wt_tui::{ActivityLine, AttentionLine, Board};

    use super::{overlay, visible_timestamps};

    #[test]
    fn timestamps_are_limited_to_the_current_visible_feed() {
        let board = Board {
            activity: (1..=500)
                .map(|at_ms| ActivityLine {
                    at_ms,
                    ..ActivityLine::default()
                })
                .collect(),
            attention: (501..=700)
                .map(|at_ms| AttentionLine {
                    at_ms,
                    ..AttentionLine::default()
                })
                .collect(),
            attention_seen_ms: 701,
            ..Board::default()
        };
        let timestamps = visible_timestamps(&board);
        assert_eq!(timestamps.len(), 701);
        assert!(timestamps.contains(&1));
        assert!(timestamps.contains(&700));
        assert!(timestamps.contains(&701));
    }

    #[tokio::test]
    async fn overlay_prepares_labels_and_forwards_refresh() {
        let scope = TaskScope::new();
        let (input, mut input_publisher) = source_channel();
        let output = overlay(&scope, input.clone());
        let mut updates = output.subscribe();
        let board = Board {
            activity: vec![ActivityLine {
                at_ms: 1_700_000_000_000,
                ..ActivityLine::default()
            }],
            attention: vec![AttentionLine {
                at_ms: 1_700_000_001_000,
                ..AttentionLine::default()
            }],
            attention_seen_ms: 1_700_000_002_000,
            ..Board::default()
        };
        input_publisher.publish(SourceSnapshot {
            data: Some(Arc::new(board)),
            state: SourceState::Ready,
            updated_at: Some(tokio::time::Instant::now()),
            revision: 1,
        });
        tokio::time::timeout(
            Duration::from_secs(1),
            updates.wait_for(|snapshot| {
                snapshot
                    .data
                    .as_ref()
                    .is_some_and(|board| board.local_times.len() == 3)
            }),
        )
        .await
        .unwrap()
        .unwrap();
        let labels = updates.borrow().data.as_ref().unwrap().local_times.clone();
        assert_eq!(labels.len(), 3);
        assert!(labels.values().all(|label| label.len() == 8));

        output.refresh();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), input_publisher.requested())
                .await
                .unwrap(),
            Some(())
        );
        scope.shutdown(Duration::from_secs(1)).await.unwrap();
    }
}
