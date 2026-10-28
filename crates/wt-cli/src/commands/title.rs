use anyhow::Result;
use clap::Args;
use wt_store::{TitleSource, TitleWrite};

use wt_app_core::context::AppContext;
use wt_ops::commands::resolve::resolve_worktree;

#[derive(Debug, Clone, Args, Default)]
pub struct TitleArgs {
    /// The new title. Omit it to show the current title and who set it.
    #[arg(value_name = "TITLE", num_args = 0.., allow_hyphen_values = true)]
    pub words: Vec<String>,
    /// Worktree slug or branch (default: current worktree).
    #[arg(long, short = 'w', value_name = "SLUG")]
    pub worktree: Option<String>,
}

pub async fn run(ctx: &AppContext, args: &TitleArgs) -> Result<i32> {
    let record = match resolve_worktree(ctx, args.worktree.as_deref()).await {
        Ok(record) if !record.is_main => record,
        Ok(_) => {
            eprintln!("not inside a worktree; pass --worktree <slug>");
            return Ok(1);
        }
        Err(error) => {
            eprintln!("{error}");
            return Ok(1);
        }
    };
    let slug = record.target.slug().to_owned();
    let title = args.words.join(" ").trim().to_owned();
    if title.is_empty() {
        let state = ctx
            .database
            .call(|store| Ok(store.read_wt_state()?))
            .await?;
        match wt_store::stored_title(&state, &slug) {
            Some((title, source)) => println!("{slug}: {title} ({})", source.as_str()),
            None => {
                println!("{slug} has no title set; it shows its PR title, first commit, or slug")
            }
        }
        return Ok(0);
    }
    if title.chars().any(char::is_control) {
        eprintln!("a title is one line of text");
        return Ok(2);
    }
    // The CLI is the agents' path; people rename with the TUI's `t`.
    let source = TitleSource::Agent;
    let db_slug = slug.clone();
    let saved = title.clone();
    let outcome = ctx
        .database
        .call(move |store| Ok(store.set_slug_title(&db_slug, &saved, source, None)?))
        .await?;
    match outcome {
        TitleWrite::Saved => {
            println!("✓ {slug}: {title} ({})", source.as_str());
            Ok(0)
        }
        TitleWrite::Outranked(kept) => {
            eprintln!(
                "{slug} keeps its {} title; {}",
                kept.as_str(),
                if kept == TitleSource::User {
                    "a person's rename is final"
                } else {
                    "an agent renames an issue or fallback title only once"
                }
            );
            Ok(1)
        }
        TitleWrite::Stale => unreachable!("no revision was expected"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wt_app_core::test_support::CommandFixture;

    #[tokio::test]
    async fn an_agent_replaces_an_issue_title_once() {
        let fixture = CommandFixture::new().await.unwrap();
        let slug = "one".to_owned();
        let seeded = slug.clone();
        fixture
            .ctx
            .database
            .call(move |store| {
                Ok(store.set_slug_title(&seeded, "From the tracker", TitleSource::Issue, None)?)
            })
            .await
            .unwrap();
        let set = |title: &str| TitleArgs {
            words: vec![title.into()],
            worktree: Some(slug.clone()),
        };
        assert_eq!(
            run(&fixture.ctx, &set("Render the calendar once"))
                .await
                .unwrap(),
            0
        );
        assert_eq!(run(&fixture.ctx, &set("Second try")).await.unwrap(), 1);
        let state = fixture
            .ctx
            .database
            .call(|store| Ok(store.read_wt_state()?))
            .await
            .unwrap();
        assert_eq!(
            wt_store::stored_title(&state, &slug),
            Some(("Render the calendar once".into(), TitleSource::Agent))
        );
        fixture.close().await.unwrap();
    }
}
