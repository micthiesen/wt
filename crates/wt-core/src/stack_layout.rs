use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChainMember {
    pub slug: String,
    pub branch: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_branch: Option<String>,
}

impl ChainMember {
    pub fn new(
        slug: impl Into<String>,
        branch: impl Into<String>,
        base_branch: Option<String>,
    ) -> Self {
        Self {
            slug: slug.into(),
            branch: branch.into(),
            base_branch,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StackNode {
    pub stack_id: String,
    pub slug: String,
    pub branch: String,
    pub depth: usize,
    pub lane: usize,
    pub parent_branch: Option<String>,
    pub index: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StackLayout {
    pub stack_id: String,
    pub nodes: Vec<StackNode>,
    pub by_branch: BTreeMap<String, StackNode>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StackIndexEntry {
    pub layout_index: usize,
    pub node: StackNode,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StackIndex {
    pub by_branch: BTreeMap<String, StackIndexEntry>,
    pub layouts: Vec<StackLayout>,
}

/// Infer stacks by following each live member's recorded fork base. `trunk`
/// is explicit so this core transform does not depend on global configuration.
pub fn build_stack_index(members: &[ChainMember], trunk: &str) -> StackIndex {
    let mut by_branch_member = BTreeMap::<String, ChainMember>::new();
    for member in members {
        if !member.branch.is_empty() {
            by_branch_member.insert(member.branch.clone(), member.clone());
        }
    }
    let parent_of = |member: &ChainMember| -> Option<&ChainMember> {
        let base = member.base_branch.as_deref()?;
        if base.is_empty() || base == trunk || base == member.branch {
            return None;
        }
        by_branch_member.get(base)
    };
    let mut children = BTreeMap::<String, Vec<ChainMember>>::new();
    let mut roots = Vec::new();
    for member in by_branch_member.values() {
        if let Some(parent) = parent_of(member) {
            children
                .entry(parent.branch.clone())
                .or_default()
                .push(member.clone());
        } else {
            roots.push(member.clone());
        }
    }
    // Use deterministic Unicode scalar-value ordering. The former JS
    // localeCompare ordering varies with host locale/ICU and is intentionally
    // not reproduced in this cross-host domain transform.
    roots.sort_by(|a, b| a.branch.cmp(&b.branch));
    for list in children.values_mut() {
        list.sort_by(|a, b| a.branch.cmp(&b.branch));
    }

    let mut result = StackIndex::default();
    for root in roots {
        if children.get(&root.branch).is_none_or(Vec::is_empty) {
            continue;
        }
        let stack_id = root.branch.clone();
        let mut builder = StackBuilder {
            stack_id: &stack_id,
            trunk,
            children: &children,
            seen: BTreeSet::new(),
            nodes: Vec::new(),
            next_lane: 0,
        };
        builder.walk(&root, 0, 0);
        let nodes = builder.nodes;
        let by_branch = nodes
            .iter()
            .map(|node| (node.branch.clone(), node.clone()))
            .collect();
        let layout_index = result.layouts.len();
        let layout = StackLayout {
            stack_id,
            nodes,
            by_branch,
        };
        for node in &layout.nodes {
            result.by_branch.insert(
                node.branch.clone(),
                StackIndexEntry {
                    layout_index,
                    node: node.clone(),
                },
            );
        }
        result.layouts.push(layout);
    }
    result
}

struct StackBuilder<'a> {
    stack_id: &'a str,
    trunk: &'a str,
    children: &'a BTreeMap<String, Vec<ChainMember>>,
    seen: BTreeSet<String>,
    nodes: Vec<StackNode>,
    next_lane: usize,
}

impl StackBuilder<'_> {
    fn walk(&mut self, member: &ChainMember, depth: usize, lane: usize) {
        if !self.seen.insert(member.branch.clone()) {
            return;
        }
        let parent_branch = if depth == 0 {
            member
                .base_branch
                .as_deref()
                .filter(|base| !base.is_empty() && *base != self.trunk && *base != member.branch)
                .map(str::to_owned)
        } else {
            member.base_branch.clone()
        };
        self.nodes.push(StackNode {
            stack_id: self.stack_id.to_owned(),
            slug: member.slug.clone(),
            branch: member.branch.clone(),
            depth,
            lane,
            parent_branch,
            index: self.nodes.len(),
        });
        if let Some(kids) = self.children.get(&member.branch).cloned() {
            for (child_index, child) in kids.iter().enumerate() {
                let child_lane = if child_index == 0 {
                    lane
                } else {
                    self.next_lane += 1;
                    self.next_lane
                };
                self.walk(child, depth + 1, child_lane);
            }
        }
    }
}

pub const STACK_CONNECTOR_ROOT: char = '┌';
pub const STACK_CONNECTOR_MORE: char = '├';
pub const STACK_CONNECTOR_LAST: char = '└';
pub const STACK_CONNECTOR_TRAIL: char = '│';

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpineMember {
    pub key: String,
    pub branch: String,
    pub parent_branch: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpineCell {
    pub col: usize,
    pub glyph: char,
    pub trail: Vec<bool>,
}

/// Draw connector positions for one contiguous, currently visible row group.
pub fn spine_layout(group: &[SpineMember]) -> BTreeMap<String, SpineCell> {
    let mut index_by_branch = BTreeMap::new();
    for (i, member) in group.iter().enumerate() {
        if !member.branch.is_empty() {
            index_by_branch.entry(member.branch.as_str()).or_insert(i);
        }
    }
    let parent_of: Vec<Option<usize>> = group
        .iter()
        .enumerate()
        .map(|(i, member)| {
            let parent = member
                .parent_branch
                .as_deref()
                .and_then(|p| index_by_branch.get(p).copied());
            parent.filter(|parent| *parent != i)
        })
        .collect();
    let mut depth_of = vec![0usize; group.len()];
    let mut last_child_of = BTreeMap::<usize, usize>::new();
    for (i, parent) in parent_of.iter().enumerate() {
        let mut depth = 0;
        let mut p = *parent;
        let mut hops = 0;
        while let Some(parent) = p {
            if hops >= group.len() {
                break;
            }
            depth += 1;
            p = parent_of[parent];
            hops += 1;
        }
        depth_of[i] = depth;
        if let Some(parent) = parent {
            last_child_of
                .entry(*parent)
                .and_modify(|last| *last = (*last).max(i))
                .or_insert(i);
        }
    }
    let more_below = |i: usize| {
        parent_of[i].is_some_and(|parent| last_child_of.get(&parent).is_some_and(|last| *last > i))
    };
    let mut output = BTreeMap::new();
    for (i, member) in group.iter().enumerate() {
        let depth = depth_of[i];
        if depth == 0 && !last_child_of.contains_key(&i) {
            continue;
        }
        if depth == 0 {
            output.insert(
                member.key.clone(),
                SpineCell {
                    col: 0,
                    glyph: STACK_CONNECTOR_ROOT,
                    trail: Vec::new(),
                },
            );
            continue;
        }
        let mut trail = Vec::new();
        let mut ancestor = parent_of[i];
        let mut hops = 0;
        while let Some(index) = ancestor.filter(|_| hops < group.len()) {
            if depth_of[index] == 0 {
                break;
            }
            trail.insert(0, more_below(index));
            ancestor = parent_of[index];
            hops += 1;
        }
        output.insert(
            member.key.clone(),
            SpineCell {
                col: depth - 1,
                glyph: if more_below(i) {
                    STACK_CONNECTOR_MORE
                } else {
                    STACK_CONNECTOR_LAST
                },
                trail,
            },
        );
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    fn m(slug: &str, branch: &str, base: Option<&str>) -> ChainMember {
        ChainMember::new(slug, branch, base.map(str::to_owned))
    }
    fn sp(rows: &[(&str, &str, Option<&str>)]) -> Vec<String> {
        let group: Vec<_> = rows
            .iter()
            .map(|(key, branch, parent)| SpineMember {
                key: (*key).into(),
                branch: (*branch).into(),
                parent_branch: parent.map(str::to_owned),
            })
            .collect();
        let cells = spine_layout(&group);
        group
            .iter()
            .map(|member| {
                cells
                    .get(&member.key)
                    .map(|cell| {
                        let mut out = String::new();
                        for i in 0..cell.col {
                            out.push(if cell.trail[i] {
                                STACK_CONNECTOR_TRAIL
                            } else {
                                ' '
                            });
                        }
                        out.push(cell.glyph);
                        out
                    })
                    .unwrap_or_default()
            })
            .collect()
    }

    #[test]
    fn chains_forks_external_roots_and_trunk_anchors() {
        let index = build_stack_index(
            &[
                m("c", "C", Some("B")),
                m("a", "A", None),
                m("b", "B", Some("A")),
            ],
            "main",
        );
        assert_eq!(
            index.layouts[0]
                .nodes
                .iter()
                .map(|n| n.branch.as_str())
                .collect::<Vec<_>>(),
            ["A", "B", "C"]
        );
        assert_eq!(
            index.layouts[0]
                .nodes
                .iter()
                .map(|n| n.depth)
                .collect::<Vec<_>>(),
            [0, 1, 2]
        );
        assert_eq!(index.by_branch["C"].layout_index, 0);
        let fork = build_stack_index(
            &[
                m("a", "A", None),
                m("b1", "B1", Some("A")),
                m("b2", "B2", Some("A")),
            ],
            "main",
        );
        assert_eq!(
            fork.layouts[0]
                .nodes
                .iter()
                .map(|n| n.lane)
                .collect::<Vec<_>>(),
            [0, 0, 1]
        );
        let external = build_stack_index(
            &[
                m("a", "A", Some("external")),
                m("b", "B", Some("A")),
                m("trunk", "T", Some("main")),
            ],
            "main",
        );
        assert_eq!(
            external.layouts[0].nodes[0].parent_branch.as_deref(),
            Some("external")
        );
    }

    #[test]
    fn cycles_dangling_records_and_self_base_do_not_form_false_stacks() {
        assert!(
            build_stack_index(
                &[
                    m("a", "A", Some("B")),
                    m("b", "B", Some("A")),
                    m("c", "C", Some("A"))
                ],
                "main"
            )
            .layouts
            .is_empty()
        );
        assert!(
            build_stack_index(&[m("a", "A", None), m("b", "B", Some("missing"))], "main")
                .layouts
                .is_empty()
        );
        let self_base =
            build_stack_index(&[m("a", "A", Some("A")), m("b", "B", Some("A"))], "main");
        assert_eq!(self_base.layouts[0].nodes[0].parent_branch, None);
    }

    #[test]
    fn sibling_order_is_deterministic_for_case_and_non_ascii_names() {
        let index = build_stack_index(
            &[
                m("root", "root", None),
                m("z", "z", Some("root")),
                m("accent", "é", Some("root")),
                m("lower", "a", Some("root")),
                m("upper", "A", Some("root")),
            ],
            "main",
        );
        assert_eq!(
            index.layouts[0]
                .nodes
                .iter()
                .map(|node| node.branch.as_str())
                .collect::<Vec<_>>(),
            ["root", "A", "a", "z", "é"]
        );
    }

    #[test]
    fn rendered_spines_match_order_and_split_groups() {
        assert_eq!(
            sp(&[
                ("a", "A", None),
                ("b", "B", Some("A")),
                ("d", "D", Some("B")),
                ("c", "C", Some("A"))
            ]),
            ["┌", "├", "│└", "└"]
        );
        assert_eq!(
            sp(&[("b", "B", Some("A")), ("c", "C", Some("B"))]),
            ["┌", "└"]
        );
        assert_eq!(sp(&[("a", "A", None), ("z", "Z", None)]), ["", ""]);
    }
}
