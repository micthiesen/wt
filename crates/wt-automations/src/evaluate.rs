use std::collections::{BTreeMap, BTreeSet};

use wt_config::{AutomationDef, AutomationTrigger};
use wt_core::WorkState;

use crate::types::{
    ActionAudience, AutomationEvalContext, AutomationFire, AutomationRow, BranchRange, FrozenPr,
    GuideOwner, McpEvent, status_is_gated, status_suffix, status_trigger_state,
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
            AutomationTrigger::McpEvent => fires.extend(evaluate_mcp(rule, rows, context)),
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
        if matches!(
            rule.on,
            AutomationTrigger::BranchAdvanced | AutomationTrigger::McpEvent
        ) {
            // Branch movement and MCP events are each a distinct occurrence;
            // no worktree condition clears them.
            continue;
        }
        let github_driven = matches!(
            rule.on,
            AutomationTrigger::PrChecksFailed
                | AutomationTrigger::ReviewBotUnresolved
                | AutomationTrigger::ReviewChangesRequested
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
    match &fire.mcp_event {
        // Each event is its own fire; sharing `rule|slug` would let a burst
        // for one guide replace itself in the queue, newest first.
        Some(event) => format!("{}|{target}|{}", fire.rule.id, event.event_id),
        None => format!("{}|{target}", fire.rule.id),
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
                // The human set it in the TUI; a banner would tell them what
                // they just did.
                || (rule.run == "builtin:notify" && work.by.as_deref() == Some("human"))
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
        AutomationTrigger::StatusVerificationDue => {
            let work = row.work.as_ref()?;
            if !wt_core::verification_due(Some(work), row_landing(row, context)) {
                return None;
            }
            let (stage, steps) = work.verification()?;
            // Keyed by the obligation, not the assertion: a run that fails and
            // re-asserts needs-human must not be rerun by its own status write.
            // New steps are a new check.
            Some(single(
                rule,
                row,
                format!(
                    "{}:verify:{}:{}:{:016x}",
                    rule.id,
                    slug,
                    stage.as_str(),
                    stable_hash(steps)
                ),
                &format!("verify in {}: {steps}", stage.as_str()),
            ))
        }
        AutomationTrigger::StatusVerificationOverdue => {
            let work = row.work.as_ref()?;
            let landing = row_landing(row, context);
            if !wt_core::verification_overdue_with_days(
                Some(work),
                landing,
                context.now_ms,
                rule.after_days,
            ) {
                return None;
            }
            let (stage, verify) = work.verification()?;
            Some(single(
                rule,
                row,
                format!("{}:unverified:{}:{}", rule.id, slug, context.local_day),
                &format!("in {}, verification still owed — {verify}", stage.as_str()),
            ))
        }
        AutomationTrigger::StackParentMerged
        | AutomationTrigger::BranchAdvanced
        | AutomationTrigger::McpEvent => None,
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
            fire_keys: keys.clone(),
            stack_id: None,
            close_issue: None,
            delete_branch: Some(row.branch.clone()),
            delete_branch_pr: row.pr.as_ref().map(|pr| pr.number),
            branch_range: None,
            frozen_vars: None,
            frozen_pr: None,
            mcp_event: None,
            detail: format!("{landed} — deleting remote branch {}", row.branch),
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
            detail: landed,
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
        mcp_event: None,
        detail: format!("{branch} advanced {start}..{end}"),
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
            detail: wt_core::sanitize_terminal_text(&detail),
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
}
