//! `wt orphans`: your open pull requests whose head branch no worktree row
//! tracks, each with the command that gives it one. One GitHub read, made
//! only when the command is run.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use clap::Args;
use serde::Deserialize;
use serde_json::json;
use wt_app_core::context::AppContext;
use wt_platform::process::CommandSpec;

#[derive(Debug, Clone, Args, Default)]
pub struct OrphansArgs {
    /// Print machine-readable JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OpenPr {
    number: u64,
    head_ref_name: String,
    title: String,
    is_draft: bool,
    url: String,
    #[serde(default)]
    is_cross_repository: bool,
}

#[derive(Debug, PartialEq, Eq)]
struct Orphan {
    number: u64,
    branch: String,
    title: String,
    draft: bool,
    url: String,
    /// The worktree whose checkout has been on the branch, when one has,
    /// and whether the branch is checked out there now.
    held_by: Option<(String, bool)>,
}

impl Orphan {
    /// The commands that give the pull request its own row.
    fn adopt(&self) -> String {
        match &self.held_by {
            Some((slug, true)) => format!(
                "switch {slug} back to its own branch, then: wt new {}",
                self.branch
            ),
            _ => format!("wt new {}", self.branch),
        }
    }
}

/// The pull requests of `prs` on a branch that is no row's own. `own` maps
/// each row's branch to its slug; `others` maps a row's other branches to
/// its slug and whether the branch is checked out there.
fn orphans(
    prs: Vec<OpenPr>,
    own: &BTreeMap<String, String>,
    others: &BTreeMap<String, (String, bool)>,
) -> Vec<Orphan> {
    let mut found: Vec<Orphan> = prs
        .into_iter()
        .filter(|pr| !pr.is_cross_repository && !own.contains_key(&pr.head_ref_name))
        .map(|pr| Orphan {
            held_by: others.get(&pr.head_ref_name).cloned(),
            number: pr.number,
            branch: pr.head_ref_name,
            title: pr.title,
            draft: pr.is_draft,
            url: pr.url,
        })
        .collect();
    found.sort_by_key(|orphan| orphan.number);
    found
}

pub async fn run(ctx: &AppContext, args: &OrphansArgs) -> Result<i32> {
    let listed = ctx
        .processes
        .run(
            CommandSpec::new("gh")
                .args([
                    "pr",
                    "list",
                    "--author",
                    "@me",
                    "--state",
                    "open",
                    "--limit",
                    "200",
                    "--json",
                    "number,headRefName,title,isDraft,url,isCrossRepository",
                ])
                .cwd(&ctx.config.paths.main_clone),
            &ctx.cancellation,
        )
        .await?
        .checked("gh pr list")?;
    let prs: Vec<OpenPr> =
        serde_json::from_slice(&listed.stdout).context("parse `gh pr list` output")?;
    let rows = ctx.repository.inventory(&ctx.cancellation).await?;
    let state = ctx
        .database
        .call(|store| Ok(store.read_wt_state()?))
        .await?;
    let mut own = BTreeMap::new();
    let mut others = BTreeMap::new();
    for row in &rows {
        let slug = row.target.slug().to_owned();
        let slug_state = &state["slugs"][&slug];
        let recorded = wt_core::recorded_branch(slug_state)
            .unwrap_or(&row.target.branch)
            .to_owned();
        if row.target.branch != recorded && !row.is_main {
            others.insert(row.target.branch.clone(), (slug.clone(), true));
        }
        for other in wt_core::stored_other_branches(slug_state) {
            others
                .entry(other.branch)
                .or_insert_with(|| (slug.clone(), false));
        }
        own.insert(recorded, slug);
    }
    let found = orphans(prs, &own, &others);
    if args.json {
        let rows: Vec<_> = found
            .iter()
            .map(|orphan| {
                json!({
                    "number": orphan.number, "branch": orphan.branch, "title": orphan.title,
                    "draft": orphan.draft, "url": orphan.url,
                    "held_by": orphan.held_by.as_ref().map(|(slug, _)| slug),
                    "adopt": orphan.adopt(),
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(0);
    }
    if found.is_empty() {
        println!("every open pull request of yours has a worktree row");
        return Ok(0);
    }
    println!(
        "{} open pull request{} of yours {} no worktree row:",
        found.len(),
        if found.len() == 1 { "" } else { "s" },
        if found.len() == 1 { "has" } else { "have" },
    );
    for orphan in &found {
        let held = orphan
            .held_by
            .as_ref()
            .map(|(slug, _)| format!("  (held by {slug})"))
            .unwrap_or_default();
        println!(
            "  #{}  {}{}  {}{held}",
            orphan.number,
            orphan.branch,
            if orphan.draft { "  draft" } else { "" },
            orphan.title,
        );
        println!("      {}", orphan.adopt());
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr(number: u64, branch: &str) -> OpenPr {
        OpenPr {
            number,
            head_ref_name: branch.into(),
            title: format!("PR {number}"),
            is_draft: false,
            url: String::new(),
            is_cross_repository: false,
        }
    }

    #[test]
    fn a_pull_request_is_an_orphan_unless_its_branch_is_a_rows_own() {
        let own = BTreeMap::from([("michael/stop-x".to_owned(), "stop-x".to_owned())]);
        let others = BTreeMap::from([
            ("michael/retire-x".to_owned(), ("stop-x".to_owned(), false)),
            ("michael/here".to_owned(), ("stop-x".to_owned(), true)),
        ]);
        let mut fork = pr(9, "someone/fork");
        fork.is_cross_repository = true;
        let found = orphans(
            vec![
                pr(4, "michael/gone"),
                pr(1, "michael/stop-x"),
                pr(3, "michael/here"),
                pr(2, "michael/retire-x"),
                fork,
            ],
            &own,
            &others,
        );
        let adopt: Vec<_> = found
            .iter()
            .map(|orphan| (orphan.number, orphan.adopt()))
            .collect();
        assert_eq!(
            adopt,
            [
                (2, "wt new michael/retire-x".to_owned()),
                (
                    3,
                    "switch stop-x back to its own branch, then: wt new michael/here".to_owned()
                ),
                (4, "wt new michael/gone".to_owned()),
            ]
        );
        assert_eq!(found[0].held_by, Some(("stop-x".to_owned(), false)));
        assert_eq!(found[2].held_by, None);
    }
}
