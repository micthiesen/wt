use std::collections::{BTreeMap, BTreeSet};

use wt_config::AutomationDef;

use crate::{
    evaluate, fire_identity,
    types::{AutomationEvalContext, AutomationFire, AutomationIntent, AutomationRow},
};

pub struct CancellableFiresInput<'a> {
    pub paused: bool,
    pub state_ready: bool,
    pub rules: &'a [AutomationDef],
    pub rows: &'a [AutomationRow],
    pub context: &'a AutomationEvalContext,
    pub pending: &'a [AutomationFire],
    pub executing: &'a BTreeSet<String>,
    pub handled: &'a BTreeSet<String>,
}

pub struct QueueIntentsInput<'a> {
    pub existing: &'a [AutomationIntent],
    pub current: &'a [AutomationFire],
    pub rows: &'a [AutomationRow],
    pub context: &'a AutomationEvalContext,
    pub now_ms: i64,
    pub paused: bool,
    pub state_ready: bool,
    pub handled: &'a BTreeSet<String>,
    pub executing: &'a BTreeSet<String>,
}

/// Reconstruct exact cancellable fires while paused. The queue itself is
/// ephemeral, but cancellation writes these keys to the durable ledger before
/// the caller removes its pending intents.
pub fn cancellable_fires(input: CancellableFiresInput<'_>) -> Vec<AutomationFire> {
    let CancellableFiresInput {
        paused,
        state_ready,
        rules,
        rows,
        context,
        pending,
        executing,
        handled,
    } = input;
    if !state_ready {
        return Vec::new();
    }
    let candidates: Vec<_> = if paused {
        pending
            .iter()
            .cloned()
            .chain(evaluate(rules, rows, context))
            .collect()
    } else {
        pending.to_vec()
    };
    let mut merged: BTreeMap<String, AutomationFire> = BTreeMap::new();
    for mut fire in candidates {
        let id = fire_identity(&fire);
        if executing.contains(&id) {
            continue;
        }
        fire.fire_keys.retain(|key| !handled.contains(key));
        if fire.fire_keys.is_empty() {
            continue;
        }
        if let Some(prior) = merged.get_mut(&id) {
            let keys: BTreeSet<_> = prior
                .fire_keys
                .iter()
                .chain(&fire.fire_keys)
                .cloned()
                .collect();
            prior.fire_keys = keys.into_iter().collect();
        } else {
            merged.insert(id, fire);
        }
    }
    merged.into_values().collect()
}

/// Upsert current fires while preserving settle age for unchanged key sets,
/// drop ordinary intents whose level condition cleared, and retain frozen
/// post-merge external intent if its row disappeared during the settle window.
pub fn queue_intents(input: QueueIntentsInput<'_>) -> Vec<AutomationIntent> {
    let QueueIntentsInput {
        existing,
        current,
        rows,
        context,
        now_ms,
        paused,
        state_ready,
        handled,
        executing,
    } = input;
    if paused || !state_ready {
        return Vec::new();
    }
    let by_id: BTreeMap<_, _> = current
        .iter()
        .map(|fire| (fire_identity(fire), fire))
        .collect();
    let row_by_slug: BTreeMap<_, _> = rows.iter().map(|row| (row.slug.as_str(), row)).collect();
    let mut output = BTreeMap::new();
    for fire in current {
        let id = fire_identity(fire);
        if executing.contains(&id) {
            continue;
        }
        let keys: Vec<_> = fire
            .fire_keys
            .iter()
            .filter(|key| !handled.contains(*key))
            .cloned()
            .collect();
        if keys.is_empty() {
            continue;
        }
        let mut fire = fire.clone();
        fire.fire_keys = keys;
        let old = existing
            .iter()
            .find(|intent| fire_identity(&intent.fire) == id);
        let old_keys = old.map(|intent| &intent.fire.fire_keys);
        let same = old_keys.is_some_and(|old| old == &fire.fire_keys);
        let settle_ms = (fire.rule.settle_seconds.max(0.0) * 1000.0).min(i64::MAX as f64) as i64;
        let queued_at_ms = if same {
            old.unwrap().queued_at_ms
        } else {
            now_ms
        };
        output.insert(
            id,
            AutomationIntent {
                fire,
                queued_at_ms,
                settle_at_ms: queued_at_ms.saturating_add(settle_ms),
            },
        );
    }

    for intent in existing {
        let fire = &intent.fire;
        let id = fire_identity(fire);
        if output.contains_key(&id) || executing.contains(&id) || by_id.contains_key(&id) {
            continue;
        }
        if !is_frozen_external_fire(fire) {
            continue;
        }
        if context.pauses.slugs.contains(&fire.slug) {
            continue;
        }
        match row_by_slug.get(fire.slug.as_str()) {
            None => {
                output.insert(id, intent.clone());
            }
            Some(row)
                if !row.archived && !row.busy && !context.pauses.slugs.contains(&fire.slug) =>
            {
                let _ = row;
            }
            Some(_) if context.pauses.slugs.contains(&fire.slug) => {}
            _ => {
                output.insert(id, intent.clone());
            }
        }
    }
    output.into_values().collect()
}

fn is_frozen_external_fire(fire: &AutomationFire) -> bool {
    fire.quiesce_slugs.is_empty()
        && (fire.close_issue.is_some()
            || fire.delete_branch.is_some()
            || fire.frozen_vars.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{AutomationEvalContext, AutomationRow};
    use wt_config::{AutomationBusyPolicy, AutomationDef, AutomationTrigger};

    fn fire(key: &str, slug: &str) -> AutomationFire {
        AutomationFire {
            rule: AutomationDef {
                id: "r".into(),
                on: AutomationTrigger::WtCreated,
                run: "act".into(),
                busy: AutomationBusyPolicy::Queue,
                settle_seconds: 10.0,
                ..AutomationDef::default()
            },
            slug: slug.into(),
            quiesce_slugs: vec![slug.into()],
            fire_keys: vec![key.into()],
            stack_id: None,
            close_issue: None,
            delete_branch: None,
            delete_branch_pr: None,
            branch_range: None,
            frozen_vars: None,
            frozen_pr: None,
            detail: "test".into(),
        }
    }

    #[test]
    fn unchanged_intent_keeps_settle_clock_and_changed_key_restarts_it() {
        let old = AutomationIntent {
            fire: fire("old", "s"),
            queued_at_ms: 100,
            settle_at_ms: 10_100,
        };
        let unchanged = queue_intents(QueueIntentsInput {
            existing: std::slice::from_ref(&old),
            current: std::slice::from_ref(&old.fire),
            rows: &[],
            context: &AutomationEvalContext::default(),
            now_ms: 5_000,
            paused: false,
            state_ready: true,
            handled: &BTreeSet::new(),
            executing: &BTreeSet::new(),
        });
        assert_eq!(unchanged[0].queued_at_ms, 100);
        let changed = queue_intents(QueueIntentsInput {
            existing: &[old],
            current: &[fire("new", "s")],
            rows: &[],
            context: &AutomationEvalContext::default(),
            now_ms: 5_000,
            paused: false,
            state_ready: true,
            handled: &BTreeSet::new(),
            executing: &BTreeSet::new(),
        });
        assert_eq!(changed[0].queued_at_ms, 5_000);
        assert_eq!(changed[0].settle_at_ms, 15_000);
    }

    #[test]
    fn cancellable_pass_merges_unseen_keys_and_filters_executing_or_handled() {
        let pending = [fire("a", "s")];
        let rules = [pending[0].rule.clone()];
        let rows = [AutomationRow::default()];
        let mut handled = BTreeSet::new();
        handled.insert("a".into());
        let result = cancellable_fires(CancellableFiresInput {
            paused: true,
            state_ready: true,
            rules: &rules,
            rows: &rows,
            context: &AutomationEvalContext::default(),
            pending: &pending,
            executing: &BTreeSet::new(),
            handled: &handled,
        });
        assert!(result.is_empty());
        assert!(
            cancellable_fires(CancellableFiresInput {
                paused: false,
                state_ready: false,
                rules: &rules,
                rows: &rows,
                context: &AutomationEvalContext::default(),
                pending: &pending,
                executing: &BTreeSet::new(),
                handled: &BTreeSet::new(),
            })
            .is_empty()
        );
    }
}
