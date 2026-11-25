//! Deploy proof kept in Git. A stage's deploy probe reports the commit the
//! stage is running; wt records it as `refs/wt/deployed/<stage>` in the main
//! clone. Every reader asks the same question of that ref, so the board,
//! automations and watches cannot disagree about what is deployed, and no
//! cached source has to read durable state to answer it.

use std::{path::Path, time::Duration};

use tokio_util::sync::CancellationToken;
use wt_core::VerifyStage;
use wt_platform::process::{CommandSpec, ProcessOutput, ProcessRunner};

const GIT_TIMEOUT: Duration = Duration::from_secs(8);
const FETCH_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct DeployProofError(pub String);

/// The first whitespace-delimited token of a probe's stdout, when it can be
/// a commit SHA: 7 to 40 hexadecimal characters.
pub fn parse_probe_output(stdout: &str) -> Result<&str, DeployProofError> {
    let Some(token) = stdout.split_whitespace().next() else {
        return Err(DeployProofError("the probe printed nothing".into()));
    };
    if (7..=40).contains(&token.len()) && token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(token)
    } else {
        let shown: String = token.chars().take(48).collect();
        Err(DeployProofError(format!(
            "the probe printed {shown:?}, not a commit SHA (7 to 40 hexadecimal characters)"
        )))
    }
}

/// The commit `stage`'s probe last reported, or `None` when none was recorded.
pub async fn deployed_oid(
    runner: &ProcessRunner,
    main_clone: &Path,
    stage: VerifyStage,
    cancel: &CancellationToken,
) -> Result<Option<String>, DeployProofError> {
    rev_parse_commit(runner, main_clone, &stage.deployed_ref(), cancel).await
}

/// Whether `commit` is deployed to `stage`: it is the stage's recorded
/// commit or an ancestor of it. `None` is unknown: nothing recorded yet, a
/// commit this clone does not have, or a Git failure.
pub async fn deployed_contains(
    runner: &ProcessRunner,
    main_clone: &Path,
    stage: VerifyStage,
    commit: &str,
    cancel: &CancellationToken,
) -> Option<bool> {
    let output = git(
        runner,
        main_clone,
        ["merge-base", "--is-ancestor", commit, &stage.deployed_ref()],
        GIT_TIMEOUT,
        cancel,
    )
    .await
    .ok()?;
    match output.status.code() {
        Some(0) => Some(true),
        Some(1) => Some(false),
        _ => None,
    }
}

/// The full id of the commit a probe named. With `fetch`, a commit this clone
/// lacks is fetched from origin once before giving up.
pub async fn resolve_deployed_commit(
    runner: &ProcessRunner,
    main_clone: &Path,
    sha: &str,
    fetch: bool,
    cancel: &CancellationToken,
) -> Result<String, DeployProofError> {
    let sha = parse_probe_output(sha)?;
    if let Some(oid) = rev_parse_commit(runner, main_clone, sha, cancel).await? {
        return Ok(oid);
    }
    if !fetch {
        return Err(DeployProofError(format!(
            "commit {sha} is not in this repository"
        )));
    }
    // Only a full id can be requested by name; an abbreviation needs the
    // remote's branches.
    let mut args = vec!["fetch", "--quiet", "--no-write-fetch-head", "origin"];
    if sha.len() == 40 {
        args.extend(["--no-tags", sha]);
    }
    let fetched = git(runner, main_clone, args, FETCH_TIMEOUT, cancel).await?;
    match rev_parse_commit(runner, main_clone, sha, cancel).await? {
        Some(oid) => Ok(oid),
        None if fetched.status.success() => Err(DeployProofError(format!(
            "commit {sha} is not in this repository or on origin"
        ))),
        None => Err(DeployProofError(format!(
            "commit {sha} is not in this repository and fetching origin failed: {}",
            last_line(&fetched.stderr_text())
        ))),
    }
}

/// Point the stage's ref at `oid`. Returns whether it moved.
pub async fn record_deployed(
    runner: &ProcessRunner,
    main_clone: &Path,
    stage: VerifyStage,
    oid: &str,
    cancel: &CancellationToken,
) -> Result<bool, DeployProofError> {
    if deployed_oid(runner, main_clone, stage, cancel)
        .await?
        .as_deref()
        == Some(oid)
    {
        return Ok(false);
    }
    let output = git(
        runner,
        main_clone,
        ["update-ref", &stage.deployed_ref(), oid],
        GIT_TIMEOUT,
        cancel,
    )
    .await?;
    if output.status.success() {
        Ok(true)
    } else {
        Err(DeployProofError(format!(
            "cannot record the deployed commit: {}",
            last_line(&output.stderr_text())
        )))
    }
}

async fn rev_parse_commit(
    runner: &ProcessRunner,
    main_clone: &Path,
    name: &str,
    cancel: &CancellationToken,
) -> Result<Option<String>, DeployProofError> {
    let output = git(
        runner,
        main_clone,
        [
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{name}^{{commit}}"),
        ],
        GIT_TIMEOUT,
        cancel,
    )
    .await?;
    let oid = output.stdout_text().trim().to_owned();
    match output.status.code() {
        Some(0) if !oid.is_empty() => Ok(Some(oid)),
        Some(0 | 1) => Ok(None),
        _ => Err(DeployProofError(format!(
            "git rev-parse {name}: {}",
            last_line(&output.stderr_text())
        ))),
    }
}

async fn git<'a>(
    runner: &ProcessRunner,
    main_clone: &Path,
    args: impl IntoIterator<Item = &'a str>,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<ProcessOutput, DeployProofError> {
    let mut spec = CommandSpec::new("git").args(args).cwd(main_clone);
    spec.env = vec![("GIT_TERMINAL_PROMPT".into(), Some("0".into()))];
    spec.timeout = timeout;
    spec.output_limit = 64 * 1024;
    runner
        .run(spec, cancel)
        .await
        .map_err(|error| DeployProofError(error.to_string()))
}

fn last_line(text: &str) -> String {
    text.lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("no output")
        .trim()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    async fn fixture_git(runner: &ProcessRunner, cwd: &Path, args: &[&str]) -> String {
        let mut spec = CommandSpec::new("git").args(args.iter().copied()).cwd(cwd);
        for (key, value) in [
            ("GIT_AUTHOR_NAME", "wt test"),
            ("GIT_AUTHOR_EMAIL", "wt-test@example.invalid"),
            ("GIT_COMMITTER_NAME", "wt test"),
            ("GIT_COMMITTER_EMAIL", "wt-test@example.invalid"),
            ("GIT_CONFIG_GLOBAL", "/dev/null"),
            ("GIT_CONFIG_SYSTEM", "/dev/null"),
        ] {
            spec.env.push((key.into(), Some(value.into())));
        }
        runner
            .run(spec, &CancellationToken::new())
            .await
            .unwrap()
            .checked("git fixture")
            .unwrap()
            .stdout_text()
            .trim()
            .to_owned()
    }

    async fn commit(runner: &ProcessRunner, repo: &Path, name: &str) -> String {
        std::fs::write(repo.join(name), name).unwrap();
        fixture_git(runner, repo, &["add", name]).await;
        fixture_git(runner, repo, &["commit", "-m", name]).await;
        fixture_git(runner, repo, &["rev-parse", "HEAD"]).await
    }

    /// An origin with three commits and a clone that has only the first.
    async fn fixture() -> (tempfile::TempDir, ProcessRunner, PathBuf, [String; 3]) {
        let root = tempfile::tempdir().unwrap();
        let origin = root.path().join("origin");
        let clone = root.path().join("main clone");
        std::fs::create_dir_all(&origin).unwrap();
        let runner = ProcessRunner::default();
        fixture_git(&runner, &origin, &["init", "-b", "main"]).await;
        // Lets a clone ask for a commit by id, as hosted remotes do.
        fixture_git(
            &runner,
            &origin,
            &["config", "uploadpack.allowAnySHA1InWant", "true"],
        )
        .await;
        let first = commit(&runner, &origin, "one").await;
        fixture_git(
            &runner,
            root.path(),
            &["clone", "--quiet", origin.to_str().unwrap(), "main clone"],
        )
        .await;
        let second = commit(&runner, &origin, "two").await;
        let third = commit(&runner, &origin, "three").await;
        (root, runner, clone, [first, second, third])
    }

    #[test]
    fn a_probe_names_a_commit_with_its_first_token() {
        assert_eq!(
            parse_probe_output("  abc1234 deployed 2m ago\n").unwrap(),
            "abc1234"
        );
        let full = "0123456789abcdefABCDEF0123456789abcdef01";
        assert_eq!(parse_probe_output(full).unwrap(), full);
        for bad in ["", "  \n", "abc123", "v1.2.3-release", &format!("{full}0")] {
            assert!(parse_probe_output(bad).is_err(), "{bad:?}");
        }
    }

    #[tokio::test]
    async fn a_commit_is_deployed_once_the_stage_ref_contains_it() {
        let (_root, runner, clone, [first, second, third]) = fixture().await;
        let cancel = CancellationToken::new();
        let stage = VerifyStage::Staging;
        // Nothing recorded: unknown, never "not deployed".
        assert_eq!(
            deployed_oid(&runner, &clone, stage, &cancel).await,
            Ok(None)
        );
        assert_eq!(
            deployed_contains(&runner, &clone, stage, &first, &cancel).await,
            None
        );

        assert_eq!(
            record_deployed(&runner, &clone, stage, &first, &cancel).await,
            Ok(true)
        );
        assert_eq!(
            record_deployed(&runner, &clone, stage, &first, &cancel).await,
            Ok(false)
        );
        assert_eq!(
            deployed_contains(&runner, &clone, stage, &first, &cancel).await,
            Some(true)
        );
        // A commit this clone has never seen is unknown.
        assert_eq!(
            deployed_contains(&runner, &clone, stage, &third, &cancel).await,
            None
        );
        // The other stage has its own ref.
        assert_eq!(
            deployed_contains(&runner, &clone, VerifyStage::Production, &first, &cancel).await,
            None
        );

        // A reported commit the clone lacks is fetched, full or abbreviated.
        assert!(
            resolve_deployed_commit(&runner, &clone, &second, false, &cancel)
                .await
                .is_err()
        );
        assert_eq!(
            resolve_deployed_commit(&runner, &clone, &second, true, &cancel).await,
            Ok(second.clone())
        );
        assert_eq!(
            resolve_deployed_commit(&runner, &clone, &third[..10], true, &cancel).await,
            Ok(third.clone())
        );
        let unknown = "f".repeat(40);
        let error = resolve_deployed_commit(&runner, &clone, &unknown, true, &cancel)
            .await
            .unwrap_err();
        assert!(error.0.contains("is not in this repository"), "{error}");

        record_deployed(&runner, &clone, stage, &second, &cancel)
            .await
            .unwrap();
        assert_eq!(
            deployed_oid(&runner, &clone, stage, &cancel).await,
            Ok(Some(second.clone()))
        );
        assert_eq!(
            deployed_contains(&runner, &clone, stage, &first, &cancel).await,
            Some(true)
        );
        assert_eq!(
            deployed_contains(&runner, &clone, stage, &third, &cancel).await,
            Some(false)
        );
    }
}
