//! Deploy proof for landings: whether a stage with a deploy probe
//! (`[environments.<stage>]`) is running a commit. The proof is the Git ref
//! the deploy source maintains (`wt_vcs::deployed_contains`); this is the one
//! reading every landing computation shares.

use tokio_util::sync::CancellationToken;
use wt_app_core::context::AppContext;
use wt_core::VerifyStage;

/// Whether any of `commits` (a landing's HEAD and exact merge commit) is
/// deployed to `stage`. `None` when the stage has no probe: deploys are not
/// tracked there and callers keep the Git landing alone. With a probe, an
/// unknown answer (nothing recorded yet, a commit this clone lacks, a Git
/// failure) reads as `Some(false)`: not shown deployed.
pub async fn landing_deployed<S: AsRef<str>>(
    ctx: &AppContext,
    stage: VerifyStage,
    commits: &[S],
    cancel: &CancellationToken,
) -> Option<bool> {
    ctx.config.environments.get(stage)?;
    for commit in commits {
        if wt_vcs::deployed_contains(
            &ctx.processes,
            &ctx.config.paths.main_clone,
            stage,
            commit.as_ref(),
            cancel,
        )
        .await
            == Some(true)
        {
            return Some(true);
        }
    }
    Some(false)
}

/// The commit `stage`'s probe last reported, or `None` when the stage has no
/// probe, nothing was recorded yet, or the ref cannot be read.
pub async fn deployed_oid(
    ctx: &AppContext,
    stage: VerifyStage,
    cancel: &CancellationToken,
) -> Option<String> {
    ctx.config.environments.get(stage)?;
    wt_vcs::deployed_oid(&ctx.processes, &ctx.config.paths.main_clone, stage, cancel)
        .await
        .ok()
        .flatten()
}
