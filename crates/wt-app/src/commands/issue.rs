use std::{path::Path, time::Duration};

use anyhow::Result;
use clap::Args;
use wt_platform::process::CommandSpec;

use crate::{commands::resolve::resolve_worktree, context::AppContext};

#[derive(Debug, Clone, Args, Default)]
pub struct IssueArgs {
    #[arg(value_name = "SLUG_OR_BRANCH")]
    pub target: Option<String>,
    #[arg(long, value_name = "ID", conflicts_with_all = ["no_id", "clear_id", "clear_gh", "gh", "read"])]
    pub id: Option<String>,
    #[arg(long, conflicts_with_all = ["id", "clear_id", "clear_gh", "gh", "read"])]
    pub no_id: bool,
    #[arg(long, conflicts_with_all = ["id", "no_id", "clear_gh", "gh", "read"])]
    pub clear_id: bool,
    #[arg(long, value_name = "N", conflicts_with_all = ["id", "no_id", "clear_id", "clear_gh", "read"])]
    pub gh: Option<u64>,
    #[arg(long, conflicts_with_all = ["id", "no_id", "clear_id", "gh", "read"])]
    pub clear_gh: bool,
    #[arg(long, conflicts_with_all = ["id", "no_id", "clear_id", "gh", "clear_gh"])]
    pub read: bool,
}

pub async fn run(ctx: &AppContext, args: &IssueArgs) -> Result<i32> {
    let mutating =
        args.id.is_some() || args.no_id || args.clear_id || args.gh.is_some() || args.clear_gh;
    if mutating && args.target.is_none() {
        eprintln!("a worktree slug or branch is required for issue mutations");
        return Ok(2);
    }
    if args.read && args.target.is_none() && !is_inside_non_main_worktree(ctx).await? {
        eprintln!("not inside a wt worktree; pass a slug to `wt issue <slug> --read`");
        return Ok(1);
    }
    let record = match resolve_worktree(ctx, args.target.as_deref()).await {
        Ok(record) if !record.is_main => record,
        Ok(_) => {
            eprintln!("the main clone has no worktree issue record");
            return Ok(1);
        }
        Err(error) => {
            eprintln!("{error}");
            return Ok(1);
        }
    };
    let slug = record.target.slug().to_owned();

    if let Some(raw) = args.id.as_deref() {
        let raw = raw.trim();
        let id = raw.to_ascii_uppercase();
        if !valid_issue_id(&id) {
            eprintln!("not an issue id: {raw} (expected e.g. COZ-2185)");
            return Ok(2);
        }
        let write_slug = slug.clone();
        let write_id = id.clone();
        ctx.database
            .call(move |store| {
                store.set_slug_issue_id(&write_slug, Some(&write_id))?;
                Ok(())
            })
            .await?;
        println!("✓ {slug} ← {id}");
        if let Some(url) = issue_url(ctx, &id).await {
            println!("  {url}");
        }
        return Ok(0);
    }
    if args.no_id {
        let write_slug = slug.clone();
        ctx.database
            .call(move |store| {
                store.set_slug_issue_id(&write_slug, Some(""))?;
                Ok(())
            })
            .await?;
        println!("✓ {slug} has no tracker id");
        if let Some(parsed) = issue_id_from_slug(ctx, record.target.slug()) {
            println!("  (overrides {parsed} from the slug; --clear-id restores it)");
        }
        return Ok(0);
    }
    if args.clear_id {
        let write_slug = slug.clone();
        ctx.database
            .call(move |store| {
                store.set_slug_issue_id(&write_slug, None)?;
                Ok(())
            })
            .await?;
        match issue_id_from_slug(ctx, record.target.slug()) {
            Some(id) => println!("✓ {slug} tracker id override cleared; back to {id} (from slug)"),
            None => println!("✓ {slug} tracker id cleared; the slug carries none"),
        }
        return Ok(0);
    }
    if let Some(number) = args.gh {
        if number == 0 {
            eprintln!("--gh requires a positive issue number");
            return Ok(2);
        }
        let write_slug = slug.clone();
        ctx.database
            .call(move |store| {
                store.set_slug_github_issue(&write_slug, Some(number))?;
                Ok(())
            })
            .await?;
        println!("✓ {slug} ← gh issue #{number}");
        if let Some(url) = github_issue_url(ctx, number).await {
            println!("  {url}");
        }
        return Ok(0);
    }
    if args.clear_gh {
        let write_slug = slug.clone();
        ctx.database
            .call(move |store| {
                store.set_slug_github_issue(&write_slug, None)?;
                Ok(())
            })
            .await?;
        println!("✓ {slug} gh issue cleared");
        return Ok(0);
    }

    let entry = ctx
        .database
        .call({
            let slug = slug.clone();
            move |store| Ok(store.read_slug_state(&slug)?)
        })
        .await?;
    let stored_id = entry
        .as_ref()
        .and_then(|entry| entry.get("issueId"))
        .and_then(serde_json::Value::as_str);
    let id = resolve_issue_id(record.target.slug(), stored_id);
    if args.read {
        return read_issue(ctx, Path::new(&record.target.path), id.as_deref()).await;
    }
    let gh = entry
        .as_ref()
        .and_then(|entry| entry.get("githubIssue"))
        .and_then(serde_json::Value::as_u64);
    println!("{}", record.target.slug());
    let source = if stored_id.is_some() {
        " (set)"
    } else if id.is_some() {
        " (from slug)"
    } else {
        ""
    };
    println!("  issue: {}{source}", id.as_deref().unwrap_or("—"));
    if let Some(id) = id.as_deref()
        && let Some(url) = issue_url(ctx, id).await
    {
        println!("         {url}");
    }
    if let Some(number) = gh {
        println!("  gh:    #{number}");
        if let Some(url) = github_issue_url(ctx, number).await {
            println!("         {url}");
        }
    } else {
        println!("  gh:    —");
    }
    Ok(0)
}

async fn is_inside_non_main_worktree(ctx: &AppContext) -> Result<bool> {
    let inventory = ctx.repository.inventory(&ctx.cancellation).await?;
    Ok(
        crate::commands::resolve::worktree_at_cwd(&inventory, &ctx.cwd)
            .is_some_and(|record| !record.is_main),
    )
}

async fn read_issue(ctx: &AppContext, cwd: &Path, id: Option<&str>) -> Result<i32> {
    let Some(id) = id else {
        eprintln!("no tracker task attached; nothing to read");
        return Ok(0);
    };
    let Some(command) = ctx
        .config
        .issue_tracker
        .as_ref()
        .and_then(|config| config.read_command.as_ref())
    else {
        eprintln!(
            "no task reader configured for {id}; set [issue_tracker] read_command or read the linked task directly"
        );
        return Ok(3);
    };
    let argv = command
        .iter()
        .map(|arg| arg.replace("{id}", id))
        .collect::<Vec<_>>();
    let Some((program, args)) = argv.split_first() else {
        eprintln!("configured issue reader has no executable");
        return Ok(3);
    };
    let mut spec = CommandSpec::new(program.clone())
        .args(args.iter().cloned())
        .cwd(cwd);
    spec.timeout = Duration::from_secs(300);
    let output = match ctx.processes.run(spec, &ctx.cancellation).await {
        Ok(output) => output,
        Err(error) => {
            eprintln!("task read incomplete for {id}: {error}");
            return Ok(match error {
                wt_platform::process::ProcessError::Timeout { .. } => 124,
                _ => 1,
            });
        }
    };
    if !output.stdout.is_empty() {
        print!("{}", String::from_utf8_lossy(&output.stdout));
    }
    if !output.stderr.is_empty() {
        eprint!("{}", String::from_utf8_lossy(&output.stderr));
    }
    if output.status.success() {
        return Ok(0);
    }
    let code = output.status.code().unwrap_or(1);
    eprintln!("task read incomplete for {id}: reader exited {code}");
    Ok(if code == 3 { 1 } else { code })
}

fn valid_issue_id(raw: &str) -> bool {
    let Some((prefix, number)) = raw.split_once('-') else {
        return false;
    };
    !prefix.is_empty()
        && prefix.bytes().all(|byte| byte.is_ascii_uppercase())
        && !number.is_empty()
        && number.bytes().all(|byte| byte.is_ascii_digit())
}

fn issue_id_from_slug(_ctx: &AppContext, slug: &str) -> Option<String> {
    crate::issue_identity::resolve(slug, None)
}

fn resolve_issue_id(slug: &str, stored: Option<&str>) -> Option<String> {
    crate::issue_identity::resolve(slug, stored)
}

async fn issue_url(ctx: &AppContext, id: &str) -> Option<String> {
    if let Some(number) = id
        .strip_prefix("GH-")
        .and_then(|value| value.parse::<u64>().ok())
    {
        return github_issue_url(ctx, number).await;
    }
    ctx.config
        .issue_tracker
        .as_ref()?
        .url_template
        .as_ref()
        .map(|template| template.replace("{id}", id))
}

async fn github_issue_url(ctx: &AppContext, number: u64) -> Option<String> {
    let remote = ctx
        .processes
        .run(
            CommandSpec::new("git")
                .args(["remote", "get-url", "origin"])
                .cwd(ctx.config.paths.main_clone.clone()),
            &ctx.cancellation,
        )
        .await
        .ok()?;
    if !remote.status.success() {
        return None;
    }
    Some(format!(
        "{}/issues/{number}",
        repo_web_url(&remote.stdout_text())?
    ))
}

fn repo_web_url(remote: &str) -> Option<String> {
    let raw = remote.trim();
    let raw = raw
        .strip_prefix("https://")
        .or_else(|| raw.strip_prefix("http://"))
        .or_else(|| raw.strip_prefix("git@"))
        .or_else(|| raw.strip_prefix("ssh://git@"))?;
    let (host, path) = raw.split_once(':').or_else(|| raw.split_once('/'))?;
    let host = host.split('/').next()?;
    let path = path.trim_end_matches(".git").trim_end_matches('/');
    let mut parts = path.split('/');
    let owner = parts.next()?;
    let name = parts.next()?;
    if parts.next().is_some() || host.is_empty() || owner.is_empty() || name.is_empty() {
        return None;
    }
    Some(format!("https://{host}/{owner}/{name}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issue_ids_require_prefix_and_decimal_suffix() {
        assert!(valid_issue_id("COZ-2185"));
        assert!(valid_issue_id("GH-42"));
        assert!(!valid_issue_id("COZ-"));
        assert!(!valid_issue_id("coz-42"));
        assert!(!valid_issue_id("COZ-4-2"));
    }

    #[test]
    fn explicit_empty_issue_id_suppresses_slug_fallback() {
        assert_eq!(resolve_issue_id("coz-2185-fix", Some("")), None);
        assert_eq!(
            resolve_issue_id("coz-2185-fix", None).as_deref(),
            Some("COZ-2185")
        );
    }

    #[test]
    fn github_urls_support_https_and_ssh_remotes() {
        assert_eq!(
            repo_web_url("https://github.com/acme/wt.git"),
            Some("https://github.com/acme/wt".into())
        );
        assert_eq!(
            repo_web_url("git@github.com:acme/wt.git"),
            Some("https://github.com/acme/wt".into())
        );
    }
}
