//! Where rows sit on the board: archive, sections, folding, and the
//! attention feed's seen mark. All of it is durable wt metadata.
use wt_core::Domain;
use wt_runtime::Target;

use super::{
    ARCHIVED_SECTION, ARCHIVED_TITLE, Family, INBOX_SECTION, INBOX_TITLE, find_section, place,
    row_section, target_index, target_row,
};
use crate::Board;

#[derive(Clone, Debug, PartialEq)]
pub enum LayoutChange {
    Archive { archived: bool },
    MoveSection { section: Option<String> },
    FoldSection { section: String, folded: bool },
    RenameSection { old: String, new: String },
    AttentionSeen { at_ms: u64 },
}

impl Family for LayoutChange {
    fn field(&self) -> String {
        match self {
            Self::Archive { .. } => "archive".into(),
            Self::MoveSection { .. } => "section".into(),
            Self::FoldSection { section, .. } => format!("fold:{section}"),
            Self::RenameSection { old, .. } => format!("rename:{old}"),
            Self::AttentionSeen { .. } => "attention".into(),
        }
    }

    fn label(&self) -> String {
        match self {
            Self::Archive { archived: true } => "archiving".into(),
            Self::Archive { archived: false } => "restoring".into(),
            Self::MoveSection { section } => {
                format!("moving to {}", section.as_deref().unwrap_or(INBOX_TITLE))
            }
            Self::FoldSection { .. } => "folding".into(),
            Self::RenameSection { .. } => "renaming section".into(),
            Self::AttentionSeen { .. } => "marking seen".into(),
        }
    }

    fn domains(&self) -> &'static [Domain] {
        &[Domain::Metadata]
    }

    fn apply(&self, target: &Target, board: &mut Board) {
        match self {
            Self::Archive { archived } => {
                if let Some(index) = target_index(board, target) {
                    board.rows[index].archived = *archived;
                    if *archived {
                        place(board, index, ARCHIVED_SECTION, ARCHIVED_TITLE);
                    } else if row_section(board, index)
                        .is_some_and(|section| board.sections[section].key == ARCHIVED_SECTION)
                    {
                        place(board, index, INBOX_SECTION, INBOX_TITLE);
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
        }
    }

    fn observed(&self, target: &Target, board: &Board) -> bool {
        let row = target_row(board, target);
        match self {
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
        }
    }
}
