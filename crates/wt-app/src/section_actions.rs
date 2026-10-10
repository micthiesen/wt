//! Controller-side section operations; UI code only handles prepared choices.
use anyhow::{Context, Result, bail};
use wt_core::{ChainMember, build_stack_index};
use wt_tui::{PickerAction, PickerOption, UiModal, UiReply};

use crate::{commands::section, context::AppContext, lifecycle_ops::resolve_key};

pub async fn prepare(ctx: &AppContext, key: String) -> Result<UiReply> {
    let row = resolve_key(ctx, &key).await?;
    let slug = row.target.slug().to_owned();
    let (state, archived) = ctx
        .database
        .call(|store| Ok((store.read_wt_state()?, store.read_archived_keys()?)))
        .await?;
    if archived.contains(&slug) {
        bail!("Restore the archived row before moving it to a section");
    }
    let current = state["slugs"][&slug]["section"].as_str();
    let mut options: Vec<_> = section::manual_sections(&state)
        .into_iter()
        .filter(|name| Some(name.as_str()) != current)
        .map(|name| PickerOption {
            value: Some(name.clone()),
            label: name,
            chord: None,
            note: None,
            verify_after_merge: None,
        })
        .collect();
    if current.is_some() {
        options.push(PickerOption {
            value: None,
            label: "Inbox".into(),
            chord: None,
            note: None,
            verify_after_merge: None,
        });
    }
    Ok(UiReply {
        modal: Some(UiModal::Picker {
            action: PickerAction::Section { key },
            title: format!("Move {slug} to section · n new"),
            options,
            selected: 0,
        }),
        ..Default::default()
    })
}

pub async fn move_row(ctx: &AppContext, key: String, section: Option<String>) -> Result<UiReply> {
    let row = resolve_key(ctx, &key).await?;
    let records = ctx.repository.inventory(&ctx.cancellation).await?;
    let (state, archived) = ctx
        .database
        .call(|store| Ok((store.read_wt_state()?, store.read_archived_keys()?)))
        .await?;
    if archived.contains(row.target.slug()) {
        bail!("Restore the archived row before moving it to a section");
    }
    let destination = section
        .map(|name| {
            if let Some(error) = section::invalid_name(&name) {
                bail!("{error}");
            }
            Ok(section::resolve_section(&state, &name).unwrap_or_else(|| name.trim().to_owned()))
        })
        .transpose()?;
    let members: Vec<_> = records
        .iter()
        .filter(|row| !row.is_main && !archived.contains(row.target.slug()))
        .map(|row| {
            ChainMember::new(
                row.target.slug(),
                &row.target.branch,
                state["slugs"][row.target.slug()]["baseBranch"]
                    .as_str()
                    .map(str::to_owned),
            )
        })
        .collect();
    let stacks = build_stack_index(&members, &ctx.config.branch.base);
    let moving: Vec<_> = stacks
        .by_branch
        .get(&row.target.branch)
        .map(|entry| {
            stacks.layouts[entry.layout_index]
                .nodes
                .iter()
                .map(|node| node.slug.clone())
                .collect()
        })
        .unwrap_or_else(|| vec![row.target.slug().to_owned()]);
    let to = destination.clone();
    let changed = ctx
        .database
        .call(move |store| Ok(store.move_worktrees_to_section(&moving, to.as_deref())?))
        .await?;
    tracing::info!(slugs = ?changed, section = ?destination, "moved worktrees to section");
    Ok(UiReply {
        message: format!(
            "Moved {} to {}",
            row.target.slug(),
            destination.as_deref().unwrap_or("Inbox")
        ),
        ..Default::default()
    })
}

pub async fn rename(ctx: &AppContext, old: String, new: String) -> Result<UiReply> {
    if old.starts_with('\0') {
        bail!("This group is named by wt and cannot be renamed");
    }
    if let Some(error) = section::invalid_name(&new) {
        bail!("{error}");
    }
    let state = ctx
        .database
        .call(|store| Ok(store.read_wt_state()?))
        .await?;
    let from = section::resolve_section(&state, &old).context("Section no longer exists")?;
    let new = new.trim().to_owned();
    let to = section::resolve_section(&state, &new).unwrap_or(new);
    let target = to.clone();
    ctx.database
        .call(move |store| Ok(store.rename_section(&from, &target)?))
        .await?;
    Ok(UiReply {
        message: format!("Section renamed to {to}"),
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn moving_files_the_stack_and_rename_preserves_unrelated_state() {
        let fixture = crate::commands::test_support::CommandFixture::new()
            .await
            .unwrap();
        fixture
            .ctx
            .database
            .call(|store| {
                store.set_slug_base("two", Some(("feature/one", None)))?;
                store.set_slug_manual_title("two", "Pinned", None)?;
                Ok(())
            })
            .await
            .unwrap();
        let rows = fixture
            .ctx
            .repository
            .inventory(&fixture.ctx.cancellation)
            .await
            .unwrap();
        let row = rows.iter().find(|row| row.target.slug() == "one").unwrap();
        let key = wt_core::worktree_target_key(&row.target);
        let picker = prepare(&fixture.ctx, key.clone()).await.unwrap();
        assert!(matches!(picker.modal, Some(UiModal::Picker { .. })));
        move_row(&fixture.ctx, key.clone(), Some("Release".into()))
            .await
            .unwrap();
        rename(&fixture.ctx, "Release".into(), "Today".into())
            .await
            .unwrap();
        let state = fixture
            .ctx
            .database
            .call(|store| Ok(store.read_wt_state()?))
            .await
            .unwrap();
        assert_eq!(state["slugs"]["one"]["section"], "Today");
        assert_eq!(state["slugs"]["two"]["section"], "Today");
        assert_eq!(state["slugs"]["two"]["manualTitle"], "Pinned");
        fixture
            .ctx
            .database
            .call(|store| {
                store.set_archived("one", true)?;
                Ok(())
            })
            .await
            .unwrap();
        assert!(move_row(&fixture.ctx, key, None).await.is_err());
        assert!(
            rename(&fixture.ctx, "\0inbox".into(), "Renamed".into())
                .await
                .is_err()
        );
        fixture.close().await.unwrap();
    }
}
