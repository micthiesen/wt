//! Cursor stops in render order and how the cursor follows its row, section
//! or review across a new board.

use std::sync::Arc;

use super::{ARCHIVED_SECTION, Board, BoardRow, BoardSection, INBOX_SECTION, InputResult, Model};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum VisualItem {
    Section(usize),
    Row(usize),
    ReviewHeader,
    ReviewRequest(usize),
}

impl Model {
    /// Show a new board, keeping the cursor on the row, section or review
    /// the user was on, or its nearest surviving neighbor.
    pub(super) fn present(&mut self, mut board: Arc<Board>) {
        if let Some(key) = &self.pending_selection
            && let Some(row) = board.rows.iter().position(|row| &row.key == key)
        {
            for section in &mut Arc::make_mut(&mut board).sections {
                if section.rows.contains(&row) {
                    section.folded = false;
                }
            }
        }
        let first_rows = self.board.rows.is_empty() && !board.rows.is_empty();
        let requested_selection = self.pending_selection.is_some();
        let previous_key = self.selected_row().map(|row| row.key.clone());
        let previous_review = self
            .selected_review()
            .map(|row| (row.host.clone(), row.url.clone()));
        let previous_review_index = match self.selected_item() {
            Some(VisualItem::ReviewRequest(index)) => Some(index),
            _ => None,
        };
        let on_reviews_header = matches!(self.selected_item(), Some(VisualItem::ReviewHeader));
        let previous_section = self.selected_section().map(|section| section.key.clone());
        let previous_neighbors: Vec<_> = self
            .selected_section()
            .map(|section| {
                let rows: Vec<_> = section
                    .rows
                    .iter()
                    .filter_map(|&index| self.board.rows.get(index))
                    .collect();
                let index = rows
                    .iter()
                    .position(|row| Some(&row.key) == previous_key.as_ref())
                    .unwrap_or(0);
                rows.iter()
                    .skip(index + 1)
                    .chain(rows.iter().take(index).rev())
                    .map(|row| row.key.clone())
                    .collect()
            })
            .unwrap_or_default();
        let on_header = matches!(self.selected_item(), Some(VisualItem::Section(_)));
        let previous_index = self.selected.unwrap_or_default();
        self.board = board;
        self.rebuild_items();
        let left_section = previous_key.is_some()
            && previous_section.as_deref().is_some_and(|old| {
                old != ARCHIVED_SECTION
                    && !self.board.sections.iter().any(|section| {
                        section.key == old
                            && section.rows.iter().any(|&index| {
                                self.board
                                    .rows
                                    .get(index)
                                    .is_some_and(|row| Some(&row.key) == previous_key.as_ref())
                            })
                    })
            });
        let neighbor = left_section
            .then(|| {
                previous_neighbors.iter().find_map(|key| {
                    self.row_position(key).filter(|_| {
                        self.board.sections.iter().any(|section| {
                            Some(&section.key) == previous_section.as_ref()
                                && section.rows.iter().any(|&index| {
                                    self.board
                                        .rows
                                        .get(index)
                                        .is_some_and(|row| &row.key == key)
                                })
                        })
                    })
                })
            })
            .flatten();
        self.selected = neighbor
            .or_else(|| {
                (!left_section)
                    .then_some(previous_key.as_deref())
                    .flatten()
                    .and_then(|key| self.row_position(key))
            })
            .or_else(|| {
                previous_section.as_deref().and_then(|key| {
                    (on_header
                        || self.board.sections.iter().any(|section| {
                            section.key == key
                                && section.folded
                                && section.rows.iter().any(|&index| {
                                    self.board
                                        .rows
                                        .get(index)
                                        .is_some_and(|row| Some(&row.key) == previous_key.as_ref())
                                })
                        }))
                    .then(|| self.section_position(key))
                    .flatten()
                })
            })
            .or_else(|| {
                if previous_key.is_none() && previous_section.is_none() {
                    (0..self.item_count())
                        .find(|&index| matches!(self.item(index), Some(VisualItem::Row(_))))
                } else {
                    None
                }
            })
            .or_else(|| (self.item_count() > 0).then(|| previous_index.min(self.item_count() - 1)));
        if let Some(key) = &self.pending_selection
            && let Some(index) = self.row_position(key)
        {
            self.selected = Some(index);
            self.pending_selection = None;
        }
        if first_rows
            && !requested_selection
            && previous_key.is_none()
            && previous_section
                .as_deref()
                .is_none_or(|section| section == INBOX_SECTION)
            && let Some(index) = (0..self.item_count())
                .find(|&index| matches!(self.item(index), Some(VisualItem::Row(_))))
        {
            self.selected = Some(index);
        }
        if self.selected_row().map(|row| row.key.as_str()) != previous_key.as_deref() {
            self.details_scroll = 0;
            self.reset_verification();
        }
        if !requested_selection {
            if let Some((host, url)) = previous_review {
                let review_position = |model: &Self, wanted: &dyn Fn(usize) -> bool| {
                    (0..model.item_count()).find(|&position| {
                        matches!(model.item(position), Some(VisualItem::ReviewRequest(index)) if wanted(index))
                    })
                };
                let same = review_position(self, &|index| {
                    self.board.review_requests[index].host == host
                        && self.board.review_requests[index].url == url
                });
                // A dismissed or checked-out review hands the cursor to
                // the next surviving review, else the previous one, else
                // whatever now occupies its slot.
                let survivor = || {
                    let last = self.board.review_requests.len().checked_sub(1)?;
                    let wanted = previous_review_index.unwrap_or(0).min(last);
                    review_position(self, &|index| index == wanted)
                };
                self.selected = same
                    .or_else(survivor)
                    .or_else(|| {
                        (self.item_count() > 0).then(|| previous_index.min(self.item_count() - 1))
                    })
                    .or(self.selected);
            } else if on_reviews_header && !self.board.review_requests.is_empty() {
                self.selected = (0..self.item_count()).find(|&position| {
                    matches!(
                        self.item(position),
                        Some(VisualItem::ReviewHeader | VisualItem::ReviewRequest(_))
                    )
                });
            }
        }
    }

    pub fn selected_row(&self) -> Option<&BoardRow> {
        match self.selected_item()? {
            VisualItem::Row(index) => self.board.rows.get(index),
            _ => None,
        }
    }

    pub(crate) fn creation_placement(&self) -> Option<crate::CreatePlacement> {
        let section = self.selected_section()?;
        if section.key == super::ARCHIVED_SECTION || section.key == super::REMOVING_SECTION {
            return None;
        }
        Some(crate::CreatePlacement {
            section: (section.key != super::INBOX_SECTION).then(|| section.key.clone()),
            after: self.actionable_row().map(|row| row.key.clone()),
        })
    }

    pub(crate) fn selected_section(&self) -> Option<&BoardSection> {
        match self.selected_item()? {
            VisualItem::Section(index) => self.board.sections.get(index),
            VisualItem::Row(index) => self
                .board
                .sections
                .iter()
                .find(|section| section.rows.contains(&index)),
            _ => None,
        }
    }

    pub(crate) fn item_count(&self) -> usize {
        if self.board.sections.is_empty() && self.board.review_requests.is_empty() {
            self.board.rows.len()
        } else {
            self.items.len()
        }
    }

    pub(crate) fn item(&self, position: usize) -> Option<VisualItem> {
        if self.board.sections.is_empty() && self.board.review_requests.is_empty() {
            (position < self.board.rows.len()).then_some(VisualItem::Row(position))
        } else {
            self.items.get(position).copied()
        }
    }

    pub(crate) fn selected_item(&self) -> Option<VisualItem> {
        self.selected.and_then(|index| self.item(index))
    }

    /// Cursor stops in render order. An expanded section is a divider, not a
    /// stop; a folded section collapses to one selectable header. Sections
    /// with no rows are kept for pickers but never drawn. Requested reviews
    /// sit between the active board and the archived block.
    pub(crate) fn rebuild_items(&mut self) {
        self.items.clear();
        if self.board.sections.is_empty() {
            self.items
                .extend((0..self.board.rows.len()).map(VisualItem::Row));
        }
        let (archived, active): (Vec<_>, Vec<_>) = self
            .board
            .sections
            .iter()
            .enumerate()
            .partition(|(_, section)| {
                matches!(
                    section.key.as_str(),
                    ARCHIVED_SECTION | super::REMOVING_SECTION
                )
            });
        let push_section = |items: &mut Vec<VisualItem>,
                            (index, section): (usize, &BoardSection)| {
            let rows: Vec<_> = section
                .rows
                .iter()
                .copied()
                .filter(|&row| row < self.board.rows.len())
                .collect();
            if rows.is_empty() {
                return;
            }
            if section.folded {
                items.push(VisualItem::Section(index));
            } else {
                items.extend(rows.into_iter().map(VisualItem::Row));
            }
        };
        let mut items = Vec::new();
        for entry in active {
            push_section(&mut items, entry);
        }
        if !self.board.review_requests.is_empty() {
            if self.reviews_folded {
                items.push(VisualItem::ReviewHeader);
            } else {
                items.extend((0..self.board.review_requests.len()).map(VisualItem::ReviewRequest));
            }
        }
        for entry in archived {
            push_section(&mut items, entry);
        }
        self.items.extend(items);
    }

    /// First cursor stop belonging to a section: its folded header or its
    /// first row.
    pub(super) fn section_first_position(&self, key: &str) -> Option<usize> {
        let section = self
            .board
            .sections
            .iter()
            .position(|section| section.key == key)?;
        (0..self.item_count()).find(|&position| match self.item(position) {
            Some(VisualItem::Section(index)) => index == section,
            Some(VisualItem::Row(row)) => self.board.sections[section].rows.contains(&row),
            _ => false,
        })
    }

    pub(super) fn row_position(&self, key: &str) -> Option<usize> {
        (0..self.item_count()).find(|&position| match self.item(position) {
            Some(VisualItem::Row(index)) => self.board.rows[index].key == key,
            _ => false,
        })
    }

    pub(super) fn section_position(&self, key: &str) -> Option<usize> {
        self.section_first_position(key)
    }

    pub fn keep_selection_visible(&mut self, height: usize) {
        let Some(selected) = self.selected else {
            self.offset = 0;
            return;
        };
        if height == 0 {
            self.offset = selected;
            return;
        }
        let margin = 3.min(height.saturating_sub(1) / 2);
        if selected < self.offset.saturating_add(margin) {
            self.offset = selected.saturating_sub(margin);
        } else if selected >= self.offset.saturating_add(height.saturating_sub(margin)) {
            self.offset = selected.saturating_add(margin + 1).saturating_sub(height);
        }
        self.offset = self.offset.min(self.item_count().saturating_sub(height));
    }

    pub(super) fn select(&mut self, index: usize) -> InputResult {
        self.pending_selection = None;
        let selected = (self.item_count() > 0).then(|| index.min(self.item_count() - 1));
        let freed = std::mem::take(&mut self.list_free_scroll);
        if self.selected == selected {
            return if freed {
                InputResult::Draw
            } else {
                InputResult::Unchanged
            };
        }
        self.selected = selected;
        self.details_scroll = 0;
        self.reset_verification();
        InputResult::Draw
    }

    /// `V` is per row: it starts open once the post-merge check is due and
    /// resets whenever the cursor lands on another row.
    pub(super) fn reset_verification(&mut self) {
        self.show_verification = self
            .selected_row()
            .and_then(|row| row.work.as_ref())
            .is_some_and(|work| work.verification_owed);
    }

    /// Move the cursor one stop; at an edge, scroll the list to that edge
    /// instead (TS `normal-keys.ts:545-570`).
    pub(super) fn step(&mut self, forward: bool) -> InputResult {
        let Some(current) = self.selected else {
            return self.select(0);
        };
        let count = self.item_count();
        let at_edge = if forward {
            count == 0 || current + 1 >= count
        } else {
            current == 0
        };
        if at_edge {
            self.pending_selection = None;
            self.list_free_scroll = true;
            self.offset = if forward { usize::MAX } else { 0 };
            return InputResult::Draw;
        }
        self.select(if forward { current + 1 } else { current - 1 })
    }

    pub(super) fn jump_section(&mut self, forward: bool) -> InputResult {
        let Some(current) = self.selected_section().and_then(|section| {
            self.board
                .sections
                .iter()
                .position(|candidate| candidate.key == section.key)
        }) else {
            return InputResult::Unchanged;
        };
        let mut candidates: Box<dyn Iterator<Item = usize>> = if forward {
            Box::new(current + 1..self.board.sections.len())
        } else {
            Box::new((0..current).rev())
        };
        let Some(position) = candidates
            .find_map(|index| self.section_first_position(&self.board.sections[index].key))
        else {
            return InputResult::Unchanged;
        };
        self.select(position)
    }
}
