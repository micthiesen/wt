//! Palette preparation and dispatch live off the terminal's input thread.
use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail};
use wt_config::{ActionDef, ActionKind};
use wt_github::GithubData;
use wt_tui::{
    ActionSurface, Board, PickerAction, PickerOption, TextAction, UiAction, UiModal, UiReply,
};

use crate::{action_dispatch, context::AppContext};

pub const CUSTOM_ID: &str = "__custom__";
pub const AUTO_MERGE_ID: &str = "__auto_merge__";

pub async fn execute(
    ctx: &AppContext,
    request: UiAction,
    board: Option<&Board>,
    github: &GithubData,
) -> Result<UiReply> {
    match request {
        UiAction::PrepareActions { surface } => palette(ctx, surface, board, github).await,
        UiAction::PrepareAction { surface, id, arg } => {
            prepare(ctx, surface, id, arg, board, github).await
        }
        UiAction::RunAction {
            surface,
            id,
            arg,
            extras,
        } => run(ctx, surface, id, arg, extras, board, github).await,
        _ => bail!("unexpected request at the action palette boundary"),
    }
}

async fn palette(
    ctx: &AppContext,
    surface: ActionSurface,
    board: Option<&Board>,
    github: &GithubData,
) -> Result<UiReply> {
    let definitions = action_dispatch::definitions(ctx, &surface);
    // TS opened a slot palette even when its checkout was missing; every
    // row then shows why it can't run instead of the key failing outright.
    let blocked = slot_unavailable(ctx, &surface);
    let context = match blocked {
        Some(_) => None,
        None => Some(action_dispatch::prepare(ctx, &surface, github, board, None).await?),
    };
    if let Some(context) = &context
        && let Some(run) = crate::actions::service(ctx)?
            .list_runs(usize::MAX)
            .await?
            .into_iter()
            .find(|run| {
                let action_key = if run.meta.action_key.is_empty() {
                    &run.meta.slug
                } else {
                    &run.meta.action_key
                };
                action_key == &context.action_key
                    && matches!(
                        run.meta.status,
                        wt_actions::ActionRunStatus::Running
                            | wt_actions::ActionRunStatus::Ambiguous
                    )
            })
    {
        return Ok(modal(UiModal::Confirm {
            action: wt_tui::ConfirmAction::KillAction {action_key:context.action_key.clone(), run_id:run.meta.run_id},
            title: format!("Stop {}?", run.meta.action_name),
            lines: vec![format!("{} is active on {}.", run.meta.action_name, context.slug), "Stopping it terminates its worker and descendants. Partial changes remain in the worktree.".into()],
            cancel_key:Some('!'),
        }));
    }
    let keys = assign_keys(&definitions, reserved_keys(&surface));
    let mut groups: Vec<(String, Vec<PickerOption>)> = Vec::new();
    for def in definitions.iter().filter(|def| def.id != CUSTOM_ID) {
        let reason = match (&blocked, &context) {
            (Some(reason), _) => Some(reason.clone()),
            (None, Some(context)) => availability(def, &surface, &context.row),
            (None, None) => None,
        };
        let group = def.group.clone().unwrap_or_default();
        let label = if group.is_empty() {
            def.name.clone()
        } else {
            format!("{group}: {}", def.name)
        };
        let option = PickerOption {
            value: Some(def.id.clone()),
            label: label.clone(),
            chord: keys.get(&def.id).copied(),
            note: None,
            verify_after_merge: None,
            detail: Some(reason.map_or_else(|| action_detail(def), |reason| format!("({reason})"))),
        };
        if let Some((_, values)) = groups.iter_mut().find(|(name, _)| *name == group) {
            values.push(option);
        } else {
            groups.push((group, vec![option]));
        }
    }
    if let (ActionSurface::Row { .. }, Some(context)) = (&surface, &context) {
        let mut pinned = vec![(
            "worktree",
            PickerOption {
                value: Some(crate::action_builtins::RENAME_ID.into()),
                label: "worktree: Rename worktree with AI".into(),
                chord: Some('t'),
                note: None,
                verify_after_merge: None,
                detail: Some(rename_detail(ctx, board, &surface)),
            },
        )];
        if ctx.config.dev_server.is_some() {
            let running = crate::dev::service(ctx)?
                .logs(&context.slug, 1, &ctx.cancellation)
                .await?
                .is_some();
            pinned.push((
                "dev server",
                PickerOption {
                    value: Some(crate::action_builtins::DEV_LOGS_ID.into()),
                    label: "dev server: Open dev server logs".into(),
                    chord: Some('l'),
                    note: None,
                    verify_after_merge: None,
                    detail: Some(if running {
                        "live · scrollable".into()
                    } else {
                        "(dev server is not running)".into()
                    }),
                },
            ));
        }
        for (group, option) in pinned {
            if let Some((_, values)) = groups.iter_mut().find(|(name, _)| name == group) {
                values.push(option);
            } else {
                groups.push((group.to_owned(), vec![option]));
            }
        }
    }
    // Stable grouping preserves first appearance, except user-configured tail groups.
    groups.sort_by_key(|(group, _)| {
        ctx.config
            .ui
            .action_groups_last
            .iter()
            .position(|name| name == group)
            .map(|index| index + 1)
            .unwrap_or(0)
    });
    let mut options: Vec<_> = groups.into_iter().flat_map(|(_, values)| values).collect();
    if let ActionSurface::Row { key } = &surface {
        for option in &mut options {
            if option.chord == Some('m') {
                option.chord = None;
            }
        }
        // As in TS, merge-when-ready sits in a github group after the
        // configured actions.
        let has_pr = board
            .and_then(|board| board.rows.iter().find(|row| &row.key == key))
            .is_some_and(|row| row.pr.as_ref().is_some_and(|pr| pr.number.is_some()));
        options.push(PickerOption {
            value: Some(AUTO_MERGE_ID.into()),
            label: "github: Toggle merge when ready".into(),
            chord: Some('m'),
            note: None,
            verify_after_merge: None,
            detail: Some(if has_pr {
                "gh · merge queue aware".into()
            } else {
                "(no PR)".into()
            }),
        });
    }
    if matches!(surface, ActionSurface::Slot { .. }) {
        options.push(PickerOption {
            value: Some(crate::action_builtins::OPEN_EDITOR_ID.into()),
            label: "Open in editor".into(),
            chord: Some('z'),
            note: None,
            verify_after_merge: None,
            detail: Some(
                blocked
                    .as_ref()
                    .map_or_else(|| "local".into(), |reason| format!("({reason})")),
            ),
        });
    }
    options.push(PickerOption {
        value: Some(CUSTOM_ID.into()),
        // TS: manager and slot palettes message a live session.
        label: if matches!(surface, ActionSurface::Row { .. }) {
            "Custom prompt…"
        } else {
            "Custom message…"
        }
        .into(),
        chord: Some('c'),
        note: None,
        verify_after_merge: None,
        detail: Some(
            blocked
                .as_ref()
                .map_or_else(|| "freeform".into(), |reason| format!("({reason})")),
        ),
    });
    let slug = context
        .as_ref()
        .map(|context| context.slug.clone())
        .unwrap_or_else(|| match &surface {
            ActionSurface::Slot { target } => slot_label(*target).into(),
            _ => String::new(),
        });
    let title = match &surface {
        ActionSurface::Row { .. } => format!("{slug} actions"),
        ActionSurface::Manager { .. } => "Manager commands".into(),
        ActionSurface::Slot { .. } => format!("{slug} commands"),
    };
    Ok(modal(UiModal::Picker {
        action: PickerAction::Actions { surface },
        title,
        options,
        selected: 0,
    }))
}

fn slot_label(target: wt_tui::SessionTarget) -> &'static str {
    match target {
        wt_tui::SessionTarget::WtSource => "wt",
        wt_tui::SessionTarget::Main => "main",
        wt_tui::SessionTarget::Manager => "manager",
        wt_tui::SessionTarget::Dotfiles => "dotfiles",
        _ => "slot",
    }
}

/// Why a slot palette's checkout can't run anything, short enough for a
/// row detail. `None` for available slots and non-slot surfaces.
fn slot_unavailable(ctx: &AppContext, surface: &ActionSurface) -> Option<String> {
    let ActionSurface::Slot { target } = surface else {
        return None;
    };
    let paths = &ctx.config.paths;
    let (path, missing) = match target {
        wt_tui::SessionTarget::WtSource => match &paths.wt_source {
            None => return Some("paths.wt_source not set".into()),
            Some(path) => (path, "wt source checkout missing"),
        },
        wt_tui::SessionTarget::Dotfiles => (&paths.dotfiles, "dotfiles checkout missing"),
        wt_tui::SessionTarget::Main | wt_tui::SessionTarget::Manager => {
            (&paths.main_clone, "main clone missing")
        }
        _ => return None,
    };
    (!path.is_dir()).then(|| missing.into())
}

fn availability(
    def: &ActionDef,
    surface: &ActionSurface,
    row: &wt_actions::ActionRow,
) -> Option<String> {
    if matches!(surface, ActionSurface::Manager { key: None })
        && !crate::action_builtins::is_fleet(&def.id)
        && def.id != CUSTOM_ID
    {
        return Some("no row selected".into());
    }
    wt_actions::evaluate_requirements(&def.requires, row)
}

async fn prepare(
    ctx: &AppContext,
    surface: ActionSurface,
    id: String,
    arg: Option<String>,
    board: Option<&Board>,
    github: &GithubData,
) -> Result<UiReply> {
    if let (Some(reason), ActionSurface::Slot { target }) =
        (slot_unavailable(ctx, &surface), &surface)
    {
        bail!("{} slot unavailable: {reason}", slot_label(*target));
    }
    if let ActionSurface::Slot { target } = &surface
        && id == crate::action_builtins::OPEN_EDITOR_ID
    {
        let path = crate::controller_actions::slot_path(ctx, *target)?;
        crate::editor::open(ctx, &path).await?;
        return Ok(UiReply {
            message: format!("Opened {}", path.display()),
            ..Default::default()
        });
    }
    if matches!(surface, ActionSurface::Row { .. }) && id == crate::action_builtins::DEV_LOGS_ID {
        return dev_logs(ctx, surface, board, github).await;
    }
    let def = definition(ctx, &surface, &id)?;
    let context = action_dispatch::prepare(ctx, &surface, github, board, arg.clone()).await?;
    if let Some(reason) = availability(&def, &surface, &context.row) {
        bail!("{}: {reason}", def.name);
    }
    if let Some(prompt) = &def.arg_prompt
        && arg.is_none()
    {
        let history = wt_actions::recent_values(
            &ctx.config.paths.cache_root.join("action-history.json"),
            &id,
        )
        .await;
        if history.is_empty() {
            return Ok(modal(UiModal::Text {
                action: TextAction::ActionArg { surface, id },
                prompt: prompt.label.clone(),
                initial: String::new(),
                allow_empty: false,
            }));
        }
        let mut options = vec![PickerOption {
            value: None,
            label: "Enter a new value…".into(),
            chord: Some('n'),
            note: None,
            verify_after_merge: None,
            detail: None,
        }];
        options.extend(history.into_iter().map(|entry| {
            PickerOption {
                label: entry
                    .label
                    .map(|label| format!("{label}  {}", entry.value))
                    .unwrap_or_else(|| entry.value.clone()),
                value: Some(entry.value),
                chord: None,
                note: None,
                verify_after_merge: None,
                detail: None,
            }
        }));
        return Ok(modal(UiModal::Picker {
            action: PickerAction::ActionArg { surface, id },
            title: prompt.label.clone(),
            options,
            selected: 0,
        }));
    }
    if def.kind == ActionKind::Shell || crate::action_builtins::is_direct(&id) {
        return run(ctx, surface, id, arg, String::new(), board, github).await;
    }
    Ok(modal(UiModal::Text {
        action: TextAction::ActionExtras {
            surface,
            id: id.clone(),
            arg,
        },
        prompt: if id == CUSTOM_ID {
            "Prompt".into()
        } else {
            format!("{} · extra instructions (optional)", def.name)
        },
        initial: String::new(),
        allow_empty: id != CUSTOM_ID,
    }))
}

async fn run(
    ctx: &AppContext,
    surface: ActionSurface,
    id: String,
    arg: Option<String>,
    extras: String,
    board: Option<&Board>,
    github: &GithubData,
) -> Result<UiReply> {
    if let (Some(reason), ActionSurface::Slot { target }) =
        (slot_unavailable(ctx, &surface), &surface)
    {
        bail!("{} slot unavailable: {reason}", slot_label(*target));
    }
    let def = definition(ctx, &surface, &id)?;
    if id == CUSTOM_ID && extras.trim().is_empty() {
        bail!("custom prompt is empty");
    }
    if def.arg_prompt.is_some() && arg.as_deref().is_none_or(|arg| arg.trim().is_empty()) {
        bail!("{} requires a value", def.name);
    }
    let context = action_dispatch::prepare(ctx, &surface, github, board, arg.clone()).await?;
    if let Some(reason) = availability(&def, &surface, &context.row) {
        bail!("{}: {reason}", def.name);
    }
    if let Some(arg) = &arg {
        wt_actions::record_value(
            &ctx.config.paths.cache_root.join("action-history.json"),
            &id,
            arg,
            None,
            (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64,
            &ctx.cancellation,
        )
        .await?;
    }
    let message =
        action_dispatch::dispatch(ctx, &surface, &def, github, board, arg, &extras, Vec::new())
            .await?;
    Ok(UiReply {
        message,
        ..Default::default()
    })
}

fn definition(ctx: &AppContext, surface: &ActionSurface, id: &str) -> Result<ActionDef> {
    action_dispatch::definitions(ctx, surface)
        .into_iter()
        .find(|def| def.id == id)
        .with_context(|| format!("action {id:?} is no longer available"))
}

fn modal(modal: UiModal) -> UiReply {
    UiReply {
        modal: Some(modal),
        ..Default::default()
    }
}

/// Most dev-server output kept in the `! l` overlay.
const DEV_LOG_LINES: u32 = 500;

async fn dev_logs(
    ctx: &AppContext,
    surface: ActionSurface,
    board: Option<&Board>,
    github: &GithubData,
) -> Result<UiReply> {
    let context = action_dispatch::prepare(ctx, &surface, github, board, None).await?;
    let text = crate::dev::service(ctx)?
        .logs(&context.slug, DEV_LOG_LINES, &ctx.cancellation)
        .await?
        .ok_or_else(|| crate::controller::notice("dev server is not running"))?;
    let lines: Vec<String> = text.lines().map(str::to_owned).collect();
    let skip = lines.len().saturating_sub(DEV_LOG_LINES as usize);
    Ok(modal(UiModal::Log {
        title: format!("{} dev logs", context.slug),
        lines: lines.into_iter().skip(skip).collect(),
        close_key: Some('l'),
        refresh: Some(Box::new(UiAction::PrepareAction {
            surface,
            id: crate::action_builtins::DEV_LOGS_ID.into(),
            arg: None,
        })),
    }))
}

/// The `! t` row label, with the reason when it cannot run.
/// The palette's trailing hint, as in TS: `$ id` for shell commands, the
/// agent glyph and id for prompts, with `↪` when it goes to the live session.
fn action_detail(def: &wt_config::ActionDef) -> String {
    match (def.kind, def.target) {
        (wt_config::ActionKind::Shell, _) => format!("$ {}", def.id),
        (_, wt_config::ActionTarget::Session) => format!("↪ {}", def.id),
        _ => def.id.clone(),
    }
}

fn rename_detail(ctx: &AppContext, board: Option<&Board>, surface: &ActionSurface) -> String {
    let busy = match surface {
        ActionSurface::Row { key } => board
            .and_then(|board| board.rows.iter().find(|row| &row.key == key))
            .is_some_and(|row| row.busy.is_some()),
        _ => false,
    };
    if ctx.config.naming.is_none() {
        "(worktree naming not configured)".into()
    } else if busy {
        "(worktree is busy)".into()
    } else {
        "AI".into()
    }
}

/// Letters a surface keeps for its fixed rows, so user actions never take
/// them: custom prompt and movement everywhere, then merge, dev logs, and
/// rename on rows and open-in-editor on slots.
fn reserved_keys(surface: &ActionSurface) -> &'static [char] {
    match surface {
        ActionSurface::Row { .. } => &['c', 'j', 'k', 'q', 'm', 'l', 't'],
        ActionSurface::Manager { .. } => &['c', 'j', 'k', 'q'],
        ActionSurface::Slot { .. } => &['c', 'j', 'k', 'q', 'z'],
    }
}

fn assign_keys(definitions: &[ActionDef], reserved: &[char]) -> BTreeMap<String, char> {
    let mut taken: BTreeSet<_> = reserved.iter().copied().collect();
    let mut keys = BTreeMap::new();
    for def in definitions.iter().filter(|def| def.id != CUSTOM_ID) {
        if let Some(key) = def
            .key
            .as_deref()
            .filter(|key| key.len() == 1)
            .and_then(|key| key.chars().next())
            .map(|key| key.to_ascii_lowercase())
            && key.is_ascii_alphanumeric()
            && taken.insert(key)
        {
            keys.insert(def.id.clone(), key);
        }
    }
    for def in definitions.iter().filter(|def| def.id != CUSTOM_ID) {
        if keys.contains_key(&def.id) {
            continue;
        }
        if let Some(key) = def
            .name
            .to_lowercase()
            .chars()
            .chain('a'..='z')
            .find(|key| key.is_ascii_lowercase() && !taken.contains(key))
        {
            taken.insert(key);
            keys.insert(def.id.clone(), key);
        }
    }
    keys
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unavailable_slot_palette_opens_with_reasons() {
        let fixture = crate::commands::test_support::CommandFixture::new()
            .await
            .unwrap();
        assert!(fixture.ctx.config.paths.wt_source.is_none());
        let surface = ActionSurface::Slot {
            target: wt_tui::SessionTarget::WtSource,
        };
        let github = GithubData::default();
        let reply = palette(&fixture.ctx, surface.clone(), None, &github)
            .await
            .unwrap();
        let Some(UiModal::Picker { title, options, .. }) = reply.modal else {
            panic!("expected the slot palette");
        };
        assert_eq!(title, "wt commands");
        assert!(options.len() > 2);
        assert!(
            options
                .iter()
                .all(|option| { option.detail.as_deref() == Some("(paths.wt_source not set)") })
        );
        assert_eq!(options.last().unwrap().label, "Custom message…");
        let error = prepare(
            &fixture.ctx,
            surface,
            "slot-continue".into(),
            None,
            None,
            &github,
        )
        .await
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "wt slot unavailable: paths.wt_source not set"
        );
    }

    #[test]
    fn explicit_keys_win_before_derived_keys_and_reserved_keys_remain_free() {
        let defs = vec![
            ActionDef {
                id: "derived".into(),
                name: "Alpha".into(),
                ..Default::default()
            },
            ActionDef {
                id: "explicit".into(),
                name: "Beta".into(),
                key: Some("a".into()),
                ..Default::default()
            },
            ActionDef {
                id: "reserved".into(),
                name: "Continue".into(),
                key: Some("c".into()),
                ..Default::default()
            },
        ];
        let keys = assign_keys(&defs, reserved_keys(&ActionSurface::Manager { key: None }));
        assert_eq!(keys["explicit"], 'a');
        assert_ne!(keys["derived"], 'a');
        assert!(!keys.values().any(|key| ['c', 'j', 'k', 'q'].contains(key)));
    }

    #[test]
    fn row_palette_reserves_merge_logs_and_rename_letters() {
        let defs = vec![
            ActionDef {
                id: "lint".into(),
                name: "Lint".into(),
                key: Some("l".into()),
                ..Default::default()
            },
            ActionDef {
                id: "test".into(),
                name: "Test".into(),
                ..Default::default()
            },
        ];
        let keys = assign_keys(
            &defs,
            reserved_keys(&ActionSurface::Row { key: "a".into() }),
        );
        assert!(!keys.values().any(|key| ['m', 'l', 't'].contains(key)));
        let slot = reserved_keys(&ActionSurface::Slot {
            target: wt_tui::SessionTarget::Main,
        });
        assert!(slot.contains(&'z'));
    }
}
