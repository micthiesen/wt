use anyhow::Result;
use clap::Args;
use serde_json::Value;

use crate::{commands::resolve::resolve_worktree, context::AppContext, database::Database};

const USAGE: &str = "usage: wt base <slug>                 show the recorded fork base\n       wt base set <slug> <ref>      record <ref> as the fork base\n       wt base clear <slug>          return to trunk, retaining the replay anchor";

#[derive(Debug, Clone, Args, Default)]
pub struct BaseArgs {
    #[arg(value_name = "ARG", num_args = 0..)]
    pub positionals: Vec<String>,
}

pub async fn run(ctx: &AppContext, args: &BaseArgs) -> Result<i32> {
    let values = &args.positionals;
    match values.first().map(String::as_str) {
        Some("set" | "clear") => {
            let clearing = values[0] == "clear";
            if values.len() != if clearing { 2 } else { 3 } {
                eprintln!("{USAGE}");
                return Ok(2);
            }
            let row = resolve_worktree(ctx, Some(&values[1])).await?;
            let base = if clearing {
                None
            } else {
                Some(values[2].clone())
            };
            let (branch, anchor) =
                crate::fork_base::set(ctx, &wt_core::worktree_target_key(&row.target), base)
                    .await?;
            println!(
                "✓ {} base → {} @ {}",
                row.target.slug(),
                branch,
                &anchor[..anchor.len().min(12)]
            );
            Ok(0)
        }
        Some(slug) if values.len() == 1 => {
            let state = read_state(&ctx.database).await?;
            let entry = state.get("slugs").and_then(|s| s.get(slug));
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
        _ => {
            eprintln!("{USAGE}");
            Ok(2)
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
    async fn set_records_a_merge_base_anchor_and_clear_retains_it() {
        let fixture = CommandFixture::new().await.unwrap();
        let path = &fixture.ctx.cwd;
        std::fs::write(path.join("parent-work.txt"), "parent contribution\n").unwrap();
        for args in [
            vec!["add", "parent-work.txt"],
            vec!["commit", "-m", "parent contribution"],
            vec!["branch", "-f", "base-branch", "HEAD"],
        ] {
            crate::commands::resolve::run_git(&fixture.ctx, path, args)
                .await
                .unwrap()
                .checked("git")
                .unwrap();
        }
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
        let saved_anchor = state["slugs"]["one"]["baseSha"].clone();
        let trunk = crate::commands::resolve::run_git(&fixture.ctx, path, ["rev-parse", "main"])
            .await
            .unwrap()
            .stdout_text();
        assert_ne!(saved_anchor.as_str().unwrap(), trunk.trim());
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
        assert_eq!(state["slugs"]["one"]["baseBranch"], "main");
        assert_eq!(state["slugs"]["one"]["baseSha"], saved_anchor);
        fixture.close().await.unwrap();
    }
}
