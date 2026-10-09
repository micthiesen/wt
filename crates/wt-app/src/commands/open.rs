use crate::{context::AppContext, editor, prompt};
use anyhow::{Result, bail};
use clap::Args;
use std::io::IsTerminal;

#[derive(Debug, Clone, Args, Default)]
pub struct OpenArgs {
    /// Exact slug or case-insensitive substring; no query opens a picker.
    pub query: Option<String>,
}

pub async fn run(context: &AppContext, args: &OpenArgs) -> Result<i32> {
    let rows = context.repository.inventory(&context.cancellation).await?;
    let rows: Vec<_> = rows.iter().filter(|row| !row.is_main).collect();
    if rows.is_empty() {
        println!("No worktrees.");
        return Ok(1);
    }
    let matches = if let Some(query) = &args.query {
        if let Some(exact) = rows.iter().find(|row| row.target.slug() == query) {
            vec![*exact]
        } else {
            let query = query.to_lowercase();
            rows.iter()
                .copied()
                .filter(|row| row.target.slug().to_lowercase().contains(&query))
                .collect()
        }
    } else {
        rows
    };
    if matches.is_empty() {
        bail!(
            "No worktree matching: {}",
            args.query.as_deref().unwrap_or_default()
        );
    }
    let target = if args.query.is_some() && matches.len() == 1 {
        matches[0]
    } else {
        if !std::io::stdin().is_terminal() {
            eprintln!("An unambiguous slug is required in non-interactive mode.");
            return Ok(2);
        }
        let choices = matches
            .iter()
            .map(|row| row.target.slug().to_owned())
            .collect::<Vec<_>>();
        let Some(index) =
            prompt::pick(&choices, "Open which worktree?", &context.cancellation).await?
        else {
            return Ok(0);
        };
        matches[index]
    };
    editor::open(context, std::path::Path::new(&target.target.path)).await?;
    Ok(0)
}
