use std::sync::Arc;

use crate::{
    ConfirmAction, LineEditor, PickerAction, PickerOption, SessionTarget, TextAction, UiAction,
    UiModal, UiReply, UrlKind, editor::EditResult,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use wt_runtime::{SourceSnapshot, SourceState};

pub struct TitlePrompt {
    pub key: String,
    pub editor: LineEditor,
}

pub struct TextPrompt {
    pub action: TextAction,
    pub prompt: String,
    pub editor: LineEditor,
    pub allow_empty: bool,
}

pub struct PickerPrompt {
    pub action: PickerAction,
    pub title: String,
    pub options: Vec<PickerOption>,
    pub selected: usize,
}

pub struct ConfirmPrompt {
    pub action: ConfirmAction,
    pub title: String,
    pub lines: Vec<String>,
    pub selected: usize,
    pub cancel_key: Option<char>,
}

pub struct ReviewerPrompt {
    pub key: String,
    pub pr_number: u64,
    pub original: Vec<String>,
    pub candidates: Vec<crate::ReviewerOption>,
    pub selected: usize,
}

#[derive(Default)]
pub enum Interaction {
    #[default]
    None,
    Text(TextPrompt),
    Picker(PickerPrompt),
    Confirm(ConfirmPrompt),
    Log {
        title: String,
        lines: Vec<String>,
        scroll: usize,
    },
    Reviewers(ReviewerPrompt),
}

/// Prepared, immutable presentation data. Service code sanitizes terminal
/// controls and computes labels once when the underlying source changes.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Board {
    pub name: String,
    #[serde(default)]
    pub automations_paused: bool,
    #[serde(default)]
    pub full_width_activity: bool,
    pub rows: Vec<BoardRow>,
    pub activity: Vec<ActivityLine>,
    #[serde(default)]
    pub attention: Vec<AttentionLine>,
    #[serde(default)]
    pub attention_seen_ms: u64,
    #[serde(default)]
    pub slot_logs: std::collections::BTreeMap<String, Vec<LogView>>,
    #[serde(default)]
    pub removed_history: RemovedHistorySnapshot,
    #[serde(default)]
    pub review_requests: Vec<ReviewRequestRow>,
    #[serde(default)]
    pub perf: Vec<String>,
    pub sections: Vec<BoardSection>,
    pub hosts: Vec<HostChoice>,
    #[serde(default)]
    pub usage: Vec<String>,
    #[serde(default)]
    pub slot_sessions: std::collections::BTreeMap<String, Vec<SessionView>>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ActivityLine {
    pub at_ms: u64,
    pub level: String,
    pub channel: String,
    pub source: String,
    pub text: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AttentionLine {
    pub at_ms: u64,
    pub source: String,
    pub text: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionView {
    pub id: String,
    pub harness: String,
    pub name: String,
    pub state: String,
    pub live: bool,
    pub queued: u32,
    pub output: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LogView {
    pub id: String,
    pub title: String,
    pub lines: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RemovedHistorySnapshot {
    pub rows: Vec<RemovedHistoryRow>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RemovedHistoryRow {
    pub key: String,
    pub host: Option<String>,
    pub slug: String,
    pub branch: String,
    pub title: String,
    pub removed_at: String,
    pub details: Vec<String>,
    pub issue_url: Option<String>,
    pub pr_url: Option<String>,
    pub issue_status: Option<String>,
    pub production_landed: Option<bool>,
    pub automations_paused: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct HostChoice {
    pub id: Option<String>,
    pub label: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReviewRequestRow {
    pub host: Option<String>,
    pub url: String,
    pub updated_at: String,
    pub branch: String,
    pub title: String,
    pub number: u64,
    pub author: String,
    pub details: Vec<String>,
    pub issue_url: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BoardSection {
    pub key: String,
    pub title: String,
    pub folded: bool,
    /// Indices into the prepared board's rows, already sorted by the source.
    pub rows: Vec<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VisualItem {
    Section(usize),
    Row(usize),
    ReviewHeader,
    ReviewRequest(usize),
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BoardRow {
    pub key: String,
    pub host: Option<String>,
    pub slug: String,
    pub title: String,
    pub branch: String,
    pub base_branch: Option<String>,
    pub path: String,
    pub badge: String,
    pub details: Vec<String>,
    pub needs_attention: bool,
    #[serde(default)]
    pub work_rank: u8,
    #[serde(default)]
    pub verify_steps: Option<String>,
    pub issue_id: Option<String>,
    pub issue_status: Option<String>,
    pub issue_url: Option<String>,
    pub github_issue_url: Option<String>,
    pub pr_url: Option<String>,
    pub stage_url: Option<String>,
    pub dev_url: Option<String>,
    pub archived: bool,
    pub stack_prefix: String,
    #[serde(default)]
    pub sessions: Vec<SessionView>,
    #[serde(default)]
    pub logs: Vec<LogView>,
}

pub struct Model {
    pub board: Arc<Board>,
    pub source_state: SourceState,
    pub selected: Option<usize>,
    pub offset: usize,
    pub details_scroll: u16,
    pub(crate) output: crate::output::OutputState,
    pub(crate) ui_generation: u64,
    pub(crate) pr_chord: Option<(std::time::Instant, String, bool)>,
    pub help: bool,
    pub(crate) help_scroll: usize,
    pub(crate) help_query: crate::LineEditor,
    pub(crate) help_searching: bool,
    pub(crate) show_verification: bool,
    pub show_perf: bool,
    pub(crate) perf_continuous: bool,
    pub(crate) perf_scroll: usize,
    pub(crate) history: crate::history::HistoryView,
    pub(crate) reviews_folded: bool,
    pub frame_count: u64,
    pub last_frame_micros: u128,
    pub title_prompt: Option<TitlePrompt>,
    pub interaction: Interaction,
    pub(crate) interaction_host: Option<String>,
    pub yank: Option<usize>,
    pub toast: Option<(String, bool)>,
    pub pending_selection: Option<String>,
    pub(crate) last_section_target: Option<Option<String>>,
    pub(crate) items: Vec<VisualItem>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum InputResult {
    Unchanged,
    Draw,
    Refresh,
    Quit,
    Action(UiAction),
}

impl Default for Model {
    fn default() -> Self {
        Self {
            board: Arc::new(Board::default()),
            source_state: SourceState::Empty,
            selected: None,
            offset: 0,
            details_scroll: 0,
            output: Default::default(),
            ui_generation: 0,
            pr_chord: None,
            help: false,
            help_scroll: 0,
            help_query: crate::LineEditor::default(),
            help_searching: false,
            show_verification: false,
            show_perf: false,
            perf_continuous: false,
            perf_scroll: 0,
            history: Default::default(),
            reviews_folded: false,
            frame_count: 0,
            last_frame_micros: 0,
            title_prompt: None,
            interaction: Interaction::None,
            interaction_host: None,
            yank: None,
            toast: None,
            pending_selection: None,
            last_section_target: None,
            items: Vec::new(),
        }
    }
}

impl Model {
    pub fn apply(&mut self, snapshot: SourceSnapshot<Board>) {
        self.source_state = snapshot.state;
        if let Some(board) = snapshot.data {
            let first_rows = self.board.rows.is_empty() && !board.rows.is_empty();
            let requested_selection = self.pending_selection.is_some();
            let previous_key = self.selected_row().map(|row| row.key.clone());
            let previous_review = self
                .selected_review()
                .map(|row| (row.host.clone(), row.url.clone()));
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
                    old != "\0archived"
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
                                        self.board.rows.get(index).is_some_and(|row| {
                                            Some(&row.key) == previous_key.as_ref()
                                        })
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
                .or_else(|| {
                    (self.item_count() > 0).then(|| previous_index.min(self.item_count() - 1))
                });
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
                    .is_none_or(|section| section == "\0inbox")
                && let Some(index) = (0..self.item_count())
                    .find(|&index| matches!(self.item(index), Some(VisualItem::Row(_))))
            {
                self.selected = Some(index);
            }
            if self.selected_row().map(|row| row.key.as_str()) != previous_key.as_deref() {
                self.details_scroll = 0;
            }
            if !requested_selection {
                if let Some((host, url)) = previous_review {
                    if let Some(position) = (0..self.item_count()).find(|&position| {
                        matches!(self.item(position), Some(VisualItem::ReviewRequest(index))
                            if self.board.review_requests[index].host == host && self.board.review_requests[index].url == url)
                    }) {
                        self.selected = Some(position);
                    }
                } else if on_reviews_header && !self.board.review_requests.is_empty() {
                    self.selected = Some(0);
                }
            }
        }
    }

    pub fn reply(&mut self, reply: UiReply) -> Option<crate::TerminalHandoff> {
        let current = reply
            .ui_generation
            .is_none_or(|generation| generation == self.ui_generation);
        if reply.select_when_visible.is_some() && current {
            self.history.active = false;
        }
        if reply.modal.is_some() && current {
            self.interaction_host = reply.modal_host;
        }
        self.toast = Some((reply.message, reply.failed));
        self.interaction = match reply.modal.filter(|_| current) {
            Some(UiModal::Reviewers {
                key,
                pr_number,
                original,
                candidates,
            }) => Interaction::Reviewers(ReviewerPrompt {
                key,
                pr_number,
                original,
                candidates,
                selected: 0,
            }),
            Some(UiModal::Log { title, lines }) => Interaction::Log {
                title,
                lines,
                scroll: 0,
            },
            Some(UiModal::Confirm {
                action,
                title,
                lines,
                cancel_key,
            }) => Interaction::Confirm(ConfirmPrompt {
                action,
                title,
                lines,
                selected: 0,
                cancel_key,
            }),
            Some(UiModal::Picker {
                action,
                title,
                options,
                selected,
            }) => {
                let selected = if matches!(action, PickerAction::Section { .. }) {
                    self.last_section_target
                        .as_ref()
                        .and_then(|target| {
                            options.iter().position(|option| &option.value == target)
                        })
                        .unwrap_or(selected)
                } else {
                    selected
                }
                .min(options.len().saturating_sub(1));
                Interaction::Picker(PickerPrompt {
                    action,
                    title,
                    options,
                    selected,
                })
            }
            Some(UiModal::Text {
                action,
                prompt,
                initial,
                allow_empty,
            }) => Interaction::Text(TextPrompt {
                action,
                prompt,
                editor: LineEditor::new(&initial),
                allow_empty,
            }),
            None => std::mem::take(&mut self.interaction),
        };
        if current && let Some(key) = reply.select_when_visible {
            if let Some(index) = self.row_position(&key) {
                self.select(index);
            } else {
                self.pending_selection = Some(key);
            }
        }
        reply.handoff
    }

    pub(crate) fn paste(&mut self, text: &str) -> bool {
        if let Interaction::Text(prompt) = &mut self.interaction {
            prompt.editor.paste(text);
            return true;
        }
        if let Some(prompt) = &mut self.title_prompt {
            prompt.editor.paste(text);
            true
        } else {
            false
        }
    }

    pub(crate) fn yank_choices(&self) -> Vec<(char, &'static str, String)> {
        if matches!(self.selected_item(), Some(VisualItem::Section(_)))
            && let Some(section) = self.selected_section()
        {
            let rows = section
                .rows
                .iter()
                .filter_map(|&index| self.board.rows.get(index))
                .collect::<Vec<_>>();
            return vec![
                ('n', "section", section.title.clone()),
                (
                    's',
                    "slugs",
                    rows.iter()
                        .map(|row| row.slug.as_str())
                        .collect::<Vec<_>>()
                        .join(" "),
                ),
                (
                    'b',
                    "branches",
                    rows.iter()
                        .map(|row| row.branch.as_str())
                        .collect::<Vec<_>>()
                        .join(" "),
                ),
                (
                    'l',
                    "list",
                    format!(
                        "{}\n{}",
                        section.title,
                        rows.iter()
                            .map(|row| format!("- {}: {}", row.slug, row.title))
                            .collect::<Vec<_>>()
                            .join("\n")
                    ),
                ),
            ];
        }
        self.selected_row()
            .map(|row| {
                let mut choices = vec![
                    ('b', "branch", row.branch.clone()),
                    ('p', "path", row.path.clone()),
                    ('n', "slug", row.slug.clone()),
                ];
                for (key, label, value) in [
                    ('S', "stage URL", row.stage_url.as_ref()),
                    ('d', "dev URL", row.dev_url.as_ref()),
                    (
                        'i',
                        "issue",
                        row.issue_url.as_ref().or(row.github_issue_url.as_ref()),
                    ),
                    ('I', "primary issue", row.issue_url.as_ref()),
                    ('r', "PR URL", row.pr_url.as_ref()),
                ] {
                    if let Some(value) = value {
                        choices.push((key, label, value.clone()));
                    }
                }
                choices
            })
            .unwrap_or_default()
    }

    pub fn selected_row(&self) -> Option<&BoardRow> {
        match self.selected_item()? {
            VisualItem::Row(index) => self.board.rows.get(index),
            _ => None,
        }
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

    pub(crate) fn rebuild_items(&mut self) {
        self.items.clear();
        if !self.board.review_requests.is_empty() {
            self.items.push(VisualItem::ReviewHeader);
            if !self.reviews_folded {
                self.items
                    .extend((0..self.board.review_requests.len()).map(VisualItem::ReviewRequest));
            }
        }
        if self.board.sections.is_empty() {
            self.items
                .extend((0..self.board.rows.len()).map(VisualItem::Row));
        }
        for (index, section) in self.board.sections.iter().enumerate() {
            self.items.push(VisualItem::Section(index));
            if !section.folded {
                self.items.extend(
                    section
                        .rows
                        .iter()
                        .copied()
                        .filter(|&row| row < self.board.rows.len())
                        .map(VisualItem::Row),
                );
            }
        }
    }

    fn row_position(&self, key: &str) -> Option<usize> {
        (0..self.item_count()).find(|&position| match self.item(position) {
            Some(VisualItem::Row(index)) => self.board.rows[index].key == key,
            _ => false,
        })
    }

    fn section_position(&self, key: &str) -> Option<usize> {
        (0..self.item_count()).find(|&position| match self.item(position) {
            Some(VisualItem::Section(index)) => self.board.sections[index].key == key,
            _ => false,
        })
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

    fn select(&mut self, index: usize) -> InputResult {
        self.pending_selection = None;
        let selected = (self.item_count() > 0).then(|| index.min(self.item_count() - 1));
        if self.selected == selected {
            return InputResult::Unchanged;
        }
        self.selected = selected;
        self.details_scroll = 0;
        InputResult::Draw
    }

    fn jump_section(&mut self, forward: bool) -> InputResult {
        let Some(current) = self.selected_section().and_then(|section| {
            self.board
                .sections
                .iter()
                .position(|candidate| candidate.key == section.key)
        }) else {
            return InputResult::Unchanged;
        };
        let next = if forward {
            current.checked_add(1)
        } else {
            current.checked_sub(1)
        };
        let Some(section) = next.and_then(|index| self.board.sections.get(index)) else {
            return InputResult::Unchanged;
        };
        let Some(position) = self.section_position(&section.key) else {
            return InputResult::Unchanged;
        };
        self.select(position + usize::from(!section.folded && !section.rows.is_empty()))
    }

    pub(crate) fn input(&mut self, key: KeyEvent, height: usize) -> InputResult {
        self.ui_generation = self.ui_generation.wrapping_add(1);
        let chord_allowed = matches!(
            self.interaction,
            Interaction::None
                | Interaction::Picker(PickerPrompt {
                    action: PickerAction::Section { .. },
                    ..
                })
        ) && self.title_prompt.is_none()
            && !self.history.active
            && !self.help
            && !self.show_perf;
        if chord_allowed
            && key.modifiers.is_empty()
            && key.code == KeyCode::Char('p')
            && let Some((started, url, linear)) = self.pr_chord.take()
            && started.elapsed() <= std::time::Duration::from_millis(1200)
        {
            self.interaction = Interaction::None;
            self.interaction_host = None;
            return InputResult::Action(UiAction::OpenPrLink { url, linear });
        }
        if chord_allowed
            && matches!(self.interaction, Interaction::None)
            && key.modifiers.is_empty()
            && matches!(key.code, KeyCode::Char('g' | 'l'))
        {
            self.pr_chord = self
                .selected_row()
                .and_then(|row| row.pr_url.clone())
                .map(|url| {
                    (
                        std::time::Instant::now(),
                        url,
                        key.code == KeyCode::Char('l'),
                    )
                });
        }
        if !matches!(self.interaction, Interaction::None) {
            let interaction = std::mem::take(&mut self.interaction);
            let host = self.interaction_host.clone();
            let result = self.interaction_input(key, interaction);
            if matches!(self.interaction, Interaction::None) {
                self.interaction_host = None;
            }
            return match (host, result) {
                (Some(host), InputResult::Action(action)) => {
                    InputResult::Action(UiAction::OnHost {
                        host: Some(host),
                        action: Box::new(action),
                    })
                }
                (_, result) => result,
            };
        }
        if let Some(prompt) = &mut self.title_prompt {
            return match prompt.editor.input(key) {
                EditResult::Cancel => {
                    self.title_prompt = None;
                    InputResult::Draw
                }
                EditResult::Submit => {
                    let title = prompt.editor.text();
                    if title.trim().is_empty() {
                        return InputResult::Unchanged;
                    }
                    let action = UiAction::SetTitle {
                        key: prompt.key.clone(),
                        title,
                    };
                    self.title_prompt = None;
                    InputResult::Action(action)
                }
                EditResult::Changed => InputResult::Draw,
                EditResult::Unchanged => InputResult::Unchanged,
            };
        }
        // Alt letters are also encoded as Esc followed by a letter in legacy
        // terminals. Do not turn those chords into row navigation/actions.
        if key
            .modifiers
            .intersects(KeyModifiers::ALT | KeyModifiers::SUPER)
        {
            return InputResult::Unchanged;
        }
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        if let Some(selected) = self.yank {
            let choices = self.yank_choices();
            let direct = choices
                .iter()
                .position(|(letter, _, _)| key.code == KeyCode::Char(*letter));
            let pick = match key.code {
                KeyCode::Esc | KeyCode::Char('q') => {
                    self.yank = None;
                    return InputResult::Draw;
                }
                KeyCode::Char('c') if control => {
                    self.yank = None;
                    return InputResult::Draw;
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.yank = Some((selected + 1).min(choices.len().saturating_sub(1)));
                    return InputResult::Draw;
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.yank = Some(selected.saturating_sub(1));
                    return InputResult::Draw;
                }
                KeyCode::Enter | KeyCode::Char('y') => Some(selected),
                KeyCode::Char(digit @ '1'..='9') => Some(digit as usize - '1' as usize),
                _ => direct,
            };
            if let Some((_, label, value)) = pick.and_then(|index| choices.get(index)) {
                let action = UiAction::Copy {
                    value: value.clone(),
                    label: (*label).to_owned(),
                };
                self.yank = None;
                return InputResult::Action(action);
            }
            return InputResult::Unchanged;
        }
        if self.help {
            if self.help_searching {
                match self.help_query.input(key) {
                    EditResult::Cancel => {
                        if control && key.code == KeyCode::Char('c') {
                            self.help = false;
                            self.help_searching = false;
                        } else {
                            self.help_searching = false;
                            self.help_query = crate::LineEditor::default();
                        }
                    }
                    EditResult::Submit => self.help_searching = false,
                    EditResult::Changed => self.help_scroll = 0,
                    EditResult::Unchanged => {}
                }
                return InputResult::Draw;
            }
            if (key.code == KeyCode::Esc || (control && key.code == KeyCode::Char('c')))
                && !self.help_query.text().is_empty()
            {
                self.help_query = crate::LineEditor::default();
                self.help_scroll = 0;
                return InputResult::Draw;
            }
            if matches!(key.code, KeyCode::Esc | KeyCode::Char('q' | '?'))
                || (control && key.code == KeyCode::Char('c'))
            {
                self.help = false;
                return InputResult::Draw;
            }
            if key.code == KeyCode::Char('/') {
                self.help_searching = true;
                return InputResult::Draw;
            }
            let maximum = crate::help::filtered_lines(&self.help_query.text())
                .len()
                .saturating_sub(1);
            match key.code {
                KeyCode::Down | KeyCode::Char('j') => {
                    self.help_scroll = self.help_scroll.saturating_add(1).min(maximum);
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.help_scroll = self.help_scroll.saturating_sub(1);
                }
                KeyCode::PageDown => {
                    self.help_scroll = self.help_scroll.saturating_add(height / 2).min(maximum);
                }
                KeyCode::PageUp => {
                    self.help_scroll = self.help_scroll.saturating_sub(height / 2);
                }
                KeyCode::Home | KeyCode::Char('g') => self.help_scroll = 0,
                KeyCode::End | KeyCode::Char('G') => self.help_scroll = maximum,
                _ => return InputResult::Unchanged,
            }
            return InputResult::Draw;
        }
        if self.show_perf {
            if matches!(key.code, KeyCode::Esc | KeyCode::Char('q' | 'P'))
                || (control && key.code == KeyCode::Char('c'))
            {
                self.show_perf = false;
                return InputResult::Action(UiAction::SetPerf {
                    active: false,
                    continuous: self.perf_continuous,
                    refresh: false,
                });
            }
            if key.code == KeyCode::Char('i') {
                self.perf_continuous = !self.perf_continuous;
            }
            if matches!(key.code, KeyCode::Char('i' | 'r')) {
                return InputResult::Action(UiAction::SetPerf {
                    active: true,
                    continuous: self.perf_continuous,
                    refresh: key.code == KeyCode::Char('r'),
                });
            }
            match key.code {
                KeyCode::Down | KeyCode::Char('j') => {
                    self.perf_scroll = self
                        .perf_scroll
                        .saturating_add(3)
                        .min(self.board.perf.len().saturating_sub(1))
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.perf_scroll = self.perf_scroll.saturating_sub(3)
                }
                KeyCode::Home | KeyCode::Char('g') => self.perf_scroll = 0,
                KeyCode::End | KeyCode::Char('G') => {
                    self.perf_scroll = self.board.perf.len().saturating_sub(1)
                }
                _ => return InputResult::Unchanged,
            }
            return InputResult::Draw;
        }
        if self.history.active && !crate::history::is_global_key(key) {
            return self.history_input(key, height);
        }
        if let Some(result) = self.review_input(key) {
            return result;
        }
        match key.code {
            KeyCode::Char('r' | 'R') if control => {
                InputResult::Action(UiAction::PrepareHardRefresh)
            }
            KeyCode::Char('h') if !control => {
                self.history.active = true;
                InputResult::Action(UiAction::SetHistoryActive { active: true })
            }
            KeyCode::Char('R') if !control => self.row_action(|key| UiAction::Restack { key }),
            KeyCode::Char('e' | 'E') if !control => {
                let ship = key.code == KeyCode::Char('E');
                self.row_action(|key| UiAction::PrepareGithub { key, ship })
            }
            KeyCode::Char('f') if !control => {
                self.row_action(|key| UiAction::GithubFailedChecks { key })
            }
            KeyCode::Char('v') if !control => {
                self.row_action(|key| UiAction::PrepareReviewers { key })
            }
            KeyCode::Char('!') if !control => self.row_action(|key| UiAction::PrepareActions {
                surface: crate::ActionSurface::Row { key },
            }),
            KeyCode::Char('a' | 'A') if control && shift => {
                InputResult::Action(UiAction::CancelAutomations)
            }
            KeyCode::Char('a' | 'A') if control => {
                self.row_action(|key| UiAction::ToggleAutomations { key: Some(key) })
            }
            KeyCode::Char('A') if !control => {
                InputResult::Action(UiAction::ToggleAutomations { key: None })
            }
            KeyCode::Char('\'') if !control => {
                self.open_output_picker();
                InputResult::Draw
            }
            KeyCode::Char('"') if !control => {
                self.output.toggle_feed();
                InputResult::Draw
            }
            KeyCode::Char('x')
                if !control
                    && matches!(self.output.target, crate::output::OutputTarget::Attention)
                    && !self.board.attention.is_empty() =>
            {
                let at_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis()
                    .min(u64::MAX as u128) as u64;
                self.output.mark_seen();
                InputResult::Action(UiAction::SetAttentionSeen { at_ms })
            }
            KeyCode::Char('[' | ']') if !control => {
                self.output.cycle(key.code == KeyCode::Char(']'));
                InputResult::Draw
            }
            KeyCode::Char('e') if control => {
                self.output.scroll(false);
                InputResult::Draw
            }
            KeyCode::Char('y') if control => {
                self.output.scroll(true);
                InputResult::Draw
            }
            KeyCode::Char('j' | 'J') if control && shift => {
                self.output.scroll(false);
                InputResult::Draw
            }
            KeyCode::Char('k' | 'K') if control && shift => {
                self.output.scroll(true);
                InputResult::Draw
            }
            KeyCode::Char('T') if !control => {
                self.row_action(|key| UiAction::GenerateTitle { key })
            }
            KeyCode::Char('V') if !control => {
                self.show_verification = !self.show_verification;
                InputResult::Draw
            }
            KeyCode::Char('M') if !control => InputResult::Action(UiAction::PrepareActions {
                surface: crate::ActionSurface::Manager {
                    key: (!self.history.active)
                        .then(|| self.selected_row())
                        .flatten()
                        .map(|row| row.key.clone()),
                },
            }),
            KeyCode::Char('<' | '>' | '\\') if !control => {
                InputResult::Action(UiAction::PrepareActions {
                    surface: crate::ActionSurface::Slot {
                        target: match key.code {
                            KeyCode::Char('<') => SessionTarget::WtSource,
                            KeyCode::Char('>') => SessionTarget::Main,
                            _ => SessionTarget::Dotfiles,
                        },
                    },
                })
            }
            KeyCode::Char('d') if control => self.jump_section(true),
            KeyCode::Char('J' | 'K') if !control => self
                .selected_section()
                .map(|section| {
                    InputResult::Action(UiAction::Reorder {
                        key: self.selected_row().map(|row| row.key.clone()),
                        section: section.key.clone(),
                        down: key.code == KeyCode::Char('J'),
                    })
                })
                .unwrap_or(InputResult::Unchanged),
            KeyCode::Char('u') if control => self.jump_section(false),
            KeyCode::Tab if !control => self
                .selected_section()
                .map(|section| {
                    InputResult::Action(UiAction::FoldSection {
                        key: section.key.clone(),
                        folded: !section.folded,
                    })
                })
                .unwrap_or(InputResult::Unchanged),
            KeyCode::Char('q') | KeyCode::Char('c')
                if key.code == KeyCode::Char('q') || control =>
            {
                InputResult::Quit
            }
            KeyCode::Char('?') => {
                self.help = true;
                InputResult::Draw
            }
            KeyCode::Char('P') => {
                self.show_perf = !self.show_perf;
                InputResult::Action(UiAction::SetPerf {
                    active: true,
                    continuous: self.perf_continuous,
                    refresh: false,
                })
            }
            KeyCode::Char('l') if !control => {
                self.row_action(|key| UiAction::PrepareSection { key })
            }
            KeyCode::Char('L') if !control => {
                if let Some(section) = self
                    .selected_section()
                    .filter(|section| !section.key.starts_with('\0'))
                {
                    self.interaction = Interaction::Text(TextPrompt {
                        action: TextAction::RenameSection {
                            old: section.key.clone(),
                        },
                        prompt: "Rename section".into(),
                        editor: LineEditor::new(&section.key),
                        allow_empty: false,
                    });
                    InputResult::Draw
                } else {
                    InputResult::Unchanged
                }
            }
            KeyCode::Char('t') if !control => {
                if let Some(row) = self.selected_row() {
                    self.title_prompt = Some(TitlePrompt {
                        key: row.key.clone(),
                        editor: LineEditor::new(&row.title),
                    });
                    self.toast = None;
                    InputResult::Draw
                } else {
                    InputResult::Unchanged
                }
            }
            KeyCode::Char('n') if !control && !shift => {
                if self.board.hosts.len() > 1 {
                    return InputResult::Action(UiAction::PrepareCreate {
                        initial: String::new(),
                    });
                }
                self.interaction = Interaction::Text(TextPrompt {
                    action: TextAction::Create,
                    prompt: "new: ".into(),
                    editor: LineEditor::default(),
                    allow_empty: false,
                });
                InputResult::Draw
            }
            KeyCode::Char('n') if control => InputResult::Action(UiAction::PrepareCreate {
                initial: String::new(),
            }),
            KeyCode::Char('N') | KeyCode::Char('n') if !control && shift => {
                let initial = self
                    .selected_row()
                    .map(|row| format!("--base {}", row.branch))
                    .unwrap_or_default();
                if let Some(host) = self.selected_row().and_then(|row| row.host.as_ref()) {
                    return InputResult::Action(UiAction::OnHost {
                        host: Some(host.clone()),
                        action: Box::new(UiAction::PrepareCreate { initial }),
                    });
                }
                self.interaction = Interaction::Text(TextPrompt {
                    action: TextAction::Create,
                    prompt: "new: ".into(),
                    editor: LineEditor::new(&initial),
                    allow_empty: false,
                });
                InputResult::Draw
            }
            KeyCode::BackTab if !control => InputResult::Action(UiAction::CyclePrimary),
            KeyCode::Char('o') if !control => self.row_action(|key| UiAction::OpenEditor { key }),
            KeyCode::Char('d') if !control => {
                self.row_action(|key| UiAction::PrepareRemove { key })
            }
            KeyCode::Char('c') if !control => InputResult::Action(UiAction::PrepareCleanup),
            KeyCode::Char('a') if !control => {
                self.row_action(|key| UiAction::ToggleArchive { key })
            }
            KeyCode::Char('u') if !control => {
                self.row_action(|key| UiAction::PrepareStatus { key })
            }
            KeyCode::Char('b') if !control => self.row_action(|key| UiAction::PrepareBase { key }),
            KeyCode::Char('#') => {
                if let Some(row) = self.selected_row() {
                    self.interaction = Interaction::Text(TextPrompt {
                        action: TextAction::IssueOverride {
                            key: row.key.clone(),
                        },
                        prompt: format!("{} issue: ", row.slug),
                        editor: LineEditor::new(row.issue_id.as_deref().unwrap_or_default()),
                        allow_empty: true,
                    });
                    InputResult::Draw
                } else {
                    InputResult::Unchanged
                }
            }
            KeyCode::Char('p') if !control => self.url_action(UrlKind::PullRequest),
            KeyCode::Char('i') if !control => self.url_action(UrlKind::Issue),
            KeyCode::Char('I') if !control => self.url_action(UrlKind::PrimaryIssue),
            KeyCode::Char('s') if !control => self.url_action(UrlKind::StageOrDev),
            KeyCode::F(10) if shift => self.row_action(|key| UiAction::PrepareStopTerminal {
                key,
                target: SessionTarget::Shell,
            }),
            KeyCode::F(11) if shift => self.row_action(|key| UiAction::PrepareStopTerminal {
                key,
                target: SessionTarget::Diff,
            }),
            KeyCode::F(10) => self.session_action(SessionTarget::Shell),
            KeyCode::F(11) => self.session_action(SessionTarget::Diff),
            KeyCode::F(12) if shift => self.row_action(|key| UiAction::PrepareSessions {
                key: Some(key),
                target: SessionTarget::Harness,
            }),
            KeyCode::Char(';') if !control => self.row_action(|key| UiAction::PrepareSessions {
                key: Some(key),
                target: SessionTarget::Harness,
            }),
            KeyCode::F(12) => self.session_action(SessionTarget::Harness),
            KeyCode::Char('m') if !control => InputResult::Action(UiAction::Session {
                key: None,
                target: SessionTarget::Manager,
            }),
            KeyCode::Char('.') => InputResult::Action(UiAction::Session {
                key: None,
                target: SessionTarget::Main,
            }),
            KeyCode::Char(',') => InputResult::Action(UiAction::Session {
                key: None,
                target: SessionTarget::WtSource,
            }),
            KeyCode::Char('/') => InputResult::Action(UiAction::Session {
                key: None,
                target: SessionTarget::Dotfiles,
            }),
            KeyCode::Char('y') if !control => {
                if !self.yank_choices().is_empty() {
                    self.yank = Some(0);
                    InputResult::Draw
                } else {
                    InputResult::Unchanged
                }
            }
            KeyCode::Char('r') if !control => InputResult::Refresh,
            KeyCode::Char('j') if control => {
                self.details_scroll = self.details_scroll.saturating_add(3);
                InputResult::Draw
            }
            KeyCode::Char('k') if control => {
                self.details_scroll = self.details_scroll.saturating_sub(3);
                InputResult::Draw
            }
            KeyCode::Down | KeyCode::Char('j') if !control => {
                self.select(self.selected.unwrap_or(0).saturating_add(1))
            }
            KeyCode::Up | KeyCode::Char('k') if !control => {
                self.select(self.selected.unwrap_or(0).saturating_sub(1))
            }
            KeyCode::Home | KeyCode::Char('g') => self.select(0),
            KeyCode::End | KeyCode::Char('G') => self.select(self.item_count().saturating_sub(1)),
            KeyCode::PageDown => self.select(self.selected.unwrap_or(0).saturating_add(height / 2)),
            KeyCode::PageUp => self.select(self.selected.unwrap_or(0).saturating_sub(height / 2)),
            KeyCode::Char(' ') => {
                let length = self.item_count();
                let next = (1..=length)
                    .map(|delta| (self.selected.unwrap_or(0) + delta) % length)
                    .find(|&index| match self.item(index) {
                        Some(VisualItem::Row(row)) => self.board.rows[row].needs_attention,
                        Some(VisualItem::Section(section)) => {
                            let section = &self.board.sections[section];
                            section.folded
                                && section
                                    .rows
                                    .iter()
                                    .any(|&row| self.board.rows[row].needs_attention)
                        }
                        Some(VisualItem::ReviewRequest(_)) => true,
                        Some(VisualItem::ReviewHeader) => self.reviews_folded,
                        None => false,
                    });
                next.map_or(InputResult::Unchanged, |index| self.select(index))
            }
            _ => InputResult::Unchanged,
        }
    }

    fn row_action(&self, build: impl FnOnce(String) -> UiAction) -> InputResult {
        self.selected_row()
            .map(|row| InputResult::Action(build(row.key.clone())))
            .unwrap_or(InputResult::Unchanged)
    }

    fn session_action(&self, target: SessionTarget) -> InputResult {
        self.selected_row()
            .map(|row| {
                InputResult::Action(UiAction::Session {
                    key: Some(row.key.clone()),
                    target,
                })
            })
            .unwrap_or(InputResult::Unchanged)
    }

    fn url_action(&self, kind: UrlKind) -> InputResult {
        self.selected_row()
            .filter(|row| match kind {
                UrlKind::PullRequest => row.pr_url.is_some(),
                UrlKind::Issue => row.issue_url.is_some() || row.github_issue_url.is_some(),
                UrlKind::PrimaryIssue => row.issue_url.is_some(),
                UrlKind::StageOrDev => row.stage_url.is_some() || row.dev_url.is_some(),
            })
            .map(|row| {
                InputResult::Action(UiAction::OpenUrl {
                    key: row.key.clone(),
                    kind,
                })
            })
            .unwrap_or(InputResult::Unchanged)
    }

    fn interaction_input(&mut self, key: KeyEvent, mut interaction: Interaction) -> InputResult {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let code = key.code;
        match &mut interaction {
            Interaction::None => InputResult::Unchanged,
            Interaction::Reviewers(picker) => {
                if matches!(code, KeyCode::Esc | KeyCode::Char('q'))
                    || (control && code == KeyCode::Char('c'))
                {
                    return InputResult::Draw;
                }
                if matches!(code, KeyCode::Enter | KeyCode::Char('v')) {
                    return InputResult::Action(UiAction::SubmitReviewers {
                        key: picker.key.clone(),
                        pr_number: picker.pr_number,
                        original: picker.original.clone(),
                        selected: picker
                            .candidates
                            .iter()
                            .filter(|option| option.selected)
                            .map(|option| option.login.clone())
                            .collect(),
                    });
                }
                if code == KeyCode::Char(' ')
                    && let Some(option) = picker.candidates.get_mut(picker.selected)
                {
                    option.selected = !option.selected;
                } else {
                    picker_move(code, &mut picker.selected, picker.candidates.len());
                }
                self.interaction = interaction;
                InputResult::Draw
            }
            Interaction::Log { lines, scroll, .. } => {
                if matches!(key.code, KeyCode::Esc | KeyCode::Char('q'))
                    || (key.modifiers.contains(KeyModifiers::CONTROL)
                        && key.code == KeyCode::Char('c'))
                {
                    return InputResult::Draw;
                }
                match key.code {
                    KeyCode::Down | KeyCode::Char('j') => {
                        *scroll = scroll.saturating_add(3).min(lines.len().saturating_sub(1))
                    }
                    KeyCode::Up | KeyCode::Char('k') => *scroll = scroll.saturating_sub(3),
                    KeyCode::Home | KeyCode::Char('g') => *scroll = 0,
                    KeyCode::End | KeyCode::Char('G') => *scroll = lines.len().saturating_sub(1),
                    KeyCode::PageDown => {
                        *scroll = scroll.saturating_add(12).min(lines.len().saturating_sub(1))
                    }
                    KeyCode::PageUp => *scroll = scroll.saturating_sub(12),
                    _ => {}
                }
                self.interaction = interaction;
                InputResult::Draw
            }
            Interaction::Text(prompt) => {
                let result = prompt.editor.input(key);
                match result {
                    EditResult::Cancel => InputResult::Draw,
                    EditResult::Changed => {
                        self.interaction = interaction;
                        InputResult::Draw
                    }
                    EditResult::Unchanged => {
                        self.interaction = interaction;
                        InputResult::Unchanged
                    }
                    EditResult::Submit => {
                        let text = prompt.editor.text();
                        if !prompt.allow_empty && text.trim().is_empty() {
                            self.interaction = interaction;
                            return InputResult::Unchanged;
                        }
                        let action = match &prompt.action {
                            TextAction::SessionName { selection } => {
                                let mut selection = selection.clone();
                                selection.managed_name = Some(text.trim().to_owned());
                                UiAction::SelectSession { selection }
                            }
                            TextAction::ActionArg { surface, id } => UiAction::PrepareAction {
                                surface: surface.clone(),
                                id: id.clone(),
                                arg: Some(text),
                            },
                            TextAction::ActionExtras { surface, id, arg } => UiAction::RunAction {
                                surface: surface.clone(),
                                id: id.clone(),
                                arg: arg.clone(),
                                extras: text,
                            },
                            TextAction::Create => UiAction::Create { input: text },
                            TextAction::NewSection { key } => {
                                self.last_section_target = Some(Some(text.clone()));
                                UiAction::MoveSection {
                                    key: key.clone(),
                                    section: Some(text),
                                }
                            }
                            TextAction::RenameSection { old } => UiAction::RenameSection {
                                old: old.clone(),
                                new: text,
                            },
                            TextAction::IssueOverride { key } => UiAction::SetIssueOverride {
                                key: key.clone(),
                                issue_id: (!text.is_empty()).then_some(text),
                            },
                            TextAction::StatusNote { key, state } => UiAction::SetStatus {
                                key: key.clone(),
                                state: Some(state.clone()),
                                note: Some(text),
                                verify_after_merge: clear_verification_for(state),
                            },
                            TextAction::VerifyAfterMerge { key, state } => UiAction::SetStatus {
                                key: key.clone(),
                                state: Some(state.clone()),
                                note: None,
                                verify_after_merge: Some(text),
                            },
                        };
                        InputResult::Action(action)
                    }
                }
            }
            Interaction::Confirm(confirm) => {
                if confirm_scroll(code, &mut confirm.selected, confirm.lines.len()) {
                    self.interaction = interaction;
                    return InputResult::Draw;
                }
                let cancel = matches!(code, KeyCode::Esc | KeyCode::Char('q'))
                    || (control && code == KeyCode::Char('c'))
                    || confirm
                        .cancel_key
                        .is_some_and(|ch| code == KeyCode::Char(ch));
                if cancel {
                    return InputResult::Draw;
                }
                if code == KeyCode::Enter || code == KeyCode::Char('y') {
                    return InputResult::Action(match &confirm.action {
                        ConfirmAction::HardRefresh => UiAction::HardRefresh,
                        ConfirmAction::StopTerminal {
                            key,
                            target,
                            session_id,
                            created_at,
                        } => UiAction::StopTerminal {
                            key: key.clone(),
                            target: *target,
                            session_id: session_id.clone(),
                            created_at: *created_at,
                        },
                        ConfirmAction::ReviewCheckout {
                            url,
                            updated_at,
                            branch,
                        } => UiAction::ReviewCheckout {
                            url: url.clone(),
                            updated_at: updated_at.clone(),
                            branch: branch.clone(),
                        },
                        ConfirmAction::RestoreRemoved {
                            key,
                            removed_at,
                            branch,
                        } => UiAction::RestoreRemoved {
                            key: key.clone(),
                            removed_at: removed_at.clone(),
                            branch: branch.clone(),
                        },
                        ConfirmAction::Github { key, ship } => {
                            if *ship {
                                UiAction::GithubShip { key: key.clone() }
                            } else {
                                UiAction::GithubMarkReady { key: key.clone() }
                            }
                        }
                        ConfirmAction::StopSession { selection } => UiAction::StopSession {
                            selection: selection.clone(),
                        },
                        ConfirmAction::KillAction { action_key, run_id } => UiAction::KillAction {
                            action_key: action_key.clone(),
                            run_id: run_id.clone(),
                        },
                        ConfirmAction::Remove {
                            key,
                            force,
                            revision,
                        } => UiAction::Remove {
                            key: key.clone(),
                            force: *force,
                            revision: revision.clone(),
                        },
                        ConfirmAction::Cleanup { revisions } => UiAction::Cleanup {
                            revisions: revisions.clone(),
                        },
                    });
                }
                self.interaction = interaction;
                InputResult::Unchanged
            }
            Interaction::Picker(picker) => {
                if code == KeyCode::Char('d')
                    && !control
                    && let PickerAction::Sessions { choices } = &picker.action
                    && let Some(selection) = choices.get(picker.selected)
                    && selection.mode == crate::SessionMode::Resume
                {
                    return InputResult::Action(UiAction::PrepareStopSession {
                        selection: selection.clone(),
                    });
                }
                let quick_pick = match code {
                    KeyCode::Char(digit @ '1'..='9') => {
                        let index = digit as usize - '1' as usize;
                        (index < picker.options.len()).then_some(index)
                    }
                    _ => None,
                };
                let action_chord = matches!(picker.action, PickerAction::Actions { .. })
                    && picker
                        .options
                        .iter()
                        .any(|option| option.chord.is_some_and(|ch| code == KeyCode::Char(ch)));
                if quick_pick.is_none()
                    && !action_chord
                    && picker_move(code, &mut picker.selected, picker.options.len())
                {
                    self.interaction = interaction;
                    return InputResult::Draw;
                }
                if code == KeyCode::Esc
                    || code == KeyCode::Char('q')
                    || (control && code == KeyCode::Char('c'))
                {
                    return InputResult::Draw;
                }
                let direct = match code {
                    KeyCode::Char(ch) => picker
                        .options
                        .iter()
                        .position(|option| option.chord == Some(ch)),
                    _ => None,
                };
                if matches!(picker.action, PickerAction::Status { .. })
                    && code == KeyCode::Char('m')
                {
                    let Some(option) = picker.options.get(picker.selected).cloned() else {
                        self.interaction = interaction;
                        return InputResult::Unchanged;
                    };
                    let PickerAction::Status { key } = &picker.action else {
                        unreachable!()
                    };
                    let Some(state) = option.value.clone() else {
                        self.interaction = interaction;
                        return InputResult::Unchanged;
                    };
                    let (action, prompt, initial) = if let Some(steps) = option.verify_after_merge {
                        (
                            TextAction::VerifyAfterMerge {
                                key: key.clone(),
                                state,
                            },
                            "verify after merge: ".into(),
                            steps,
                        )
                    } else {
                        (
                            TextAction::StatusNote {
                                key: key.clone(),
                                state,
                            },
                            "status note: ".into(),
                            option.note.clone().unwrap_or_default(),
                        )
                    };
                    self.interaction = Interaction::Text(TextPrompt {
                        action,
                        prompt,
                        editor: LineEditor::new(&initial),
                        allow_empty: true,
                    });
                    return InputResult::Draw;
                }
                if let PickerAction::Section { key } = &picker.action
                    && code == KeyCode::Char('n')
                    && !control
                {
                    self.interaction = Interaction::Text(TextPrompt {
                        action: TextAction::NewSection { key: key.clone() },
                        prompt: "New section".into(),
                        editor: LineEditor::new(""),
                        allow_empty: false,
                    });
                    return InputResult::Draw;
                }
                let opener = match picker.action {
                    PickerAction::Host { .. } => KeyCode::Enter,
                    PickerAction::Actions { .. } => KeyCode::Char('!'),
                    PickerAction::ActionArg { .. } => KeyCode::Enter,
                    PickerAction::Status { .. } => KeyCode::Char('u'),
                    PickerAction::Base { .. } => KeyCode::Char('b'),
                    PickerAction::Section { .. } => KeyCode::Char('l'),
                    PickerAction::Output { .. } => KeyCode::Char('\''),
                    PickerAction::Sessions { .. } => KeyCode::Char(';'),
                };
                let pick = if matches!(picker.action, PickerAction::Actions { .. }) {
                    direct.or(quick_pick)
                } else {
                    quick_pick.or(direct)
                };
                let chosen = pick.or_else(|| {
                    (code == KeyCode::Enter || code == KeyCode::Char(' ') || code == opener)
                        .then_some(picker.selected)
                });
                let Some(option) = chosen.and_then(|index| picker.options.get(index)).cloned()
                else {
                    self.interaction = interaction;
                    return InputResult::Unchanged;
                };
                match &picker.action {
                    PickerAction::Sessions { choices } => {
                        let Some(selection) = option
                            .value
                            .and_then(|value| value.parse::<usize>().ok())
                            .and_then(|index| choices.get(index))
                            .cloned()
                        else {
                            return InputResult::Unchanged;
                        };
                        if selection.mode == crate::SessionMode::New
                            && selection.harness == wt_core::HarnessId::Claude
                        {
                            self.interaction = Interaction::Text(TextPrompt {
                                action: TextAction::SessionName { selection },
                                prompt: "Session name: ".into(),
                                editor: LineEditor::new(""),
                                allow_empty: false,
                            });
                            InputResult::Draw
                        } else {
                            InputResult::Action(UiAction::SelectSession { selection })
                        }
                    }
                    PickerAction::Output { choices } => {
                        if let Some(target) = option
                            .value
                            .and_then(|value| value.parse::<usize>().ok())
                            .and_then(|index| choices.get(index))
                            .cloned()
                        {
                            self.output.choose(target);
                        }
                        InputResult::Draw
                    }
                    PickerAction::Host { action } => InputResult::Action(UiAction::OnHost {
                        host: option.value,
                        action: action.clone(),
                    }),
                    PickerAction::ActionArg { surface, id } => {
                        if let Some(value) = option.value {
                            InputResult::Action(UiAction::PrepareAction {
                                surface: surface.clone(),
                                id: id.clone(),
                                arg: Some(value),
                            })
                        } else {
                            self.interaction = Interaction::Text(TextPrompt {
                                action: TextAction::ActionArg {
                                    surface: surface.clone(),
                                    id: id.clone(),
                                },
                                prompt: picker.title.clone(),
                                editor: LineEditor::new(""),
                                allow_empty: false,
                            });
                            InputResult::Draw
                        }
                    }
                    PickerAction::Actions { surface } => {
                        InputResult::Action(UiAction::PrepareAction {
                            surface: surface.clone(),
                            id: option.value.unwrap_or_default(),
                            arg: None,
                        })
                    }
                    PickerAction::Section { key } => {
                        self.last_section_target = Some(option.value.clone());
                        InputResult::Action(UiAction::MoveSection {
                            key: key.clone(),
                            section: option.value,
                        })
                    }
                    PickerAction::Base { key } => InputResult::Action(UiAction::SetBase {
                        key: key.clone(),
                        base: option.value,
                    }),
                    PickerAction::Status { key } => {
                        if let Some(steps) = option.verify_after_merge {
                            self.interaction = Interaction::Text(TextPrompt {
                                action: TextAction::VerifyAfterMerge {
                                    key: key.clone(),
                                    state: option.value.unwrap_or_else(|| "ready".into()),
                                },
                                prompt: "verify after merge: ".into(),
                                editor: LineEditor::new(&steps),
                                allow_empty: true,
                            });
                            InputResult::Draw
                        } else {
                            let state = option.value;
                            let verify_after_merge = match state.as_deref() {
                                None | Some("verified" | "dropped") => Some(String::new()),
                                _ => None,
                            };
                            InputResult::Action(UiAction::SetStatus {
                                key: key.clone(),
                                state,
                                note: None,
                                verify_after_merge,
                            })
                        }
                    }
                }
            }
        }
    }
}

fn clear_verification_for(state: &str) -> Option<String> {
    matches!(state, "verified" | "dropped").then(String::new)
}

fn confirm_scroll(code: KeyCode, selected: &mut usize, count: usize) -> bool {
    let next = match code {
        KeyCode::Down | KeyCode::Char('j') => {
            Some(selected.saturating_add(1).min(count.saturating_sub(1)))
        }
        KeyCode::Up | KeyCode::Char('k') => Some(selected.saturating_sub(1)),
        KeyCode::PageDown => Some(selected.saturating_add(5).min(count.saturating_sub(1))),
        KeyCode::PageUp => Some(selected.saturating_sub(5)),
        KeyCode::Home | KeyCode::Char('g') => Some(0),
        KeyCode::End | KeyCode::Char('G') => Some(count.saturating_sub(1)),
        _ => None,
    };
    if let Some(next) = next {
        *selected = next;
        true
    } else {
        false
    }
}

fn picker_move(code: KeyCode, selected: &mut usize, count: usize) -> bool {
    let next = match code {
        KeyCode::Down | KeyCode::Char('j') => {
            Some(selected.saturating_add(1).min(count.saturating_sub(1)))
        }
        KeyCode::Up | KeyCode::Char('k') => Some(selected.saturating_sub(1)),
        KeyCode::PageDown => Some(selected.saturating_add(5).min(count.saturating_sub(1))),
        KeyCode::PageUp => Some(selected.saturating_sub(5)),
        KeyCode::Home | KeyCode::Char('g') => Some(0),
        KeyCode::End | KeyCode::Char('G') => Some(count.saturating_sub(1)),
        _ => None,
    };
    if let Some(next) = next {
        *selected = next;
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_inventory_selects_a_visible_row_after_empty_inbox() {
        let mut model = Model::default();
        let mut empty = snapshot(&[]);
        Arc::make_mut(empty.data.as_mut().unwrap()).sections = vec![BoardSection {
            key: "\0inbox".into(),
            title: "Inbox".into(),
            ..Default::default()
        }];
        model.apply(empty);
        let mut populated = snapshot(&["one", "two"]);
        Arc::make_mut(populated.data.as_mut().unwrap()).sections = vec![BoardSection {
            key: "\0inbox".into(),
            title: "Inbox".into(),
            rows: vec![0, 1],
            ..Default::default()
        }];
        model.apply(populated);
        assert_eq!(model.selected_row().unwrap().key, "one");
        model.input(KeyEvent::from(KeyCode::Char('j')), 20);
        assert_eq!(model.selected_row().unwrap().key, "two");
    }

    #[test]
    fn github_keys_capture_target_and_require_confirmation() {
        let mut model = Model::default();
        model.apply(snapshot(&["one", "two"]));
        assert_eq!(
            model.input(KeyEvent::from(KeyCode::Char('e')), 20),
            InputResult::Action(UiAction::PrepareGithub {
                key: "one".into(),
                ship: false
            })
        );
        model.reply(UiReply {
            modal: Some(UiModal::Confirm {
                action: ConfirmAction::Github {
                    key: "one".into(),
                    ship: true,
                },
                title: "Ship?".into(),
                lines: vec![],
                cancel_key: Some('E'),
            }),
            ..Default::default()
        });
        assert_eq!(
            model.input(KeyEvent::from(KeyCode::Enter), 20),
            InputResult::Action(UiAction::GithubShip { key: "one".into() })
        );
    }

    #[test]
    fn delayed_modal_and_background_completion_preserve_newer_input() {
        let mut model = Model::default();
        model.apply(snapshot(&["one"]));
        model.input(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE), 20);
        let generation = model.ui_generation;
        model.input(KeyEvent::new(KeyCode::Char('#'), KeyModifiers::NONE), 20);
        assert!(matches!(model.interaction, Interaction::Text(_)));
        model.reply(UiReply {
            ui_generation: Some(generation),
            modal: Some(UiModal::Log {
                title: "Old result".into(),
                lines: vec![],
            }),
            ..Default::default()
        });
        assert!(matches!(model.interaction, Interaction::Text(_)));
        model.reply(UiReply {
            message: "Background command finished".into(),
            ..Default::default()
        });
        assert!(matches!(model.interaction, Interaction::Text(_)));
    }

    fn grouped(keys: &[&str], folded: bool) -> SourceSnapshot<Board> {
        let mut state = snapshot(keys);
        let board = Arc::make_mut(state.data.as_mut().unwrap());
        board.sections = vec![BoardSection {
            key: "Batch".into(),
            title: "Batch".into(),
            folded,
            rows: (0..keys.len()).collect(),
        }];
        state
    }

    #[test]
    fn action_palette_chords_and_arguments_keep_the_original_target() {
        let mut model = Model::default();
        model.apply(snapshot(&["one", "two"]));
        let surface = crate::ActionSurface::Row { key: "one".into() };
        assert_eq!(
            model.input(KeyEvent::from(KeyCode::Char('!')), 20),
            InputResult::Action(UiAction::PrepareActions {
                surface: surface.clone()
            })
        );
        model.reply(UiReply {
            modal: Some(UiModal::Picker {
                action: PickerAction::Actions {
                    surface: surface.clone(),
                },
                title: "Actions".into(),
                selected: 0,
                options: vec![PickerOption {
                    value: Some("continue".into()),
                    label: "Continue".into(),
                    chord: Some('g'),
                    note: None,
                    verify_after_merge: None,
                }],
            }),
            ..Default::default()
        });
        model.apply(snapshot(&["two", "one"]));
        assert_eq!(
            model.input(KeyEvent::from(KeyCode::Char('g')), 20),
            InputResult::Action(UiAction::PrepareAction {
                surface: surface.clone(),
                id: "continue".into(),
                arg: None
            })
        );
        model.reply(UiReply {
            modal: Some(UiModal::Text {
                action: TextAction::ActionExtras {
                    surface: surface.clone(),
                    id: "continue".into(),
                    arg: Some("saved".into()),
                },
                prompt: "Extra instructions".into(),
                initial: "go".into(),
                allow_empty: true,
            }),
            ..Default::default()
        });
        model.input(KeyEvent::from(KeyCode::Char('q')), 20);
        assert_eq!(
            model.input(KeyEvent::from(KeyCode::Enter), 20),
            InputResult::Action(UiAction::RunAction {
                surface,
                id: "continue".into(),
                arg: Some("saved".into()),
                extras: "goq".into()
            })
        );
    }

    #[test]
    fn special_palettes_open_without_a_selected_worktree() {
        let mut model = Model::default();
        assert_eq!(
            model.input(KeyEvent::from(KeyCode::Char('M')), 20),
            InputResult::Action(UiAction::PrepareActions {
                surface: crate::ActionSurface::Manager { key: None }
            })
        );
        for (key, target) in [
            ('<', SessionTarget::WtSource),
            ('>', SessionTarget::Main),
            ('\\', SessionTarget::Dotfiles),
        ] {
            assert_eq!(
                model.input(KeyEvent::from(KeyCode::Char(key)), 20),
                InputResult::Action(UiAction::PrepareActions {
                    surface: crate::ActionSurface::Slot { target }
                })
            );
        }
    }

    #[test]
    fn folding_keeps_the_group_selected_and_hidden_rows_cannot_receive_actions() {
        let mut model = Model::default();
        model.apply(grouped(&["one", "two"], false));
        assert_eq!(model.selected_row().unwrap().key, "one");
        assert_eq!(
            model.input(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), 10),
            InputResult::Action(UiAction::FoldSection {
                key: "Batch".into(),
                folded: true
            })
        );
        model.apply(grouped(&["one", "two"], true));
        assert_eq!(model.selected, Some(0));
        assert_eq!(model.item_count(), 1);
        assert!(model.selected_row().is_none());
        assert_eq!(
            model.input(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE), 10),
            InputResult::Unchanged
        );
        assert_eq!(model.yank_choices()[0].2, "Batch");
        model.apply(grouped(&["one", "two"], false));
        model.input(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), 10);
        assert_eq!(model.selected_row().unwrap().key, "one");
    }

    #[test]
    fn section_picker_creates_from_empty_list_and_remembers_last_target() {
        let mut model = Model::default();
        model.apply(grouped(&["one", "two"], false));
        let key = |ch| KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE);
        assert_eq!(
            model.input(key('l'), 20),
            InputResult::Action(UiAction::PrepareSection { key: "one".into() })
        );
        model.reply(UiReply {
            modal: Some(UiModal::Picker {
                action: PickerAction::Section { key: "one".into() },
                title: "Section".into(),
                options: vec![],
                selected: 0,
            }),
            ..Default::default()
        });
        assert_eq!(model.input(key('n'), 20), InputResult::Draw);
        model.paste("Release");
        assert_eq!(
            model.input(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), 20),
            InputResult::Action(UiAction::MoveSection {
                key: "one".into(),
                section: Some("Release".into())
            })
        );
        let options = ["Other", "Release"]
            .map(|name| PickerOption {
                value: Some(name.into()),
                label: name.into(),
                chord: None,
                note: None,
                verify_after_merge: None,
            })
            .to_vec();
        model.reply(UiReply {
            modal: Some(UiModal::Picker {
                action: PickerAction::Section { key: "two".into() },
                title: "Section".into(),
                options,
                selected: 0,
            }),
            ..Default::default()
        });
        assert_eq!(
            model.input(key('l'), 20),
            InputResult::Action(UiAction::MoveSection {
                key: "two".into(),
                section: Some("Release".into())
            })
        );
    }

    #[test]
    fn moved_row_keeps_cursor_in_original_section_and_restore_follows_row() {
        let mut model = Model::default();
        model.apply(grouped(&["one", "two"], false));
        let mut moved = grouped(&["one", "two"], false);
        let board = Arc::make_mut(moved.data.as_mut().unwrap());
        board.sections[0].rows = vec![1];
        board.sections.push(BoardSection {
            key: "\0archived".into(),
            title: "Archived".into(),
            rows: vec![0],
            folded: false,
        });
        model.apply(moved);
        assert_eq!(model.selected_row().unwrap().key, "two");
        model.select(3);
        assert_eq!(model.selected_row().unwrap().key, "one");
        model.apply(grouped(&["one", "two"], false));
        assert_eq!(model.selected_row().unwrap().key, "one");
    }

    #[test]
    fn folded_manual_section_can_be_renamed_without_affecting_inbox() {
        let mut model = Model::default();
        model.apply(grouped(&["one"], true));
        let key = KeyEvent::new(KeyCode::Char('L'), KeyModifiers::SHIFT);
        assert_eq!(model.input(key, 20), InputResult::Draw);
        assert!(matches!(
            model.interaction,
            Interaction::Text(TextPrompt {
                action: TextAction::RenameSection { .. },
                ..
            })
        ));
        model.input(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), 20);
        let mut inbox = grouped(&["one"], true);
        Arc::make_mut(inbox.data.as_mut().unwrap()).sections[0].key = "\0inbox".into();
        model.apply(inbox);
        assert_eq!(model.input(key, 20), InputResult::Unchanged);
    }

    #[test]
    fn creation_selection_waits_until_the_new_row_is_visible_in_its_expanded_section() {
        let mut model = Model::default();
        model.apply(grouped(&["old"], true));
        model.reply(UiReply {
            select_when_visible: Some("new".into()),
            ..Default::default()
        });
        model.apply(grouped(&["old", "new"], true));
        assert_eq!(model.pending_selection.as_deref(), Some("new"));
        model.apply(grouped(&["old", "new"], false));
        assert_eq!(model.selected_row().unwrap().key, "new");
        assert!(model.pending_selection.is_none());
        model.apply(grouped(&["new", "old"], false));
        assert_eq!(model.selected_row().unwrap().key, "new");
    }

    #[test]
    fn removed_group_member_selects_its_visual_neighbor_instead_of_the_header() {
        let mut model = Model::default();
        model.apply(grouped(&["one", "two"], false));
        model.input(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), 10);
        assert_eq!(model.selected_row().unwrap().key, "two");
        model.apply(grouped(&["one"], false));
        assert_eq!(model.selected_row().unwrap().key, "one");
    }

    fn snapshot(keys: &[&str]) -> SourceSnapshot<Board> {
        SourceSnapshot {
            data: Some(Arc::new(Board {
                rows: keys
                    .iter()
                    .map(|key| BoardRow {
                        key: (*key).into(),
                        title: (*key).into(),
                        ..BoardRow::default()
                    })
                    .collect(),
                ..Board::default()
            })),
            state: SourceState::Ready,
            updated_at: None,
            revision: 0,
        }
    }

    fn removal_revision(key: &str) -> crate::RemovalRevision {
        crate::RemovalRevision {
            key: key.into(),
            path: format!("/worktrees/{key}"),
            branch: format!("feature/{key}"),
            head: "abc123".into(),
            digest: "deadbeef".into(),
            hazards: vec![],
        }
    }

    #[test]
    fn creation_waits_for_real_row_and_manual_navigation_cancels_old_intent() {
        let mut model = Model::default();
        model.apply(snapshot(&["first", "second"]));
        model.reply(UiReply {
            message: "Created".into(),
            failed: false,
            select_when_visible: Some("new".into()),
            ..UiReply::default()
        });
        model.apply(snapshot(&["first", "second"]));
        assert_eq!(model.selected_row().unwrap().key, "first");
        model.apply(snapshot(&["first", "second", "new"]));
        assert_eq!(model.selected_row().unwrap().key, "new");
        assert!(model.pending_selection.is_none());
        model.reply(UiReply {
            message: "Created".into(),
            failed: false,
            select_when_visible: Some("later".into()),
            ..UiReply::default()
        });
        model.input(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE), 20);
        model.apply(snapshot(&["first", "second", "new", "later"]));
        assert_eq!(model.selected_row().unwrap().key, "second");
    }

    #[test]
    fn title_prompt_keeps_target_identity_and_consumes_global_keys() {
        let mut model = Model::default();
        model.apply(snapshot(&["first", "second"]));
        model.input(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE), 20);
        model.apply(snapshot(&["second", "first"]));
        assert_eq!(
            model.input(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE), 20),
            InputResult::Draw
        );
        assert_eq!(
            model.input(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), 20),
            InputResult::Action(UiAction::SetTitle {
                key: "first".into(),
                title: "firstq".into()
            })
        );
        model.input(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE), 20);
        model.input(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL), 20);
        assert_eq!(
            model.input(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), 20),
            InputResult::Unchanged
        );
        assert!(model.title_prompt.is_some());
    }

    #[test]
    fn selection_tracks_identity_across_reorder_and_neighbor_after_removal() {
        let mut model = Model::default();
        model.apply(snapshot(&["a", "b", "c"]));
        model.select(1);
        model.details_scroll = 12;
        model.apply(snapshot(&["c", "a", "b"]));
        assert_eq!(model.selected, Some(2));
        assert_eq!(model.details_scroll, 12);
        model.apply(snapshot(&["c", "a"]));
        assert_eq!(model.selected_row().unwrap().key, "a");
        assert_eq!(model.details_scroll, 0);
        model.apply(snapshot(&[]));
        assert_eq!(model.selected, None);
    }

    #[test]
    fn viewport_keeps_selection_visible_after_shrinking() {
        let mut model = Model::default();
        model.apply(snapshot(&["a", "b", "c", "d", "e", "f", "g", "h", "i"]));
        model.select(6);
        for height in [8, 3, 1, 0, 6] {
            model.keep_selection_visible(height);
            let selected = model.selected.unwrap();
            assert!(selected >= model.offset);
            assert!(height == 0 || selected < model.offset + height);
        }
    }

    #[test]
    fn overlays_consume_navigation_and_cancel_before_global_keys() {
        let mut model = Model::default();
        model.apply(snapshot(&["a", "b"]));
        let key = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE);
        model.input(key('?'), 10);
        assert_eq!(model.input(key('j'), 10), InputResult::Draw);
        assert_eq!(model.help_scroll, 1);
        assert_eq!(model.selected, Some(0));
        assert_eq!(model.input(key('q'), 10), InputResult::Draw);
        assert!(!model.help);
        model.input(key('P'), 10);
        assert_eq!(
            model.input(key('q'), 10),
            InputResult::Action(UiAction::SetPerf {
                active: false,
                continuous: false,
                refresh: false
            })
        );
        assert!(!model.show_perf);
        assert_eq!(model.input(key('q'), 10), InputResult::Quit);
        assert_eq!(
            model.input(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::ALT), 10),
            InputResult::Unchanged
        );
    }

    #[test]
    fn help_search_filters_rows_and_esc_clears_before_closing() {
        let mut model = Model {
            help: true,
            ..Default::default()
        };
        let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
        assert_eq!(model.input(key(KeyCode::Char('/')), 20), InputResult::Draw);
        assert!(model.help_searching);
        model.input(key(KeyCode::Char('r')), 20);
        model.input(key(KeyCode::Char('e')), 20);
        assert!(
            crate::help::filtered_lines(&model.help_query.text())
                .iter()
                .any(|line| line.contains("Refresh"))
        );
        assert_eq!(model.input(key(KeyCode::Esc), 20), InputResult::Draw);
        assert!(!model.help_searching);
        assert!(model.help_query.text().is_empty());
        assert!(model.help);
        assert_eq!(model.input(key(KeyCode::Esc), 20), InputResult::Draw);
        assert!(!model.help);
    }

    #[test]
    fn attention_toggle_and_seen_mark_preserve_the_feed_contract() {
        let mut model = Model {
            board: Arc::new(Board {
                attention: (0..20)
                    .map(|at_ms| crate::AttentionLine {
                        at_ms,
                        source: "test".into(),
                        text: at_ms.to_string(),
                    })
                    .collect(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let key = |character| KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE);
        model.output_view(4);
        model.output.scroll(true);
        assert!(model.output.is_scrolled());
        assert!(matches!(
            model.input(key('x'), 20),
            InputResult::Action(UiAction::SetAttentionSeen { at_ms }) if at_ms > 0
        ));
        assert!(!model.output.is_scrolled());
        assert_eq!(model.input(key('"'), 20), InputResult::Draw);
        assert_eq!(model.output.target, crate::output::OutputTarget::Activity);
        assert_eq!(model.input(key('"'), 20), InputResult::Draw);
        assert_eq!(model.output.target, crate::output::OutputTarget::Attention);
    }

    #[test]
    fn create_prompt_preserves_raw_cli_text_and_shift_n_seeds_base() {
        let mut model = Model::default();
        model.apply(snapshot(&["first"]));
        model.board = Arc::new(Board {
            rows: vec![BoardRow {
                key: "first".into(),
                branch: "feature/base".into(),
                ..BoardRow::default()
            }],
            ..Board::default()
        });
        model.input(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE), 20);
        for ch in "ENG-123 --attach --gh 12 --base origin/main".chars() {
            model.input(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE), 20);
        }
        assert_eq!(
            model.input(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), 20),
            InputResult::Action(UiAction::Create {
                input: "ENG-123 --attach --gh 12 --base origin/main".into()
            })
        );
        model.input(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::SHIFT), 20);
        assert_eq!(
            model.input(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), 20),
            InputResult::Action(UiAction::Create {
                input: "--base feature/base".into()
            })
        );
    }

    #[test]
    fn remove_confirm_captures_target_and_cancel_key_never_mutates() {
        let mut model = Model::default();
        model.apply(snapshot(&["first", "second"]));
        model.select(1);
        assert_eq!(
            model.input(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE), 20),
            InputResult::Action(UiAction::PrepareRemove {
                key: "second".into()
            })
        );
        model.reply(UiReply {
            modal: Some(UiModal::Confirm {
                action: ConfirmAction::Remove {
                    key: "second".into(),
                    force: true,
                    revision: removal_revision("second"),
                },
                title: "remove second".into(),
                lines: vec!["dirty changes will be lost".into()],
                cancel_key: Some('d'),
            }),
            ..UiReply::default()
        });
        assert_eq!(
            model.input(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE), 20),
            InputResult::Draw
        );
        model.reply(UiReply {
            modal: Some(UiModal::Confirm {
                action: ConfirmAction::Remove {
                    key: "second".into(),
                    force: true,
                    revision: removal_revision("second"),
                },
                title: "remove second".into(),
                lines: vec!["dirty changes will be lost".into()],
                cancel_key: Some('d'),
            }),
            ..UiReply::default()
        });
        assert_eq!(
            model.input(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), 20),
            InputResult::Action(UiAction::Remove {
                key: "second".into(),
                force: true,
                revision: removal_revision("second"),
            })
        );
    }

    #[test]
    fn cleanup_confirmation_keeps_the_prepared_candidate_set() {
        let mut model = Model::default();
        model.apply(snapshot(&["a", "b"]));
        model.input(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), 20);
        model.reply(UiReply {
            modal: Some(UiModal::Confirm {
                action: ConfirmAction::Cleanup {
                    revisions: vec![removal_revision("a")],
                },
                title: "clean merged".into(),
                lines: vec!["a".into(), "kept b: dirty".into()],
                cancel_key: Some('c'),
            }),
            ..UiReply::default()
        });
        assert_eq!(
            model.input(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE), 20),
            InputResult::Action(UiAction::Cleanup {
                revisions: vec![removal_revision("a")]
            })
        );
    }

    #[test]
    fn status_picker_supports_chords_notes_and_verify_steps() {
        let mut model = Model::default();
        model.apply(snapshot(&["row"]));
        model.input(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE), 20);
        model.reply(UiReply {
            modal: Some(UiModal::Picker {
                action: PickerAction::Status { key: "row".into() },
                title: "status".into(),
                options: vec![
                    PickerOption {
                        value: Some("ready".into()),
                        label: "ready".into(),
                        chord: Some('y'),
                        note: Some("old note".into()),
                        verify_after_merge: None,
                    },
                    PickerOption {
                        value: Some("ready".into()),
                        label: "ready + verify after merge".into(),
                        chord: Some('a'),
                        note: None,
                        verify_after_merge: Some("probe".into()),
                    },
                    PickerOption {
                        value: None,
                        label: "clear".into(),
                        chord: Some('x'),
                        note: None,
                        verify_after_merge: None,
                    },
                ],
                selected: 0,
            }),
            ..UiReply::default()
        });
        assert_eq!(
            model.input(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE), 20),
            InputResult::Draw
        );
        model.input(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL), 20);
        for ch in "needs review".chars() {
            model.input(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE), 20);
        }
        assert_eq!(
            model.input(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), 20),
            InputResult::Action(UiAction::SetStatus {
                key: "row".into(),
                state: Some("ready".into()),
                note: Some("needs review".into()),
                verify_after_merge: None,
            })
        );
        model.input(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE), 20);
        model.reply(UiReply {
            modal: Some(UiModal::Picker {
                action: PickerAction::Status { key: "row".into() },
                title: "status".into(),
                options: vec![PickerOption {
                    value: Some("ready".into()),
                    label: "ready".into(),
                    chord: Some('y'),
                    note: Some("old note".into()),
                    verify_after_merge: None,
                }],
                selected: 0,
            }),
            ..UiReply::default()
        });
        assert_eq!(
            model.input(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE), 20),
            InputResult::Action(UiAction::SetStatus {
                key: "row".into(),
                state: Some("ready".into()),
                note: None,
                verify_after_merge: None,
            })
        );
        model.input(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE), 20);
        model.reply(UiReply {
            modal: Some(UiModal::Picker {
                action: PickerAction::Status { key: "row".into() },
                title: "status".into(),
                options: vec![PickerOption {
                    value: Some("ready".into()),
                    label: "ready + verify".into(),
                    chord: Some('a'),
                    note: None,
                    verify_after_merge: Some("probe".into()),
                }],
                selected: 0,
            }),
            ..UiReply::default()
        });
        assert_eq!(
            model.input(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE), 20),
            InputResult::Draw
        );
        assert_eq!(
            model.input(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), 20),
            InputResult::Action(UiAction::SetStatus {
                key: "row".into(),
                state: Some("ready".into()),
                note: None,
                verify_after_merge: Some("probe".into()),
            })
        );
    }

    #[test]
    fn row_urls_and_session_keys_only_emit_for_available_targets() {
        let mut model = Model::default();
        model.apply(snapshot(&["row"]));
        assert_eq!(
            model.input(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE), 20),
            InputResult::Unchanged
        );
        model.board = Arc::new(Board {
            rows: vec![BoardRow {
                key: "row".into(),
                pr_url: Some("https://example.test/pr/1".into()),
                ..BoardRow::default()
            }],
            ..Board::default()
        });
        assert_eq!(
            model.input(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE), 20),
            InputResult::Action(UiAction::OpenUrl {
                key: "row".into(),
                kind: UrlKind::PullRequest
            })
        );
        assert_eq!(
            model.input(KeyEvent::new(KeyCode::F(12), KeyModifiers::NONE), 20),
            InputResult::Action(UiAction::Session {
                key: Some("row".into()),
                target: SessionTarget::Harness
            })
        );
        assert_eq!(
            model.input(KeyEvent::new(KeyCode::Char(','), KeyModifiers::NONE), 20),
            InputResult::Action(UiAction::Session {
                key: None,
                target: SessionTarget::WtSource
            })
        );
    }
}
