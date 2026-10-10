use std::collections::BTreeMap;

use wt_core::{ChainMember, WorktreeTarget, build_stack_index};
use wt_vcs::WorktreeRecord;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StackStep {
    pub slug: String,
    pub branch: String,
    pub parent_branch: Option<String>,
    pub base_sha: Option<String>,
    pub has_record: bool,
    pub worktree_path: String,
    pub target: WorktreeTarget,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestackChain {
    pub root: String,
    /// Parents precede children. Siblings are deterministic by branch name.
    pub steps: Vec<StackStep>,
}

pub(crate) fn resolve_chain(
    branch: &str,
    trunk: &str,
    records: &[WorktreeRecord],
    state: &serde_json::Value,
) -> Option<RestackChain> {
    let worktrees = records
        .iter()
        .filter(|row| !row.is_main && !row.target.branch.is_empty())
        .collect::<Vec<_>>();
    let members = worktrees
        .iter()
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
    let by_branch = worktrees
        .iter()
        .map(|row| (row.target.branch.as_str(), *row))
        .collect::<BTreeMap<_, _>>();
    let to_step = |slug: &str, branch: &str, parent: Option<String>| {
        let row = by_branch.get(branch)?;
        let record = &state["slugs"][slug];
        Some(StackStep {
            slug: slug.to_owned(),
            branch: branch.to_owned(),
            parent_branch: parent,
            base_sha: record["baseSha"].as_str().map(str::to_owned),
            has_record: record["baseBranch"].as_str().is_some(),
            worktree_path: row.target.path.clone(),
            target: row.target.clone(),
        })
    };

    let index = build_stack_index(&members, trunk);
    if let Some(entry) = index.by_branch.get(branch) {
        let layout = &index.layouts[entry.layout_index];
        let steps = layout
            .nodes
            .iter()
            .filter_map(|node| to_step(&node.slug, &node.branch, node.parent_branch.clone()))
            .collect();
        return Some(RestackChain {
            root: layout.stack_id.clone(),
            steps,
        });
    }

    let self_member = members.iter().find(|member| member.branch == branch)?;
    let record = &state["slugs"][&self_member.slug];
    let self_loop = record["baseBranch"].as_str() == Some(branch);
    let parent = if self_loop {
        None
    } else {
        record["baseBranch"]
            .as_str()
            .filter(|base| !base.is_empty() && *base != trunk)
            .map(str::to_owned)
    };
    Some(RestackChain {
        root: branch.to_owned(),
        steps: vec![to_step(&self_member.slug, branch, parent)?],
    })
}
