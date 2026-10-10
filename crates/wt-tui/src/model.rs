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

#[derive(Default)]
pub enum Interaction {
    #[default]
    None,
    Text(TextPrompt),
    Picker(PickerPrompt),
    Confirm(ConfirmPrompt),
}

/// Prepared, immutable presentation data. Service code sanitizes terminal
/// controls and computes labels once when the underlying source changes.
#[derive(Clone, Debug, Default)]
pub struct Board {
    pub name: String,
    pub rows: Vec<BoardRow>,
    pub activity: Vec<String>,
    pub sections: Vec<BoardSection>,
}

#[derive(Clone, Debug, Default)]
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
}

#[derive(Clone, Debug, Default)]
pub struct BoardRow {
    pub key: String,
    pub slug: String,
    pub title: String,
    pub branch: String,
    pub path: String,
    pub badge: String,
    pub details: Vec<String>,
    pub needs_attention: bool,
    pub issue_id: Option<String>,
    pub issue_url: Option<String>,
    pub github_issue_url: Option<String>,
    pub pr_url: Option<String>,
    pub stage_url: Option<String>,
    pub dev_url: Option<String>,
    pub archived: bool,
    pub stack_prefix: String,
}

pub struct Model {
    pub board: Arc<Board>,
    pub source_state: SourceState,
    pub selected: Option<usize>,
    pub offset: usize,
    pub details_scroll: u16,
    pub help: bool,
    pub show_perf: bool,
    pub frame_count: u64,
    pub last_frame_micros: u128,
    pub title_prompt: Option<TitlePrompt>,
    pub interaction: Interaction,
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
            help: false,
            show_perf: false,
            frame_count: 0,
            last_frame_micros: 0,
            title_prompt: None,
            interaction: Interaction::None,
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
            let previous_key = self.selected_row().map(|row| row.key.clone());
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
            if self.selected_row().map(|row| row.key.as_str()) != previous_key.as_deref() {
                self.details_scroll = 0;
            }
        }
    }

    pub fn reply(&mut self, reply: UiReply) -> Option<crate::TerminalHandoff> {
        self.toast = Some((reply.message, reply.failed));
        self.interaction = match reply.modal {
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
            None => Interaction::None,
        };
        if let Some(key) = reply.select_when_visible {
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
            VisualItem::Section(_) => None,
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
        }
    }

    pub(crate) fn item_count(&self) -> usize {
        if self.board.sections.is_empty() {
            self.board.rows.len()
        } else {
            self.items.len()
        }
    }

    pub(crate) fn item(&self, position: usize) -> Option<VisualItem> {
        if self.board.sections.is_empty() {
            (position < self.board.rows.len()).then_some(VisualItem::Row(position))
        } else {
            self.items.get(position).copied()
        }
    }

    fn selected_item(&self) -> Option<VisualItem> {
        self.selected.and_then(|index| self.item(index))
    }

    fn rebuild_items(&mut self) {
        self.items.clear();
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
        if !matches!(self.interaction, Interaction::None) {
            let interaction = std::mem::take(&mut self.interaction);
            return self.interaction_input(key, interaction);
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
            if matches!(key.code, KeyCode::Esc | KeyCode::Char('q' | '?'))
                || (control && key.code == KeyCode::Char('c'))
            {
                self.help = false;
                return InputResult::Draw;
            }
            return InputResult::Unchanged;
        }
        if self.show_perf {
            if matches!(key.code, KeyCode::Esc | KeyCode::Char('q' | 'P'))
                || (control && key.code == KeyCode::Char('c'))
            {
                self.show_perf = false;
                return InputResult::Draw;
            }
            return InputResult::Unchanged;
        }
        match key.code {
            KeyCode::Char('d') if control => self.jump_section(true),
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
                InputResult::Draw
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
                self.interaction = Interaction::Text(TextPrompt {
                    action: TextAction::Create,
                    prompt: "new: ".into(),
                    editor: LineEditor::default(),
                    allow_empty: false,
                });
                InputResult::Draw
            }
            KeyCode::Char('N') | KeyCode::Char('n') if !control && shift => {
                let initial = self
                    .selected_row()
                    .map(|row| format!("--base {}", row.branch))
                    .unwrap_or_default();
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
            KeyCode::F(10) => self.session_action(SessionTarget::Shell),
            KeyCode::F(11) => self.session_action(SessionTarget::Diff),
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
                let quick_pick = match code {
                    KeyCode::Char(digit @ '1'..='9') => {
                        let index = digit as usize - '1' as usize;
                        (index < picker.options.len()).then_some(index)
                    }
                    _ => None,
                };
                if quick_pick.is_none()
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
                    PickerAction::Status { .. } => KeyCode::Char('u'),
                    PickerAction::Base { .. } => KeyCode::Char('b'),
                    PickerAction::Section { .. } => KeyCode::Char('l'),
                };
                let chosen = quick_pick.or(direct).or_else(|| {
                    (code == KeyCode::Enter || code == KeyCode::Char(' ') || code == opener)
                        .then_some(picker.selected)
                });
                let Some(option) = chosen.and_then(|index| picker.options.get(index)).cloned()
                else {
                    self.interaction = interaction;
                    return InputResult::Unchanged;
                };
                match &picker.action {
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
        assert_eq!(model.input(key('j'), 10), InputResult::Unchanged);
        assert_eq!(model.selected, Some(0));
        assert_eq!(model.input(key('q'), 10), InputResult::Draw);
        assert!(!model.help);
        model.input(key('P'), 10);
        assert_eq!(model.input(key('q'), 10), InputResult::Draw);
        assert!(!model.show_perf);
        assert_eq!(model.input(key('q'), 10), InputResult::Quit);
        assert_eq!(
            model.input(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::ALT), 10),
            InputResult::Unchanged
        );
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
