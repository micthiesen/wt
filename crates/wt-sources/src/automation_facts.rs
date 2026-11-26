//! Join host-local source snapshots into the pure automation evaluator input.

use std::collections::{BTreeMap, BTreeSet};
use wt_ops::lifecycle_ops::LifecycleFacts;

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
use wt_vcs::{WorktreeRecord, WorktreeSnapshot};

use crate::{
    local_source::Metadata,
    session_activity::SessionActivitySnapshot,
    session_source::{SessionDiscoveries, SessionInventory},
};
use wt_app_core::{automation_engine::RequirementState, context::AppContext};

#[derive(Clone, Debug, Default)]
pub struct PreparedAutomationFacts {
    pub rows: Vec<AutomationRow>,
    pub context: AutomationEvalContext,
    pub requirements: BTreeMap<String, RequirementState>,
    pub quiescence: BTreeMap<String, wt_app_core::automation_engine::WorktreeQuiescence>,
    pub activity: wt_app_core::automation_engine::RuntimeActivity,
    /// Successful live tips to persist only after a branch-advanced action starts.
    pub initial_branch_tips: BTreeMap<String, String>,
    pub state_ready: bool,
    pub globally_paused: bool,
    /// The PR facts above came from this process's confirmed, fresh GitHub
    /// leaf. Without this witness, absent row PR values remain unknown.
    pub github_fresh: bool,
    /// Exact branch set covered by that GitHub read.
    pub github_covered_branches: BTreeSet<String>,
    /// What watches may ask of each live worktree. An unknown answer is
    /// `None`, never `false`.
    pub watch_rows: BTreeMap<String, wt_core::watch::RowFacts>,
    /// The fleet's own pull requests by number, from the fresh GitHub read.
    pub watch_prs: BTreeMap<u64, wt_core::watch::PrFacts>,
    /// Each live worktree's branch, for a check's `{{branch}}`.
    pub branches: BTreeMap<String, String>,
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

#[derive(Default)]
pub struct PreparedLifecycleFacts {
    pub rows: BTreeMap<String, LifecycleFacts>,
    /// Landing commits of the landed rows a watch belongs to.
    pub landing_commits: BTreeMap<String, String>,
    pub errors: BTreeMap<String, String>,
    /// A live lifecycle operation owns some evidence; retry once it settles.
    pub deferred: bool,
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
                | wt_config::AutomationTrigger::StatusVerificationDue
                | wt_config::AutomationTrigger::StatusVerificationOverdue
                | wt_config::AutomationTrigger::WtCreated
        )
    });
    let (Some(snapshots), Some(metadata)) = (git, metadata) else {
        return Ok(PreparedLifecycleFacts::default());
    };
    let (state, _) = &**metadata;
    // Without such a rule, only the worktrees a pending watch depends on
    // are worth the Git calls.
    let watched = wt_ops::lifecycle_ops::watch_wants(state);
    // wt wakes the session of a worktree whose owed check came due, with or
    // without a rule: that takes the row's landing.
    let checked = wt_ops::lifecycle_ops::check_wants(state);
    if !required && watched.is_empty() && checked.is_empty() {
        return Ok(PreparedLifecycleFacts::default());
    }
    let mut result = PreparedLifecycleFacts::default();
    let mut rows = Vec::new();
    for snapshot in snapshots.iter().filter(|snapshot| {
        let slug = snapshot.worktree.target.slug();
        !snapshot.worktree.is_main
            && (required || watched.contains_key(slug) || checked.contains_key(slug))
    }) {
        let slug = snapshot.worktree.target.slug();
        // An accepted removal owns this checkout; its files may disappear at
        // any moment. A retired target contributes no facts and no error.
        if removal_recorded(state, &snapshot.worktree) || checkout_missing(&snapshot.worktree).await
        {
            continue;
        }
        match wt_ops::commands::diagnostics::operation_lock(ctx, slug).await {
            Ok(Some(_)) => result.deferred = true,
            Ok(None) => rows.push(snapshot.worktree.clone()),
            Err(error) => {
                result.errors.insert(
                    slug.to_owned(),
                    format!("inspect lifecycle operation: {error:#}"),
                );
            }
        }
    }
    let paths: BTreeMap<String, WorktreeRecord> = rows
        .iter()
        .map(|row| (row.target.slug().to_owned(), row.clone()))
        .collect();
    let plans = wt_ops::lifecycle_ops::plan_automation_facts(ctx, rows, state, github)
        .await
        .context("prepare automation lifecycle evidence")?;
    let mut fresh_state = None;
    for (slug, error) in plans.row_errors {
        // An operation can start after the initial probe. Its changing checkout
        // contributes no facts, but an expected race is not a failed Git install.
        if let Ok(Some(_)) = wt_ops::commands::diagnostics::operation_lock(ctx, &slug).await {
            result.deferred = true;
            continue;
        }
        // A removal accepted or completed during the pass retires the target.
        if let Some(row) = paths.get(&slug) {
            if checkout_missing(row).await {
                continue;
            }
            if fresh_state.is_none() {
                fresh_state = ctx
                    .database
                    .call(|store| Ok(store.read_wt_state()?))
                    .await
                    .ok();
            }
            if fresh_state
                .as_ref()
                .is_some_and(|state| removal_recorded(state, row))
            {
                continue;
            }
        }
        result.errors.insert(slug, error);
    }
    result.rows = plans.rows;
    result.landing_commits = plans.landing_commits;
    Ok(result)
}

/// An accepted removal of this exact checkout. A reused key at another path
/// is a different checkout.
fn removal_recorded(state: &Value, row: &WorktreeRecord) -> bool {
    state["removals"][wt_core::worktree_target_key(&row.target)]["path"].as_str()
        == Some(row.target.path.as_str())
}

/// Only a confirmed absence counts; an unreadable path stays a live row.
async fn checkout_missing(row: &WorktreeRecord) -> bool {
    matches!(tokio::fs::try_exists(&row.target.path).await, Ok(false))
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
    let mut watch_rows = BTreeMap::new();
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
            auto_merge: pr.auto_merge.is_some(),
            // A missing entry proves nothing unless the read held the
            // whole queue.
            merge_queued: if github.merge_queue.contains_key(&branch) {
                Some(true)
            } else {
                github.merge_queue_complete.then_some(false)
            },
        });
        let conflict = github.prs.get(&branch).and_then(|pr| {
            pr.mergeable.as_ref().map(|mergeable| AutomationConflict {
                base: pr.base_ref_name.clone(),
                conflicted: mergeable == "CONFLICTING",
            })
        });
        let issue_override = stored["issueId"].as_str();
        let issue_id = wt_app_core::issue_identity::resolve(&slug, issue_override);
        let github_issue = stored["githubIssue"].as_u64();
        // A row with no landing evidence yet has no deploy answer either,
        // which a probed stage must not read as "no probe".
        let probed = |stage| ctx.config.environments.get(stage).is_some();
        let gathered = input.lifecycle.get(&slug).copied().map(|mut lifecycle| {
            lifecycle.deploys = lifecycle.deploys.for_probes(probed);
            lifecycle
        });
        let lifecycle = gathered.unwrap_or_default();
        let clean_candidate = lifecycle.clean_candidate;
        let local_merged = lifecycle.local_merged;
        let in_production = lifecycle.in_production;
        let deploys = lifecycle.deploys.for_probes(probed);
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
        watch_rows.insert(
            slug.clone(),
            watch_row(WatchRowInput {
                created_at: stored["createdAt"].as_str(),
                work: work.as_ref(),
                pr: github.prs.get(&branch),
                // GitHub read this branch just now: no PR is an answer.
                pr_read: github_fresh && github_covered_branches.contains(&branch),
                lifecycle: gathered,
                separate_production: ctx.config.branch.separate_production(),
            }),
        );
        let row = AutomationRow {
            slug: slug.clone(),
            branch: branch.clone(),
            archived: archived_keys.contains(&wt_core::worktree_target_key(&record.target)),
            busy: locked,
            created_at: stored["createdAt"].as_str().map(str::to_owned),
            local_merged,
            gone: None,
            in_production,
            deploys,
            clean_candidate,
            merge_arm: serde_json::from_value(stored["mergeArm"].clone()).ok(),
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
            wt_app_core::automation_engine::WorktreeQuiescence {
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
    let check_settle_seconds = wt_config::verification_due_settle_seconds(&ctx.config.environments);
    let unmet_requirements = unmet_requirements(
        &ctx.config.automations,
        &ctx.config.actions,
        ctx.config.issue_tracker.as_ref(),
        &rows,
        input.board,
        github_fresh,
        &github_covered_branches,
    );
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
        github_read_at_ms: None,
        now_ms,
        local_day: OffsetDateTime::now_utc().date().to_string(),
        base_branch: ctx.config.branch.base.clone(),
        separate_production: ctx.config.branch.separate_production(),
        reviewers_enabled: ctx.config.github.reviewers,
        actions,
        pauses: PauseSnapshot {
            global: state["automationsPaused"] == true,
            slugs: paused_slugs,
            stack_ids: paused_stack_ids,
        },
        branch_tips: branch_tips.tips,
        row_vars,
        mcp_events: wt_automations::pending_mcp_events(
            &ctx.config.automations,
            &ctx.config.mcp_servers,
            state,
        ),
        guides: rows
            .iter()
            .filter_map(|row| {
                let url = state["slugs"][&row.slug]["guide"]["url"].as_str()?;
                Some(wt_automations::GuideOwner {
                    slug: row.slug.clone(),
                    url: url.to_owned(),
                })
            })
            .collect(),
        // Set on every pass from the stored watches.
        watches: Vec::new(),
        check_settle_seconds,
        unmet_requirements,
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
        activity: wt_app_core::automation_engine::RuntimeActivity {
            headless_running,
            busy_sessions,
        },
        initial_branch_tips,
        state_ready: input.lanes_ready,
        globally_paused,
        github_fresh,
        github_covered_branches,
        watch_rows,
        watch_prs: if github_fresh {
            github
                .prs
                .values()
                .map(|pr| {
                    (
                        pr.number,
                        wt_ops::watch_probe::pr_facts(&pr.state, pr.merge_commit_oid.clone()),
                    )
                })
                .collect()
        } else {
            BTreeMap::new()
        },
        branches: records
            .iter()
            .filter(|record| !record.is_main)
            .map(|record| {
                (
                    record.target.slug().to_owned(),
                    record.target.branch.clone(),
                )
            })
            .collect(),
    })
}

struct WatchRowInput<'a> {
    created_at: Option<&'a str>,
    work: Option<&'a WorkStatusRecord>,
    pr: Option<&'a wt_github::PullRequest>,
    pr_read: bool,
    /// `None` when no landing evidence was gathered for the row.
    lifecycle: Option<LifecycleFacts>,
    separate_production: bool,
}

/// One live worktree as watches see it, from the same landing the
/// automations use. "Yes" needs proof; "no" needs both the local evidence
/// and a fresh GitHub read of the branch, and is otherwise unknown.
fn watch_row(input: WatchRowInput<'_>) -> wt_core::watch::RowFacts {
    let pr_merged = input.pr_read && input.pr.is_some_and(|pr| pr.state == "MERGED");
    let lifecycle = input.lifecycle.unwrap_or_default();
    let base = lifecycle.local_merged == Some(true) || pr_merged;
    let landing = wt_core::Landing::new(base, lifecycle.in_production, input.separate_production)
        .with_deploys(lifecycle.deploys);
    let can_deny = input.lifecycle.is_some() && input.pr_read;
    let answer = |holds: bool| holds.then_some(true).or(can_deny.then_some(false));
    // Without landing evidence the deploy proof is missing too.
    let reached = |stage| input.lifecycle.and_then(|_| answer(landing.reached(stage)));
    wt_core::watch::RowFacts {
        exists: true,
        incarnation: input.created_at.map(str::to_owned),
        merged: answer(base),
        staging: reached(wt_core::VerifyStage::Staging),
        production: reached(wt_core::VerifyStage::Production),
        ready: Some(input.work.is_some_and(|work| {
            work.state == wt_core::WorkState::Ready && !wt_automations::status_is_gated(work)
        })),
        green: input.pr_read.then(|| {
            input
                .pr
                .is_some_and(|pr| pr.state == "OPEN" && pr.checks == wt_github::PrChecks::Pass)
        }),
        verified: Some(
            input
                .work
                .is_some_and(|work| work.state == wt_core::WorkState::Verified),
        ),
        closed: input
            .pr_read
            .then(|| input.pr.is_some_and(|pr| pr.state == "CLOSED")),
    }
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
    queued: &[AutomationFire],
) {
    let mut fires = evaluate(&ctx.config.automations, &facts.rows, &facts.context);
    // A queued fire the evaluation no longer produces (its worktree was
    // removed after a merge, or is busy being removed) still needs a decision,
    // or it would wait for one forever.
    let current = fires.iter().map(fire_identity).collect::<BTreeSet<_>>();
    fires.extend(
        queued
            .iter()
            .filter(|fire| !current.contains(&fire_identity(fire)))
            .cloned(),
    );
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
        let output = wt_ops::commands::resolve::run_git(
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
    // A stage's deployed ref is watched like a branch: its watermark is kept
    // with the branch tips, under the ref's name.
    let stages = [
        wt_core::VerifyStage::Staging,
        wt_core::VerifyStage::Production,
    ]
    .into_iter()
    .filter(|stage| {
        rules.iter().any(|rule| {
            rule.on == wt_config::AutomationTrigger::StageDeployed && rule.stage == Some(*stage)
        })
    });
    for stage in stages {
        let Some(now) = wt_ops::deploys::deployed_oid(ctx, stage, &ctx.cancellation).await else {
            continue;
        };
        let deployed_ref = stage.deployed_ref();
        let seen = state["branchTips"][&deployed_ref]
            .as_str()
            .map(str::to_owned);
        if seen.is_none() {
            newly_observed.insert(deployed_ref.clone(), now.clone());
        }
        tips.insert(deployed_ref, BranchTip { now, seen });
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
    // A branch advance, and an MCP event no worktree owns, target the fleet,
    // not a worktree row; dispatch runs them against an empty row, so judge
    // their requirements the same way.
    if fire.branch_range.is_some()
        || (fire.mcp_event.is_some() && fire.slug == wt_automations::FLEET_SLUG)
    {
        return match wt_actions::evaluate_requirements(
            &definition.requires,
            &wt_actions::ActionRow::default(),
        ) {
            Some(reason) => RequirementState::FrozenUnmet(reason),
            None => RequirementState::Ready,
        };
    }
    let Some(row) = rows.iter().find(|row| row.slug == fire.slug) else {
        return if let Some(vars) = &fire.frozen_vars {
            // Dispatch trusts the frozen variables, so a PR requirement needs
            // the PR number captured before the worktree went away.
            let needs_pr = definition.requires.iter().any(|tag| {
                matches!(
                    tag,
                    wt_config::RequireTag::Pr | wt_config::RequireTag::PrReady
                )
            });
            if needs_pr && vars.get("pr").is_none_or(String::is_empty) {
                RequirementState::FrozenUnmet("no PR was observed before removal".into())
            } else {
                RequirementState::Ready
            }
        } else {
            RequirementState::FrozenUnmet("worktree is no longer available".into())
        };
    };
    let (unmet, pr_unknown) = row_requirements(
        definition,
        issue_tracker,
        row,
        deployed,
        github_confirmed,
        github_covered_branches,
    );
    match unmet {
        Some(_) if pr_unknown => RequirementState::Waiting,
        Some(reason) if fire.frozen_vars.is_some() => RequirementState::FrozenUnmet(reason),
        Some(_) => RequirementState::Waiting,
        None => RequirementState::Ready,
    }
}

/// The (action, slug) pairs where a configured `status.verification_due`
/// rule's action refuses the row: there the rule does not own the delivery
/// and wt's own wake for a due check prompts the session instead. A PR
/// GitHub has not confirmed yet is not a refusal.
fn unmet_requirements(
    automations: &[wt_config::AutomationDef],
    actions: &[wt_config::ActionDef],
    issue_tracker: Option<&wt_config::IssueTrackerConfig>,
    rows: &[AutomationRow],
    board: Option<&wt_tui::Board>,
    github_fresh: bool,
    github_covered_branches: &BTreeSet<String>,
) -> BTreeSet<(String, String)> {
    automations
        .iter()
        .filter(|rule| rule.on == wt_config::AutomationTrigger::StatusVerificationDue)
        .filter_map(|rule| {
            actions
                .iter()
                .find(|action| action.id == rule.run && !action.requires.is_empty())
        })
        .flat_map(|definition| {
            rows.iter().filter_map(|row| {
                let deployed = board
                    .and_then(|board| board.rows.iter().find(|shown| shown.slug == row.slug))
                    .is_some_and(|shown| shown.stage_url.is_some());
                let (unmet, pr_unknown) = row_requirements(
                    definition,
                    issue_tracker,
                    row,
                    deployed,
                    github_fresh,
                    github_covered_branches,
                );
                (unmet.is_some() && !pr_unknown).then(|| (definition.id.clone(), row.slug.clone()))
            })
        })
        .collect()
}

/// Why the row does not meet the action's `requires`, if it does not, and
/// whether that is only because GitHub has not confirmed the row's PR yet.
fn row_requirements(
    definition: &wt_config::ActionDef,
    issue_tracker: Option<&wt_config::IssueTrackerConfig>,
    row: &AutomationRow,
    deployed: bool,
    github_confirmed: bool,
    github_covered_branches: &BTreeSet<String>,
) -> (Option<String>, bool) {
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
    // GitHub not yet confirming this row says nothing about its PR, so a
    // PR requirement waits for the next fetch instead of skipping the fire.
    let pr_unknown = matches!(action_row.pr, wt_actions::ActionPrObservation::Unknown)
        && definition.requires.iter().any(|tag| {
            matches!(
                tag,
                wt_config::RequireTag::Pr | wt_config::RequireTag::PrReady
            )
        });
    (
        wt_actions::evaluate_requirements(&definition.requires, &action_row),
        pr_unknown,
    )
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
            || inventory.kinds.pi.contains(slug)
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
        wt_config::HarnessId::Codex => "$".into(),
        wt_config::HarnessId::Opencode => "@".into(),
        wt_config::HarnessId::Pi => "/skill:".into(),
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
            mcp_event: None,
            watch: None,
            detail: String::new(),
            check: None,
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
        // A merge seen before GitHub confirms the row's PR waits for it; the
        // same fire with GitHub confirming no PR is a final skip.
        let actions = [wt_config::ActionDef {
            id: fire.rule.run.clone(),
            requires: vec![wt_config::RequireTag::Pr],
            ..Default::default()
        }];
        assert_eq!(
            requirement_state(&actions, None, &fire, &rows, false, false, &BTreeSet::new()),
            RequirementState::Waiting,
        );
        let covered = BTreeSet::from([rows[0].branch.clone()]);
        assert!(matches!(
            requirement_state(&actions, None, &fire, &rows, false, true, &covered),
            RequirementState::FrozenUnmet(_)
        ));
        // Once the worktree is gone, only a captured PR number lets it run.
        assert!(matches!(
            requirement_state(&actions, None, &fire, &[], false, false, &BTreeSet::new()),
            RequirementState::FrozenUnmet(_)
        ));
        fire.frozen_vars = Some(BTreeMap::from([("pr".into(), "3056".into())]));
        assert_eq!(
            requirement_state(&actions, None, &fire, &[], false, false, &BTreeSet::new()),
            RequirementState::Ready,
        );
    }

    #[test]
    fn a_due_check_rule_owns_delivery_only_for_rows_its_action_accepts() {
        let rule = |id: &str, on, run: &str| wt_config::AutomationDef {
            id: id.into(),
            on,
            run: run.into(),
            ..Default::default()
        };
        let due = wt_config::AutomationTrigger::StatusVerificationDue;
        let automations = [
            rule("verify", due, "needs-pr"),
            rule("plain", due, "no-requirements"),
            rule("builtin", due, "builtin:notify"),
            // Another trigger's requirements say nothing about due checks.
            rule(
                "ready",
                wt_config::AutomationTrigger::StatusReady,
                "needs-deploy",
            ),
        ];
        let action = |id: &str, requires: Vec<wt_config::RequireTag>| wt_config::ActionDef {
            id: id.into(),
            requires,
            ..Default::default()
        };
        let actions = [
            action("needs-pr", vec![wt_config::RequireTag::Pr]),
            action("no-requirements", Vec::new()),
            action("needs-deploy", vec![wt_config::RequireTag::Deployed]),
        ];
        let row = |slug: &str, pr: bool| AutomationRow {
            slug: slug.into(),
            branch: format!("feature/{slug}"),
            pr: pr.then(|| wt_automations::AutomationPr {
                number: 7,
                state: "OPEN".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        let rows = [row("with-pr", true), row("without-pr", false)];
        let covered: BTreeSet<String> = rows.iter().map(|row| row.branch.clone()).collect();
        let unmet = |fresh: bool, covered: &BTreeSet<String>| {
            unmet_requirements(&automations, &actions, None, &rows, None, fresh, covered)
        };
        // GitHub confirms one row has no PR: only there does the rule's
        // action refuse, so only there wt's own wake prompts the session.
        assert_eq!(
            unmet(true, &covered),
            BTreeSet::from([("needs-pr".to_owned(), "without-pr".to_owned())])
        );
        // A PR GitHub has not confirmed yet is not a refusal: stale data, or
        // a read that did not cover the branch.
        assert!(unmet(false, &covered).is_empty());
        assert!(unmet(true, &BTreeSet::new()).is_empty());
    }

    #[tokio::test]
    async fn a_queued_merge_whose_worktree_was_removed_still_gets_a_decision() {
        // COZ-3199: cleanup razed the worktree inside the settle window, so
        // the evaluation stopped producing the fire and it waited forever for
        // a requirement decision.
        let mut fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        std::sync::Arc::make_mut(&mut fixture.ctx.config).actions = vec![wt_config::ActionDef {
            id: "record-merge".into(),
            requires: vec![wt_config::RequireTag::Pr],
            external: true,
            ..Default::default()
        }];
        let fire = AutomationFire {
            rule: AutomationDef {
                id: "in-review-on-merge".into(),
                on: wt_config::AutomationTrigger::WtMerged,
                run: "record-merge".into(),
                ..Default::default()
            },
            slug: "removed".into(),
            quiesce_slugs: vec![],
            fire_keys: vec!["in-review-on-merge:merged:removed:3092".into()],
            stack_id: None,
            close_issue: None,
            delete_branch: None,
            delete_branch_pr: None,
            branch_range: None,
            frozen_vars: Some(BTreeMap::from([("pr".into(), "3092".into())])),
            frozen_pr: None,
            mcp_event: None,
            watch: None,
            detail: String::new(),
            check: None,
        };
        let mut facts = PreparedAutomationFacts::default();
        refresh_requirements(&fixture.ctx, &mut facts, None, &[]);
        assert!(!facts.requirements.contains_key(&fire_identity(&fire)));
        refresh_requirements(&fixture.ctx, &mut facts, None, std::slice::from_ref(&fire));
        assert_eq!(
            facts.requirements.get(&fire_identity(&fire)),
            Some(&RequirementState::Ready)
        );
        fixture.close().await.unwrap();
    }

    #[test]
    fn a_branch_advance_runs_its_action_without_a_worktree_row() {
        let fire = AutomationFire {
            rule: AutomationDef {
                id: "complete-on-release".into(),
                on: wt_config::AutomationTrigger::BranchAdvanced,
                run: "release".into(),
                branch: Some("main".into()),
                ..Default::default()
            },
            slug: wt_automations::FLEET_SLUG.into(),
            quiesce_slugs: vec![],
            fire_keys: vec![],
            stack_id: None,
            close_issue: None,
            delete_branch: None,
            delete_branch_pr: None,
            branch_range: Some(wt_automations::BranchRange {
                branch: "main".into(),
                from: "a".repeat(40),
                to: "b".repeat(40),
            }),
            frozen_vars: None,
            frozen_pr: None,
            mcp_event: None,
            watch: None,
            detail: String::new(),
            check: None,
        };
        let mut actions = [wt_config::ActionDef {
            id: "release".into(),
            ..Default::default()
        }];
        assert_eq!(
            requirement_state(&actions, None, &fire, &[], false, false, &BTreeSet::new()),
            RequirementState::Ready,
        );
        // A row requirement can never hold for the fleet: skip, do not wait.
        actions[0].requires = vec![wt_config::RequireTag::IssueTracker];
        assert!(matches!(
            requirement_state(&actions, None, &fire, &[], false, false, &BTreeSet::new()),
            RequirementState::FrozenUnmet(_)
        ));
    }

    #[tokio::test]
    async fn one_broken_checkout_does_not_suppress_other_merged_automations() {
        use wt_app_core::test_support::CommandFixture;
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
        // An existing directory that is not a checkout: a genuinely broken row,
        // unlike a deleted one, which is retired without an error.
        let broken = fixture._root.path().join("broken-checkout");
        std::fs::create_dir_all(&broken).unwrap();
        snapshots
            .iter_mut()
            .find(|row| row.worktree.target.slug() == "two")
            .unwrap()
            .worktree
            .target
            .path = broken.to_string_lossy().into();
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
        // Removal may already have unlinked the checkout while finishing its
        // hooks. A proven live lock defers evidence, without declaring it safe.
        let removing = FileLock::try_acquire(&ctx.config.paths.lock_dir, "two", "remove")
            .await
            .unwrap()
            .unwrap();
        let busy = lifecycle_facts(
            ctx,
            Some(&git),
            Some(&metadata),
            None,
            &ctx.config.automations,
        )
        .await
        .unwrap();
        assert!(busy.deferred);
        assert!(!busy.errors.contains_key("two"));
        assert!(!busy.rows.contains_key("two"));
        assert!(busy.rows["one"].clean_candidate);
        drop(removing);
        let unlocked = lifecycle_facts(
            ctx,
            Some(&git),
            Some(&metadata),
            None,
            &ctx.config.automations,
        )
        .await
        .unwrap();
        assert!(!unlocked.deferred);
        assert!(unlocked.errors.contains_key("two"));
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
            wt_ops::lifecycle_ops::plan_with_facts(ctx, rows, &metadata.0, None, None)
                .await
                .is_err()
        );
        fixture.close().await.unwrap();
    }

    /// A check owed on a probed stage must not come due through a row whose
    /// deploy was never looked up.
    #[tokio::test]
    async fn a_row_without_landing_evidence_is_not_shown_deployed_on_a_probed_stage() {
        let mut fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        std::sync::Arc::make_mut(&mut fixture.ctx.config)
            .environments
            .staging = Some(wt_config::EnvironmentConfig {
            deployed: vec!["true".into()],
            ..wt_config::EnvironmentConfig::default()
        });
        let ctx = &fixture.ctx;
        let git = Confirmed::for_test(
            ctx.repository
                .inventory_status(&ctx.cancellation)
                .await
                .unwrap(),
        );
        let state = ctx
            .database
            .call(|store| Ok(store.read_wt_state()?))
            .await
            .unwrap();
        let metadata = Confirmed::for_test((state, BTreeSet::new()));
        let (edits, no_evidence) = (BTreeMap::new(), BTreeMap::new());
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
                lifecycle: &no_evidence,
            },
        )
        .await
        .unwrap();
        let row = prepared.rows.iter().find(|row| row.slug == "one").unwrap();
        assert_eq!(
            row.deploys,
            wt_core::StageDeploys {
                staging: Some(false),
                production: None,
            }
        );
        fixture.close().await.unwrap();
    }

    async fn fixture_git(ctx: &AppContext, cwd: &std::path::Path, args: &[&str]) -> String {
        ctx.processes
            .run(
                wt_platform::process::CommandSpec::new("git")
                    .args(args.iter().copied())
                    .cwd(cwd),
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

    /// `one` carries landed work pushed to origin; `two` has no own work.
    async fn landed_fixture() -> wt_app_core::test_support::CommandFixture {
        let mut fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        std::sync::Arc::make_mut(&mut fixture.ctx.config).automations = vec![AutomationDef {
            id: "delete-landed-branch".into(),
            on: wt_config::AutomationTrigger::WtMerged,
            run: "builtin:delete-branch".into(),
            ..Default::default()
        }];
        let ctx = &fixture.ctx;
        let origin = fixture._root.path().join("origin.git");
        fixture_git(ctx, &ctx.cwd, &["init", "--bare", origin.to_str().unwrap()]).await;
        fixture_git(
            ctx,
            &ctx.cwd,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        )
        .await;
        let base = fixture_git(ctx, &ctx.cwd, &["rev-parse", "HEAD"]).await;
        ctx.database
            .call(move |store| {
                store.set_slug_base("one", Some(("main", Some(&base))))?;
                Ok(())
            })
            .await
            .unwrap();
        std::fs::write(ctx.cwd.join("tracked.txt"), "landed work\n").unwrap();
        fixture_git(ctx, &ctx.cwd, &["commit", "-am", "landed"]).await;
        fixture_git(ctx, &ctx.cwd, &["push", "origin", "HEAD:refs/heads/main"]).await;
        fixture
    }

    async fn facts_for(
        ctx: &AppContext,
        snapshots: Vec<WorktreeSnapshot>,
    ) -> PreparedLifecycleFacts {
        let state = ctx
            .database
            .call(|store| Ok(store.read_wt_state()?))
            .await
            .unwrap();
        lifecycle_facts(
            ctx,
            Some(&Confirmed::for_test(snapshots)),
            Some(&Confirmed::for_test((state, BTreeSet::new()))),
            None,
            &ctx.config.automations,
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn removing_or_deleted_checkouts_are_retired_without_errors() {
        let fixture = landed_fixture().await;
        let ctx = &fixture.ctx;
        let snapshots = ctx
            .repository
            .inventory_status(&ctx.cancellation)
            .await
            .unwrap();
        let one = snapshots
            .iter()
            .find(|row| row.worktree.target.slug() == "one")
            .unwrap()
            .worktree
            .clone();
        let two = snapshots
            .iter()
            .find(|row| row.worktree.target.slug() == "two")
            .unwrap()
            .worktree
            .clone();
        let healthy = facts_for(ctx, snapshots.clone()).await;
        assert!(healthy.rows["one"].clean_candidate);
        assert!(healthy.rows.contains_key("two"));

        // An accepted removal owns `one`, though its files still exist.
        let (path, branch) = (one.target.path.clone(), one.target.branch.clone());
        ctx.database
            .call(move |store| Ok(store.begin_removal("one", "job-1", &path, &branch)?))
            .await
            .unwrap();
        // `two` was deleted out from under the (still listed) inventory row.
        std::fs::remove_dir_all(&two.target.path).unwrap();
        let retired = facts_for(ctx, snapshots).await;
        assert!(!retired.rows.contains_key("one"));
        assert!(!retired.rows.contains_key("two"));
        assert!(retired.errors.is_empty(), "{:?}", retired.errors);
        assert!(!retired.deferred);
        fixture.close().await.unwrap();
    }

    /// Keep rewriting an untracked file, as an agent editing the checkout does.
    fn churn(
        path: std::path::PathBuf,
    ) -> (
        tokio::task::JoinHandle<()>,
        tokio_util::sync::CancellationToken,
    ) {
        let stop = tokio_util::sync::CancellationToken::new();
        let token = stop.clone();
        let task = tokio::spawn(async move {
            let mut index = 0u64;
            while !token.is_cancelled() {
                index += 1;
                let _ = tokio::fs::write(&path, index.to_string()).await;
                tokio::task::yield_now().await;
            }
        });
        (task, stop)
    }

    #[tokio::test]
    async fn actively_edited_checkouts_are_not_attention_errors() {
        let fixture = landed_fixture().await;
        let ctx = &fixture.ctx;
        let snapshots = ctx
            .repository
            .inventory_status(&ctx.cancellation)
            .await
            .unwrap();
        let path = |slug: &str| {
            std::path::PathBuf::from(
                &snapshots
                    .iter()
                    .find(|row| row.worktree.target.slug() == slug)
                    .unwrap()
                    .worktree
                    .target
                    .path,
            )
        };
        // An unlanded row reads no checkout hazards, so edits there cannot
        // make its evidence fail.
        let (task, stop) = churn(path("two").join("scratch.txt"));
        let unlanded = facts_for(ctx, snapshots.clone()).await;
        stop.cancel();
        task.await.unwrap();
        assert!(unlanded.errors.is_empty(), "{:?}", unlanded.errors);
        assert!(!unlanded.rows["two"].clean_candidate);
        assert!(unlanded.rows["one"].clean_candidate);

        // A landed row reads only its hazards, so a file that keeps changing
        // is uncommitted work: no error, and never a cleanup candidate.
        let (task, stop) = churn(path("one").join("scratch.txt"));
        let landed = facts_for(ctx, snapshots).await;
        stop.cancel();
        task.await.unwrap();
        assert!(landed.errors.is_empty(), "{:?}", landed.errors);
        assert!(!landed.rows["one"].clean_candidate);
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
