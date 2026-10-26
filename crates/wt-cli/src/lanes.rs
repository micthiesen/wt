//! Which scheduling lane each host request runs in, and which lanes exclude
//! each other. A pure classification of `UiAction`, shared by the controller
//! that schedules requests and by `AppContext::for_action`, which picks the
//! process budget for the same request.

use std::collections::BTreeSet;

use anyhow::Result;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Lane {
    Host(Option<String>),
    /// Host preferences and controls touch no checkout; share the control budget.
    HostControl(Option<String>),
    /// Commands on one checkout cannot exclude unrelated lifecycle work.
    Target(Option<String>, String),
    /// Session stop owns the slot resource that terminal entry also uses.
    SessionControl(Option<String>, Option<String>),
    /// Create operations serialize with each other, but a durable removal of
    /// an existing target must not hold up creating an unrelated target.
    Creation(Option<String>),
    /// Removal jobs are durable and already own the target's lifecycle lock.
    /// Keep the same target serialized while allowing unrelated work through.
    Removal(Option<String>, String),
    /// Target-scoped metadata writes must not wait for unrelated lifecycle jobs.
    Metadata(Option<String>, String),
    /// Read-only prompts and pickers must not sit behind a long accepted
    /// mutation on the same host. The final mutation still revalidates.
    Preparation(Option<String>),
    /// Background reads that prepare a board menu ahead of its key (palette
    /// prefetch). One runs at a time beside foreground preparations, never
    /// taking their slot or ordinary host capacity.
    Prefetch(Option<String>),
    /// One accepted cleanup confirmation, which may span several hosts. It
    /// owns exactly the checkouts it removes, as `(host, slug)` pairs, so
    /// unrelated checkouts stay available while each teardown runs.
    FleetCleanup(BTreeSet<(Option<String>, String)>),
    Presentation,
    /// Bounded, device-local controls must remain responsive while host jobs
    /// or interactive handoffs occupy all ordinary action slots.
    ViewControl,
    /// One device terminal, with host-local checkout identity for removal
    /// conflicts. Special slots have no worktree target.
    Terminal(Option<String>, Option<String>),
}

pub fn lane_for(action: &wt_tui::UiAction) -> Result<Lane> {
    if is_cleanup_action(action) {
        // A single confirmation can contain revisions from several hosts.
        // Serialize it as one accepted operation so shutdown drains the whole
        // fleet mutation and no second cleanup races its prepared revisions.
        // It owns only the checkouts it removes; unrelated work proceeds.
        return cleanup_lane(action, None);
    }
    let (host, action, _) =
        crate::host_routing::resolve(crate::host_routing::rename_as_title(action.clone()))?;
    if action.is_preparation() {
        // Menu ownership stays in UiAction::menu, not in the scheduler.
        return Ok(Lane::Preparation(host));
    }
    use wt_tui::UiAction as A;
    // Exhaustive by design: adding an action requires choosing the resources
    // it actually owns, rather than silently taking a whole-host lock.
    Ok(match action {
        A::OnHost { .. } => unreachable!("routing has been resolved"),
        A::SetAttentionSeen { .. } | A::SetHistoryActive { .. } | A::SetPerf { .. } => {
            Lane::ViewControl
        }
        A::CyclePrimary
        | A::ToggleAutomations { .. }
        | A::CancelAutomations
        | A::ToggleRemovedAutomations { .. }
        | A::DismissReviewRequest { .. }
        | A::KillAction { .. }
        | A::HardRefresh => Lane::HostControl(host),
        A::WarmPalette { .. } | A::DiscoverSessions { .. } => Lane::Prefetch(host),
        A::Session { key, .. } => Lane::Terminal(host, key),
        A::SelectSession { selection } => Lane::Terminal(host, selection.key),
        A::EnterHarness { key, .. } => Lane::Terminal(host, Some(key)),
        A::PerfInvestigate { .. } | A::ManagedSignIn { .. } => Lane::Terminal(host, None),
        A::OpenLink { .. }
        | A::OpenPrLink { .. }
        | A::OpenPrDefault { .. }
        | A::OpenSlotEditor { .. }
        | A::Copy { .. }
        | A::OpenEditor { .. }
        | A::OpenUrl { .. }
        | A::FoldSection { .. }
        | A::MoveSection { .. }
        | A::Reorder { .. }
        | A::RenameSection { .. }
        | A::ToggleArchive { .. } => Lane::Presentation,
        A::PrepareCreate { .. }
        | A::ResolveCreation { .. }
        | A::PrepareHardRefresh
        | A::PrepareReviewCheckout { .. }
        | A::PrepareReviewers { .. }
        | A::PrepareRestoreRemoved { .. }
        | A::PrepareGithub { .. }
        | A::PrepareActions { .. }
        | A::PrepareAction { .. }
        | A::PrepareSection { .. }
        | A::PrepareRemove { .. }
        | A::PrepareCleanup
        | A::PrepareStatus { .. }
        | A::PrepareBase { .. }
        | A::PrepareSessions { .. }
        | A::PrepareHarnesses { .. }
        | A::PrepareStopTerminal { .. }
        | A::GithubFailedChecks { .. } => unreachable!("menu declaration handles preparations"),
        A::Create { .. } | A::CreateResolved { .. } => Lane::Creation(host),
        A::Remove { key, .. } | A::DismissFailedRemoval { key, .. } => Lane::Removal(host, key),
        A::SetTitle { key, .. }
        | A::GenerateTitle { key }
        | A::SetStatus { key, .. }
        | A::SetIssueOverride { key, .. } => Lane::Metadata(host, key),
        A::SubmitReviewers { key, .. }
        | A::GithubMarkReady { key, .. }
        | A::GithubSetAutoMerge { key, .. }
        | A::GithubShip { key, .. }
        | A::SetBase { key, .. } => Lane::Target(host, key),
        A::StopTerminal { key, .. } => Lane::SessionControl(host, Some(key)),
        A::StopSession { selection } | A::KillSession { selection } => {
            Lane::SessionControl(host, selection.key)
        }
        // Arbitrary actions and stack operations can span multiple checkouts.
        A::RunAction { .. }
        | A::Restack { .. }
        | A::ReviewCheckout { .. }
        | A::RestoreRemoved { .. } => Lane::Host(host),
        A::Cleanup { .. } | A::CleanupTargets { .. } => {
            unreachable!("cleanup lanes are resolved before host routing")
        }
    })
}

/// Collects the `(host, slug)` checkouts a cleanup removes, in the same form
/// host routing gives Removal and Terminal lanes. Local keys inside an
/// explicit `OnHost` wrapper belong to that host; on a worker they are local.
pub fn cleanup_lane(action: &wt_tui::UiAction, explicit: Option<&str>) -> Result<Lane> {
    let keys: Vec<&str> = match action {
        wt_tui::UiAction::OnHost { host, action } => {
            if matches!(**action, wt_tui::UiAction::OnHost { .. }) {
                anyhow::bail!("nested host routing is not allowed");
            }
            return cleanup_lane(action, host.as_deref());
        }
        wt_tui::UiAction::CleanupTargets { targets } => {
            targets.iter().map(|target| target.key.as_str()).collect()
        }
        wt_tui::UiAction::Cleanup { revisions } => revisions
            .iter()
            .map(|revision| revision.key.as_str())
            .collect(),
        _ => unreachable!("caller checked is_cleanup_action"),
    };
    let mut targets = BTreeSet::new();
    for key in keys {
        let target = match wt_core::parse_worktree_ledger_key(key)
            .ok_or_else(|| anyhow::anyhow!("invalid worktree identity {key:?}"))?
        {
            wt_core::WorktreeRef::Local { slug } => (explicit.map(str::to_owned), slug),
            wt_core::WorktreeRef::Remote { host, slug } => (Some(host), slug),
        };
        targets.insert(target);
    }
    Ok(Lane::FleetCleanup(targets))
}

fn is_cleanup_action(action: &wt_tui::UiAction) -> bool {
    match action {
        wt_tui::UiAction::Cleanup { .. } | wt_tui::UiAction::CleanupTargets { .. } => true,
        wt_tui::UiAction::OnHost { action, .. } => is_cleanup_action(action),
        _ => false,
    }
}

pub fn lanes_conflict(left: &Lane, right: &Lane) -> bool {
    if left == right {
        return true;
    }
    match (left, right) {
        (Lane::FleetCleanup(_), Lane::FleetCleanup(_)) => return true,
        (Lane::FleetCleanup(targets), other) | (other, Lane::FleetCleanup(targets)) => {
            return cleanup_conflicts(targets, other);
        }
        _ => {}
    }
    match (left, right) {
        (Lane::Terminal(_, _), Lane::Terminal(_, _)) => true,
        (Lane::Terminal(host, Some(key)), Lane::Removal(other_host, other_key))
        | (Lane::Removal(other_host, other_key), Lane::Terminal(host, Some(key))) => {
            host == other_host && key == other_key
        }
        (Lane::Host(left), Lane::Creation(right))
        | (Lane::Creation(left), Lane::Host(right))
        | (Lane::Host(left), Lane::Removal(right, _))
        | (Lane::Removal(left, _), Lane::Host(right)) => left == right,
        (Lane::Removal(left_host, left_key), Lane::Removal(right_host, right_key)) => {
            left_host == right_host && left_key == right_key
        }
        (Lane::Target(host, key), Lane::Removal(other_host, other_key))
        | (Lane::Removal(other_host, other_key), Lane::Target(host, key)) => {
            host == other_host && key == other_key
        }
        (Lane::SessionControl(host, key), Lane::Terminal(other_host, other_key))
        | (Lane::Terminal(other_host, other_key), Lane::SessionControl(host, key)) => {
            host == other_host && key == other_key
        }
        (Lane::SessionControl(host, Some(key)), Lane::Removal(other_host, other_key))
        | (Lane::Removal(other_host, other_key), Lane::SessionControl(host, Some(key))) => {
            host == other_host && key == other_key
        }
        (Lane::Target(host, _), Lane::Host(other_host))
        | (Lane::Host(other_host), Lane::Target(host, _)) => host == other_host,
        (Lane::Metadata(host, key), Lane::Removal(other_host, other_key))
        | (Lane::Removal(other_host, other_key), Lane::Metadata(host, key)) => {
            host == other_host && key == other_key
        }
        _ => false,
    }
}

/// A cleanup excludes work on the checkouts it removes and whole-host jobs
/// on hosts it touches, matching a single Removal of each target. Creation
/// and work on unrelated checkouts proceed while its teardowns run.
fn cleanup_conflicts(targets: &BTreeSet<(Option<String>, String)>, other: &Lane) -> bool {
    let owns = |host: &Option<String>, key: &String| targets.contains(&(host.clone(), key.clone()));
    match other {
        Lane::Removal(host, key)
        | Lane::Terminal(host, Some(key))
        | Lane::SessionControl(host, Some(key))
        | Lane::Metadata(host, key)
        | Lane::Target(host, key) => owns(host, key),
        Lane::Host(host) => targets.iter().any(|(target_host, _)| target_host == host),
        _ => false,
    }
}
