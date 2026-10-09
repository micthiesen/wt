use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

use anyhow::{Result, anyhow};
use wt_core::{WorktreeLocation, WorktreeTarget};
use wt_platform::process::{CommandSpec, ProcessOutput};
use wt_vcs::WorktreeRecord;

use crate::context::AppContext;

/// List wt-owned worktrees and resolve either the current checkout or an
/// explicit slug/branch. Exact names win; unique slug prefixes are accepted.
/// Multiple rows or a remote target are rejected before an operation routes.
pub async fn resolve_worktree(ctx: &AppContext, input: Option<&str>) -> Result<WorktreeRecord> {
    let records = ctx.repository.inventory(&ctx.cancellation).await?;
    let record = if let Some(input) = input {
        let exact = records
            .iter()
            .filter(|record| !record.is_main && matches_exact(&record.target, input))
            .collect::<Vec<_>>();
        let matches = if exact.is_empty() {
            records
                .iter()
                .filter(|record| !record.is_main && record.target.slug().starts_with(input))
                .collect::<Vec<_>>()
        } else {
            exact
        };
        choose(input, matches)?
    } else {
        worktree_at_cwd(&records, &ctx.cwd)
            .ok_or_else(|| anyhow!("not inside a worktree; pass a worktree slug or branch"))?
    };
    ensure_local(&record.target)?;
    Ok(record.clone())
}

/// Resolve an explicit target while allowing the main clone only when callers
/// opt in (for example, `status` is cwd-aware but only displays non-main rows).
pub async fn resolve_named_worktree(ctx: &AppContext, input: &str) -> Result<WorktreeRecord> {
    let records = ctx.repository.inventory(&ctx.cancellation).await?;
    resolve_from_inventory(&records, input, false)
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

pub(crate) fn worktree_at_cwd<'a>(
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
