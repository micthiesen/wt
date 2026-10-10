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
use wt_runtime::{SourceSnapshot, SourceState};
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
}

pub struct AutomationInputs<'a> {
    pub git: &'a SourceSnapshot<Vec<WorktreeSnapshot>>,
    pub metadata: &'a SourceSnapshot<Metadata>,
    pub github: &'a SourceSnapshot<GithubData>,
    pub discoveries: &'a SourceSnapshot<SessionDiscoveries>,
    pub inventory: &'a SourceSnapshot<SessionInventory>,
    pub activity: &'a SourceSnapshot<SessionActivitySnapshot>,
    pub board: &'a SourceSnapshot<Board>,
    pub last_edit_at_ms: &'a BTreeMap<String, i64>,
    pub rules: &'a [AutomationDef],
    pub lifecycle: &'a BTreeMap<String, LifecycleFacts>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LifecycleFacts {
    pub local_merged: Option<bool>,
    pub clean_candidate: bool,
}

pub async fn lifecycle_facts(
    ctx: &AppContext,
    git: &SourceSnapshot<Vec<WorktreeSnapshot>>,
    metadata: &SourceSnapshot<Metadata>,
    github: &GithubData,
    rules: &[AutomationDef],
) -> Result<BTreeMap<String, LifecycleFacts>> {
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
        return Ok(BTreeMap::new());
    }
    if git.state != SourceState::Ready || metadata.state != SourceState::Ready {
        return Ok(BTreeMap::new());
    }
    let (Some(snapshots), Some((state, _))) = (git.data.as_deref(), metadata.data.as_deref())
    else {
        return Ok(BTreeMap::new());
    };
    let rows = snapshots
        .iter()
        .map(|snapshot| snapshot.worktree.clone())
        .collect();
    let plans = crate::lifecycle_ops::plan_with_facts(ctx, rows, state, github, None)
        .await
        .context("prepare automation lifecycle evidence")?;
    Ok(plans
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
        .collect())
}

pub async fn prepare(
    ctx: &AppContext,
    input: AutomationInputs<'_>,
) -> Result<PreparedAutomationFacts> {
    let now_ms = epoch_ms();
    let Some(git) = input.git.data.as_deref() else {
        return Ok(PreparedAutomationFacts::default());
    };
    let Some((state, archived_keys)) = input.metadata.data.as_deref() else {
        return Ok(PreparedAutomationFacts::default());
    };
    let github_fresh = input.github.state == SourceState::Ready
        && input
            .github
            .updated_at
            .is_some_and(|at| at.elapsed() <= std::time::Duration::from_secs(90));
    // A failed or stale GitHub source is never allowed to prove a PR merge.
    let github = if github_fresh {
        input.github.data.as_deref().cloned().unwrap_or_default()
    } else {
        GithubData::default()
    };
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
        let session_busy = session_busy(
            input.discoveries.data.as_deref(),
            input.inventory.data.as_deref(),
            &slug,
        );
        let action_running = input
            .inventory
            .data
            .as_deref()
            .is_some_and(|inventory| inventory.kinds.action.contains(&slug));
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
            .chain(latest_session_activity_ms(
                input.activity.data.as_deref(),
                &slug,
            ))
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
    let busy_sessions = input
        .discoveries
        .data
        .as_deref()
        .into_iter()
        .flatten()
        .filter(|discovered| {
            discovered.session.is_live
                && discovered.session.extras.derived_state.is_none_or(|state| {
                    !matches!(
                        state,
                        wt_harness::DerivedState::Waiting
                            | wt_harness::DerivedState::Idle
                            | wt_harness::DerivedState::Abandoned
                    )
                })
        })
        .map(|discovered| discovered.key.slug.clone())
        .filter(|slug| active_slug_set.contains(slug.as_str()) || slug == "manager")
        .collect::<BTreeSet<_>>();
    let headless_running = input
        .inventory
        .data
        .as_deref()
        .map(|inventory| inventory.kinds.action.clone())
        .unwrap_or_default();
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
                .data
                .as_deref()
                .and_then(|board| board.rows.iter().find(|row| row.slug == fire.slug))
                .is_some_and(|row| row.stage_url.is_some());
            let result = requirement_state(ctx, fire, &rows, deployed);
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
        state_ready: input.metadata.state == SourceState::Ready
            && input.git.state == SourceState::Ready,
        globally_paused,
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
                requirement_state(ctx, fire, &facts.rows, deployed),
            )
        })
        .collect();
}

pub fn refresh_sessions(
    facts: &mut PreparedAutomationFacts,
    discoveries: Option<&SessionDiscoveries>,
    inventory: Option<&SessionInventory>,
) {
    let mut busy_sessions = BTreeSet::new();
    for row in &facts.rows {
        let quiescence = facts.quiescence.entry(row.slug.clone()).or_default();
        quiescence.session_busy = session_busy(discoveries, inventory, &row.slug);
        quiescence.action_running =
            inventory.is_some_and(|inventory| inventory.kinds.action.contains(&row.slug));
    }
    let active_slugs = facts
        .rows
        .iter()
        .map(|row| row.slug.as_str())
        .collect::<BTreeSet<_>>();
    for discovered in discoveries.into_iter().flatten().filter(|discovered| {
        discovered.session.is_live
            && discovered.session.extras.derived_state.is_none_or(|state| {
                !matches!(
                    state,
                    wt_harness::DerivedState::Waiting
                        | wt_harness::DerivedState::Idle
                        | wt_harness::DerivedState::Abandoned
                )
            })
    }) {
        if active_slugs.contains(discovered.key.slug.as_str()) || discovered.key.slug == "manager" {
            busy_sessions.insert(discovered.key.slug.clone());
        }
    }
    facts.activity.busy_sessions = busy_sessions;
    facts.activity.headless_running = inventory
        .map(|inventory| inventory.kinds.action.clone())
        .unwrap_or_default();
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
    ctx: &AppContext,
    fire: &AutomationFire,
    rows: &[AutomationRow],
    deployed: bool,
) -> RequirementState {
    let Some(definition) = ctx
        .config
        .actions
        .iter()
        .find(|action| action.id == fire.rule.run)
    else {
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
        issue_prefix: ctx
            .config
            .issue_tracker
            .as_ref()
            .and_then(|issue| issue.prefix.clone()),
        pr: row.pr.as_ref().map(|pr| wt_actions::ActionPr {
            state: pr.state.clone(),
            is_draft: pr.is_draft,
        }),
        pr_number: row.pr.as_ref().map(|pr| pr.number.to_string()),
        deployed,
    };
    match wt_actions::evaluate_requirements(&definition.requires, &action_row) {
        Some(reason) if fire.frozen_vars.is_some() => RequirementState::FrozenUnmet(reason),
        Some(_) => RequirementState::Waiting,
        None => RequirementState::Ready,
    }
}

fn session_busy(
    discoveries: Option<&SessionDiscoveries>,
    inventory: Option<&SessionInventory>,
    slug: &str,
) -> bool {
    let has_live = inventory.is_some_and(|inventory| {
        inventory.kinds.claude.iter().any(|slot| slot.slug == slug)
            || inventory.kinds.codex.contains(slug)
            || inventory.kinds.opencode.contains(slug)
    });
    if !has_live {
        return false;
    }
    let mut observed = false;
    let mut busy = false;
    for discovered in discoveries
        .into_iter()
        .flatten()
        .filter(|item| item.key.slug == slug && item.session.is_live)
    {
        observed = true;
        busy |= discovered.session.extras.derived_state.is_none_or(|state| {
            !matches!(
                state,
                wt_harness::DerivedState::Waiting
                    | wt_harness::DerivedState::Idle
                    | wt_harness::DerivedState::Abandoned
            )
        });
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
