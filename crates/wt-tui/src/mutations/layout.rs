//! Where rows sit on the board: archive, sections, folding, and the
//! attention feed's seen mark. All of it is durable wt metadata.
use serde::{Deserialize, Serialize};
use wt_core::Domain;
use wt_runtime::Target;

use super::{
    ARCHIVED_SECTION, ARCHIVED_TITLE, Family, INBOX_SECTION, INBOX_TITLE, find_section, place,
    row_section, target_index, target_row,
};
use crate::model::REMOVING_SECTION;
use crate::{Board, BoardSection};

#[derive(Clone, Debug, PartialEq)]
pub enum LayoutChange {
    Archive {
        archived: bool,
    },
    MoveSection {
        section: Option<String>,
    },
    FoldSection {
        section: String,
        folded: bool,
    },
    RenameSection {
        old: String,
        new: String,
    },
    AttentionSeen {
        at_ms: u64,
    },
    Creation {
        placement: crate::CreatePlacement,
    },
    /// J or K inside a section: `unit` is drawn on the far side of `past`.
    Pass {
        section: String,
        unit: String,
        past: String,
        down: bool,
    },
    /// J or K at a section's edge: `rows`, the rows of `unit`, are drawn
    /// in `to`, entering at its top when `top`.
    Cross {
        unit: String,
        rows: Vec<String>,
        to: String,
        top: bool,
    },
    /// J or K on a section header: `section` is drawn on the far side of
    /// `past`.
    SectionPass {
        section: String,
        past: String,
        down: bool,
    },
}

impl LayoutChange {
    /// What a resolved J or K press shows before the host has stored it.
    pub(super) fn reorder(section: &str, down: bool, step: &ReorderStep) -> Self {
        match step {
            ReorderStep::Pass { unit, past, .. } => Self::Pass {
                section: section.to_owned(),
                unit: unit.clone(),
                past: past.clone(),
                down,
            },
            ReorderStep::Cross { unit, rows, to, .. } => Self::Cross {
                unit: unit.clone(),
                rows: rows.clone(),
                to: to.clone(),
                top: down,
            },
            ReorderStep::Section { past, .. } => Self::SectionPass {
                section: section.to_owned(),
                past: past.clone(),
                down,
            },
        }
    }
}

/// The two names of a pair in one order, whichever of them moved.
fn pair<'a>(a: &'a str, b: &'a str) -> (&'a str, &'a str) {
    if a <= b { (a, b) } else { (b, a) }
}

impl Family for LayoutChange {
    fn field(&self) -> String {
        match self {
            Self::Creation { .. } => "creation-position".into(),
            Self::Archive { .. } => "archive".into(),
            Self::MoveSection { .. } => "section".into(),
            Self::FoldSection { section, .. } => format!("fold:{section}"),
            Self::RenameSection { old, .. } => format!("rename:{old}"),
            Self::AttentionSeen { .. } => "attention".into(),
            Self::Pass {
                section,
                unit,
                past,
                ..
            } => {
                let (a, b) = pair(unit, past);
                format!("order:{section:?}:{a:?}:{b:?}")
            }
            // Where the unit is filed, the field a move from the section menu
            // writes: a later crossing replaces an earlier one.
            Self::Cross { .. } => "section".into(),
            Self::SectionPass { section, past, .. } => {
                let (a, b) = pair(section, past);
                format!("sections:{a:?}:{b:?}")
            }
        }
    }

    fn pairwise(&self) -> bool {
        matches!(self, Self::Pass { .. })
    }

    fn key_row(&self) -> Option<&str> {
        match self {
            Self::Cross { unit, .. } => Some(unit),
            _ => None,
        }
    }

    fn rows(&self) -> Vec<String> {
        match self {
            Self::Pass { unit, past, .. } => vec![unit.clone(), past.clone()],
            Self::Cross { rows, .. } => rows.clone(),
            _ => Vec::new(),
        }
    }

    fn label(&self) -> String {
        match self {
            Self::Creation { .. } => "placing new worktree".into(),
            Self::Archive { archived: true } => "archiving".into(),
            Self::Archive { archived: false } => "restoring".into(),
            Self::MoveSection { section } => {
                format!("moving to {}", section.as_deref().unwrap_or(INBOX_TITLE))
            }
            Self::FoldSection { .. } => "folding".into(),
            Self::RenameSection { .. } => "renaming section".into(),
            Self::AttentionSeen { .. } => "marking seen".into(),
            Self::Pass { .. } => "reordering".into(),
            Self::Cross { to, .. } => format!(
                "moving to {}",
                if to == INBOX_SECTION { INBOX_TITLE } else { to }
            ),
            Self::SectionPass { .. } => "reordering sections".into(),
        }
    }

    fn domains(&self) -> &'static [Domain] {
        &[Domain::Metadata]
    }

    fn apply(&self, target: &Target, board: &mut Board) {
        match self {
            Self::Creation { placement } => {
                if let Some(index) = target_index(board, target) {
                    place_creation(board, index, placement);
                }
            }
            Self::Archive { archived } => {
                if let Some(index) = target_index(board, target) {
                    board.rows[index].archived = *archived;
                    if *archived {
                        place(board, index, ARCHIVED_SECTION, ARCHIVED_TITLE);
                    } else if row_section(board, index)
                        .is_some_and(|section| board.sections[section].key == ARCHIVED_SECTION)
                    {
                        // A restored row returns to the Inbox, unless it has
                        // landed and `[ui] owed_sections` draws it in the
                        // Landed block.
                        let row = &board.rows[index];
                        if board.display.owed_sections
                            && row.host.is_none()
                            && row.git.landed_on.is_some()
                            && row.owed_block.is_some()
                        {
                            place(board, index, crate::LANDED_SECTION, crate::LANDED_TITLE);
                        } else {
                            place(board, index, INBOX_SECTION, INBOX_TITLE);
                        }
                    }
                }
            }
            Self::MoveSection { section } => {
                if let Some(index) = target_index(board, target) {
                    match section {
                        None => place(board, index, INBOX_SECTION, INBOX_TITLE),
                        Some(name) => {
                            let (key, title) = match find_section(board, name) {
                                Some(found) => (
                                    board.sections[found].key.clone(),
                                    board.sections[found].title.clone(),
                                ),
                                None => (name.trim().to_owned(), name.trim().to_owned()),
                            };
                            place(board, index, &key, &title);
                        }
                    }
                }
            }
            Self::FoldSection { section, folded } => {
                if let Some(found) = board.sections.iter_mut().find(|s| &s.key == section) {
                    found.folded = *folded;
                }
            }
            Self::RenameSection { old, new } => {
                if let Some(found) = find_section(board, old) {
                    board.sections[found].title = new.clone();
                }
            }
            Self::AttentionSeen { at_ms } => {
                board.attention_seen_ms = board.attention_seen_ms.max(*at_ms);
            }
            Self::Pass {
                section,
                unit,
                past,
                down,
            } => {
                let Some(index) = section_at(board, section) else {
                    return;
                };
                let units = section_units(board, &board.sections[index]);
                let rows_of = |key: &str| units.iter().find(|unit| unit.key == key);
                let (Some(moving), Some(fixed)) = (rows_of(unit), rows_of(past)) else {
                    return;
                };
                let rows = &mut board.sections[index].rows;
                if passed(rows, &moving.rows, &fixed.rows, *down) {
                    return;
                }
                rows.retain(|row| !moving.rows.contains(row));
                let at = if *down {
                    rows.iter()
                        .rposition(|row| fixed.rows.contains(row))
                        .map_or(rows.len(), |last| last + 1)
                } else {
                    rows.iter()
                        .position(|row| fixed.rows.contains(row))
                        .unwrap_or(0)
                };
                rows.splice(at..at, moving.rows.iter().copied());
                stored_positions(board, index);
            }
            Self::Cross {
                unit,
                rows,
                to,
                top,
                ..
            } => {
                let Some(destination) = section_at(board, to) else {
                    return;
                };
                // A row already drawn there keeps the place it has, which a
                // later press may have given it. One that wt has since drawn
                // in a derived block (archived, landed) stays there.
                let entering: Vec<usize> = rows
                    .iter()
                    .filter_map(|key| board.rows.iter().position(|row| row.key == *key))
                    .filter(|&index| {
                        row_section(board, index).is_none_or(|drawn| {
                            drawn != destination && filed_in(&board.sections[drawn])
                        })
                    })
                    .collect();
                if entering.is_empty() {
                    return;
                }
                let source = row_section(board, entering[0]);
                for section in &mut board.sections {
                    section.rows.retain(|row| !entering.contains(row));
                }
                let before = section_units(board, &board.sections[destination]);
                board.sections[destination]
                    .rows
                    .extend(entering.iter().copied());
                // Drawn there, the rows are one unit with the rows of their
                // stack filed there already: a row stacked on them joins
                // them, and stacked on one of those they join its unit.
                let Some(joined) = section_units(board, &board.sections[destination])
                    .into_iter()
                    .find(|unit| unit.rows.contains(&entering[0]))
                else {
                    return;
                };
                let rank = joined.rank(board);
                let mut block = joined.rows;
                block.sort_by_key(|&row| board.rows[row].stack_index);
                // A unit at its creation position that was created after a
                // row of this unit is drawn behind all of it. Not when the
                // rows enter at the top under manual sort: that gives every
                // unit there an order, and it stays.
                if board.display.status_sort || !*top {
                    let mut following: Vec<usize> = Vec::new();
                    for unit in before.iter().filter(|unit| !block.contains(&unit.rows[0])) {
                        let created_after = board
                            .rows
                            .iter()
                            .find(|row| row.key == unit.key)
                            .and_then(|row| row.creation_placement.as_ref())
                            .and_then(|placement| placement.after.as_deref());
                        if unit.at_creation_position(board)
                            && created_after.is_some_and(|after| {
                                block
                                    .iter()
                                    .chain(&following)
                                    .any(|&row| board.rows[row].key == after)
                            })
                        {
                            following.extend(unit.rows.iter().copied());
                        }
                    }
                    block.extend(following);
                }
                let own_row = |row: &usize| board.rows[*row].key == joined.key;
                let leads = entering.iter().any(own_row);
                let drawn = &board.sections[destination].rows;
                // A unit they join stays where its own row is drawn.
                let held = drawn.iter().position(own_row).map(|at| {
                    drawn[..at]
                        .iter()
                        .filter(|row| !block.contains(row))
                        .count()
                });
                board.sections[destination]
                    .rows
                    .retain(|row| !block.contains(row));
                let at = match held {
                    Some(held) if !leads => held,
                    _ => entry_point(board, &board.sections[destination], rank, *top),
                };
                board.sections[destination].rows.splice(at..at, block);
                // The unit is given a stored order where it enters, and the
                // section it left is written as a reorder writes one. So is
                // the one it enters at the top under manual sort.
                if let Some(row) = board.rows.iter_mut().find(|row| row.key == *unit) {
                    row.creation_placement = None;
                }
                if let Some(source) = source {
                    stored_positions(board, source);
                }
                if *top || board.display.status_sort {
                    stored_positions(board, destination);
                }
            }
            Self::SectionPass {
                section,
                past,
                down,
            } => {
                let (Some(moving), Some(fixed)) =
                    (section_at(board, section), section_at(board, past))
                else {
                    return;
                };
                if (moving > fixed) == *down {
                    return;
                }
                let moved = board.sections.remove(moving);
                // Removing `moving` shifted `past` by one when it was behind
                // it, so the far side of `past` is its old index either way.
                board.sections.insert(fixed, moved);
            }
        }
    }

    fn observed(&self, target: &Target, board: &Board) -> bool {
        let row = target_row(board, target);
        match self {
            // The host filed the row before stamping the accept, so any
            // read that started after it is the truth, including a later
            // move. Comparing positions would hold a moved row here until
            // the deadline.
            Self::Creation { .. } => true,
            Self::Archive { archived } => row.is_none_or(|row| row.archived == *archived),
            Self::MoveSection { section } => row.is_none_or(|_| {
                target_index(board, target)
                    .and_then(|index| row_section(board, index))
                    .is_some_and(|found| {
                        let found = &board.sections[found];
                        match section {
                            None => found.key == INBOX_SECTION,
                            Some(name) => {
                                found.key.eq_ignore_ascii_case(name.trim())
                                    || found.title.eq_ignore_ascii_case(name.trim())
                            }
                        }
                    })
            }),
            Self::FoldSection { section, folded } => board
                .sections
                .iter()
                .find(|s| &s.key == section)
                .is_none_or(|s| s.folded == *folded),
            Self::RenameSection { new, .. } => find_section(board, new).is_some(),
            Self::AttentionSeen { at_ms } => board.attention_seen_ms >= *at_ms,
            // A unit or a section that is no longer drawn here has nothing
            // left to show.
            Self::Pass {
                section,
                unit,
                past,
                down,
            } => section_at(board, section).is_none_or(|index| {
                let drawn = &board.sections[index];
                let units = section_units(board, drawn);
                let rows_of = |key: &str| units.iter().find(|unit| unit.key == key);
                match (rows_of(unit), rows_of(past)) {
                    (Some(moving), Some(fixed)) => {
                        passed(&drawn.rows, &moving.rows, &fixed.rows, *down)
                    }
                    _ => true,
                }
            }),
            Self::Cross { rows, to, .. } => section_at(board, to).is_none_or(|destination| {
                rows.iter()
                    .filter_map(|key| board.rows.iter().position(|row| row.key == *key))
                    .all(|index| board.sections[destination].rows.contains(&index))
            }),
            Self::SectionPass {
                section,
                past,
                down,
            } => match (section_at(board, section), section_at(board, past)) {
                (Some(moving), Some(fixed)) => (moving > fixed) == *down,
                _ => true,
            },
        }
    }
}

/// What one J or K press does, resolved against the board the person saw.
/// It is the request and its witness: the host moves exactly the units named
/// here, and refuses when the stored layout is no longer the one listed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReorderStep {
    /// Pass `past`, the next unit drawn in the section. `units` are the
    /// section's units in the order drawn before the move.
    Pass {
        unit: String,
        past: String,
        units: Vec<String>,
    },
    /// Leave the section at its edge for `to`, the next section drawn.
    /// `rows` are the unit's rows; `units` are the units drawn in the
    /// section it leaves and `to_units` those drawn in `to`.
    Cross {
        unit: String,
        rows: Vec<String>,
        units: Vec<String>,
        to: String,
        to_units: Vec<String>,
    },
    /// Move the section itself past `past`. `sections` are the sections
    /// that can move, in the order drawn.
    Section { past: String, sections: Vec<String> },
}

/// What a section sorts and moves as one: a row, or the rows of a stack
/// drawn together in it. `key` is the row that carries the unit's stored
/// order; `rows` index the board's rows in the order drawn.
struct SectionUnit {
    key: String,
    rows: Vec<usize>,
}

impl SectionUnit {
    /// The status the unit sorts by under status sort: the first of its
    /// rows'.
    fn rank(&self, board: &Board) -> u8 {
        self.rows
            .iter()
            .map(|&row| board.rows[row].work_rank)
            .min()
            .unwrap_or(u8::MAX)
    }

    /// Whether the unit is still drawn behind the unit it was created
    /// after: it has no stored order and, under status sort, none of its
    /// rows has a status, which that sort would draw it by instead.
    fn at_creation_position(&self, board: &Board) -> bool {
        board
            .rows
            .iter()
            .any(|row| row.key == self.key && row.creation_placement.is_some())
            && !(board.display.status_sort
                && self.rows.iter().any(|&row| {
                    board.rows[row]
                        .work
                        .as_ref()
                        .is_some_and(|work| work.record.is_some())
                }))
    }
}

/// Show what the store does to the units at their creation position when a
/// press writes a section, so that the next press is resolved from the
/// board the read after it will bring. Manual sort gives every unit an
/// order, and none is at its creation position after. Status sort leaves
/// them there and restates which unit each follows.
fn stored_positions(board: &mut Board, section: usize) {
    let units = section_units(board, &board.sections[section]);
    let mut before: Option<&str> = None;
    for unit in &units {
        let follows = unit.at_creation_position(board);
        if let Some(row) = board.rows.iter_mut().find(|row| row.key == unit.key) {
            if !board.display.status_sort {
                row.creation_placement = None;
            } else if let Some(placement) = row.creation_placement.as_mut().filter(|_| follows) {
                placement.after = before.map(str::to_owned);
            }
        }
        before = Some(&unit.key);
    }
}

/// Where a unit of status `rank` that enters `section` at its edge is
/// drawn, as an index into the section's rows. Manual sort draws by stored
/// order alone and the unit is given the first or the last. Status sort
/// draws by status before that, so the unit is first or last among the
/// units of its own status; a unit still at its creation position stays
/// behind the unit it follows.
fn entry_point(board: &Board, section: &BoardSection, rank: u8, top: bool) -> usize {
    let end = section.rows.len();
    if !board.display.status_sort {
        return if top { 0 } else { end };
    }
    let units = section_units(board, section);
    let placed = |unit: &SectionUnit| !unit.at_creation_position(board);
    let starts = |unit: &SectionUnit| {
        section
            .rows
            .iter()
            .position(|row| unit.rows.contains(row))
            .unwrap_or(end)
    };
    if top {
        return units
            .iter()
            .find(|unit| placed(unit) && unit.rank(board) >= rank)
            .map_or(end, starts);
    }
    match units
        .iter()
        .rposition(|unit| placed(unit) && unit.rank(board) <= rank)
    {
        // Behind the last such unit and whatever follows it from creation.
        Some(last) => units[last + 1..]
            .iter()
            .find(|unit| placed(unit))
            .map_or(end, starts),
        None => units.iter().find(|unit| placed(unit)).map_or(end, starts),
    }
}

/// The units of a section in the order drawn. A stacked row belongs to the
/// unit of the shallowest row of its stack drawn in the same section, so a
/// member filed elsewhere is a unit of its own, here and there. A row still
/// being created is not one: its host has not filed it yet.
fn section_units(board: &Board, section: &BoardSection) -> Vec<SectionUnit> {
    let drawn: Vec<(usize, &crate::BoardRow)> = section
        .rows
        .iter()
        .filter_map(|&index| Some((index, board.rows.get(index)?)))
        .filter(|(_, row)| !row.pending_creation)
        .collect();
    let row_of = |key: &str| drawn.iter().map(|(_, row)| *row).find(|row| row.key == key);
    let mut units: Vec<SectionUnit> = Vec::new();
    for &(index, row) in &drawn {
        let mut anchor = row;
        // Bounded by the rows drawn, so a malformed cycle of parents ends.
        for _ in 0..drawn.len() {
            match anchor.stack_parent.as_deref().and_then(row_of) {
                Some(parent) => anchor = parent,
                None => break,
            }
        }
        match units.iter_mut().find(|unit| unit.key == anchor.key) {
            Some(unit) => unit.rows.push(index),
            None => units.push(SectionUnit {
                key: anchor.key.clone(),
                rows: vec![index],
            }),
        }
    }
    units
}

/// Whether the board draws a unit behind the unit it was created after
/// instead of by its stored order and, under status sort, its status.
pub fn unit_at_creation_position(board: &Board, unit: &str) -> bool {
    let Some(index) = board.rows.iter().position(|row| row.key == unit) else {
        return false;
    };
    board
        .sections
        .iter()
        .find(|section| section.rows.contains(&index))
        .and_then(|section| {
            section_units(board, section)
                .into_iter()
                .find(|drawn| drawn.key == unit)
        })
        .is_some_and(|drawn| drawn.at_creation_position(board))
}

/// The status a unit sorts by under status sort, where `board` draws it.
/// `None` for a row the board does not list.
pub fn unit_rank(board: &Board, unit: &str) -> Option<u8> {
    let index = board.rows.iter().position(|row| row.key == unit)?;
    let rank = board
        .sections
        .iter()
        .find(|section| section.rows.contains(&index))
        .and_then(|section| {
            section_units(board, section)
                .into_iter()
                .find(|drawn| drawn.key == unit)
        })
        .map_or(board.rows[index].work_rank, |drawn| drawn.rank(board));
    Some(rank)
}

/// Why nothing in a section, nor the section itself, moves by hand: wt
/// derives it, so no order or placement is stored for it. `None` for a
/// section rows are filed in. `title` names a derived block in its refusal.
pub fn reorder_refusal(key: &str, title: &str) -> Option<String> {
    if key == ARCHIVED_SECTION {
        Some("Archived rows do not reorder; use a to restore".into())
    } else if key == REMOVING_SECTION {
        Some("Rows being removed do not reorder".into())
    } else if key.starts_with(crate::OWED_BLOCK_PREFIX) {
        // wt orders these blocks and the rows in them.
        Some(crate::owed_block_refusal(title))
    } else if key.starts_with('\0') && key != INBOX_SECTION {
        Some("This group is named by wt and holds no filed rows".into())
    } else {
        None
    }
}

/// Resolve a J or K press on `key` (the section header when `None`) against
/// the board on screen. `Err` is the notice for a press that moves nothing.
pub fn plan_reorder(
    board: &Board,
    key: Option<&str>,
    section: &str,
    down: bool,
) -> Result<ReorderStep, String> {
    let index = section_at(board, section).ok_or("section disappeared")?;
    let here = &board.sections[index];
    if let Some(refusal) = reorder_refusal(&here.key, &here.title) {
        return Err(refusal);
    }
    // A row or a section only moves next to a section on screen that rows
    // are filed in: a named one even while empty, the Inbox only with rows.
    let on_screen = |section: &&BoardSection| filed_in(section) && section.drawn();
    let next = if down {
        board.sections[index + 1..].iter().find(on_screen)
    } else {
        board.sections[..index].iter().rev().find(on_screen)
    };
    let edge = if down { "bottom" } else { "top" };
    let Some(key) = key else {
        let past = next.ok_or_else(|| format!("section already at {edge}"))?;
        return Ok(ReorderStep::Section {
            past: past.key.clone(),
            sections: board
                .sections
                .iter()
                .filter(|section| on_screen(section) || section.key == here.key)
                .map(|section| section.key.clone())
                .collect(),
        });
    };
    let units = section_units(board, here);
    let keys = |units: &[SectionUnit]| -> Vec<String> {
        units.iter().map(|unit| unit.key.clone()).collect()
    };
    let at = units
        .iter()
        .position(|unit| unit.rows.iter().any(|&row| board.rows[row].key == key))
        .ok_or("selected row left this section")?;
    let unit = &units[at];
    let neighbor = if down {
        units.get(at + 1)
    } else {
        at.checked_sub(1).and_then(|before| units.get(before))
    };
    if let Some(past) = neighbor {
        // Status sort draws a section by status first, so the stored order
        // only decides among units of one status.
        if board.display.status_sort && unit.rank(board) != past.rank(board) {
            return Err(STATUS_SORT_PINS.into());
        }
        return Ok(ReorderStep::Pass {
            unit: unit.key.clone(),
            past: past.key.clone(),
            units: keys(&units),
        });
    }
    let to = next.ok_or_else(|| format!("already at {edge}"))?;
    Ok(ReorderStep::Cross {
        unit: unit.key.clone(),
        rows: unit
            .rows
            .iter()
            .map(|&row| board.rows[row].key.clone())
            .collect(),
        units: keys(&units),
        to: to.key.clone(),
        to_units: keys(&section_units(board, to)),
    })
}

/// Why status sort refuses to exchange two neighbours.
pub const STATUS_SORT_PINS: &str =
    "status sort pins this row; reorder within the same status or use manual sort";

fn section_at(board: &Board, key: &str) -> Option<usize> {
    board.sections.iter().position(|section| section.key == key)
}

/// Whether rows are filed in this section, as opposed to drawn in it by wt
/// (the archive, removals, the Landed block).
fn filed_in(section: &BoardSection) -> bool {
    reorder_refusal(&section.key, &section.title).is_none()
}

/// Whether `moving` is drawn on the far side of `fixed`.
fn passed(drawn: &[usize], moving: &[usize], fixed: &[usize], down: bool) -> bool {
    let first = |rows: &[usize]| drawn.iter().position(|row| rows.contains(row));
    match (first(moving), first(fixed)) {
        (Some(moving), Some(fixed)) => (moving > fixed) == down,
        _ => true,
    }
}

/// Show the operation row at its captured position while its layout settles.
pub(super) fn place_creation(board: &mut Board, index: usize, placement: &crate::CreatePlacement) {
    board.rows[index].creation_placement = Some(placement.clone());
    let section_key = placement.section.as_deref().unwrap_or(INBOX_SECTION);
    let title = placement.section.as_deref().unwrap_or(INBOX_TITLE);
    place(board, index, section_key, title);
    let anchor_index = placement
        .after
        .as_ref()
        .and_then(|key| board.rows.iter().position(|row| row.key == *key));
    let section = board
        .sections
        .iter_mut()
        .find(|section| section.key == section_key)
        .unwrap();
    section.folded = false;
    section.rows.retain(|row| *row != index);
    let position = anchor_index
        .and_then(|anchor| section.rows.iter().position(|row| *row == anchor))
        .map_or(0, |position| position + 1);
    section.rows.insert(position, index);
}
