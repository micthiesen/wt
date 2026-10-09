//! Transactional mutations for wt's durable JSON state.
//!
//! These pure value transforms preserve keys unknown to this build. Each
//! public `Store` mutation runs against a freshly read snapshot inside one
//! `BEGIN IMMEDIATE` transaction, replacing the old file-lock read/modify/write
//! boundary without exposing a blocking mutex to async callers.

use std::collections::BTreeSet;

use serde_json::{Map, Number, Value, json};

use crate::{
    MergeEdge, RemovedWorktree, ReviewRequestDismissal, Store, StoreError, WorkStatusRecord,
    WtState,
};

const GROUP_INBOX: &str = "\0inbox";
const STACK_PREFIX: &str = "\0stack:";
const REMOVED_MAX_ENTRIES: usize = 30;
const REMOVED_MAX_AGE_MS: i64 = 14 * 24 * 60 * 60 * 1000;
const MAX_REVIEW_REQUEST_DISMISSALS: usize = 200;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

pub(crate) fn empty_state() -> WtState {
    json!({
        "version": crate::CURRENT_WT_STATE_VERSION,
        "slugs": {},
        "remoteLayouts": {},
        "sectionsOrder": [],
        "foldedSections": [],
        "pausedStacks": [],
        "automationsPaused": false,
        "attentionSeenTs": 0,
        "removed": [],
        "reviewRequestDismissals": [],
        "branchTips": {},
        "edges": []
    })
}

pub(crate) fn normalize_state(value: Value) -> WtState {
    let version = crate::raw_wt_state_version(&value);
    let mut state = match value {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    for (key, fallback) in [
        ("slugs", json!({})),
        ("remoteLayouts", json!({})),
        ("sectionsOrder", json!([])),
        ("foldedSections", json!([])),
        ("pausedStacks", json!([])),
        ("removed", json!([])),
        ("reviewRequestDismissals", json!([])),
        ("branchTips", json!({})),
        ("edges", json!([])),
    ] {
        if !state.get(key).is_some_and(|value| match fallback {
            Value::Object(_) => value.is_object(),
            Value::Array(_) => value.is_array(),
            _ => false,
        }) {
            state.insert(key.to_owned(), fallback);
        }
    }
    state
        .entry("automationsPaused")
        .or_insert(Value::Bool(false));
    state
        .entry("attentionSeenTs")
        .or_insert(Value::Number(Number::from(0)));
    let incoming_order = state
        .get("sectionsOrder")
        .and_then(Value::as_array)
        .into_iter()
        .flat_map(|order| order.iter())
        .filter_map(Value::as_str)
        .filter(|key| !key.trim().is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let has_inbox = incoming_order.iter().any(|key| key == GROUP_INBOX);
    let mut order = Vec::<Value>::new();
    let mut seen = BTreeSet::new();
    if !has_inbox {
        order.push(json!(GROUP_INBOX));
        seen.insert(GROUP_INBOX.to_owned());
    }
    for key in incoming_order {
        if !has_inbox && key.starts_with(STACK_PREFIX) {
            continue;
        }
        if seen.insert(key.clone()) {
            order.push(json!(key));
        }
    }
    let referenced = state
        .get("slugs")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|slugs| slugs.values())
        .chain(
            state
                .get("remoteLayouts")
                .and_then(Value::as_object)
                .into_iter()
                .flat_map(|layouts| layouts.values()),
        )
        .filter_map(|entry| entry.get("section")?.as_str())
        .filter(|section| !section.trim().is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    for section in referenced {
        if seen.insert(section.clone()) {
            order.push(json!(section));
        }
    }
    state.insert("sectionsOrder".to_owned(), Value::Array(order));
    let folded = state
        .get("foldedSections")
        .and_then(Value::as_array)
        .into_iter()
        .flat_map(|items| items.iter())
        .filter_map(Value::as_str)
        .filter(|key| !key.trim().is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let mut folded_seen = BTreeSet::new();
    let mut folded_unique = Vec::new();
    for key in folded {
        if folded_seen.insert(key.clone()) {
            folded_unique.push(json!(key));
        }
    }
    state.insert("foldedSections".to_owned(), Value::Array(folded_unique));
    if version <= crate::CURRENT_WT_STATE_VERSION {
        state.insert(
            "version".to_owned(),
            Value::Number(Number::from(crate::CURRENT_WT_STATE_VERSION)),
        );
    }
    Value::Object(state)
}

impl Store {
    pub fn read_slug_dev_port(&mut self, slug: &str) -> Result<Option<u16>, StoreError> {
        let state = self.read_wt_state()?;
        Ok(state
            .get("slugs")
            .and_then(|slugs| slugs.get(slug))
            .and_then(|entry| entry.get("devPort"))
            .and_then(Value::as_u64)
            .and_then(|port| u16::try_from(port).ok()))
    }

    pub fn read_section_order(&mut self) -> Result<Vec<String>, StoreError> {
        let state = self.read_wt_state()?;
        Ok(state
            .get("sectionsOrder")
            .and_then(Value::as_array)
            .into_iter()
            .flat_map(|items| items.iter())
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect())
    }

    pub fn read_removed_worktrees(&mut self) -> Result<Vec<RemovedWorktree>, StoreError> {
        let state = self.read_wt_state()?;
        Ok(state
            .get("removed")
            .and_then(Value::as_array)
            .into_iter()
            .flat_map(|items| items.iter())
            .filter_map(|item| serde_json::from_value(item.clone()).ok())
            .collect())
    }

    pub fn recently_removed_worktrees(
        &mut self,
        live_slugs: &BTreeSet<String>,
        now_ms: i64,
    ) -> Result<Vec<RemovedWorktree>, StoreError> {
        let cutoff = now_ms.saturating_sub(48 * 60 * 60 * 1000);
        let mut entries: Vec<_> = self
            .read_removed_worktrees()?
            .into_iter()
            .filter(|entry| {
                !live_slugs.contains(&entry.slug)
                    && parse_timestamp_ms(&entry.removed_at).is_some_and(|at| at >= cutoff)
            })
            .collect();
        entries.sort_by(|a, b| b.removed_at.cmp(&a.removed_at));
        Ok(entries)
    }

    pub fn read_review_request_dismissals(
        &mut self,
    ) -> Result<Vec<ReviewRequestDismissal>, StoreError> {
        let state = self.read_wt_state()?;
        Ok(state
            .get("reviewRequestDismissals")
            .and_then(Value::as_array)
            .into_iter()
            .flat_map(|items| items.iter())
            .filter_map(|item| serde_json::from_value(item.clone()).ok())
            .collect())
    }

    pub fn is_section_folded(&mut self, section_key: &str) -> Result<bool, StoreError> {
        let state = self.read_wt_state()?;
        Ok(state
            .get("foldedSections")
            .and_then(Value::as_array)
            .is_some_and(|keys| keys.iter().any(|key| key.as_str() == Some(section_key))))
    }

    pub fn read_slug_state(&mut self, slug: &str) -> Result<Option<Value>, StoreError> {
        Ok(self
            .read_wt_state()?
            .get("slugs")
            .and_then(|slugs| slugs.get(slug))
            .cloned())
    }

    pub fn clear_slug_state(&mut self, slug: &str) -> Result<bool, StoreError> {
        self.mutate_wt_state(|state| {
            let slugs = object_mut(state, "slugs");
            if slugs.remove(slug).is_none() {
                return Ok((false, false));
            }
            prune_sections_order(state);
            Ok((true, true))
        })
    }

    pub fn place_slug(
        &mut self,
        slug: &str,
        section: Option<&str>,
        at_top: bool,
    ) -> Result<(), StoreError> {
        self.mutate_wt_state(|state| {
            if let Some(section) = section {
                ensure_section(state, section);
            }
            let section_matches = |value: &Value| {
                value.get("section").and_then(Value::as_str) == section
                    || (section.is_none() && value.get("section").is_some_and(Value::is_null))
            };
            let orders = all_layouts(state)
                .filter(|layout| section_matches(layout))
                .filter_map(|layout| layout.get("order").and_then(Value::as_f64));
            let order = if at_top {
                orders
                    .fold(None, |min, n| Some(min.map_or(n, |m: f64| m.min(n))))
                    .map_or(0.0, |min| min - 1.0)
            } else {
                orders
                    .fold(None, |max, n| Some(max.map_or(n, |m: f64| m.max(n))))
                    .map_or(0.0, |max| max + 1.0)
            };
            let mut entry = state
                .get("slugs")
                .and_then(|slugs| slugs.get(slug))
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            entry.insert(
                "section".to_owned(),
                section.map_or(Value::Null, |s| json!(s)),
            );
            entry.insert("order".to_owned(), number(order));
            object_mut(state, "slugs").insert(slug.to_owned(), Value::Object(entry));
            prune_sections_order(state);
            Ok(((), true))
        })
    }

    /// Set the section for a local slug or controller-owned remote ledger key.
    /// Remote keys are stored only in `remoteLayouts`; operational slug state
    /// remains on the worker that owns the checkout.
    pub fn set_worktree_section(
        &mut self,
        key: &str,
        section: Option<&str>,
    ) -> Result<(), StoreError> {
        if !key.starts_with("@remote/") {
            return self.place_slug(key, section, false);
        }
        self.mutate_wt_state(|state| {
            if let Some(section) = section {
                ensure_section(state, section);
            }
            let order = max_layout_order(state, section).map_or(0.0, |max| max + 1.0);
            let mut layout = state
                .get("remoteLayouts")
                .and_then(|layouts| layouts.get(key))
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            layout.insert(
                "section".to_owned(),
                section.map_or(Value::Null, |s| json!(s)),
            );
            layout.insert("order".to_owned(), number(order));
            object_mut(state, "remoteLayouts").insert(key.to_owned(), Value::Object(layout));
            prune_sections_order(state);
            Ok(((), true))
        })
    }

    pub fn set_slug_github_issue(
        &mut self,
        slug: &str,
        issue: Option<u64>,
    ) -> Result<(), StoreError> {
        self.mutate_wt_state(|state| {
            if issue.is_none() && !has_slug(state, slug) {
                return Ok(((), false));
            }
            let mut entry = slug_entry(state, slug);
            if let Some(issue) = issue {
                entry.insert("githubIssue".to_owned(), json!(issue));
            } else {
                entry.remove("githubIssue");
            }
            object_mut(state, "slugs").insert(slug.to_owned(), Value::Object(entry));
            Ok(((), true))
        })
    }

    /// `None` clears the tracker override, while `Some("")` persists an
    /// explicit no-issue assertion instead of falling back to the slug.
    pub fn set_slug_issue_id(
        &mut self,
        slug: &str,
        issue_id: Option<&str>,
    ) -> Result<(), StoreError> {
        self.mutate_wt_state(|state| {
            if issue_id.is_none() && !has_slug(state, slug) {
                return Ok(((), false));
            }
            let mut entry = slug_entry(state, slug);
            if let Some(issue_id) = issue_id {
                entry.insert("issueId".to_owned(), json!(issue_id.trim().to_uppercase()));
            } else {
                entry.remove("issueId");
            }
            object_mut(state, "slugs").insert(slug.to_owned(), Value::Object(entry));
            Ok(((), true))
        })
    }

    pub fn set_slug_examined(
        &mut self,
        slug: &str,
        examined: Option<Value>,
    ) -> Result<(), StoreError> {
        self.mutate_wt_state(|state| {
            if examined.is_none() && !has_slug(state, slug) {
                return Ok(((), false));
            }
            let mut entry = slug_entry(state, slug);
            if let Some(examined) = examined {
                let mut examined = examined;
                preserve_unknown_fields(
                    entry.get("examined"),
                    &mut examined,
                    &["sha", "baseSha", "verdict", "by", "at"],
                );
                entry.insert("examined".to_owned(), examined);
            } else {
                entry.remove("examined");
            }
            object_mut(state, "slugs").insert(slug.to_owned(), Value::Object(entry));
            Ok(((), true))
        })
    }

    pub fn rename_section(&mut self, old_name: &str, new_name: &str) -> Result<(), StoreError> {
        let new_name = new_name.trim().to_owned();
        if new_name.is_empty() || new_name == old_name {
            return Ok(());
        }
        self.mutate_wt_state(|state| {
            let referenced = all_layouts(state)
                .any(|layout| layout.get("section").and_then(Value::as_str) == Some(old_name));
            let order_has_old = array_mut(state, "sectionsOrder")
                .iter()
                .any(|key| key.as_str() == Some(old_name));
            if !referenced && !order_has_old {
                return Ok(((), false));
            }
            let merge = new_name != old_name
                && (array_mut(state, "sectionsOrder")
                    .iter()
                    .any(|key| key.as_str() == Some(&new_name))
                    || all_layouts(state).any(|layout| {
                        layout.get("section").and_then(Value::as_str) == Some(&new_name)
                    }));
            if merge {
                let mut sources = layout_refs_for_section(state, old_name);
                sources.sort_by(|a, b| a.2.total_cmp(&b.2));
                let mut order = max_layout_order(state, Some(&new_name)).map_or(0.0, |n| n + 1.0);
                for (remote, key, _) in sources {
                    let collection = if remote { "remoteLayouts" } else { "slugs" };
                    if let Some(layout) = state
                        .get_mut(collection)
                        .and_then(Value::as_object_mut)
                        .and_then(|map| map.get_mut(&key))
                        .and_then(Value::as_object_mut)
                    {
                        layout.insert("section".to_owned(), json!(new_name));
                        layout.insert("order".to_owned(), number(order));
                        order += 1.0;
                    }
                }
                array_mut(state, "sectionsOrder").retain(|key| key.as_str() != Some(old_name));
                array_mut(state, "foldedSections").retain(|key| key.as_str() != Some(old_name));
            } else {
                for (remote, key, _) in layout_refs_for_section(state, old_name) {
                    let collection = if remote { "remoteLayouts" } else { "slugs" };
                    if let Some(layout) = state
                        .get_mut(collection)
                        .and_then(Value::as_object_mut)
                        .and_then(|map| map.get_mut(&key))
                        .and_then(Value::as_object_mut)
                    {
                        layout.insert("section".to_owned(), json!(new_name));
                    }
                }
                for key in array_mut(state, "sectionsOrder") {
                    if key.as_str() == Some(old_name) {
                        *key = json!(new_name);
                    }
                }
                for key in array_mut(state, "foldedSections") {
                    if key.as_str() == Some(old_name) {
                        *key = json!(new_name);
                    }
                }
            }
            prune_sections_order(state);
            Ok(((), true))
        })
    }

    pub fn remove_section(&mut self, name: &str) -> Result<usize, StoreError> {
        self.mutate_wt_state(|state| {
            let local = layout_refs_for_section(state, name)
                .into_iter()
                .filter(|(remote, _, _)| !remote)
                .collect::<Vec<_>>();
            let remote = layout_refs_for_section(state, name)
                .into_iter()
                .filter(|(remote, _, _)| *remote)
                .collect::<Vec<_>>();
            let order_has = array_mut(state, "sectionsOrder")
                .iter()
                .any(|section| section.as_str() == Some(name));
            if local.is_empty() && remote.is_empty() && !order_has {
                return Ok((0, false));
            }
            let mut order = max_layout_order(state, None).map_or(0.0, |max| max + 1.0);
            for (is_remote, key, _) in local.iter().chain(remote.iter()) {
                let collection = if *is_remote { "remoteLayouts" } else { "slugs" };
                if let Some(layout) = state
                    .get_mut(collection)
                    .and_then(Value::as_object_mut)
                    .and_then(|map| map.get_mut(key))
                    .and_then(Value::as_object_mut)
                {
                    layout.insert("section".to_owned(), Value::Null);
                    layout.insert("order".to_owned(), number(order));
                    order += 1.0;
                }
            }
            array_mut(state, "foldedSections").retain(|key| key.as_str() != Some(name));
            prune_sections_order(state);
            Ok((local.len() + remote.len(), true))
        })
    }

    pub fn swap_orders(
        &mut self,
        slug_a: &str,
        slug_b: &str,
        section: Option<&str>,
        bucket_display: &[String],
    ) -> Result<bool, StoreError> {
        self.mutate_wt_state(|state| {
            let baseline = all_layouts(state)
                .filter(|layout| layout.get("section").and_then(Value::as_str) == section)
                .filter_map(|layout| layout.get("order").and_then(Value::as_f64))
                .reduce(f64::min)
                .unwrap_or(0.0);
            let slugs = object_mut(state, "slugs");
            for (index, slug) in bucket_display.iter().enumerate() {
                let mut entry = slugs
                    .get(slug)
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default();
                entry.insert(
                    "section".to_owned(),
                    section.map_or(Value::Null, |s| json!(s)),
                );
                entry.insert("order".to_owned(), number(baseline + index as f64));
                slugs.insert(slug.clone(), Value::Object(entry));
            }
            let Some(a_order) = slugs
                .get(slug_a)
                .and_then(|entry| entry.get("order"))
                .cloned()
            else {
                return Ok((false, false));
            };
            let Some(b_order) = slugs
                .get(slug_b)
                .and_then(|entry| entry.get("order"))
                .cloned()
            else {
                return Ok((false, false));
            };
            if let Some(entry) = slugs.get_mut(slug_a).and_then(Value::as_object_mut) {
                entry.insert("order".to_owned(), b_order);
            }
            if let Some(entry) = slugs.get_mut(slug_b).and_then(Value::as_object_mut) {
                entry.insert("order".to_owned(), a_order);
            }
            Ok((true, true))
        })
    }

    pub fn move_group_past(
        &mut self,
        key: &str,
        past_key: &str,
        before: bool,
        visual_order: &[String],
    ) -> Result<bool, StoreError> {
        self.mutate_wt_state(|state| {
            if key == past_key {
                return Ok((false, false));
            }
            let current: Vec<String> = array_mut(state, "sectionsOrder")
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect();
            let mut order = seed_visual_stacks(&current, visual_order);
            for candidate in [key, past_key] {
                if !order.iter().any(|entry| entry == candidate)
                    && candidate.starts_with(STACK_PREFIX)
                {
                    if order.first().is_some_and(|entry| entry == GROUP_INBOX) {
                        order.insert(1, candidate.to_owned());
                    } else {
                        order.insert(0, candidate.to_owned());
                    }
                }
            }
            if !order.iter().any(|entry| entry == key) {
                return Ok((false, false));
            }
            order.retain(|entry| entry != key);
            let Some(index) = order.iter().position(|entry| entry == past_key) else {
                return Ok((false, false));
            };
            order.insert(if before { index } else { index + 1 }, key.to_owned());
            if order == current {
                return Ok((false, false));
            }
            state["sectionsOrder"] = json!(order);
            Ok((true, true))
        })
    }

    pub fn reap_wt_state(&mut self, live_slugs: &BTreeSet<String>) -> Result<bool, StoreError> {
        self.mutate_wt_state(|state| {
            let slugs = state
                .get("slugs")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            let edges = state
                .get("edges")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let mut next_edges: Vec<Value> = edges
                .iter()
                .filter(|edge| {
                    edge.get("from")
                        .and_then(Value::as_str)
                        .is_some_and(|slug| live_slugs.contains(slug))
                        && edge
                            .get("to")
                            .and_then(Value::as_str)
                            .is_some_and(|slug| live_slugs.contains(slug))
                })
                .cloned()
                .collect();
            let changed = slugs.keys().any(|slug| !live_slugs.contains(slug))
                || next_edges.len() != edges.len();
            if !changed {
                return Ok((false, false));
            }
            let kept = slugs
                .into_iter()
                .filter(|(slug, _)| live_slugs.contains(slug))
                .collect::<Map<_, _>>();
            state["slugs"] = Value::Object(kept);
            state["edges"] = Value::Array(std::mem::take(&mut next_edges));
            prune_sections_order(state);
            Ok((true, true))
        })
    }

    pub fn reap_remote_layouts(
        &mut self,
        host: &str,
        live_slugs: &BTreeSet<String>,
    ) -> Result<bool, StoreError> {
        let prefix = format!("@remote/{host}/");
        self.mutate_wt_state(|state| {
            let layouts = object_mut(state, "remoteLayouts");
            let stale: Vec<String> = layouts
                .keys()
                .filter(|key| key.starts_with(&prefix))
                .filter(|key| {
                    let suffix = key.strip_prefix(&prefix).unwrap_or_default();
                    let slug = percent_decode(suffix).unwrap_or_else(|| suffix.to_owned());
                    !live_slugs.contains(&slug)
                })
                .cloned()
                .collect();
            if stale.is_empty() {
                return Ok((false, false));
            }
            for key in stale {
                layouts.remove(&key);
            }
            prune_sections_order(state);
            Ok((true, true))
        })
    }

    pub fn set_slug_manual_title(
        &mut self,
        slug: &str,
        title: &str,
        expected_revision: Option<u64>,
    ) -> Result<bool, StoreError> {
        let title = title.trim().to_owned();
        if title.is_empty() {
            return Err(StoreError::EmptyManualTitle {
                slug: slug.to_owned(),
            });
        }
        self.mutate_wt_state(|state| {
            let previous = state
                .get("slugs")
                .and_then(|slugs| slugs.get(slug))
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            let revision = previous
                .get("manualTitleRevision")
                .and_then(Value::as_u64)
                .filter(|revision| *revision <= MAX_SAFE_INTEGER)
                .unwrap_or(0);
            if expected_revision.is_some_and(|expected| revision != expected) {
                return Ok((false, false));
            }
            if revision >= MAX_SAFE_INTEGER {
                return Err(StoreError::ManualTitleRevisionExhausted {
                    slug: slug.to_owned(),
                });
            }
            let mut next = previous;
            next.entry("section").or_insert(Value::Null);
            next.entry("order").or_insert(json!(0));
            next.insert("manualTitle".to_owned(), json!(title));
            next.insert("manualTitleRevision".to_owned(), json!(revision + 1));
            object_mut(state, "slugs").insert(slug.to_owned(), Value::Object(next));
            Ok((true, true))
        })
    }

    pub fn claim_dev_port(
        &mut self,
        slug: &str,
        candidates: &[u16],
    ) -> Result<Option<u16>, StoreError> {
        self.mutate_wt_state(|state| {
            let claimed: BTreeSet<u16> = state
                .get("slugs")
                .and_then(Value::as_object)
                .into_iter()
                .flat_map(|slugs| slugs.iter())
                .filter(|(key, _)| key.as_str() != slug)
                .filter_map(|(_, value)| value.get("devPort")?.as_u64()?.try_into().ok())
                .collect();
            let Some(port) = candidates
                .iter()
                .copied()
                .find(|port| !claimed.contains(port))
            else {
                return Ok((None, false));
            };
            let mut entry = slug_entry(state, slug);
            entry.insert("devPort".to_owned(), json!(port));
            object_mut(state, "slugs").insert(slug.to_owned(), Value::Object(entry));
            Ok((Some(port), true))
        })
    }

    pub fn set_slug_dev_port(&mut self, slug: &str, port: Option<u16>) -> Result<(), StoreError> {
        self.mutate_wt_state(|state| {
            if port.is_none() && !has_slug(state, slug) {
                return Ok(((), false));
            }
            let mut entry = slug_entry(state, slug);
            if let Some(port) = port {
                entry.insert("devPort".to_owned(), json!(port));
            } else {
                entry.remove("devPort");
            }
            object_mut(state, "slugs").insert(slug.to_owned(), Value::Object(entry));
            Ok(((), true))
        })
    }

    pub fn set_slug_dev_started_sha(
        &mut self,
        slug: &str,
        sha: Option<&str>,
    ) -> Result<(), StoreError> {
        self.mutate_wt_state(|state| {
            if sha.is_none() && !has_slug(state, slug) {
                return Ok(((), false));
            }
            let mut entry = slug_entry(state, slug);
            if let Some(sha) = sha {
                entry.insert("devStartedSha".to_owned(), json!(sha));
            } else {
                entry.remove("devStartedSha");
            }
            object_mut(state, "slugs").insert(slug.to_owned(), Value::Object(entry));
            Ok(((), true))
        })
    }

    pub fn set_slug_base(
        &mut self,
        slug: &str,
        branch: Option<(&str, Option<&str>)>,
    ) -> Result<(), StoreError> {
        self.mutate_wt_state(|state| {
            if branch.is_none() && !has_slug(state, slug) {
                return Ok(((), false));
            }
            let mut entry = slug_entry(state, slug);
            entry.remove("baseBranch");
            entry.remove("baseSha");
            if let Some((branch, sha)) = branch {
                entry.insert("baseBranch".to_owned(), json!(branch));
                if let Some(sha) = sha.filter(|sha| !sha.is_empty()) {
                    entry.insert("baseSha".to_owned(), json!(sha));
                }
            }
            object_mut(state, "slugs").insert(slug.to_owned(), Value::Object(entry));
            Ok(((), true))
        })
    }

    pub fn advance_base_anchor(
        &mut self,
        slug: &str,
        expected_parent: &str,
        sha: &str,
    ) -> Result<bool, StoreError> {
        self.mutate_wt_state(|state| {
            let Some(mut entry) = state
                .get("slugs")
                .and_then(Value::as_object)
                .and_then(|slugs| slugs.get(slug))
                .and_then(Value::as_object)
                .cloned()
            else {
                return Ok((false, false));
            };
            if entry.get("baseBranch").and_then(Value::as_str) != Some(expected_parent) {
                return Ok((false, false));
            }
            entry.insert("baseSha".to_owned(), json!(sha));
            object_mut(state, "slugs").insert(slug.to_owned(), Value::Object(entry));
            Ok((true, true))
        })
    }

    pub fn reparent_base_references(
        &mut self,
        branch: &str,
        trunk: &str,
        deleted_slug: Option<&str>,
    ) -> Result<Vec<String>, StoreError> {
        self.mutate_wt_state(|state| {
            let slugs = state
                .get("slugs")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            let affected: Vec<String> = slugs
                .iter()
                .filter(|(slug, value)| {
                    Some(slug.as_str()) != deleted_slug
                        && value.get("baseBranch").and_then(Value::as_str) == Some(branch)
                })
                .map(|(slug, _)| slug.clone())
                .collect();
            if affected.is_empty() {
                return Ok((affected, false));
            }
            let deleted_base = deleted_slug
                .and_then(|slug| slugs.get(slug))
                .and_then(|record| record.get("baseBranch"))
                .and_then(Value::as_str)
                .filter(|base| *base != branch)
                .unwrap_or(trunk)
                .to_owned();
            let records = object_mut(state, "slugs");
            for slug in &affected {
                if let Some(entry) = records.get_mut(slug).and_then(Value::as_object_mut) {
                    entry.insert("baseBranch".to_owned(), json!(deleted_base));
                    // baseSha intentionally survives reparenting.
                }
            }
            Ok((affected, true))
        })
    }

    pub fn set_slug_work_status(
        &mut self,
        slug: &str,
        record: Option<&WorkStatusRecord>,
    ) -> Result<bool, StoreError> {
        self.mutate_wt_state(|state| {
            if record.is_none() && !has_slug(state, slug) {
                return Ok((false, false));
            }
            if let (Some(record), Some(previous)) = (
                record,
                state
                    .get("slugs")
                    .and_then(|slugs| slugs.get(slug))
                    .and_then(|slug| slug.get("work"))
                    .and_then(|value| {
                        serde_json::from_value::<WorkStatusRecord>(value.clone()).ok()
                    }),
            ) && previous.same_claim(record)
            {
                return Ok((false, false));
            }
            let mut entry = slug_entry(state, slug);
            if let Some(record) = record {
                let mut next_work = serde_json::to_value(record)?;
                preserve_unknown_fields(
                    entry.get("work"),
                    &mut next_work,
                    &[
                        "state",
                        "at",
                        "note",
                        "risk",
                        "sha",
                        "by",
                        "blockedOn",
                        "verifyAfterMerge",
                    ],
                );
                entry.insert("work".to_owned(), next_work);
            } else {
                entry.remove("work");
            }
            object_mut(state, "slugs").insert(slug.to_owned(), Value::Object(entry));
            Ok((true, true))
        })
    }

    pub fn set_attention_seen(&mut self, timestamp_ms: u64) -> Result<(), StoreError> {
        self.mutate_wt_state(|state| {
            let previous = state
                .get("attentionSeenTs")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            if timestamp_ms <= previous {
                return Ok(((), false));
            }
            state["attentionSeenTs"] = json!(timestamp_ms);
            Ok(((), true))
        })
    }

    pub fn set_branch_tip(&mut self, branch: &str, sha: &str) -> Result<bool, StoreError> {
        self.mutate_wt_state(|state| {
            let tips = object_mut(state, "branchTips");
            if tips.get(branch).and_then(Value::as_str) == Some(sha) {
                return Ok((false, false));
            }
            tips.insert(branch.to_owned(), json!(sha));
            Ok((true, true))
        })
    }

    pub fn set_merge_edge(&mut self, edge: &MergeEdge) -> Result<(), StoreError> {
        let mut edge = serde_json::to_value(edge)?;
        self.mutate_wt_state(|state| {
            let edges = array_mut(state, "edges");
            if let Some(previous) = edges.iter().find(|existing| {
                existing.get("from") == edge.get("from") && existing.get("to") == edge.get("to")
            }) {
                preserve_unknown_fields(
                    Some(previous),
                    &mut edge,
                    &[
                        "from", "to", "kind", "strength", "at", "by", "why", "fromSha", "toSha",
                    ],
                );
            }
            edges.retain(|existing| {
                existing.get("from") != edge.get("from") || existing.get("to") != edge.get("to")
            });
            edges.push(edge);
            Ok(((), true))
        })
    }

    pub fn remove_merge_edge(&mut self, from: &str, to: &str) -> Result<bool, StoreError> {
        self.mutate_wt_state(|state| {
            let edges = array_mut(state, "edges");
            let previous = edges.len();
            edges.retain(|edge| {
                edge.get("from").and_then(Value::as_str) != Some(from)
                    || edge.get("to").and_then(Value::as_str) != Some(to)
            });
            let changed = previous != edges.len();
            Ok((changed, changed))
        })
    }

    pub fn prune_merge_edges(&mut self, live_slugs: &BTreeSet<String>) -> Result<bool, StoreError> {
        self.mutate_wt_state(|state| {
            let edges = array_mut(state, "edges");
            let previous = edges.len();
            edges.retain(|edge| {
                edge.get("from")
                    .and_then(Value::as_str)
                    .is_some_and(|slug| live_slugs.contains(slug))
                    && edge
                        .get("to")
                        .and_then(Value::as_str)
                        .is_some_and(|slug| live_slugs.contains(slug))
            });
            let changed = previous != edges.len();
            Ok((changed, changed))
        })
    }

    pub fn add_review_request_dismissal(
        &mut self,
        dismissal: &ReviewRequestDismissal,
    ) -> Result<(), StoreError> {
        let mut dismissal = serde_json::to_value(dismissal)?;
        self.mutate_wt_state(|state| {
            let dismissals = array_mut(state, "reviewRequestDismissals");
            if let Some(previous) = dismissals
                .iter()
                .find(|existing| existing.get("url") == dismissal.get("url"))
            {
                preserve_unknown_fields(
                    Some(previous),
                    &mut dismissal,
                    &["url", "updatedAt", "dismissedAt"],
                );
            }
            dismissals.retain(|existing| existing.get("url") != dismissal.get("url"));
            dismissals.push(dismissal);
            if dismissals.len() > MAX_REVIEW_REQUEST_DISMISSALS {
                let remove = dismissals.len() - MAX_REVIEW_REQUEST_DISMISSALS;
                dismissals.drain(0..remove);
            }
            Ok(((), true))
        })
    }

    pub fn toggle_global_automations_paused(&mut self) -> Result<bool, StoreError> {
        self.mutate_wt_state(|state| {
            let paused = !state["automationsPaused"].as_bool().unwrap_or(false);
            state["automationsPaused"] = json!(paused);
            Ok((paused, true))
        })
    }

    pub fn toggle_slug_automations_paused(&mut self, slug: &str) -> Result<bool, StoreError> {
        self.mutate_wt_state(|state| {
            let mut entry = slug_entry(state, slug);
            let paused = entry.get("automationsPaused").and_then(Value::as_bool) != Some(true);
            if paused {
                entry.insert("automationsPaused".to_owned(), Value::Bool(true));
            } else {
                entry.remove("automationsPaused");
            }
            object_mut(state, "slugs").insert(slug.to_owned(), Value::Object(entry));
            Ok((paused, true))
        })
    }

    pub fn toggle_stack_automations_paused(
        &mut self,
        stack_id: &str,
        member_slugs: &[String],
    ) -> Result<bool, StoreError> {
        self.mutate_wt_state(|state| {
            let paused_stacks = array_mut(state, "pausedStacks");
            let was_paused = paused_stacks.iter().any(|id| id.as_str() == Some(stack_id));
            paused_stacks.retain(|id| id.as_str() != Some(stack_id));
            if !was_paused {
                paused_stacks.push(json!(stack_id));
            }
            let paused = !was_paused;
            for slug in member_slugs {
                let mut entry = slug_entry(state, slug);
                if paused {
                    entry.insert("automationsPaused".to_owned(), Value::Bool(true));
                } else {
                    entry.remove("automationsPaused");
                }
                object_mut(state, "slugs").insert(slug.clone(), Value::Object(entry));
            }
            Ok((paused, true))
        })
    }

    pub fn toggle_removed_automations_paused(
        &mut self,
        slug: &str,
    ) -> Result<Option<bool>, StoreError> {
        self.mutate_wt_state(|state| {
            let Some(entry) = array_mut(state, "removed")
                .iter_mut()
                .find(|entry| entry.get("slug").and_then(Value::as_str) == Some(slug))
            else {
                return Ok((None, false));
            };
            let paused = entry.get("automationsPaused").and_then(Value::as_bool) != Some(true);
            if paused {
                entry["automationsPaused"] = Value::Bool(true);
            } else if let Some(object) = entry.as_object_mut() {
                object.remove("automationsPaused");
            }
            Ok((Some(paused), true))
        })
    }

    pub fn toggle_section_folded(&mut self, section_key: &str) -> Result<bool, StoreError> {
        self.mutate_wt_state(|state| {
            let folded = array_mut(state, "foldedSections");
            let is_folded = folded.iter().any(|key| key.as_str() == Some(section_key));
            if is_folded {
                folded.retain(|key| key.as_str() != Some(section_key));
            } else {
                folded.push(json!(section_key));
            }
            Ok((!is_folded, true))
        })
    }

    pub fn set_section_folded(
        &mut self,
        section_key: &str,
        folded: bool,
    ) -> Result<bool, StoreError> {
        self.mutate_wt_state(|state| {
            let keys = array_mut(state, "foldedSections");
            let current = keys.iter().any(|key| key.as_str() == Some(section_key));
            if current == folded {
                return Ok((folded, false));
            }
            if folded {
                keys.push(json!(section_key));
            } else {
                keys.retain(|key| key.as_str() != Some(section_key));
            }
            Ok((folded, true))
        })
    }

    pub fn record_slug_created(&mut self, slug: &str, at: &str) -> Result<bool, StoreError> {
        self.mutate_wt_state(|state| {
            let mut entry = slug_entry(state, slug);
            if entry.get("createdAt").and_then(Value::as_str).is_some() {
                return Ok((false, false));
            }
            entry.insert("createdAt".to_owned(), json!(at));
            object_mut(state, "slugs").insert(slug.to_owned(), Value::Object(entry));
            Ok((true, true))
        })
    }

    pub fn record_removed_worktrees(
        &mut self,
        incoming: &[RemovedWorktree],
        now_ms: i64,
    ) -> Result<(), StoreError> {
        if incoming.is_empty() {
            return Ok(());
        }
        self.mutate_wt_state(|state| {
            let current_slugs = state
                .get("slugs")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            let mut by_slug: std::collections::BTreeMap<String, Value> =
                array_mut(state, "removed")
                    .iter()
                    .filter_map(|entry| {
                        Some((entry.get("slug")?.as_str()?.to_owned(), entry.clone()))
                    })
                    .collect();
            for incoming in incoming {
                let mut next = by_slug
                    .get(&incoming.slug)
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_default();
                let previous = by_slug.get(&incoming.slug);
                next.insert("slug".to_owned(), json!(incoming.slug));
                next.insert("branch".to_owned(), json!(incoming.branch));
                next.insert("removedAt".to_owned(), json!(incoming.removed_at));
                merge_optional(
                    &mut next,
                    "work",
                    &incoming.work,
                    || {
                        current_slugs
                            .get(&incoming.slug)
                            .and_then(|slug| slug.get("work"))
                            .cloned()
                    },
                    previous,
                );
                let paused = incoming
                    .automations_paused
                    .or_else(|| {
                        current_slugs
                            .get(&incoming.slug)?
                            .get("automationsPaused")?
                            .as_bool()
                    })
                    .or_else(|| {
                        previous
                            .and_then(|prev| prev.get("automationsPaused"))?
                            .as_bool()
                    });
                if paused == Some(true) {
                    next.insert("automationsPaused".to_owned(), Value::Bool(true));
                }
                for (key, value) in &incoming.extra {
                    next.insert(key.clone(), value.clone());
                }
                by_slug.insert(incoming.slug.clone(), Value::Object(next));
            }
            let cutoff = now_ms.saturating_sub(REMOVED_MAX_AGE_MS);
            let mut removed: Vec<Value> = by_slug
                .into_values()
                .filter(|entry| {
                    entry
                        .get("removedAt")
                        .and_then(Value::as_str)
                        .and_then(parse_timestamp_ms)
                        .is_some_and(|removed_at| removed_at >= cutoff)
                })
                .collect();
            removed.sort_by(|a, b| {
                b.get("removedAt")
                    .and_then(Value::as_str)
                    .cmp(&a.get("removedAt").and_then(Value::as_str))
            });
            removed.truncate(REMOVED_MAX_ENTRIES);
            state["removed"] = Value::Array(removed);
            Ok(((), true))
        })
    }

    pub fn clear_removed_worktree(&mut self, slug: &str) -> Result<bool, StoreError> {
        self.mutate_wt_state(|state| {
            let removed = array_mut(state, "removed");
            let previous = removed.len();
            removed.retain(|entry| entry.get("slug").and_then(Value::as_str) != Some(slug));
            let changed = removed.len() != previous;
            Ok((changed, changed))
        })
    }
}

pub fn is_merged_removal(entry: &RemovedWorktree) -> bool {
    entry.extra.get("prState").and_then(Value::as_str) == Some("MERGED")
        || entry.extra.get("gitState").and_then(Value::as_str) == Some("merged")
}

/// Whether a removed row still had an outstanding post-merge check. This is
/// independent of whether the branch landed; absence of a status remains
/// unknown rather than being interpreted as an obligation.
pub fn verification_owed_at_removal(entry: &RemovedWorktree) -> bool {
    entry.work.as_ref().is_some_and(|work| {
        work.verify_after_merge
            .as_ref()
            .is_some_and(|steps| !steps.is_empty())
            && work.state != "verified"
            && work.state != "dropped"
    })
}

fn object_mut<'a>(state: &'a mut Value, key: &str) -> &'a mut Map<String, Value> {
    if !state.get(key).is_some_and(Value::is_object) {
        state[key] = Value::Object(Map::new());
    }
    state[key]
        .as_object_mut()
        .expect("object shape established")
}

fn array_mut<'a>(state: &'a mut Value, key: &str) -> &'a mut Vec<Value> {
    if !state.get(key).is_some_and(Value::is_array) {
        state[key] = Value::Array(Vec::new());
    }
    state[key].as_array_mut().expect("array shape established")
}

fn has_slug(state: &Value, slug: &str) -> bool {
    state
        .get("slugs")
        .and_then(|slugs| slugs.get(slug))
        .is_some()
}

fn slug_entry(state: &Value, slug: &str) -> Map<String, Value> {
    let mut entry = state
        .get("slugs")
        .and_then(|slugs| slugs.get(slug))
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    entry.entry("section").or_insert(Value::Null);
    entry.entry("order").or_insert(json!(0));
    entry
}

fn all_layouts(state: &Value) -> impl Iterator<Item = &Value> {
    state
        .get("slugs")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|slugs| slugs.values())
        .chain(
            state
                .get("remoteLayouts")
                .and_then(Value::as_object)
                .into_iter()
                .flat_map(|layouts| layouts.values()),
        )
}

fn ensure_section(state: &mut Value, section: &str) {
    let order = array_mut(state, "sectionsOrder");
    if !order.iter().any(|name| name.as_str() == Some(section)) {
        order.push(json!(section));
    }
}

fn max_layout_order(state: &Value, section: Option<&str>) -> Option<f64> {
    all_layouts(state)
        .filter(|layout| layout.get("section").and_then(Value::as_str) == section)
        .filter_map(|layout| layout.get("order").and_then(Value::as_f64))
        .reduce(f64::max)
}

/// `(is_remote, key, order)` entries for the rows in a manual section.
fn layout_refs_for_section(state: &Value, section: &str) -> Vec<(bool, String, f64)> {
    let local = state
        .get("slugs")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|map| map.iter())
        .filter(|(_, layout)| layout.get("section").and_then(Value::as_str) == Some(section))
        .map(|(key, layout)| {
            (
                false,
                key.clone(),
                layout.get("order").and_then(Value::as_f64).unwrap_or(0.0),
            )
        });
    let remote = state
        .get("remoteLayouts")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|map| map.iter())
        .filter(|(_, layout)| layout.get("section").and_then(Value::as_str) == Some(section))
        .map(|(key, layout)| {
            (
                true,
                key.clone(),
                layout.get("order").and_then(Value::as_f64).unwrap_or(0.0),
            )
        });
    local.chain(remote).collect()
}

fn seed_visual_stacks(order: &[String], visual_order: &[String]) -> Vec<String> {
    let mut out = order.to_vec();
    for (index, group) in visual_order.iter().enumerate() {
        if out.contains(group) || !group.starts_with(STACK_PREFIX) {
            continue;
        }
        let mut anchor = usize::from(out.first().is_some_and(|entry| entry == GROUP_INBOX));
        for previous in visual_order[..index].iter().rev() {
            if let Some(previous_index) = out.iter().position(|entry| entry == previous) {
                anchor = previous_index + 1;
                break;
            }
        }
        out.insert(anchor, group.clone());
    }
    out
}

fn percent_decode(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            let high = hex_value(*bytes.get(index + 1)?)?;
            let low = hex_value(*bytes.get(index + 2)?)?;
            decoded.push(high * 16 + low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).ok()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn prune_sections_order(state: &mut Value) {
    let live: BTreeSet<String> = all_layouts(state)
        .filter_map(|entry| entry.get("section")?.as_str().map(str::to_owned))
        .collect();
    array_mut(state, "sectionsOrder").retain(|entry| {
        let Some(key) = entry.as_str() else {
            return false;
        };
        key == GROUP_INBOX || key.starts_with(STACK_PREFIX) || live.contains(key)
    });
}

fn number(value: f64) -> Value {
    Number::from_f64(value).map_or(Value::Null, Value::Number)
}

fn preserve_unknown_fields(existing: Option<&Value>, next: &mut Value, known_fields: &[&str]) {
    let (Some(existing), Some(next)) = (existing.and_then(Value::as_object), next.as_object_mut())
    else {
        return;
    };
    for (key, value) in existing {
        if !known_fields.contains(&key.as_str()) {
            next.entry(key.clone()).or_insert_with(|| value.clone());
        }
    }
}

fn parse_timestamp_ms(value: &str) -> Option<i64> {
    use time::{OffsetDateTime, format_description::well_known::Rfc3339};
    let timestamp = OffsetDateTime::parse(value, &Rfc3339).ok()?;
    i64::try_from(timestamp.unix_timestamp_nanos() / 1_000_000).ok()
}

fn merge_optional(
    target: &mut Map<String, Value>,
    key: &str,
    explicit: &Option<WorkStatusRecord>,
    fallback: impl FnOnce() -> Option<Value>,
    previous: Option<&Value>,
) {
    if let Some(value) = explicit {
        if let Ok(mut value) = serde_json::to_value(value) {
            preserve_unknown_fields(
                target
                    .get(key)
                    .or_else(|| previous.and_then(|entry| entry.get(key))),
                &mut value,
                &[
                    "state",
                    "at",
                    "note",
                    "risk",
                    "sha",
                    "by",
                    "blockedOn",
                    "verifyAfterMerge",
                ],
            );
            target.insert(key.to_owned(), value);
        }
    } else if let Some(value) = fallback() {
        target.insert(key.to_owned(), value);
    } else if let Some(value) = previous.and_then(|previous| previous.get(key)) {
        target.insert(key.to_owned(), value.clone());
    }
}
