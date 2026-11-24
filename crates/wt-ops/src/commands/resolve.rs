use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

use anyhow::{Result, anyhow};
use wt_core::{WorktreeLocation, WorktreeTarget};
use wt_platform::process::{CommandSpec, ProcessOutput};
use wt_vcs::WorktreeRecord;

use wt_app_core::context::AppContext;

/// List wt-owned worktrees and resolve either the current checkout or an
/// explicit slug/branch. Exact names win; unique slug prefixes are accepted.
/// Multiple rows or a remote target are rejected before an operation routes.
pub async fn resolve_worktree(ctx: &AppContext, input: Option<&str>) -> Result<WorktreeRecord> {
    let records = ctx.repository.inventory(&ctx.cancellation).await?;
    let Some(input) = input else {
        let record = worktree_at_cwd(&records, &ctx.cwd)
            .ok_or_else(|| anyhow!("not inside a worktree; pass a worktree slug or branch"))?;
        ensure_local(&record.target)?;
        return Ok(record.clone());
    };
    resolve_from_inventory(&records, input, true)
}

/// Resolve an explicit slug or branch, including the main clone. Callers that
/// cannot act on the main clone must check `is_main` themselves. Destructive
/// commands use `resolve_exact_worktree` instead.
pub async fn resolve_named_worktree(ctx: &AppContext, input: &str) -> Result<WorktreeRecord> {
    let records = ctx.repository.inventory(&ctx.cancellation).await?;
    resolve_from_inventory(&records, input, false)
}

/// Resolve an explicit slug or branch for a destructive command. A prefix
/// is never enough: `eng-19` must not select `eng-1953-...`.
pub async fn resolve_exact_worktree(ctx: &AppContext, input: &str) -> Result<WorktreeRecord> {
    let records = ctx.repository.inventory(&ctx.cancellation).await?;
    resolve_exact_from_inventory(&records, input)
}

pub fn resolve_exact_from_inventory(
    records: &[WorktreeRecord],
    input: &str,
) -> Result<WorktreeRecord> {
    let exact = records
        .iter()
        .filter(|record| matches_exact(&record.target, input))
        .collect::<Vec<_>>();
    if exact.is_empty() {
        let near = records
            .iter()
            .filter(|record| !record.is_main && record.target.slug().starts_with(input))
            .map(|record| record.target.slug())
            .collect::<Vec<_>>();
        return Err(match near.as_slice() {
            [] => anyhow!("no such worktree: {input}"),
            [slug] => anyhow!("no worktree named {input}; did you mean {slug}?"),
            many => anyhow!(
                "no worktree named {input}; did you mean one of {}?",
                many.join(", ")
            ),
        });
    }
    let record = choose(input, exact)?;
    ensure_local(&record.target)?;
    Ok(record.clone())
}

pub fn resolve_from_inventory(
    records: &[WorktreeRecord],
    input: &str,
    exclude_main: bool,
) -> Result<WorktreeRecord> {
    let exact = records
        .iter()
        .filter(|record| (!exclude_main || !record.is_main) && matches_exact(&record.target, input))
        .collect::<Vec<_>>();
    let matches = if exact.is_empty() {
        records
            .iter()
            .filter(|record| {
                (!exclude_main || !record.is_main) && record.target.slug().starts_with(input)
            })
            .collect::<Vec<_>>()
    } else {
        exact
    };
    let record = choose(input, matches)?;
    ensure_local(&record.target)?;
    Ok(record.clone())
}

fn matches_exact(target: &WorktreeTarget, input: &str) -> bool {
    target.slug() == input || target.branch == input
}

fn choose<'a>(input: &str, matches: Vec<&'a WorktreeRecord>) -> Result<&'a WorktreeRecord> {
    match matches.as_slice() {
        [] => Err(anyhow!("no such worktree: {input}")),
        [record] => Ok(record),
        many => Err(anyhow!(
            "ambiguous worktree `{input}`; matches {}",
            many.iter()
                .map(|record| record.target.slug())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

fn ensure_local(target: &WorktreeTarget) -> Result<()> {
    if matches!(target.location(), WorktreeLocation::Remote { .. }) {
        return Err(anyhow!(
            "{} is remote; this command currently requires a local worktree",
            target.slug()
        ));
    }
    Ok(())
}

pub fn worktree_at_cwd<'a>(
    records: &'a [WorktreeRecord],
    cwd: &Path,
) -> Option<&'a WorktreeRecord> {
    records
        .iter()
        .filter(|record| path_contains(&record.target.path, cwd))
        .max_by_key(|record| record.target.path.len())
}

fn path_contains(root: &str, cwd: &Path) -> bool {
    let root = canonical_or_original(Path::new(root));
    let cwd = canonical_or_original(cwd);
    cwd.starts_with(root)
}

fn canonical_or_original(path: &Path) -> std::path::PathBuf {
    path.canonicalize().unwrap_or_else(|_| {
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| ".".into())
                .join(path)
        }
    })
}

pub async fn run_git(
    ctx: &AppContext,
    cwd: impl Into<PathBuf>,
    args: impl IntoIterator<Item = impl Into<OsString>>,
) -> Result<ProcessOutput> {
    let spec = CommandSpec::new("git").args(args).cwd(cwd);
    Ok(ctx.processes.run(spec, &ctx.cancellation).await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wt_vcs::RepositoryKind;

    fn record(slug: &str, branch: &str) -> WorktreeRecord {
        WorktreeRecord {
            target: wt_core::local_worktree_target(
                slug.to_owned(),
                branch.to_owned(),
                format!("/fixture/{slug}"),
                String::new(),
            ),
            is_main: false,
            head_sha: None,
            detached: false,
            kind: RepositoryKind::LinkedWorktree,
            locked: false,
            prunable: false,
            git_dir: None,
            common_dir: None,
        }
    }

    #[test]
    fn destructive_resolution_never_accepts_a_slug_prefix() {
        let records = [
            record("eng-1953-billing", "michael/eng-1953-billing"),
            record("eng-2-search", "michael/eng-2-search"),
            record("eng-20-search-index", "michael/eng-20-search-index"),
        ];
        // The non-destructive resolver still takes the unique prefix.
        assert_eq!(
            resolve_from_inventory(&records, "eng-19", true)
                .unwrap()
                .target
                .slug(),
            "eng-1953-billing"
        );
        assert_eq!(
            resolve_exact_from_inventory(&records, "eng-19")
                .unwrap_err()
                .to_string(),
            "no worktree named eng-19; did you mean eng-1953-billing?"
        );
        assert_eq!(
            resolve_exact_from_inventory(&records, "eng-2")
                .unwrap_err()
                .to_string(),
            "no worktree named eng-2; did you mean one of eng-2-search, eng-20-search-index?"
        );
        assert_eq!(
            resolve_exact_from_inventory(&records, "zzz")
                .unwrap_err()
                .to_string(),
            "no such worktree: zzz"
        );
        for exact in ["eng-2-search", "michael/eng-2-search"] {
            assert_eq!(
                resolve_exact_from_inventory(&records, exact)
                    .unwrap()
                    .target
                    .slug(),
                "eng-2-search"
            );
        }
    }
}
