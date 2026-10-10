//! Fleet projection combines independent snapshots of the same host service.
use crate::{
    context::AppContext,
    host_protocol::{HostSnapshot, HostState},
    host_service::HostService,
    local_source::Metadata,
    remote_host::RemoteHost,
};
use std::sync::Arc;
use wt_runtime::{SourceHandle, SourceSnapshot, SourceState, TaskScope, source_channel};
use wt_tui::{Board, HostChoice};

pub struct Fleet {
    pub board: SourceHandle<Board>,
    pub local: Arc<HostService>,
    pub remotes: Vec<RemoteHost>,
}

pub fn start(scope: &TaskScope, context: &AppContext, local: Arc<HostService>) -> Fleet {
    let remotes = context
        .config
        .remotes
        .iter()
        .map(|remote| RemoteHost::start(scope, context, remote.clone()))
        .collect::<Vec<_>>();
    let (board, mut publisher) = source_channel();
    let (changed, mut changes) = tokio::sync::mpsc::channel(1);
    for remote in &remotes {
        let mut updates = remote.snapshot.subscribe();
        let changed = changed.clone();
        let cancelled = scope.token();
        scope.spawn(async move {
            loop {
                tokio::select! {
                    _ = cancelled.cancelled() => break,
                    next = updates.changed() => {
                        if next.is_err() { break; }
                        updates.borrow_and_update();
                        let _ = changed.try_send(());
                    }
                }
            }
        });
    }
    let owned_local = local.clone();
    let owned_remotes = remotes.clone();
    let config = context.config.clone();
    let cancelled = scope.token();
    scope.spawn(async move {
        let mut updates = owned_local.sources.board.subscribe();
        let mut metadata = owned_local.sources.metadata.subscribe();
        updates.mark_changed(); metadata.mark_changed();
        let mut previous: Option<SourceSnapshot<Board>> = None;
        loop {
            tokio::select! {
                biased;
                _ = cancelled.cancelled() => break,
                refresh = publisher.requested() => {
                    if refresh.is_none() { break; }
                    owned_local.sources.board.refresh();
                    for remote in &owned_remotes { remote.refresh(); }
                    continue;
                }
                update = updates.changed() => { if update.is_err() { break; } updates.borrow_and_update(); }
                update = metadata.changed() => { if update.is_err() { break; } metadata.borrow_and_update(); }
                _ = changes.recv(), if !owned_remotes.is_empty() => {}
            }
            let snapshots = owned_remotes.iter().map(|remote| (&remote.endpoint, remote.snapshot.snapshot())).collect::<Vec<_>>();
            let snapshot = compose(&config, updates.borrow().clone(), metadata.borrow().clone(), &snapshots);
            if previous.as_ref().is_some_and(|old| old.data == snapshot.data && old.state == snapshot.state) { continue; }
            previous = Some(snapshot.clone());
            publisher.publish(snapshot);
        }
    });
    Fleet {
        board,
        local,
        remotes,
    }
}

fn compose(
    config: &wt_config::Config,
    mut snapshot: SourceSnapshot<Board>,
    metadata: SourceSnapshot<Metadata>,
    hosts: &[(&wt_config::RemoteConfig, SourceSnapshot<HostSnapshot>)],
) -> SourceSnapshot<Board> {
    let mut board = snapshot.data.as_deref().cloned().unwrap_or_else(|| Board {
        name: config.repo_id.clone(),
        ..Board::default()
    });
    board.hosts = vec![HostChoice {
        id: None,
        label: "This machine".into(),
    }];
    let mut state = metadata
        .data
        .as_ref()
        .map(|m| m.0.clone())
        .unwrap_or_else(|| serde_json::json!({"slugs": {}}));
    if !state.is_object() {
        state = serde_json::json!({"slugs": {}});
    }
    if !state["slugs"].is_object() {
        state["slugs"] = serde_json::json!({});
    }
    board.attention_seen_ms = state["attentionSeenTs"].as_u64().unwrap_or_default();
    let archived = metadata.data.as_ref().map(|m| &m.1);
    for (endpoint, source) in hosts {
        let host = endpoint.key();
        let label = wt_core::sanitize_terminal_text(&endpoint.label);
        board.hosts.push(HostChoice {
            id: Some(host.clone()),
            label: label.clone(),
        });
        let error = match &source.state {
            SourceState::Failed(error) => Some(error.to_string()),
            _ => source.data.as_ref().and_then(|s| match &s.state {
                HostState::Failed(error) => Some(error.clone()),
                _ => None,
            }),
        };
        if let Some(data) = &source.data {
            for mut row in data.board.iter().flat_map(|b| &b.rows).cloned() {
                let local_key = row.key.clone();
                row.key = wt_core::remote_worktree_ledger_key(&host, &local_key);
                row.host = Some(host.clone());
                row.archived = archived.is_some_and(|keys| keys.contains(&row.key));
                row.badge = format!("▣ {label} {}", row.badge);
                row.details.insert(0, format!("Host: {label}"));
                if let Some(error) = &error {
                    row.details.insert(
                        1,
                        format!("Unavailable: {}", wt_core::sanitize_terminal_text(error)),
                    );
                    row.needs_attention = true;
                    for session in &mut row.sessions {
                        session.live = false;
                        session.state = "unknown".into();
                    }
                }
                // Layout belongs to this controller; facts belong to the host.
                let layout = state["remoteLayouts"][&row.key].as_object().cloned();
                let entry = &mut state["slugs"][&row.key];
                if !entry.is_object() {
                    *entry = serde_json::json!({});
                }
                if let Some(layout) = layout {
                    for field in ["section", "order"] {
                        if let Some(value) = layout.get(field) {
                            entry[field] = value.clone();
                        }
                    }
                }
                if let Some(layout) = data.layout.get(&local_key) {
                    entry["baseBranch"] = serde_json::json!(layout.base_branch);
                    entry["work"] = serde_json::json!(layout.work);
                }
                // Preserve pre-rewrite controller pins until an explicit edit.
                if let Some(title) = entry["manualTitle"]
                    .as_str()
                    .filter(|s| !s.trim().is_empty())
                {
                    row.title = wt_core::sanitize_terminal_text(title);
                }
                for value in [
                    &mut row.title,
                    &mut row.slug,
                    &mut row.branch,
                    &mut row.path,
                    &mut row.badge,
                ] {
                    *value = wt_core::sanitize_terminal_text(value);
                }
                for detail in &mut row.details {
                    *detail = wt_core::sanitize_terminal_text(detail);
                }
                if let Some(steps) = &mut row.verify_steps {
                    *steps = wt_core::sanitize_terminal_text(steps);
                }
                for log in &mut row.logs {
                    log.title = wt_core::sanitize_terminal_text(&log.title);
                    for line in &mut log.lines {
                        *line = wt_core::sanitize_terminal_text(line);
                    }
                }
                for session in &mut row.sessions {
                    for value in [&mut session.name, &mut session.harness, &mut session.state] {
                        *value = wt_core::sanitize_terminal_text(value);
                    }
                    for line in &mut session.output {
                        *line = wt_core::sanitize_terminal_text(line);
                    }
                }
                board.rows.push(row);
            }
            if let Some(remote) = &data.board {
                for mut review in remote.review_requests.clone() {
                    review.host = Some(host.clone());
                    for value in [&mut review.title, &mut review.author] {
                        *value = wt_core::sanitize_terminal_text(value);
                    }
                    review.title = format!("{label} · {}", review.title);
                    review.details.insert(0, format!("Host: {label}"));
                    for line in &mut review.details {
                        *line = wt_core::sanitize_terminal_text(line);
                    }
                    if let Some(error) = &error {
                        review.details.insert(
                            0,
                            format!("Unavailable: {}", wt_core::sanitize_terminal_text(error)),
                        );
                    }
                    board.review_requests.push(review);
                }
                if !remote.perf.is_empty() {
                    board.perf.push(format!("Host: {label}"));
                    board.perf.extend(
                        remote
                            .perf
                            .iter()
                            .map(|line| wt_core::sanitize_terminal_text(line)),
                    );
                }
                for mut row in remote.removed_history.rows.clone() {
                    row.key = wt_core::remote_worktree_ledger_key(&host, &row.key);
                    row.host = Some(host.clone());
                    for value in [
                        &mut row.slug,
                        &mut row.title,
                        &mut row.branch,
                        &mut row.removed_at,
                    ] {
                        *value = wt_core::sanitize_terminal_text(value);
                    }
                    for line in &mut row.details {
                        *line = wt_core::sanitize_terminal_text(line);
                    }
                    for value in [
                        &mut row.work_state,
                        &mut row.blocked_on,
                        &mut row.verify_steps,
                        &mut row.git_state,
                        &mut row.pr_state,
                        &mut row.issue_status,
                    ]
                    .into_iter()
                    .flatten()
                    {
                        *value = wt_core::sanitize_terminal_text(value);
                    }
                    if let Some(error) = &error {
                        row.details.insert(
                            0,
                            format!("Unavailable: {}", wt_core::sanitize_terminal_text(error)),
                        );
                    }
                    board.removed_history.rows.push(row);
                }
                board
                    .activity
                    .extend(remote.activity.iter().cloned().map(|mut line| {
                        line.source =
                            format!("{label}: {}", wt_core::sanitize_terminal_text(&line.source));
                        line.text = wt_core::sanitize_terminal_text(&line.text);
                        line
                    }));
                board
                    .attention
                    .extend(remote.attention.iter().cloned().map(|mut line| {
                        line.source =
                            format!("{label}: {}", wt_core::sanitize_terminal_text(&line.source));
                        line.text = wt_core::sanitize_terminal_text(&line.text);
                        line
                    }));
                // Usage describes the account, which the local header already shows.
                for (slot, logs) in &remote.slot_logs {
                    let mut logs = logs.clone();
                    for log in &mut logs {
                        log.title = wt_core::sanitize_terminal_text(&log.title);
                        for line in &mut log.lines {
                            *line = wt_core::sanitize_terminal_text(line);
                        }
                    }
                    board
                        .slot_logs
                        .insert(wt_core::remote_worktree_ledger_key(&host, slot), logs);
                }
                for (slot, sessions) in &remote.slot_sessions {
                    let mut sessions = sessions.clone();
                    for session in &mut sessions {
                        session.name = wt_core::sanitize_terminal_text(&session.name);
                        session.harness = wt_core::sanitize_terminal_text(&session.harness);
                        session.state = if error.is_some() {
                            "unknown".into()
                        } else {
                            wt_core::sanitize_terminal_text(&session.state)
                        };
                        session.live &= error.is_none();
                        for line in &mut session.output {
                            *line = wt_core::sanitize_terminal_text(line);
                        }
                    }
                    board
                        .slot_sessions
                        .insert(wt_core::remote_worktree_ledger_key(&host, slot), sessions);
                }
            }
        }
        if let Some(error) = error {
            let at_ms = crate::activity_source::epoch_ms();
            let text = wt_core::sanitize_terminal_text(&error);
            board.activity.push(wt_tui::ActivityLine {
                at_ms,
                level: "ERROR".into(),
                channel: "attention".into(),
                source: label.clone(),
                text: text.clone(),
            });
            board.attention.push(wt_tui::AttentionLine {
                at_ms,
                source: label.clone(),
                text,
            });
        }
    }
    board.removed_history.rows.sort_by(|a, b| {
        b.removed_at
            .cmp(&a.removed_at)
            .then_with(|| a.key.cmp(&b.key))
    });
    crate::board_layout::prepare(&mut board, &state, &config.branch.base, config.ui.sort);
    crate::board_layout::refresh_rollups(&mut board);
    crate::activity_source::bound_feeds(&mut board);
    snapshot.data = Some(Arc::new(board));
    snapshot
}

#[cfg(test)]
mod tests {
    use super::*;
    fn ready<T>(data: T) -> SourceSnapshot<T> {
        SourceSnapshot {
            data: Some(Arc::new(data)),
            state: SourceState::Ready,
            updated_at: None,
            revision: 0,
        }
    }
    #[tokio::test]
    async fn remote_layout_uses_controller_section_and_preserves_worker_title() {
        let fixture = crate::commands::test_support::CommandFixture::new()
            .await
            .unwrap();
        let endpoint = wt_config::RemoteConfig {
            host: "builder".into(),
            ..Default::default()
        };
        let key = wt_core::remote_worktree_ledger_key(&endpoint.key(), "one");
        let metadata = ready((
            serde_json::json!({
                "slugs": {"local": {"section": "Review", "order": 1}},
                "remoteLayouts": { &key: { "section": "Review", "order": 9 } },
                "sectionsOrder": ["Review"]
            }),
            Default::default(),
        ));
        let mut local_work =
            wt_core::WorkStatusRecord::new(wt_core::WorkState::Ready, "2026-10-09T00:00:00Z");
        local_work.risk = Some(wt_core::WorkRisk::High);
        let mut remote_work =
            wt_core::WorkStatusRecord::new(wt_core::WorkState::NeedsHuman, "2026-10-09T00:00:00Z");
        remote_work.blocked_on = Some("waiting for review".into());
        let source = ready(HostSnapshot {
            board: Some(Board {
                rows: vec![wt_tui::BoardRow {
                    key: "one".into(),
                    slug: "one".into(),
                    title: "Worker title".into(),
                    branch: "branch".into(),
                    work: Some(wt_tui::WorkPresentation {
                        record: Some(remote_work),
                        effective_state: Some(wt_core::WorkState::NeedsHuman),
                        ..Default::default()
                    }),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            state: HostState::Ready,
            layout: Default::default(),
        });
        let local_work = wt_tui::BoardRow {
            key: "local".into(),
            slug: "local".into(),
            title: "Local work".into(),
            work: Some(wt_tui::WorkPresentation {
                record: Some(local_work),
                effective_state: Some(wt_core::WorkState::Ready),
                ..Default::default()
            }),
            ..Default::default()
        };
        let result = compose(
            &fixture.ctx.config,
            ready(Board {
                rows: vec![local_work],
                ..Default::default()
            }),
            metadata,
            &[(&endpoint, source)],
        );
        let board = result.data.unwrap();
        assert_eq!(
            board
                .rows
                .iter()
                .find(|row| row.slug == "one")
                .unwrap()
                .title,
            "Worker title"
        );
        let review = board
            .sections
            .iter()
            .find(|section| section.key == "Review")
            .unwrap();
        assert_eq!(review.rows.len(), 2);
        assert!(
            review.rollup.states.iter().any(|state| {
                state.state == Some(wt_core::WorkState::Ready) && state.count == 1
            })
        );
        assert!(review.rollup.states.iter().any(|state| {
            state.state == Some(wt_core::WorkState::NeedsHuman) && state.count == 1
        }));
        assert_eq!(review.rollup.risks[0].risk, wt_core::WorkRisk::High);
        assert!(
            review
                .rollup
                .blocked_notes
                .iter()
                .any(|note| note == "one: waiting for review")
        );
        fixture.close().await.unwrap();
    }
    #[tokio::test]
    async fn same_slugs_on_three_hosts_stay_separate_when_one_host_disconnects() {
        let fixture = crate::commands::test_support::CommandFixture::new()
            .await
            .unwrap();
        let row = wt_tui::BoardRow {
            key: "one".into(),
            slug: "one".into(),
            title: "Local".into(),
            branch: "feature/one".into(),
            ..Default::default()
        };
        let local = ready(Board {
            rows: vec![row.clone()],
            ..Default::default()
        });
        let metadata = ready((serde_json::json!({"slugs":{}}), Default::default()));
        let first = wt_config::RemoteConfig {
            host: "builder-a".into(),
            label: "A".into(),
            ..Default::default()
        };
        let second = wt_config::RemoteConfig {
            host: "builder-b".into(),
            label: "B".into(),
            ..Default::default()
        };
        let source = ready(HostSnapshot {
            board: Some(Board {
                rows: vec![row],
                ..Default::default()
            }),
            state: HostState::Ready,
            layout: Default::default(),
        });
        let mut failed = source.clone();
        failed.state = SourceState::Failed("SSH disconnected".into());
        let result = compose(
            &fixture.ctx.config,
            local,
            metadata,
            &[(&first, source), (&second, failed)],
        );
        let board = result.data.unwrap();
        assert_eq!(board.rows.len(), 3);
        assert_eq!(
            board
                .rows
                .iter()
                .map(|row| &row.key)
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            3
        );
        assert!(board.rows[2].needs_attention);
        assert!(!board.rows[1].needs_attention);
        assert!(
            board.rows[2]
                .details
                .iter()
                .any(|line| line.contains("SSH disconnected"))
        );
        fixture.close().await.unwrap();
    }
}
