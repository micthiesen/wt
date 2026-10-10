use std::{path::Path, time::Duration};

use tokio_util::sync::CancellationToken;
use wt_platform::process::{CommandSpec, ProcessError, ProcessOutput, ProcessRunner};

use crate::service::StackError;

pub(crate) const GIT_TIMEOUT: Duration = Duration::from_secs(90);
pub(crate) const OUTPUT_LIMIT: usize = 16 * 1024 * 1024;

pub(crate) async fn run_git(
    runner: &ProcessRunner,
    cwd: &Path,
    args: impl IntoIterator<Item = impl Into<std::ffi::OsString>>,
    cancellation: &CancellationToken,
) -> Result<ProcessOutput, StackError> {
    let mut spec = CommandSpec::new("git").args(args).cwd(cwd.to_path_buf());
    spec.timeout = GIT_TIMEOUT;
    spec.output_limit = OUTPUT_LIMIT;
    runner
        .run(spec, cancellation)
        .await
        .map_err(StackError::Process)
}

pub(crate) async fn checked_git(
    runner: &ProcessRunner,
    cwd: &Path,
    args: impl IntoIterator<Item = impl Into<std::ffi::OsString>>,
    cancellation: &CancellationToken,
) -> Result<ProcessOutput, StackError> {
    run_git(runner, cwd, args, cancellation)
        .await?
        .checked("git")
        .map_err(StackError::Process)
}

pub(crate) async fn git_sha(
    runner: &ProcessRunner,
    cwd: &Path,
    reference: &str,
    cancellation: &CancellationToken,
) -> Result<Option<String>, StackError> {
    let output = run_git(
        runner,
        cwd,
        [
            "rev-parse",
            "--verify",
            "--quiet",
            "--end-of-options",
            &format!("{reference}^{{commit}}"),
        ],
        cancellation,
    )
    .await?;
    if !output.status.success() {
        return Ok(None);
    }
    let sha = output.stdout_text().trim().to_owned();
    Ok((!sha.is_empty()).then_some(sha))
}

pub(crate) async fn is_ancestor(
    runner: &ProcessRunner,
    cwd: &Path,
    ancestor: &str,
    descendant: &str,
    cancellation: &CancellationToken,
) -> Result<bool, StackError> {
    let output = run_git(
        runner,
        cwd,
        ["merge-base", "--is-ancestor", ancestor, descendant],
        cancellation,
    )
    .await?;
    Ok(output.status.success())
}

pub(crate) fn process_message(error: &ProcessError) -> String {
    match error {
        ProcessError::Exit { stderr, stdout, .. } => {
            let detail = if stderr.trim().is_empty() {
                stdout
            } else {
                stderr
            };
            detail.split_whitespace().collect::<Vec<_>>().join(" ")
        }
        _ => error.to_string(),
    }
}
