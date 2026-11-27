//! Finds each worktree's other branches (see `wt_core::other_branches`),
//! records them, and records what GitHub says of the pull request on each.
//! The record is what every surface shows and what removal checks under its
//! lock, so a sighting made here outlives the reflog and the worktree.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Duration;

use serde_json::Value;
use wt_app_core::context::AppContext;
use wt_core::{OtherBranch, OtherPrState, OtherPrVerdict};
use wt_github::{GithubClient, GithubData, GithubOptions};
use wt_vcs::WorktreeRecord;

use crate::commands::resolve::run_git;

/// How long an explicit removal waits for GitHub to answer for the other
/// branches before leaving them unknown.
const GITHUB_WAIT: Duration = Duration::from_secs(8);

/// Whether the caller may spend a GitHub read on branches the data it holds
/// does not cover.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ask {
    /// A person or a command asked for this removal or listing.
    Github,
    /// A timer or a board refresh: use what is at hand.
    Nothing,
}

fn now_ms() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(i64::MAX)
}

fn answer(branch: &str, github: &GithubData, now: i64) -> Option<OtherBranch> {
    if !github.covered_branches.contains(branch) {
        return None;
    }
    let pr = github.prs.get(branch);
    Some(OtherBranch {
        branch: branch.to_owned(),
        number: pr.map(|pr| pr.number),
        state: pr.map_or(OtherPrState::None, |pr| OtherPrState::of_github(&pr.state)),
        draft: pr.is_some_and(|pr| pr.is_draft),
        checked_at_ms: Some(now),
    })
}

async fn ref_exists(ctx: &AppContext, path: &Path, reference: &str) -> bool {
    run_git(
        ctx,
        path,
        [
            "rev-parse",
            "--verify",
            "--quiet",
            "--end-of-options",
            &format!("{reference}^{{commit}}"),
        ],
    )
    .await
    .is_ok_and(|output| output.status.success())
}

/// The other branches of each of `rows`, by slug, after recording every new
/// sighting and every answer `github` holds. A branch `github` does not
/// cover keeps its recorded answer, or reads unknown: never "no PR". Rows
/// with none are left out.
pub async fn observe(
    ctx: &AppContext,
    rows: &[WorktreeRecord],
    state: &Value,
    github: Option<&GithubData>,
    ask: Ask,
) -> BTreeMap<String, Vec<OtherBranch>> {
    let Ok(lifecycle) = crate::lifecycle_ops::service(ctx) else {
        return BTreeMap::new();
    };
    let now = now_ms();
    let mut found: Vec<(&WorktreeRecord, Vec<OtherBranch>)> = Vec::new();
    for row in rows.iter().filter(|row| !row.is_main) {
        let slug_state = &state["slugs"][row.target.slug()];
        let Ok((names, _)) = lifecycle
            .other_branch_names(row, Some(slug_state), &ctx.cancellation)
            .await
        else {
            continue;
        };
        let unrecorded = wt_core::recorded_branch(slug_state).is_none()
            && !row.detached
            && !row.target.branch.is_empty();
        if names.is_empty() && !unrecorded {
            continue;
        }
        let stored = wt_core::stored_other_branches(slug_state);
        let path = Path::new(&row.target.path);
        let mut others = Vec::new();
        for branch in names {
            if let Some(answered) = github.and_then(|github| answer(&branch, github, now)) {
                others.push(answered);
                continue;
            }
            let stored = stored.iter().find(|other| other.branch == branch);
            // GitHub's own answer stands until it is asked again. "None"
            // without one was read off a branch origin did not have, which
            // a push changes.
            let settled = stored.is_some_and(|other| {
                other.number.is_some()
                    || (other.state == OtherPrState::None && other.checked_at_ms.is_some())
            });
            if settled {
                others.extend(stored.cloned());
                continue;
            }
            match whereabouts(ctx, path, row.target.slug(), &branch).await {
                Whereabouts::Origin => others.push(OtherBranch::unknown(branch)),
                // A branch origin never had cannot be a pull request's head.
                Whereabouts::LocalOnly => others.push(OtherBranch {
                    state: OtherPrState::None,
                    ..OtherBranch::unknown(branch)
                }),
                // A name the reflog kept of a branch that exists nowhere
                // any more holds nothing and is not recorded.
                Whereabouts::Nowhere => others.extend(stored.cloned()),
            }
        }
        found.push((row, others));
    }
    if ask == Ask::Github && std::env::var("WT_GITHUB").as_deref() != Ok("off") {
        let unanswered: BTreeSet<String> = found
            .iter()
            .flat_map(|(_, others)| others)
            .filter(|other| other.verdict(now) == OtherPrVerdict::Unknown)
            .map(|other| other.branch.clone())
            .collect();
        if !unanswered.is_empty() {
            let client = GithubClient::new(
                ctx.processes.clone(),
                ctx.config.paths.main_clone.clone(),
                GithubOptions::from_config(&ctx.config, false),
            );
            let branches: Vec<String> = unanswered.into_iter().collect();
            if let Ok(Ok(fresh)) = tokio::time::timeout(
                GITHUB_WAIT,
                client.fetch_worktrees(&branches, &ctx.cancellation),
            )
            .await
            {
                for other in found.iter_mut().flat_map(|(_, others)| others) {
                    if let Some(answered) = answer(&other.branch, &fresh, now) {
                        *other = answered;
                    }
                }
            }
        }
    }
    let mut result = BTreeMap::new();
    for (row, others) in found {
        let slug = row.target.slug().to_owned();
        let recorded = (!row.detached).then(|| row.target.branch.clone());
        let write = (slug.clone(), others.clone());
        let written = ctx
            .database
            .call(move |store| {
                Ok(store.record_other_branches(&write.0, recorded.as_deref(), &write.1)?)
            })
            .await;
        if let Err(error) = written {
            tracing::warn!(%slug, %error, "could not record a worktree's other branches");
        }
        if !others.is_empty() {
            result.insert(slug, others);
        }
    }
    result
}

/// [`observe`], then `state` brought up to date with what it recorded, for a
/// command that prints from the state it already read.
pub async fn refresh(
    ctx: &AppContext,
    rows: &[WorktreeRecord],
    state: &mut Value,
    github: Option<&GithubData>,
    ask: Ask,
) {
    for (slug, others) in observe(ctx, rows, state, github, ask).await {
        let entries: serde_json::Map<String, Value> = others
            .iter()
            .filter_map(|other| Some((other.branch.clone(), serde_json::to_value(other).ok()?)))
            .collect();
        if let Some(slugs) = state.get_mut("slugs").and_then(Value::as_object_mut) {
            let entry = slugs
                .entry(slug)
                .or_insert_with(|| Value::Object(serde_json::Map::new()));
            if let Some(entry) = entry.as_object_mut() {
                entry.insert(wt_core::OTHER_BRANCHES.to_owned(), Value::Object(entries));
            }
        }
    }
}

enum Whereabouts {
    Origin,
    LocalOnly,
    Nowhere,
}

/// Where `branch` still exists. "Nowhere" is remembered for a while: reflogs
/// keep the names of long-deleted branches, and every pass would ask Git
/// about each again.
async fn whereabouts(ctx: &AppContext, path: &Path, slug: &str, branch: &str) -> Whereabouts {
    const REMEMBER: Duration = Duration::from_secs(10 * 60);
    static GONE: std::sync::Mutex<BTreeMap<(String, String), std::time::Instant>> =
        std::sync::Mutex::new(BTreeMap::new());
    let key = (slug.to_owned(), branch.to_owned());
    if GONE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&key)
        .is_some_and(|at| at.elapsed() < REMEMBER)
    {
        return Whereabouts::Nowhere;
    }
    let found = if ref_exists(ctx, path, &format!("refs/remotes/origin/{branch}")).await {
        Whereabouts::Origin
    } else if ref_exists(ctx, path, &format!("refs/heads/{branch}")).await {
        Whereabouts::LocalOnly
    } else {
        Whereabouts::Nowhere
    };
    let mut gone = GONE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if matches!(found, Whereabouts::Nowhere) {
        gone.insert(key, std::time::Instant::now());
    } else {
        gone.remove(&key);
    }
    found
}

/// Tell the session of a worktree whose checkout left its recorded branch,
/// once per branch it lands on, and only a session that is already running.
pub async fn notify_off_branch(ctx: &AppContext, row: &WorktreeRecord, slug_state: &Value) {
    let current = row.target.branch.as_str();
    let Some(recorded) = wt_core::recorded_branch(slug_state) else {
        return;
    };
    if row.is_main
        || row.detached
        || current.is_empty()
        || current == recorded
        || slug_state[wt_core::OFF_BRANCH_NOTICE].as_str() == Some(current)
    {
        return;
    }
    let slug = row.target.slug().to_owned();
    let app = wt_app_core::harness::AppHarness::new(ctx);
    let Ok(routes) = app.routes(ctx).await else {
        return;
    };
    let Some(route) = routes.iter().find(|route| {
        route.target.slug == slug
            && !route.target.remote
            && route
                .choice
                .live
                .as_ref()
                .is_some_and(|live| !live.is_empty())
    }) else {
        return;
    };
    let claim = (slug.clone(), current.to_owned());
    let claimed = ctx
        .database
        .call(move |store| Ok(store.claim_off_branch_notice(&claim.0, &claim.1)?))
        .await
        .unwrap_or(false);
    if !claimed {
        return;
    }
    let text = wt_core::off_branch_notice(&slug, recorded, current);
    if let Err(error) = app.send(route, &text, Some("wt"), ctx).await {
        tracing::warn!(%slug, %error, "could not tell a session it left its worktree's branch");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wt_app_core::test_support::CommandFixture;

    async fn git(ctx: &AppContext, args: &[&str]) -> String {
        let output = run_git(ctx, &ctx.cwd, args.iter().copied()).await.unwrap();
        assert!(output.status.success(), "git {args:?}");
        output.stdout_text().trim().to_owned()
    }

    async fn state(ctx: &AppContext) -> Value {
        ctx.database
            .call(|store| Ok(store.read_wt_state()?))
            .await
            .unwrap()
    }

    async fn row_one(ctx: &AppContext) -> WorktreeRecord {
        ctx.repository
            .inventory(&ctx.cancellation)
            .await
            .unwrap()
            .into_iter()
            .find(|row| row.target.slug() == "one")
            .unwrap()
    }

    fn open_pr(branch: &str, number: u64) -> GithubData {
        let mut data = GithubData::default();
        data.prs.insert(
            branch.to_owned(),
            serde_json::from_value(serde_json::json!({
                "number": number, "url": "https://example.invalid/pull/1",
                "headRefName": branch, "headRefOid": "a".repeat(40),
                "baseRefName": "main", "title": "Retire X", "isDraft": true, "state": "OPEN",
                "checks": "pass", "failedChecks": [], "review": "none",
                "reviewRequests": 0, "requestedReviewers": [], "suggestedReviewers": [],
                "comments": [], "unresolvedThreads": 0, "unresolvedThreadsTotal": 0
            }))
            .unwrap(),
        );
        data.covered_branches.insert(branch.to_owned());
        data
    }

    /// An agent opens a second branch inside a worktree, pushes it and goes
    /// back. The branch is found in the reflog, recorded, held against
    /// removal while its pull request is open or unknown, and stays
    /// recorded once the reflog is gone.
    #[tokio::test]
    async fn a_second_branch_in_a_worktree_is_recorded_and_holds_the_worktree() {
        let fixture = CommandFixture::new().await.unwrap();
        let ctx = &fixture.ctx;
        let origin = fixture._root.path().join("origin.git");
        git(ctx, &["init", "--bare", origin.to_str().unwrap()]).await;
        git(ctx, &["remote", "add", "origin", origin.to_str().unwrap()]).await;
        let own = row_one(ctx).await.target.branch;
        git(ctx, &["push", "-q", "origin", &own]).await;

        // A branch that was only ever local cannot be a pull request's head.
        git(ctx, &["switch", "-q", "-c", "retire-x"]).await;
        git(ctx, &["switch", "-q", &own]).await;
        let rows = [row_one(ctx).await];
        let seen = observe(ctx, &rows, &state(ctx).await, None, Ask::Nothing).await;
        assert_eq!(seen["one"].len(), 1);
        assert_eq!(seen["one"][0].branch, "retire-x");
        assert_eq!(seen["one"][0].state, OtherPrState::None);
        let recorded = state(ctx).await;
        assert_eq!(recorded["slugs"]["one"]["branch"], own.as_str());
        let hazards = || async {
            crate::lifecycle_ops::plan_with_facts(
                ctx,
                vec![row_one(ctx).await],
                &state(ctx).await,
                None,
                None,
            )
            .await
            .unwrap()
            .rows
            .remove(0)
            .hazards
        };
        assert!(hazards().await.is_empty(), "{:?}", hazards().await);

        // Pushed, it may carry one: unknown until GitHub answers.
        git(ctx, &["push", "-q", "origin", "retire-x"]).await;
        assert_eq!(hazards().await, [wt_core::unknown_pr_hazard("retire-x")]);
        // The row's own data does not cover the branch, so it stays unknown.
        let own_only = {
            let mut data = GithubData::default();
            data.covered_branches.insert(own.clone());
            data
        };
        let seen = observe(ctx, &rows, &state(ctx).await, Some(&own_only), Ask::Nothing).await;
        assert_eq!(seen["one"][0].state, OtherPrState::Unknown);

        let github = open_pr("retire-x", 3961);
        let mut current = state(ctx).await;
        refresh(ctx, &rows, &mut current, Some(&github), Ask::Nothing).await;
        assert_eq!(
            wt_core::other_prs_json(&current["slugs"]["one"]),
            serde_json::json!([
                {"number": 3961, "branch": "retire-x", "state": "open", "draft": true}
            ])
        );
        assert_eq!(
            hazards().await,
            ["holds open PR #3961 on retire-x: give it its own worktree with wt new retire-x"]
        );

        // The record outlives the reflog, and a later read that does not
        // cover the branch does not turn the open pull request into none.
        let git_dir = row_one(ctx).await.git_dir.unwrap();
        std::fs::remove_file(git_dir.join("logs/HEAD")).unwrap();
        let seen = observe(ctx, &rows, &state(ctx).await, Some(&own_only), Ask::Nothing).await;
        assert_eq!(seen["one"][0].state, OtherPrState::Open);
        assert_eq!(hazards().await.len(), 1);

        // wt next leaves a landed branch behind on purpose: the worktree is
        // for the new one, and the old one is not an other branch.
        ctx.database
            .call(|store| Ok(store.set_slug_branch("one", "retire-x")?))
            .await
            .unwrap();
        let next = state(ctx).await;
        assert_eq!(
            next["slugs"]["one"]["leftBranches"],
            serde_json::json!([own])
        );
        assert!(wt_core::stored_other_branches(&next["slugs"]["one"]).is_empty());
        fixture.close().await.unwrap();
    }
}
