//! `wt _gh-guard`: what the `gh` shim asks before `gh pr create`. One pull
//! request per worktree: the pull request comes from the worktree's recorded
//! branch, and that branch does not already have one. Only an explicit
//! refusal exits [`wt_harness::GH_GUARD_REFUSED`]; whatever cannot be
//! established lets `gh` run.

use std::time::Duration;

use anyhow::Result;
use serde::Deserialize;
use wt_app_core::context::AppContext;
use wt_ops::commands::resolve::resolve_worktree;
use wt_platform::process::CommandSpec;

const GH_WAIT: Duration = Duration::from_secs(8);

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

pub async fn run(ctx: &AppContext, args: &[String]) -> Result<i32> {
    let Ok(row) = resolve_worktree(ctx, None).await else {
        return Ok(0);
    };
    if row.is_main || row.detached || row.target.branch.is_empty() {
        return Ok(0);
    }
    let slug = row.target.slug().to_owned();
    let lookup = slug.clone();
    let state = ctx
        .database
        .call(move |store| Ok(store.read_slug_state(&lookup)?))
        .await?
        .unwrap_or_default();
    let recorded = wt_core::recorded_branch(&state)
        .unwrap_or(&row.target.branch)
        .to_owned();
    let head = head_flag(args).unwrap_or_else(|| row.target.branch.clone());
    let prs = if head == recorded {
        let gh = std::env::var("WT_REAL_GH").unwrap_or_else(|_| "gh".into());
        let mut spec = CommandSpec::new(gh)
            .args([
                "pr",
                "list",
                "--state",
                "all",
                "--head",
                recorded.as_str(),
                "--json",
                "number,state,isCrossRepository",
            ])
            .cwd(&row.target.path);
        spec.timeout = GH_WAIT;
        let listed = ctx
            .processes
            .run(spec, &ctx.cancellation)
            .await
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| serde_json::from_slice::<Vec<HeadPr>>(&output.stdout).ok());
        match listed {
            Some(prs) => prs,
            None => {
                eprintln!(
                    "wt: could not list the pull requests of {recorded}; not checking for a second one"
                );
                return Ok(0);
            }
        }
    } else {
        Vec::new()
    };
    match second_pr(&recorded, &head, &slug, &prs) {
        Some(reason) => {
            eprintln!("{}", wt_core::second_pr_refusal(&reason, &recorded));
            Ok(wt_harness::GH_GUARD_REFUSED)
        }
        None => Ok(0),
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
