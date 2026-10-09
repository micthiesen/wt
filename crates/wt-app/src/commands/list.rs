use std::collections::BTreeSet;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use clap::Args;
use serde_json::{Value, json};
use wt_store::{RemovedWorktree, WorkStatusRecord};

use crate::{commands::resolve::run_git, context::AppContext};

#[derive(Debug, Clone, Args, Default)]
pub struct ListArgs {
    #[arg(long)]
    pub json: bool,
}

pub async fn run(ctx: &AppContext, args: &ListArgs) -> Result<i32> {
    let snapshots = ctx.repository.inventory_status(&ctx.cancellation).await?;
    let live = snapshots
        .iter()
        .filter(|snapshot| !snapshot.worktree.is_main)
        .collect::<Vec<_>>();
    let live_slugs = live
        .iter()
        .map(|snapshot| snapshot.worktree.target.slug().to_owned())
        .collect::<BTreeSet<_>>();
    let now = now_ms();
    let (state, removed) = ctx
        .database
        .call(move |store| {
            Ok((
                store.read_wt_state()?,
                store.recently_removed_worktrees(&live_slugs, now)?,
            ))
        })
        .await?;
    if args.json {
        let remote_url = run_git(
            ctx,
            &ctx.config.paths.main_clone,
            ["remote", "get-url", "origin"],
        )
        .await
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned());
        let rows = live
            .iter()
            .map(|snapshot| live_json(ctx, &state, snapshot, remote_url.as_deref()))
            .chain(removed.iter().map(removed_json))
            .collect::<Vec<_>>();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(0);
    }
    if live.is_empty() {
        if removed.is_empty() {
            println!("No worktrees.");
        } else {
            println!("No active worktrees ({} recently removed).", removed.len());
        }
        return Ok(0);
    }
    println!("{:<34}  {:<42}  {:<8}  PATH", "SLUG", "BRANCH", "STATUS");
    for snapshot in live {
        let row = &snapshot.worktree;
        let status = match snapshot.status.as_ref() {
            Some(status) if status.dirty => "dirty",
            Some(_) => "clean",
            None => "missing",
        };
        println!(
            "{:<34}  {:<42}  {:<8}  {}",
            row.target.slug(),
            row.target.branch,
            status,
            row.target.path
        );
    }
    let merged = removed
        .iter()
        .filter(|entry| is_merged(entry))
        .map(|entry| entry.slug.as_str())
        .collect::<Vec<_>>();
    if !merged.is_empty() {
        println!("\nrecently merged: {}", merged.join(", "));
    }
    Ok(0)
}

fn live_json(
    ctx: &AppContext,
    state: &Value,
    snapshot: &wt_vcs::WorktreeSnapshot,
    remote_url: Option<&str>,
) -> Value {
    let target = &snapshot.worktree.target;
    let entry = state
        .get("slugs")
        .and_then(|slugs| slugs.get(target.slug()));
    let work = entry
        .and_then(|entry| entry.get("work"))
        .and_then(|value| serde_json::from_value::<WorkStatusRecord>(value.clone()).ok());
    let issue_override = entry
        .and_then(|entry| entry.get("issueId"))
        .and_then(Value::as_str);
    let issue_id = if issue_override == Some("") {
        None
    } else {
        issue_override
            .map(str::to_owned)
            .or_else(|| issue_id_from_slug(ctx, target.slug()))
    };
    let status = match snapshot.status.as_ref() {
        Some(status) if status.dirty => ("dirty", "dirty"),
        Some(_) => ("clean", "clean"),
        None => ("missing", "missing"),
    };
    let expected_origin = format!("origin/{}", target.branch);
    let remote_push = snapshot
        .status
        .as_ref()
        .filter(|status| status.upstream.as_deref() == Some(expected_origin.as_str()));
    let deployed =
        safe_pinned_stage(&ctx.config.stage.prefix, target.path.as_ref()).is_some_and(|stage| {
            std::fs::read_to_string(std::path::Path::new(&target.path).join(".sst/outputs.json"))
                .is_ok_and(|outputs| outputs.contains(&stage))
        });
    let base = entry
        .and_then(|entry| entry.get("baseBranch"))
        .and_then(Value::as_str)
        .unwrap_or(&ctx.config.branch.base);
    json!({
        "slug": target.slug(),
        "branch": target.branch,
        "path": target.path,
        "stage": target.stage,
        "deployed": deployed,
        "kind": "live",
        "section": entry.and_then(|entry| entry.get("section")).and_then(Value::as_str),
        "base": base,
        "exists": true,
        "status": status.0,
        "status_label": status.1,
        "status_age": Value::Null,
        "status_op": Value::Null,
        "dev": Value::Null,
        "dirty": snapshot.status.as_ref().is_some_and(|status| status.dirty),
        "unpushed": remote_push.and_then(|status| status.ahead).map_or(Value::Null, |ahead| json!(ahead)),
        "pushed": remote_push.map_or(Value::Null, |_| json!(true)),
        "ahead_of_base": Value::Null,
        "issue_id": issue_id,
        "issue_url": issue_id.as_deref().and_then(|id| issue_url(ctx, id, remote_url)),
        "gh_issue": entry.and_then(|entry| entry.get("githubIssue")),
        "gh_issue_url": entry
            .and_then(|entry| entry.get("githubIssue"))
            .and_then(Value::as_u64)
            .and_then(|issue| remote_url.and_then(repo_web_url).map(|repo| format!("{repo}/issues/{issue}"))),
        "work_state": work.as_ref().map(|work| work.state.as_str()),
        "work_note": work.as_ref().and_then(|work| work.note.as_deref()),
        "work_risk": work.as_ref().and_then(|work| work.risk.as_deref()),
        "work_blocked_on": work.as_ref().and_then(|work| work.blocked_on.as_deref()),
        "work_verify_after_merge": work.as_ref().and_then(|work| work.verify_after_merge.as_deref()),
        "work_at": work.as_ref().map(|work| work.at.as_str()),
        "inventory_error": snapshot.error,
    })
}

fn issue_id_from_slug(ctx: &AppContext, slug: &str) -> Option<String> {
    let pattern = regex::Regex::new(&ctx.config.branch.id_pattern).ok()?;
    pattern
        .captures(slug)?
        .get(0)
        .map(|value| value.as_str().trim_end_matches('-').to_ascii_uppercase())
}

fn issue_url(ctx: &AppContext, id: &str, remote_url: Option<&str>) -> Option<String> {
    if id.to_ascii_uppercase().starts_with("GH-") {
        let repo = repo_web_url(remote_url?)?;
        return Some(format!("{repo}/issues/{}", id.split_once('-')?.1));
    }
    Some(
        ctx.config
            .issue_tracker
            .as_ref()?
            .url_template
            .as_ref()?
            .replace("{id}", &id.to_ascii_uppercase()),
    )
}

fn repo_web_url(remote: &str) -> Option<String> {
    let raw = remote
        .strip_prefix("https://")
        .or_else(|| remote.strip_prefix("http://"))
        .or_else(|| remote.strip_prefix("git@"))
        .or_else(|| remote.strip_prefix("ssh://git@"))?;
    let (host, path) = if let Some((host, path)) = raw.split_once(':') {
        (host, path)
    } else {
        raw.split_once('/')?
    };
    let host = host.split('/').next()?;
    let path = path.trim_end_matches(".git").trim_end_matches('/');
    Some(format!("https://{host}/{path}"))
}

fn safe_pinned_stage(prefix: &str, path: &str) -> Option<String> {
    if prefix.is_empty() {
        return None;
    }
    let stage = std::fs::read_to_string(std::path::Path::new(path).join(".sst/stage")).ok()?;
    let stage = stage.trim();
    stage.starts_with(prefix).then(|| stage.to_owned())
}

fn removed_json(entry: &RemovedWorktree) -> Value {
    let work = entry.work.as_ref();
    let verify = work.and_then(|work| work.verify_after_merge.as_deref());
    let verification_owed = verify.is_some_and(|steps| {
        !steps.trim().is_empty()
            && !matches!(
                work.map(|work| work.state.as_str()),
                Some("verified" | "dropped")
            )
    });
    json!({
        "slug": entry.slug,
        "branch": entry.branch,
        "kind": if is_merged(entry) { "merged" } else { "removed" },
        "pr": entry.extra.get("prNumber").or_else(|| entry.extra.get("pr")),
        "pr_url": entry.extra.get("prUrl").or_else(|| entry.extra.get("pr_url")),
        "title": entry.extra.get("title"),
        "archived_at": entry.removed_at,
        "work_state": work.map(|work| work.state.as_str()),
        "verify_after_merge": verify,
        "verification_owed": verification_owed,
    })
}

fn is_merged(entry: &RemovedWorktree) -> bool {
    entry
        .extra
        .get("prState")
        .and_then(Value::as_str)
        .is_some_and(|state| state == "MERGED")
        || entry
            .extra
            .get("gitState")
            .and_then(Value::as_str)
            .is_some_and(|state| state == "merged")
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

#[cfg(test)]
mod tests {
    use super::{is_merged, removed_json};
    use serde_json::json;
    use wt_store::RemovedWorktree;

    #[test]
    fn removed_history_keeps_kind_and_verification_contract() {
        let row = RemovedWorktree {
            slug: "feature".into(),
            branch: "michael/feature".into(),
            removed_at: "2026-10-09T00:00:00Z".into(),
            work: Some(wt_store::WorkStatusRecord {
                state: "ready".into(),
                at: "2026-10-01T00:00:00Z".into(),
                verify_after_merge: Some("check staging".into()),
                ..serde_json::from_value(json!({"state":"ready","at":"2026-10-01T00:00:00Z"}))
                    .unwrap()
            }),
            automations_paused: None,
            extra: serde_json::Map::from_iter([("prState".into(), json!("MERGED"))]),
        };
        assert!(is_merged(&row));
        let json = removed_json(&row);
        assert_eq!(json["kind"], "merged");
        assert_eq!(json["verify_after_merge"], "check staging");
        assert_eq!(json["verification_owed"], true);
    }
}
