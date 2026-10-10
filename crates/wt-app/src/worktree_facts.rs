//! Shared worktree facts used by local CLI output and worker snapshots.

use std::path::Path;

use crate::{commands::resolve::run_git, context::AppContext};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PushFacts {
    pub unpushed: Option<u64>,
    pub pushed: Option<bool>,
    pub ahead_of_base: Option<u64>,
}

/// Compute push counts using wt's public contract: `unpushed` is relative to
/// `origin/<branch>`, while `ahead_of_base` is relative to the recorded fork
/// base (or configured trunk). Nulls mean Git could not prove the fact.
pub(crate) async fn push_facts(
    context: &AppContext,
    path: &Path,
    branch: &str,
    effective_base: &str,
) -> PushFacts {
    let ahead_of_base = ahead_of_base(context, path, effective_base).await;
    let origin_ref = format!("origin/{branch}");
    let origin_exists = run_git(
        context,
        path,
        ["rev-parse", "--verify", "--quiet", origin_ref.as_str()],
    )
    .await
    .ok()
    .map(|output| output.status.success());

    match origin_exists {
        Some(true) => PushFacts {
            unpushed: commit_count(context, path, &format!("{origin_ref}..HEAD")).await,
            pushed: Some(true),
            ahead_of_base,
        },
        Some(false) => PushFacts {
            unpushed: ahead_of_base,
            pushed: Some(false),
            ahead_of_base,
        },
        None => PushFacts {
            unpushed: None,
            pushed: None,
            ahead_of_base,
        },
    }
}

async fn ahead_of_base(context: &AppContext, path: &Path, base: &str) -> Option<u64> {
    let trunk = &context.config.branch.base;
    let requested = if base.is_empty() {
        trunk.as_str()
    } else {
        base
    };
    let base_ref = if requested == trunk || requested == format!("origin/{trunk}") {
        let origin_trunk = format!("origin/{trunk}");
        let fresh = run_git(
            context,
            &context.config.paths.main_clone,
            ["rev-parse", origin_trunk.as_str()],
        )
        .await
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned());
        if let Some(sha) = fresh {
            let commit = format!("{sha}^{{commit}}");
            let available = run_git(context, path, ["cat-file", "-e", commit.as_str()])
                .await
                .ok()
                .is_some_and(|output| output.status.success());
            if available { sha } else { origin_trunk }
        } else {
            origin_trunk
        }
    } else {
        let local = run_git(context, path, ["rev-parse", requested])
            .await
            .ok()
            .filter(|output| output.status.success());
        if local.is_some() {
            requested.to_owned()
        } else {
            let origin = format!("origin/{requested}");
            let remote = run_git(context, path, ["rev-parse", origin.as_str()])
                .await
                .ok()
                .is_some_and(|output| output.status.success());
            if remote {
                origin
            } else {
                format!("origin/{trunk}")
            }
        }
    };
    commit_count(context, path, &format!("{base_ref}..HEAD")).await
}

async fn commit_count(context: &AppContext, path: &Path, range: &str) -> Option<u64> {
    let output = run_git(context, path, ["rev-list", "--count", range])
        .await
        .ok()?
        .checked("git")
        .ok()?;
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}
