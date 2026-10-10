//! Prepare session presentation once per changed source, on its owning host.

use std::sync::Arc;

use wt_harness::DerivedState;
use wt_runtime::{SourceHandle, SourceSnapshot, SourceState, TaskScope, source_channel};
use wt_tui::{Board, SessionView};

use crate::session_activity::{SessionActivitySnapshot, UsagePeriodDto};
use crate::session_source::{SessionDiscoveries, SessionInventory, SessionSources};

pub fn overlay(
    scope: &TaskScope,
    board: SourceHandle<Board>,
    sessions: &SessionSources,
    activity: SourceHandle<SessionActivitySnapshot>,
) -> SourceHandle<Board> {
    let (source, mut publisher) = source_channel();
    let cancel = scope.token();
    let inventory = sessions.inventory.clone();
    let discoveries = sessions.discoveries.clone();
    scope.spawn(async move {
        let mut board_updates = board.subscribe();
        let mut inventory_updates = inventory.subscribe();
        let mut discovery_updates = discoveries.subscribe();
        let mut activity_updates = activity.subscribe();
        board_updates.mark_changed();
        inventory_updates.mark_changed();
        discovery_updates.mark_changed();
        activity_updates.mark_changed();
        let mut last = None;
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                request = publisher.requested() => {
                    if request.is_none() { break; }
                    board.refresh(); inventory.refresh();
                    continue;
                },
                changed = board_updates.changed() => if changed.is_err() { break; },
                changed = inventory_updates.changed() => if changed.is_err() { break; },
                changed = discovery_updates.changed() => if changed.is_err() { break; },
                changed = activity_updates.changed() => if changed.is_err() { break; },
            }
            let prepared = compose(
                board_updates.borrow_and_update().clone(),
                inventory_updates.borrow_and_update().clone(),
                discovery_updates.borrow_and_update().clone(),
                activity_updates.borrow_and_update().clone(),
            );
            let visible = (prepared.data.clone(), prepared.state.clone());
            if last.as_ref() != Some(&visible) {
                last = Some(visible);
                publisher.publish(prepared);
            }
        }
    });
    source
}

fn compose(
    mut source: SourceSnapshot<Board>,
    inventory: SourceSnapshot<SessionInventory>,
    discoveries: SourceSnapshot<SessionDiscoveries>,
    activity: SourceSnapshot<SessionActivitySnapshot>,
) -> SourceSnapshot<Board> {
    let Some(prepared) = source.data.as_ref() else {
        return source;
    };
    let mut board = prepared.as_ref().clone();
    for (label, state) in [
        ("Session inventory", &inventory.state),
        ("Session discovery", &discoveries.state),
        ("Agent activity", &activity.state),
    ] {
        if let SourceState::Failed(error) = state {
            crate::activity_source::append_attention(
                &mut board,
                label,
                &format!("{}: {}", label, clean(error)),
            );
        }
    }
    let inventory_ok = matches!(inventory.state, SourceState::Ready);
    let discoveries_ok = matches!(discoveries.state, SourceState::Ready);
    for entry in discoveries.data.iter().flat_map(|data| data.iter()) {
        let session = &entry.session;
        let live = inventory_ok
            && inventory.data.as_ref().is_some_and(|all| {
                all.all.contains_key(&session.tmux_session_name)
                    && all
                        .harness_session_ids
                        .get(&session.tmux_session_name)
                        .is_none_or(|id| id == &session.session_id)
            });
        let state = if !inventory_ok || !discoveries_ok {
            "unknown"
        } else {
            state_name(session.extras.derived_state)
        };
        let output = activity
            .data
            .as_ref()
            .and_then(|data| data.tails.iter().find(|tail| tail.key == entry.key))
            .map(|tail| {
                tail.lines
                    .iter()
                    .rev()
                    .take(120)
                    .rev()
                    .map(|line| clean(&line.text).chars().take(2048).collect())
                    .collect()
            })
            .unwrap_or_default();
        let view = SessionView {
            id: session.session_id.clone(),
            harness: match entry.key.harness {
                wt_core::HarnessId::Claude => "Claude",
                wt_core::HarnessId::Codex => "Codex",
                wt_core::HarnessId::Opencode => "OpenCode",
            }
            .into(),
            name: clean(&session.display_name),
            state: state.into(),
            live,
            queued: session.extras.queued,
            output,
        };
        if let Some(row) = board.rows.iter_mut().find(|row| row.slug == entry.key.slug) {
            if live {
                row.badge.push_str(&format!("  {} {state}", view.harness));
                row.needs_attention |= state == "asking";
            }
            row.sessions.push(view);
        } else if ["main", "manager", "wt", "dotfiles"].contains(&entry.key.slug.as_str()) {
            board
                .slot_sessions
                .entry(entry.key.slug.clone())
                .or_default()
                .push(view);
        }
    }
    if let Some(activity) = activity.data.as_ref() {
        for event in activity.events.iter().rev().take(100).rev() {
            let at_ms = u64::try_from(event.timestamp_ms).unwrap_or_default();
            let text = clean(&event.text);
            let (level, attention) = match event.level {
                crate::session_activity::ActivityKindDto::Warn => ("WARN", true),
                crate::session_activity::ActivityKindDto::ToolError => ("ERROR", true),
                _ => ("INFO", false),
            };
            board.activity.push(wt_tui::ActivityLine {
                at_ms,
                level: level.into(),
                channel: if attention { "attention" } else { "activity" }.into(),
                source: "agent session".into(),
                text: text.clone(),
            });
            if attention {
                board.attention.push(wt_tui::AttentionLine {
                    at_ms,
                    source: "agent session".into(),
                    text,
                });
            }
        }
        if let Some(usage) = &activity.usage {
            for (name, five, week) in [
                ("Claude", &usage.claude_five_hour, &usage.claude_seven_day),
                ("Codex", &usage.codex_five_hour, &usage.codex_seven_day),
            ] {
                let periods = [("5h", five), ("7d", week)]
                    .into_iter()
                    .filter_map(|(period, value)| usage_text(period, value.as_ref()))
                    .collect::<Vec<_>>();
                if !periods.is_empty() {
                    board.usage.push(format!("{name} {}", periods.join(" / ")));
                }
            }
            if let Some(cost) = usage.opencode_five_hour.filter(|cost| cost.is_finite()) {
                board.usage.push(format!("OpenCode 5h ${cost:.2}"));
            }
        }
    }
    crate::activity_source::bound_feeds(&mut board);
    source.data = Some(Arc::new(board));
    source
}

fn usage_text(label: &str, period: Option<&UsagePeriodDto>) -> Option<String> {
    let period = period.filter(|period| period.utilization.is_finite())?;
    Some(format!("{label} {:.0}%", period.utilization))
}

fn clean(value: &str) -> String {
    wt_core::sanitize_terminal_text(value)
}

fn state_name(state: Option<DerivedState>) -> &'static str {
    match state {
        Some(DerivedState::Working) => "working",
        Some(DerivedState::Asking) => "asking",
        Some(DerivedState::Polling) => "polling",
        Some(DerivedState::Waiting) => "waiting",
        Some(DerivedState::Abandoned) => "abandoned",
        Some(DerivedState::Idle) => "idle",
        Some(DerivedState::Unknown) | None => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_source::{DiscoveredSession, SessionKey};
    use wt_harness::{HarnessExtras, HarnessSession};
    use wt_tui::BoardRow;

    #[test]
    fn failed_inventory_does_not_claim_a_cached_session_is_live() {
        let board = SourceSnapshot {
            data: Some(Arc::new(Board {
                rows: vec![BoardRow {
                    slug: "one".into(),
                    ..Default::default()
                }],
                ..Default::default()
            })),
            state: SourceState::Ready,
            ..Default::default()
        };
        let discoveries = SourceSnapshot {
            data: Some(Arc::new(vec![DiscoveredSession {
                key: SessionKey {
                    slug: "one".into(),
                    harness: wt_core::HarnessId::Codex,
                    session_id: "id".into(),
                },
                session: HarnessSession {
                    display_name: "primary".into(),
                    session_id: "id".into(),
                    tmux_session_name: "one-codex".into(),
                    last_active_ms: None,
                    is_live: true,
                    extras: HarnessExtras {
                        derived_state: Some(DerivedState::Working),
                        ..Default::default()
                    },
                },
            }])),
            state: SourceState::Ready,
            ..Default::default()
        };
        let result = compose(
            board,
            SourceSnapshot {
                state: SourceState::Failed("tmux unavailable".into()),
                ..Default::default()
            },
            discoveries,
            SourceSnapshot::default(),
        );
        let board = result.data.unwrap();
        assert_eq!(board.rows[0].sessions[0].state, "unknown");
        assert!(!board.rows[0].sessions[0].live);
        assert!(
            board
                .activity
                .iter()
                .any(|line| line.text.contains("tmux unavailable"))
        );
    }
}
