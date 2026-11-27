//! What a watch can wait on beyond the board: whether a commit reached a
//! stage, what identifies a stage's current release, configured checks, and
//! pull requests named by number. Each reads Git, a probe command or GitHub,
//! so callers run them off any loop that must stay responsive. Unknown is
//! reported as unknown, never as "not yet".

use std::{collections::BTreeMap, time::Duration};

use tokio_util::sync::CancellationToken;
use wt_app_core::context::AppContext;
use wt_config::CheckConfig;
use wt_core::{
    VerifyStage,
    watch::{CheckKey, PrFacts, PrLife},
};
use wt_github::{GithubClient, GithubOptions};
use wt_platform::process::CommandSpec;

const GIT_TIMEOUT: Duration = Duration::from_secs(8);
const OUTPUT_LIMIT: usize = 64 * 1024;

/// The branch whose tip is what a stage runs when nothing reports deploys.
pub fn stage_branch(ctx: &AppContext, stage: VerifyStage) -> &str {
    let branch = &ctx.config.branch;
    match stage {
        VerifyStage::Staging => &branch.base,
        VerifyStage::Production => branch.production.as_deref().unwrap_or(&branch.base),
    }
}

async fn git(
    ctx: &AppContext,
    args: &[&str],
    cancel: &CancellationToken,
) -> Option<wt_platform::process::ProcessOutput> {
    let mut spec = CommandSpec::new("git")
        .args(args.iter().copied())
        .cwd(&ctx.config.paths.main_clone);
    spec.env = vec![("GIT_TERMINAL_PROMPT".into(), Some("0".into()))];
    spec.timeout = GIT_TIMEOUT;
    spec.output_limit = OUTPUT_LIMIT;
    ctx.processes.run(spec, cancel).await.ok()
}

/// The stage branch as the main clone last saw it on origin, or the local
/// branch where origin has none.
async fn stage_tip(
    ctx: &AppContext,
    stage: VerifyStage,
    cancel: &CancellationToken,
) -> Option<String> {
    let branch = stage_branch(ctx, stage);
    for name in [
        format!("refs/remotes/origin/{branch}"),
        format!("refs/heads/{branch}"),
    ] {
        let spec = format!("{name}^{{commit}}");
        let output = git(ctx, &["rev-parse", "--verify", "--quiet", &spec], cancel).await?;
        let oid = output.stdout_text().trim().to_owned();
        if output.status.success() && !oid.is_empty() {
            return Some(oid);
        }
    }
    None
}

/// What identifies the stage's current release: the commit its deploy probe
/// last reported, or its branch's tip where no probe is configured.
pub async fn release_marker(
    ctx: &AppContext,
    stage: VerifyStage,
    cancel: &CancellationToken,
) -> Option<String> {
    if ctx.config.environments.get(stage).is_some() {
        return crate::deploys::deployed_oid(ctx, stage, cancel).await;
    }
    stage_tip(ctx, stage, cancel).await
}

/// Whether a commit reached a stage: it is on the stage's branch and, where
/// the stage has a deploy probe, deployed. `None` is unknown (a commit this
/// clone lacks, a branch it cannot resolve, nothing recorded as deployed).
pub async fn commit_reached(
    ctx: &AppContext,
    oid: &str,
    stage: VerifyStage,
    cancel: &CancellationToken,
) -> Option<bool> {
    let tip = stage_tip(ctx, stage, cancel).await?;
    let output = git(ctx, &["merge-base", "--is-ancestor", oid, &tip], cancel).await?;
    match output.status.code() {
        Some(0) => {}
        Some(1) => return Some(false),
        _ => return None,
    }
    if ctx.config.environments.get(stage).is_none() {
        return Some(true);
    }
    wt_vcs::deployed_contains(
        &ctx.processes,
        &ctx.config.paths.main_clone,
        stage,
        oid,
        cancel,
    )
    .await
}

/// One argument of a configured command with its placeholders replaced.
/// Substitution is per argument and never re-read, so a value cannot add
/// arguments or name another placeholder.
pub fn expand(template: &str, values: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let known = after.find("}}").and_then(|end| {
            let name = after[..end].trim();
            values
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value, end))
        });
        match known {
            Some((value, end)) => {
                out.push_str(value);
                rest = &after[end + 2..];
            }
            None => {
                out.push_str("{{");
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Run a configured check for one worktree. Exit 0 holds, exit 1 does not
/// yet; anything else, a timeout included, is an error to show.
pub async fn run_check(
    ctx: &AppContext,
    check: &CheckConfig,
    key: &CheckKey,
    branch: &str,
    cancel: &CancellationToken,
) -> Result<bool, String> {
    let main_clone = ctx.config.paths.main_clone.to_string_lossy();
    let values = [
        ("arg", key.arg.as_deref().unwrap_or_default()),
        ("slug", key.slug.as_str()),
        ("branch", branch),
        ("main_clone", main_clone.as_ref()),
    ];
    let mut argv = check.run.iter().map(|arg| expand(arg, &values));
    let program = argv.next().ok_or("the check has no command")?;
    let mut spec = CommandSpec::new(program)
        .args(argv)
        .cwd(&ctx.config.paths.main_clone);
    spec.timeout = Duration::from_secs(check.timeout_seconds);
    spec.output_limit = OUTPUT_LIMIT;
    let output = ctx
        .processes
        .run(spec, cancel)
        .await
        .map_err(|error| format!("check {}: {error}", check.id))?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        code => {
            let said = output.stderr_text();
            let said = said
                .lines()
                .rev()
                .map(str::trim)
                .find(|line| !line.is_empty())
                .unwrap_or("no output");
            Err(wt_core::sanitize_terminal_text(&format!(
                "check {} {}: {said}",
                check.id,
                code.map_or_else(|| "was killed".to_owned(), |code| format!("exited {code}")),
            )))
        }
    }
}

/// The pull requests named by number, in one GitHub request. A number the
/// repository lacks has no entry; a failed read is an error for all.
pub async fn pull_requests(
    ctx: &AppContext,
    numbers: &[u64],
    cancel: &CancellationToken,
) -> Result<BTreeMap<u64, PrFacts>, wt_github::GithubError> {
    let client = GithubClient::new(
        ctx.processes.clone(),
        ctx.config.paths.main_clone.clone(),
        GithubOptions::from_config(&ctx.config, false),
    );
    let outcomes = client.fetch_pull_request_outcomes(numbers, cancel).await?;
    Ok(outcomes
        .into_iter()
        .map(|(number, outcome)| (number, pr_facts(&outcome.state, outcome.merge_commit_oid)))
        .collect())
}

/// A pull request's state as a watch reads it. A state this build does not
/// know stays unknown.
pub fn pr_facts(state: &str, merge_oid: Option<String>) -> PrFacts {
    let life = match state {
        "OPEN" => Some(PrLife::Open),
        "MERGED" => Some(PrLife::Merged),
        "CLOSED" => Some(PrLife::Closed),
        _ => None,
    };
    PrFacts {
        life,
        merge_oid: merge_oid.filter(|_| life == Some(PrLife::Merged)),
        staging: None,
        production: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_are_replaced_once_inside_one_argument() {
        let values = [("arg", "API KEY {{slug}}"), ("slug", "one")];
        assert_eq!(expand("--name={{arg}}", &values), "--name=API KEY {{slug}}");
        assert_eq!(expand("{{ slug }}/{{slug}}", &values), "one/one");
        assert_eq!(expand("{{unknown}} {{", &values), "{{unknown}} {{");
        assert_eq!(expand("plain", &values), "plain");
    }

    #[test]
    fn only_a_merged_pull_request_carries_a_merge_commit() {
        assert_eq!(
            pr_facts("MERGED", Some("abc".into())),
            PrFacts {
                life: Some(PrLife::Merged),
                merge_oid: Some("abc".into()),
                ..PrFacts::default()
            }
        );
        assert_eq!(pr_facts("CLOSED", Some("abc".into())).merge_oid, None);
        assert_eq!(pr_facts("DRAFTY", None).life, None);
    }

    #[tokio::test]
    async fn checks_and_stage_reach_are_read_from_the_main_clone() {
        let fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        let ctx = &fixture.ctx;
        let cancel = &ctx.cancellation;
        let check = CheckConfig {
            id: "flag".into(),
            run: vec![
                "test".into(),
                "-f".into(),
                "{{main_clone}}/{{slug}}-{{arg}}.flag".into(),
            ],
            ..CheckConfig::default()
        };
        let key = CheckKey {
            slug: "one".into(),
            id: "flag".into(),
            arg: Some("ship it".into()),
        };
        assert_eq!(
            run_check(ctx, &check, &key, "feature/one", cancel).await,
            Ok(false)
        );
        std::fs::write(ctx.config.paths.main_clone.join("one-ship it.flag"), "").unwrap();
        assert_eq!(
            run_check(ctx, &check, &key, "feature/one", cancel).await,
            Ok(true)
        );
        let broken = CheckConfig {
            run: vec![
                "sh".into(),
                "-c".into(),
                "echo vault sealed >&2; exit 3".into(),
            ],
            ..check.clone()
        };
        assert_eq!(
            run_check(ctx, &broken, &key, "feature/one", cancel).await,
            Err("check flag exited 3: vault sealed".into())
        );
        let missing = CheckConfig {
            run: vec!["wt-no-such-program".into()],
            ..check
        };
        assert!(run_check(ctx, &missing, &key, "", cancel).await.is_err());

        // No origin and no probe: the local base branch is the stage.
        let tip = release_marker(ctx, VerifyStage::Staging, cancel)
            .await
            .unwrap();
        assert_eq!(
            release_marker(ctx, VerifyStage::Production, cancel).await,
            Some(tip.clone())
        );
        assert_eq!(
            commit_reached(ctx, &tip, VerifyStage::Production, cancel).await,
            Some(true)
        );
        let one = ctx.config.paths.worktree_root.join("one");
        std::fs::write(one.join("new.txt"), "x").unwrap();
        for args in [
            &["add", "new.txt"][..],
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@example.invalid",
                "commit",
                "-m",
                "work",
            ],
        ] {
            let spec = CommandSpec::new("git").args(args.iter().copied()).cwd(&one);
            ctx.processes
                .run(spec, cancel)
                .await
                .unwrap()
                .checked("git")
                .unwrap();
        }
        let spec = CommandSpec::new("git")
            .args(["rev-parse", "HEAD"])
            .cwd(&one);
        let head = ctx
            .processes
            .run(spec, cancel)
            .await
            .unwrap()
            .stdout_text()
            .trim()
            .to_owned();
        assert_eq!(
            commit_reached(ctx, &head, VerifyStage::Staging, cancel).await,
            Some(false)
        );
        assert_eq!(
            commit_reached(ctx, &"f".repeat(40), VerifyStage::Staging, cancel).await,
            None
        );
        fixture.close().await.unwrap();
    }
}
