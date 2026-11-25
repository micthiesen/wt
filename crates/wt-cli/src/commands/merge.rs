use anyhow::{Result, bail};
use clap::Args;
use wt_app_core::context::AppContext;
use wt_github::{GithubClient, GithubOptions, PrMergeTarget};
use wt_ops::commands::resolve::resolve_worktree;

#[derive(Debug, Clone, Args, Default)]
pub struct MergeArgs {
    /// Worktree slug or branch; defaults to WT_AGENT, then the current checkout.
    pub slug: Option<String>,
    /// Cancel the actual merge-queue entry or classic auto-merge request.
    #[arg(long)]
    pub cancel: bool,
}

pub async fn run(context: &AppContext, args: &MergeArgs) -> Result<i32> {
    let client = GithubClient::new(
        context.processes.clone(),
        context.config.paths.main_clone.clone(),
        GithubOptions::from_config(&context.config, false),
    );
    run_with_client(context, args, &client).await
}

async fn run_with_client(
    context: &AppContext,
    args: &MergeArgs,
    client: &GithubClient,
) -> Result<i32> {
    let worktree = if args.slug.is_some() {
        resolve_worktree(context, args.slug.as_deref()).await?
    } else if let Ok(agent) = std::env::var("WT_AGENT") {
        match resolve_worktree(context, Some(&agent)).await {
            Ok(record) => record,
            Err(_) => resolve_worktree(context, None).await?,
        }
    } else {
        resolve_worktree(context, None).await?
    };
    if worktree.is_main {
        bail!("no worktree given, and this directory isn't in one");
    }
    let Some(pr) = client
        .view_pr(&worktree.target.branch, &context.cancellation)
        .await?
    else {
        bail!("no PR for {}", worktree.target.branch);
    };
    if pr.state != "OPEN" {
        bail!("#{} is {}", pr.number, pr.state.to_lowercase());
    }
    if !args.cancel && pr.is_draft {
        bail!(
            "#{} is a draft; mark it ready first: gh pr ready {}",
            pr.number,
            pr.number
        );
    }
    let landing = if args.cancel {
        // Cancelling merges nothing; the stack check applies to arming only.
        wt_github::MergeLanding::default()
    } else {
        wt_ops::fork_base::merge_landing(context, &worktree.target.branch).await?
    };
    let target = PrMergeTarget {
        id: pr.id,
        number: pr.number,
        base_ref_name: pr.base_ref_name,
        head_ref_oid: pr.head_ref_oid,
        landing,
    };
    let result = if args.cancel {
        client
            .disable_auto_merge(&target, &context.cancellation)
            .await
    } else {
        client
            .enable_auto_merge(&target, &context.cancellation)
            .await
    };
    // The same record the TUI's arm and cancel keep, so a merge GitHub
    // later drops is noticed whichever way it was armed.
    let slug = worktree.target.slug();
    if result.ok {
        if args.cancel {
            wt_ops::merge_arm::cancelled(&context.database, slug).await;
            println!("cancelled merge-when-ready on #{}", target.number);
        } else {
            wt_ops::merge_arm::armed(&context.database, slug, &target, &result).await;
            println!(
                "merge when ready armed on #{} → {}\n  GitHub merges it once its requirements are met.",
                target.number, target.base_ref_name
            );
        }
        Ok(0)
    } else {
        eprintln!(
            "{}",
            result.error.as_deref().unwrap_or("GitHub mutation failed")
        );
        if result.retryable == Some(true) && !args.cancel {
            eprintln!("  temporary refusal; retry once the check reports.");
            Ok(75)
        } else {
            Ok(1)
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt};
    use wt_app_core::test_support::CommandFixture;

    #[tokio::test]
    async fn draft_refusal_never_attempts_a_github_write() {
        let fixture = CommandFixture::new().await.unwrap();
        let fake = fixture._root.path().join("fake-gh");
        let trace = fixture._root.path().join("calls");
        fs::write(
            &fake,
            format!(
                r#"#!/usr/bin/env python3
import json, sys
with open({trace:?}, 'a') as f: f.write(json.dumps(sys.argv[1:]) + '\n')
print(json.dumps({{'number': 42, 'baseRefName': 'main', 'state': 'OPEN', 'isDraft': True,
                  'title': 'Draft', 'id': 'PR_fake', 'headRefOid': 'abc'}}))
"#,
                trace = trace.to_string_lossy()
            ),
        )
        .unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
        let client = GithubClient::new(
            fixture.ctx.processes.clone(),
            fixture.ctx.config.paths.main_clone.clone(),
            GithubOptions::from_config(&fixture.ctx.config, false),
        )
        .with_gh_program(fake);
        let result = run_with_client(
            &fixture.ctx,
            &MergeArgs {
                slug: Some("one".into()),
                cancel: false,
            },
            &client,
        )
        .await;
        assert!(result.unwrap_err().to_string().contains("is a draft"));
        let calls = fs::read_to_string(trace).unwrap();
        assert_eq!(calls.lines().count(), 1);
        assert!(calls.contains("feature/one"));
        fixture.close().await.unwrap();
    }
    /// `wt merge` leaves the record `pr.merge_dropped` compares GitHub
    /// against, and `wt merge --cancel` takes it away.
    #[tokio::test]
    async fn arming_records_the_merge_and_cancelling_forgets_it() {
        let fixture = CommandFixture::new().await.unwrap();
        let fake = fixture._root.path().join("fake-gh");
        fs::write(
            &fake,
            r#"#!/usr/bin/env python3
import json, sys
argv = sys.argv[1:]
args = ' '.join(argv)
if argv[:2] == ['repo', 'view']:
    print(json.dumps({'nameWithOwner': 'o/r'}))
elif argv[:2] == ['pr', 'merge']:
    pass
elif 'repos/o/r/pulls/42' in args:
    print('{}')
elif 'mergeQueue(branch' in args:
    print(json.dumps({'data': {'repository': {'mergeQueue': None}}}))
elif 'enablePullRequestAutoMerge' in args:
    print(json.dumps({'data': {'enablePullRequestAutoMerge': {'pullRequest': {'number': 42}}}}))
elif 'autoMergeRequest' in args:
    print(json.dumps({'data': {'node': {'mergeQueueEntry': None, 'autoMergeRequest': {'enabledAt': 'x'}}}}))
else:
    print(json.dumps({'number': 42, 'baseRefName': 'main', 'state': 'OPEN', 'isDraft': False,
                      'title': 'Ready', 'id': 'PR_fake', 'headRefOid': 'abc'}))
"#,
        )
        .unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
        let client = GithubClient::new(
            fixture.ctx.processes.clone(),
            fixture.ctx.config.paths.main_clone.clone(),
            GithubOptions::from_config(&fixture.ctx.config, false),
        )
        .with_gh_program(fake);
        let args = |cancel| MergeArgs {
            slug: Some("one".into()),
            cancel,
        };
        let stored = || async {
            fixture
                .ctx
                .database
                .call(|store| Ok(store.read_slug_state("one")?))
                .await
                .unwrap()
                .unwrap_or_default()["mergeArm"]
                .clone()
        };
        assert_eq!(
            run_with_client(&fixture.ctx, &args(false), &client)
                .await
                .unwrap(),
            0
        );
        let arm: wt_core::MergeArm = serde_json::from_value(stored().await).unwrap();
        assert_eq!((arm.pr, arm.head.as_deref()), (42, Some("abc")));
        assert_eq!(arm.mechanism, wt_core::MergeMechanism::AutoMerge);
        assert_eq!(
            run_with_client(&fixture.ctx, &args(true), &client)
                .await
                .unwrap(),
            0
        );
        assert!(stored().await.is_null());
        fixture.close().await.unwrap();
    }
}
