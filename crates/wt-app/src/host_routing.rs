//! Central, exhaustive routing for host-owned requests. A new action must
//! choose its ownership here; local and SSH dispatch share the same handler.
use anyhow::{Result, bail};
use wt_tui::{ActionSurface, UiAction};

pub fn controller_owned(action: &UiAction) -> bool {
    match action {
        UiAction::SetPerf { .. }
        | UiAction::SetHistoryActive { .. }
        | UiAction::SetAttentionSeen { .. }
        | UiAction::OpenLink { .. }
        | UiAction::OpenPrLink { .. }
        | UiAction::OpenPrDefault { .. }
        | UiAction::OpenSlotEditor { .. }
        | UiAction::PerfInvestigate { .. }
        | UiAction::EnterHarness { .. }
        | UiAction::Copy { .. }
        | UiAction::OpenEditor { .. }
        | UiAction::OpenUrl { .. }
        | UiAction::Session { .. }
        | UiAction::SelectSession { .. }
        | UiAction::FoldSection { .. }
        | UiAction::PrepareSection { .. }
        | UiAction::MoveSection { .. }
        | UiAction::Reorder { .. }
        | UiAction::RenameSection { .. }
        | UiAction::ToggleArchive { .. } => true,
        UiAction::OnHost { action, .. } => controller_owned(action),
        UiAction::PrepareHardRefresh
        | UiAction::HardRefresh
        | UiAction::PrepareReviewCheckout { .. }
        | UiAction::ReviewCheckout { .. }
        | UiAction::DismissReviewRequest { .. }
        | UiAction::PrepareReviewers { .. }
        | UiAction::SubmitReviewers { .. }
        | UiAction::Restack { .. }
        | UiAction::PrepareRestoreRemoved { .. }
        | UiAction::RestoreRemoved { .. }
        | UiAction::ToggleRemovedAutomations { .. }
        | UiAction::KillAction { .. }
        | UiAction::PrepareActions { .. }
        | UiAction::PrepareAction { .. }
        | UiAction::RunAction { .. }
        | UiAction::CyclePrimary
        | UiAction::SetTitle { .. }
        | UiAction::GenerateTitle { .. }
        | UiAction::PrepareCreate { .. }
        | UiAction::Create { .. }
        | UiAction::PrepareRemove { .. }
        | UiAction::Remove { .. }
        | UiAction::PrepareCleanup
        | UiAction::Cleanup { .. }
        | UiAction::PrepareStatus { .. }
        | UiAction::SetStatus { .. }
        | UiAction::PrepareBase { .. }
        | UiAction::SetBase { .. }
        | UiAction::ToggleAutomations { .. }
        | UiAction::CancelAutomations
        | UiAction::PrepareGithub { .. }
        | UiAction::GithubMarkReady { .. }
        | UiAction::GithubSetAutoMerge { .. }
        | UiAction::GithubShip { .. }
        | UiAction::GithubFailedChecks { .. }
        | UiAction::PrepareSessions { .. }
        | UiAction::PrepareStopTerminal { .. }
        | UiAction::StopTerminal { .. }
        | UiAction::StopSession { .. }
        | UiAction::KillSession { .. }
        | UiAction::PrepareHarnesses { .. }
        | UiAction::SetIssueOverride { .. } => false,
    }
}

/// `Some(None)` is an explicit choice of the local host, unlike no selection.
pub fn resolve(mut action: UiAction) -> Result<(Option<String>, UiAction, bool)> {
    let explicit = if let UiAction::OnHost {
        host,
        action: inner,
    } = action
    {
        action = *inner;
        if matches!(action, UiAction::OnHost { .. }) {
            bail!("nested host routing is not allowed");
        }
        Some(host)
    } else {
        None
    };
    let mut owner: Option<Option<String>> = explicit.clone();
    visit_keys(&mut action, &mut |key| {
        let reference = wt_core::parse_worktree_ledger_key(key)
            .ok_or_else(|| anyhow::anyhow!("invalid worktree identity {key:?}"))?;
        let (host, slug) = match reference {
            wt_core::WorktreeRef::Local { slug } => (explicit.clone().flatten(), slug),
            wt_core::WorktreeRef::Remote { host, slug } => (Some(host), slug),
        };
        if let Some(owner) = &owner {
            if owner != &host {
                bail!("one host operation cannot address several hosts");
            }
        } else {
            owner = Some(host);
        }
        *key = slug;
        Ok(())
    })?;
    Ok((owner.flatten(), action, explicit.is_some()))
}

fn visit_keys(
    action: &mut UiAction,
    visit: &mut impl FnMut(&mut String) -> Result<()>,
) -> Result<()> {
    match action {
        UiAction::PrepareReviewers { key }
        | UiAction::PrepareStopTerminal { key, .. }
        | UiAction::StopTerminal { key, .. }
        | UiAction::SubmitReviewers { key, .. }
        | UiAction::Restack { key }
        | UiAction::PrepareRestoreRemoved { key }
        | UiAction::RestoreRemoved { key, .. }
        | UiAction::ToggleRemovedAutomations { key }
        | UiAction::SetTitle { key, .. }
        | UiAction::GenerateTitle { key }
        | UiAction::OpenEditor { key }
        | UiAction::PrepareRemove { key }
        | UiAction::ToggleArchive { key }
        | UiAction::PrepareStatus { key }
        | UiAction::SetStatus { key, .. }
        | UiAction::PrepareBase { key }
        | UiAction::SetBase { key, .. }
        | UiAction::SetIssueOverride { key, .. }
        | UiAction::OpenUrl { key, .. }
        | UiAction::PrepareSection { key }
        | UiAction::MoveSection { key, .. }
        | UiAction::PrepareHarnesses { key }
        | UiAction::EnterHarness { key, .. } => visit(key)?,
        UiAction::KillAction { action_key, .. } => visit(action_key)?,
        UiAction::Remove { key, revision, .. } => {
            visit(key)?;
            visit(&mut revision.key)?;
        }
        UiAction::Cleanup { revisions } => {
            for revision in revisions {
                visit(&mut revision.key)?;
            }
        }
        UiAction::Session { key, .. } | UiAction::PrepareSessions { key, .. } => {
            if let Some(key) = key {
                visit(key)?;
            }
        }
        UiAction::SelectSession { selection }
        | UiAction::KillSession { selection }
        | UiAction::StopSession { selection } => {
            if let Some(key) = &mut selection.key {
                visit(key)?;
            }
        }
        UiAction::ToggleAutomations { key } => {
            if let Some(key) = key {
                visit(key)?;
            }
        }
        UiAction::Reorder { key, .. } => {
            if let Some(key) = key {
                visit(key)?;
            }
        }
        UiAction::PrepareGithub { key, .. }
        | UiAction::GithubMarkReady { key }
        | UiAction::GithubSetAutoMerge { key, .. }
        | UiAction::GithubShip { key }
        | UiAction::GithubFailedChecks { key } => visit(key)?,
        UiAction::PrepareActions { surface }
        | UiAction::PrepareAction { surface, .. }
        | UiAction::RunAction { surface, .. } => match surface {
            ActionSurface::Row { key } => visit(key)?,
            ActionSurface::Manager { key } => {
                if let Some(key) = key {
                    visit(key)?;
                }
            }
            ActionSurface::Slot { .. } => {}
        },
        UiAction::OnHost { .. } => bail!("unresolved host routing"),
        UiAction::PrepareHardRefresh
        | UiAction::HardRefresh
        | UiAction::PrepareReviewCheckout { .. }
        | UiAction::ReviewCheckout { .. }
        | UiAction::DismissReviewRequest { .. }
        | UiAction::SetPerf { .. }
        | UiAction::SetHistoryActive { .. }
        | UiAction::SetAttentionSeen { .. }
        | UiAction::OpenLink { .. }
        | UiAction::OpenPrLink { .. }
        | UiAction::OpenPrDefault { .. }
        | UiAction::OpenSlotEditor { .. }
        | UiAction::PerfInvestigate { .. }
        | UiAction::FoldSection { .. }
        | UiAction::RenameSection { .. }
        | UiAction::Copy { .. }
        | UiAction::CyclePrimary
        | UiAction::PrepareCreate { .. }
        | UiAction::Create { .. }
        | UiAction::PrepareCleanup
        | UiAction::CancelAutomations => {}
    }
    Ok(())
}

pub fn qualify(mut action: UiAction, host: &str) -> Result<UiAction> {
    visit_keys(&mut action, &mut |key| {
        *key = wt_core::remote_worktree_ledger_key(host, key);
        Ok(())
    })?;
    Ok(action)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn same_slug_on_different_hosts_cannot_share_confirmation() {
        let action = UiAction::Remove {
            key: wt_core::remote_worktree_ledger_key("host-a", "same"),
            force: false,
            revision: wt_tui::RemovalRevision {
                key: wt_core::remote_worktree_ledger_key("host-b", "same"),
                path: "/work/same".into(),
                branch: "same".into(),
                head: "abc".into(),
                digest: "def".into(),
                hazards: vec![],
                published_base: None,
            },
        };
        assert!(resolve(action).is_err());
    }
    #[test]
    fn host_modal_captures_ownership_and_qualified_rows_decode() {
        let action = UiAction::SetTitle {
            key: "same".into(),
            title: "chosen".into(),
        };
        let (host, decoded, explicit) = resolve(UiAction::OnHost {
            host: Some("builder".into()),
            action: Box::new(action.clone()),
        })
        .unwrap();
        assert_eq!(host.as_deref(), Some("builder"));
        assert_eq!(decoded, action);
        assert!(explicit);
        let (host, decoded, _) = resolve(qualify(action.clone(), "builder").unwrap()).unwrap();
        assert_eq!(host.as_deref(), Some("builder"));
        assert_eq!(decoded, action);
    }
    #[test]
    fn selected_remote_session_is_rekeyed_before_worker_handoff() {
        let host = "builder [config: ~/work two.toml]";
        let action = UiAction::SelectSession {
            selection: wt_tui::SessionSelection {
                key: Some(wt_core::remote_worktree_ledger_key(host, "same")),
                target: wt_tui::SessionTarget::Harness,
                harness: wt_core::HarnessId::Codex,
                session_id: Some("exact-uuid".into()),
                managed_name: None,
                mode: wt_tui::SessionMode::Resume,
                live: false,
            },
        };
        let (owner, action, _) = resolve(action).unwrap();
        assert_eq!(owner.as_deref(), Some(host));
        let UiAction::SelectSession { selection } = action else {
            panic!("selection changed type")
        };
        assert_eq!(selection.key.as_deref(), Some("same"));
        assert_eq!(selection.session_id.as_deref(), Some("exact-uuid"));
    }
}
