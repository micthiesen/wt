use anyhow::Result;
use clap::Args;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::task::JoinSet;
use wt_dev::{DevStatusSnapshot, DevWorktree};
use wt_github::{GithubClient, GithubOptions, MergeQueueEntry};
use wt_harness::ClaudePaths;
use wt_tmux::{TmuxClient, TmuxServer};
use wt_vcs::{WorktreeRecord, WorktreeSnapshot};

use wt_app_core::context::AppContext;

#[derive(Debug, Clone, Args, Default)]
pub struct FleetArgs {
    /// Report only these worktrees, by slug or branch (default: every
    /// worktree, plus recently removed ones). The output is still a table or
    /// an array, and an unknown name exits 1.
    #[arg(value_name = "SLUG")]
    pub slugs: Vec<String>,
    /// Print one object per worktree: kind (live, merged, removed), slug,
    /// branch, path, base, section, work (state, risk, note, blocked_on,
    /// verify_after_merge, verify_in), edges, dirty, session, merge_queue, and pr (number,
    /// state, draft, checks, auto_merge, mergeable, merge_state, review_bot {state, unresolved,
    /// stale}, unresolved_threads, unresolved_human_threads). pr is null with
    /// pr_note set when GitHub is unreachable.
    #[arg(long)]
    pub json: bool,
}

type Heads = BTreeMap<String, Option<String>>;

/// The rows to report and every live worktree's head. Edges name worktrees
/// outside a selection, so their staleness needs all heads.
async fn select_rows(
    ctx: &AppContext,
    wanted: &[String],
) -> Result<Result<(Vec<WorktreeSnapshot>, Heads), Vec<String>>> {
    let heads = |records: &mut dyn Iterator<Item = &WorktreeRecord>| {
        records
            .map(|record| (record.target.slug().to_owned(), record.head_sha.clone()))
            .collect::<Heads>()
    };
    if wanted.is_empty() {
        let mut rows = ctx.repository.inventory_status(&ctx.cancellation).await?;
        rows.retain(|row| !row.worktree.is_main);
        let heads = heads(&mut rows.iter().map(|row| &row.worktree));
        return Ok(Ok((rows, heads)));
    }
    let mut records = ctx.repository.inventory(&ctx.cancellation).await?;
    records.retain(|record| !record.is_main);
    let heads = heads(&mut records.iter());
    // A name is a slug first: a branch another worktree carries under a
    // different slug must not answer for the worktree actually named.
    let slugs = records
        .iter()
        .map(|record| record.target.slug().to_owned())
        .collect::<std::collections::BTreeSet<_>>();
    let names = |record: &WorktreeRecord, name: &String| {
        record.target.slug() == name || (!slugs.contains(name) && record.target.branch == *name)
    };
    let unknown = wanted
        .iter()
        .filter(|name| !records.iter().any(|record| names(record, name)))
        .cloned()
        .collect::<Vec<_>>();
    if !unknown.is_empty() {
        return Ok(Err(unknown));
    }
    records.retain(|record| wanted.iter().any(|name| names(record, name)));
    let mut rows = Vec::with_capacity(records.len());
    for worktree in records {
        let (status, error) = match ctx.repository.status(&worktree, &ctx.cancellation).await {
            Ok(status) => (Some(status), None),
            Err(error) => (None, Some(error.to_string())),
        };
        rows.push(WorktreeSnapshot {
            worktree,
            status,
            error,
        });
    }
    Ok(Ok((rows, heads)))
}

async fn dev_status(
    ctx: &AppContext,
    rows: &[WorktreeSnapshot],
) -> (Option<DevStatusSnapshot>, Option<String>) {
    if ctx.config.dev_server.is_none() {
        return (None, Some("dev server is not configured".to_owned()));
    }
    let service = match wt_app_core::dev::service(ctx) {
        Ok(service) => service,
        Err(error) => return (None, Some(error.to_string())),
    };
    let inventory = rows
        .iter()
        .map(|row| DevWorktree::from(&row.worktree))
        .collect::<Vec<_>>();
    match service.status_all(&inventory, &ctx.cancellation).await {
        Ok(snapshot) => (Some(snapshot), None),
        Err(error) => (None, Some(error.to_string())),
    }
}

/// Each row's operation lock, or why it could not be read.
async fn operation_locks(
    ctx: &AppContext,
    rows: &[WorktreeSnapshot],
) -> BTreeMap<String, (Option<Value>, Option<String>)> {
    let mut tasks = JoinSet::new();
    for row in rows {
        let slug = row.worktree.target.slug().to_owned();
        let context = ctx.clone();
        tasks.spawn(async move {
            let lock = wt_ops::commands::diagnostics::operation_lock(&context, &slug).await;
            (slug, lock)
        });
    }
    let mut locks = BTreeMap::new();
    while let Some(Ok((slug, lock))) = tasks.join_next().await {
        locks.insert(
            slug,
            match lock {
                Ok(lock) => (
                    lock.map(|lock| serde_json::to_value(lock).unwrap_or(Value::Null)),
                    None,
                ),
                Err(error) => (None, Some(error.to_string())),
            },
        );
    }
    locks
}

/// `wt status --all --json` spells these keys in snake_case while the stored
/// record nested here is camelCase, and a key read in the wrong spelling is
/// `null`, which means "no gate". Both spellings are present so neither
/// reader is told a gated branch is mergeable.
fn add_snake_case_aliases(work: &mut serde_json::Map<String, Value>) {
    let present = |work: &serde_json::Map<String, Value>, key: &str| {
        work.get(key).filter(|value| !value.is_null()).cloned()
    };
    let steps = present(work, "verifyAfterMerge");
    let stage = steps
        .as_ref()
        .filter(|steps| steps.as_str().is_some_and(|steps| !steps.is_empty()))
        .map(|_| present(work, "verifyIn").unwrap_or_else(|| json!("staging")));
    work.insert(
        "blocked_on".into(),
        present(work, "blockedOn").unwrap_or(Value::Null),
    );
    work.insert("verify_after_merge".into(), steps.unwrap_or(Value::Null));
    work.insert("verify_in".into(), stage.unwrap_or(Value::Null));
}

pub async fn run(ctx: &AppContext, args: &FleetArgs) -> Result<i32> {
    let (rows, head_by_slug) = match select_rows(ctx, &args.slugs).await? {
        Ok(selected) => selected,
        Err(unknown) => {
            eprintln!(
                "no such worktree: {} (see `wt fleet` for the live slugs)",
                unknown.join(", ")
            );
            return Ok(1);
        }
    };
    let selected = !args.slugs.is_empty();
    let branches = rows
        .iter()
        .map(|row| row.worktree.target.branch.clone())
        .collect::<Vec<_>>();
    let github = GithubClient::new(
        ctx.processes.clone(),
        ctx.config.paths.main_clone.clone(),
        GithubOptions::from_config(&ctx.config, true),
    );
    let tmux = TmuxClient::new(
        ctx.processes.clone(),
        TmuxServer::named(ctx.config.tmux.socket.clone()).with_cwd(ctx.home.clone()),
    );
    let claude_paths = ClaudePaths::new(ctx.home.clone(), ctx.config.paths.cache_root.clone());
    let registry_dir = claude_paths.sessions_dir();
    let live_slugs = head_by_slug.keys().cloned().collect::<BTreeSet<_>>();
    // One batched GitHub read for exactly the reported branches; the other
    // probes do not depend on it, so none waits behind the slowest.
    let (stored, fetched, sessions, registry, (dev, dev_note), locks) = tokio::join!(
        ctx.database.call(move |store| {
            let removed = if selected {
                Vec::new()
            } else {
                store.recently_removed_worktrees(&live_slugs, now_ms())?
            };
            Ok((store.read_wt_state()?, removed))
        }),
        github.fetch_worktrees(&branches, &ctx.cancellation),
        tmux.list_sessions(&ctx.cancellation),
        tokio::task::spawn_blocking(move || read_claude_registry(&registry_dir)),
        dev_status(ctx, &rows),
        operation_locks(ctx, &rows),
    );
    let (state, reports_removed) = stored?;
    let registry = registry?;
    let (prs, queue, pr_note) = match fetched {
        Ok(data) => (Some(data.prs), Some(data.merge_queue), None),
        Err(error) => (None, None, Some(error.to_string())),
    };
    let (live_sessions, session_note) = match sessions {
        Ok(sessions) => (
            Some(
                sessions
                    .into_iter()
                    .map(|session| session.name)
                    .collect::<std::collections::HashSet<_>>(),
            ),
            None,
        ),
        Err(error) => (None, Some(error.to_string())),
    };
    let dev_by_slug = dev
        .as_ref()
        .map(|snapshot| {
            snapshot
                .worktrees
                .iter()
                .map(|row| (row.slug.as_str(), row))
                .collect::<std::collections::HashMap<_, _>>()
        })
        .unwrap_or_default();
    let mut reports = Vec::new();
    for row in rows {
        let slug = row.worktree.target.slug().to_owned();
        let worktree_state = state
            .get("slugs")
            .and_then(Value::as_object)
            .and_then(|slugs| slugs.get(&slug));
        let mut work = worktree_state
            .and_then(|record| record.get("work"))
            .cloned();
        if let Some(work) = work.as_mut().and_then(Value::as_object_mut) {
            // Attribution is explicitly nullable in the fleet JSON contract,
            // even when legacy durable records omitted the optional field.
            work.entry("by").or_insert(Value::Null);
            let stale = work
                .get("sha")
                .and_then(Value::as_str)
                .zip(row.worktree.head_sha.as_deref())
                .is_some_and(|(old, current)| old != current);
            work.insert("stale".into(), Value::Bool(stale));
            add_snake_case_aliases(work);
        }
        let section = worktree_state
            .and_then(|record| record.get("section"))
            .cloned()
            .unwrap_or(Value::Null);
        let pr = prs
            .as_ref()
            .and_then(|prs| prs.get(&row.worktree.target.branch));
        let session_names = live_sessions.as_ref();
        let live_claude = session_names.is_some_and(|sessions| {
            sessions
                .iter()
                .any(|name| name == &slug || name.starts_with(&format!("{slug}~")))
        });
        let alive = session_names.map(|sessions| {
            sessions.iter().any(|name| {
                name == &slug
                    || name.starts_with(&format!("{slug}~"))
                    || name == &format!("{slug}-codex")
                    || name == &format!("{slug}-opencode")
                    || name == &format!("{slug}-pi")
            })
        });
        let claude_activity = registry
            .iter()
            .filter(|entry| {
                entry.cwd == row.worktree.target.path
                    && (entry.name.as_deref() == Some(slug.as_str())
                        || entry.name.as_deref() == Some("primary")
                        || entry.name.is_none())
            })
            .max_by_key(|entry| entry.updated_at);
        let busy = if !live_claude {
            None
        } else {
            claude_activity.map(|entry| matches!(entry.status.as_str(), "busy" | "shell"))
        };
        let last_activity = claude_activity.and_then(|entry| format_timestamp_ms(entry.updated_at));
        let (operation, operation_note) = locks.get(&slug).cloned().unwrap_or_else(|| {
            (
                None,
                Some("operation lock could not be inspected".to_owned()),
            )
        });
        let dev_row = dev_by_slug.get(slug.as_str());
        let dev_status = dev_row
            .and_then(|row| row.status.as_ref())
            .map(|status| serde_json::to_value(status).unwrap_or(Value::Null));
        let dev_error = dev_row.and_then(|row| row.error.as_deref());
        let edges = state
            .get("edges")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter(|edge| {
                edge.get("from").and_then(Value::as_str) == Some(&slug)
                    || edge.get("to").and_then(Value::as_str) == Some(&slug)
            })
            .map(|edge| {
                let from = edge.get("from").and_then(Value::as_str).unwrap_or_default();
                let to = edge.get("to").and_then(Value::as_str).unwrap_or_default();
                let from_sha = edge.get("fromSha").and_then(Value::as_str);
                let to_sha = edge.get("toSha").and_then(Value::as_str);
                let stale = from_sha
                    .zip(head_by_slug.get(from).and_then(Option::as_deref))
                    .is_some_and(|(expected, actual)| expected != actual)
                    || to_sha
                        .zip(head_by_slug.get(to).and_then(Option::as_deref))
                        .is_some_and(|(expected, actual)| expected != actual)
                    || from_sha.is_none()
                    || to_sha.is_none();
                let mut edge = edge.clone();
                if let Some(object) = edge.as_object_mut() {
                    object.insert("stale".into(), Value::Bool(stale));
                }
                edge
            })
            .collect::<Vec<_>>();
        let pr_json = pr.map(|pr| json!({
            "number":pr.number,"url":pr.url,"title":pr.title,"state":pr.state,"isDraft":pr.is_draft,
            "draft":pr.is_draft,
            "checks":pr.checks,"failedChecks":pr.failed_checks,
            "auto_merge":pr.auto_merge.is_some(),
            "mergeStateStatus":merge_field(pr.merge_state_status.as_deref(), &pr.state),
            "merge_state":merge_field(pr.merge_state_status.as_deref(), &pr.state),
            "mergeable":merge_field(pr.mergeable.as_deref(), &pr.state),
            "unresolved_threads":pr.unresolved_threads_total,"unresolved_human_threads":pr.unresolved_threads,
            "review_bot":pr.review_bot.as_ref().map(|status| json!({"state":status.state,"unresolved":status.unresolved,"stale":status.stale.unwrap_or(false)})),
        }));
        let pr_queue = queue
            .as_ref()
            .and_then(|entries| entries.get(&row.worktree.target.branch))
            .map(queue_json);
        let report = json!({
            "kind":"live", "slug":slug, "branch":row.worktree.target.branch,
            "path":row.worktree.target.path, "head":row.worktree.head_sha,
            "base":worktree_state.and_then(|record| record.get("baseBranch")).and_then(Value::as_str).unwrap_or(&ctx.config.branch.base),
            "section":section, "work":work, "edges":edges,
            "dirty":row.status.as_ref().map(|status| status.dirty),
            "session":{"alive":alive,"busy":busy,"last_activity":last_activity}, "session_note":session_note,
            "operation":operation,"operation_note":operation_note,
            "dev":{"status":dev_status,"error":dev_error,"note":dev_note,"slots":dev.as_ref().map(|snapshot| &snapshot.slots)},
            "pr":pr_json, "pr_note":pr_note,"merge_queue":pr_queue,
            "inventory_error":row.error
        });
        reports.push(report);
    }
    for entry in reports_removed {
        let merged = wt_store::is_merged_removal(&entry);
        let mut work = entry.work.as_ref().map(serde_json::to_value).transpose()?;
        if let Some(work) = work.as_mut().and_then(Value::as_object_mut) {
            add_snake_case_aliases(work);
        }
        let verification_owed = wt_store::verification_owed_at_removal(&entry);
        reports.push(json!({"kind":if merged {"merged"} else {"removed"},"slug":entry.slug,"branch":entry.branch,"path":Value::Null,"head":Value::Null,"base":Value::Null,"section":Value::Null,"work":work,"dirty":Value::Null,"session":Value::Null,"session_note":Value::Null,"pr":entry.extra.get("prNumber").filter(|number| !number.is_null()).map(|number| json!({"number":number,"url":entry.extra.get("prUrl"),"title":entry.extra.get("title")})),"pr_note":Value::Null,"archived_at":entry.removed_at,"verify_in":entry.work.as_ref().and_then(|work| work.verify_after_merge.as_ref().map(|_| work.verify_in.clone().unwrap_or_else(|| "staging".into()))),"verify_after_merge":entry.work.and_then(|work| work.verify_after_merge),"verification_owed":verification_owed}));
    }
    if args.json {
        println!("{}", serde_json::to_string_pretty(&reports)?);
    } else {
        println!(
            "SLUG                 BRANCH                         WORK       AGENT   DIRTY    PR"
        );
        for row in &reports {
            println!(
                "{:<20} {:<30} {:<10} {:<7} {:<8} {}",
                row["slug"].as_str().unwrap_or("?"),
                row["branch"].as_str().unwrap_or("?"),
                work_label(row),
                row["session"]["alive"]
                    .as_bool()
                    .map(|v| if v { "live" } else { "—" })
                    .unwrap_or("unknown"),
                row["dirty"]
                    .as_bool()
                    .map(|v| if v { "dirty" } else { "clean" })
                    .unwrap_or("unknown"),
                row["pr"]["number"]
                    .as_u64()
                    .map(|n| format!("#{n}"))
                    .unwrap_or_else(|| "—".into())
            );
        }
        if reports.is_empty() {
            println!("no live worktrees");
        }
        // Report failed sources even when no live row carries them.
        if let Some(note) = &pr_note {
            println!("PR data unavailable: {note}");
        }
        if let Some(note) = &session_note {
            println!("Session data unavailable: {note}");
        }
    }
    Ok(0)
}

/// The human WORK column. A merge gate overrides the state (`blocked/ready`
/// must never read as mergeable), and a landed row still owing its
/// post-merge check reads `unverified/<state>`.
fn work_label(row: &Value) -> String {
    let work = &row["work"];
    let Some(state) = work["state"].as_str() else {
        return "—".into();
    };
    let landed = row["kind"] == "merged" || row["pr"]["state"] == "MERGED";
    let owed = row["verification_owed"].as_bool().unwrap_or_else(|| {
        landed
            && work["verifyAfterMerge"]
                .as_str()
                .is_some_and(|steps| !steps.is_empty())
            && !matches!(state, "verified" | "dropped")
    });
    if work["blockedOn"].is_string() {
        format!("blocked/{state}")
    } else if owed {
        format!("unverified/{state}")
    } else {
        state.to_owned()
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

fn format_timestamp_ms(timestamp_ms: i64) -> Option<String> {
    if timestamp_ms <= 0 {
        return None;
    }
    OffsetDateTime::from_unix_timestamp_nanos(timestamp_ms as i128 * 1_000_000)
        .ok()?
        .format(&Rfc3339)
        .ok()
}

fn merge_field(value: Option<&str>, state: &str) -> Option<String> {
    if state != "OPEN" {
        return None;
    }
    value.map(|value| {
        if value == "UNKNOWN" {
            "computing".to_owned()
        } else {
            value.to_ascii_lowercase()
        }
    })
}

fn queue_json(entry: &MergeQueueEntry) -> Value {
    serde_json::to_value(entry).unwrap_or(Value::Null)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaudeActivity {
    pid: u32,
    cwd: String,
    name: Option<String>,
    status: String,
    updated_at: i64,
}

fn read_claude_registry(directory: &Path) -> Vec<ClaudeActivity> {
    let Ok(entries) = fs::read_dir(directory) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            if entry
                .path()
                .extension()
                .is_none_or(|extension| extension != "json")
            {
                return None;
            }
            let value: ClaudeActivity =
                serde_json::from_slice(&fs::read(entry.path()).ok()?).ok()?;
            process_is_alive(value.pid).then_some(value)
        })
        .collect()
}

fn process_is_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(unix)]
    {
        // SAFETY: signal zero only checks whether a positive pid exists.
        let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
        result == 0
            || std::io::Error::last_os_error().kind() == std::io::ErrorKind::PermissionDenied
    }
    #[cfg(not(unix))]
    {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn work_block_answers_in_both_key_spellings() {
        let mut gated =
            json!({"state":"ready","blockedOn":"mobile release","verifyAfterMerge":"1. check"});
        add_snake_case_aliases(gated.as_object_mut().unwrap());
        assert_eq!(gated["blocked_on"], "mobile release");
        assert_eq!(gated["blockedOn"], "mobile release");
        assert_eq!(gated["verify_after_merge"], "1. check");
        // A record written before stages existed comes due in staging.
        assert_eq!(gated["verify_in"], "staging");
        let mut production =
            json!({"state":"ready","verifyAfterMerge":"1. check","verifyIn":"production"});
        add_snake_case_aliases(production.as_object_mut().unwrap());
        assert_eq!(production["verify_in"], "production");
        let mut plain = json!({"state":"working"});
        add_snake_case_aliases(plain.as_object_mut().unwrap());
        for key in ["blocked_on", "verify_after_merge", "verify_in"] {
            assert!(plain.as_object().unwrap().contains_key(key), "{key}");
            assert!(plain[key].is_null(), "{key}");
        }
    }

    #[tokio::test]
    async fn named_rows_are_selected_before_any_status_is_read() {
        let fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        let (rows, heads) = select_rows(&fixture.ctx, &["one".into()])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].worktree.target.slug(), "one");
        assert!(rows[0].status.is_some());
        // Edges to unselected worktrees still need their heads.
        assert!(heads.contains_key("two"), "{heads:?}");
        let by_branch = select_rows(&fixture.ctx, &[rows[0].worktree.target.branch.clone()])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(by_branch.0.len(), 1);
        let (all, _) = select_rows(&fixture.ctx, &[]).await.unwrap().unwrap();
        assert!(all.len() >= 2 && all.iter().all(|row| !row.worktree.is_main));
        assert_eq!(
            select_rows(&fixture.ctx, &["one".into(), "absent".into()])
                .await
                .unwrap()
                .unwrap_err(),
            vec!["absent".to_owned()]
        );
        assert_eq!(
            run(
                &fixture.ctx,
                &FleetArgs {
                    slugs: vec!["absent".into()],
                    json: true
                }
            )
            .await
            .unwrap(),
            1
        );
        fixture.close().await.unwrap();
    }

    #[test]
    fn work_column_never_shows_a_gated_or_unverified_row_as_plain_state() {
        let row = |work: Value, pr: Value, kind: &str| json!({"kind":kind,"work":work,"pr":pr});
        let ready = json!({"state":"ready"});
        assert_eq!(
            work_label(&row(ready.clone(), Value::Null, "live")),
            "ready"
        );
        assert_eq!(
            work_label(&row(
                json!({"state":"ready","blockedOn":"mobile release"}),
                Value::Null,
                "live"
            )),
            "blocked/ready"
        );
        let owing = json!({"state":"ready","verifyAfterMerge":"1. check prod"});
        assert_eq!(
            work_label(&row(owing.clone(), json!({"state":"OPEN"}), "live")),
            "ready"
        );
        assert_eq!(
            work_label(&row(owing, json!({"state":"MERGED"}), "live")),
            "unverified/ready"
        );
        let mut removed = row(ready, Value::Null, "merged");
        removed["verification_owed"] = json!(true);
        assert_eq!(work_label(&removed), "unverified/ready");
        assert_eq!(work_label(&json!({"work":null})), "—");
    }
}
