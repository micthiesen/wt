use std::collections::{BTreeMap, BTreeSet};

use time::OffsetDateTime;
use wt_config::{AutomationDef, AutomationTrigger};
use wt_core::WorkState;

use crate::types::{
    ActionAudience, AutomationEvalContext, AutomationFire, AutomationRow, BranchRange, FrozenPr,
    status_is_gated, status_suffix, status_trigger_state,
};

pub const FLEET_SLUG: &str = "_fleet";

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
    let fired: BTreeSet<_> = fires.iter().map(fire_identity).collect();
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
        if rule.on == AutomationTrigger::BranchAdvanced {
            // Fleet branch movement has no worktree whose condition can clear.
            continue;
        }
        let github_driven = matches!(
            rule.on,
            AutomationTrigger::PrChecksFailed
                | AutomationTrigger::ReviewBotUnresolved
                | AutomationTrigger::ReviewChangesRequested
                | AutomationTrigger::WtMerged
                | AutomationTrigger::StackParentMerged
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
    format!(
        "{}|{}",
        fire.rule.id,
        fire.stack_id.as_deref().unwrap_or(&fire.slug)
    )
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
        AutomationTrigger::WtMerged => evaluate_merged(rule, row, context),
        AutomationTrigger::StatusNeedsHuman
        | AutomationTrigger::StatusNeedsTesting
        | AutomationTrigger::StatusReady => {
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
            {
                return None;
            }
            Some(single(
                rule,
                row,
                format!("{}:work:{}:{}", rule.id, slug, work.at),
                &format!("{}{}", work.state.as_str(), status_suffix(work)),
            ))
        }
        AutomationTrigger::StatusVerificationOverdue => {
            let work = row.work.as_ref()?;
            let landed = row.local_merged == Some(true)
                || row.gone == Some(true)
                || (context.github_fresh && row.pr.as_ref().is_some_and(|pr| pr.state == "MERGED"));
            let verify = work.verify_after_merge.as_deref()?;
            if verify.is_empty()
                || !landed
                || matches!(work.state, WorkState::Verified | WorkState::Dropped)
            {
                return None;
            }
            let overdue =
                OffsetDateTime::parse(&work.at, &time::format_description::well_known::Rfc3339)
                    .map(|at| {
                        context
                            .now_ms
                            .saturating_sub((at.unix_timestamp_nanos() / 1_000_000) as i64)
                            >= (rule.after_days * 86_400_000.0) as i64
                    })
                    .unwrap_or(true);
            if !overdue {
                return None;
            }
            Some(single(
                rule,
                row,
                format!("{}:unverified:{}:{}", rule.id, slug, context.local_day),
                &format!("merged, verification still owed — {verify}"),
            ))
        }
        AutomationTrigger::StackParentMerged | AutomationTrigger::BranchAdvanced => None,
    }
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
    if !row.clean_candidate {
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
    let key = format!(
        "{}:merged:{}:{}",
        rule.id,
        row.slug,
        row.pr
            .as_ref()
            .map_or_else(|| "local".into(), |pr| pr.number.to_string())
    );
    if closes_issue {
        let issue = row
            .github_issue
            .or_else(|| github_issue_from_slug(&row.slug))?;
        return Some(AutomationFire {
            rule: rule.clone(),
            slug: row.slug.clone(),
            quiesce_slugs: Vec::new(),
            fire_keys: vec![key],
            stack_id: None,
            close_issue: Some(issue),
            delete_branch: None,
            delete_branch_pr: None,
            branch_range: None,
            frozen_vars: None,
            frozen_pr: None,
            detail: format!("{landed} — closing issue #{issue}"),
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
            fire_keys: vec![key],
            stack_id: None,
            close_issue: None,
            delete_branch: Some(row.branch.clone()),
            delete_branch_pr: row.pr.as_ref().map(|pr| pr.number),
            branch_range: None,
            frozen_vars: None,
            frozen_pr: None,
            detail: format!("{landed} — deleting remote branch {}", row.branch),
        });
    }
    if external {
        return Some(AutomationFire {
            rule: rule.clone(),
            slug: row.slug.clone(),
            quiesce_slugs: Vec::new(),
            fire_keys: vec![key],
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
            detail: landed,
        });
    }
    Some(single(rule, row, key, &landed))
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
            detail: format!("{} under {} open member(s)", parts.join(" + "), open.len()),
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
        detail: format!("{branch} advanced {start}..{end}"),
    })
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
    use wt_core::{WorkRisk, WorkStatusRecord};

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
            }),
            ..AutomationRow::default()
        }
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
}
