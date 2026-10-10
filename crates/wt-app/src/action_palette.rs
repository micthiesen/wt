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
    let context = action_dispatch::prepare(ctx, &surface, github, board, None).await?;
    if let Some(run) = crate::actions::service(ctx)?
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
                    wt_actions::ActionRunStatus::Running | wt_actions::ActionRunStatus::Ambiguous
                )
        })
    {
        return Ok(modal(UiModal::Confirm {
            action: wt_tui::ConfirmAction::KillAction {action_key:context.action_key, run_id:run.meta.run_id},
            title: format!("Stop {}?", run.meta.action_name),
            lines: vec![format!("{} is active on {}.", run.meta.action_name, context.slug), "Stopping it terminates its worker and descendants. Partial changes remain in the worktree.".into()],
            cancel_key:Some('!'),
        }));
    }
    let keys = assign_keys(&definitions);
    let mut groups: Vec<(String, Vec<PickerOption>)> = Vec::new();
    for def in definitions.iter().filter(|def| def.id != CUSTOM_ID) {
        let reason = availability(def, &surface, &context.row);
        let group = def.group.clone().unwrap_or_default();
        let label = if group.is_empty() {
            def.name.clone()
        } else {
            format!("{group}: {}", def.name)
        };
        let option = PickerOption {
            value: Some(def.id.clone()),
            label: reason.map_or(label.clone(), |reason| format!("{label} ({reason})")),
            chord: keys.get(&def.id).copied(),
            note: None,
            verify_after_merge: None,
        };
        if let Some((_, values)) = groups.iter_mut().find(|(name, _)| *name == group) {
            values.push(option);
        } else {
            groups.push((group, vec![option]));
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
    if matches!(surface, ActionSurface::Row { .. }) {
        for option in &mut options {
            if option.chord == Some('m') {
                option.chord = None;
            }
        }
        options.insert(
            0,
            PickerOption {
                value: Some(AUTO_MERGE_ID.into()),
                label: "Toggle merge when ready".into(),
                chord: Some('m'),
                note: None,
                verify_after_merge: None,
            },
        );
    }
    options.push(PickerOption {
        value: Some(CUSTOM_ID.into()),
        label: "Custom prompt…".into(),
        chord: Some('c'),
        note: None,
        verify_after_merge: None,
    });
    let title = match &surface {
        ActionSurface::Row { .. } => format!("{} actions", context.slug),
        ActionSurface::Manager { .. } => "Manager commands".into(),
        ActionSurface::Slot { .. } => format!("{} commands", context.slug),
    };
    Ok(modal(UiModal::Picker {
        action: PickerAction::Actions { surface },
        title,
        options,
        selected: 0,
    }))
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

fn assign_keys(definitions: &[ActionDef]) -> BTreeMap<String, char> {
    let mut taken: BTreeSet<_> = ['c', 'j', 'k', 'q'].into_iter().collect();
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
        let keys = assign_keys(&defs);
        assert_eq!(keys["explicit"], 'a');
        assert_ne!(keys["derived"], 'a');
        assert!(!keys.values().any(|key| ['c', 'j', 'k', 'q'].contains(key)));
    }
}
