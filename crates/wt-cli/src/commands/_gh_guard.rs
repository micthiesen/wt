//! `wt _gh-guard`: what the `gh` shim asks before `gh pr create`. One pull
//! request per worktree: the pull request comes from the worktree's recorded
//! branch, and that branch does not already have one. A pull request for a
//! repository that is none of the worktree's remotes is not the worktree's
//! and is not checked. Only an explicit
//! refusal exits [`wt_harness::GH_GUARD_REFUSED`]; whatever cannot be
//! established lets `gh` run.

use std::{path::Path, time::Duration};

use anyhow::Result;
use serde::Deserialize;
use wt_app_core::context::AppContext;
use wt_ops::commands::resolve::resolve_worktree;
use wt_platform::process::{CommandSpec, ProcessRunner};

/// `gh pr create` waits on this lookup; past it the guard lets gh run.
const GH_WAIT: Duration = Duration::from_secs(3);

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HeadPr {
    number: u64,
    state: String,
    #[serde(default)]
    is_cross_repository: bool,
}

/// The `--head` branch of a `gh pr create` command line, without an owner.
fn head_flag(args: &[String]) -> Option<String> {
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let value = if arg == "--head" || arg == "-H" {
            args.next().cloned()
        } else {
            arg.strip_prefix("--head=")
                .or_else(|| arg.strip_prefix("-H").filter(|rest| !rest.is_empty()))
                .map(str::to_owned)
        };
        if let Some(value) = value {
            return Some(value.rsplit(':').next().unwrap_or(&value).to_owned());
        }
    }
    None
}

/// The repository a `gh pr create` command line names with `--repo` or `-R`.
fn repo_flag(args: &[String]) -> Option<String> {
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let value = if arg == "--repo" || arg == "-R" {
            args.next().cloned()
        } else {
            arg.strip_prefix("--repo=")
                .or_else(|| arg.strip_prefix("-R").filter(|rest| !rest.is_empty()))
                .map(str::to_owned)
        };
        if value.is_some() {
            return value;
        }
    }
    None
}

/// `owner/name` of a repository however it is written: `OWNER/REPO`,
/// `HOST/OWNER/REPO`, or an HTTPS or SSH URL, with or without `.git`.
fn repo_key(repository: &str) -> Option<String> {
    let trimmed = repository.trim().trim_end_matches('/');
    let trimmed = trimmed.strip_suffix(".git").unwrap_or(trimmed);
    let mut parts = trimmed.rsplit(['/', ':']);
    let (name, owner) = (parts.next()?, parts.next()?);
    (!name.is_empty() && !owner.is_empty()).then(|| format!("{owner}/{name}").to_ascii_lowercase())
}

/// Whether `named` is a repository other than every one of `remotes`.
/// `None` when either side cannot be read, which decides nothing.
fn names_another_repository(named: &str, remotes: &[String]) -> Option<bool> {
    let named = repo_key(named)?;
    let own: Vec<String> = remotes.iter().filter_map(|url| repo_key(url)).collect();
    (!own.is_empty()).then(|| !own.contains(&named))
}

/// The URLs of the remotes of the checkout at `cwd`.
async fn remote_urls(
    processes: &ProcessRunner,
    cancellation: &tokio_util::sync::CancellationToken,
    cwd: &Path,
) -> Vec<String> {
    let spec = CommandSpec::new("git")
        .args(["config", "--get-regexp", r"^remote\..*\.url$"])
        .cwd(cwd);
    processes
        .run(spec, cancellation)
        .await
        .ok()
        .filter(|output| output.status.success())
        .map(|output| {
            output
                .stdout_text()
                .lines()
                .filter_map(|line| line.split_whitespace().nth(1).map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// The pull requests of `branch`, or `None` when GitHub did not say within
/// `wait`.
async fn pull_requests_of(
    processes: &ProcessRunner,
    cancellation: &tokio_util::sync::CancellationToken,
    gh: &str,
    branch: &str,
    cwd: &Path,
    wait: Duration,
) -> Option<Vec<HeadPr>> {
    let mut spec = CommandSpec::new(gh)
        .args([
            "pr",
            "list",
            "--state",
            "all",
            "--head",
            branch,
            "--json",
            "number,state,isCrossRepository",
        ])
        .cwd(cwd);
    spec.timeout = wait;
    processes
        .run(spec, cancellation)
        .await
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| serde_json::from_slice(&output.stdout).ok())
}

/// Why this pull request would be the worktree's second, if it would.
fn second_pr(recorded: &str, head: &str, slug: &str, prs: &[HeadPr]) -> Option<String> {
    if head != recorded {
        return Some(format!(
            "this worktree ({slug}) is for {recorded}, and this PR would come from {head}"
        ));
    }
    prs.iter()
        .filter(|pr| !pr.is_cross_repository)
        .find(|pr| pr.state == "OPEN" || pr.state == "MERGED")
        .map(|pr| {
            format!(
                "{recorded} already has PR #{} ({})",
                pr.number,
                pr.state.to_ascii_lowercase()
            )
        })
}

/// What the guard decided about one `gh pr create` in a worktree it knows.
#[derive(Debug, PartialEq)]
enum Verdict {
    Allowed,
    /// gh runs although the check could not be made.
    Unchecked(String),
    Refused(String),
}

/// One application log line for each refusal and each unchecked run, so
/// both can be reviewed after the session that met them is gone.
fn log_verdict(slug: &str, branch: &str, verdict: &Verdict) {
    match verdict {
        Verdict::Allowed => {}
        Verdict::Unchecked(reason) => {
            tracing::warn!(slug, branch, reason, "gh pr create ran unchecked");
        }
        Verdict::Refused(reason) => {
            tracing::warn!(slug, branch, reason, "gh pr create refused");
        }
    }
}

pub async fn run(ctx: &AppContext, args: &[String]) -> Result<i32> {
    let Ok(row) = resolve_worktree(ctx, None).await else {
        return Ok(0);
    };
    if row.is_main || row.detached || row.target.branch.is_empty() {
        return Ok(0);
    }
    let slug = row.target.slug().to_owned();
    let named = repo_flag(args).or_else(|| {
        std::env::var("GH_REPO")
            .ok()
            .filter(|repo| !repo.is_empty())
    });
    if let Some(named) = named {
        let remotes = remote_urls(
            &ctx.processes,
            &ctx.cancellation,
            Path::new(&row.target.path),
        )
        .await;
        match names_another_repository(&named, &remotes) {
            Some(true) => return Ok(0),
            Some(false) => {}
            None => {
                let reason =
                    format!("could not tell whether {named} is this worktree's repository");
                log_verdict(&slug, &row.target.branch, &Verdict::Unchecked(reason));
                return Ok(0);
            }
        }
    }
    let lookup = slug.clone();
    let state = match ctx
        .database
        .call(move |store| Ok(store.read_slug_state(&lookup)?))
        .await
    {
        Ok(state) => state.unwrap_or_default(),
        Err(error) => {
            let reason = format!("could not read the worktree's recorded branch: {error}");
            log_verdict(&slug, &row.target.branch, &Verdict::Unchecked(reason));
            return Ok(0);
        }
    };
    let recorded = wt_core::recorded_branch(&state)
        .unwrap_or(&row.target.branch)
        .to_owned();
    let head = head_flag(args).unwrap_or_else(|| row.target.branch.clone());
    let prs = if head == recorded {
        let gh = std::env::var("WT_REAL_GH").unwrap_or_else(|_| "gh".into());
        let listed = pull_requests_of(
            &ctx.processes,
            &ctx.cancellation,
            &gh,
            &recorded,
            Path::new(&row.target.path),
            GH_WAIT,
        )
        .await;
        match listed {
            Some(prs) => prs,
            None => {
                eprintln!(
                    "wt: could not list the pull requests of {recorded}; not checking for a second one"
                );
                let reason = format!("GitHub did not list the pull requests of {recorded}");
                log_verdict(&slug, &head, &Verdict::Unchecked(reason));
                return Ok(0);
            }
        }
    } else {
        Vec::new()
    };
    let verdict =
        second_pr(&recorded, &head, &slug, &prs).map_or(Verdict::Allowed, Verdict::Refused);
    log_verdict(&slug, &head, &verdict);
    match verdict {
        Verdict::Refused(reason) => {
            eprintln!("{}", wt_core::second_pr_refusal(&reason, &recorded));
            Ok(wt_harness::GH_GUARD_REFUSED)
        }
        Verdict::Allowed | Verdict::Unchecked(_) => Ok(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|arg| (*arg).to_owned()).collect()
    }

    fn pr(number: u64, state: &str) -> HeadPr {
        HeadPr {
            number,
            state: state.into(),
            is_cross_repository: false,
        }
    }

    #[test]
    fn the_head_flag_is_read_in_every_spelling() {
        assert_eq!(head_flag(&args(&["pr", "create", "--draft"])), None);
        for spelling in [
            &["pr", "create", "--head", "michael/x"][..],
            &["pr", "create", "-H", "michael/x"],
            &["pr", "create", "--head=michael/x"],
            &["pr", "create", "-Hmichael/x"],
            &["pr", "create", "--head", "owner:michael/x"],
        ] {
            assert_eq!(head_flag(&args(spelling)).as_deref(), Some("michael/x"));
        }
    }

    #[test]
    fn refusals_and_unchecked_runs_each_leave_one_log_line() {
        use std::sync::{Arc, Mutex};
        #[derive(Clone, Default)]
        struct Lines(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Lines {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let lines = Lines::default();
        let writer = lines.clone();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            log_verdict("one", "michael/one", &Verdict::Allowed);
            log_verdict(
                "one",
                "michael/retire",
                &Verdict::Refused("this PR would come from michael/retire".into()),
            );
            log_verdict(
                "two",
                "michael/two",
                &Verdict::Unchecked("GitHub did not list the pull requests".into()),
            );
        });
        let logged = String::from_utf8(lines.0.lock().unwrap().clone()).unwrap();
        let logged: Vec<serde_json::Value> = logged
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(logged.len(), 2, "{logged:?}");
        let fields = |line: &serde_json::Value| {
            ["message", "slug", "branch", "reason"].map(|key| line["fields"][key].clone())
        };
        assert_eq!(
            fields(&logged[0]),
            [
                "gh pr create refused",
                "one",
                "michael/retire",
                "this PR would come from michael/retire"
            ]
        );
        assert_eq!(
            fields(&logged[1]),
            [
                "gh pr create ran unchecked",
                "two",
                "michael/two",
                "GitHub did not list the pull requests"
            ]
        );
    }

    #[test]
    fn only_a_repository_that_is_none_of_the_remotes_is_another_one() {
        for spelling in [
            &["pr", "create", "--repo", "o/r"][..],
            &["pr", "create", "-R", "o/r"],
            &["pr", "create", "--repo=o/r", "--draft"],
            &["-Ro/r", "pr", "create"],
        ] {
            assert_eq!(repo_flag(&args(spelling)).as_deref(), Some("o/r"));
        }
        assert_eq!(repo_flag(&args(&["pr", "create", "--draft"])), None);

        let remotes = [
            "git@github.com:Acme/App.git".to_owned(),
            "https://github.com/upstream/app".to_owned(),
        ];
        // The session's own repository, however it is spelled, is guarded.
        for own in [
            "acme/app",
            "github.com/acme/app",
            "https://github.com/Acme/App.git",
            "https://github.com/acme/app/",
            "upstream/app",
        ] {
            assert_eq!(
                names_another_repository(own, &remotes),
                Some(false),
                "{own}"
            );
        }
        assert_eq!(names_another_repository("acme/other", &remotes), Some(true));
        // Nothing to compare decides nothing.
        assert_eq!(names_another_repository("acme/app", &[]), None);
        assert_eq!(names_another_repository("app", &remotes), None);
    }

    #[tokio::test]
    async fn the_remotes_of_a_checkout_are_read_from_its_git_config() {
        let tmp = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(tmp.path())
                .output()
                .unwrap();
            assert!(status.status.success(), "{args:?}");
        };
        git(&["init", "-q"]);
        let processes = ProcessRunner::default();
        let token = tokio_util::sync::CancellationToken::new();
        assert!(remote_urls(&processes, &token, tmp.path()).await.is_empty());
        git(&["remote", "add", "origin", "git@github.com:acme/app.git"]);
        git(&[
            "remote",
            "add",
            "upstream",
            "https://github.com/upstream/app",
        ]);
        let mut urls = remote_urls(&processes, &token, tmp.path()).await;
        urls.sort();
        assert_eq!(
            urls,
            [
                "git@github.com:acme/app.git",
                "https://github.com/upstream/app"
            ]
        );
    }

    #[tokio::test]
    async fn a_lookup_github_does_not_answer_in_time_is_given_up() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let fake = |name: &str, body: &str| {
            let path = tmp.path().join(name);
            std::fs::write(&path, body).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path.to_string_lossy().into_owned()
        };
        let answer = r#"echo '[{"number":4,"state":"OPEN","isCrossRepository":false}]'"#;
        let prompt = fake("gh", &format!("#!/bin/sh\n{answer}\n"));
        let slow = fake("slow-gh", &format!("#!/bin/sh\nsleep 20\n{answer}\n"));
        let processes = ProcessRunner::default();
        let token = tokio_util::sync::CancellationToken::new();
        let wait = Duration::from_secs(5);
        let listed = pull_requests_of(&processes, &token, &prompt, "x", tmp.path(), wait).await;
        assert_eq!(listed.unwrap()[0].number, 4);
        let started = std::time::Instant::now();
        let wait = Duration::from_millis(300);
        let listed = pull_requests_of(&processes, &token, &slow, "x", tmp.path(), wait).await;
        assert!(listed.is_none());
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[test]
    fn a_worktree_gets_one_pull_request_from_its_own_branch() {
        // The first pull request of the recorded branch is the worktree's.
        assert_eq!(second_pr("michael/one", "michael/one", "one", &[]), None);
        assert_eq!(
            second_pr("michael/one", "michael/one", "one", &[pr(3, "CLOSED")]),
            None
        );
        // Another branch, or a branch that already has its pull request.
        let other = second_pr("michael/one", "michael/retire", "one", &[]).unwrap();
        assert!(other.contains("is for michael/one") && other.contains("from michael/retire"));
        assert_eq!(
            second_pr(
                "michael/one",
                "michael/one",
                "one",
                &[pr(3, "CLOSED"), pr(4, "MERGED")]
            )
            .unwrap(),
            "michael/one already has PR #4 (merged)"
        );
        assert!(
            second_pr("michael/one", "michael/one", "one", &[pr(5, "OPEN")])
                .unwrap()
                .ends_with("(open)")
        );
        let refusal = wt_core::second_pr_refusal(&other, "michael/one");
        assert_eq!(refusal.lines().count(), 2);
        assert!(
            refusal.contains("wt next") && refusal.contains("wt new <slug> --base michael/one")
        );
    }
}
