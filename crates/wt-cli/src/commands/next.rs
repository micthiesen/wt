//! `wt next`: keep a worktree whose PR landed and start its next PR there.
//!
//! The worktree keeps its slug, directory, sessions, section and status
//! history. Only its branch changes: a fresh branch from the latest trunk has
//! no PR history, so every PR-keyed surface (board, automations, `wt fleet`)
//! sees "no PR yet" and then the new PR, never the merged one.

use std::path::Path;

use anyhow::{Context, Result};
use clap::Args;
use serde::Deserialize;
use wt_platform::{lock::FileLock, process::CommandSpec};
use wt_store::BaseUpdate;

use crate::commands::resolve::{resolve_worktree, run_git};
use wt_app_core::context::AppContext;

const USAGE: &str = "usage: wt next [BRANCH] [--slug SLUG] [--issue ID|none] [--gh N|none]";

#[derive(Debug, Clone, Args, Default)]
pub struct NextArgs {
    /// Branch for the next PR; a name without `/` gets the configured prefix.
    /// Defaults to `<prefix>/<slug>-<n>`, the first unused name.
    #[arg(value_name = "BRANCH")]
    pub branch: Option<String>,
    /// Worktree to continue; defaults to the one containing the current directory.
    #[arg(long, value_name = "SLUG")]
    pub slug: Option<String>,
    /// Tracker issue of the next PR (`none` asserts that it has none).
    #[arg(long, value_name = "ID")]
    pub issue: Option<String>,
    /// GitHub issue of the next PR (`none` clears the link).
    #[arg(long, value_name = "N")]
    pub gh: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BranchPr {
    number: u64,
    state: String,
    head_ref_oid: String,
    base_ref_name: String,
    is_cross_repository: bool,
}

pub async fn run(ctx: &AppContext, args: &NextArgs) -> Result<i32> {
    let issue = match args.issue.as_deref().map(str::trim) {
        None => None,
        Some(value) if value.eq_ignore_ascii_case("none") => Some(String::new()),
        Some(value) if crate::commands::issue::valid_issue_id(&value.to_ascii_uppercase()) => {
            Some(value.to_ascii_uppercase())
        }
        Some(value) => {
            eprintln!("not an issue id: {value} (expected e.g. COZ-2185, or none)\n{USAGE}");
            return Ok(2);
        }
    };
    let gh = match args.gh.as_deref().map(str::trim) {
        None => None,
        Some(value) if value.eq_ignore_ascii_case("none") => Some(None),
        Some(value) => match value.trim_start_matches('#').parse::<u64>() {
            Ok(number) if number > 0 => Some(Some(number)),
            _ => {
                eprintln!("--gh takes an issue number or none\n{USAGE}");
                return Ok(2);
            }
        },
    };
    let row = match resolve_worktree(ctx, args.slug.as_deref()).await {
        Ok(row) if !row.is_main => row,
        Ok(_) => {
            eprintln!("the main clone stays on trunk; run wt next inside a worktree");
            return Ok(1);
        }
        Err(error) => {
            eprintln!("{error}");
            return Ok(1);
        }
    };
    let slug = row.target.slug().to_owned();
    let path = Path::new(&row.target.path).to_path_buf();
    let old_branch = row.target.branch.clone();
    let trunk = ctx.config.branch.base.clone();
    if old_branch.is_empty() || row.detached {
        eprintln!("{slug} is not on a branch; check out its branch before wt next");
        return Ok(1);
    }
    // Removal, restack and creation hold this lock while they change the
    // checkout; never wait behind one.
    let Some(_lock) = FileLock::try_acquire(&ctx.config.paths.lock_dir, &slug, "next").await?
    else {
        eprintln!("{slug} is busy with another wt operation; retry when it finishes");
        return Ok(1);
    };

    let checked_out = git_text(ctx, &path, ["symbolic-ref", "--quiet", "--short", "HEAD"])
        .await
        .unwrap_or_default();
    if checked_out.trim() != old_branch {
        eprintln!("{slug} switched branches while wt next started; retry");
        return Ok(1);
    }
    let state = ctx
        .database
        .call(|store| Ok(store.read_wt_state()?))
        .await?;
    // A live worktree stacked on this branch would be left based on a
    // deleted branch. Its restack reparents it once this PR landed. Removed
    // worktrees keep their records, so only checkouts that exist count.
    let live = ctx
        .repository
        .inventory(&ctx.cancellation)
        .await?
        .into_iter()
        .filter(|row| !row.is_main)
        .map(|row| row.target.slug().to_owned())
        .collect::<std::collections::BTreeSet<_>>();
    if let Some(child) = live.iter().find(|other| {
        **other != slug
            && state["slugs"][other.as_str()]["baseBranch"].as_str() == Some(old_branch.as_str())
    }) {
        eprintln!(
            "{child} is stacked on {old_branch}; restack it (`wt restack {child}`) before starting the next PR here"
        );
        return Ok(1);
    }
    let previous = (
        state["slugs"][&slug]["baseBranch"]
            .as_str()
            .map(str::to_owned),
        state["slugs"][&slug]["baseSha"].as_str().map(str::to_owned),
    );

    let dirty = git_text(
        ctx,
        &path,
        ["status", "--porcelain", "--untracked-files=no"],
    )
    .await?;
    if !dirty.trim().is_empty() {
        eprintln!(
            "{slug} has uncommitted changes to tracked files; commit them to {old_branch} (and land them) or discard them first"
        );
        return Ok(1);
    }
    let trunk_ref = format!("refs/remotes/origin/{trunk}");
    let fetched = run_git(
        ctx,
        &path,
        [
            "fetch",
            "--quiet",
            "origin",
            &format!("+refs/heads/{trunk}:{trunk_ref}"),
        ],
    )
    .await?;
    if !fetched.status.success() {
        eprintln!(
            "could not fetch origin/{trunk}: {}",
            fetched.stderr_text().trim()
        );
        return Ok(1);
    }
    let trunk_tip = rev(ctx, &path, &trunk_ref).await?;
    let head = rev(ctx, &path, "HEAD").await?;

    let prs = branch_prs(ctx, &path, &old_branch).await?;
    let contained = run_git(
        ctx,
        &path,
        ["merge-base", "--is-ancestor", &head, &trunk_tip],
    )
    .await?
    .status
    .success();
    match landed(&prs, &head, &trunk, contained) {
        Landed::Yes(proof) => println!("{old_branch} landed ({proof})"),
        Landed::Open(number) => {
            eprintln!(
                "#{number} from {old_branch} is still open; land or close it before starting the next PR here"
            );
            return Ok(1);
        }
        Landed::No => {
            eprintln!(
                "{old_branch} has commits that are not in origin/{trunk} or in the head of a PR merged into {trunk}; land them first, or move them with `wt new --base {old_branch}`"
            );
            return Ok(1);
        }
    }

    let branch = match &args.branch {
        Some(name) => {
            let name = name.trim();
            let name = if name.contains('/') {
                name.to_owned()
            } else {
                prefixed(ctx, name)
            };
            if name.is_empty() || !valid_branch_name(ctx, &path, &name).await? {
                eprintln!("not a valid branch name: {name}\n{USAGE}");
                return Ok(2);
            }
            if local_branch_exists(ctx, &path, &name).await?
                || remote_branches(ctx, &path, &format!("refs/heads/{name}"))
                    .await?
                    .contains(&name)
            {
                eprintln!("branch {name} already exists locally or on origin; choose another name");
                return Ok(1);
            }
            name
        }
        None => {
            let stem = prefixed(ctx, &format!("{slug}-"));
            let taken = remote_branches(ctx, &path, &format!("refs/heads/{stem}*")).await?;
            let mut found = None;
            for n in 2..1000 {
                let candidate = format!("{stem}{n}");
                if candidate != old_branch
                    && !taken.contains(&candidate)
                    && !local_branch_exists(ctx, &path, &candidate).await?
                {
                    found = Some(candidate);
                    break;
                }
            }
            found.context("no unused <slug>-<n> branch name; pass a branch name")?
        }
    };

    // Record the base first. With the trunk tip as the anchor and HEAD still
    // on the landed branch (contained in, or squashed into, the trunk), the
    // worktree has no own commits, so a later failure can never leave it
    // reading as landed and clean against a stale anchor.
    match write_base(
        ctx,
        &slug,
        (previous.0.as_deref(), previous.1.as_deref()),
        &trunk,
        &trunk_tip,
    )
    .await?
    {
        BaseUpdate::Updated => {}
        BaseUpdate::Stale => {
            eprintln!(
                "{slug}'s fork base changed while wt next ran; nothing was switched, retry from the current state"
            );
            return Ok(1);
        }
        BaseUpdate::Cycle => {
            eprintln!(
                "recording {trunk} as {slug}'s fork base would create a stack cycle; nothing was switched"
            );
            return Ok(1);
        }
    }
    // The agent session is not under the operation lock: refuse if it
    // committed since the landing proof.
    if rev(ctx, &path, "HEAD").await? != head {
        restore_base(ctx, &slug, &previous, &trunk, &trunk_tip).await;
        eprintln!("{old_branch} moved while wt next ran; nothing was switched, retry");
        return Ok(1);
    }
    let switched = run_git(
        ctx,
        &path,
        [
            "switch",
            "--no-track",
            "-c",
            branch.as_str(),
            trunk_tip.as_str(),
        ],
    )
    .await?;
    if !switched.status.success() {
        restore_base(ctx, &slug, &previous, &trunk, &trunk_tip).await;
        eprintln!(
            "could not create {branch} at origin/{trunk}: {}",
            switched.stderr_text().trim()
        );
        return Ok(1);
    }
    let mut issue_note = None;
    if issue.is_some() || gh.is_some() {
        let write_slug = slug.clone();
        let written = ctx
            .database
            .call(move |store| {
                if let Some(issue) = issue.as_deref() {
                    store.set_slug_issue_id(&write_slug, Some(issue))?;
                }
                if let Some(gh) = gh {
                    store.set_slug_github_issue(&write_slug, gh)?;
                }
                Ok(())
            })
            .await;
        issue_note = Some(match written {
            Ok(()) => "  issue links updated; `wt issue` shows them".to_owned(),
            Err(error) => format!(
                "  warning: issue links were not updated ({error:#}); set them with `wt issue {slug}`"
            ),
        });
    }
    // Last, and only at the proven head: a branch that moved is kept.
    let deleted = run_git(
        ctx,
        &path,
        [
            "update-ref",
            "-d",
            &format!("refs/heads/{old_branch}"),
            head.as_str(),
        ],
    )
    .await?
    .status
    .success();

    println!(
        "✓ {slug} is on {branch} at origin/{trunk} {}",
        short(&trunk_tip)
    );
    if deleted {
        println!("  deleted local branch {old_branch} (its work landed)");
    } else {
        println!("  kept local branch {old_branch}: it could not be deleted at its landed head");
    }
    if let Some(note) = issue_note {
        println!("{note}");
    }
    println!(
        "next: `wt status working -m <what this PR does>`, commit, then `git push -u origin HEAD` and open the PR"
    );
    Ok(0)
}

#[derive(Debug, PartialEq, Eq)]
enum Landed {
    Yes(String),
    Open(u64),
    No,
}

/// The current branch's work is safe to leave when trunk contains its head,
/// or a PR from this repository merged into trunk with exactly this head
/// (squash merges leave trunk without the branch's own commits). A PR merged
/// into another branch is not landing, and an open PR is never left behind.
fn landed(prs: &[BranchPr], head: &str, trunk: &str, contained: bool) -> Landed {
    let own = prs.iter().filter(|pr| !pr.is_cross_repository);
    if let Some(open) = own.clone().find(|pr| pr.state == "OPEN") {
        return Landed::Open(open.number);
    }
    if contained {
        return Landed::Yes(format!("origin/{trunk} contains {}", short(head)));
    }
    match own
        .filter(|pr| pr.state == "MERGED" && pr.base_ref_name == trunk)
        .find(|pr| pr.head_ref_oid == head)
    {
        Some(pr) => Landed::Yes(format!("#{} merged into {trunk} at this head", pr.number)),
        None => Landed::No,
    }
}

async fn branch_prs(ctx: &AppContext, path: &Path, branch: &str) -> Result<Vec<BranchPr>> {
    let output = ctx
        .processes
        .run(
            CommandSpec::new("gh")
                .args([
                    "pr",
                    "list",
                    "--state",
                    "all",
                    "--head",
                    branch,
                    "--json",
                    "number,state,headRefOid,baseRefName,isCrossRepository",
                ])
                .cwd(path),
            &ctx.cancellation,
        )
        .await?
        .checked("gh pr list")?;
    serde_json::from_slice(&output.stdout).context("parse `gh pr list` output")
}

/// Write the base only while the record is still what this run read.
async fn write_base(
    ctx: &AppContext,
    slug: &str,
    expected: (Option<&str>, Option<&str>),
    branch: &str,
    sha: &str,
) -> Result<BaseUpdate> {
    let branches = ctx
        .repository
        .inventory(&ctx.cancellation)
        .await?
        .into_iter()
        .map(|row| (row.target.branch.clone(), row.target.slug().to_owned()))
        .collect::<std::collections::BTreeMap<_, _>>();
    let expected = (expected.0.map(str::to_owned), expected.1.map(str::to_owned));
    let (slug, branch, sha) = (slug.to_owned(), branch.to_owned(), sha.to_owned());
    let outcome = ctx
        .database
        .call(move |store| {
            Ok(store.set_slug_base_checked(
                &slug,
                (expected.0.as_deref(), expected.1.as_deref()),
                &branch,
                &sha,
                &branches,
            )?)
        })
        .await?;
    Ok(outcome)
}

/// Undo `write_base` when the switch did not happen. Best effort: a record
/// at the trunk tip only understates the old branch's own work.
async fn restore_base(
    ctx: &AppContext,
    slug: &str,
    previous: &(Option<String>, Option<String>),
    trunk: &str,
    trunk_tip: &str,
) {
    let (Some(branch), Some(sha)) = previous else {
        return;
    };
    let _ = write_base(ctx, slug, (Some(trunk), Some(trunk_tip)), branch, sha).await;
}

async fn local_branch_exists(ctx: &AppContext, path: &Path, branch: &str) -> Result<bool> {
    Ok(run_git(
        ctx,
        path,
        [
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )
    .await?
    .status
    .success())
}

/// Branch names on origin matching an exact `refs/heads/...` pattern.
async fn remote_branches(
    ctx: &AppContext,
    path: &Path,
    pattern: &str,
) -> Result<std::collections::BTreeSet<String>> {
    let output = run_git(ctx, path, ["ls-remote", "origin", pattern]).await?;
    if !output.status.success() {
        anyhow::bail!(
            "could not list branches on origin: {}",
            output.stderr_text().trim()
        );
    }
    Ok(output
        .stdout_text()
        .lines()
        .filter_map(|line| line.split_whitespace().nth(1))
        .filter_map(|reference| reference.strip_prefix("refs/heads/"))
        .map(str::to_owned)
        .collect())
}

async fn valid_branch_name(ctx: &AppContext, path: &Path, name: &str) -> Result<bool> {
    Ok(run_git(ctx, path, ["check-ref-format", "--branch", name])
        .await?
        .status
        .success())
}

/// `wt new` names branches `<prefix>/<name>`.
fn prefixed(ctx: &AppContext, name: &str) -> String {
    match ctx.config.branch.prefix.trim_end_matches('/') {
        "" => name.to_owned(),
        prefix => format!("{prefix}/{name}"),
    }
}

async fn rev(ctx: &AppContext, path: &Path, reference: &str) -> Result<String> {
    Ok(git_text(
        ctx,
        path,
        ["rev-parse", "--verify", &format!("{reference}^{{commit}}")],
    )
    .await?
    .trim()
    .to_owned())
}

async fn git_text<const N: usize>(
    ctx: &AppContext,
    path: &Path,
    args: [&str; N],
) -> Result<String> {
    Ok(run_git(ctx, path, args)
        .await?
        .checked("git")?
        .stdout_text())
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(12)]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr(number: u64, state: &str, head: &str, base: &str, fork: bool) -> BranchPr {
        BranchPr {
            number,
            state: state.into(),
            head_ref_oid: head.into(),
            base_ref_name: base.into(),
            is_cross_repository: fork,
        }
    }

    #[test]
    fn landing_needs_trunk_containment_or_an_exact_merge_into_trunk() {
        let staging = "staging";
        // Merge-commit and rebase merges: trunk contains the head.
        assert!(matches!(landed(&[], "h", staging, true), Landed::Yes(_)));
        // Squash merge: only an exact merged head into trunk proves it.
        let squash = [pr(7, "MERGED", "h", staging, false)];
        assert!(matches!(
            landed(&squash, "h", staging, false),
            Landed::Yes(_)
        ));
        // New commits after that merge are not landed.
        assert_eq!(landed(&squash, "newer", staging, false), Landed::No);
        // A PR merged into its stack parent is not in trunk.
        let stacked = [pr(8, "MERGED", "h", "parent", false)];
        assert_eq!(landed(&stacked, "h", staging, false), Landed::No);
        // A fork's same-named branch proves nothing about this one.
        let fork = [pr(9, "MERGED", "h", staging, true)];
        assert_eq!(landed(&fork, "h", staging, false), Landed::No);
        // An open PR blocks even when trunk contains the head.
        let open = [
            pr(7, "MERGED", "h", staging, false),
            pr(10, "OPEN", "h", staging, false),
        ];
        assert_eq!(landed(&open, "h", staging, true), Landed::Open(10));
        // A fork's open PR does not.
        let fork_open = [pr(11, "OPEN", "h", staging, true)];
        assert!(matches!(
            landed(&fork_open, "h", staging, true),
            Landed::Yes(_)
        ));
    }
}
