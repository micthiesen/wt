//! Prepare session presentation once per changed source, on its owning host.

use std::sync::Arc;

use std::path::PathBuf;

use wt_core::{HarnessId, sanitize_terminal_text};
use wt_harness::DerivedState;
use wt_runtime::{SourceHandle, SourceSnapshot, SourceState, TaskScope, derived_channel};
use wt_tui::{Board, SessionTarget, SessionView, TerminalSlotView};

use crate::session_activity::{SessionActivitySnapshot, UsagePeriodDto};
use crate::session_source::{SessionDiscoveries, SessionInventory, SessionSources};
use wt_app_core::feeds::FailureClock;
use wt_app_core::harness::{AgentTarget, AgentTargetKind, AppHarness};

pub fn overlay(
    scope: &TaskScope,
    board: SourceHandle<Board>,
    sessions: &SessionSources,
    activity: SourceHandle<SessionActivitySnapshot>,
    app: AppHarness,
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
        let mut failures = FailureClock::default();
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
                &mut failures,
                board_updates.borrow_and_update().clone(),
                inventory_updates.borrow_and_update().clone(),
                discovery_updates.borrow_and_update().clone(),
                activity_updates.borrow_and_update().clone(),
                &app.slot_agents(),
            );
            publisher.publish_if_changed(prepared);
        }
    });
    source
}

fn compose(
    failures: &mut FailureClock,
    source: SourceSnapshot<Board>,
    inventory: SourceSnapshot<SessionInventory>,
    discoveries: SourceSnapshot<SessionDiscoveries>,
    activity: SourceSnapshot<SessionActivitySnapshot>,
    slots: &[(SessionTarget, AgentTarget)],
) -> SourceSnapshot<Board> {
    let Some(prepared) = source.data() else {
        // This compose reports nothing, so every tracked failure has cleared.
        failures.sweep();
        return source;
    };
    let mut board = prepared.as_ref().clone();
    for (label, state) in [
        ("Session inventory", inventory.state()),
        ("Session discovery", discoveries.state()),
        ("Agent activity", activity.state()),
    ] {
        if let SourceState::Failed(error) = state {
            failures.attention(
                &mut board,
                label,
                &format!("{}: {}", label, sanitize_terminal_text(error)),
            );
        }
    }
    failures.sweep();
    let inventory_ok = matches!(inventory.state(), SourceState::Ready);
    let discoveries_ok = matches!(discoveries.state(), SourceState::Ready);
    board.live_slots.clear();
    for row in &mut board.rows {
        row.live_sessions.clear();
    }
    if inventory_ok && let Some(inventory) = inventory.data() {
        for name in inventory.all.keys() {
            // Managed Claude sessions are `slug~name`, whatever the name
            // ends with; classify them first, as session inventory does.
            let (slug, harness) = if board.rows.iter().any(|row| row.slug == *name) {
                (name.as_str(), "Claude")
            } else if let Some((slug, _)) =
                name.rsplit_once('~').filter(|(slug, _)| !slug.is_empty())
            {
                (slug, "Claude")
            } else if let Some(slug) = name.strip_suffix("-codex") {
                (slug, "Codex")
            } else if let Some(slug) = name.strip_suffix("-pi") {
                (slug, "Pi")
            } else if let Some(slug) = name.strip_suffix("-opencode") {
                (slug, "OpenCode")
            } else if ["main", "manager", "wt", "dotfiles"].contains(&name.as_str()) {
                (name.as_str(), "Claude")
            } else {
                continue;
            };
            board
                .live_slots
                .entry(slug.into())
                .or_default()
                .push(wt_tui::LiveSlotView {
                    harness: harness.into(),
                    state: inventory
                        .pane_activity
                        .get(name)
                        .cloned()
                        .unwrap_or_else(|| "unknown".into()),
                });
        }
    }
    for row in &mut board.rows {
        // This stage owns only host-local inventory. A same-named row on a
        // remote host can never inherit a local physical slot.
        if row.host.is_none() {
            row.live_sessions = board.live_slots.get(&row.slug).cloned().unwrap_or_default();
            row.needs_attention |= row.live_sessions.iter().any(|slot| slot.state == "asking");
        }
    }
    for entry in discoveries.data().iter().flat_map(|data| data.iter()) {
        let session = &entry.session;
        let inventory_data = inventory.data();
        let live = inventory_ok
            && inventory_data.is_some_and(|all| {
                crate::session_source::is_session_live(all, entry.key.harness, session)
            });
        // A live Codex/OpenCode slot without a captured UUID cannot be
        // attributed to this historical transcript (or declared idle).
        let identity_unproven = inventory_ok
            && matches!(
                entry.key.harness,
                wt_core::HarnessId::Codex | wt_core::HarnessId::Opencode | wt_core::HarnessId::Pi
            )
            && inventory_data.is_some_and(|all| {
                all.all.contains_key(&session.tmux_session_name)
                    && !all
                        .harness_session_ids
                        .contains_key(&session.tmux_session_name)
            });
        let mut state = if !inventory_ok || !discoveries_ok || identity_unproven {
            "unknown"
        } else {
            state_name(session.extras.derived_state)
        };
        if live
            && !identity_unproven
            && let Some(inputs) = activity.data()
        {
            state = opencode_input_state(inputs, &entry.key, state);
            if entry.key.harness == wt_core::HarnessId::Opencode {
                if let Some(slots) = board.live_slots.get_mut(&entry.key.slug) {
                    for slot in slots.iter_mut().filter(|slot| slot.harness == "OpenCode") {
                        slot.state = state.into();
                    }
                }
                if let Some(row) = board
                    .rows
                    .iter_mut()
                    .find(|row| row.host.is_none() && row.slug == entry.key.slug)
                {
                    for slot in row
                        .live_sessions
                        .iter_mut()
                        .filter(|slot| slot.harness == "OpenCode")
                    {
                        slot.state = state.into();
                    }
                }
            }
        }
        let output = activity
            .data()
            .and_then(|data| data.tails.iter().find(|tail| tail.key == entry.key))
            .map(|tail| {
                tail.lines
                    .iter()
                    .rev()
                    .take(120)
                    .rev()
                    .map(|line| {
                        sanitize_terminal_text(&line.text)
                            .chars()
                            .take(2048)
                            .collect()
                    })
                    .collect()
            })
            .unwrap_or_default();
        let view = SessionView {
            id: session.session_id.clone(),
            harness: match entry.key.harness {
                wt_core::HarnessId::Claude => "Claude",
                wt_core::HarnessId::Codex => "Codex",
                wt_core::HarnessId::Opencode => "OpenCode",
                wt_core::HarnessId::Pi => "Pi",
            }
            .into(),
            name: sanitize_terminal_text(&session.display_name),
            state: state.into(),
            live,
            queued: session.extras.queued,
            summary: session
                .extras
                .session_summary
                .as_deref()
                .map(sanitize_terminal_text)
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
    // Prepared picker entries. Built only from a Ready inventory, so an unknown
    // tmux state never reads as "nothing is running". Absent entries send the
    // menu to its host, which answers from its own cached discovery.
    board.session_menus.clear();
    board.terminals.clear();
    if inventory_ok && let Some(inventory) = inventory.data() {
        let live = inventory.live_sessions();
        let visible = board.display.harnesses.clone();
        let discovered: &[crate::session_source::DiscoveredSession] =
            discoveries.data().map_or(&[], |data| data.as_slice());
        for row in board
            .rows
            .iter()
            .filter(|row| row.host.is_none() && !row.is_main)
        {
            let agent = AgentTarget {
                slug: row.slug.clone(),
                kind: AgentTargetKind::Worktree,
                branch: Some(row.branch.clone()),
                cwd: PathBuf::from(&row.path),
                managed_name: None,
                remote: false,
            };
            board.session_menus.insert(
                wt_tui::session_menu_key(Some(&row.key), SessionTarget::Harness),
                wt_app_core::harness::picker_entries(
                    &visible,
                    &agent,
                    Some(row.key.clone()),
                    SessionTarget::Harness,
                    discovered,
                    &live,
                ),
            );
            board
                .terminals
                .insert(row.key.clone(), terminal_slots(inventory, &row.slug));
        }
        for (target, agent) in slots {
            board.session_menus.insert(
                wt_tui::session_menu_key(None, *target),
                wt_app_core::harness::picker_entries(
                    &visible, agent, None, *target, discovered, &live,
                ),
            );
        }
    }
    if let Some(feed) = activity.data() {
        for event in feed.events.iter().rev().take(100).rev() {
            let at_ms = u64::try_from(event.timestamp_ms).unwrap_or_default();
            let text = sanitize_terminal_text(&event.text);
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
            board.display.primary = HarnessId::ALL
                .into_iter()
                .find(|harness| harness.as_str() == primary);
        }
    }
    wt_app_core::feeds::bound_feeds(&mut board);
    source
        .with_data(Some(Arc::new(board)))
        .with_input(&inventory)
        .with_input(&discoveries)
        .with_input(&activity)
}

/// Live shell and diff terminals of one worktree, with the exact tmux identity
/// a stop request must match.
fn terminal_slots(inventory: &SessionInventory, slug: &str) -> Vec<TerminalSlotView> {
    [
        (SessionTarget::Shell, "shell"),
        (SessionTarget::Diff, "diff"),
    ]
    .into_iter()
    .filter_map(|(target, label)| {
        let name = format!("{slug}-{label}");
        let descriptor = inventory.all.get(&name)?;
        Some(TerminalSlotView {
            target,
            name,
            id: descriptor.id.clone(),
            created_at: descriptor.created_at,
        })
    })
    .collect()
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

fn opencode_input_state<'a>(
    activity: &crate::session_activity::SessionActivitySnapshot,
    key: &crate::session_source::SessionKey,
    observed: &'a str,
) -> &'a str {
    if key.harness != wt_core::HarnessId::Opencode {
        return observed;
    }
    match activity
        .opencode_inputs
        .iter()
        .find(|input| &input.key == key)
    {
        Some(input) => match input.pending {
            Some(true) => "asking",
            Some(false) => observed,
            None => "unknown",
        },
        None => observed,
    }
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
    #[test]
    fn native_pending_input_is_exact_and_clears_back_to_observed_state() {
        use crate::session_activity::{OpenCodeInputSnapshot, SessionActivitySnapshot};
        use crate::session_source::SessionKey;
        let key = SessionKey {
            slug: "one".into(),
            harness: wt_core::HarnessId::Opencode,
            session_id: "ses_one".into(),
        };
        let mut activity = SessionActivitySnapshot {
            opencode_inputs: vec![OpenCodeInputSnapshot {
                key: key.clone(),
                pending: Some(true),
            }],
            ..Default::default()
        };
        assert_eq!(
            super::opencode_input_state(&activity, &key, "working"),
            "asking"
        );
        activity.opencode_inputs[0].pending = Some(false);
        assert_eq!(
            super::opencode_input_state(&activity, &key, "working"),
            "working"
        );
        assert_eq!(super::opencode_input_state(&activity, &key, "idle"), "idle");
        activity.opencode_inputs[0].pending = None;
        assert_eq!(
            super::opencode_input_state(&activity, &key, "idle"),
            "unknown"
        );
        let other = SessionKey {
            session_id: "replacement".into(),
            ..key.clone()
        };
        assert_eq!(
            super::opencode_input_state(&activity, &other, "working"),
            "working"
        );
        let codex = SessionKey {
            harness: wt_core::HarnessId::Codex,
            ..key
        };
        assert_eq!(
            super::opencode_input_state(&activity, &codex, "idle"),
            "idle"
        );
    }

    use super::*;
    use crate::session_source::{DiscoveredSession, SessionKey};
    use wt_core::Domain;
    use wt_harness::{HarnessExtras, HarnessSession};
    use wt_tui::BoardRow;

    #[test]
    fn opencode_pending_input_updates_slots_and_session_view_together() {
        let key = SessionKey {
            slug: "one".into(),
            harness: wt_core::HarnessId::Opencode,
            session_id: "ses_one".into(),
        };
        let source = SourceSnapshot::test_live(
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
        let mut inventory = SessionInventory::default();
        inventory.all.insert(
            "one-opencode".into(),
            crate::session_source::SessionDescriptor {
                name: "one-opencode".into(),
                id: "$1".into(),
                created_at: 1,
                attached_clients: 0,
                window_count: 1,
                harness_session_id: Some("ses_one".into()),
            },
        );
        inventory
            .harness_session_ids
            .insert("one-opencode".into(), "ses_one".into());
        let discoveries = SourceSnapshot::test_live(
            Some(vec![DiscoveredSession {
                key: key.clone(),
                session: HarnessSession {
                    display_name: "one".into(),
                    session_id: "ses_one".into(),
                    tmux_session_name: "one-opencode".into(),
                    last_active_ms: None,
                    is_live: true,
                    extras: HarnessExtras {
                        derived_state: Some(DerivedState::Working),
                        ..Default::default()
                    },
                },
            }]),
            SourceState::Ready,
            Domain::Sessions,
        );
        for (pending, expected) in [
            (Some(true), "asking"),
            (Some(false), "working"),
            (None, "unknown"),
        ] {
            let activity = SessionActivitySnapshot {
                opencode_inputs: vec![crate::session_activity::OpenCodeInputSnapshot {
                    key: key.clone(),
                    pending,
                }],
                ..Default::default()
            };
            let result = compose(
                &mut FailureClock::default(),
                source.clone(),
                SourceSnapshot::test_live(
                    Some(inventory.clone()),
                    SourceState::Ready,
                    Domain::Sessions,
                ),
                discoveries.clone(),
                SourceSnapshot::test_live(Some(activity), SourceState::Ready, Domain::Sessions),
                &[],
            );
            let board = result.data().unwrap();
            assert_eq!(board.live_slots["one"][0].state, expected);
            assert_eq!(board.rows[0].live_sessions[0].state, expected);
            assert_eq!(board.rows[0].sessions[0].state, expected);
        }
    }

    #[test]
    fn physical_slot_activity_does_not_require_or_fabricate_history_identity() {
        let source = SourceSnapshot::test_live(
            Some(Board {
                rows: vec![
                    BoardRow {
                        slug: "manager".into(),
                        ..Default::default()
                    },
                    BoardRow {
                        slug: "manager".into(),
                        host: Some("remote".into()),
                        ..Default::default()
                    },
                    BoardRow {
                        slug: "other".into(),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            }),
            SourceState::Ready,
            Domain::Git,
        );
        let mut inventory = SessionInventory::default();
        inventory.all.insert(
            "manager-codex".into(),
            crate::session_source::SessionDescriptor {
                name: "manager-codex".into(),
                id: "$42".into(),
                created_at: 42,
                attached_clients: 0,
                window_count: 1,
                harness_session_id: None,
            },
        );
        for state in ["working", "asking", "unknown"] {
            inventory
                .pane_activity
                .insert("manager-codex".into(), state.into());
            let result = compose(
                &mut FailureClock::default(),
                source.clone(),
                SourceSnapshot::test_live(
                    Some(inventory.clone()),
                    SourceState::Ready,
                    Domain::Sessions,
                ),
                SourceSnapshot::default(),
                SourceSnapshot::default(),
                &[],
            );
            let board = result.data().unwrap();
            assert_eq!(board.live_slots["manager"][0].state, state);
            assert_eq!(board.rows[0].live_sessions[0].state, state);
            assert!(board.rows[0].sessions.is_empty(), "no invented transcript");
            assert!(
                board.rows[1].live_sessions.is_empty(),
                "same slug on another host"
            );
            assert!(board.rows[2].live_sessions.is_empty(), "unrelated row");
            assert!(board.slot_sessions.is_empty());
        }
        let result = compose(
            &mut FailureClock::default(),
            source.clone(),
            SourceSnapshot::test_live(
                Some(inventory),
                SourceState::Failed("offline".into()),
                Domain::Sessions,
            ),
            SourceSnapshot::default(),
            SourceSnapshot::default(),
            &[],
        );
        assert!(result.data().unwrap().live_slots.is_empty());
        assert!(
            result
                .data()
                .unwrap()
                .rows
                .iter()
                .all(|row| row.live_sessions.is_empty())
        );
        let result = compose(
            &mut FailureClock::default(),
            source,
            SourceSnapshot::test_live(
                Some(SessionInventory::default()),
                SourceState::Ready,
                Domain::Sessions,
            ),
            SourceSnapshot::default(),
            SourceSnapshot::default(),
            &[],
        );
        assert!(result.data().unwrap().live_slots.is_empty());
    }

    #[test]
    fn prepared_menus_need_a_ready_inventory_and_skip_remote_rows() {
        let source = SourceSnapshot::test_live(
            Some(Board {
                rows: vec![
                    BoardRow {
                        key: "one".into(),
                        slug: "one".into(),
                        path: "/work/one".into(),
                        branch: "one".into(),
                        ..Default::default()
                    },
                    BoardRow {
                        key: "remote:one".into(),
                        slug: "one".into(),
                        host: Some("remote".into()),
                        ..Default::default()
                    },
                ],
                display: wt_tui::DisplayPolicy {
                    harnesses: wt_core::HarnessId::ALL.to_vec(),
                    ..Default::default()
                },
                ..Default::default()
            }),
            SourceState::Ready,
            Domain::Git,
        );
        let mut inventory = SessionInventory::default();
        for (name, id, created_at) in [("one-shell", "$3", 7), ("one-codex", "$1", 5)] {
            inventory.all.insert(
                name.into(),
                crate::session_source::SessionDescriptor {
                    name: name.into(),
                    id: id.into(),
                    created_at,
                    attached_clients: 0,
                    window_count: 1,
                    harness_session_id: (name == "one-codex").then(|| "exact".into()),
                },
            );
        }
        let discoveries = SourceSnapshot::test_live(
            Some(vec![DiscoveredSession {
                key: SessionKey {
                    slug: "one".into(),
                    harness: wt_core::HarnessId::Codex,
                    session_id: "exact".into(),
                },
                session: HarnessSession {
                    display_name: "primary".into(),
                    session_id: "exact".into(),
                    tmux_session_name: "one-codex".into(),
                    last_active_ms: None,
                    is_live: true,
                    extras: Default::default(),
                },
            }]),
            SourceState::Ready,
            Domain::Sessions,
        );
        let inventory =
            SourceSnapshot::test_live(Some(inventory), SourceState::Ready, Domain::Sessions);
        let result = compose(
            &mut FailureClock::default(),
            source.clone(),
            inventory.clone(),
            discoveries.clone(),
            SourceSnapshot::default(),
            &[],
        );
        let board = result.data().unwrap();
        let entries = &board.session_menus
            [&wt_tui::session_menu_key(Some("one"), wt_tui::SessionTarget::Harness)];
        assert_eq!(entries.len(), 1);
        assert!(entries[0].is_live);
        assert_eq!(
            entries[0].selection.live_slot.as_ref().unwrap().id,
            "$1",
            "the entry carries the exact tmux identity a selection witnesses"
        );
        assert!(!board.session_menus.contains_key("remote:one"));
        assert_eq!(board.terminals["one"][0].name, "one-shell");
        assert_eq!(board.terminals["one"][0].id, "$3");
        assert_eq!(board.terminals["one"][0].created_at, 7);
        assert!(!board.terminals.contains_key("remote:one"));

        // Without a Ready inventory nothing is claimed as prepared or stopped.
        let failed = compose(
            &mut FailureClock::default(),
            source,
            SourceSnapshot::default().with_state(SourceState::Failed("tmux down".into())),
            discoveries,
            SourceSnapshot::default(),
            &[],
        );
        let failed = failed.data().unwrap();
        assert!(failed.session_menus.is_empty());
        assert!(failed.terminals.is_empty());
    }

    /// A lasting discovery failure recomposed on every activity update keeps
    /// one attention time: the board does not change, so it is not
    /// republished, and an acknowledgement of the line sticks.
    #[test]
    fn a_lasting_lane_failure_recomposes_to_the_same_board() {
        let board =
            SourceSnapshot::test_live(Some(Board::default()), SourceState::Ready, Domain::Git);
        let failed = SourceSnapshot::<SessionDiscoveries>::default()
            .with_state(SourceState::Failed("opencode unavailable".into()));
        let mut failures = FailureClock::default();
        let mut pass = || {
            compose(
                &mut failures,
                board.clone(),
                SourceSnapshot::default(),
                failed.clone(),
                SourceSnapshot::default(),
                &[],
            )
        };
        let first = pass();
        assert_eq!(first.data().unwrap().attention.len(), 1);
        std::thread::sleep(std::time::Duration::from_millis(3));
        assert!(first.same_view(&pass()));
    }

    /// A failure that clears while the board has no data and then recurs is
    /// a new event, not the old one still running.
    #[test]
    fn a_failure_recurring_after_a_board_gap_gets_a_new_time() {
        let board =
            SourceSnapshot::test_live(Some(Board::default()), SourceState::Ready, Domain::Git);
        let failed = SourceSnapshot::<SessionDiscoveries>::default()
            .with_state(SourceState::Failed("opencode unavailable".into()));
        let mut failures = FailureClock::default();
        let mut pass = |board: SourceSnapshot<Board>| {
            compose(
                &mut failures,
                board,
                SourceSnapshot::default(),
                failed.clone(),
                SourceSnapshot::default(),
                &[],
            )
        };
        let first = pass(board.clone()).data().unwrap().attention[0].at_ms;
        pass(SourceSnapshot::default());
        std::thread::sleep(std::time::Duration::from_millis(3));
        let recurred = pass(board).data().unwrap().attention[0].at_ms;
        assert!(recurred > first);
    }

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
            &mut FailureClock::default(),
            board,
            SourceSnapshot::default().with_state(SourceState::Failed("tmux unavailable".into())),
            discoveries,
            SourceSnapshot::default(),
            &[],
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

    #[test]
    fn unproven_reused_slots_are_unknown_until_exact_uuid_is_captured() {
        for harness in [wt_core::HarnessId::Codex, wt_core::HarnessId::Pi] {
            let slot = format!("one-{}", harness.as_str());
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
                Some(
                    vec!["old", "current"]
                        .into_iter()
                        .map(|id| DiscoveredSession {
                            key: SessionKey {
                                slug: "one".into(),
                                harness,
                                session_id: id.into(),
                            },
                            session: HarnessSession {
                                display_name: id.into(),
                                session_id: id.into(),
                                tmux_session_name: slot.clone(),
                                last_active_ms: None,
                                is_live: true,
                                extras: HarnessExtras {
                                    derived_state: Some(DerivedState::Idle),
                                    ..Default::default()
                                },
                            },
                        })
                        .collect(),
                ),
                SourceState::Ready,
                Domain::Sessions,
            );
            let mut inventory = SessionInventory::default();
            inventory.all.insert(
                slot.clone(),
                crate::session_source::SessionDescriptor {
                    name: slot.clone(),
                    id: "tmux-id".into(),
                    created_at: 0,
                    attached_clients: 1,
                    window_count: 1,
                    harness_session_id: None,
                },
            );
            let inventory =
                SourceSnapshot::test_live(Some(inventory), SourceState::Ready, Domain::Sessions);
            let activity = SourceSnapshot::default();
            let result = compose(
                &mut FailureClock::default(),
                board.clone(),
                inventory.clone(),
                discoveries.clone(),
                activity.clone(),
                &[],
            );
            let sessions = &result.data().unwrap().rows[0].sessions;
            assert!(
                sessions
                    .iter()
                    .all(|session| !session.live && session.state == "unknown")
            );
            assert!(result.data().unwrap().rows[0].badge.is_empty());

            let mut inventory = inventory.data().unwrap().as_ref().clone();
            inventory
                .harness_session_ids
                .insert(slot.clone(), "current".into());
            let inventory =
                SourceSnapshot::test_live(Some(inventory), SourceState::Ready, Domain::Sessions);
            let result = compose(
                &mut FailureClock::default(),
                board,
                inventory,
                discoveries,
                activity,
                &[],
            );
            let sessions = &result.data().unwrap().rows[0].sessions;
            let old = sessions.iter().find(|session| session.id == "old").unwrap();
            let current = sessions
                .iter()
                .find(|session| session.id == "current")
                .unwrap();
            assert!(!old.live);
            assert_eq!(old.state, "idle");
            assert!(current.live);
            assert_eq!(current.state, "idle");
        }
    }
}
