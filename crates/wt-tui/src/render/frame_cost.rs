//! Frame cost over a board the size of a busy fleet. The ignored test prints
//! what one cursor step costs; run it with
//! `cargo test -p wt-tui --release frame_cost -- --ignored --nocapture`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{Terminal, TerminalOptions, Viewport, backend::CrosstermBackend, layout::Rect};
use wt_core::{OwedKind, OwedState, VerifyStage, WorkRisk, WorkState, WorkStatusRecord};

use crate::{
    ActivityLine, AttentionLine, Board, BoardRow, BoardSection, CheckState, DiffStat,
    DisplayPolicy, GitPresentation, LANDED_SECTION, Model, OwedView, PrCommentView, PrPresentation,
    PreparedDetailGroup, SessionView, WatchActionKind, WatchConditionView, WatchView,
    WorkPresentation,
    model::{ARCHIVED_SECTION, INBOX_SECTION},
};

const NOTE: &str = "Session refresh renews once per tab instead of once per request, and the retry queue drains in order.\nREVERT:   safe\nIF WRONG: sign-in, users bounced to the login page after an expired token\nUNTESTED: the provider's real expiry on production";

const TITLES: [&str; 6] = [
    "Rework session refresh",
    "Cache the avatar lookup",
    "Tidy the settings page",
    "Start the importer",
    "Billing webhook retries",
    "Nightly export for brand",
];

fn group(id: &str, lines: &[&str]) -> PreparedDetailGroup {
    PreparedDetailGroup {
        id: id.into(),
        label: id.into(),
        lines: lines.iter().map(|line| (*line).to_owned()).collect(),
        error: None,
    }
}

fn busy_row(index: usize) -> BoardRow {
    let waits = |text: &str| WatchConditionView {
        key: text.into(),
        text: text.into(),
        label: text.into(),
        ..Default::default()
    };
    let mut check = OwedView::new(
        OwedKind::Check,
        OwedState::Waiting,
        "Chat topic sweep and queue run on the Worker crons. STEPS: 1. post three 50+ char messages in one channel of a brand with topics 2. after the next :07/:22/:37/:52, entity_topics rows exist for the brand and the queue table is empty again 3. the Worker log shows one sweep and no retries",
    );
    check.stage = Some(VerifyStage::Staging);
    let mut step = OwedView::new(
        OwedKind::Step,
        OwedState::Due,
        "confirm prod logs, then open the PR retiring the old path",
    );
    step.by = Some(format!("row-{index}"));
    step.due_at = Some("11:36".into());
    let mut gate = OwedView::new(
        OwedKind::Gate,
        OwedState::Waiting,
        "prod secret missing from the vault the worker reads",
    );
    gate.conditions = vec![waits("#3806 in production")];
    BoardRow {
        key: format!("row-{index}"),
        slug: format!("eng-{index}-session-refresh"),
        title: format!(
            "{} {index} so an expired token is renewed once per tab and the retry queue drains in order",
            TITLES[index % TITLES.len()]
        ),
        branch: format!("feature/eng-{index}"),
        path: format!("/very/long/path/to/the/worktrees/eng-{index}-session-refresh"),
        issue_id: Some(format!("ENG-{index}")),
        issue_status: Some("In Progress".into()),
        needs_attention: index.is_multiple_of(5),
        work: Some(WorkPresentation {
            record: Some(WorkStatusRecord {
                risk: Some(WorkRisk::Low),
                note: Some(NOTE.into()),
                ..WorkStatusRecord::new(WorkState::Ready, "2026-10-09T00:00:00Z")
            }),
            effective_state: Some(WorkState::Ready),
            age: Some("2h".into()),
            ..Default::default()
        }),
        git: GitPresentation {
            tracked_changes: Some(1),
            untracked_files: Some(0),
            upstream: Some(format!("origin/feature/eng-{index}")),
            ahead: Some(2),
            behind: Some(0),
            base_ahead: Some(3),
            base_behind: Some(1),
            diff: Some(DiffStat {
                files: 12,
                added: 340,
                removed: 87,
            }),
            base_conflicts: Some(vec!["src/login.rs".into(), "src/session/refresh.rs".into()]),
            ..Default::default()
        },
        pr: Some(PrPresentation {
            number: Some(3000 + index as u64),
            state: Some("OPEN".into()),
            checks: CheckState::Fail,
            failed_checks: vec!["lint".into(), "e2e (chromium)".into()],
            comments: (0..4)
                .map(|comment| PrCommentView {
                    author: format!("reviewer-{comment}"),
                    body: "Please add a test for the path where the token expires while two tabs are open; the queue ordering is not covered and I could not convince myself the retry is idempotent.".into(),
                    age: Some("3h".into()),
                })
                .collect(),
            unresolved_threads: 2,
            ..Default::default()
        }),
        detail_groups: vec![
            group("branch", &["feature/eng-12", "main"]),
            group("issue", &[]),
            group("pr", &[]),
            group("claude", &[]),
            group("git", &[]),
        ],
        sessions: vec![SessionView {
            id: format!("session-{index}"),
            harness: "Claude".into(),
            name: "main".into(),
            state: "idle".into(),
            live: true,
            summary: Some("Renewal now happens in the tab's shared worker; the request interceptor waits on one promise and replays in arrival order. Remaining: the provider's own expiry cannot be forced locally.".into()),
            output: (0..40).map(|line| format!("output line {line} of the session transcript")).collect(),
            ..Default::default()
        }],
        owes: vec![step, gate, check],
        watches: vec![WatchView {
            id: "tell44".into(),
            by: "human".into(),
            conditions: vec![waits("in production")],
            actions: vec![WatchActionKind::Notify],
            ..Default::default()
        }],
        ..Default::default()
    }
}

/// A fleet of `rows` worktrees in the inbox, two named sections, the
/// Landed block and a folded archive, with full activity and attention feeds.
pub(crate) fn busy_model(rows: usize) -> Model {
    let mut board = Board {
        rows: (0..rows).map(busy_row).collect(),
        display: DisplayPolicy {
            primary_harness: "Claude".into(),
            automations: true,
            ..Default::default()
        },
        activity: (0..500)
            .map(|index| ActivityLine {
                at_ms: 1_800_000_000_000 + index * 1000,
                level: "info".into(),
                channel: "activity".into(),
                source: format!("eng-{}-session-refresh", index % 40),
                text:
                    "checks passed; review requested from the configured bot and one human reviewer"
                        .into(),
            })
            .collect(),
        attention: (0..200)
            .map(|index| AttentionLine {
                at_ms: 1_800_000_000_000 + index * 1000,
                source: format!("eng-{}-session-refresh", index % 40),
                text: "needs you: the staging check is due and the gate it waited for has cleared"
                    .into(),
            })
            .collect(),
        ..Default::default()
    };
    let cuts = [
        0,
        rows * 3 / 10,
        rows * 5 / 10,
        rows * 7 / 10,
        rows * 9 / 10,
        rows,
    ];
    let keys = [
        INBOX_SECTION,
        "Auth",
        "Importer",
        LANDED_SECTION,
        ARCHIVED_SECTION,
    ];
    for (section, key) in keys.into_iter().enumerate() {
        board.sections.push(BoardSection {
            key: key.into(),
            title: key.trim_start_matches('\0').into(),
            folded: key == ARCHIVED_SECTION,
            rows: (cuts[section]..cuts[section + 1]).collect(),
        });
    }
    for &row in &board.sections[3].rows {
        board.rows[row].git.landed_on = Some(crate::LandingKind::Base);
        board.rows[row].owed_phase = Some(wt_core::OwedPhase::Waiting);
    }
    for &row in &board.sections[4].rows {
        board.rows[row].archived = true;
    }
    let mut model = Model {
        board: Arc::new(board),
        ..Model::default()
    };
    model.rebuild_items();
    model.selected = Some(0);
    model
}

/// A real backend over a byte sink, so the cost includes the cell diff and
/// the escape sequences a terminal would receive.
fn terminal(width: u16, height: u16) -> (Terminal<CrosstermBackend<Sink>>, Sink) {
    let sink = Sink::default();
    let terminal = Terminal::with_options(
        CrosstermBackend::new(sink.clone()),
        TerminalOptions {
            viewport: Viewport::Fixed(Rect::new(0, 0, width, height)),
        },
    )
    .unwrap();
    (terminal, sink)
}

#[derive(Clone, Default)]
struct Sink(std::rc::Rc<std::cell::RefCell<Vec<u8>>>);

impl std::io::Write for Sink {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn percentile(samples: &mut [Duration], percent: usize) -> Duration {
    samples.sort();
    samples[(samples.len() * percent / 100).min(samples.len() - 1)]
}

#[test]
#[ignore = "prints timings; not an assertion"]
fn frame_cost_of_holding_a_movement_key() {
    for rows in [60, 150] {
        let mut model = busy_model(rows);
        let (mut terminal, sink) = terminal(200, 60);
        let down = KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE);
        let up = KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE);
        terminal
            .draw(|frame| super::render(frame, &mut model))
            .unwrap();
        let (mut input, mut draw, mut bytes) = (Vec::new(), Vec::new(), Vec::new());
        let stops = model.item_count();
        for step in 0..stops * 4 {
            let key = if (step / stops).is_multiple_of(2) {
                down
            } else {
                up
            };
            let start = Instant::now();
            model.input(key, 56);
            input.push(start.elapsed());
            sink.0.borrow_mut().clear();
            let start = Instant::now();
            terminal
                .draw(|frame| super::render(frame, &mut model))
                .unwrap();
            draw.push(start.elapsed());
            bytes.push(sink.0.borrow().len());
        }
        let moved = |from: usize| -> Vec<Duration> {
            draw.iter()
                .zip(&bytes)
                .filter(|(_, bytes)| (**bytes >= 3000) == (from > 0))
                .map(|(time, _)| *time)
                .collect()
        };
        let (mut small, mut large) = (moved(0), moved(1));
        println!(
            "  frames under 3000 bytes: {} p50 {:?} p90 {:?}; larger: {} p50 {:?} p90 {:?}",
            small.len(),
            percentile(&mut small, 50),
            percentile(&mut small, 90),
            large.len(),
            percentile(&mut large, 50),
            percentile(&mut large, 90),
        );
        let total: Duration = draw.iter().sum();
        bytes.sort_unstable();
        println!(
            "{rows} rows: {} keys; input p50 {:?} max {:?}; frame p50 {:?} p90 {:?} max {:?} mean {:?}; bytes p50 {} max {}",
            draw.len(),
            percentile(&mut input, 50),
            percentile(&mut input, 100),
            percentile(&mut draw, 50),
            percentile(&mut draw, 90),
            percentile(&mut draw, 100),
            total / draw.len() as u32,
            bytes[bytes.len() / 2],
            bytes[bytes.len() - 1],
        );
        let mut panes = [
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ];
        let mut test = Terminal::new(ratatui::backend::TestBackend::new(200, 60)).unwrap();
        for _ in 0..200 {
            model.input(down, 56);
            test.draw(|frame| {
                let layout = super::panes(frame.area(), false);
                let start = Instant::now();
                super::list::render(frame, &mut model, layout.list);
                panes[0].push(start.elapsed());
                let start = Instant::now();
                super::details::render(frame, &mut model, layout.details);
                panes[1].push(start.elapsed());
                let start = Instant::now();
                let _ = model.output_view(
                    layout.activity.height.saturating_sub(2) as usize,
                    layout.activity.width.saturating_sub(2) as usize,
                );
                panes[2].push(start.elapsed());
                let start = Instant::now();
                super::header(frame, &model, layout.header);
                panes[3].push(start.elapsed());
                let start = Instant::now();
                super::footer(frame, &model, layout.footer);
                panes[4].push(start.elapsed());
                let start = Instant::now();
                super::overlays::render(frame, &mut model, frame.area());
                panes[5].push(start.elapsed());
            })
            .unwrap();
            let start = Instant::now();
            test.draw(|frame| super::render(frame, &mut model)).unwrap();
            panes[6].push(start.elapsed());
        }
        println!(
            "  list p50 {:?}; details p50 {:?}; activity p50 {:?}; header {:?}; footer {:?}; overlays {:?}; whole frame on a test backend {:?}",
            percentile(&mut panes[0], 50),
            percentile(&mut panes[1], 50),
            percentile(&mut panes[2], 50),
            percentile(&mut panes[3], 50),
            percentile(&mut panes[4], 50),
            percentile(&mut panes[5], 50),
            percentile(&mut panes[6], 50),
        );
    }
}
