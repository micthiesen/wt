use std::collections::{BTreeMap, BTreeSet};

use wt_config::{AutomationDef, AutomationTrigger};
use wt_core::{WorkState, watch::WatchAction};

use crate::types::{
    ActionAudience, AutomationEvalContext, AutomationFire, AutomationRow, BranchRange, CheckFire,
    FrozenPr, GuideOwner, McpEvent, WatchFire, status_is_gated, status_suffix,
    status_trigger_state,
};

pub const FLEET_SLUG: &str = "_fleet";
pub use wt_config::{CHECK_RULE_ID, WATCH_RULE_ID};
pub const WATCH_RUN: &str = "builtin:watch";
pub const CHECK_RUN: &str = "builtin:check";
/// GitHub's answer about a merge armed more recently than this may predate
/// the arming, so it is not yet evidence that the merge was dropped. It is
/// the age up to which the sources still call GitHub data fresh.
pub const MERGE_ARM_GRACE_MS: i64 = 90_000;

/// The rule every watch fire runs under. It exists without configuration,
/// and configuration cannot declare it.
pub fn watch_rule() -> AutomationDef {
    AutomationDef {
        id: WATCH_RULE_ID.into(),
        on: AutomationTrigger::WatchMet,
        run: WATCH_RUN.into(),
        busy: wt_config::AutomationBusyPolicy::Queue,
        settle_seconds: 0.0,
        ..AutomationDef::default()
    }
}

/// The rule that wakes a worktree's session when a check it owes comes due.
/// Like the watch rule it exists without configuration, and configuration
/// cannot declare it.
pub fn check_rule(settle_seconds: f64) -> AutomationDef {
    AutomationDef {
        id: CHECK_RULE_ID.into(),
        on: AutomationTrigger::StatusVerificationDue,
        run: CHECK_RUN.into(),
        busy: wt_config::AutomationBusyPolicy::Queue,
        settle_seconds,
        ..AutomationDef::default()
    }
}

/// Whether the fire is wt's own wake for a due check.
pub fn is_check_wake(fire: &AutomationFire) -> bool {
    fire.rule.run == CHECK_RUN && fire.check.is_some()
}

/// Evaluate level conditions from one consistent source snapshot. Callers
/// supply freshness, durable pause state, local time, and lifecycle safety
/// evidence; this function performs no I/O and has no ledger side effects.
pub fn evaluate(
    rules: &[AutomationDef],
    rows: &[AutomationRow],
    context: &AutomationEvalContext,
) -> Vec<AutomationFire> {
    let mut fires = Vec::new();
    for rule in rules {
        match rule.on {
            AutomationTrigger::StackParentMerged => {
                fires.extend(evaluate_stack(rule, rows, context));
            }
            AutomationTrigger::BranchAdvanced => {
                if let Some(fire) = evaluate_branch(rule, context) {
                    fires.push(fire);
                }
            }
            AutomationTrigger::StageDeployed => {
                if let Some(fire) = evaluate_stage(rule, context) {
                    fires.push(fire);
                }
            }
            AutomationTrigger::McpEvent => fires.extend(evaluate_mcp(rule, rows, context)),
            // Watch fires come from the watches themselves, below.
            AutomationTrigger::WatchMet => {}
            AutomationTrigger::StatusVerificationDue => {
                for row in rows.iter().filter(|row| eligible(row, context)) {
                    fires.extend(evaluate_checks_due(rule, row, context));
                }
            }
            trigger => {
                for row in rows {
                    if eligible(row, context)
                        && let Some(fire) = evaluate_row(trigger, rule, row, context)
                    {
                        fires.push(fire);
                    }
                }
            }
        }
    }
    fires.extend(evaluate_own_checks(rules, rows, context));
    fires.extend(evaluate_watches(rows, context));
    fires
}

/// One fire per check of the row that is due: the change is in the check's
/// stage and no not-before holds it back.
fn evaluate_checks_due(
    rule: &AutomationDef,
    row: &AutomationRow,
    context: &AutomationEvalContext,
) -> Vec<AutomationFire> {
    let landing = row_landing(row, context);
    wt_core::check_standings(row.work.as_ref(), landing, context.now_ms)
        .into_iter()
        .filter(|(_, standing)| standing.due)
        .map(|(check, standing)| {
            let stage = standing.stage;
            // Keyed by the obligation, not the assertion: a run that fails and
            // re-asserts needs-human must not be rerun by its own status write.
            // New steps are a new check, and so is one put off to a later time.
            let mut key = format!(
                "{}:verify:{}:{}:{:016x}",
                rule.id,
                row.slug,
                stage.as_str(),
                stable_hash(&check.steps)
            );
            // Only when set: a key that always carried it would fire every
            // check delivered before the key had it once more.
            if let Some(not_before) = check.not_before_ms {
                key.push_str(&format!(":{not_before}"));
            }
            let mut fire = single(
                rule,
                row,
                key,
                &format!("verify in {}: {}", stage.as_str(), check.steps),
            );
            fire.check = Some(CheckFire {
                stage,
                steps: check.steps,
            });
            fire
        })
        .collect()
}

/// wt's own wake for each due check, where no configured rule already
/// brings the check to the row's session. Like a wake set with `wt wake`,
/// it waits for the session and for a paused worktree, and needs no rule.
fn evaluate_own_checks(
    rules: &[AutomationDef],
    rows: &[AutomationRow],
    context: &AutomationEvalContext,
) -> Vec<AutomationFire> {
    let rule = check_rule(context.check_settle_seconds);
    let mut fires = Vec::new();
    for row in rows {
        if row.archived || context.pauses.slugs.contains(&row.slug) {
            continue;
        }
        // A rule that tells the person or the manager, or whose action does
        // not accept this row, leaves the session unprompted.
        let owned = rules.iter().any(|configured| {
            configured.on == AutomationTrigger::StatusVerificationDue
                && action_traits(configured, context)
                    .is_some_and(|traits| traits.audience == ActionAudience::Session)
                && !context
                    .unmet_requirements
                    .contains(&(configured.run.clone(), row.slug.clone()))
        });
        if owned {
            continue;
        }
        // The owner's incarnation: a later worktree with the same slug and
        // the same steps owes its own check.
        let created = row.created_at.as_deref().unwrap_or_default();
        let prefix = format!("{}:verify:{}:", rule.id, row.slug);
        for mut fire in evaluate_checks_due(&rule, row, context) {
            for key in &mut fire.fire_keys {
                let rest = key.strip_prefix(&prefix).unwrap_or(key);
                *key = format!("{}:{}:{created}:{rest}", rule.id, row.slug);
            }
            fires.push(fire);
        }
    }
    fires
}

/// One fire per action a met watch still owes. Clearing a gate and telling
/// the person need nothing from the worktree, so they neither wait for it
/// nor need its row. Waking the session waits for the gate to be cleared
/// first, and for the session like any prompt. A paused worktree holds all
/// three: nothing is consumed.
fn evaluate_watches(
    rows: &[AutomationRow],
    context: &AutomationEvalContext,
) -> Vec<AutomationFire> {
    let mut fires = Vec::new();
    for watch in &context.watches {
        if context.pauses.slugs.contains(&watch.slug) {
            continue;
        }
        let owes_unblock = watch
            .actions
            .iter()
            .any(|(action, _)| *action == WatchAction::Unblock);
        let owner = rows
            .iter()
            .find(|row| row.slug == watch.slug && !row.archived);
        for (action, text) in &watch.actions {
            let wake = *action == WatchAction::Wake;
            if wake && (owes_unblock || owner.is_none()) {
                continue;
            }
            fires.push(AutomationFire {
                rule: watch_rule(),
                slug: watch.slug.clone(),
                quiesce_slugs: if wake {
                    vec![watch.slug.clone()]
                } else {
                    Vec::new()
                },
                fire_keys: vec![wt_core::watch::action_key(&watch.id, *action)],
                stack_id: None,
                close_issue: None,
                delete_branch: None,
                delete_branch_pr: None,
                branch_range: None,
                frozen_vars: None,
                frozen_pr: None,
                mcp_event: None,
                watch: Some(WatchFire {
                    id: watch.id.clone(),
                    action: *action,
                    by: watch.by.clone(),
                    held: watch.held.clone(),
                    text: text.clone(),
                    cleared_gate: watch.cleared_gate.clone(),
                    landing: owner.map_or(wt_core::OwnerLanding::Unknown, |row| {
                        owner_landing(row, context)
                    }),
                }),
                check: None,
                detail: wt_core::sanitize_terminal_text(&format!(
                    "{}: {}",
                    action.as_str(),
                    watch.held
                )),
            });
        }
    }
    fires
}

/// Return breaker pairs whose trigger was observed false in this pass.
///
/// This deliberately uses the same evaluator as dispatch instead of treating
/// a missing fire as a clear. Missing can also mean that the source is stale,
/// a row or stack member is ineligible, a status fire was suppressed as an
/// echo, or a global branch watermark has only just been initialized.
pub fn evaluate_breaker_resets(
    rules: &[AutomationDef],
    rows: &[AutomationRow],
    context: &AutomationEvalContext,
) -> BTreeSet<(String, String)> {
    let fires = evaluate(rules, rows, context);
    // By rule and target: a rule may have one fire per owed check of a row.
    let fired: BTreeSet<_> = fires
        .iter()
        .map(|fire| {
            format!(
                "{}|{}",
                fire.rule.id,
                fire.stack_id.as_deref().unwrap_or(&fire.slug)
            )
        })
        .collect();
    let mut ineligible_stacks = BTreeSet::new();
    for row in rows {
        if let Some(stack) = &row.stack
            && !eligible(row, context)
        {
            ineligible_stacks.insert(stack.id.clone());
        }
    }

    let mut resets = BTreeSet::new();
    for rule in rules {
        if matches!(
            rule.on,
            AutomationTrigger::BranchAdvanced
                | AutomationTrigger::StageDeployed
                | AutomationTrigger::McpEvent
                | AutomationTrigger::WatchMet
        ) {
            // Branch movement, deploys and MCP events are each a distinct occurrence;
            // no worktree condition clears them.
            continue;
        }
        let github_driven = matches!(
            rule.on,
            AutomationTrigger::PrChecksFailed
                | AutomationTrigger::ReviewBotUnresolved
                | AutomationTrigger::ReviewChangesRequested
                | AutomationTrigger::PrMergeDropped
                | AutomationTrigger::WtMerged
                | AutomationTrigger::StackParentMerged
                | AutomationTrigger::StatusVerificationDue
        );
        if github_driven && !context.github_fresh {
            continue;
        }
        if rule.on == AutomationTrigger::StackParentMerged {
            let stack_ids: BTreeSet<_> = rows
                .iter()
                .filter_map(|row| row.stack.as_ref().map(|stack| stack.id.clone()))
                .collect();
            for stack_id in stack_ids {
                if context.pauses.stack_ids.contains(&stack_id)
                    || ineligible_stacks.contains(&stack_id)
                    || !rows.iter().any(|row| {
                        row.stack.as_ref().is_some_and(|stack| stack.id == stack_id)
                            && eligible(row, context)
                    })
                {
                    continue;
                }
                let identity = format!("{}|{}", rule.id, stack_id);
                if !fired.contains(&identity) {
                    resets.insert((rule.id.clone(), stack_id));
                }
            }
            continue;
        }
        let status_wanted = status_trigger_state(rule.on);
        for row in rows {
            if !eligible(row, context) {
                continue;
            }
            // A stale GitHub snapshot cannot prove a PR-backed conflict
            // cleared. PR-less conflict probes are local and remain usable.
            if rule.on == AutomationTrigger::PrConflict && row.pr.is_some() && !context.github_fresh
            {
                continue;
            }
            // An armed merge whose queue membership the read cannot tell is
            // not known to be intact.
            // Nor is one GitHub may not have caught up with, or has never
            // been seen holding.
            if rule.on == AutomationTrigger::PrMergeDropped
                && row.merge_arm.as_ref().is_some_and(|arm| {
                    row.pr.as_ref().is_some_and(|pr| pr.merge_queued.is_none())
                        || arm.seen_armed_at_ms.is_none()
                        || context.now_ms.saturating_sub(arm.armed_at_ms) < MERGE_ARM_GRACE_MS
                })
            {
                continue;
            }
            // The evaluator suppresses a status fire written by the audience
            // it would brief. That is still a true condition, not a reset.
            if status_wanted
                .is_some_and(|wanted| row.work.as_ref().is_some_and(|work| work.state == wanted))
            {
                continue;
            }
            let identity = format!("{}|{}", rule.id, row.slug);
            if !fired.contains(&identity) {
                resets.insert((rule.id.clone(), row.slug.clone()));
            }
        }
    }
    resets
}

pub fn fire_identity(fire: &AutomationFire) -> String {
    let target = fire.stack_id.as_deref().unwrap_or(&fire.slug);
    match (&fire.mcp_event, &fire.watch, &fire.check) {
        // Each event is its own fire; sharing `rule|slug` would let a burst
        // for one guide replace itself in the queue, newest first.
        (Some(event), _, _) => format!("{}|{target}|{}", fire.rule.id, event.event_id),
        // So is each action of each watch: several met watches on one
        // worktree must not replace each other.
        (None, Some(watch), _) => format!(
            "{}|{target}|{}:{}",
            fire.rule.id,
            watch.id,
            watch.action.as_str()
        ),
        // And each stage's check: staging and production can be due at once.
        (None, None, Some(check)) => {
            format!("{}|{target}|check:{}", fire.rule.id, check.stage.as_str())
        }
        (None, None, None) => format!("{}|{target}", fire.rule.id),
    }
}

pub fn eligible(row: &AutomationRow, context: &AutomationEvalContext) -> bool {
    !row.archived && !row.busy && !context.pauses.slugs.contains(&row.slug)
}

fn evaluate_row(
    trigger: AutomationTrigger,
    rule: &AutomationDef,
    row: &AutomationRow,
    context: &AutomationEvalContext,
) -> Option<AutomationFire> {
    let slug = row.slug.as_str();
    match trigger {
        AutomationTrigger::WtCreated => {
            let created = row.created_at.as_ref()?;
            if row.local_merged == Some(true)
                || row.gone == Some(true)
                || row.pr.as_ref().is_some_and(|pr| pr.state == "MERGED")
            {
                return None;
            }
            let mut fire = single(
                rule,
                row,
                format!("{}:created:{}:{created}", rule.id, slug),
                "worktree created",
            );
            if action_traits(rule, context).is_some_and(|traits| traits.shell && traits.external) {
                fire.quiesce_slugs.clear();
            }
            Some(fire)
        }
        AutomationTrigger::PrChecksFailed => {
            let pr = fresh_open_pr(row, context)?;
            if !pr.checks_failed {
                return None;
            }
            let checks = if pr.failed_checks.is_empty() {
                "checks".into()
            } else {
                pr.failed_checks.join(", ")
            };
            Some(single(
                rule,
                row,
                format!("{}:ci:{}:{}", rule.id, slug, pr.head_sha.as_deref()?),
                &format!("checks failing on #{} ({checks})", pr.number),
            ))
        }
        AutomationTrigger::ReviewBotUnresolved => {
            let pr = fresh_open_pr(row, context)?;
            if pr.review_bot_unresolved == 0 {
                return None;
            }
            Some(single(
                rule,
                row,
                format!("{}:rabbit:{}:{}", rule.id, slug, pr.head_sha.as_deref()?),
                &format!(
                    "{} unresolved review-bot finding(s) on #{}",
                    pr.review_bot_unresolved, pr.number
                ),
            ))
        }
        AutomationTrigger::ReviewChangesRequested => {
            if !context.reviewers_enabled {
                return None;
            }
            let pr = fresh_open_pr(row, context)?;
            if !pr.changes_requested {
                return None;
            }
            Some(single(
                rule,
                row,
                format!("{}:review:{}:{}", rule.id, slug, pr.head_sha.as_deref()?),
                &format!("changes requested on #{}", pr.number),
            ))
        }
        AutomationTrigger::PrConflict => {
            let conflict = row.conflict.as_ref()?;
            if !conflict.conflicted {
                return None;
            }
            if row.pr.is_some() && (!context.github_fresh || row.pr.as_ref()?.head_sha.is_none()) {
                return None;
            }
            let head = row
                .pr
                .as_ref()
                .and_then(|pr| pr.head_sha.as_deref())
                .unwrap_or("local");
            let base = conflict
                .base
                .strip_prefix("origin/")
                .unwrap_or(&conflict.base);
            Some(single(
                rule,
                row,
                format!("{}:conflict:{}:{}:{}", rule.id, slug, conflict.base, head),
                &format!("conflicts with {base}"),
            ))
        }
        AutomationTrigger::PrMergeDropped => {
            let arm = row.merge_arm.as_ref()?;
            let pr = fresh_open_pr(row, context)?;
            // A record GitHub was never seen to hold may be one a build
            // that does not clear records cancelled.
            if pr.number != arm.pr
                || arm.seen_armed_at_ms.is_none()
                || pr.auto_merge
                || pr.merge_queued != Some(false)
                || context.now_ms.saturating_sub(arm.armed_at_ms) < MERGE_ARM_GRACE_MS
            {
                return None;
            }
            // Keyed by the arming: one arming is reported once, and arming
            // again is a new merge to lose.
            Some(single(
                rule,
                row,
                format!("{}:{}:{}", rule.id, slug, arm.armed_at_ms),
                &merge_dropped_detail(arm, pr, row),
            ))
        }
        AutomationTrigger::WtMerged => evaluate_merged(rule, row, context),
        AutomationTrigger::StatusNeedsHuman
        | AutomationTrigger::StatusNeedsTesting
        | AutomationTrigger::StatusReady
        | AutomationTrigger::StatusVerified => {
            let wanted = status_trigger_state(trigger)?;
            let work = row.work.as_ref()?;
            if work.state != wanted || (wanted == WorkState::Ready && status_is_gated(work)) {
                return None;
            }
            let audience =
                action_traits(rule, context).map_or(ActionAudience::None, |traits| traits.audience);
            if matches!(
                (&audience, work.by.as_deref()),
                (ActionAudience::Manager, Some("manager"))
            ) || matches!((&audience, work.by.as_deref()), (ActionAudience::Session, Some(by)) if by == slug)
                // The human set it in the TUI; a banner would tell them what
                // they just did.
                || (rule.run == "builtin:notify" && work.by.as_deref() == Some("human"))
            {
                return None;
            }
            let mut fire = single(
                rule,
                row,
                format!("{}:work:{}:{}", rule.id, slug, work.at),
                &format!("{}{}", work.state.as_str(), status_suffix(work)),
            );
            // A verified worktree is finished and usually removed within
            // moments. A run that leaves the repository needs nothing from
            // the checkout, so it must neither wait for the session to go
            // idle nor be dropped when the row disappears.
            if wanted == WorkState::Verified
                && action_traits(rule, context)
                    .is_some_and(|traits| traits.shell && traits.external)
            {
                fire.quiesce_slugs.clear();
                fire.frozen_vars =
                    Some(context.row_vars.get(&row.slug).cloned().unwrap_or_default());
            }
            Some(fire)
        }
        AutomationTrigger::StatusVerificationOverdue => {
            let work = row.work.as_ref()?;
            let overdue = wt_core::checks_overdue_with_days(
                Some(work),
                row_landing(row, context),
                context.now_ms,
                rule.after_days,
            );
            if overdue.is_empty() {
                return None;
            }
            // One reminder a day for the row, naming every overdue check.
            let detail = overdue
                .into_iter()
                .filter_map(|stage| {
                    let check = work.open_check(stage)?;
                    Some(format!(
                        "still owes the check in {}: {}",
                        stage.as_str(),
                        check.steps
                    ))
                })
                .collect::<Vec<_>>()
                .join("; ");
            Some(single(
                rule,
                row,
                format!("{}:unverified:{}:{}", rule.id, slug, context.local_day),
                &detail,
            ))
        }
        // One fire per due check: see `evaluate_checks_due`.
        AutomationTrigger::StatusVerificationDue
        | AutomationTrigger::StackParentMerged
        | AutomationTrigger::BranchAdvanced
        | AutomationTrigger::StageDeployed
        | AutomationTrigger::McpEvent
        | AutomationTrigger::WatchMet => None,
    }
}

/// What was observed, and the likely cause when the row already shows one.
fn merge_dropped_detail(
    arm: &wt_core::MergeArm,
    pr: &crate::types::AutomationPr,
    row: &AutomationRow,
) -> String {
    let what = match arm.mechanism {
        wt_core::MergeMechanism::Queue => format!("#{} left the merge queue", pr.number),
        wt_core::MergeMechanism::AutoMerge => format!("auto-merge was dropped on #{}", pr.number),
    };
    let mut causes = Vec::new();
    if pr.checks_failed {
        causes.push(if pr.failed_checks.is_empty() {
            "checks failing".to_owned()
        } else {
            format!("checks failing: {}", pr.failed_checks.join(", "))
        });
    }
    if let Some(conflict) = row.conflict.as_ref().filter(|conflict| conflict.conflicted) {
        let base = conflict
            .base
            .strip_prefix("origin/")
            .unwrap_or(&conflict.base);
        causes.push(format!("conflicts with {base}"));
    }
    if arm.head.is_some() && arm.head != pr.head_sha {
        causes.push("new commits were pushed after it was armed".to_owned());
    }
    if causes.is_empty() {
        what
    } else {
        wt_core::sanitize_terminal_text(&format!("{what} ({})", causes.join("; ")))
    }
}

/// Armed merges GitHub is now seen to hold, as `(slug, armed_at_ms)`, for
/// records that do not say so yet. Only a read that started after the
/// arming counts: an earlier one may show an arming since cancelled.
pub fn observed_merge_arms(
    rows: &[AutomationRow],
    context: &AutomationEvalContext,
) -> Vec<(String, i64)> {
    let Some(read_at_ms) = context.github_read_at_ms.filter(|_| context.github_fresh) else {
        return Vec::new();
    };
    rows.iter()
        .filter_map(|row| {
            let arm = row.merge_arm.as_ref()?;
            let pr = row.pr.as_ref()?;
            (arm.seen_armed_at_ms.is_none()
                && read_at_ms > arm.armed_at_ms
                && pr.number == arm.pr
                && pr.state == "OPEN"
                && (pr.auto_merge || pr.merge_queued == Some(true)))
            .then(|| (row.slug.clone(), arm.armed_at_ms))
        })
        .collect()
}

/// Armed merges that have ended, as `(slug, armed_at_ms)`: the branch
/// landed, or fresh GitHub data shows the armed pull request no longer open
/// or replaced by another. The caller forgets each record.
pub fn finished_merge_arms(
    rows: &[AutomationRow],
    context: &AutomationEvalContext,
) -> Vec<(String, i64)> {
    rows.iter()
        .filter_map(|row| {
            let arm = row.merge_arm.as_ref()?;
            let pr_ended = context.github_fresh
                && row
                    .pr
                    .as_ref()
                    .is_some_and(|pr| pr.number != arm.pr || pr.state != "OPEN");
            (pr_ended || row.local_merged == Some(true) || row.gone == Some(true))
                .then(|| (row.slug.clone(), arm.armed_at_ms))
        })
        .collect()
}

fn evaluate_merged(
    rule: &AutomationDef,
    row: &AutomationRow,
    context: &AutomationEvalContext,
) -> Option<AutomationFire> {
    let closes_issue = rule.run == "builtin:close-issue";
    let deletes_branch = rule.run == "builtin:delete-branch";
    let traits = action_traits(rule, context);
    let external = traits.is_some_and(|traits| traits.shell && traits.external);
    if row.stack.is_some() && !closes_issue && !deletes_branch && !external {
        return None;
    }
    let pr_done = context.github_fresh && row.pr.as_ref().is_some_and(|pr| pr.state == "MERGED");
    if row.local_merged != Some(true) && row.gone != Some(true) && !pr_done {
        return None;
    }
    // Clean safety gates actions on the checkout. An external action (a
    // tracker update) is about the landing alone, and a merged PR is proof of
    // that even when the worktree is retained, e.g. for an owed check.
    if !row.clean_candidate && !(external && pr_done) {
        return None;
    }
    let landed = if pr_done {
        format!("#{} merged", row.pr.as_ref()?.number)
    } else if let Some(pr) = &row.pr {
        format!(
            "branch landed on trunk (#{} not observed merged)",
            pr.number
        )
    } else {
        "branch landed on trunk".into()
    };
    // One landing can be observed through the merged PR or, once that PR
    // drops out of the snapshot, through trunk containment alone. The PR key
    // differs between those observations, so the landing key (this
    // worktree's branch in this incarnation) is what keeps an issue a human
    // reopened from being closed again. `wt next` moves to a new branch, so
    // a follow-up PR is a new landing.
    let keys = vec![
        format!(
            "{}:merged:{}:{}",
            rule.id,
            row.slug,
            row.pr
                .as_ref()
                .map_or_else(|| "local".into(), |pr| pr.number.to_string())
        ),
        format!(
            "{}:landed:{}:{}:{}",
            rule.id,
            row.slug,
            row.branch,
            row.created_at.as_deref().unwrap_or("unknown")
        ),
    ];
    if closes_issue {
        let issue = row
            .github_issue
            .or_else(|| github_issue_from_slug(&row.slug))?;
        return Some(AutomationFire {
            rule: rule.clone(),
            slug: row.slug.clone(),
            quiesce_slugs: Vec::new(),
            fire_keys: keys.clone(),
            stack_id: None,
            close_issue: Some(issue),
            delete_branch: None,
            delete_branch_pr: None,
            branch_range: None,
            frozen_vars: None,
            frozen_pr: None,
            mcp_event: None,
            watch: None,
            detail: format!("{landed} — closing issue #{issue}"),
            check: None,
        });
    }
    if deletes_branch {
        if row.branch.is_empty() || row.branch == context.base_branch {
            return None;
        }
        return Some(AutomationFire {
            rule: rule.clone(),
            slug: row.slug.clone(),
            quiesce_slugs: Vec::new(),
            fire_keys: keys.clone(),
            stack_id: None,
            close_issue: None,
            delete_branch: Some(row.branch.clone()),
            delete_branch_pr: row.pr.as_ref().map(|pr| pr.number),
            branch_range: None,
            frozen_vars: None,
            frozen_pr: None,
            mcp_event: None,
            watch: None,
            detail: format!("{landed} — deleting remote branch {}", row.branch),
            check: None,
        });
    }
    if external {
        return Some(AutomationFire {
            rule: rule.clone(),
            slug: row.slug.clone(),
            quiesce_slugs: Vec::new(),
            fire_keys: keys,
            stack_id: None,
            close_issue: None,
            delete_branch: None,
            delete_branch_pr: None,
            branch_range: None,
            frozen_vars: context.row_vars.get(&row.slug).cloned(),
            frozen_pr: row.pr.as_ref().map(|pr| FrozenPr {
                state: pr.state.clone(),
                is_draft: pr.is_draft,
            }),
            mcp_event: None,
            watch: None,
            detail: landed,
            check: None,
        });
    }
    let mut fire = single(rule, row, keys[0].clone(), &landed);
    fire.fire_keys = keys;
    Some(fire)
}

fn evaluate_stack(
    rule: &AutomationDef,
    rows: &[AutomationRow],
    context: &AutomationEvalContext,
) -> Vec<AutomationFire> {
    let mut by_stack: BTreeMap<String, Vec<&AutomationRow>> = BTreeMap::new();
    let mut paused_stack_ids = context.pauses.stack_ids.clone();
    for row in rows {
        if let Some(stack) = &row.stack {
            if context.pauses.slugs.contains(&row.slug) {
                paused_stack_ids.insert(stack.id.clone());
            } else if eligible(row, context) {
                by_stack.entry(stack.id.clone()).or_default().push(row);
            }
        }
    }
    let by_slug: BTreeMap<_, _> = rows.iter().map(|row| (row.slug.as_str(), row)).collect();
    let mut fires = Vec::new();
    for (stack_id, members) in by_stack {
        if paused_stack_ids.contains(&stack_id) {
            continue;
        }
        let merged: Vec<_> = members
            .iter()
            .copied()
            .filter(|row| {
                row.clean_candidate
                    && (row.pr.as_ref().is_none_or(|pr| pr.state != "MERGED")
                        || context.github_fresh)
            })
            .collect();
        let open: Vec<_> = members
            .iter()
            .copied()
            .filter(|row| !merged.contains(row))
            .collect();
        if open.is_empty() {
            continue;
        }
        let member_branches: BTreeSet<_> = members.iter().map(|row| row.branch.as_str()).collect();
        let mut external_merged = Vec::new();
        let mut external_gone = Vec::new();
        let mut seen_parents = BTreeSet::new();
        for member in &open {
            let Some(parent) = member
                .stack
                .as_ref()
                .and_then(|stack| stack.parent.as_ref())
            else {
                continue;
            };
            if member_branches.contains(parent.branch.as_str())
                || !seen_parents.insert(parent.branch.as_str())
            {
                continue;
            }
            match parent
                .slug
                .as_deref()
                .and_then(|slug| by_slug.get(slug).copied())
            {
                None if parent.slug.is_none() => external_gone.push(parent.branch.clone()),
                Some(parent_row)
                    if !parent_row.archived
                        && !parent_row.busy
                        && !context.pauses.slugs.contains(&parent_row.slug)
                        && parent_row.clean_candidate
                        && (parent_row.pr.as_ref().is_none_or(|pr| pr.state != "MERGED")
                            || context.github_fresh) =>
                {
                    external_merged.push(parent_row);
                }
                _ => {}
            }
        }
        let mut keys: Vec<String> = merged
            .iter()
            .map(|row| {
                format!(
                    "{}:restack:{}:{}",
                    rule.id,
                    stack_id,
                    row.pr
                        .as_ref()
                        .map_or_else(|| row.branch.clone(), |pr| pr.number.to_string())
                )
            })
            .collect();
        keys.extend(external_merged.iter().map(|row| {
            format!(
                "{}:restack:{}:ext:{}",
                rule.id,
                stack_id,
                row.pr
                    .as_ref()
                    .map_or_else(|| row.branch.clone(), |pr| pr.number.to_string())
            )
        }));
        keys.extend(
            external_gone
                .iter()
                .map(|branch| format!("{}:restack:{}:extgone:{}", rule.id, stack_id, branch)),
        );
        if keys.is_empty() {
            continue;
        }
        let mut parts = Vec::new();
        if !merged.is_empty() {
            parts.push(format!("{} merged member(s)", merged.len()));
        }
        if !external_merged.is_empty() {
            let parents = external_merged
                .iter()
                .map(|row| {
                    row.pr
                        .as_ref()
                        .map_or_else(|| row.branch.clone(), |pr| format!("#{}", pr.number))
                })
                .collect::<Vec<_>>()
                .join(", ");
            parts.push(format!("merged external parent(s) {parents}"));
        }
        if !external_gone.is_empty() {
            parts.push(format!(
                "external parent gone ({})",
                external_gone.join(", ")
            ));
        }
        let mut quiesce: Vec<String> = members.iter().map(|row| row.slug.clone()).collect();
        quiesce.extend(external_merged.iter().map(|row| row.slug.clone()));
        fires.push(AutomationFire {
            rule: rule.clone(),
            slug: open[0].slug.clone(),
            quiesce_slugs: quiesce,
            fire_keys: keys,
            stack_id: Some(stack_id.clone()),
            close_issue: None,
            delete_branch: None,
            delete_branch_pr: None,
            branch_range: None,
            frozen_vars: None,
            frozen_pr: None,
            mcp_event: None,
            watch: None,
            detail: format!("{} under {} open member(s)", parts.join(" + "), open.len()),
            check: None,
        });
    }
    fires
}

fn evaluate_branch(
    rule: &AutomationDef,
    context: &AutomationEvalContext,
) -> Option<AutomationFire> {
    let branch = rule.branch.as_ref()?;
    let tip = context.branch_tips.get(branch)?;
    let from = tip.seen.as_ref()?;
    if from == &tip.now {
        return None;
    }
    let (start, end) = (&from[..from.len().min(7)], &tip.now[..tip.now.len().min(7)]);
    Some(AutomationFire {
        rule: rule.clone(),
        slug: FLEET_SLUG.into(),
        quiesce_slugs: Vec::new(),
        fire_keys: vec![format!("{}:branch:{}:{}", rule.id, branch, tip.now)],
        stack_id: None,
        close_issue: None,
        delete_branch: None,
        delete_branch_pr: None,
        branch_range: Some(BranchRange {
            branch: branch.clone(),
            from: from.clone(),
            to: tip.now.clone(),
        }),
        frozen_vars: None,
        frozen_pr: None,
        mcp_event: None,
        watch: None,
        detail: format!("{branch} advanced {start}..{end}"),
        check: None,
    })
}

/// A stage's probe reported a new deployed commit. The watermark lives with
/// the branch tips, keyed by the stage's deployed ref, and likewise advances
/// only once a run has started: the first sighting fires nothing.
fn evaluate_stage(rule: &AutomationDef, context: &AutomationEvalContext) -> Option<AutomationFire> {
    let stage = rule.stage?;
    let deployed_ref = stage.deployed_ref();
    let tip = context.branch_tips.get(&deployed_ref)?;
    let from = tip.seen.as_ref()?;
    if from == &tip.now {
        return None;
    }
    let (start, end) = (&from[..from.len().min(7)], &tip.now[..tip.now.len().min(7)]);
    Some(AutomationFire {
        rule: rule.clone(),
        slug: FLEET_SLUG.into(),
        quiesce_slugs: Vec::new(),
        // Keyed by the move, not the commit: a stage redeployed to a commit
        // it ran before (a rollback and the roll forward) is a new deploy.
        fire_keys: vec![format!(
            "{}:deployed:{}:{from}:{}",
            rule.id,
            stage.as_str(),
            tip.now
        )],
        stack_id: None,
        close_issue: None,
        delete_branch: None,
        delete_branch_pr: None,
        branch_range: Some(BranchRange {
            branch: deployed_ref,
            from: from.clone(),
            to: tip.now.clone(),
        }),
        frozen_vars: None,
        frozen_pr: None,
        mcp_event: None,
        watch: None,
        detail: format!("{} deployed {start}..{end}", stage.as_str()),
        check: None,
    })
}

/// One fire per pending event. An event about an artifact a live worktree
/// recorded as its guide goes to that worktree; it waits while the worktree
/// is paused, archived or busy. Any other event goes to the manager.
fn evaluate_mcp(
    rule: &AutomationDef,
    rows: &[AutomationRow],
    context: &AutomationEvalContext,
) -> Vec<AutomationFire> {
    let mut fires = Vec::new();
    for event in context
        .mcp_events
        .iter()
        .filter(|event| event.rule == rule.id)
    {
        let owner = guide_owner(&event.data, &event.server_url, &context.guides);
        let slug = match owner {
            Some(slug) => match rows.iter().find(|row| row.slug == slug) {
                Some(row) if eligible(row, context) => slug.to_owned(),
                _ => continue,
            },
            None => FLEET_SLUG.to_owned(),
        };
        let text = |field: &str| {
            event
                .data
                .get(field)
                .and_then(serde_json::Value::as_str)
                .map(|value| value.chars().take(60).collect::<String>())
        };
        let mut detail = event.name.clone();
        if let Some(title) = text("title") {
            detail.push_str(&format!(" on {title}"));
        }
        if let Some(by) = text("by") {
            detail.push_str(&format!(" by {by}"));
        }
        fires.push(AutomationFire {
            rule: rule.clone(),
            quiesce_slugs: if slug == FLEET_SLUG {
                Vec::new()
            } else {
                vec![slug.clone()]
            },
            slug,
            fire_keys: vec![mcp_fire_key(&rule.id, &event.event_id)],
            stack_id: None,
            close_issue: None,
            delete_branch: None,
            delete_branch_pr: None,
            branch_range: None,
            frozen_vars: None,
            frozen_pr: None,
            mcp_event: Some(event.clone()),
            watch: None,
            detail: wt_core::sanitize_terminal_text(&detail),
            check: None,
        });
    }
    fires
}

/// What a rule's stored cursor belongs to. Editing the rule's server, event
/// or arguments starts a new subscription from now rather than resuming one
/// cursor against different arguments.
pub fn mcp_subscription(rule: &AutomationDef) -> String {
    format!(
        "{}|{}|{}",
        rule.server.as_deref().unwrap_or_default(),
        rule.event.as_deref().unwrap_or_default(),
        serde_json::to_string(&rule.arguments).unwrap_or_default()
    )
}

/// Pending events recorded in wt state for the configured `mcp.event`
/// rules, skipping any recorded under a subscription the rule no longer has.
pub fn pending_mcp_events(
    rules: &[AutomationDef],
    servers: &BTreeMap<String, wt_config::McpServerConfig>,
    state: &serde_json::Value,
) -> Vec<McpEvent> {
    let mut events = Vec::new();
    for rule in rules
        .iter()
        .filter(|rule| rule.on == AutomationTrigger::McpEvent)
    {
        let entry = &state["mcpEvents"][&rule.id];
        if entry["subscription"].as_str() != Some(mcp_subscription(rule).as_str()) {
            continue;
        }
        for event in entry["pending"].as_array().into_iter().flatten() {
            let (Some(event_id), Some(name)) = (event["eventId"].as_str(), event["name"].as_str())
            else {
                continue;
            };
            events.push(McpEvent {
                rule: rule.id.clone(),
                server: rule.server.clone().unwrap_or_default(),
                server_url: rule
                    .server
                    .as_ref()
                    .and_then(|name| servers.get(name))
                    .map(|server| server.url.clone())
                    .unwrap_or_default(),
                event_id: event_id.to_owned(),
                name: name.to_owned(),
                timestamp: event["timestamp"].as_str().unwrap_or_default().to_owned(),
                data: event["data"].clone(),
            });
        }
    }
    events
}

/// The ledger key of one event's fire: a replayed or re-polled event with
/// the same `eventId` is the same fire.
pub fn mcp_fire_key(rule: &str, event_id: &str) -> String {
    format!("{rule}:mcp:{event_id}")
}

/// The worktree whose recorded guide is the event's artifact: the event's
/// `url` matches the guide's page, or its `artifactId` is the guide's last
/// path segment.
pub fn guide_owner<'a>(
    data: &serde_json::Value,
    server_url: &str,
    guides: &'a [GuideOwner],
) -> Option<&'a str> {
    let only = |matches: Vec<&'a GuideOwner>| -> Option<Option<&'a str>> {
        let mut slugs = matches
            .iter()
            .map(|guide| guide.slug.as_str())
            .collect::<Vec<_>>();
        slugs.dedup();
        match slugs.as_slice() {
            [] => None,
            [slug] => Some(Some(slug)),
            // Two worktrees claim the page: neither is the owner.
            _ => Some(None),
        }
    };
    if let Some(url) = data.get("url").and_then(serde_json::Value::as_str) {
        let url = normalize_url(url);
        if let Some(owner) = only(
            guides
                .iter()
                .filter(|guide| normalize_url(&guide.url) == url)
                .collect(),
        ) {
            return owner;
        }
    }
    // An id names an artifact only on the server that sent it.
    let id = data.get("artifactId").and_then(serde_json::Value::as_str)?;
    let host = url_host(server_url);
    only(
        guides
            .iter()
            .filter(|guide| {
                let page = normalize_url(&guide.url);
                url_host(&guide.url) == host && page.rsplit('/').next() == Some(id)
            })
            .collect(),
    )
    .flatten()
}

fn url_host(url: &str) -> String {
    normalize_url(url)
        .split('/')
        .next()
        .unwrap_or_default()
        .to_owned()
}

fn normalize_url(url: &str) -> String {
    let url = url.trim();
    let url = url.split(['?', '#']).next().unwrap_or(url);
    let url = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .unwrap_or(url);
    let (host, path) = url.split_once('/').unwrap_or((url, ""));
    format!(
        "{}/{}",
        host.to_ascii_lowercase(),
        path.trim_end_matches('/')
    )
}

fn fresh_open_pr<'a>(
    row: &'a AutomationRow,
    context: &AutomationEvalContext,
) -> Option<&'a crate::types::AutomationPr> {
    if !context.github_fresh {
        return None;
    }
    let pr = row.pr.as_ref()?;
    (pr.state == "OPEN" && pr.head_sha.is_some()).then_some(pr)
}

fn action_traits<'a>(
    rule: &AutomationDef,
    context: &'a AutomationEvalContext,
) -> Option<&'a crate::types::ActionTraits> {
    context.actions.get(&rule.run)
}

/// How far the row landed. The PR-merged leg needs fresh GitHub data, so a
/// boot-stale cache cannot report a merge it has not confirmed.
fn row_landing(row: &AutomationRow, context: &AutomationEvalContext) -> wt_core::Landing {
    let base = row.local_merged == Some(true)
        || row.gone == Some(true)
        || (context.github_fresh && row.pr.as_ref().is_some_and(|pr| pr.state == "MERGED"));
    wt_core::Landing::new(base, row.in_production, context.separate_production)
        .with_deploys(row.deploys)
}

/// Whether the row's own work had landed, for a wake about to be delivered.
/// A pull request the stale hosting data cannot vouch for, with nothing
/// local proving a landing, is unknown.
fn owner_landing(row: &AutomationRow, context: &AutomationEvalContext) -> wt_core::OwnerLanding {
    if row_landing(row, context).base {
        wt_core::OwnerLanding::Landed
    } else if row.pr.is_some() && !context.github_fresh {
        wt_core::OwnerLanding::Unknown
    } else {
        wt_core::OwnerLanding::NotLanded
    }
}

/// FNV-1a: fire keys persist in the ledger, so the hash must not change
/// between builds the way `DefaultHasher` may.
fn stable_hash(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

fn single(rule: &AutomationDef, row: &AutomationRow, key: String, detail: &str) -> AutomationFire {
    AutomationFire {
        rule: rule.clone(),
        slug: row.slug.clone(),
        quiesce_slugs: vec![row.slug.clone()],
        fire_keys: vec![key],
        stack_id: None,
        close_issue: None,
        delete_branch: None,
        delete_branch_pr: None,
        branch_range: None,
        frozen_vars: None,
        frozen_pr: None,
        mcp_event: None,
        watch: None,
        check: None,
        detail: detail.to_owned(),
    }
}

fn github_issue_from_slug(slug: &str) -> Option<u64> {
    let bytes = slug.as_bytes();
    for start in 0..bytes.len() {
        if !bytes[start].is_ascii_alphabetic()
            || (start > 0 && bytes[start - 1].is_ascii_alphanumeric())
        {
            continue;
        }
        let mut i = start;
        while i < bytes.len() && bytes[i].is_ascii_alphabetic() {
            i += 1;
        }
        if i + 1 >= bytes.len() || !slug[start..i].eq_ignore_ascii_case("GH") || bytes[i] != b'-' {
            continue;
        }
        i += 1;
        let digit_start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i == digit_start || (i < bytes.len() && bytes[i] != b'-') {
            continue;
        }
        return slug[digit_start..i].parse().ok();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::AutomationStack;
    use wt_config::{AutomationBusyPolicy, AutomationDef};
    use wt_core::{OwnerLanding, VerifyStage, WorkRisk, WorkStatusRecord};

    /// What the configured rules fire. wt's own wake for a due check has
    /// its own tests below.
    fn evaluate(
        rules: &[AutomationDef],
        rows: &[AutomationRow],
        context: &AutomationEvalContext,
    ) -> Vec<AutomationFire> {
        super::evaluate(rules, rows, context)
            .into_iter()
            .filter(|fire| fire.rule.id != CHECK_RULE_ID)
            .collect()
    }

    fn own_checks(
        rules: &[AutomationDef],
        rows: &[AutomationRow],
        context: &AutomationEvalContext,
    ) -> Vec<AutomationFire> {
        super::evaluate(rules, rows, context)
            .into_iter()
            .filter(|fire| fire.rule.id == CHECK_RULE_ID)
            .collect()
    }

    fn owing(stages: &[(VerifyStage, &str)]) -> WorkStatusRecord {
        let mut work = WorkStatusRecord::new(WorkState::Ready, "2026-01-01T00:00:00Z");
        for (stage, steps) in stages {
            work.set_check(*stage, *steps);
        }
        work
    }

    #[test]
    fn each_owed_stage_fires_on_its_own_and_a_check_put_off_fires_again() {
        let due = rule("verify", AutomationTrigger::StatusVerificationDue, "verify");
        let mut context = AutomationEvalContext {
            separate_production: true,
            now_ms: 1_000,
            ..AutomationEvalContext::default()
        };
        let mut row = row();
        let mut work = owing(&[
            (VerifyStage::Staging, "check staging"),
            (VerifyStage::Production, "check prod"),
        ]);
        row.work = Some(work.clone());
        row.local_merged = Some(true);
        let fires = evaluate(
            std::slice::from_ref(&due),
            std::slice::from_ref(&row),
            &context,
        );
        // Only staging has the change.
        assert_eq!(fires.len(), 1);
        assert_eq!(
            fires[0].check,
            Some(CheckFire {
                stage: VerifyStage::Staging,
                steps: "check staging".into()
            })
        );
        let staging_key = fires[0].fire_keys[0].clone();
        // A key written before checks were owed per stage reads the same:
        // no check already delivered fires again.
        assert_eq!(
            staging_key,
            format!(
                "verify:verify:feature-1:staging:{:016x}",
                stable_hash("check staging")
            )
        );
        row.in_production = true;
        let fires = evaluate(
            std::slice::from_ref(&due),
            std::slice::from_ref(&row),
            &context,
        );
        assert_eq!(
            fires
                .iter()
                .map(|fire| fire.detail.as_str())
                .collect::<Vec<_>>(),
            [
                "verify in staging: check staging",
                "verify in production: check prod"
            ]
        );
        // Two fires of one rule on one row must not replace each other.
        assert_ne!(fire_identity(&fires[0]), fire_identity(&fires[1]));
        assert_eq!(fires[0].fire_keys, std::slice::from_ref(&staging_key));
        // With both due the rule's breaker is not reset.
        context.github_fresh = true;
        assert!(
            evaluate_breaker_resets(
                std::slice::from_ref(&due),
                std::slice::from_ref(&row),
                &context
            )
            .is_empty()
        );

        // Put off: not due before its time, and then a new obligation.
        assert!(work.defer_check(VerifyStage::Staging, Some(5_000)));
        row.work = Some(work);
        let stages = |context: &AutomationEvalContext| {
            evaluate(
                std::slice::from_ref(&due),
                std::slice::from_ref(&row),
                context,
            )
            .into_iter()
            .map(|fire| (fire.check.unwrap().stage, fire.fire_keys[0].clone()))
            .collect::<Vec<_>>()
        };
        assert_eq!(
            stages(&context)
                .iter()
                .map(|(stage, _)| *stage)
                .collect::<Vec<_>>(),
            [VerifyStage::Production]
        );
        context.now_ms = 5_000;
        let again = stages(&context);
        assert_eq!(again[0].0, VerifyStage::Staging);
        assert_eq!(again[0].1, format!("{staging_key}:5000"));
    }

    #[test]
    fn an_overdue_reminder_names_every_overdue_check_once_a_day() {
        let nag = rule(
            "nag",
            AutomationTrigger::StatusVerificationOverdue,
            "notify",
        );
        let mut context = AutomationEvalContext {
            now_ms: wt_core::parse_iso_millis("2026-01-05T00:00:00Z").unwrap(),
            local_day: "2026-01-05".into(),
            ..AutomationEvalContext::default()
        };
        let mut row = row();
        let mut work = owing(&[
            (VerifyStage::Staging, "check staging"),
            (VerifyStage::Production, "check prod"),
        ]);
        row.work = Some(work.clone());
        row.local_merged = Some(true);
        let fires = evaluate(
            std::slice::from_ref(&nag),
            std::slice::from_ref(&row),
            &context,
        );
        assert_eq!(fires.len(), 1);
        assert_eq!(
            fires[0].detail,
            "still owes the check in staging: check staging; still owes the check in production: check prod"
        );
        assert_eq!(fires[0].fire_keys, ["nag:unverified:feature-1:2026-01-05"]);
        // A check put off is nobody's lapse until its time has passed.
        let later = context.now_ms + 86_400_000;
        assert!(work.defer_check(VerifyStage::Staging, Some(later)));
        assert!(work.defer_check(VerifyStage::Production, Some(later)));
        row.work = Some(work);
        context.now_ms += 3_600_000;
        assert!(evaluate(&[nag], &[row], &context).is_empty());
    }

    #[test]
    fn wt_wakes_the_session_for_a_due_check_unless_a_rule_already_does() {
        let mut context = AutomationEvalContext {
            check_settle_seconds: 600.0,
            ..AutomationEvalContext::default()
        };
        let mut row = row();
        row.created_at = Some("2026-01-01T00:00:00Z".into());
        row.work = Some(owing(&[(VerifyStage::Staging, "1. open the page")]));
        // Not landed: nothing is due.
        assert!(own_checks(&[], std::slice::from_ref(&row), &context).is_empty());
        row.local_merged = Some(true);
        let fires = own_checks(&[], std::slice::from_ref(&row), &context);
        assert_eq!(fires.len(), 1);
        let fire = &fires[0];
        assert!(is_check_wake(fire));
        assert_eq!(
            (fire.rule.run.as_str(), fire.rule.settle_seconds),
            (CHECK_RUN, 600.0)
        );
        assert_eq!(fire.quiesce_slugs, ["feature-1"]);
        // Slug, the owner's incarnation, stage, steps.
        assert_eq!(
            fire.fire_keys,
            [format!(
                "wt.check:feature-1:2026-01-01T00:00:00Z:staging:{:016x}",
                stable_hash("1. open the page")
            )]
        );
        // A session at work does not lose the prompt: it waits like a wake.
        row.busy = true;
        assert_eq!(
            own_checks(&[], std::slice::from_ref(&row), &context).len(),
            1
        );
        row.busy = false;

        // A rule whose action prompts the row's own session owns the delivery.
        let configured = rule("verify", AutomationTrigger::StatusVerificationDue, "verify");
        let audience = |audience| crate::types::ActionTraits {
            audience,
            ..Default::default()
        };
        context
            .actions
            .insert("verify".into(), audience(ActionAudience::Session));
        let rules = std::slice::from_ref(&configured);
        assert!(own_checks(rules, std::slice::from_ref(&row), &context).is_empty());
        assert_eq!(
            evaluate(rules, std::slice::from_ref(&row), &context).len(),
            1
        );
        // One that tells the manager, or runs a command, leaves the session
        // unprompted.
        for other in [ActionAudience::Manager, ActionAudience::None] {
            context.actions.insert("verify".into(), audience(other));
            assert_eq!(
                own_checks(rules, std::slice::from_ref(&row), &context).len(),
                1
            );
        }
        let notify = rule(
            "tell",
            AutomationTrigger::StatusVerificationDue,
            "builtin:notify",
        );
        assert_eq!(
            own_checks(&[notify], std::slice::from_ref(&row), &context).len(),
            1
        );
        // So does a session rule whose action does not accept this row,
        // and only for this row.
        context
            .actions
            .insert("verify".into(), audience(ActionAudience::Session));
        context
            .unmet_requirements
            .insert(("verify".into(), "feature-1".into()));
        let mut other = row.clone();
        other.slug = "feature-2".into();
        let fired = own_checks(rules, &[row.clone(), other], &context);
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].slug, "feature-1");
        context.unmet_requirements.clear();
        context.actions.clear();

        // A paused or archived worktree is not prompted.
        context.pauses.slugs.insert("feature-1".into());
        assert!(own_checks(&[], std::slice::from_ref(&row), &context).is_empty());
        context.pauses.slugs.clear();
        row.archived = true;
        assert!(own_checks(&[], std::slice::from_ref(&row), &context).is_empty());
        row.archived = false;
        // A later worktree of the same name owes its own check.
        let first = own_checks(&[], std::slice::from_ref(&row), &context)[0]
            .fire_keys
            .clone();
        row.created_at = Some("2026-02-01T00:00:00Z".into());
        assert_ne!(
            own_checks(&[], std::slice::from_ref(&row), &context)[0].fire_keys,
            first
        );
    }

    #[test]
    fn a_wake_carries_whether_its_worktree_had_landed() {
        let landing = |row: &AutomationRow, context: &AutomationEvalContext| {
            let mut context = context.clone();
            context.watches = vec![crate::types::MetWatch {
                id: "aaaaaa".into(),
                slug: row.slug.clone(),
                by: row.slug.clone(),
                held: "17:00".into(),
                actions: vec![(WatchAction::Wake, "go".into())],
                cleared_gate: None,
            }];
            super::evaluate(&[], std::slice::from_ref(row), &context)[0]
                .watch
                .as_ref()
                .unwrap()
                .landing
        };
        let fresh = AutomationEvalContext {
            github_fresh: true,
            ..AutomationEvalContext::default()
        };
        let stale = AutomationEvalContext::default();
        let mut row = row();
        row.pr = None;
        // Nothing says it landed, and no pull request could have.
        assert_eq!(landing(&row, &stale), OwnerLanding::NotLanded);
        row.local_merged = Some(true);
        assert_eq!(landing(&row, &stale), OwnerLanding::Landed);
        row.local_merged = None;
        row.pr = Some(crate::types::AutomationPr {
            number: 7,
            state: "MERGED".into(),
            ..Default::default()
        });
        assert_eq!(landing(&row, &fresh), OwnerLanding::Landed);
        // A merge the stale data reports is not proof either way.
        assert_eq!(landing(&row, &stale), OwnerLanding::Unknown);
        row.pr.as_mut().unwrap().state = "OPEN".into();
        assert_eq!(landing(&row, &stale), OwnerLanding::Unknown);
        assert_eq!(landing(&row, &fresh), OwnerLanding::NotLanded);
    }

    fn rule(id: &str, on: AutomationTrigger, run: &str) -> AutomationDef {
        AutomationDef {
            id: id.into(),
            on,
            run: run.into(),
            busy: AutomationBusyPolicy::Queue,
            ..AutomationDef::default()
        }
    }

    fn row() -> AutomationRow {
        AutomationRow {
            slug: "feature-1".into(),
            branch: "feature-1".into(),
            pr: Some(crate::types::AutomationPr {
                number: 42,
                state: "OPEN".into(),
                head_sha: Some("abc12345".into()),
                is_draft: false,
                checks_failed: true,
                failed_checks: vec!["test".into()],
                review_bot_unresolved: 2,
                changes_requested: true,
                auto_merge: false,
                merge_queued: None,
            }),
            ..AutomationRow::default()
        }
    }

    #[test]
    fn a_dropped_merge_fires_once_per_arming_on_fresh_complete_data() {
        use wt_core::{MergeArm, MergeMechanism};
        const ARMED: i64 = 1_000_000;
        let dropped = rule("dropped", AutomationTrigger::PrMergeDropped, "rearm");
        let rules = std::slice::from_ref(&dropped);
        let mut context = AutomationEvalContext {
            github_fresh: true,
            now_ms: ARMED + MERGE_ARM_GRACE_MS,
            ..AutomationEvalContext::default()
        };
        let armed = |armed_at_ms, mechanism| {
            let mut row = row();
            let pr = row.pr.as_mut().unwrap();
            pr.checks_failed = false;
            pr.failed_checks.clear();
            pr.merge_queued = Some(false);
            row.merge_arm = Some(MergeArm {
                pr: 42,
                head: Some("abc12345".into()),
                armed_at_ms,
                mechanism,
                seen_armed_at_ms: Some(armed_at_ms + 1),
            });
            row
        };
        let fires = |row: &AutomationRow, context: &AutomationEvalContext| {
            evaluate(rules, std::slice::from_ref(row), context)
        };
        let resets = |row: &AutomationRow, context: &AutomationEvalContext| {
            evaluate_breaker_resets(rules, std::slice::from_ref(row), context).len()
        };

        // wt never armed it: an open PR with no merge pending is ordinary.
        let mut unarmed = armed(ARMED, MergeMechanism::Queue);
        unarmed.merge_arm = None;
        assert!(fires(&unarmed, &context).is_empty());
        assert_eq!(finished_merge_arms(&[unarmed], &context), []);

        let mut row = armed(ARMED, MergeMechanism::Queue);
        let fire = fires(&row, &context).pop().unwrap();
        assert_eq!(fire.fire_keys, ["dropped:feature-1:1000000"]);
        assert_eq!(fire.detail, "#42 left the merge queue");
        assert_eq!(fire.quiesce_slugs, ["feature-1"]);
        // The same arming is the same fire on every later pass.
        context.now_ms += 3_600_000;
        assert_eq!(fires(&row, &context)[0].fire_keys, fire.fire_keys);
        assert_eq!(resets(&row, &context), 0);
        context.now_ms = ARMED + MERGE_ARM_GRACE_MS;

        // GitHub's answer may predate an arming this recent: not a drop,
        // and not proof the merge is intact either.
        context.now_ms -= 1;
        assert!(fires(&row, &context).is_empty());
        assert_eq!(resets(&row, &context), 0);
        context.now_ms += 1;

        // A record GitHub was never seen to hold may be a cancelled merge
        // an older build left behind.
        row.merge_arm.as_mut().unwrap().seen_armed_at_ms = None;
        assert!(fires(&row, &context).is_empty());
        assert_eq!(resets(&row, &context), 0);
        // Seeing it armed takes a read made after the arming.
        row.pr.as_mut().unwrap().merge_queued = Some(true);
        let seen = |context: &AutomationEvalContext, row: &AutomationRow| {
            observed_merge_arms(std::slice::from_ref(row), context)
        };
        assert_eq!(seen(&context, &row), []);
        context.github_read_at_ms = Some(ARMED);
        assert_eq!(seen(&context, &row), []);
        context.github_read_at_ms = Some(ARMED + 1);
        assert_eq!(seen(&context, &row), [("feature-1".to_owned(), ARMED)]);
        context.github_fresh = false;
        assert_eq!(seen(&context, &row), []);
        context.github_fresh = true;
        row.pr.as_mut().unwrap().merge_queued = Some(false);
        assert_eq!(seen(&context, &row), []);
        row.merge_arm.as_mut().unwrap().seen_armed_at_ms = Some(ARMED + 1);
        assert_eq!(seen(&context, &row), []);

        // Stale data proves nothing, and neither does a partial queue read.
        context.github_fresh = false;
        assert!(fires(&row, &context).is_empty());
        assert_eq!(resets(&row, &context), 0);
        context.github_fresh = true;
        row.pr.as_mut().unwrap().merge_queued = None;
        assert!(fires(&row, &context).is_empty());
        assert_eq!(resets(&row, &context), 0);

        // Still queued, or auto-merge still requested: nothing was dropped.
        row.pr.as_mut().unwrap().merge_queued = Some(true);
        assert!(fires(&row, &context).is_empty());
        assert_eq!(resets(&row, &context), 1);
        row.pr.as_mut().unwrap().merge_queued = Some(false);
        row.pr.as_mut().unwrap().auto_merge = true;
        assert!(fires(&row, &context).is_empty());
        row.pr.as_mut().unwrap().auto_merge = false;

        // Merged or closed is an ending, not a drop, and ends the record.
        for state in ["MERGED", "CLOSED"] {
            row.pr.as_mut().unwrap().state = state.into();
            assert!(fires(&row, &context).is_empty(), "{state}");
            assert_eq!(
                finished_merge_arms(std::slice::from_ref(&row), &context),
                [("feature-1".to_owned(), ARMED)]
            );
            context.github_fresh = false;
            assert_eq!(
                finished_merge_arms(std::slice::from_ref(&row), &context),
                []
            );
            context.github_fresh = true;
        }
        row.pr.as_mut().unwrap().state = "OPEN".into();
        assert_eq!(
            finished_merge_arms(std::slice::from_ref(&row), &context),
            []
        );
        row.local_merged = Some(true);
        assert_eq!(
            finished_merge_arms(std::slice::from_ref(&row), &context).len(),
            1
        );
        row.local_merged = None;
        // The branch has another pull request now.
        row.pr.as_mut().unwrap().number = 43;
        assert!(fires(&row, &context).is_empty());
        assert_eq!(
            finished_merge_arms(std::slice::from_ref(&row), &context).len(),
            1
        );
        row.pr.as_mut().unwrap().number = 42;

        // Cancelled: the record is gone.
        row.merge_arm = None;
        assert!(fires(&row, &context).is_empty());

        // Armed again: a new record, and a new fire if that one drops too.
        let rearmed = armed(ARMED + 500_000, MergeMechanism::AutoMerge);
        context.now_ms = ARMED + 500_000 + MERGE_ARM_GRACE_MS;
        let again = fires(&rearmed, &context).pop().unwrap();
        assert_eq!(again.fire_keys, ["dropped:feature-1:1500000"]);
        assert_ne!(again.fire_keys, fire.fire_keys);
        assert_eq!(again.detail, "auto-merge was dropped on #42");

        // The likely cause is named when the row already shows it.
        let mut caused = rearmed;
        let pr = caused.pr.as_mut().unwrap();
        pr.checks_failed = true;
        pr.failed_checks = vec!["test".into(), "lint".into()];
        pr.head_sha = Some("def67890".into());
        caused.conflict = Some(crate::types::AutomationConflict {
            base: "origin/main".into(),
            conflicted: true,
        });
        assert_eq!(
            fires(&caused, &context)[0].detail,
            "auto-merge was dropped on #42 (checks failing: test, lint; conflicts with main; new commits were pushed after it was armed)"
        );

        // A paused or busy worktree is not evaluated at all.
        caused.busy = true;
        assert!(fires(&caused, &context).is_empty());
    }

    #[test]
    fn verification_due_waits_for_its_stage_and_fires_once_per_obligation() {
        let due = rule("verify", AutomationTrigger::StatusVerificationDue, "verify");
        let mut context = AutomationEvalContext {
            separate_production: true,
            ..AutomationEvalContext::default()
        };
        let mut row = row();
        let mut work = WorkStatusRecord::new(WorkState::Ready, "2026-01-01T00:00:00Z");
        work.verify_after_merge = Some("check prod".into());
        work.verify_in = Some(wt_core::VerifyStage::Production);
        row.work = Some(work.clone());
        // Not landed: nothing owed yet.
        assert!(
            evaluate(
                std::slice::from_ref(&due),
                std::slice::from_ref(&row),
                &context
            )
            .is_empty()
        );
        // In staging only: a production check is not due.
        row.local_merged = Some(true);
        assert!(
            evaluate(
                std::slice::from_ref(&due),
                std::slice::from_ref(&row),
                &context
            )
            .is_empty()
        );
        row.in_production = true;
        let fire = evaluate(
            std::slice::from_ref(&due),
            std::slice::from_ref(&row),
            &context,
        )
        .pop()
        .unwrap();
        assert_eq!(fire.detail, "verify in production: check prod");
        let key = fire.fire_keys[0].clone();
        assert!(key.starts_with("verify:verify:feature-1:production:"));
        // Its own failure report re-asserts the status; the key is unchanged.
        let mut failed = work.clone();
        failed.state = WorkState::NeedsHuman;
        failed.at = "2026-01-02T00:00:00Z".into();
        row.work = Some(failed);
        let again = evaluate(
            std::slice::from_ref(&due),
            std::slice::from_ref(&row),
            &context,
        );
        assert_eq!(again[0].fire_keys, std::slice::from_ref(&key));
        // Verified discharges it.
        let mut verified = work.clone();
        verified.state = WorkState::Verified;
        row.work = Some(verified);
        assert!(
            evaluate(
                std::slice::from_ref(&due),
                std::slice::from_ref(&row),
                &context
            )
            .is_empty()
        );
        // A staging check is due on the base landing alone.
        let mut staging = work;
        staging.verify_in = Some(wt_core::VerifyStage::Staging);
        row.work = Some(staging);
        row.in_production = false;
        let fire = evaluate(
            std::slice::from_ref(&due),
            std::slice::from_ref(&row),
            &context,
        )
        .pop()
        .unwrap();
        assert!(fire.fire_keys[0].starts_with("verify:verify:feature-1:staging:"));
        // Without a separate production branch the base landing is the release.
        context.separate_production = false;
        let mut production = row.work.clone().unwrap();
        production.verify_in = Some(wt_core::VerifyStage::Production);
        row.work = Some(production);
        assert_eq!(evaluate(&[due], &[row], &context).len(), 1);
    }

    #[test]
    fn verification_due_waits_for_a_probed_stage_to_deploy_and_overdue_does_not() {
        let rules = [
            rule("verify", AutomationTrigger::StatusVerificationDue, "verify"),
            rule(
                "nag",
                AutomationTrigger::StatusVerificationOverdue,
                "notify",
            ),
        ];
        let mut context = AutomationEvalContext {
            separate_production: true,
            now_ms: wt_core::parse_iso_millis("2026-01-01T01:00:00Z").unwrap(),
            local_day: "2026-01-01".into(),
            ..AutomationEvalContext::default()
        };
        let mut row = row();
        let mut work = WorkStatusRecord::new(WorkState::Ready, "2026-01-01T00:00:00Z");
        work.verify_after_merge = Some("check staging".into());
        work.verify_in = Some(wt_core::VerifyStage::Staging);
        row.work = Some(work.clone());
        row.local_merged = Some(true);
        let fired = |row: &AutomationRow, context: &AutomationEvalContext| {
            evaluate(&rules, std::slice::from_ref(row), context)
                .into_iter()
                .map(|fire| fire.rule.id)
                .collect::<Vec<_>>()
        };
        // Landed in Git; the staging probe has not shown it deployed.
        row.deploys.staging = Some(false);
        assert!(fired(&row, &context).is_empty());
        row.deploys.staging = Some(true);
        assert_eq!(fired(&row, &context), ["verify"]);
        // The obligation's key does not depend on how it came due.
        let probed = evaluate(&rules, std::slice::from_ref(&row), &context)[0]
            .fire_keys
            .clone();
        row.deploys.staging = None;
        assert_eq!(
            evaluate(&rules, std::slice::from_ref(&row), &context)[0].fire_keys,
            probed
        );

        // A production check waits for the promotion, then for its deploy.
        work.verify_in = Some(wt_core::VerifyStage::Production);
        row.work = Some(work);
        row.deploys = wt_core::StageDeploys {
            staging: None,
            production: Some(false),
        };
        assert!(fired(&row, &context).is_empty());
        row.in_production = true;
        assert!(fired(&row, &context).is_empty());
        row.deploys.production = Some(true);
        assert_eq!(fired(&row, &context), ["verify"]);

        // A probe that stopped answering cannot hide a check owed for days.
        row.deploys.production = Some(false);
        context.now_ms += 3 * 86_400_000;
        assert_eq!(fired(&row, &context), ["nag"]);
    }

    #[test]
    fn stage_deployed_needs_a_prior_watermark_and_fires_once_per_deployed_commit() {
        let staging = AutomationDef {
            stage: Some(wt_core::VerifyStage::Staging),
            ..rule("released", AutomationTrigger::StageDeployed, "announce")
        };
        let deployed_ref = wt_core::VerifyStage::Staging.deployed_ref();
        let mut context = AutomationEvalContext::default();
        // No probe result yet: nothing to compare.
        assert!(evaluate(std::slice::from_ref(&staging), &[], &context).is_empty());
        // First sight only sets the baseline.
        context.branch_tips.insert(
            deployed_ref.clone(),
            crate::types::BranchTip {
                now: "abcdef123".into(),
                seen: None,
            },
        );
        assert!(evaluate(std::slice::from_ref(&staging), &[], &context).is_empty());
        context.branch_tips.insert(
            deployed_ref.clone(),
            crate::types::BranchTip {
                now: "abcdef123".into(),
                seen: Some("abcdef123".into()),
            },
        );
        assert!(evaluate(std::slice::from_ref(&staging), &[], &context).is_empty());
        context.branch_tips.insert(
            deployed_ref.clone(),
            crate::types::BranchTip {
                now: "abcdef123".into(),
                seen: Some("123456789".into()),
            },
        );
        let fire = evaluate(std::slice::from_ref(&staging), &[], &context)
            .pop()
            .unwrap();
        assert_eq!(fire.slug, FLEET_SLUG);
        assert_eq!(
            fire.fire_keys,
            ["released:deployed:staging:123456789:abcdef123"]
        );
        // Rolled back and forward again: the same commit, another deploy.
        context.branch_tips.insert(
            deployed_ref.clone(),
            crate::types::BranchTip {
                now: "abcdef123".into(),
                seen: Some("fedcba987".into()),
            },
        );
        assert_ne!(
            evaluate(std::slice::from_ref(&staging), &[], &context)[0].fire_keys,
            fire.fire_keys
        );
        assert_eq!(fire.detail, "staging deployed 1234567..abcdef1");
        assert!(fire.quiesce_slugs.is_empty());
        let range = fire.branch_range.unwrap();
        assert_eq!(
            (range.branch, range.from, range.to),
            (deployed_ref, "123456789".into(), "abcdef123".into())
        );
        // The other stage has its own watermark, and a branch rule never
        // reads a stage's.
        let production = AutomationDef {
            stage: Some(wt_core::VerifyStage::Production),
            ..staging.clone()
        };
        let branch = AutomationDef {
            branch: Some("main".into()),
            ..rule("advance", AutomationTrigger::BranchAdvanced, "publish")
        };
        assert!(evaluate(&[production, branch], &[], &context).is_empty());
        // A deploy is an occurrence: nothing observed later clears its breaker.
        assert!(evaluate_breaker_resets(&[staging], &[], &context).is_empty());
    }

    #[test]
    fn pr_rules_require_fresh_github_and_key_by_rule_slug_and_head() {
        let rules = [
            rule("ci", AutomationTrigger::PrChecksFailed, "fix-ci"),
            rule(
                "review",
                AutomationTrigger::ReviewBotUnresolved,
                "fix-review",
            ),
            rule(
                "changes",
                AutomationTrigger::ReviewChangesRequested,
                "fix-review",
            ),
        ];
        let mut context = AutomationEvalContext {
            reviewers_enabled: true,
            ..AutomationEvalContext::default()
        };
        assert!(evaluate(&rules, &[row()], &context).is_empty());
        context.github_fresh = true;
        let fires = evaluate(&rules, &[row()], &context);
        assert_eq!(fires.len(), 3);
        assert_eq!(fires[0].fire_keys[0], "ci:ci:feature-1:abc12345");
        assert_eq!(fires[1].fire_keys[0], "review:rabbit:feature-1:abc12345");
        assert_eq!(fires[2].fire_keys[0], "changes:review:feature-1:abc12345");
    }

    #[test]
    fn ready_echo_and_blocked_gate_are_suppressed_but_other_status_fires() {
        let mut row = row();
        let mut work = WorkStatusRecord::new(WorkState::Ready, "2026-10-01T00:00:00Z");
        work.by = Some("manager".into());
        row.work = Some(work.clone());
        let ready = rule("ready", AutomationTrigger::StatusReady, "manager-brief");
        let mut context = AutomationEvalContext::default();
        context.actions.insert(
            "manager-brief".into(),
            crate::types::ActionTraits {
                audience: crate::types::ActionAudience::Manager,
                ..Default::default()
            },
        );
        assert!(
            evaluate(
                std::slice::from_ref(&ready),
                std::slice::from_ref(&row),
                &context
            )
            .is_empty()
        );
        work.by = None;
        work.blocked_on = Some("approval".into());
        row.work = Some(work);
        assert!(evaluate(&[ready], &[row], &context).is_empty());
    }

    #[test]
    fn verified_fires_once_per_assertion_and_an_external_run_outlives_the_row() {
        let mut row = row();
        row.work = Some(WorkStatusRecord::new(
            WorkState::Verified,
            "2026-10-01T00:00:00Z",
        ));
        let mut context = AutomationEvalContext::default();
        context.row_vars.insert(
            row.slug.clone(),
            BTreeMap::from([("issue_id".into(), "WK-1".into())]),
        );
        context.actions.insert(
            "external-update".into(),
            crate::types::ActionTraits {
                shell: true,
                external: true,
                ..Default::default()
            },
        );
        let rules = [
            rule("done", AutomationTrigger::StatusVerified, "external-update"),
            rule("brief", AutomationTrigger::StatusVerified, "manager-brief"),
            rule("ready", AutomationTrigger::StatusReady, "external-update"),
        ];
        let fires = evaluate(&rules, std::slice::from_ref(&row), &context);
        assert_eq!(fires.len(), 2, "a verified row is not ready");
        assert_eq!(
            fires[0].fire_keys,
            [format!("done:work:{}:2026-10-01T00:00:00Z", row.slug)]
        );
        assert!(fires[0].quiesce_slugs.is_empty());
        assert_eq!(fires[0].frozen_vars.as_ref().unwrap()["issue_id"], "WK-1");
        // A run into the worktree's session still needs the worktree.
        assert_eq!(fires[1].quiesce_slugs, [row.slug.clone()]);
        assert!(fires[1].frozen_vars.is_none());
    }

    #[test]
    fn mcp_events_route_to_the_guide_owner_else_the_manager_and_key_by_event() {
        let rule = AutomationDef {
            id: "guide-comments".into(),
            on: AutomationTrigger::McpEvent,
            run: "answer".into(),
            server: Some("boo".into()),
            event: Some("comment.created".into()),
            ..AutomationDef::default()
        };
        let state = serde_json::json!({"mcpEvents": {"guide-comments": {
            "subscription": crate::mcp_subscription(&rule),
            "pending": [
                {"eventId": "evt_1", "name": "comment.created", "data": {"artifactId": "a9ib", "url": "https://Cozee.artifacts.boo/a/a9ib", "title": "Guide", "by": "p@x.com"}},
                {"eventId": "evt_2", "name": "comment.created", "data": {"artifactId": "other", "url": "https://cozee.artifacts.boo/a/other"}},
                {"eventId": "evt_3", "name": "comment.created", "data": {"artifactId": "a9ib"}}
            ]
        }}});
        let servers = BTreeMap::from([(
            "boo".to_owned(),
            wt_config::McpServerConfig {
                url: "https://cozee.artifacts.boo/mcp".into(),
                token_command: None,
            },
        )]);
        let mut context = AutomationEvalContext {
            mcp_events: crate::pending_mcp_events(std::slice::from_ref(&rule), &servers, &state),
            guides: vec![GuideOwner {
                slug: "feature-1".into(),
                url: "https://cozee.artifacts.boo/a/a9ib/".into(),
            }],
            ..AutomationEvalContext::default()
        };
        let fires = evaluate(std::slice::from_ref(&rule), &[row()], &context);
        let routed = fires
            .iter()
            .map(|fire| (fire.fire_keys[0].as_str(), fire.slug.as_str()))
            .collect::<Vec<_>>();
        assert_eq!(
            routed,
            [
                ("guide-comments:mcp:evt_1", "feature-1"),
                ("guide-comments:mcp:evt_2", FLEET_SLUG),
                ("guide-comments:mcp:evt_3", "feature-1"),
            ]
        );
        assert_eq!(fires[0].quiesce_slugs, ["feature-1"]);
        assert!(fires[1].quiesce_slugs.is_empty());
        assert_eq!(fires[0].detail, "comment.created on Guide by p@x.com");

        // A paused owner holds its events; the manager does not get them.
        context.pauses.slugs.insert("feature-1".into());
        let fires = evaluate(std::slice::from_ref(&rule), &[row()], &context);
        assert_eq!(fires.len(), 1);
        assert_eq!(fires[0].slug, FLEET_SLUG);

        // A page two worktrees recorded has no owner.
        context.pauses.slugs.clear();
        context.guides.push(GuideOwner {
            slug: "feature-2".into(),
            url: "https://cozee.artifacts.boo/a/a9ib".into(),
        });
        let fires = evaluate(std::slice::from_ref(&rule), &[row()], &context);
        assert!(fires.iter().all(|fire| fire.slug == FLEET_SLUG));
        // An id alone never matches a guide on another host.
        assert_eq!(
            guide_owner(
                &serde_json::json!({"artifactId": "12"}),
                "https://cozee.artifacts.boo/mcp",
                &[GuideOwner {
                    slug: "x".into(),
                    url: "https://github.com/o/r/pull/12".into()
                }]
            ),
            None
        );

        // A global pause cancels other fires but leaves events pending.
        let cancellable = crate::cancellable_fires(crate::CancellableFiresInput {
            rules: std::slice::from_ref(&rule),
            rows: &[row()],
            pending: &[],
            context: &context,
            paused: true,
            state_ready: true,
            handled: &BTreeSet::new(),
            executing: &BTreeSet::new(),
        });
        assert!(cancellable.is_empty());

        // Events recorded under other arguments belong to an old cursor.
        let mut edited = rule.clone();
        edited
            .arguments
            .insert("artifact".into(), serde_json::json!("a9ib"));
        assert!(crate::pending_mcp_events(&[edited], &servers, &state).is_empty());
    }

    #[test]
    fn a_status_the_human_set_does_not_notify_the_human() {
        let mut row = row();
        let mut work = WorkStatusRecord::new(WorkState::Ready, "2026-10-01T00:00:00Z");
        work.by = Some("human".into());
        row.work = Some(work.clone());
        let notify = rule("ping", AutomationTrigger::StatusReady, "builtin:notify");
        let context = AutomationEvalContext::default();
        assert!(
            evaluate(
                std::slice::from_ref(&notify),
                std::slice::from_ref(&row),
                &context
            )
            .is_empty()
        );
        work.by = Some(row.slug.clone());
        row.work = Some(work);
        assert_eq!(evaluate(&[notify], &[row], &context).len(), 1);
    }

    #[test]
    fn merged_external_fire_freezes_issue_and_variables_and_ignores_stack_membership() {
        let mut row = row();
        row.local_merged = Some(true);
        row.clean_candidate = true;
        row.github_issue = Some(19);
        row.stack = Some(AutomationStack {
            id: "root".into(),
            parent: None,
        });
        let mut context = AutomationEvalContext::default();
        context.row_vars.insert(
            row.slug.clone(),
            BTreeMap::from([("issue_id".into(), "WK-1".into())]),
        );
        context.actions.insert(
            "external-update".into(),
            crate::types::ActionTraits {
                shell: true,
                external: true,
                ..Default::default()
            },
        );
        let rules = [
            rule("close", AutomationTrigger::WtMerged, "builtin:close-issue"),
            rule("update", AutomationTrigger::WtMerged, "external-update"),
        ];
        let fires = evaluate(&rules, &[row], &context);
        assert_eq!(fires.len(), 2);
        assert_eq!(fires[0].close_issue, Some(19));
        assert!(fires[0].quiesce_slugs.is_empty());
        assert_eq!(fires[1].frozen_vars.as_ref().unwrap()["issue_id"], "WK-1");
    }

    #[test]
    fn merged_pr_fires_external_actions_for_a_retained_worktree() {
        let mut row = row();
        row.pr.as_mut().unwrap().state = "MERGED".into();
        // Retained: an owed post-merge check keeps it from being cleanable.
        row.clean_candidate = false;
        let mut context = AutomationEvalContext {
            github_fresh: true,
            ..AutomationEvalContext::default()
        };
        context.actions.insert(
            "tracker".into(),
            crate::types::ActionTraits {
                shell: true,
                external: true,
                ..Default::default()
            },
        );
        let rules = [
            rule("close", AutomationTrigger::WtMerged, "builtin:close-issue"),
            rule("track", AutomationTrigger::WtMerged, "tracker"),
        ];
        let fires = evaluate(&rules, std::slice::from_ref(&row), &context);
        assert_eq!(fires.len(), 1, "only the external action fires");
        assert_eq!(fires[0].rule.id, "track");
        assert_eq!(fires[0].fire_keys[0], "track:merged:feature-1:42");
        // Without GitHub's word the landing is unproven for a retained row.
        context.github_fresh = false;
        assert!(evaluate(&rules, &[row], &context).is_empty());
    }

    #[test]
    fn branch_advanced_needs_a_prior_watermark_and_status_daily_key_is_local_day() {
        let branch = rule("release", AutomationTrigger::BranchAdvanced, "publish");
        let mut context = AutomationEvalContext::default();
        context.branch_tips.insert(
            "main".into(),
            crate::types::BranchTip {
                now: "abcdef123".into(),
                seen: None,
            },
        );
        assert!(evaluate(std::slice::from_ref(&branch), &[], &context).is_empty());
        context.branch_tips.insert(
            "main".into(),
            crate::types::BranchTip {
                now: "abcdef123".into(),
                seen: Some("123456789".into()),
            },
        );
        let branch = AutomationDef {
            branch: Some("main".into()),
            ..branch
        };
        let fire = evaluate(&[branch], &[], &context).pop().unwrap();
        assert_eq!(fire.slug, FLEET_SLUG);
        assert_eq!(fire.branch_range.unwrap().from, "123456789");

        let mut row = row();
        row.local_merged = Some(true);
        let mut work = WorkStatusRecord::new(WorkState::Working, "2026-01-01T00:00:00Z");
        work.risk = Some(WorkRisk::High);
        work.verify_after_merge = Some("check deployed".into());
        row.work = Some(work);
        let overdue = rule(
            "verify",
            AutomationTrigger::StatusVerificationOverdue,
            "notify",
        );
        context.now_ms = 1_800_000_000_000;
        context.local_day = "2026-10-09".into();
        let fire = evaluate(&[overdue], &[row], &context).pop().unwrap();
        assert_eq!(fire.fire_keys[0], "verify:unverified:feature-1:2026-10-09");
    }

    #[test]
    fn stack_parent_fire_is_once_per_landed_parent_and_pause_protects_whole_stack() {
        use crate::types::StackParent;
        let rule = rule(
            "restack",
            AutomationTrigger::StackParentMerged,
            "builtin:restack",
        );
        let mut merged = row();
        merged.slug = "parent".into();
        merged.branch = "parent-branch".into();
        merged.clean_candidate = true;
        merged.stack = Some(AutomationStack {
            id: "root".into(),
            parent: None,
        });
        let mut open = row();
        open.slug = "child".into();
        open.branch = "child-branch".into();
        open.clean_candidate = false;
        open.stack = Some(AutomationStack {
            id: "root".into(),
            parent: Some(StackParent {
                branch: "parent-branch".into(),
                slug: Some("parent".into()),
            }),
        });
        let rows = [merged, open];
        let mut context = AutomationEvalContext {
            github_fresh: true,
            ..AutomationEvalContext::default()
        };
        let fires = evaluate(std::slice::from_ref(&rule), &rows, &context);
        assert_eq!(fires.len(), 1);
        assert_eq!(fires[0].slug, "child");
        assert_eq!(fires[0].stack_id.as_deref(), Some("root"));
        assert_eq!(fires[0].fire_keys, ["restack:restack:root:42"]);
        assert_eq!(fires[0].quiesce_slugs, ["parent", "child"]);
        context.pauses.slugs.insert("parent".into());
        assert!(evaluate(&[rule], &rows, &context).is_empty());
    }

    #[test]
    fn merged_claim_requires_nonvacuous_lifecycle_evidence_and_close_issue_can_use_gh_slug() {
        let rule = rule("close", AutomationTrigger::WtMerged, "builtin:close-issue");
        let mut row = row();
        row.slug = "GH-73".into();
        row.local_merged = Some(true);
        row.clean_candidate = false;
        let context = AutomationEvalContext::default();
        assert!(
            evaluate(
                std::slice::from_ref(&rule),
                std::slice::from_ref(&row),
                &context
            )
            .is_empty()
        );
        row.clean_candidate = true;
        let fire = evaluate(&[rule], &[row], &context).pop().unwrap();
        assert_eq!(fire.close_issue, Some(73));
        assert!(fire.quiesce_slugs.is_empty());
    }

    #[tokio::test]
    async fn one_landing_closes_its_issue_once_whether_or_not_the_pr_is_observed() {
        use crate::ledger::{AutomationLedger, DispatchKind};
        use tokio_util::sync::CancellationToken;

        let rule = rule("close", AutomationTrigger::WtMerged, "builtin:close-issue");
        let mut row = row();
        row.created_at = Some("2026-10-20T17:56:35Z".into());
        row.github_issue = Some(2642);
        row.local_merged = Some(true);
        row.clean_candidate = true;
        let context = AutomationEvalContext {
            github_fresh: true,
            ..AutomationEvalContext::default()
        };
        let dir = tempfile::tempdir().unwrap();
        let ledger = AutomationLedger::new(dir.path().join("automations.json"));
        let cancel = CancellationToken::new();
        // The ledger prunes against the wall clock.
        let now = (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64;
        let claim = |fire: AutomationFire| {
            let ledger = &ledger;
            let cancel = &cancel;
            async move {
                ledger
                    .claim_dispatch(
                        &fire.fire_keys,
                        &fire.rule.id,
                        &fire.slug,
                        DispatchKind::HeadlessAction,
                        now,
                        cancel,
                    )
                    .await
                    .unwrap()
            }
        };

        // Landed on trunk while the open PR is still in the snapshot.
        let first = evaluate(
            std::slice::from_ref(&rule),
            std::slice::from_ref(&row),
            &context,
        )
        .pop()
        .unwrap();
        assert_eq!(first.close_issue, Some(2642));
        let delivered = claim(first).await.unwrap();
        ledger
            .mark_delivered(&delivered, now, &cancel)
            .await
            .unwrap();

        // A human reopens the issue; the PR then drops out of the snapshot.
        // The same landing seen without its PR must not close it again.
        row.pr = None;
        let again = evaluate(
            std::slice::from_ref(&rule),
            std::slice::from_ref(&row),
            &context,
        )
        .pop()
        .unwrap();
        assert!(claim(again).await.is_none());

        // `wt next` lands a follow-up on a new branch: that is a new landing.
        row.branch = "feature-1-2".into();
        let follow_up = evaluate(&[rule], &[row], &context).pop().unwrap();
        assert!(claim(follow_up).await.is_some());
    }

    #[test]
    fn breaker_resets_require_an_observable_clear_condition() {
        let ci = rule("ci", AutomationTrigger::PrChecksFailed, "fix-ci");
        let ready = rule("ready", AutomationTrigger::StatusReady, "manager-brief");
        let mut ci_row = row();
        ci_row.pr.as_mut().unwrap().checks_failed = false;
        let mut ready_row = row();
        let mut work = WorkStatusRecord::new(WorkState::Ready, "2026-10-01T00:00:00Z");
        work.by = Some("manager".into());
        ready_row.work = Some(work);
        let mut context = AutomationEvalContext {
            github_fresh: true,
            ..AutomationEvalContext::default()
        };
        context.actions.insert(
            "manager-brief".into(),
            crate::types::ActionTraits {
                audience: crate::types::ActionAudience::Manager,
                ..Default::default()
            },
        );

        // A fresh observation that checks passed clears CI's breaker.
        assert!(
            evaluate_breaker_resets(
                std::slice::from_ref(&ci),
                std::slice::from_ref(&ci_row),
                &context,
            )
            .contains(&("ci".into(), "feature-1".into()))
        );
        // A status audience echo is a still-true condition, even though the
        // evaluator correctly suppresses the duplicate briefing.
        assert!(
            !evaluate_breaker_resets(
                std::slice::from_ref(&ready),
                std::slice::from_ref(&ready_row),
                &context,
            )
            .contains(&("ready".into(), "feature-1".into()))
        );
        // Changing the status is an observed clear.
        ready_row.work.as_mut().unwrap().state = WorkState::Working;
        assert!(
            evaluate_breaker_resets(
                std::slice::from_ref(&ready),
                std::slice::from_ref(&ready_row),
                &context,
            )
            .contains(&("ready".into(), "feature-1".into()))
        );
    }

    #[test]
    fn breaker_resets_skip_stale_and_ineligible_rows() {
        let ci = rule("ci", AutomationTrigger::PrChecksFailed, "fix-ci");
        let local = rule("conflict", AutomationTrigger::PrConflict, "fix-conflict");
        let mut row = row();
        row.pr.as_mut().unwrap().checks_failed = false;
        row.conflict = Some(crate::types::AutomationConflict {
            base: "origin/main".into(),
            conflicted: false,
        });
        let context = AutomationEvalContext::default();
        assert!(
            evaluate_breaker_resets(
                std::slice::from_ref(&ci),
                std::slice::from_ref(&row),
                &context,
            )
            .is_empty()
        );
        // Conflict observations on a PR-backed row also need fresh GitHub
        // state, while PR-less probes remain locally observable.
        assert!(
            evaluate_breaker_resets(
                std::slice::from_ref(&local),
                std::slice::from_ref(&row),
                &context,
            )
            .is_empty()
        );
        row.pr = None;
        assert!(
            evaluate_breaker_resets(
                std::slice::from_ref(&local),
                std::slice::from_ref(&row),
                &context,
            )
            .contains(&("conflict".into(), "feature-1".into()))
        );

        row.busy = true;
        assert!(
            evaluate_breaker_resets(
                std::slice::from_ref(&local),
                std::slice::from_ref(&row),
                &context,
            )
            .is_empty()
        );
        row.busy = false;
        let mut context = context;
        context.pauses.slugs.insert("feature-1".into());
        assert!(evaluate_breaker_resets(&[local], &[row], &context).is_empty());
    }

    fn met_watch(id: &str, slug: &str, actions: &[WatchAction]) -> crate::types::MetWatch {
        crate::types::MetWatch {
            id: id.into(),
            slug: slug.into(),
            by: "manager".into(),
            held: "#12 merged".into(),
            actions: actions
                .iter()
                .map(|action| (*action, "text".to_owned()))
                .collect(),
            cleared_gate: None,
        }
    }

    #[test]
    fn a_met_watch_fires_once_per_owed_action_without_any_rule() {
        let live = AutomationRow {
            slug: "one".into(),
            ..AutomationRow::default()
        };
        let mut context = AutomationEvalContext {
            watches: vec![
                met_watch("aaaaaa", "one", &WatchAction::ALL),
                met_watch("bbbbbb", "one", &[WatchAction::Notify]),
                // Its worktree is gone: telling the person still happens,
                // waking a session cannot.
                met_watch("cccccc", "gone", &[WatchAction::Notify, WatchAction::Wake]),
            ],
            ..AutomationEvalContext::default()
        };
        let fires = evaluate(&[], std::slice::from_ref(&live), &context);
        let seen = |fires: &[AutomationFire]| {
            fires
                .iter()
                .map(|fire| fire.fire_keys[0].clone())
                .collect::<Vec<_>>()
        };
        // The wake waits until the same watch has cleared its gate.
        assert_eq!(
            seen(&fires),
            [
                "wt.watch:aaaaaa:unblock",
                "wt.watch:aaaaaa:notify",
                "wt.watch:bbbbbb:notify",
                "wt.watch:cccccc:notify",
            ]
        );
        assert!(fires.iter().all(|fire| fire.quiesce_slugs.is_empty()));
        let identities: BTreeSet<_> = fires.iter().map(fire_identity).collect();
        assert_eq!(identities.len(), fires.len());
        assert_eq!(fires[0].detail, "unblock: #12 merged");
        assert!(evaluate_breaker_resets(&[], std::slice::from_ref(&live), &context).is_empty());

        context.watches[0].actions.remove(0);
        let fires = evaluate(&[], std::slice::from_ref(&live), &context);
        assert_eq!(fires[1].fire_keys, ["wt.watch:aaaaaa:wake"]);
        assert_eq!(fires[1].quiesce_slugs, ["one"]);
        let archived = AutomationRow {
            archived: true,
            ..live.clone()
        };
        assert!(
            !seen(&evaluate(&[], &[archived], &context))
                .contains(&"wt.watch:aaaaaa:wake".to_owned())
        );

        // A paused worktree holds everything of its watches.
        context.pauses.slugs.insert("one".into());
        assert_eq!(
            seen(&evaluate(&[], &[live], &context)),
            ["wt.watch:cccccc:notify"]
        );
    }
}
