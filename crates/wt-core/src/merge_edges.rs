use crate::time::parse_iso_millis;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MergeEdgeKind {
    Before,
    Conflicts,
    Enables,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MergeEdgeStrength {
    Blocks,
    #[default]
    Prefer,
}

/// A pairwise, self-expiring ordering assertion.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MergeEdge {
    pub from: String,
    pub to: String,
    pub kind: MergeEdgeKind,
    #[serde(default)]
    pub strength: MergeEdgeStrength,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
    #[serde(default)]
    pub at: String,
    #[serde(default = "fleet")]
    pub by: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from_sha: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to_sha: Option<String>,
    /// Unknown durable fields survive a read/modify/write cycle.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

fn fleet() -> String {
    "fleet".into()
}

pub fn edge_orders(kind: MergeEdgeKind) -> bool {
    kind != MergeEdgeKind::Conflicts
}

/// Lenient persisted-record parser; malformed edges are discarded.
pub fn parse_merge_edge(raw: &Value) -> Option<MergeEdge> {
    let object = raw.as_object()?;
    let from = object.get("from")?.as_str()?;
    let to = object.get("to")?.as_str()?;
    if from.trim().is_empty() || to.trim().is_empty() || from == to {
        return None;
    }
    let kind = match object.get("kind")?.as_str()? {
        "before" => MergeEdgeKind::Before,
        "conflicts" => MergeEdgeKind::Conflicts,
        "enables" => MergeEdgeKind::Enables,
        _ => return None,
    };
    let strength = match object.get("strength").and_then(Value::as_str) {
        Some("blocks") => MergeEdgeStrength::Blocks,
        _ => MergeEdgeStrength::Prefer,
    };
    let mut edge = MergeEdge {
        from: from.into(),
        to: to.into(),
        kind,
        strength,
        why: object
            .get("why")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .map(str::to_owned),
        at: object
            .get("at")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        by: object
            .get("by")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .unwrap_or("fleet")
            .to_owned(),
        from_sha: object
            .get("fromSha")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .map(str::to_owned),
        to_sha: object
            .get("toSha")
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .map(str::to_owned),
        extra: BTreeMap::new(),
    };
    const KNOWN: &[&str] = &[
        "from", "to", "kind", "strength", "why", "at", "by", "fromSha", "toSha",
    ];
    edge.extra.extend(
        object
            .iter()
            .filter(|(key, _)| !KNOWN.contains(&key.as_str()))
            .map(|(k, v)| (k.clone(), v.clone())),
    );
    Some(edge)
}

pub fn edge_is_stale_by_sha<F>(edge: &MergeEdge, mut head_of: F) -> bool
where
    F: FnMut(&str) -> Option<String>,
{
    let (Some(from_sha), Some(to_sha)) = (&edge.from_sha, &edge.to_sha) else {
        return true;
    };
    if head_of(&edge.from).is_some_and(|head| head != *from_sha) {
        return true;
    }
    if head_of(&edge.to).is_some_and(|head| head != *to_sha) {
        return true;
    }
    false
}

pub fn edge_is_stale_by_time<F>(edge: &MergeEdge, mut last_commit_ms_of: F) -> bool
where
    F: FnMut(&str) -> Option<i64>,
{
    if edge.from_sha.as_deref().is_none_or(str::is_empty)
        || edge.to_sha.as_deref().is_none_or(str::is_empty)
    {
        return true;
    }
    let Some(asserted) = parse_iso_millis(&edge.at) else {
        return true;
    };
    [edge.from.as_str(), edge.to.as_str()]
        .into_iter()
        .any(|slug| last_commit_ms_of(slug).is_some_and(|last| last > asserted))
}

/// Stable Kahn pass: among currently available nodes, choose the earliest
/// incoming row. If a cycle remains, release its earliest row and continue.
pub fn topo_order_slugs(order: &[String], edges: &[MergeEdge]) -> Vec<String> {
    let present: BTreeSet<&str> = order.iter().map(String::as_str).collect();
    let mut indegree: BTreeMap<&str, usize> = order.iter().map(|s| (s.as_str(), 0)).collect();
    let mut outs: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for edge in edges {
        if !edge_orders(edge.kind)
            || edge.from == edge.to
            || !present.contains(edge.from.as_str())
            || !present.contains(edge.to.as_str())
        {
            continue;
        }
        outs.entry(&edge.from).or_default().push(&edge.to);
        *indegree.entry(&edge.to).or_default() += 1;
    }
    if outs.is_empty() {
        return order.to_vec();
    }
    let mut remaining: BTreeSet<&str> = present.clone();
    let mut result = Vec::with_capacity(order.len());
    while !remaining.is_empty() {
        let pick = order
            .iter()
            .map(String::as_str)
            .find(|slug| remaining.contains(slug) && indegree.get(slug).copied().unwrap_or(0) == 0)
            .or_else(|| {
                order
                    .iter()
                    .map(String::as_str)
                    .find(|slug| remaining.contains(slug))
            })
            .expect("remaining node is in incoming order");
        remaining.remove(pick);
        result.push(pick.to_owned());
        if let Some(children) = outs.get(pick) {
            for child in children {
                if remaining.contains(child) {
                    *indegree.entry(child).or_default() -= 1;
                }
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn edge(from: &str, to: &str, kind: MergeEdgeKind) -> MergeEdge {
        MergeEdge {
            from: from.into(),
            to: to.into(),
            kind,
            strength: MergeEdgeStrength::Prefer,
            why: None,
            at: "2026-08-10T12:00:00.000Z".into(),
            by: "fleet".into(),
            from_sha: Some("aaa".into()),
            to_sha: Some("bbb".into()),
            extra: BTreeMap::new(),
        }
    }

    #[test]
    fn stable_ordering_obeys_edges_and_breaks_cycles_without_stalling() {
        let order = ["x", "b", "a", "y"].map(str::to_owned);
        assert_eq!(
            topo_order_slugs(&order, &[edge("a", "b", MergeEdgeKind::Before)]),
            ["x", "a", "b", "y"]
        );
        let order = ["a", "b"].map(str::to_owned);
        assert_eq!(
            topo_order_slugs(
                &order,
                &[
                    edge("a", "b", MergeEdgeKind::Before),
                    edge("b", "a", MergeEdgeKind::Before)
                ]
            ),
            order
        );
        assert_eq!(
            topo_order_slugs(&order, &[edge("b", "a", MergeEdgeKind::Conflicts)]),
            order
        );
    }

    #[test]
    fn parsing_defaults_and_preserves_unknown_durable_fields() {
        let edge =
            parse_merge_edge(&json!({"from":"a", "to":"b", "kind":"before", "future": {"x":1}}))
                .unwrap();
        assert_eq!(edge.strength, MergeEdgeStrength::Prefer);
        assert_eq!(edge.by, "fleet");
        assert_eq!(edge.extra["future"]["x"], 1);
        let roundtrip = serde_json::to_value(edge).unwrap();
        assert_eq!(roundtrip["future"]["x"], 1);
        assert!(parse_merge_edge(&json!({"from":"a", "to":"a", "kind":"before"})).is_none());
    }

    #[test]
    fn sha_and_time_staleness_fail_closed_only_for_known_changes() {
        let e = edge("a", "b", MergeEdgeKind::Enables);
        assert!(!edge_is_stale_by_sha(&e, |s| Some(
            if s == "a" { "aaa" } else { "bbb" }.into()
        )));
        assert!(edge_is_stale_by_sha(&e, |_| Some("moved".into())));
        assert!(!edge_is_stale_by_sha(&e, |_| None));
        let at = parse_iso_millis(&e.at).unwrap();
        assert!(!edge_is_stale_by_time(&e, |_| Some(at - 1)));
        assert!(edge_is_stale_by_time(&e, |s| if s == "a" {
            Some(at + 1)
        } else {
            None
        }));
        assert!(edge_is_stale_by_time(
            &MergeEdge {
                from_sha: None,
                ..e
            },
            |_| None
        ));
    }
}
