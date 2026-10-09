use std::{ffi::OsString, path::Path};

use anyhow::Result;
use clap::Args;
use serde_json::Value;

use crate::{
    commands::resolve::{resolve_worktree, run_git},
    context::AppContext,
    database::Database,
};

const USAGE: &str = "usage: wt base <slug>                 show the recorded fork base\n       wt base set <slug> <ref>      record <ref> as the fork base\n       wt base clear <slug>          forget the recorded fork base";

/// Positional form is retained because `wt base <slug>` is a show command,
/// while `set` and `clear` are legacy verb prefixes.
#[derive(Debug, Clone, Args, Default)]
pub struct BaseArgs {
    #[arg(value_name = "ARG", num_args = 0.., allow_hyphen_values = true)]
    pub positionals: Vec<String>,
}

pub async fn run(ctx: &AppContext, args: &BaseArgs) -> Result<i32> {
    let values = &args.positionals;
    match values.first().map(String::as_str) {
        None => {
            println!("{USAGE}");
            Ok(2)
        }
        Some("set") => {
            if values.len() != 3 {
                eprintln!("{USAGE}");
                return Ok(2);
            }
            let slug = values[1].clone();
            let record = match resolve_worktree(ctx, Some(&slug)).await {
                Ok(record) if !record.is_main => record,
                Ok(_) => {
                    eprintln!("no worktree: {slug}");
                    return Ok(1);
                }
                Err(error) => {
                    eprintln!("{error}");
                    return Ok(1);
                }
            };
            let reference = values[2].clone();
            let branch = reference
                .strip_prefix("origin/")
                .unwrap_or(&reference)
                .to_owned();
            if branch == ctx.config.branch.base {
                eprintln!(
                    "{} is trunk; that's the default; use `wt base clear` instead",
                    branch
                );
                return Ok(2);
            }
            if branch == record.target.branch {
                eprintln!(
                    "{} is {}'s own branch; a worktree can't be based on itself",
                    branch,
                    record.target.slug()
                );
                return Ok(2);
            }
            let output = run_git(
                ctx,
                Path::new(&record.target.path),
                [
                    OsString::from("rev-parse"),
                    OsString::from("--verify"),
                    OsString::from(&reference),
                ],
            )
            .await?;
            let valid = output.status.success() || {
                let fallback = format!("origin/{branch}");
                run_git(
                    ctx,
                    Path::new(&record.target.path),
                    [
                        OsString::from("rev-parse"),
                        OsString::from("--verify"),
                        OsString::from(fallback),
                    ],
                )
                .await?
                .status
                .success()
            };
            if !valid {
                eprintln!("ref does not resolve: {reference}");
                return Ok(1);
            }
            let merge_base = run_git(
                ctx,
                Path::new(&record.target.path),
                [
                    OsString::from("merge-base"),
                    OsString::from(&record.target.branch),
                    OsString::from(&reference),
                ],
            )
            .await?;
            let sha = merge_base
                .status
                .success()
                .then(|| merge_base.stdout_text().trim().to_owned())
                .filter(|s| !s.is_empty());
            let key = record.target.slug().to_owned();
            let sha_for_db = sha.clone();
            let stored_branch = branch.clone();
            ctx.database
                .call(move |store| {
                    store.set_slug_base(&key, Some((&stored_branch, sha_for_db.as_deref())))?;
                    Ok(())
                })
                .await?;
            println!(
                "✓ {} base → {}{}",
                record.target.slug(),
                branch,
                sha.as_deref()
                    .map(|s| format!(" @ {}", &s[..s.len().min(12)]))
                    .unwrap_or_default()
            );
            println!("restart wt (or wait for the next state refresh) to see it in the TUI");
            Ok(0)
        }
        Some("clear") => {
            if values.len() != 2 {
                eprintln!("{USAGE}");
                return Ok(2);
            }
            let slug = values[1].clone();
            let state = read_state(&ctx.database).await?;
            let base = state
                .get("slugs")
                .and_then(|slugs| slugs.get(&slug))
                .and_then(|entry| entry.get("baseBranch"))
                .and_then(Value::as_str);
            if base.is_none() {
                println!("{slug}: nothing recorded");
                return Ok(0);
            }
            let key = slug.clone();
            ctx.database
                .call(move |store| {
                    store.set_slug_base(&key, None)?;
                    Ok(())
                })
                .await?;
            println!(
                "✓ cleared — {slug} diffs against {} again",
                ctx.config.branch.base
            );
            Ok(0)
        }
        Some("--help" | "-h") => {
            println!("{USAGE}");
            Ok(0)
        }
        Some(slug) => {
            if values.len() != 1 {
                eprintln!("{USAGE}");
                return Ok(2);
            }
            let slug = slug.to_owned();
            let state = read_state(&ctx.database).await?;
            let entry = state.get("slugs").and_then(|s| s.get(&slug));
            let base = entry
                .and_then(|e| e.get("baseBranch"))
                .and_then(Value::as_str);
            let sha = entry.and_then(|e| e.get("baseSha")).and_then(Value::as_str);
            if let Some(base) = base {
                println!(
                    "{slug}: {base}{}",
                    sha.map(|s| format!(" @ {}", &s[..s.len().min(12)]))
                        .unwrap_or_default()
                );
            } else {
                println!(
                    "{slug}: no recorded fork base (diffs against {})",
                    ctx.config.branch.base
                );
            }
            Ok(0)
        }
    }
}

async fn read_state(database: &Database) -> Result<Value> {
    database.call(|store| Ok(store.read_wt_state()?)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::test_support::CommandFixture;

    #[tokio::test]
    async fn set_records_a_merge_base_anchor_and_clear_removes_it() {
        let fixture = CommandFixture::new().await.unwrap();
        assert_eq!(
            run(
                &fixture.ctx,
                &BaseArgs {
                    positionals: vec!["set".into(), "one".into(), "base-branch".into()],
                },
            )
            .await
            .unwrap(),
            0
        );
        let state = read_state(&fixture.ctx.database).await.unwrap();
        assert_eq!(state["slugs"]["one"]["baseBranch"], "base-branch");
        assert!(state["slugs"]["one"]["baseSha"].as_str().is_some());
        assert_eq!(
            run(
                &fixture.ctx,
                &BaseArgs {
                    positionals: vec!["clear".into(), "one".into()],
                },
            )
            .await
            .unwrap(),
            0
        );
        let state = read_state(&fixture.ctx.database).await.unwrap();
        assert!(state["slugs"]["one"].get("baseBranch").is_none());
        fixture.close().await.unwrap();
    }
}
