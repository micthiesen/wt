//! Join host-local source snapshots into the pure automation evaluator input.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result};
use serde_json::Value;
use time::OffsetDateTime;
use wt_automations::{
    ActionAudience, ActionTraits, AutomationConflict, AutomationEvalContext, AutomationFire,
    AutomationPr, AutomationRow, AutomationStack, BranchTip, PauseSnapshot, StackParent, evaluate,
    fire_identity,
};
use wt_config::{ActionKind, ActionTarget, AutomationDef};
use wt_core::{ChainMember, WorkStatusRecord, build_stack_index};
use wt_github::{GithubData, PrChecks, PrReview};
use wt_platform::lock::FileLock;
use wt_runtime::Confirmed;
use wt_tui::Board;
use wt_vcs::WorktreeSnapshot;

use crate::{
    automation_engine::RequirementState,
    context::AppContext,
    local_source::Metadata,
    session_activity::SessionActivitySnapshot,
    session_source::{SessionDiscoveries, SessionInventory},
};

#[derive(Clone, Debug, Default)]
pub struct PreparedAutomationFacts {
    pub rows: Vec<AutomationRow>,
    pub context: AutomationEvalContext,
    pub requirements: BTreeMap<String, RequirementState>,
    pub quiescence: BTreeMap<String, crate::automation_engine::WorktreeQuiescence>,
    pub activity: crate::automation_engine::RuntimeActivity,
    /// Successful live tips to persist only after a branch-advanced action starts.
    pub initial_branch_tips: BTreeMap<String, String>,
    pub state_ready: bool,
    pub globally_paused: bool,
    /// The PR facts above came from this process's confirmed, fresh GitHub
    /// leaf. Without this witness, absent row PR values remain unknown.
    pub github_fresh: bool,
    /// Exact branch set covered by that GitHub read.
    pub github_covered_branches: BTreeSet<String>,
}

/// GitHub evidence older than this cannot prove a merge or drive a fire.
pub const GITHUB_FRESH_WITHIN: std::time::Duration = std::time::Duration::from_secs(90);

/// Confirmed GitHub data recent enough for automation decisions. Restored,
/// failed-first or stale data is never evidence.
pub fn fresh_github(github: Option<Confirmed<GithubData>>) -> Option<Confirmed<GithubData>> {
    github.filter(|github| github.fresh_within(GITHUB_FRESH_WITHIN))
}

/// Facts that drive actions come only from confirmed leaves: data this
/// process fetched live, never restored cache or optimistic projections.
pub struct AutomationInputs<'a> {
    pub git: Option<&'a Confirmed<Vec<WorktreeSnapshot>>>,
    pub metadata: Option<&'a Confirmed<Metadata>>,
    /// Both the Git and metadata lanes report Ready (not mid-refresh or failed).
    pub lanes_ready: bool,
    /// Already filtered by `fresh_github`.
    pub github: Option<&'a Confirmed<GithubData>>,
    pub discoveries: Option<&'a Confirmed<SessionDiscoveries>>,
    pub inventory: Option<&'a Confirmed<SessionInventory>>,
    pub activity: Option<&'a SessionActivitySnapshot>,
    /// Display board, used only for the presentation-level `deployed` hint.
    pub board: Option<&'a Board>,
    pub last_edit_at_ms: &'a BTreeMap<String, i64>,
    pub rules: &'a [AutomationDef],
    pub lifecycle: &'a BTreeMap<String, LifecycleFacts>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LifecycleFacts {
    pub local_merged: Option<bool>,
    pub clean_candidate: bool,
}

#[derive(Default)]
pub struct PreparedLifecycleFacts {
    pub rows: BTreeMap<String, LifecycleFacts>,
    pub errors: BTreeMap<String, String>,
}

pub async fn lifecycle_facts(
    ctx: &AppContext,
    git: Option<&Confirmed<Vec<WorktreeSnapshot>>>,
    metadata: Option<&Confirmed<Metadata>>,
    github: Option<&Confirmed<GithubData>>,
    rules: &[AutomationDef],
) -> Result<PreparedLifecycleFacts> {
    let required = rules.iter().any(|rule| {
        matches!(
            rule.on,
            wt_config::AutomationTrigger::WtMerged
                | wt_config::AutomationTrigger::StackParentMerged
                | wt_config::AutomationTrigger::StatusVerificationOverdue
                | wt_config::AutomationTrigger::WtCreated
        )
    });
    if !required {
        return Ok(PreparedLifecycleFacts::default());
    }
    let (Some(snapshots), Some(metadata)) = (git, metadata) else {
        return Ok(PreparedLifecycleFacts::default());
    };
    let (state, _) = &**metadata;
    let rows = snapshots
        .iter()
        .map(|snapshot| snapshot.worktree.clone())
        .collect();
    let plans = crate::lifecycle_ops::plan_automation_facts(ctx, rows, state, github)
        .await
        .context("prepare automation lifecycle evidence")?;
    Ok(PreparedLifecycleFacts {
        errors: plans.row_errors,
        rows: plans
            .rows
            .into_iter()
            .map(|plan| {
                (
                    plan.row.target.slug().to_owned(),
                    LifecycleFacts {
                        local_merged: plan.local_merged.then_some(true),
                        clean_candidate: plan.landed && plan.hazards.is_empty(),
                    },
                )
            })
            .collect(),
    })
}

pub async fn prepare(
    ctx: &AppContext,
    input: AutomationInputs<'_>,
) -> Result<PreparedAutomationFacts> {
    let now_ms = epoch_ms();
    let Some(git) = input.git else {
        return Ok(PreparedAutomationFacts::default());
    };
    let Some(metadata) = input.metadata else {
        return Ok(PreparedAutomationFacts::default());
    };
    let (state, archived_keys) = &**metadata;
    // A failed, stale or restored GitHub source is never allowed to prove a
    // PR merge.
    let github_fresh = input.github.is_some();
    let github_covered_branches = input
        .github
        .map(|github| github.covered_branches.clone())
        .unwrap_or_default();
    let empty = GithubData::default();
    let github = input.github.map_or(&empty, |github| &**github);
    let records = git
        .iter()
        .map(|snapshot| snapshot.worktree.clone())
        .collect::<Vec<_>>();
    let members = records
        .iter()
        .filter(|row| !row.is_main && !row.target.branch.is_empty())
        .map(|row| {
            ChainMember::new(
                row.target.slug(),
                row.target.branch.clone(),
                state["slugs"][row.target.slug()]["baseBranch"]
                    .as_str()
                    .map(str::to_owned),
            )
        })
        .collect::<Vec<_>>();
    let stacks = build_stack_index(&members, &ctx.config.branch.base);
    let mut rows = Vec::with_capacity(records.len());
    let mut quiescence = BTreeMap::new();
    let mut paused_slugs = BTreeSet::new();
    for (slug, record) in state["slugs"].as_object().into_iter().flatten() {
        if record["automationsPaused"] == true {
            paused_slugs.insert(slug.clone());
        }
    }
    for removed in state["removed"].as_array().into_iter().flatten() {
        if removed["automationsPaused"] == true
            && let Some(slug) = removed["slug"].as_str()
        {
            paused_slugs.insert(slug.to_owned());
        }
    }
    let paused_stack_ids = state["pausedStacks"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    for record in records.iter().filter(|record| !record.is_main) {
        let slug = record.target.slug().to_owned();
        let branch = record.target.branch.clone();
        let stack_entry = stacks.by_branch.get(&branch);
        let stack = stack_entry.and_then(|entry| {
            let layout = stacks.layouts.get(entry.layout_index)?;
            let parent = entry.node.parent_branch.as_ref().map(|branch| StackParent {
                branch: branch.clone(),
                slug: members
                    .iter()
                    .find(|member| member.branch == *branch)
                    .map(|member| member.slug.clone()),
            });
            Some(AutomationStack {
                id: layout.stack_id.clone(),
                parent,
            })
        });
        if stack
            .as_ref()
            .is_some_and(|stack| paused_stack_ids.contains(&stack.id))
        {
            paused_slugs.insert(slug.clone());
        }
        let stored = &state["slugs"][&slug];
        let work = serde_json::from_value::<WorkStatusRecord>(stored["work"].clone()).ok();
        let pr = github.prs.get(&branch).map(|pr| AutomationPr {
            number: pr.number,
            state: pr.state.clone(),
            head_sha: pr.head_ref_oid.clone(),
            is_draft: pr.is_draft,
            checks_failed: pr.checks == PrChecks::Fail,
            failed_checks: pr.failed_checks.clone(),
            review_bot_unresolved: pr.review_bot.as_ref().map_or(0, |bot| bot.unresolved),
            changes_requested: pr.review == PrReview::ChangesRequested,
        });
        let conflict = github.prs.get(&branch).and_then(|pr| {
            pr.mergeable.as_ref().map(|mergeable| AutomationConflict {
                base: pr.base_ref_name.clone(),
                conflicted: mergeable == "CONFLICTING",
            })
        });
        let issue_override = stored["issueId"].as_str();
        let issue_id = crate::issue_identity::resolve(&slug, issue_override);
        let github_issue = stored["ghIssue"].as_u64();
        let lifecycle = input.lifecycle.get(&slug).copied().unwrap_or_default();
        let clean_candidate = lifecycle.clean_candidate;
        let local_merged = lifecycle.local_merged;
        let locked = match FileLock::try_acquire(
            &ctx.config.paths.lock_dir,
            &slug,
            "automation probe",
        )
        .await
        {
            Ok(Some(lock)) => {
                drop(lock);
                record.locked
            }
            Ok(None) => true,
            Err(error) => {
                tracing::warn!(slug, %error, "cannot probe worktree lock for automation");
                true
            }
        };
        let session_busy = session_busy(input.discoveries, input.inventory, &slug);
        let action_running = action_running(input.inventory, &slug);
        let row = AutomationRow {
            slug: slug.clone(),
            branch: branch.clone(),
            archived: archived_keys.contains(&wt_core::worktree_target_key(&record.target)),
            busy: locked,
            created_at: stored["createdAt"].as_str().map(str::to_owned),
            local_merged,
            gone: None,
            clean_candidate,
            pr,
            conflict,
            work,
            github_issue,
            issue_id,
            stack,
        };
        let last_edit_at_ms = input
            .last_edit_at_ms
            .get(&slug)
            .copied()
            .into_iter()
            .chain(latest_session_activity_ms(input.activity, &slug))
            .max();
        quiescence.insert(
            slug.clone(),
            crate::automation_engine::WorktreeQuiescence {
                locked,
                action_running,
                session_busy,
                last_edit_at_ms,
            },
        );
        rows.push(row);
    }
    let branch_tips = branch_tips(ctx, state, input.rules).await?;
    let active_slug_set = rows
        .iter()
        .map(|row| row.slug.as_str())
        .collect::<BTreeSet<_>>();
    let busy_sessions = busy_sessions(input.discoveries, &active_slug_set);
    let headless_running = headless_running(input.inventory, &active_slug_set);
    let actions = ctx
        .config
        .actions
        .iter()
        .map(|definition| {
            let audience = if definition.kind == ActionKind::Claude {
                match definition.target {
                    ActionTarget::Session => ActionAudience::Session,
                    ActionTarget::Manager => ActionAudience::Manager,
                    _ => ActionAudience::None,
                }
            } else {
                ActionAudience::None
            };
            (
                definition.id.clone(),
                ActionTraits {
                    shell: definition.kind == ActionKind::Shell,
                    external: definition.external,
                    audience,
                },
            )
        })
        .collect::<BTreeMap<_, _>>();
    let row_vars = rows
        .iter()
        .filter_map(|row| {
            let rule = ctx.config.actions.iter().find(|action| action.external);
            rule.map(|definition| {
                let base = row
                    .stack
                    .as_ref()
                    .and_then(|stack| stack.parent.as_ref())
                    .map(|parent| parent.branch.as_str())
                    .unwrap_or(&ctx.config.branch.base);
                let branch = row.branch.as_str();
                let path = records
                    .iter()
                    .find(|record| record.target.slug() == row.slug)
                    .map(|record| record.target.path.clone())
                    .unwrap_or_default();
                let stage = records
                    .iter()
                    .find(|record| record.target.slug() == row.slug)
                    .map(|record| record.target.stage.clone())
                    .unwrap_or_default();
                let vars = BTreeMap::from([
                    ("base".into(), base.to_owned()),
                    ("base_branch".into(), base.to_owned()),
                    ("branch".into(), branch.to_owned()),
                    ("slug".into(), row.slug.clone()),
                    ("cwd".into(), path),
                    ("issue_id".into(), row.issue_id.clone().unwrap_or_default()),
                    ("stage".into(), stage),
                    ("skill_prefix".into(), primary_skill_prefix(ctx)),
                    (
                        "pr".into(),
                        row.pr
                            .as_ref()
                            .map_or_else(String::new, |pr| pr.number.to_string()),
                    ),
                ]);
                let _ = definition;
                (row.slug.clone(), vars)
            })
        })
        .collect();
    let mut context = AutomationEvalContext {
        github_fresh,
        now_ms,
        local_day: OffsetDateTime::now_utc().date().to_string(),
        base_branch: ctx.config.branch.base.clone(),
        reviewers_enabled: ctx.config.github.reviewers,
        actions,
        pauses: PauseSnapshot {
            global: state["automationsPaused"] == true,
            slugs: paused_slugs,
            stack_ids: paused_stack_ids,
        },
        branch_tips: branch_tips.tips,
        row_vars,
    };
    context.local_day = OffsetDateTime::now_utc().date().to_string();
    let fires = evaluate(&ctx.config.automations, &rows, &context);
    let requirements = fires
        .iter()
        .map(|fire| {
            let deployed = input
                .board
                .and_then(|board| board.rows.iter().find(|row| row.slug == fire.slug))
                .is_some_and(|row| row.stage_url.is_some());
            let result = requirement_state(
                &ctx.config.actions,
                ctx.config.issue_tracker.as_ref(),
                fire,
                &rows,
                deployed,
                github_fresh,
                &github_covered_branches,
            );
            (fire_identity(fire), result)
        })
        .collect();
    let globally_paused = context.pauses.global;
    let initial_branch_tips = branch_tips.newly_observed;
    Ok(PreparedAutomationFacts {
        rows,
        context,
        requirements,
        quiescence,
        activity: crate::automation_engine::RuntimeActivity {
            headless_running,
            busy_sessions,
        },
        initial_branch_tips,
        state_ready: input.lanes_ready,
        globally_paused,
        github_fresh,
        github_covered_branches,
    })
}

fn latest_session_activity_ms(
    snapshot: Option<&SessionActivitySnapshot>,
    slug: &str,
) -> Option<i64> {
    let snapshot = snapshot?;
    snapshot
        .events
        .iter()
        .filter(|event| event.slug == slug)
        .map(|event| event.timestamp_ms)
        .chain(
            snapshot
                .tails
                .iter()
                .filter(|tail| tail.key.slug == slug)
                .flat_map(|tail| tail.lines.iter().map(|line| line.timestamp_ms)),
        )
        .max()
}

/// Refresh only row-backed action requirements when presentation deployment
/// facts change. This avoids repeating lifecycle Git and GitHub probes for a
/// board-only update.
pub fn refresh_requirements(
    ctx: &AppContext,
    facts: &mut PreparedAutomationFacts,
    board: Option<&Board>,
) {
    let fires = evaluate(&ctx.config.automations, &facts.rows, &facts.context);
    facts.requirements = fires
        .iter()
        .map(|fire| {
            let deployed = board
                .and_then(|board| board.rows.iter().find(|row| row.slug == fire.slug))
                .is_some_and(|row| row.stage_url.is_some());
            (
                fire_identity(fire),
                requirement_state(
                    &ctx.config.actions,
                    ctx.config.issue_tracker.as_ref(),
                    fire,
                    &facts.rows,
                    deployed,
                    facts.github_fresh,
                    &facts.github_covered_branches,
                ),
            )
        })
        .collect();
}

/// Session facts come only from confirmed discoveries and inventory. Until
/// both are live (e.g. right after a restart that restored last session
/// states), every slug is busy so nothing is injected twice.
pub fn refresh_sessions(
    facts: &mut PreparedAutomationFacts,
    discoveries: Option<&Confirmed<SessionDiscoveries>>,
    inventory: Option<&Confirmed<SessionInventory>>,
) {
    for row in &facts.rows {
        let quiescence = facts.quiescence.entry(row.slug.clone()).or_default();
        quiescence.session_busy = session_busy(discoveries, inventory, &row.slug);
        quiescence.action_running = action_running(inventory, &row.slug);
    }
    let active_slugs = facts
        .rows
        .iter()
        .map(|row| row.slug.as_str())
        .collect::<BTreeSet<_>>();
    facts.activity.busy_sessions = busy_sessions(discoveries, &active_slugs);
    facts.activity.headless_running = headless_running(inventory, &active_slugs);
}

fn session_working(discovered: &crate::session_source::DiscoveredSession) -> bool {
    discovered.session.is_live
        && discovered.session.extras.derived_state.is_none_or(|state| {
            !matches!(
                state,
                wt_harness::DerivedState::Waiting
                    | wt_harness::DerivedState::Idle
                    | wt_harness::DerivedState::Abandoned
            )
        })
}

/// Unknown session state keeps every delivery slot occupied.
fn busy_sessions(
    discoveries: Option<&Confirmed<SessionDiscoveries>>,
    active: &BTreeSet<&str>,
) -> BTreeSet<String> {
    let relevant = |slug: &str| active.contains(slug) || slug == "manager";
    match discoveries {
        Some(discoveries) => discoveries
            .iter()
            .filter(|discovered| session_working(discovered))
            .map(|discovered| discovered.key.slug.clone())
            .filter(|slug| relevant(slug))
            .collect(),
        None => active
            .iter()
            .map(|slug| (*slug).to_owned())
            .chain(std::iter::once("manager".to_owned()))
            .collect(),
    }
}

/// Unknown tmux inventory keeps every headless slot occupied.
fn headless_running(
    inventory: Option<&Confirmed<SessionInventory>>,
    active: &BTreeSet<&str>,
) -> BTreeSet<String> {
    match inventory {
        Some(inventory) => inventory.kinds.action.clone(),
        None => active.iter().map(|slug| (*slug).to_owned()).collect(),
    }
}

fn action_running(inventory: Option<&Confirmed<SessionInventory>>, slug: &str) -> bool {
    inventory.is_none_or(|inventory| inventory.kinds.action.contains(slug))
}

pub fn refresh_quiescence_edits(
    facts: &mut PreparedAutomationFacts,
    edits: &BTreeMap<String, i64>,
    activity: Option<&SessionActivitySnapshot>,
) {
    for (slug, state) in &mut facts.quiescence {
        state.last_edit_at_ms = edits
            .get(slug)
            .copied()
            .into_iter()
            .chain(latest_session_activity_ms(activity, slug))
            .max();
    }
}

struct BranchTipFacts {
    tips: BTreeMap<String, BranchTip>,
    newly_observed: BTreeMap<String, String>,
}

async fn branch_tips(
    ctx: &AppContext,
    state: &Value,
    rules: &[AutomationDef],
) -> Result<BranchTipFacts> {
    let watched = rules
        .iter()
        .filter(|rule| rule.on == wt_config::AutomationTrigger::BranchAdvanced)
        .filter_map(|rule| rule.branch.as_deref())
        .collect::<BTreeSet<_>>();
    let mut tips = BTreeMap::new();
    let mut newly_observed = BTreeMap::new();
    for branch in watched {
        let output = crate::commands::resolve::run_git(
            ctx,
            &ctx.config.paths.main_clone,
            [
                "rev-parse",
                "--verify",
                &format!("refs/remotes/origin/{branch}"),
            ],
        )
        .await?;
        if !output.status.success() {
            continue;
        }
        let now = output.stdout_text().trim().to_owned();
        if now.is_empty() {
            continue;
        }
        let seen = state["branchTips"][branch].as_str().map(str::to_owned);
        if seen.is_none() {
            newly_observed.insert(branch.to_owned(), now.clone());
        }
        tips.insert(branch.to_owned(), BranchTip { now, seen });
    }
    Ok(BranchTipFacts {
        tips,
        newly_observed,
    })
}

fn requirement_state(
    actions: &[wt_config::ActionDef],
    issue_tracker: Option<&wt_config::IssueTrackerConfig>,
    fire: &AutomationFire,
    rows: &[AutomationRow],
    deployed: bool,
    github_confirmed: bool,
    github_covered_branches: &BTreeSet<String>,
) -> RequirementState {
    // Builtins are validated by configuration and own their safety checks at
    // execution. They are not configurable actions and must not consume their
    // fire as a permanent skip just because no [[actions]] entry exists.
    if fire.rule.run.starts_with("builtin:") {
        return RequirementState::Ready;
    }
    let Some(definition) = actions.iter().find(|action| action.id == fire.rule.run) else {
        return RequirementState::FrozenUnmet(format!(
            "action {:?} is not configured",
            fire.rule.run
        ));
    };
    let Some(row) = rows.iter().find(|row| row.slug == fire.slug) else {
        return if fire.frozen_vars.is_some() {
            RequirementState::Ready
        } else {
            RequirementState::FrozenUnmet("worktree is no longer available".into())
        };
    };
    let action_row = wt_actions::ActionRow {
        slug: row.slug.clone(),
        issue_id: row.issue_id.clone(),
        issue_prefix: issue_tracker.and_then(|issue| issue.prefix.clone()),
        pr: match (
            github_confirmed && github_covered_branches.contains(&row.branch),
            row.pr.as_ref(),
        ) {
            (false, _) => wt_actions::ActionPrObservation::Unknown,
            (true, None) => wt_actions::ActionPrObservation::ConfirmedAbsent,
            (true, Some(pr)) => wt_actions::ActionPrObservation::Present(wt_actions::ActionPr {
                state: pr.state.clone(),
                is_draft: pr.is_draft,
            }),
        },
        pr_number: row.pr.as_ref().map(|pr| pr.number.to_string()),
        deployed,
    };
    match wt_actions::evaluate_requirements(&definition.requires, &action_row) {
        Some(reason) if fire.frozen_vars.is_some() => RequirementState::FrozenUnmet(reason),
        Some(_) => RequirementState::Waiting,
        None => RequirementState::Ready,
    }
}

/// Quiescence requires confirmed discoveries and inventory (judge E1):
/// restored or not-yet-fetched session state makes the slug busy.
fn session_busy(
    discoveries: Option<&Confirmed<SessionDiscoveries>>,
    inventory: Option<&Confirmed<SessionInventory>>,
    slug: &str,
) -> bool {
    let (Some(discoveries), Some(inventory)) = (discoveries, inventory) else {
        return true;
    };
    let has_live = {
        inventory.kinds.claude.iter().any(|slot| slot.slug == slug)
            || inventory.kinds.codex.contains(slug)
            || inventory.kinds.opencode.contains(slug)
    };
    if !has_live {
        return false;
    }
    let mut observed = false;
    let mut busy = false;
    for discovered in discoveries
        .iter()
        .filter(|item| item.key.slug == slug && item.session.is_live)
    {
        observed = true;
        busy |= session_working(discovered);
    }
    busy || !observed
}

fn primary_skill_prefix(ctx: &AppContext) -> String {
    match ctx.config.harness.primary {
        wt_config::HarnessId::Claude => "/".into(),
        wt_config::HarnessId::Codex | wt_config::HarnessId::Opencode => "$".into(),
    }
}

fn epoch_ms() -> i64 {
    i64::try_from(OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_source::{DiscoveredSession, SessionKey, SessionKinds};
    use wt_core::Domain;
    use wt_runtime::{
        Persistence, RefreshPolicy, SourceSnapshot, SourceSpec, TaskScope, leaf_channel,
    };

    #[test]
    fn builtins_do_not_require_configured_actions_or_a_surviving_checkout() {
        let mut fire = AutomationFire {
            rule: AutomationDef::default(),
            slug: "landed".into(),
            quiesce_slugs: vec![],
            fire_keys: vec![],
            stack_id: None,
            close_issue: None,
            delete_branch: Some("feature".into()),
            delete_branch_pr: Some(2451),
            branch_range: None,
            frozen_vars: None,
            frozen_pr: None,
            detail: String::new(),
        };
        for run in [
            "builtin:clean",
            "builtin:restack",
            "builtin:notify",
            "builtin:close-issue",
            "builtin:delete-branch",
        ] {
            fire.rule.run = run.into();
            assert_eq!(
                requirement_state(&[], None, &fire, &[], false, false, &BTreeSet::new()),
                RequirementState::Ready,
                "{run} must reach its own execution-time safety checks",
            );
        }
        fire.rule.run = "unknown-custom-action".into();
        assert!(matches!(
            requirement_state(&[], None, &fire, &[], false, false, &BTreeSet::new()),
            RequirementState::FrozenUnmet(reason) if reason.contains("not configured")
        ));

        let actions = [wt_config::ActionDef {
            id: fire.rule.run.clone(),
            requires: vec![wt_config::RequireTag::IssueTracker],
            ..Default::default()
        }];
        let rows = [AutomationRow {
            slug: fire.slug.clone(),
            ..Default::default()
        }];
        assert_eq!(
            requirement_state(&actions, None, &fire, &rows, false, false, &BTreeSet::new(),),
            RequirementState::Waiting,
        );
        fire.frozen_vars = Some(BTreeMap::new());
        assert!(matches!(
            requirement_state(&actions, None, &fire, &rows, false, false, &BTreeSet::new(),),
            RequirementState::FrozenUnmet(_)
        ));
    }

    #[tokio::test]
    async fn one_broken_checkout_does_not_suppress_other_merged_automations() {
        use crate::commands::test_support::CommandFixture;
        use wt_platform::process::CommandSpec;
        let mut fixture = CommandFixture::new().await.unwrap();
        let rule = AutomationDef {
            id: "delete-landed-branch".into(),
            on: wt_config::AutomationTrigger::WtMerged,
            run: "builtin:delete-branch".into(),
            ..Default::default()
        };
        std::sync::Arc::make_mut(&mut fixture.ctx.config).automations = vec![rule];
        let ctx = &fixture.ctx;
        async fn git(ctx: &AppContext, args: &[&str]) -> String {
            ctx.processes
                .run(
                    CommandSpec::new("git")
                        .args(args.iter().copied())
                        .cwd(&ctx.cwd),
                    &ctx.cancellation,
                )
                .await
                .unwrap()
                .checked("fixture git")
                .unwrap()
                .stdout_text()
                .trim()
                .into()
        }
        let origin = fixture._root.path().join("origin.git");
        git(ctx, &["init", "--bare", origin.to_str().unwrap()]).await;
        git(ctx, &["remote", "add", "origin", origin.to_str().unwrap()]).await;
        let base = git(ctx, &["rev-parse", "HEAD"]).await;
        ctx.database
            .call(move |store| {
                store.set_slug_base("one", Some(("main", Some(&base))))?;
                Ok(())
            })
            .await
            .unwrap();
        std::fs::write(ctx.cwd.join("tracked.txt"), "landed work\n").unwrap();
        git(ctx, &["commit", "-am", "landed"]).await;
        git(ctx, &["push", "origin", "HEAD:refs/heads/main"]).await;
        let mut snapshots = ctx
            .repository
            .inventory_status(&ctx.cancellation)
            .await
            .unwrap();
        snapshots
            .iter_mut()
            .find(|row| row.worktree.target.slug() == "two")
            .unwrap()
            .worktree
            .target
            .path = fixture
            ._root
            .path()
            .join("missing-checkout")
            .to_string_lossy()
            .into();
        // Put the broken row first to catch whole-loop early returns.
        snapshots.sort_by_key(|row| row.worktree.target.slug() != "two");
        let git = Confirmed::for_test(snapshots);
        let state = ctx
            .database
            .call(|store| Ok(store.read_wt_state()?))
            .await
            .unwrap();
        let metadata = Confirmed::for_test((state, BTreeSet::new()));
        let evidence = lifecycle_facts(
            ctx,
            Some(&git),
            Some(&metadata),
            None,
            &ctx.config.automations,
        )
        .await
        .unwrap();
        assert!(evidence.errors.contains_key("two"));
        assert!(!evidence.rows.contains_key("two"));
        assert!(evidence.rows["one"].clean_candidate);
        let edits = BTreeMap::new();
        let prepared = prepare(
            ctx,
            AutomationInputs {
                git: Some(&git),
                metadata: Some(&metadata),
                lanes_ready: true,
                github: None,
                discoveries: None,
                inventory: None,
                activity: None,
                board: None,
                last_edit_at_ms: &edits,
                rules: &ctx.config.automations,
                lifecycle: &evidence.rows,
            },
        )
        .await
        .unwrap();
        let broken = prepared.rows.iter().find(|row| row.slug == "two").unwrap();
        assert_eq!(broken.local_merged, None);
        assert!(!broken.clean_candidate);
        let fires = evaluate(&ctx.config.automations, &prepared.rows, &prepared.context);
        assert_eq!(fires.len(), 1);
        assert_eq!(fires[0].slug, "one");
        assert_eq!(
            prepared.requirements[&fire_identity(&fires[0])],
            RequirementState::Ready
        );
        let rows = git
            .iter()
            .map(|snapshot| snapshot.worktree.clone())
            .collect();
        assert!(
            crate::lifecycle_ops::plan_with_facts(ctx, rows, &metadata.0, None, None)
                .await
                .is_err()
        );
        fixture.close().await.unwrap();
    }

    fn idle_session(slug: &str) -> DiscoveredSession {
        DiscoveredSession {
            key: SessionKey {
                slug: slug.into(),
                harness: wt_core::HarnessId::Codex,
                session_id: "s1".into(),
            },
            session: wt_harness::HarnessSession {
                display_name: slug.into(),
                session_id: "s1".into(),
                tmux_session_name: slug.into(),
                last_active_ms: None,
                is_live: true,
                extras: wt_harness::HarnessExtras {
                    derived_state: Some(wt_harness::DerivedState::Idle),
                    ..Default::default()
                },
            },
        }
    }

    #[tokio::test]
    async fn restored_session_discoveries_keep_the_slug_busy() {
        let scope = TaskScope::new();
        let (discoveries, _publisher) = leaf_channel::<SessionDiscoveries>(
            &scope,
            SourceSpec {
                name: "session-discoveries",
                domain: Domain::Sessions,
                persistence: Persistence::Never("test fixture"),
                policy: RefreshPolicy::default(),
            },
        );
        let idle = vec![idle_session("one")];
        // Restored at launch: the last known state says Idle, but this
        // process has not observed it, so it cannot authorize injection.
        let restored = SourceSnapshot::test_restored(idle.clone(), Domain::Sessions, 1);
        let unconfirmed = discoveries.confirm(&restored);
        assert!(unconfirmed.is_none());
        let inventory = Confirmed::for_test(SessionInventory {
            kinds: SessionKinds {
                codex: ["one".to_owned()].into(),
                ..Default::default()
            },
            ..Default::default()
        });
        let mut facts = PreparedAutomationFacts {
            rows: vec![AutomationRow {
                slug: "one".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        refresh_sessions(&mut facts, unconfirmed.as_ref(), Some(&inventory));
        assert!(facts.quiescence["one"].session_busy);
        assert!(facts.activity.busy_sessions.contains("one"));

        // Unconfirmed inventory is equally unknown.
        let confirmed = Confirmed::for_test(idle);
        refresh_sessions(&mut facts, Some(&confirmed), None);
        assert!(facts.quiescence["one"].session_busy);
        assert!(facts.quiescence["one"].action_running);
        assert!(facts.activity.headless_running.contains("one"));

        // Both observed live: the idle session is quiescent.
        refresh_sessions(&mut facts, Some(&confirmed), Some(&inventory));
        assert!(!facts.quiescence["one"].session_busy);
        assert!(!facts.quiescence["one"].action_running);
        assert!(facts.activity.busy_sessions.is_empty());
        scope
            .shutdown(std::time::Duration::from_secs(1))
            .await
            .unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn stale_github_evidence_is_not_fresh() {
        let github = Confirmed::for_test(GithubData::default());
        assert!(fresh_github(Some(github.clone())).is_some());
        tokio::time::advance(GITHUB_FRESH_WITHIN + std::time::Duration::from_secs(1)).await;
        assert!(fresh_github(Some(github)).is_none());
        assert!(fresh_github(None).is_none());
    }
}
