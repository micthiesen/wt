//! The list viewport across keys, the wheel, folds and refreshed boards,
//! pinned on drawn frames.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{Terminal, backend::TestBackend, layout::Rect};
use wt_runtime::{SourceSnapshot, SourceState};

use super::render;
use crate::model::{ARCHIVED_SECTION, InputResult};
use crate::{Board, BoardRow, BoardSection, Model, ReviewRequestRow, theme};

const INBOX: &str = "\0inbox";

fn row(key: &str) -> BoardRow {
    BoardRow {
        key: key.into(),
        slug: key.into(),
        title: key.into(),
        ..Default::default()
    }
}

/// (section key, folded, row keys)
fn board(spec: &[(&str, bool, Vec<String>)], reviews: usize) -> Board {
    let mut rows = Vec::new();
    let mut sections = Vec::new();
    for (key, folded, keys) in spec {
        let start = rows.len();
        for k in keys {
            let mut r = row(k);
            r.archived = *key == ARCHIVED_SECTION;
            rows.push(r);
        }
        let title = match *key {
            INBOX => "Inbox".to_owned(),
            ARCHIVED_SECTION => "Archived".to_owned(),
            crate::LANDED_SECTION => crate::LANDED_TITLE.to_owned(),
            other => other.to_owned(),
        };
        sections.push(BoardSection {
            key: (*key).into(),
            title,
            folded: *folded,
            rows: (start..rows.len()).collect(),
        });
    }
    Board {
        rows,
        sections,
        review_requests: (0..reviews)
            .map(|i| ReviewRequestRow {
                url: format!("https://x/pr/{i}"),
                title: format!("review{i}"),
                number: i as u64,
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    }
}

fn keys(prefix: &str, n: usize) -> Vec<String> {
    (0..n).map(|i| format!("{prefix}{i}")).collect()
}

fn big() -> Vec<(&'static str, bool, Vec<String>)> {
    vec![
        ("One", false, keys("a", 10)),
        ("Two", false, keys("b", 10)),
        ("Three", true, keys("c", 5)),
        (INBOX, false, keys("i", 3)),
        (crate::LANDED_SECTION, true, keys("l", 8)),
        (ARCHIVED_SECTION, true, keys("z", 4)),
    ]
}

fn snap(board: Board) -> SourceSnapshot<Board> {
    SourceSnapshot::test_live(Some(board), SourceState::Ready, wt_core::Domain::Git)
}

fn screen(model: &mut Model, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(44, height + 2)).unwrap();
    terminal
        .draw(|frame| render(frame, model, frame.area()))
        .unwrap();
    let buffer = terminal.backend().buffer();
    let mut out = String::new();
    for y in 1..=height {
        let text: String = (1..43).map(|x| buffer[(x, y)].symbol()).collect();
        let sel = (1..43).any(|x| buffer[(x, y)].bg == theme::SELECTED_BG);
        out.push_str(&format!(
            "{} {}\n",
            if sel { ">" } else { " " },
            text.trim_end()
        ));
    }
    out
}

fn cursor_visible(model: &mut Model, height: u16) -> bool {
    screen(model, height).lines().any(|l| l.starts_with('>'))
}

fn key(model: &mut Model, code: KeyCode) -> InputResult {
    model.input(KeyEvent::from(code), 12)
}
fn ch(model: &mut Model, c: char) -> InputResult {
    key(model, KeyCode::Char(c))
}
fn ctrl(model: &mut Model, c: char) -> InputResult {
    model.input(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL), 12)
}
fn wheel(model: &mut Model, down: bool) {
    let event = crossterm::event::MouseEvent {
        kind: if down {
            crossterm::event::MouseEventKind::ScrollDown
        } else {
            crossterm::event::MouseEventKind::ScrollUp
        },
        column: 5,
        row: 5,
        modifiers: KeyModifiers::NONE,
    };
    assert!(crate::mouse::scroll(model, event, Rect::new(0, 0, 100, 30)));
}
fn goto(model: &mut Model, target: &str) {
    ch(model, 'g');
    for _ in 0..200 {
        if model.selected_row().is_some_and(|r| r.key == target) {
            return;
        }
        ch(model, 'j');
    }
    panic!("no row {target}");
}
fn sel(model: &Model) -> String {
    match model.selected_row() {
        Some(r) => r.key.clone(),
        None => format!(
            "{:?}/{:?}",
            model.selected_item(),
            model.selected_section().map(|s| s.title.clone())
        ),
    }
}

fn land_spec(folded: bool) -> Vec<(&'static str, bool, Vec<String>)> {
    vec![
        ("One", false, keys("a", 10)),
        ("Two", true, keys("b", 10)),
        (INBOX, false, keys("i", 3)),
        (crate::LANDED_SECTION, folded, keys("l", 8)),
    ]
}

fn cursor_row(model: &mut Model, height: u16) -> Option<usize> {
    screen(model, height)
        .lines()
        .position(|l| l.starts_with('>'))
}

#[test]
fn a_key_held_past_the_last_row_does_not_stop_the_viewport_following() {
    let mut m = Model::default();
    m.apply(snap(board(&big(), 0)));
    ch(&mut m, 'G');
    screen(&mut m, 12);
    ch(&mut m, 'j');
    screen(&mut m, 12);
    let InputResult::Action(a) = key(&mut m, KeyCode::Tab) else {
        panic!()
    };
    m.begin_action(1, &a);
    let drawn = screen(&mut m, 12);
    assert!(
        drawn
            .lines()
            .any(|l| l.starts_with('>') && l.contains("z0")),
        "{drawn}"
    );
    assert_eq!(
        drawn.lines().filter(|l| l.contains(" z")).count(),
        4,
        "{drawn}"
    );

    let mut m = Model::default();
    m.apply(snap(board(&big(), 0)));
    ch(&mut m, 'G');
    screen(&mut m, 12);
    ch(&mut m, 'j');
    screen(&mut m, 12);
    let mut spec = big();
    spec[0].2.push("a-new".into());
    m.apply(snap(board(&spec, 0)));
    assert!(cursor_visible(&mut m, 12));
}

#[test]
fn a_requested_selection_is_shown_after_the_wheel() {
    let mut m = Model::default();
    m.apply(snap(board(&big(), 0)));
    screen(&mut m, 12);
    wheel(&mut m, true);
    m.reply(crate::UiReply {
        select_when_visible: Some("i-new".into()),
        ..Default::default()
    });
    let mut spec = big();
    spec[3].2.push("i-new".into());
    m.apply(snap(board(&spec, 0)));
    assert_eq!(sel(&m), "i-new");
    assert!(cursor_visible(&mut m, 12));
}

#[test]
fn unfolding_brings_the_section_into_view() {
    let mut m = Model::default();
    m.apply(snap(board(&land_spec(true), 0)));
    ch(&mut m, 'G');
    screen(&mut m, 12);
    let InputResult::Action(a) = key(&mut m, KeyCode::Tab) else {
        panic!()
    };
    m.begin_action(1, &a);
    let drawn = screen(&mut m, 12);
    assert_eq!(
        drawn.lines().filter(|l| l.contains(" l")).count(),
        8,
        "{drawn}"
    );
    // Neither a redraw nor the confirming board scrolls it back.
    let mut open = land_spec(true);
    open[3].1 = false;
    m.apply(snap(board(&open, 0)));
    assert_eq!(screen(&mut m, 12), drawn);
    // A section taller than the pane starts under the cursor's usual context.
    let mut spec = land_spec(true);
    spec[3].2 = keys("l", 30);
    let mut m = Model::default();
    m.apply(snap(board(&spec, 0)));
    ch(&mut m, 'G');
    screen(&mut m, 12);
    let InputResult::Action(a) = key(&mut m, KeyCode::Tab) else {
        panic!()
    };
    m.begin_action(1, &a);
    let drawn = screen(&mut m, 12);
    assert!(drawn.lines().nth(2).unwrap().contains("Landed"), "{drawn}");
    assert!(drawn.lines().nth(3).unwrap().starts_with('>'), "{drawn}");
    assert_eq!(screen(&mut m, 12), drawn);
}

#[test]
fn a_refresh_holds_the_cursor_line_and_a_scrolled_viewport() {
    let mut m = Model::default();
    m.apply(snap(board(&big(), 0)));
    goto(&mut m, "b4");
    ch(&mut m, 'k');
    ch(&mut m, 'k');
    ch(&mut m, 'k');
    let before = cursor_row(&mut m, 12);
    let mut spec = big();
    spec[0].2.insert(0, "a-new".into());
    spec[0].2.insert(0, "a-new2".into());
    m.apply(snap(board(&spec, 0)));
    assert_eq!(cursor_row(&mut m, 12), before);
    spec[0].2.drain(0..4);
    m.apply(snap(board(&spec, 0)));
    assert_eq!(cursor_row(&mut m, 12), before);
    // Scrolled away with the wheel: the rows being read stay where they are.
    wheel(&mut m, true);
    wheel(&mut m, true);
    let reading = screen(&mut m, 12);
    spec[0].2.insert(0, "a-x".into());
    spec[0].2.insert(0, "a-y".into());
    m.apply(snap(board(&spec, 0)));
    let after = screen(&mut m, 12);
    assert_eq!(
        reading.lines().take(12).collect::<Vec<_>>(),
        after.lines().take(12).collect::<Vec<_>>()
    );
    // A key between the refresh and the draw wins: the cursor moves on screen.
    let mut m = Model::default();
    m.apply(snap(board(&big(), 0)));
    goto(&mut m, "a3");
    let before = cursor_row(&mut m, 12).unwrap();
    ch(&mut m, 'j');
    m.apply(snap(board(&big(), 0)));
    assert_eq!(cursor_row(&mut m, 12), Some(before + 1));
    // Folding from inside leaves the rows above the fold where they were.
    let mut m = Model::default();
    m.apply(snap(board(&big(), 0)));
    goto(&mut m, "b4");
    let top = screen(&mut m, 12).lines().next().unwrap().to_owned();
    let InputResult::Action(a) = key(&mut m, KeyCode::Tab) else {
        panic!()
    };
    m.begin_action(1, &a);
    m.apply(snap(board(&big(), 0)));
    assert_eq!(screen(&mut m, 12).lines().next().unwrap(), top);
}

#[test]
fn an_acting_key_returns_a_scrolled_viewport_to_the_cursor() {
    let mut m = Model::default();
    m.apply(snap(board(&big(), 0)));
    goto(&mut m, "a2");
    screen(&mut m, 12);
    for _ in 0..5 {
        wheel(&mut m, true);
    }
    assert!(!cursor_visible(&mut m, 12));
    // Scrolling another pane is not acting on the row.
    ctrl(&mut m, 'j');
    assert!(!cursor_visible(&mut m, 12));
    assert!(matches!(ch(&mut m, 'a'), InputResult::Action(_)));
    assert!(cursor_visible(&mut m, 12));
}

#[test]
fn a_renamed_section_keeps_the_cursor_on_its_row() {
    let mut m = Model::default();
    m.apply(snap(board(&big(), 0)));
    goto(&mut m, "b4");
    let mut spec = big();
    spec[1].0 = "Renamed";
    spec[0].2.push("a-new".into());
    m.apply(snap(board(&spec, 0)));
    assert_eq!(sel(&m), "b4");
}

#[test]
fn tiny_panes_keep_the_cursor() {
    let mut m = Model::default();
    m.apply(snap(board(&big(), 0)));
    ch(&mut m, 'k');
    for h in [1u16, 2, 3] {
        assert!(cursor_visible(&mut m, h), "{h}");
    }
    assert_eq!(ch(&mut m, 'k'), InputResult::Unchanged);
}

#[test]
fn section_jumps_visit_the_reviews_and_stop_at_the_start_of_the_current_block() {
    let mut m = Model::default();
    m.apply(snap(board(&big(), 2)));
    goto(&mut m, "a4");
    let mut trail = Vec::new();
    for _ in 0..7 {
        ctrl(&mut m, 'd');
        trail.push(sel(&m));
    }
    assert_eq!(trail[0], "b0");
    assert_eq!(trail[2], "i0");
    assert!(trail[3].contains("Landed"), "{trail:?}");
    assert!(trail[4].contains("ReviewRequest(0)"), "{trail:?}");
    assert!(trail[5].contains("Archived"), "{trail:?}");
    assert_eq!(trail[6], trail[5]);
    ctrl(&mut m, 'u');
    assert!(sel(&m).contains("ReviewRequest(0)"));
    ch(&mut m, 'j');
    ctrl(&mut m, 'u');
    assert!(sel(&m).contains("ReviewRequest(0)"));
    goto(&mut m, "b5");
    ctrl(&mut m, 'u');
    assert_eq!(sel(&m), "b0");
    ctrl(&mut m, 'u');
    assert_eq!(sel(&m), "a0");
    assert_eq!(ctrl(&mut m, 'u'), InputResult::Unchanged);
}

#[test]
fn a_page_is_half_of_the_list_as_drawn() {
    let mut m = Model::default();
    m.apply(snap(board(&big(), 0)));
    // The terminal is tall, the list pane is not.
    screen(&mut m, 6);
    m.input(KeyEvent::from(KeyCode::PageDown), 40);
    assert_eq!(sel(&m), "a3");
    m.input(KeyEvent::from(KeyCode::PageUp), 40);
    assert_eq!(sel(&m), "a0");
}

#[test]
fn keys_and_a_refresh_in_one_frame_leave_the_cursor_where_the_keys_put_it() {
    let mut spec = big();
    spec[0].2.insert(0, "a-new".into());
    // Refresh first, then keys, then one draw.
    let mut m = Model::default();
    m.apply(snap(board(&big(), 0)));
    goto(&mut m, "a3");
    let before = cursor_row(&mut m, 12).unwrap();
    m.apply(snap(board(&spec, 0)));
    ch(&mut m, 'j');
    ch(&mut m, 'j');
    assert_eq!(sel(&m), "a5");
    assert_eq!(cursor_row(&mut m, 12), Some(before + 3));
    // Keys first, then the refresh.
    let mut m = Model::default();
    m.apply(snap(board(&big(), 0)));
    goto(&mut m, "a3");
    let before = cursor_row(&mut m, 12).unwrap();
    ch(&mut m, 'j');
    ch(&mut m, 'j');
    m.apply(snap(board(&spec, 0)));
    assert_eq!(cursor_row(&mut m, 12), Some(before + 3));
    // A wheel flick and a refresh in one frame: the wheel's offset stands.
    let mut m = Model::default();
    m.apply(snap(board(&big(), 0)));
    screen(&mut m, 12);
    wheel(&mut m, true);
    screen(&mut m, 12);
    m.apply(snap(board(&spec, 0)));
    wheel(&mut m, true);
    screen(&mut m, 12);
    assert_eq!(m.offset, 6);
}
