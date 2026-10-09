use crate::{commands::resolve::resolve_worktree, context::AppContext};
use anyhow::{Result, bail};
use clap::Args;
use wt_github::{GithubClient, GithubOptions, PrMergeTarget};

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
    let target = PrMergeTarget {
        id: pr.id,
        number: pr.number,
        base_ref_name: pr.base_ref_name,
        head_ref_oid: pr.head_ref_oid,
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
    if result.ok {
        if args.cancel {
            println!("cancelled merge-when-ready on #{}", target.number);
        } else {
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
    use crate::commands::test_support::CommandFixture;
    use std::{fs, os::unix::fs::PermissionsExt};

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
}
