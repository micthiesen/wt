use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsString,
};

use anyhow::Result;
use clap::{Args, Subcommand};
use serde_json::{Value, json};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use wt_core::{
    MergeEdge, MergeEdgeKind, MergeEdgeStrength, edge_is_stale_by_sha, parse_merge_edge, work_age,
};
use wt_store::MergeEdge as StoredMergeEdge;
use wt_vcs::WorktreeRecord;

use crate::{
    commands::resolve::{resolve_named_worktree, run_git},
    context::AppContext,
};

#[derive(Debug, Clone, Args, Default)]
pub struct EdgeArgs {
    #[command(subcommand)]
    pub command: Option<EdgeCommand>,
    #[arg(value_name = "FROM")]
    pub from: Option<String>,
    #[arg(value_name = "KIND")]
    pub kind: Option<String>,
    #[arg(value_name = "TO")]
    pub to: Option<String>,
    #[arg(long, global = true, conflicts_with = "prefer")]
    pub blocks: bool,
    #[arg(long, global = true, conflicts_with = "blocks")]
    pub prefer: bool,
    #[arg(
        short = 'm',
        long = "message",
        global = true,
        allow_hyphen_values = true
    )]
    pub message: Option<String>,
    #[arg(long, global = true)]
    pub json: bool,
}

#[derive(Debug, Clone, Subcommand)]
pub enum EdgeCommand {
    Prune,
    Rm { from: String, to: String },
}

pub async fn run(ctx: &AppContext, args: &EdgeArgs) -> Result<i32> {
    match &args.command {
        Some(EdgeCommand::Prune) => return prune(ctx).await,
        Some(EdgeCommand::Rm { from, to }) => return remove(ctx, from, to).await,
        None => {}
    }
    match (&args.from, &args.kind, &args.to) {
        (None, None, None) => list(ctx, args.json).await,
        (Some(from), Some(kind), Some(to)) => assert_edge(ctx, args, from, kind, to).await,
        _ => {
            eprintln!("expected `wt edge <from> <before|conflicts|enables> <to>`");
            Ok(2)
        }
    }
}

async fn load_state(ctx: &AppContext) -> Result<Value> {
    ctx.database.call(|store| Ok(store.read_wt_state()?)).await
}

async fn list(ctx: &AppContext, json_output: bool) -> Result<i32> {
    let rows = ctx.repository.inventory(&ctx.cancellation).await?;
    let live = rows.iter().filter(|w| !w.is_main).collect::<Vec<_>>();
    let state = load_state(ctx).await?;
    let edges = state
        .get("edges")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(parse_merge_edge)
        .collect::<Vec<_>>();
    if edges.is_empty() {
        if json_output {
            println!("[]")
        } else {
            println!("No edges. Assert one: wt edge <from> before <to> -m <why>")
        }
        return Ok(0);
    }
    let heads = live
        .iter()
        .map(|w| (w.target.slug().to_owned(), w.head_sha.clone()))
        .collect::<BTreeMap<_, _>>();
    let with_stale = edges
        .iter()
        .map(|edge| {
            let stale = edge_is_stale_by_sha(edge, |slug| heads.get(slug).cloned().flatten());
            (edge, stale)
        })
        .collect::<Vec<_>>();
    if json_output {
        let value = with_stale
            .iter()
            .map(|(edge, stale)| {
                let mut value = serde_json::to_value(edge).unwrap_or(Value::Null);
                value["stale"] = json!(stale);
                value
            })
            .collect::<Vec<_>>();
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(0);
    }
    let now = now_ms();
    for (edge, stale) in with_stale {
        let age = work_age(&edge.at, now)
            .map(|age| format!(" · {age} ago"))
            .unwrap_or_default();
        let by = if edge.by == "fleet" {
            String::new()
        } else {
            format!(" · by {}", edge.by)
        };
        let why = edge
            .why
            .as_deref()
            .map(|why| format!("\n    {why}"))
            .unwrap_or_default();
        println!(
            "{} ─{}─▶ {}{}   {}{}{}{}",
            edge.from,
            kind_name(edge.kind),
            edge.to,
            if stale { " (stale)" } else { "" },
            strength_name(edge.strength),
            age,
            by,
            why,
        );
    }
    Ok(0)
}

async fn assert_edge(
    ctx: &AppContext,
    args: &EdgeArgs,
    from_arg: &str,
    kind_arg: &str,
    to_arg: &str,
) -> Result<i32> {
    let Some(kind) = parse_kind(kind_arg) else {
        eprintln!("unknown or ambiguous kind: {kind_arg} (before, conflicts, enables)");
        return Ok(2);
    };
    let from = match resolve_named_worktree(ctx, from_arg).await {
        Ok(w) if !w.is_main => w,
        Ok(_) => {
            eprintln!("no such worktree: {from_arg}");
            return Ok(1);
        }
        Err(e) => {
            eprintln!("{e}");
            return Ok(1);
        }
    };
    let to = match resolve_named_worktree(ctx, to_arg).await {
        Ok(w) if !w.is_main => w,
        Ok(_) => {
            eprintln!("no such worktree: {to_arg}");
            return Ok(1);
        }
        Err(e) => {
            eprintln!("{e}");
            return Ok(1);
        }
    };
    if from.target.slug() == to.target.slug() {
        eprintln!("an edge needs two different worktrees");
        return Ok(2);
    }
    let from_sha = match from.head_sha.clone() {
        Some(sha) => Some(sha),
        None => resolve_head(ctx, &from).await?,
    };
    let to_sha = match to.head_sha.clone() {
        Some(sha) => Some(sha),
        None => resolve_head(ctx, &to).await?,
    };
    let (Some(from_sha), Some(to_sha)) = (from_sha, to_sha) else {
        eprintln!("cannot resolve HEAD; refusing an edge with no decay anchor");
        return Ok(1);
    };
    let inventory = ctx.repository.inventory(&ctx.cancellation).await?;
    let by = crate::commands::resolve::worktree_at_cwd(&inventory, &ctx.cwd)
        .filter(|record| !record.is_main)
        .map_or_else(
            || "fleet".to_owned(),
            |record| record.target.slug().to_owned(),
        );
    let why = args
        .message
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let edge = MergeEdge {
        from: from.target.slug().into(),
        to: to.target.slug().into(),
        kind,
        strength: if args.blocks {
            MergeEdgeStrength::Blocks
        } else {
            MergeEdgeStrength::Prefer
        },
        why,
        at: now_iso(),
        by,
        from_sha: Some(from_sha),
        to_sha: Some(to_sha),
        extra: BTreeMap::new(),
    };
    let stored: StoredMergeEdge = serde_json::from_value(serde_json::to_value(&edge)?)?;
    ctx.database
        .call(move |store| {
            store.set_merge_edge(&stored)?;
            Ok(())
        })
        .await?;
    if args.json {
        println!("{}", serde_json::to_string(&edge)?)
    } else {
        println!(
            "✓ {} ─{}─▶ {} ({})",
            edge.from,
            kind_name(edge.kind),
            edge.to,
            strength_name(edge.strength)
        );
        println!("expires when either branch moves; re-assert then if it still matters");
        if kind == MergeEdgeKind::Before && edge.strength == MergeEdgeStrength::Prefer {
            println!("preference, not a gate; safe to merge out of order deliberately")
        }
    }
    Ok(0)
}

async fn resolve_head(ctx: &AppContext, worktree: &WorktreeRecord) -> Result<Option<String>> {
    let output = run_git(
        ctx,
        &worktree.target.path,
        [OsString::from("rev-parse"), OsString::from("HEAD")],
    )
    .await?;
    Ok(output
        .status
        .success()
        .then(|| output.stdout_text().trim().to_owned())
        .filter(|s| !s.is_empty()))
}

async fn remove(ctx: &AppContext, from_arg: &str, to_arg: &str) -> Result<i32> {
    let rows = ctx.repository.inventory(&ctx.cancellation).await?;
    let live = rows
        .iter()
        .filter(|w| !w.is_main)
        .map(|w| w.target.slug().to_owned())
        .collect::<BTreeSet<_>>();
    let removed = ctx
        .database
        .call(move |store| Ok(store.recently_removed_worktrees(&live, now_ms())?))
        .await?;
    let from = resolve_endpoint(ctx, from_arg, &removed).await;
    let to = resolve_endpoint(ctx, to_arg, &removed).await;
    let (Ok(from), Ok(to)) = (from, to) else {
        eprintln!("could not resolve one or both edge endpoints");
        return Ok(1);
    };
    let f = from.clone();
    let t = to.clone();
    let removed = ctx
        .database
        .call(move |store| Ok(store.remove_merge_edge(&f, &t)?))
        .await?;
    if removed {
        println!("✓ dropped {from} → {to}");
        Ok(0)
    } else {
        eprintln!("no edge {from} → {to}");
        Ok(1)
    }
}

async fn resolve_endpoint(
    ctx: &AppContext,
    input: &str,
    removed: &[wt_store::RemovedWorktree],
) -> Result<String> {
    if let Ok(record) = resolve_named_worktree(ctx, input).await {
        return Ok(record.target.slug().to_owned());
    }
    if let Some(record) = removed
        .iter()
        .find(|r| r.slug == input || r.branch == input)
    {
        return Ok(record.slug.clone());
    }
    Ok(input.to_owned())
}

async fn prune(ctx: &AppContext) -> Result<i32> {
    let rows = ctx.repository.inventory(&ctx.cancellation).await?;
    let live = rows
        .iter()
        .filter(|w| !w.is_main)
        .map(|w| w.target.slug().to_owned())
        .collect::<BTreeSet<_>>();
    let state = load_state(ctx).await?;
    let dead = state
        .get("edges")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(parse_merge_edge)
        .filter(|e| !live.contains(&e.from) || !live.contains(&e.to))
        .collect::<Vec<_>>();
    if dead.is_empty() {
        println!("No edges with dead endpoints.");
        return Ok(0);
    }
    ctx.database
        .call(move |store| Ok(store.prune_merge_edges(&live)?))
        .await?;
    for edge in dead {
        println!("✓ dropped {} → {} (endpoint gone)", edge.from, edge.to)
    }
    Ok(0)
}

fn parse_kind(input: &str) -> Option<MergeEdgeKind> {
    let kinds = [
        ("before", MergeEdgeKind::Before),
        ("conflicts", MergeEdgeKind::Conflicts),
        ("enables", MergeEdgeKind::Enables),
    ];
    let matches = kinds
        .iter()
        .filter(|(name, _)| name.starts_with(input))
        .collect::<Vec<_>>();
    (matches.len() == 1).then(|| matches[0].1)
}
fn kind_name(kind: MergeEdgeKind) -> &'static str {
    match kind {
        MergeEdgeKind::Before => "before",
        MergeEdgeKind::Conflicts => "conflicts",
        MergeEdgeKind::Enables => "enables",
    }
}
fn strength_name(strength: MergeEdgeStrength) -> &'static str {
    match strength {
        MergeEdgeStrength::Blocks => "blocks",
        MergeEdgeStrength::Prefer => "prefer",
    }
}
fn now_ms() -> i64 {
    OffsetDateTime::now_utc()
        .unix_timestamp_nanos()
        .saturating_div(1_000_000) as i64
}
fn now_iso() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn edge_kind_prefix_must_be_unique() {
        assert_eq!(parse_kind("b"), Some(MergeEdgeKind::Before));
        assert_eq!(parse_kind("c"), Some(MergeEdgeKind::Conflicts));
        assert_eq!(parse_kind("e"), Some(MergeEdgeKind::Enables));
        assert_eq!(parse_kind(""), None);
    }

    #[tokio::test]
    async fn edge_attribution_uses_third_worktree_as_author() {
        let fixture = crate::commands::test_support::CommandFixture::new()
            .await
            .unwrap();
        let third = fixture.ctx.config.paths.worktree_root.join("three");
        run_git(
            &fixture.ctx,
            &fixture.ctx.config.paths.main_clone,
            [
                OsString::from("worktree"),
                OsString::from("add"),
                OsString::from("-b"),
                OsString::from("feature/three"),
                third.as_os_str().to_owned(),
                OsString::from("main"),
            ],
        )
        .await
        .unwrap()
        .checked("git")
        .unwrap();
        let mut author = fixture.ctx.clone();
        author.cwd = third;
        assert_eq!(
            run(
                &author,
                &EdgeArgs {
                    from: Some("one".into()),
                    kind: Some("before".into()),
                    to: Some("two".into()),
                    ..Default::default()
                }
            )
            .await
            .unwrap(),
            0
        );
        assert_eq!(
            load_state(&author).await.unwrap()["edges"][0]["by"],
            "three"
        );
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn edge_assertion_persists_both_head_anchors_and_removes_pair() {
        use crate::commands::test_support::CommandFixture;
        let fixture = CommandFixture::new().await.unwrap();
        let args = EdgeArgs {
            from: Some("one".into()),
            kind: Some("before".into()),
            to: Some("two".into()),
            blocks: true,
            ..Default::default()
        };
        assert_eq!(run(&fixture.ctx, &args).await.unwrap(), 0);
        let state = load_state(&fixture.ctx).await.unwrap();
        let edge = &state["edges"][0];
        assert_eq!(edge["from"], "one");
        assert_eq!(edge["to"], "two");
        assert_eq!(edge["strength"], "blocks");
        assert!(edge["fromSha"].as_str().is_some());
        assert!(edge["toSha"].as_str().is_some());
        assert_eq!(
            run(
                &fixture.ctx,
                &EdgeArgs {
                    command: Some(EdgeCommand::Rm {
                        from: "one".into(),
                        to: "two".into()
                    }),
                    ..Default::default()
                }
            )
            .await
            .unwrap(),
            0
        );
        let state = load_state(&fixture.ctx).await.unwrap();
        assert!(state["edges"].as_array().unwrap().is_empty());
        fixture.close().await.unwrap();
    }
}
