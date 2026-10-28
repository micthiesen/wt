//! Board key dispatch outside modal prompts, plus paste and yank choices.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::{
    InputResult, Interaction, Model, PickerPrompt, TextPrompt, TitlePrompt, ToastTone, VisualItem,
    overlay_scroll,
};
use crate::{
    LineEditor, PickerAction, SessionTarget, TextAction, UiAction, UrlKind, editor::EditResult,
};

/// Keys held for a loading menu. Further keys are dropped.
pub(crate) const TYPEAHEAD_LIMIT: usize = 16;
/// A menu slower than this drops its typeahead instead of replaying keys
/// into a menu the user never saw.
pub(crate) const TYPEAHEAD_TTL: std::time::Duration = std::time::Duration::from_secs(2);
/// Session entry on a checkout whose removal was accepted.
pub(crate) const REMOVAL_IN_PROGRESS: &str = "Removal is already in progress";

impl Model {
    pub(crate) fn expire_toast(&mut self) {
        if self.pending_request.is_some() {
            return;
        }
        // An error is an outcome to acknowledge, not a four-second animation.
        if matches!(self.toast, Some((_, ToastTone::Failed))) {
            return;
        }
        self.toast = None;
    }

    /// Feedback for a key that has nothing to act on.
    pub(crate) fn notify(&mut self, text: impl Into<String>) -> InputResult {
        self.toast = Some((text.into(), ToastTone::Info));
        InputResult::Draw
    }

    /// Keys synthesized from the pointer act where the pointer is. They are
    /// not typeahead, so a loading menu is abandoned rather than fed.
    pub(crate) fn pointer_input(&mut self, key: KeyEvent, height: usize) -> InputResult {
        if self
            .pending_request
            .as_ref()
            .is_some_and(|request| request.preparation)
        {
            self.presentation_changed();
        }
        self.input(key, height)
    }

    pub(crate) fn input(&mut self, key: KeyEvent, height: usize) -> InputResult {
        if let Some(request) = &mut self.pending_request
            && request.generation == self.ui_generation
            && request.preparation
        {
            if matches!(key.code, KeyCode::Esc | KeyCode::Char('q'))
                || request.opener == Some(key)
                || (key.modifiers.contains(KeyModifiers::CONTROL)
                    && matches!(key.code, KeyCode::Char('c' | 'C')))
            {
                self.pending_request = None;
                self.ui_generation = self.ui_generation.wrapping_add(1);
                return self.notify("Menu cancelled");
            }
            // Keys typed before the menu opens belong to that menu, never to
            // the board underneath it. They replay in order when it opens.
            if request.typeahead.len() < TYPEAHEAD_LIMIT {
                request.typeahead.push((key, height));
            } else {
                request.typeahead_overflow = true;
            }
            return InputResult::Draw;
        }
        self.last_key = Some(key);
        let generation = self.ui_generation;
        let toast_before = self.toast.clone();
        self.ui_generation = self.ui_generation.wrapping_add(1);
        let menu_before = self.local_menu.clone();
        let result = self.input_inner(key, height);
        self.track_local_menu(menu_before, &result);
        let result = match result {
            InputResult::Action(action) => self.open_board_menu(action),
            result => result,
        };
        if let InputResult::Action(action) = &result
            && let Some(request) = &self.pending_request
            && request.preparation
        {
            let mut inner = action;
            while let UiAction::OnHost { action, .. } = inner {
                inner = action;
            }
            if *inner == request.action {
                self.pending_request = None;
                return self.notify("Menu cancelled");
            }
        }
        if matches!(result, InputResult::Unchanged) {
            self.ui_generation = generation;
        } else {
            let cancelled = self.pending_request.take();
            let cancelled_feedback = cancelled.as_ref().and_then(|request| {
                if request.preparation {
                    Some("Menu cancelled")
                } else if request.action.is_terminal_handoff() {
                    Some("Session connection cancelled")
                } else {
                    None
                }
            });
            if let Some(message) = cancelled_feedback
                && matches!(result, InputResult::Draw | InputResult::Refresh)
                && self.toast == toast_before
            {
                self.toast = Some((message.into(), ToastTone::Info));
            } else if matches!(toast_before, Some((_, ToastTone::Failed)))
                && self.toast == toast_before
                && !matches!(self.interaction, Interaction::Text(_))
            {
                self.toast = None;
            }
        }
        // Only a request the terminal is about to send has an opener.
        if !matches!(result, InputResult::Action(_)) {
            self.last_key = None;
        }
        let can_review = match &result {
            InputResult::Quit => false,
            InputResult::Action(action) => {
                let mut action = action;
                while let UiAction::OnHost { action: inner, .. } = action {
                    action = inner;
                }
                // Another request may open a menu or hand off the terminal.
                matches!(action, UiAction::Remove { .. })
            }
            _ => true,
        };
        if can_review {
            self.show_next_removal_review();
        }
        result
    }

    /// The terminal could not queue the request this key produced. Drop what
    /// was staged for it so a later, unrelated request cannot inherit it.
    pub(crate) fn request_not_sent(&mut self) {
        self.last_key = None;
        self.committed_menu = None;
        self.carry_typeahead.clear();
    }

    fn input_inner(&mut self, key: KeyEvent, height: usize) -> InputResult {
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
            // Requested reviews arm the chord too. A row without a PR keeps
            // an earlier chord alive, as in TS `usePrTargetChord.ts:54-57`.
            if let Some(url) = self
                .actionable_row()
                .and_then(|row| row.pr_url.clone())
                .or_else(|| self.selected_review().map(|review| review.url.clone()))
            {
                self.pr_chord = Some((
                    std::time::Instant::now(),
                    url,
                    key.code == KeyCode::Char('l'),
                ));
            }
        }
        if !matches!(self.interaction, Interaction::None) {
            let interaction = std::mem::take(&mut self.interaction);
            let host = self.interaction_host.clone();
            let result = self.interaction_input(key, interaction, height);
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
            if let Some((_, _, value)) = pick.and_then(|index| choices.get(index))
                && value.is_empty()
            {
                return self.notify("nothing to yank");
            }
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
            let maximum = self.help_max_scroll;
            return if overlay_scroll(key, &mut self.help_scroll, maximum, height) {
                InputResult::Draw
            } else {
                InputResult::Unchanged
            };
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
            let maximum = self
                .board
                .perf
                .len()
                .saturating_sub(1)
                .max(self.perf_max_scroll);
            if overlay_scroll(key, &mut self.perf_scroll, maximum, height) {
                return InputResult::Draw;
            }
            match key.code {
                // TS `perf.ts:23-55`: investigate the shown snapshot in the
                // wt-source session.
                KeyCode::Char('i') if !control => {
                    if self.board.perf.is_empty() {
                        return self.notify("no perf sample yet");
                    }
                    // The overlay closes when the session handoff arrives,
                    // so a failed send leaves it up with the reason.
                    return InputResult::Action(UiAction::PerfInvestigate {
                        report: self.board.perf.clone(),
                    });
                }
                // Continuous sampling moved from `i` to `c`.
                KeyCode::Char('c') if !control => {
                    self.perf_continuous = !self.perf_continuous;
                }
                KeyCode::Char('r') if !control => {}
                _ => return InputResult::Unchanged,
            }
            return InputResult::Action(UiAction::SetPerf {
                active: true,
                continuous: self.perf_continuous,
                refresh: key.code == KeyCode::Char('r'),
            });
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
                let seen = self
                    .actionable_row()
                    .map(|row| crate::Seen(row.automations_paused));
                self.row_action(|key| UiAction::ToggleAutomations {
                    key: Some(key),
                    seen,
                })
            }
            KeyCode::Char('A') if !control => {
                if !self.board.display.automations {
                    return self.notify("no [[automations]] configured");
                }
                InputResult::Action(UiAction::ToggleAutomations {
                    key: None,
                    seen: Some(crate::Seen(self.board.automations_paused)),
                })
            }
            KeyCode::Char('\'') if !control => {
                self.open_output_picker();
                InputResult::Draw
            }
            KeyCode::Char('"') if !control => {
                self.output.toggle_feed();
                InputResult::Draw
            }
            KeyCode::Char('x' | 'X')
                if !control
                    && matches!(self.output.target, crate::output::OutputTarget::Attention)
                    && self.board.attention.is_empty() =>
            {
                self.notify("attention feed is empty")
            }
            KeyCode::Char('x' | 'X')
                if !control
                    && matches!(self.output.target, crate::output::OutputTarget::Attention) =>
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
                let Some(row) = self.actionable_row() else {
                    return self.notify("select a worktree first");
                };
                let has_steps = row.verify_steps.is_some()
                    || row
                        .work
                        .as_ref()
                        .and_then(|work| work.record.as_ref())
                        .is_some_and(|record| record.verify_after_merge.is_some());
                if !has_steps {
                    return self.notify("no verify-after-merge steps on this row");
                }
                self.show_verification = !self.show_verification;
                InputResult::Draw
            }
            KeyCode::Char('M') if !control => InputResult::Action(UiAction::PrepareActions {
                surface: crate::ActionSurface::Manager {
                    key: (!self.history.active)
                        .then(|| self.actionable_row())
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
            KeyCode::Char('J' | 'K') if !control => {
                if self.selected_row().is_some_and(|row| row.pending_creation) {
                    return self.notify("Worktree is still being created");
                }
                let Some(section) = self.selected_section().map(|section| section.key.clone())
                else {
                    return self.notify("select a worktree first");
                };
                let row = self.actionable_row().map(|row| row.key.clone());
                // The cursor follows a moved row, including across a section
                // boundary (TS docs/tui.md "J / K").
                self.pending_selection = row.clone();
                InputResult::Action(UiAction::Reorder {
                    key: row,
                    section,
                    down: key.code == KeyCode::Char('J'),
                })
            }
            KeyCode::Char('u') if control => self.jump_section(false),
            KeyCode::Tab if !control => {
                let Some(section) = self.selected_section() else {
                    return self.notify("no section here to fold");
                };
                // Folding an empty section would hide nothing now and hide
                // the first row moved into it later; unfolding still clears
                // a flag left from before it emptied.
                if section.rows.is_empty() && !section.folded {
                    return self.notify("nothing to fold in an empty section");
                }
                let (key, folded) = (section.key.clone(), !section.folded);
                // Unfolding from the header lands on the section's first row.
                if !folded
                    && let Some(row) = section
                        .rows
                        .first()
                        .and_then(|&index| self.board.rows.get(index))
                {
                    self.pending_selection = Some(row.key.clone());
                }
                InputResult::Action(UiAction::FoldSection { key, folded })
            }
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
            KeyCode::Char('D') if !control => {
                let Some(section) = self.selected_section() else {
                    return self.notify("no section here to delete");
                };
                if section.key.starts_with('\0') {
                    return self
                        .notify(format!("{} is not a section you can delete", section.title));
                }
                let section = section.key.clone();
                let seen = self.board.section_witness(&section);
                InputResult::Action(if seen.is_empty() {
                    UiAction::DeleteSection {
                        section,
                        seen: crate::Seen(seen),
                    }
                } else {
                    UiAction::PrepareDeleteSection { section }
                })
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
                if let Some(row) = self.actionable_row() {
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
                // The host chosen last with Tab, local by default; Tab in the
                // prompt changes it and Ctrl+N still offers a picker.
                let host = self.last_create_host.clone().filter(|host| {
                    self.board
                        .hosts
                        .iter()
                        .any(|choice| choice.id.as_deref() == Some(host.as_str()))
                });
                self.interaction_host = host.clone();
                self.interaction = Interaction::Text(TextPrompt {
                    action: TextAction::Create {
                        placement: self.creation_placement(),
                    },
                    prompt: self.create_prompt(host.as_deref()),
                    editor: LineEditor::default(),
                    allow_empty: false,
                });
                InputResult::Draw
            }
            KeyCode::Char('n') if control => {
                if self.board.hosts.len() <= 1 {
                    return self.notify("[remote] is not configured");
                }
                InputResult::Action(UiAction::PrepareCreate {
                    placement: self.creation_placement(),
                    initial: String::new(),
                })
            }
            KeyCode::Char('O') if !control => InputResult::Action(UiAction::OpenSlotEditor {
                target: SessionTarget::Main,
            }),
            KeyCode::Esc => {
                // Clear the explicit output focus; the bottom pane returns to
                // its default feed.
                self.output.choose(crate::output::OutputTarget::default());
                InputResult::Draw
            }
            KeyCode::Char('N') | KeyCode::Char('n') if !control && shift => {
                if self.selected_row().is_some_and(|row| row.pending_creation) {
                    return self.notify("Worktree is still being created");
                }
                let initial = self
                    .actionable_row()
                    .map(|row| format!("--base {}", row.branch))
                    .unwrap_or_default();
                if let Some(host) = self.actionable_row().and_then(|row| row.host.as_ref()) {
                    return InputResult::Action(UiAction::OnHost {
                        host: Some(host.clone()),
                        action: Box::new(UiAction::PrepareCreate {
                            initial,
                            placement: self.creation_placement(),
                        }),
                    });
                }
                self.interaction = Interaction::Text(TextPrompt {
                    action: TextAction::Create {
                        placement: self.creation_placement(),
                    },
                    prompt: "New worktree name: ".into(),
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
            KeyCode::Char('c') if !control => {
                self.reply(crate::UiReply {
                    modal: Some(crate::cleanup::menu(&self.board)),
                    ..Default::default()
                });
                // Use the normal background refresh path after opening the
                // cached menu. The displayed confirmation keeps its targets.
                InputResult::Refresh
            }
            KeyCode::Char('a') if !control => {
                if let Some(row) = self.actionable_row()
                    && row.host.is_none()
                    && let Some(busy) = &row.busy
                {
                    let label = if busy.label.is_empty() {
                        &busy.op
                    } else {
                        &busy.label
                    };
                    return self.notify(format!(
                        "{} is {label}; can't change archive state",
                        row.slug
                    ));
                }
                let seen = self.actionable_row().map(|row| crate::Seen(row.archived));
                self.row_action(|key| UiAction::ToggleArchive { key, seen })
            }
            KeyCode::Char('u') if !control => {
                self.row_action(|key| UiAction::PrepareStatus { key })
            }
            KeyCode::Char('b') if !control => self.row_action(|key| UiAction::PrepareBase { key }),
            KeyCode::Char('#') => {
                if let Some(row) = self.actionable_row() {
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
            KeyCode::Char('z') if !control && self.board.display.guides => {
                self.url_action(UrlKind::Guide)
            }
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
            KeyCode::F(12) if shift => {
                self.session_row_action(|key| UiAction::PrepareHarnesses { key })
            }
            KeyCode::Char(';') if !control => {
                self.session_row_action(|key| UiAction::PrepareSessions {
                    key: Some(key),
                    target: SessionTarget::Harness,
                })
            }
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
            KeyCode::Down | KeyCode::Char('j') if !control => self.step(true),
            KeyCode::Up | KeyCode::Char('k') if !control => self.step(false),
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
                match next {
                    Some(index) => self.select(index),
                    None => self.notify("nothing needs you"),
                }
            }
            _ => InputResult::Unchanged,
        }
    }

    /// Projected creation rows participate in selection and rendering, but
    /// never provide an inventory identity to a worktree command.
    pub(super) fn actionable_row(&self) -> Option<&super::BoardRow> {
        self.selected_row().filter(|row| !row.pending_creation)
    }

    pub(super) fn row_action(&mut self, build: impl FnOnce(String) -> UiAction) -> InputResult {
        if self.selected_row().is_some_and(|row| row.pending_creation) {
            return self.notify("Worktree is still being created");
        }
        match self.actionable_row() {
            Some(row) => InputResult::Action(build(row.key.clone())),
            None => self.notify("select a worktree first"),
        }
    }

    pub(super) fn session_action(&mut self, target: SessionTarget) -> InputResult {
        self.session_row_action(|key| UiAction::Session {
            key: Some(key),
            target,
        })
    }

    /// Entering a checkout that is being removed would only queue behind
    /// its own teardown and then fail, so refuse it at once.
    pub(super) fn session_row_action(
        &mut self,
        build: impl FnOnce(String) -> UiAction,
    ) -> InputResult {
        if self.actionable_row().is_some_and(Self::removal_active) {
            return self.notify(REMOVAL_IN_PROGRESS);
        }
        self.row_action(build)
    }

    pub(super) fn removal_active(row: &super::BoardRow) -> bool {
        row.removal
            .as_ref()
            .is_some_and(|removal| removal.error.is_none())
    }

    /// Whether the board shows an active removal of the row with this key.
    pub(super) fn removal_active_for(&self, key: &str) -> bool {
        self.board
            .rows
            .iter()
            .any(|row| row.key == key && Self::removal_active(row))
    }

    pub(super) fn url_action(&mut self, kind: UrlKind) -> InputResult {
        let Some(row) = self.actionable_row() else {
            return self.notify("select a worktree first");
        };
        let missing = match kind {
            UrlKind::PullRequest => row.pr_url.is_none().then_some("no PR for this branch"),
            UrlKind::Issue => (row.issue_url.is_none() && row.github_issue_url.is_none())
                .then_some("no issue URL (set a tracker id with #, or attach a --gh issue)"),
            UrlKind::PrimaryIssue => row
                .issue_url
                .is_none()
                .then_some("no primary issue URL (set a tracker id with #)"),
            UrlKind::StageOrDev => (row.stage_url.is_none() && row.dev_url.is_none())
                .then_some("no stage deployed or dev server running"),
            UrlKind::Guide => row
                .guide
                .is_none()
                .then_some("no change guide recorded (the agent records one with wt guide)"),
        };
        if let Some(missing) = missing {
            return self.notify(missing);
        }
        InputResult::Action(UiAction::OpenUrl {
            key: row.key.clone(),
            kind,
        })
    }

    /// The new-worktree label names the target host whenever there is more
    /// than one to choose from.
    pub(super) fn create_prompt(&self, host: Option<&str>) -> String {
        if self.board.hosts.len() <= 1 {
            return "New worktree name: ".into();
        }
        let label = self
            .board
            .hosts
            .iter()
            .find(|choice| choice.id.as_deref() == host)
            .map(|choice| choice.label.as_str())
            .unwrap_or("This machine");
        format!("New worktree on {label} (Tab: host): ")
    }

    /// User edits and focus changes outside keyboard dispatch invalidate only
    /// presentation requests. Accepted domain mutations remain in Pending.
    pub(crate) fn presentation_changed(&mut self) {
        self.ui_generation = self.ui_generation.wrapping_add(1);
        if self
            .pending_request
            .take()
            .is_some_and(|request| request.preparation)
        {
            self.notify("Menu cancelled");
        }
    }

    pub(crate) fn paste(&mut self, text: &str) -> bool {
        if text.is_empty() {
            return false;
        }
        if let Interaction::Text(prompt) = &mut self.interaction {
            // Action prompts keep pasted line breaks (TS `! c`); footer
            // prompts stay one line.
            if matches!(prompt.action, TextAction::ActionExtras { .. }) {
                prompt.editor.paste_multiline(text);
            } else {
                prompt.editor.paste(text);
            }
            self.presentation_changed();
            return true;
        }
        if let Some(prompt) = &mut self.title_prompt {
            prompt.editor.paste(text);
            self.presentation_changed();
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
            // An empty section has only its name to copy.
            if rows.is_empty() {
                return vec![('n', "section", section.title.clone())];
            }
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
        self.actionable_row()
            .map(|row| {
                // Unavailable entries stay listed with an empty value so
                // digit positions are stable; picking one says so.
                let mut choices = vec![('b', "branch", row.branch.clone())];
                for (key, label, value) in [
                    ('s', "stage name", row.stage_name.as_ref()),
                    ('S', "stage URL", row.stage_url.as_ref()),
                    ('d', "dev URL", row.dev_url.as_ref()),
                ] {
                    choices.push((key, label, value.cloned().unwrap_or_default()));
                }
                choices.push(('p', "path", row.path.clone()));
                choices.push(('n', "slug", row.slug.clone()));
                for (key, label, value) in [
                    (
                        'i',
                        "issue",
                        row.issue_url.as_ref().or(row.github_issue_url.as_ref()),
                    ),
                    ('I', "primary issue", row.issue_url.as_ref()),
                    ('r', "PR URL", row.pr_url.as_ref()),
                ] {
                    choices.push((key, label, value.cloned().unwrap_or_default()));
                }
                choices
            })
            .unwrap_or_default()
    }
}
