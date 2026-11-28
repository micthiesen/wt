//! J and K: the row moves at the keypress, each press carries the layout
//! the person saw, and the host's answers settle it in whatever order they
//! arrive.

use crossterm::event::{KeyCode, KeyEvent};
use wt_runtime::{SourceSnapshot, SourceState, Stamp};

use super::*;
use crate::{LayoutChange, MutationKind, ReorderStep, ReplyOutcome, UiAction, UiReply};

fn rows(keys: &[&str]) -> Vec<BoardRow> {
    keys.iter()
        .map(|key| BoardRow {
            key: (*key).into(),
            slug: (*key).into(),
            title: (*key).into(),
            branch: format!("branch-{key}"),
            ..BoardRow::default()
        })
        .collect()
}

/// A board whose sections list their rows by key, in the order drawn.
fn board(sections: &[(&str, &[&str])]) -> Board {
    let keys: Vec<&str> = sections
        .iter()
        .flat_map(|(_, keys)| keys.iter().copied())
        .collect();
    let mut all = keys.clone();
    // Row indices do not follow the layout: a snapshot lists the same rows
    // in one order however its sections arrange them.
    all.sort_unstable();
    Board {
        rows: rows(&all),
        sections: sections
            .iter()
            .map(|(name, keys)| BoardSection {
                key: (*name).into(),
                title: (*name).into(),
                folded: false,
                rows: keys
                    .iter()
                    .map(|key| all.iter().position(|row| row == key).unwrap())
                    .collect(),
            })
            .collect(),
        ..Board::default()
    }
}

/// A metadata read that started now.
fn read(board: Board) -> SourceSnapshot<Board> {
    // Reads and writes are ordered by their stamps; keep them apart.
    std::thread::sleep(std::time::Duration::from_millis(2));
    let snapshot =
        SourceSnapshot::test_live(Some(board), SourceState::Ready, wt_core::Domain::Metadata);
    std::thread::sleep(std::time::Duration::from_millis(2));
    snapshot
}

fn drawn(model: &Model, section: &str) -> Vec<String> {
    model
        .board
        .sections
        .iter()
        .find(|drawn| drawn.key == section)
        .map(|drawn| {
            drawn
                .rows
                .iter()
                .map(|&row| model.board.rows[row].key.clone())
                .collect()
        })
        .unwrap_or_default()
}

fn sections(model: &Model) -> Vec<String> {
    model
        .board
        .sections
        .iter()
        .map(|section| section.key.clone())
        .collect()
}

/// Press J or K and send the request it makes, as the terminal does.
fn press(model: &mut Model, letter: char, id: u64) -> UiAction {
    let InputResult::Action(action) = model.input(KeyEvent::from(KeyCode::Char(letter)), 20) else {
        panic!(
            "{letter} must send a request, said {:?}",
            model.toast.as_ref().map(|(text, _)| text)
        );
    };
    let action = model.resolve(action);
    model.begin_action(id, &action);
    action
}

fn refused(model: &mut Model, letter: char) -> String {
    assert_eq!(
        model.input(KeyEvent::from(KeyCode::Char(letter)), 20),
        InputResult::Draw
    );
    model.toast.clone().expect("a refusal says why").0
}

fn accepted(model: &mut Model, id: u64) {
    model.reply(UiReply {
        operation_id: Some(id),
        outcome: Some(ReplyOutcome::Accepted(Stamp::now())),
        ..Default::default()
    });
}

/// No press was refused, and none was given up on as not shown.
fn assert_nothing_went_wrong(model: &Model) {
    assert!(
        !matches!(
            model.toast,
            Some((_, ToastTone::Failed | ToastTone::Notice))
        ),
        "{:?}",
        model.toast
    );
}

fn selected(model: &Model) -> &str {
    model.selected_row().map_or("", |row| row.key.as_str())
}

fn units_seen(action: &UiAction) -> Vec<String> {
    match action {
        UiAction::Reorder {
            step: ReorderStep::Pass { units, .. } | ReorderStep::Cross { units, .. },
            ..
        } => units.clone(),
        other => panic!("not a row move: {other:?}"),
    }
}

#[test]
fn a_burst_of_presses_moves_the_row_at_each_press_and_never_bounces() {
    let stored = board(&[("One", &["a", "b", "c", "d"]), ("Two", &["e"])]);
    let mut model = Model::default();
    model.apply(read(stored.clone()));
    model.select(model.row_position("a").unwrap());

    // Each press shows at once, and carries the order the one before left.
    let first = press(&mut model, 'J', 1);
    assert_eq!(drawn(&model, "One"), ["b", "a", "c", "d"]);
    assert_eq!(units_seen(&first), ["a", "b", "c", "d"]);
    let second = press(&mut model, 'J', 2);
    assert_eq!(drawn(&model, "One"), ["b", "c", "a", "d"]);
    assert_eq!(units_seen(&second), ["b", "a", "c", "d"]);
    assert_eq!(
        second,
        UiAction::Reorder {
            key: Some("a".into()),
            section: "One".into(),
            down: true,
            step: ReorderStep::Pass {
                unit: "a".into(),
                past: "c".into(),
                units: vec!["b".into(), "a".into(), "c".into(), "d".into()],
            },
        }
    );
    let third = press(&mut model, 'J', 3);
    assert_eq!(drawn(&model, "One"), ["b", "c", "d", "a"]);
    assert_eq!(units_seen(&third), ["b", "c", "a", "d"]);
    assert_eq!(selected(&model), "a");

    // A read from before any of it lands between presses and answers.
    model.apply(read(stored.clone()));
    assert_eq!(drawn(&model, "One"), ["b", "c", "d", "a"]);

    // The host is slow and its answers arrive out of order. A read that
    // started before an answer cannot settle that press, even though it
    // already shows it.
    let early = read(board(&[("One", &["b", "a", "c", "d"]), ("Two", &["e"])]));
    accepted(&mut model, 3);
    accepted(&mut model, 1);
    model.apply(early);
    assert_eq!(model.pending.len(), 3);
    assert_eq!(drawn(&model, "One"), ["b", "c", "d", "a"]);
    // The host has stored the first press and this read shows it: that
    // press settles, the two after it still show.
    model.apply(read(board(&[
        ("One", &["b", "a", "c", "d"]),
        ("Two", &["e"]),
    ])));
    assert_eq!(model.pending.len(), 2);
    assert_eq!(drawn(&model, "One"), ["b", "c", "d", "a"]);
    accepted(&mut model, 2);
    model.apply(read(board(&[
        ("One", &["b", "c", "d", "a"]),
        ("Two", &["e"]),
    ])));
    assert!(model.pending.is_empty());
    assert_eq!(drawn(&model, "One"), ["b", "c", "d", "a"]);
    assert_eq!(selected(&model), "a");
    assert_nothing_went_wrong(&model);
}

#[test]
fn a_refused_press_shows_the_stored_order_again() {
    let stored = board(&[("One", &["a", "b", "c", "d"])]);
    let mut model = Model::default();
    model.apply(read(stored));
    model.select(model.row_position("a").unwrap());
    press(&mut model, 'J', 1);
    press(&mut model, 'J', 2);
    assert_eq!(drawn(&model, "One"), ["b", "c", "a", "d"]);
    // Another process reordered the section first: the host refuses both
    // presses, the second because it was resolved on top of the first.
    for id in [1, 2] {
        model.reply(UiReply {
            operation_id: Some(id),
            ..UiReply::stale("Changed since you looked: a and b have changed places")
        });
    }
    assert!(model.pending.is_empty());
    assert_eq!(drawn(&model, "One"), ["a", "b", "c", "d"]);
    assert_eq!(
        model.toast.as_ref().map(|(_, tone)| *tone),
        Some(ToastTone::Failed)
    );
    // The read the host starts after a refusal shows what is stored.
    model.apply(read(board(&[("One", &["d", "a", "b", "c"])])));
    assert_eq!(drawn(&model, "One"), ["d", "a", "b", "c"]);
    assert_eq!(selected(&model), "a");
}

#[test]
fn moving_a_row_back_replaces_the_press_it_undoes() {
    let stored = board(&[("One", &["a", "b", "c"])]);
    let mut model = Model::default();
    model.apply(read(stored.clone()));
    model.select(model.row_position("a").unwrap());
    press(&mut model, 'J', 1);
    assert_eq!(drawn(&model, "One"), ["b", "a", "c"]);
    // The other row of the pair moves back down past it: one claim about
    // the two remains, the newer one.
    model.select(model.row_position("b").unwrap());
    press(&mut model, 'J', 2);
    assert_eq!(drawn(&model, "One"), ["a", "b", "c"]);
    assert_eq!(model.pending.len(), 1);
    accepted(&mut model, 1);
    accepted(&mut model, 2);
    model.apply(read(stored));
    assert!(model.pending.is_empty());
    assert_nothing_went_wrong(&model);

    // A different pair is its own claim and settles on its own.
    press(&mut model, 'J', 3);
    model.select(model.row_position("a").unwrap());
    press(&mut model, 'J', 4);
    assert_eq!(drawn(&model, "One"), ["c", "a", "b"]);
    assert_eq!(model.pending.len(), 2);
}

#[test]
fn a_press_at_the_edge_enters_the_next_section_and_keeps_going() {
    let stored = board(&[("One", &["a", "b"]), ("Empty", &[]), ("Two", &["c", "d"])]);
    let mut model = Model::default();
    model.apply(read(stored));
    model.select(model.row_position("b").unwrap());
    let cross = press(&mut model, 'J', 1);
    assert_eq!(
        cross,
        UiAction::Reorder {
            key: Some("b".into()),
            section: "One".into(),
            down: true,
            step: ReorderStep::Cross {
                unit: "b".into(),
                rows: vec!["b".into()],
                units: vec!["a".into(), "b".into()],
                to: "Empty".into(),
                to_units: Vec::new(),
            },
        }
    );
    assert_eq!(drawn(&model, "Empty"), ["b"]);
    // Through the empty section to the top of the next, then past its first
    // row: each press is resolved in the section the last one showed.
    let UiAction::Reorder {
        step: ReorderStep::Cross {
            units, to_units, ..
        },
        ..
    } = press(&mut model, 'J', 2)
    else {
        panic!("the only row of a section leaves it");
    };
    assert_eq!(
        (units, to_units),
        (vec!["b".to_owned()], vec!["c".to_owned(), "d".to_owned()])
    );
    assert_eq!(drawn(&model, "Two"), ["b", "c", "d"]);
    let swap = press(&mut model, 'J', 3);
    assert_eq!(units_seen(&swap), ["b", "c", "d"]);
    assert_eq!(drawn(&model, "Two"), ["c", "b", "d"]);
    assert_eq!(drawn(&model, "One"), ["a"]);
    assert_eq!(selected(&model), "b");
    // One claim says where the row is filed, however often it crossed.
    assert_eq!(model.pending.len(), 2);

    // Back up: to the top of Two, then out of it to the bottom of Empty.
    press(&mut model, 'K', 4);
    assert_eq!(drawn(&model, "Two"), ["b", "c", "d"]);
    press(&mut model, 'K', 5);
    assert_eq!(drawn(&model, "Empty"), ["b"]);
    assert_eq!(drawn(&model, "Two"), ["c", "d"]);
    for id in 1..=5 {
        accepted(&mut model, id);
    }
    model.apply(read(board(&[
        ("One", &["a"]),
        ("Empty", &["b"]),
        ("Two", &["c", "d"]),
    ])));
    assert!(model.pending.is_empty());
    assert_nothing_went_wrong(&model);
    assert_eq!(selected(&model), "b");
}

#[test]
fn a_stack_moves_as_one_unit_and_a_member_filed_elsewhere_moves_alone() {
    let mut stored = board(&[("One", &["root", "child", "x"]), ("Two", &["split", "y"])]);
    for row in &mut stored.rows {
        if row.key == "child" || row.key == "split" {
            row.stack_parent = Some("root".into());
        }
    }
    let mut model = Model::default();
    model.apply(read(stored));
    // From a member, the whole unit passes its neighbour.
    model.select(model.row_position("child").unwrap());
    let action = press(&mut model, 'J', 1);
    assert_eq!(units_seen(&action), ["root", "x"]);
    assert_eq!(drawn(&model, "One"), ["x", "root", "child"]);
    assert_eq!(selected(&model), "child");
    let UiAction::Reorder {
        step: ReorderStep::Cross { unit, rows, to, .. },
        ..
    } = press(&mut model, 'J', 2)
    else {
        panic!("the unit leaves at the edge");
    };
    assert_eq!((unit.as_str(), to.as_str()), ("root", "Two"));
    assert_eq!(rows, ["root", "child"]);
    // Drawn with its root again, the split member is part of the unit.
    assert_eq!(drawn(&model, "Two"), ["root", "child", "split", "y"]);
    let action = press(&mut model, 'J', 3);
    assert_eq!(units_seen(&action), ["root", "y"]);
    assert_eq!(drawn(&model, "Two"), ["y", "root", "child", "split"]);
}

#[test]
fn a_member_whose_root_is_filed_elsewhere_is_a_unit_of_its_own() {
    let mut stored = board(&[("One", &["root"]), ("Two", &["split", "y"])]);
    for row in &mut stored.rows {
        if row.key == "split" {
            row.stack_parent = Some("root".into());
        }
    }
    let mut model = Model::default();
    model.apply(read(stored));
    model.select(model.row_position("split").unwrap());
    let action = press(&mut model, 'J', 1);
    assert_eq!(units_seen(&action), ["split", "y"]);
    assert_eq!(drawn(&model, "Two"), ["y", "split"]);
    assert_eq!(drawn(&model, "One"), ["root"]);
}

#[test]
fn status_sort_only_exchanges_neighbours_of_one_status() {
    let mut stored = board(&[("One", &["a", "b", "c"])]);
    stored.display.status_sort = true;
    stored.rows[0].work_rank = 1;
    stored.rows[1].work_rank = 1;
    stored.rows[2].work_rank = 4;
    let mut model = Model::default();
    model.apply(read(stored.clone()));
    model.select(model.row_position("b").unwrap());
    assert_eq!(refused(&mut model, 'J'), crate::STATUS_SORT_PINS);
    assert!(model.pending.is_empty());
    assert_eq!(drawn(&model, "One"), ["a", "b", "c"]);
    press(&mut model, 'K', 1);
    assert_eq!(drawn(&model, "One"), ["b", "a", "c"]);
    // Manual sort draws the stored order, so there the same press moves.
    stored.display.status_sort = false;
    let mut model = Model::default();
    model.apply(read(stored));
    model.select(model.row_position("b").unwrap());
    press(&mut model, 'J', 1);
    assert_eq!(drawn(&model, "One"), ["a", "c", "b"]);
}

#[test]
fn rows_wt_places_itself_are_not_reordered_and_nothing_enters_their_groups() {
    let stored = Board {
        sections: vec![
            BoardSection {
                key: "One".into(),
                title: "One".into(),
                rows: vec![0],
                ..Default::default()
            },
            BoardSection {
                key: REMOVING_SECTION.into(),
                title: "Removing".into(),
                rows: vec![1],
                ..Default::default()
            },
            BoardSection {
                key: crate::LANDED_SECTION.into(),
                title: crate::LANDED_TITLE.into(),
                rows: vec![2],
                ..Default::default()
            },
            BoardSection {
                key: ARCHIVED_SECTION.into(),
                title: "Archived".into(),
                rows: vec![3],
                ..Default::default()
            },
        ],
        rows: rows(&["a", "going", "landed", "old"]),
        ..Board::default()
    };
    let mut model = Model::default();
    model.apply(read(stored));
    // The last filed row has nowhere further down to go.
    model.select(model.row_position("a").unwrap());
    assert_eq!(refused(&mut model, 'J'), "already at bottom");
    assert_eq!(refused(&mut model, 'K'), "already at top");
    model.select(model.row_position("going").unwrap());
    assert_eq!(
        refused(&mut model, 'J'),
        "Rows being removed do not reorder"
    );
    model.select(model.row_position("landed").unwrap());
    assert!(refused(&mut model, 'K').starts_with("Landed is derived"));
    assert!(model.pending.is_empty());
    assert!(model.pending_selection.is_none());
}

#[test]
fn a_section_header_moves_at_the_keypress_and_carries_the_order_seen() {
    // A header is a cursor stop while its section is folded or empty.
    let folded = |mut board: Board| {
        for section in &mut board.sections {
            section.folded = true;
        }
        board
    };
    let stored = folded(board(&[("One", &["a"]), ("Two", &["b"]), ("Three", &[])]));
    let mut model = Model::default();
    model.apply(read(stored.clone()));
    model.select(model.section_position("One").unwrap());
    let first = press(&mut model, 'J', 1);
    assert_eq!(sections(&model), ["Two", "One", "Three"]);
    assert_eq!(
        first,
        UiAction::Reorder {
            key: None,
            section: "One".into(),
            down: true,
            step: ReorderStep::Section {
                past: "Two".into(),
                sections: vec!["One".into(), "Two".into(), "Three".into()],
            },
        }
    );
    model.select(model.section_position("One").unwrap());
    let UiAction::Reorder {
        step: ReorderStep::Section {
            past,
            sections: seen,
        },
        ..
    } = press(&mut model, 'J', 2)
    else {
        panic!("a header press moves the section");
    };
    assert_eq!(past, "Three");
    assert_eq!(seen, ["Two", "One", "Three"]);
    assert_eq!(sections(&model), ["Two", "Three", "One"]);
    // An older read keeps showing both; the confirming one settles them.
    model.apply(read(stored));
    assert_eq!(sections(&model), ["Two", "Three", "One"]);
    accepted(&mut model, 2);
    accepted(&mut model, 1);
    model.apply(read(folded(board(&[
        ("Two", &["b"]),
        ("Three", &[]),
        ("One", &["a"]),
    ]))));
    assert!(model.pending.is_empty());
    assert_eq!(sections(&model), ["Two", "Three", "One"]);
    assert_nothing_went_wrong(&model);
}

#[test]
fn a_row_still_being_created_is_not_a_unit_and_keeps_its_place() {
    let mut stored = board(&[("One", &["a", "b", "new"])]);
    let placeholder = stored.rows.iter().position(|row| row.key == "new").unwrap();
    stored.rows[placeholder].pending_creation = true;
    stored.sections[0].rows = vec![0, placeholder, 1];
    let mut model = Model::default();
    model.apply(read(stored));
    model.select(model.row_position("a").unwrap());
    let action = press(&mut model, 'J', 1);
    assert_eq!(units_seen(&action), ["a", "b"]);
    assert_eq!(drawn(&model, "One"), ["new", "b", "a"]);
    let pending: Vec<_> = model.pending.mutations().collect();
    assert!(matches!(
        pending[0].kind,
        MutationKind::Layout(LayoutChange::Pass { .. })
    ));
}

#[test]
fn a_remote_rows_layout_is_confirmed_by_the_controller_from_the_first_press() {
    let remote = "@remote/box/far";
    let at = |section: &str| {
        let mut sections = vec![("One", vec![]), ("Two", vec![]), ("Three", vec![])];
        for (name, rows) in &mut sections {
            if *name == section {
                rows.push(remote);
            }
        }
        let sections: Vec<(&str, &[&str])> = sections
            .iter()
            .map(|(name, rows)| (*name, rows.as_slice()))
            .collect();
        let mut board = board(&sections);
        board.rows[0].host = Some("box".into());
        board
    };
    let mut model = Model::default();
    model.apply(read(at("One")));
    model.select(model.row_position(remote).unwrap());
    press(&mut model, 'J', 1);
    accepted(&mut model, 1);
    // The second crossing replaces the first, accepted or not: both say
    // where the row is filed, and the controller stores that for every host.
    press(&mut model, 'J', 2);
    assert_eq!(model.pending.len(), 1);
    assert_eq!(drawn(&model, "Three"), [remote]);
    accepted(&mut model, 2);
    model.apply(read(at("Three")));
    assert!(model.pending.is_empty());
    assert_eq!(drawn(&model, "Three"), [remote]);
    assert_nothing_went_wrong(&model);
}
