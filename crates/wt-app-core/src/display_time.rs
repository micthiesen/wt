//! Prepares controller-local clock labels for timestamps shown by the TUI.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use wt_runtime::{SourceHandle, TaskScope, derived_channel};
use wt_tui::Board;

const MAX_LABELLED_TIMESTAMPS: usize = 1_024;
/// Row history timestamps labelled per board; the newest win past this.
const MAX_HISTORY_TIMESTAMPS: usize = 16_384;
/// "today" and "yesterday" are rechecked this often, so labels prepared
/// before midnight do not outlive it.
const DAY_CHECK: std::time::Duration = std::time::Duration::from_secs(60);

/// Adds local clock labels to the current board snapshot without doing timezone
/// work on the terminal input or render thread.
pub fn overlay(scope: &TaskScope, board: SourceHandle<Board>) -> SourceHandle<Board> {
    let (output, mut publisher) = derived_channel(scope, "display-time");
    let cancellation = scope.token();
    scope.spawn(async move {
        let mut updates = board.subscribe();
        updates.mark_changed();
        let mut cache = BTreeMap::<u64, String>::new();
        let mut history = BTreeMap::<u64, wt_tui::LocalStamp>::new();
        let mut history_day = None::<(i64, tokio::time::Instant)>;
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
            let Some(data) = base.data().map(Arc::as_ref) else {
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

            let wanted = history_timestamps(data);
            history.retain(|timestamp, _| wanted.contains(timestamp));
            let day_checked = history_day.is_some_and(|(_, at)| at.elapsed() < DAY_CHECK);
            if !wanted.is_empty() && (!day_checked || wanted.len() != history.len()) {
                let known_day = history_day.map(|(day, _)| day);
                let missing: Vec<_> = wanted
                    .iter()
                    .filter(|timestamp| !history.contains_key(timestamp))
                    .copied()
                    .collect();
                let all: Vec<_> = wanted.iter().copied().collect();
                let now_ms = crate::feeds::epoch_ms();
                let prepared = tokio::task::spawn_blocking(move || {
                    let today = local_day(now_ms)?;
                    // Every label names its day relative to today.
                    let stale = known_day != Some(today);
                    let stamps = history_stamps(if stale { all } else { missing }, today);
                    Some((today, stale, stamps))
                })
                .await
                .ok()
                .flatten();
                if let Some((today, stale, stamps)) = prepared {
                    if stale {
                        history.clear();
                    }
                    history.extend(stamps);
                    history_day = Some((today, tokio::time::Instant::now()));
                }
            }

            let mut projected = data.clone();
            projected.local_times.clone_from(&cache);
            projected.history_times.clone_from(&history);
            // Labels add no observation of their own: the board's state,
            // domains and observation stamps pass through unchanged, which
            // replaces the old pass-through of `updated_at`.
            publisher.publish(base.derive(Some(Arc::new(projected))));
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

fn history_timestamps(board: &Board) -> BTreeSet<u64> {
    let mut timestamps: BTreeSet<u64> = board
        .row_history
        .values()
        .flatten()
        .map(|event| event.at_ms)
        .filter(|timestamp| *timestamp > 0)
        .collect();
    while timestamps.len() > MAX_HISTORY_TIMESTAMPS {
        timestamps.pop_first();
    }
    timestamps
}

/// The local calendar day a timestamp falls on, as a day count.
fn local_day(timestamp_ms: u64) -> Option<i64> {
    let (year, month, day, ..) = local_time_components(timestamp_ms)?;
    time::Date::from_calendar_date(year, time::Month::try_from(month).ok()?, day)
        .ok()
        .map(|date| i64::from(date.to_julian_day()))
}

/// Day and clock labels for a worktree's history: `today`, `yesterday` or a
/// date such as `Mon 23 Nov`, and `14:32`.
fn history_stamps(timestamps: Vec<u64>, today: i64) -> BTreeMap<u64, wt_tui::LocalStamp> {
    timestamps
        .into_iter()
        .filter_map(|timestamp_ms| {
            let (year, month, day, hour, minute, _) = local_time_components(timestamp_ms)?;
            let month = time::Month::try_from(month).ok()?;
            let date = time::Date::from_calendar_date(year, month, day).ok()?;
            let day = match today - i64::from(date.to_julian_day()) {
                0 => "today".to_owned(),
                1 => "yesterday".to_owned(),
                _ => {
                    let weekday = date.weekday().to_string();
                    let month = month.to_string();
                    format!("{} {day} {}", &weekday[..3], &month[..3])
                }
            };
            Some((
                timestamp_ms,
                wt_tui::LocalStamp {
                    day,
                    clock: format!("{hour:02}:{minute:02}"),
                },
            ))
        })
        .collect()
}

#[cfg(unix)]
fn local_time_labels(timestamps: Vec<u64>) -> BTreeMap<u64, String> {
    timestamps
        .into_iter()
        .filter_map(|timestamp_ms| {
            let (_, _, _, hour, minute, second) = local_time_components(timestamp_ms)?;
            Some((timestamp_ms, format!("{hour:02}:{minute:02}:{second:02}")))
        })
        .collect()
}

#[cfg(not(unix))]
fn local_time_labels(_timestamps: Vec<u64>) -> BTreeMap<u64, String> {
    BTreeMap::new()
}

/// Return calendar components in the host's local timezone. The caller must
/// use this only on a background/source task; libc timezone lookup is not an
/// input- or render-thread operation.
#[cfg(unix)]
pub fn local_time_components(timestamp_ms: u64) -> Option<(i32, u8, u8, u8, u8, u8)> {
    let seconds = i64::try_from(timestamp_ms / 1_000).ok()?;
    #[cfg(target_pointer_width = "64")]
    let seconds: libc::time_t = seconds;
    #[cfg(not(target_pointer_width = "64"))]
    let seconds: libc::time_t = seconds.try_into().ok()?;
    let mut broken_down = std::mem::MaybeUninit::<libc::tm>::uninit();
    // `localtime_r` writes the full tm value and uses process-local timezone
    // state without sharing static storage with other workers.
    let result = unsafe { libc::localtime_r(&seconds, broken_down.as_mut_ptr()) };
    if result.is_null() {
        return None;
    }
    let broken_down = unsafe { broken_down.assume_init() };
    Some((
        broken_down.tm_year + 1900,
        u8::try_from(broken_down.tm_mon + 1).ok()?,
        u8::try_from(broken_down.tm_mday).ok()?,
        u8::try_from(broken_down.tm_hour).ok()?,
        u8::try_from(broken_down.tm_min).ok()?,
        u8::try_from(broken_down.tm_sec).ok()?,
    ))
}

#[cfg(not(unix))]
pub fn local_time_components(_timestamp_ms: u64) -> Option<(i32, u8, u8, u8, u8, u8)> {
    None
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use wt_core::Domain;
    use wt_runtime::{SourceSnapshot, SourceState, TaskScope, derived_channel};
    use wt_tui::{ActivityLine, AttentionLine, Board};

    use super::{history_stamps, local_day, overlay, visible_timestamps};

    #[cfg(unix)]
    #[test]
    fn history_labels_name_the_day_relative_to_today_and_the_clock() {
        const DAY_MS: u64 = 86_400_000;
        // Local noon, so a day either side stays clear of any DST shift.
        let now = 1_795_000_000_000;
        let (.., hour, minute, second) = super::local_time_components(now).unwrap();
        let noon = now + 12 * 3_600_000
            - (u64::from(hour) * 3_600 + u64::from(minute) * 60 + u64::from(second)) * 1_000;
        let today = local_day(noon).unwrap();
        let stamps = history_stamps(vec![noon, noon - DAY_MS, noon - 9 * DAY_MS], today);
        assert_eq!(stamps[&noon].day, "today");
        assert_eq!(stamps[&noon].clock, "12:00");
        assert_eq!(stamps[&(noon - DAY_MS)].day, "yesterday");
        let earlier = stamps[&(noon - 9 * DAY_MS)]
            .day
            .split(' ')
            .collect::<Vec<_>>();
        assert_eq!(
            earlier.len(),
            3,
            "weekday, day of month, month: {earlier:?}"
        );
        assert_eq!((earlier[0].len(), earlier[2].len()), (3, 3));
        assert!(earlier[1].parse::<u8>().is_ok());
        // The same instant reads as yesterday once the day has turned.
        assert_eq!(
            history_stamps(vec![noon], today + 1)[&noon].day,
            "yesterday"
        );
    }

    #[tokio::test]
    async fn overlay_labels_every_row_history_timestamp() {
        let scope = TaskScope::new();
        let (input, input_publisher) = derived_channel(&scope, "test");
        let output = overlay(&scope, input);
        let mut updates = output.subscribe();
        let event = |at_ms| wt_core::WorktreeEvent {
            at_ms,
            event: wt_core::WorktreeEventKind::Conflict { resolved: true },
        };
        let board = |events: Vec<wt_core::WorktreeEvent>| {
            SourceSnapshot::test_live(
                Some(Board {
                    row_history: [("one".to_owned(), events)].into(),
                    ..Board::default()
                }),
                SourceState::Ready,
                Domain::History,
            )
        };
        input_publisher.publish(board(vec![
            event(1_700_000_000_000),
            event(1_700_000_100_000),
        ]));
        let labelled = |count: usize| {
            move |snapshot: &SourceSnapshot<Board>| {
                snapshot
                    .data()
                    .is_some_and(|board| board.history_times.len() == count)
            }
        };
        tokio::time::timeout(Duration::from_secs(1), updates.wait_for(labelled(2)))
            .await
            .unwrap()
            .unwrap();
        // A timestamp that left the board is dropped; a new one is added.
        input_publisher.publish(board(vec![
            event(1_700_000_100_000),
            event(1_700_000_200_000),
            event(1_700_000_300_000),
        ]));
        tokio::time::timeout(Duration::from_secs(1), updates.wait_for(labelled(3)))
            .await
            .unwrap()
            .unwrap();
        let times = updates.borrow().data().unwrap().history_times.clone();
        assert!(!times.contains_key(&1_700_000_000_000));
        assert!(times.values().all(|stamp| stamp.clock.len() == 5));
        scope.shutdown(Duration::from_secs(1)).await.unwrap();
    }

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
        let (input, mut input_publisher) = derived_channel(&scope, "test");
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
        input_publisher.publish(SourceSnapshot::test_live(
            Some(board),
            SourceState::Ready,
            Domain::Activity,
        ));
        tokio::time::timeout(
            Duration::from_secs(1),
            updates.wait_for(|snapshot| {
                snapshot
                    .data()
                    .is_some_and(|board| board.local_times.len() == 3)
            }),
        )
        .await
        .unwrap()
        .unwrap();
        let labels = updates.borrow().data().unwrap().local_times.clone();
        assert!(
            updates.borrow().domains()[&Domain::Activity].is_live(),
            "labels keep the board's provenance"
        );
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
