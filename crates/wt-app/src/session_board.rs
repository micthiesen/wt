//! Prepare session presentation once per changed source, on its owning host.

use std::sync::Arc;

use wt_harness::DerivedState;
use wt_runtime::{SourceHandle, SourceSnapshot, SourceState, TaskScope, derived_channel};
use wt_tui::{Board, SessionView};

use crate::session_activity::{SessionActivitySnapshot, UsagePeriodDto};
use crate::session_source::{SessionDiscoveries, SessionInventory, SessionSources};

pub fn overlay(
    scope: &TaskScope,
    board: SourceHandle<Board>,
    sessions: &SessionSources,
    activity: SourceHandle<SessionActivitySnapshot>,
) -> SourceHandle<Board> {
    let (source, mut publisher) = derived_channel(scope, "session-board");
    let cancel = scope.token();
    let inventory = sessions.inventory.view();
    let discoveries = sessions.discoveries.view();
    scope.spawn(async move {
        let mut board_updates = board.subscribe();
        let mut inventory_updates = inventory.subscribe();
        let mut discovery_updates = discoveries.subscribe();
        let mut activity_updates = activity.subscribe();
        board_updates.mark_changed();
        inventory_updates.mark_changed();
        discovery_updates.mark_changed();
        activity_updates.mark_changed();
        let mut last: Option<SourceSnapshot<Board>> = None;
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
            // Stamps alone do not change what the board shows.
            if last.as_ref().is_some_and(|last| last.same_view(&prepared)) {
                continue;
            }
            last = Some(prepared.clone());
            publisher.publish(prepared);
        }
    });
    source
}

fn compose(
    source: SourceSnapshot<Board>,
    inventory: SourceSnapshot<SessionInventory>,
    discoveries: SourceSnapshot<SessionDiscoveries>,
    activity: SourceSnapshot<SessionActivitySnapshot>,
) -> SourceSnapshot<Board> {
    let Some(prepared) = source.data() else {
        return source;
    };
    let mut board = prepared.as_ref().clone();
    for (label, state) in [
        ("Session inventory", inventory.state()),
        ("Session discovery", discoveries.state()),
        ("Agent activity", activity.state()),
    ] {
        if let SourceState::Failed(error) = state {
            crate::activity_source::append_attention(
                &mut board,
                label,
                &format!("{}: {}", label, clean(error)),
            );
        }
    }
    let inventory_ok = matches!(inventory.state(), SourceState::Ready);
    let discoveries_ok = matches!(discoveries.state(), SourceState::Ready);
    for entry in discoveries.data().iter().flat_map(|data| data.iter()) {
        let session = &entry.session;
        let live = inventory_ok
            && inventory.data().is_some_and(|all| {
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
            .data()
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
            summary: session
                .extras
                .session_summary
                .as_deref()
                .map(clean)
                .filter(|summary| !summary.is_empty()),
            context_percent: session.extras.context_percent,
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
    if let Some(feed) = activity.data() {
        for event in feed.events.iter().rev().take(100).rev() {
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
        if let Some(usage) = &feed.usage {
            for (harness, five, week) in [
                ("Claude", &usage.claude_five_hour, &usage.claude_seven_day),
                ("Codex", &usage.codex_five_hour, &usage.codex_seven_day),
            ] {
                for (period, value) in [("5h", five), ("7d", week)] {
                    if let Some(item) = usage_item(harness, period, value.as_ref()) {
                        board.usage.push(item);
                    }
                }
            }
            for (period, cost) in [
                ("5h", usage.opencode_five_hour),
                ("7d", usage.opencode_seven_day),
            ] {
                if let Some(cost) = cost.filter(|cost| cost.is_finite()) {
                    board.usage.push(wt_tui::UsageItem {
                        harness: "OpenCode".into(),
                        period: period.into(),
                        cost: Some(if cost >= 100.0 {
                            format!("${cost:.0}")
                        } else {
                            format!("${cost:.2}")
                        }),
                        ..Default::default()
                    });
                }
            }
        }
        if let Some(primary) = &feed.primary {
            board.display.primary_harness = primary.clone();
        }
    }
    crate::activity_source::bound_feeds(&mut board);
    source
        .with_data(Some(Arc::new(board)))
        .with_input(&inventory)
        .with_input(&discoveries)
        .with_input(&activity)
}

fn usage_item(
    harness: &str,
    period: &str,
    value: Option<&UsagePeriodDto>,
) -> Option<wt_tui::UsageItem> {
    let value = value.filter(|value| value.utilization.is_finite())?;
    Some(wt_tui::UsageItem {
        harness: harness.into(),
        period: period.into(),
        percent: Some(value.utilization.round().clamp(0.0, 100.0) as u8),
        resets_at_ms: value
            .resets_at
            .as_deref()
            .and_then(wt_core::parse_iso_millis)
            .and_then(|ms| u64::try_from(ms).ok()),
        cost: None,
    })
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
    use wt_runtime::Domain;
    use wt_tui::BoardRow;

    #[test]
    fn failed_inventory_does_not_claim_a_cached_session_is_live() {
        let board = SourceSnapshot::test_live(
            Some(Board {
                rows: vec![BoardRow {
                    slug: "one".into(),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            SourceState::Ready,
            Domain::Git,
        );
        let discoveries = SourceSnapshot::test_live(
            Some(vec![DiscoveredSession {
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
                        session_summary: Some("Finished the review".into()),
                        ..Default::default()
                    },
                },
            }]),
            SourceState::Ready,
            Domain::Sessions,
        );
        let result = compose(
            board,
            SourceSnapshot::default().with_state(SourceState::Failed("tmux unavailable".into())),
            discoveries,
            SourceSnapshot::default(),
        );
        assert!(result.domains()[&Domain::Sessions].is_live());
        let board = result.data().unwrap();
        assert_eq!(board.rows[0].sessions[0].state, "unknown");
        assert_eq!(
            board.rows[0].sessions[0].summary.as_deref(),
            Some("Finished the review")
        );
        assert!(!board.rows[0].sessions[0].live);
        assert!(
            board
                .activity
                .iter()
                .any(|line| line.text.contains("tmux unavailable"))
        );
    }
}
