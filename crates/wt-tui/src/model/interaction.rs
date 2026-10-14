//! Modal prompts and overlays: their state, their keymaps, and the shared
//! scroll and picker helpers.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::{InputResult, Model};
use crate::{
    ConfirmAction, LineEditor, PickerAction, PickerOption, SessionTarget, TextAction, UiAction,
    editor::EditResult,
};

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

impl Model {
    pub(super) fn interaction_input(
        &mut self,
        key: KeyEvent,
        mut interaction: Interaction,
        height: usize,
    ) -> InputResult {
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
                let close_key = self
                    .log_close_key
                    .is_some_and(|close| !control && code == KeyCode::Char(close));
                if matches!(key.code, KeyCode::Esc | KeyCode::Char('q'))
                    || (control && key.code == KeyCode::Char('c'))
                    || close_key
                {
                    self.log_close_key = None;
                    self.log_refresh = None;
                    return InputResult::Draw;
                }
                let maximum = lines.len().saturating_sub(1);
                let moved = overlay_scroll(key, scroll, maximum, height);
                self.interaction = interaction;
                if moved {
                    InputResult::Draw
                } else {
                    InputResult::Unchanged
                }
            }
            Interaction::Text(prompt) => {
                // Tab moves a new worktree between hosts. A stacked create
                // (`--base`) stays on its parent's host.
                if matches!(prompt.action, TextAction::Create)
                    && code == KeyCode::Tab
                    && !control
                    && self.board.hosts.len() > 1
                    && !prompt.editor.text().contains("--base")
                {
                    let ids = self
                        .board
                        .hosts
                        .iter()
                        .map(|choice| choice.id.clone())
                        .collect::<Vec<_>>();
                    let current = ids
                        .iter()
                        .position(|id| *id == self.interaction_host)
                        .unwrap_or(0);
                    let next = ids[(current + 1) % ids.len()].clone();
                    prompt.prompt = self.create_prompt(next.as_deref());
                    self.interaction_host = next.clone();
                    self.last_create_host = next;
                    self.interaction = interaction;
                    return InputResult::Draw;
                }
                // Session names are restricted to the Claude name charset.
                if matches!(prompt.action, TextAction::SessionName { .. })
                    && let KeyCode::Char(ch) = code
                    && !control
                    && !(ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
                {
                    self.interaction = interaction;
                    return InputResult::Unchanged;
                }
                // Backspace on an empty extras prompt is a no-op, as in TS:
                // the prompt is easy to clear by accident while editing.
                if matches!(prompt.action, TextAction::ActionExtras { .. })
                    && code == KeyCode::Backspace
                    && prompt.editor.text().is_empty()
                {
                    self.interaction = interaction;
                    return InputResult::Unchanged;
                }
                let result = prompt.editor.input(key);
                match result {
                    // Esc or Backspace on empty input inside a picker's
                    // sub-prompt returns to that picker; Ctrl+C closes all.
                    EditResult::Cancel if !(control && code == KeyCode::Char('c')) => {
                        self.toast = None;
                        match &prompt.action {
                            TextAction::NewSection { key } => {
                                InputResult::Action(UiAction::PrepareSection { key: key.clone() })
                            }
                            TextAction::SessionName { selection } => {
                                InputResult::Action(UiAction::PrepareSessions {
                                    key: selection.key.clone(),
                                    target: selection.target,
                                })
                            }
                            _ => InputResult::Draw,
                        }
                    }
                    EditResult::Cancel => {
                        self.toast = None;
                        InputResult::Draw
                    }
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
                        self.toast = None;
                        let action = match &prompt.action {
                            TextAction::SessionName { selection } => {
                                let mut selection = selection.clone();
                                // Empty input asks the host for the next
                                // automatic name.
                                let name = text.trim();
                                selection.managed_name =
                                    (!name.is_empty()).then(|| name.to_owned());
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
                let cancel = matches!(code, KeyCode::Esc | KeyCode::Char('q' | 'n'))
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
                        // The confirmation's pin is the PR the prompt described;
                        // the host refuses if its data now names another one.
                        ConfirmAction::Github { key, ship, pin } => {
                            if *ship {
                                UiAction::GithubShip {
                                    key: key.clone(),
                                    pin: pin.clone(),
                                }
                            } else {
                                UiAction::GithubMarkReady {
                                    key: key.clone(),
                                    pin: pin.clone(),
                                }
                            }
                        }
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
                        ConfirmAction::ManagedSignIn { provider, then } => {
                            UiAction::ManagedSignIn {
                                provider: provider.clone(),
                                then: then.clone(),
                            }
                        }
                        ConfirmAction::CleanupTargets { targets } => UiAction::CleanupTargets {
                            targets: targets.clone(),
                        },
                    });
                }
                self.interaction = interaction;
                InputResult::Unchanged
            }
            Interaction::Picker(picker) => {
                if let PickerAction::Sessions { choices } = &picker.action
                    && !control
                    && !key.modifiers.contains(KeyModifiers::SHIFT)
                {
                    let choices = choices.clone();
                    if let Some((result, keep_open)) =
                        self.sessions_picker_key(code, picker, &choices)
                    {
                        if keep_open {
                            self.interaction = interaction;
                        }
                        return result;
                    }
                }
                let quick_pick = match code {
                    KeyCode::Char(digit @ '1'..='9') if !control => {
                        let wanted = digit as usize - '1' as usize;
                        digit_rows(picker).get(wanted).copied()
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
                    // The note starts empty, as in TS: a new assertion
                    // carries a fresh note.
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
                            String::new(),
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
                let opener = match &picker.action {
                    PickerAction::Host { .. } => KeyCode::Enter,
                    PickerAction::Actions {
                        surface: crate::ActionSurface::Row { .. },
                    } => KeyCode::Char('!'),
                    PickerAction::Actions {
                        surface: crate::ActionSurface::Manager { .. },
                    } => KeyCode::Char('M'),
                    PickerAction::Actions {
                        surface: crate::ActionSurface::Slot { target },
                    } => KeyCode::Char(match target {
                        SessionTarget::WtSource => '<',
                        SessionTarget::Main => '>',
                        _ => '\\',
                    }),
                    PickerAction::Harness { .. } => KeyCode::F(12),
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
                // Enter or the opener re-press confirms; Space never does.
                let chosen = pick.or_else(|| {
                    (code == KeyCode::Enter || code == opener).then_some(picker.selected)
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
                                prompt: "Session name (empty for automatic): ".into(),
                                editor: LineEditor::new(""),
                                allow_empty: true,
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
                    PickerAction::Harness { key } => {
                        match option.value.as_deref().and_then(parse_harness) {
                            Some(harness) => InputResult::Action(UiAction::EnterHarness {
                                key: key.clone(),
                                harness,
                            }),
                            None => InputResult::Draw,
                        }
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
                    PickerAction::Actions { surface } => InputResult::Action(
                        UiAction::PrepareAction {
                            surface: surface.clone(),
                            id: option.value.unwrap_or_default(),
                            arg: None,
                        }
                        .resolve_palette_alias(),
                    ),
                    PickerAction::Section { key } => {
                        // The "+ new section" row (a reserved NUL value) is
                        // not a target to remember; its name is, once typed.
                        if !option
                            .value
                            .as_deref()
                            .is_some_and(|value| value.starts_with('\0'))
                        {
                            self.last_section_target = Some(option.value.clone());
                        }
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

impl Model {
    /// Sessions picker letters (TS `modal-keys/sessions.ts:105-203`). `x`
    /// on a session row kills it directly; elsewhere `c`/`x`/`o` jump to
    /// the matching `New` row without committing. `d` closes a live session
    /// gracefully. Returns the result and whether the picker stays open.
    pub(super) fn sessions_picker_key(
        &mut self,
        code: KeyCode,
        picker: &mut PickerPrompt,
        choices: &[crate::SessionSelection],
    ) -> Option<(InputResult, bool)> {
        let KeyCode::Char(letter) = code else {
            return None;
        };
        let current = picker
            .options
            .get(picker.selected)
            .and_then(|option| option.value.as_deref())
            .and_then(|value| value.parse::<usize>().ok())
            .and_then(|index| choices.get(index))
            .filter(|choice| choice.mode == crate::SessionMode::Resume)
            .cloned();
        match (letter, current) {
            ('x', Some(selection)) => {
                let (label, _) = harness_label(selection.harness);
                if selection.live {
                    Some((
                        InputResult::Action(UiAction::KillSession { selection }),
                        false,
                    ))
                } else if selection.harness == wt_core::HarnessId::Claude {
                    // Forget a dead session's stored name; nothing to forget
                    // for the unnamed primary conversation.
                    if selection.managed_name.is_some() {
                        Some((
                            InputResult::Action(UiAction::KillSession { selection }),
                            false,
                        ))
                    } else {
                        Some((InputResult::Draw, false))
                    }
                } else {
                    let result = self.notify(format!(
                        "{label} session is dead; remove via {} CLI",
                        selection.harness.as_str()
                    ));
                    Some((result, true))
                }
            }
            ('d', Some(selection)) => {
                if selection.live {
                    Some((
                        InputResult::Action(UiAction::StopSession { selection }),
                        false,
                    ))
                } else {
                    Some((self.notify("session isn't live, nothing to close"), true))
                }
            }
            ('d', None) => Some((InputResult::Unchanged, true)),
            _ => {
                let harness = wt_core::HarnessId::ALL
                    .into_iter()
                    .find(|harness| harness_label(*harness).1 == letter)?;
                let target = picker.options.iter().position(|option| {
                    option
                        .value
                        .as_deref()
                        .and_then(|value| value.parse::<usize>().ok())
                        .and_then(|index| choices.get(index))
                        .is_some_and(|choice| {
                            choice.mode == crate::SessionMode::New && choice.harness == harness
                        })
                });
                if let Some(target) = target {
                    picker.selected = target;
                }
                Some((InputResult::Draw, true))
            }
        }
    }
}

pub(super) fn parse_harness(value: &str) -> Option<wt_core::HarnessId> {
    wt_core::HarnessId::ALL
        .into_iter()
        .find(|harness| harness.as_str() == value)
}

/// Display label and picker letter for each harness (TS `VISIBLE_HARNESSES`).
fn harness_label(harness: wt_core::HarnessId) -> (&'static str, char) {
    match harness {
        wt_core::HarnessId::Claude => ("Claude", 'c'),
        wt_core::HarnessId::Codex => ("Codex", 'x'),
        wt_core::HarnessId::Opencode => ("OpenCode", 'o'),
        wt_core::HarnessId::Pi => ("Pi", 'p'),
    }
}

pub(super) fn clear_verification_for(state: &str) -> Option<String> {
    matches!(state, "verified" | "dropped").then(String::new)
}

pub(super) fn confirm_scroll(code: KeyCode, selected: &mut usize, count: usize) -> bool {
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

/// Shared overlay scroll keymap (TS `scrollbox.tsx:173-210`): j/k, arrows,
/// Ctrl+J/K/E/Y step three lines; PgUp/PgDn and Ctrl+U/D move half a page;
/// g/G and Home/End jump to the edges.
pub(crate) fn overlay_scroll(
    key: KeyEvent,
    scroll: &mut usize,
    maximum: usize,
    height: usize,
) -> bool {
    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    let half = (height / 2).max(1);
    let next = match key.code {
        KeyCode::Down | KeyCode::Char('j') => scroll.saturating_add(3),
        KeyCode::Char('e') if control => scroll.saturating_add(3),
        KeyCode::Up | KeyCode::Char('k') => scroll.saturating_sub(3),
        KeyCode::Char('y') if control => scroll.saturating_sub(3),
        KeyCode::PageDown => scroll.saturating_add(half),
        KeyCode::Char('d') if control => scroll.saturating_add(half),
        KeyCode::PageUp => scroll.saturating_sub(half),
        KeyCode::Char('u') if control => scroll.saturating_sub(half),
        KeyCode::Home => 0,
        KeyCode::Char('g') if !control => 0,
        KeyCode::End => maximum,
        KeyCode::Char('G') if !control => maximum,
        _ => return false,
    };
    *scroll = next.min(maximum);
    true
}

pub(super) fn picker_move(code: KeyCode, selected: &mut usize, count: usize) -> bool {
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

/// Picker rows that digits select, in order. Digits count real entries only
/// (TS list-picker): session rows and saved argument values; palettes are
/// picked by their letters.
pub(crate) fn digit_rows(picker: &PickerPrompt) -> Vec<usize> {
    (0..picker.options.len())
        .filter(|&index| match &picker.action {
            PickerAction::Actions { .. } => false,
            PickerAction::ActionArg { .. } => picker.options[index].value.is_some(),
            PickerAction::Sessions { choices } => picker.options[index]
                .value
                .as_deref()
                .and_then(|value| value.parse::<usize>().ok())
                .and_then(|choice| choices.get(choice))
                .is_some_and(|choice| choice.mode == crate::SessionMode::Resume),
            // A row with its own letter keeps the letter, as in TS.
            _ => picker.options[index].chord.is_none(),
        })
        .take(9)
        .collect()
}
