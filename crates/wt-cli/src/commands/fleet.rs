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
    /// verify_after_merge, verify_in), watches (id, by, when, then, met_at),
    /// edges, dirty, session, merge_queue, and pr (number,
    /// state, draft, checks, auto_merge, mergeable, merge_state, review_bot {state, unresolved,
    /// stale}, unresolved_threads, unresolved_human_threads). pr is null with
    /// pr_note set when GitHub is unreachable. pr_snapshot_age_seconds is the
    /// age of the events daemon snapshot the PR data came from, or null
    /// when GitHub was read live.
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
        let mut rows = wt_ops::worktree_facts::inventory_status(ctx).await?;
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

/// What `wt fleet` reports: one object per worktree, and why a source
/// that failed for all of them did.
struct Fleet {
    reports: Vec<Value>,
    pr_note: Option<String>,
    /// The events daemon snapshot the PR data came from, when it did.
    pr_snapshot: Option<PrSnapshot>,
    session_note: Option<String>,
}

/// What the report says of a snapshot it showed in place of a GitHub read.
#[derive(Clone, Debug)]
pub(super) struct PrSnapshot {
    age_ms: u64,
    /// Why data older than the board would use was shown.
    pub(super) held: Option<String>,
}

/// PR data for a report that only displays it: the events daemon's
/// snapshot while it covers every branch and is fresh or held back by
/// GitHub's budget (the rule `wt ls` follows), else one batched GitHub read.
pub(super) async fn pull_requests(
    ctx: &AppContext,
    branches: &[String],
) -> (
    Result<wt_github::GithubData, wt_github::GithubError>,
    Option<PrSnapshot>,
) {
    if let Some(shown) = wt_sources::github_events_source::load_for_display(ctx, branches).await {
        let snapshot = PrSnapshot {
            age_ms: shown.age_ms,
            held: shown.note(),
        };
        return (Ok(shown.read.data), Some(snapshot));
    }
    let github = GithubClient::new(
        ctx.processes.clone(),
        ctx.config.paths.main_clone.clone(),
        GithubOptions::from_config(&ctx.config, true),
    );
    (
        github.fetch_worktrees(branches, &ctx.cancellation).await,
        None,
    )
}

/// The fleet report, or the exit code after saying why there is none.
async fn fleet(ctx: &AppContext, args: &FleetArgs) -> Result<Result<Fleet, i32>> {
    let (rows, head_by_slug) = match select_rows(ctx, &args.slugs).await? {
        Ok(selected) => selected,
        Err(unknown) => {
            eprintln!(
                "no such worktree: {} (see `wt fleet` for the live slugs)",
                unknown.join(", ")
            );
            return Ok(Err(1));
        }
    };
    let selected = !args.slugs.is_empty();
    let branches = rows
        .iter()
        .map(|row| row.worktree.target.branch.clone())
        .collect::<Vec<_>>();
    let tmux = TmuxClient::new(
        ctx.processes.clone(),
        TmuxServer::named(ctx.config.tmux.socket.clone()).with_cwd(ctx.home.clone()),
    );
    let claude_paths = ClaudePaths::new(ctx.home.clone(), ctx.config.paths.cache_root.clone());
    let registry_dir = claude_paths.sessions_dir();
    let live_slugs = head_by_slug.keys().cloned().collect::<BTreeSet<_>>();
    // One batched GitHub read for exactly the reported branches; the other
    // probes do not depend on it, so none waits behind the slowest.
    let (stored, (fetched, pr_snapshot), sessions, registry, (dev, dev_note), locks) = tokio::join!(
        ctx.database.call(move |store| {
            let removed = if selected {
                Vec::new()
            } else {
                store.recently_removed_worktrees(&live_slugs, now_ms())?
            };
            Ok((store.read_wt_state()?, removed))
        }),
        pull_requests(ctx, &branches),
        tmux.list_sessions(&ctx.cancellation),
        tokio::task::spawn_blocking(move || read_claude_registry(&registry_dir)),
        dev_status(ctx, &rows),
        operation_locks(ctx, &rows),
    );
    let (mut state, reports_removed) = stored?;
    let registry = registry?;
    let records = rows
        .iter()
        .map(|row| row.worktree.clone())
        .collect::<Vec<_>>();
    // This command's own GitHub read, when it succeeded, proves PR landings.
    // A snapshot the budget is holding is shown but recorded nowhere.
    let held = pr_snapshot
        .as_ref()
        .is_some_and(|snapshot| snapshot.held.is_some());
    wt_ops::other_branches::refresh(
        ctx,
        &records,
        &mut state,
        fetched.as_ref().ok().filter(|_| !held),
        wt_ops::other_branches::Ask::Nothing,
    )
    .await;
    let landings = super::landing::read_with(ctx, &records, &state, fetched.as_ref().ok()).await;
    let separate_production = ctx.config.branch.separate_production();
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
    let watches = wt_store::stored_watches(&state);
    let clock = super::watch::clock();
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
            "unresolved_threads_more":pr.unresolved_threads_more,
            "review_bot":pr.review_bot.as_ref().map(|status| json!({"state":status.state,"unresolved":status.unresolved,"stale":status.stale.unwrap_or(false)})),
        }));
        let pr_queue = queue
            .as_ref()
            .and_then(|entries| entries.get(&row.worktree.target.branch))
            .map(queue_json);
        let owed = super::owed::derive(
            &state,
            &slug,
            landings
                .get(&slug)
                .copied()
                .unwrap_or(super::landing::RowLanding::known(wt_core::Landing::NONE)),
            clock.now_ms,
        );
        let report = json!({
            "kind":"live", "slug":slug, "branch":row.worktree.target.branch,
            "path":row.worktree.target.path, "head":row.worktree.head_sha,
            "base":worktree_state.and_then(|record| record.get("baseBranch")).and_then(Value::as_str).unwrap_or(&ctx.config.branch.base),
            "section":section, "work":work, "edges":edges,
            "other_prs":wt_core::other_prs_json(worktree_state.unwrap_or(&Value::Null)),
            "watches":super::watch::watches_json(&watches, &slug, clock),
            "owes":super::owed::owes_json(&owed.items, clock),
            "owed_phase":super::owed::phase_json(owed.phase),
            "landing":owed.landing.word(),
            "dirty":row.status.as_ref().map(|status| status.dirty),
            "session":{"alive":alive,"busy":busy,"last_activity":last_activity}, "session_note":session_note,
            "operation":operation,"operation_note":operation_note,
            "dev":{"status":dev_status,"error":dev_error,"note":dev_note,"slots":dev.as_ref().map(|snapshot| &snapshot.slots)},
            "pr":pr_json, "pr_note":pr_note,"merge_queue":pr_queue,
            "pr_snapshot_age_seconds":pr_snapshot.as_ref().map(|snapshot| snapshot.age_ms / 1000),
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
        let owed = super::owed::derive_removed(&entry, separate_production, clock.now_ms);
        reports.push(json!({"kind":if merged {"merged"} else {"removed"},"slug":entry.slug,"branch":entry.branch,"path":Value::Null,"head":Value::Null,"base":Value::Null,"section":Value::Null,"work":work,"dirty":Value::Null,"session":Value::Null,"session_note":Value::Null,"pr":entry.extra.get("prNumber").filter(|number| !number.is_null()).map(|number| json!({"number":number,"url":entry.extra.get("prUrl"),"title":entry.extra.get("title")})),"pr_note":Value::Null,"archived_at":entry.removed_at,"verify_in":entry.work.as_ref().and_then(|work| work.verify_after_merge.as_ref().map(|_| wt_core::VerifyStage::of_stored(work.verify_in.as_deref()).as_str())),"verify_after_merge":entry.work.as_ref().and_then(|work| work.verify_after_merge.as_deref()),"verification_owed":verification_owed,"owes":super::owed::owes_json(&owed.items, clock),"owed_phase":super::owed::phase_json(owed.phase),"landing":owed.landing.word()}));
    }
    Ok(Ok(Fleet {
        reports,
        pr_note,
        pr_snapshot,
        session_note,
    }))
}

pub async fn run(ctx: &AppContext, args: &FleetArgs) -> Result<i32> {
    let Fleet {
        reports,
        pr_note,
        pr_snapshot,
        session_note,
    } = match fleet(ctx, args).await? {
        Ok(fleet) => fleet,
        Err(code) => return Ok(code),
    };
    if args.json {
        println!("{}", serde_json::to_string_pretty(&reports)?);
    } else {
        println!(
            "SLUG                 BRANCH                         WORK          AGENT   DIRTY    PR"
        );
        for row in &reports {
            println!(
                "{:<20} {:<30} {:<13} {:<7} {:<8} {}",
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
                pr_cell(row)
            );
        }
        if reports.is_empty() {
            println!("no live worktrees");
        }
        // Report failed sources even when no live row carries them.
        if let Some(note) = &pr_note {
            println!("PR data unavailable: {note}");
        }
        match pr_snapshot {
            Some(PrSnapshot {
                held: Some(held), ..
            }) => println!("{held}"),
            Some(PrSnapshot { age_ms, held: None }) => println!(
                "PR data from the events daemon snapshot, {}s old",
                age_ms / 1000
            ),
            None => {}
        }
        if let Some(note) = &session_note {
            println!("Session data unavailable: {note}");
        }
    }
    Ok(0)
}

/// The human PR column: the row's own pull request, then how many of its
/// other branches hold, or may hold, one with no row (`other_prs` entries
/// that are `open` or `unknown`; the JSON names them).
fn pr_cell(row: &Value) -> String {
    let own = row["pr"]["number"]
        .as_u64()
        .map_or_else(|| "—".to_owned(), |number| format!("#{number}"));
    let others = row["other_prs"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|other| matches!(other["state"].as_str(), Some("open" | "unknown")))
        .count();
    match others {
        0 => own,
        1 => format!("{own} +1 PR"),
        count => format!("{own} +{count} PRs"),
    }
}

/// The human WORK column. A merge gate overrides the state (`blocked/ready`
/// must never read as mergeable). A landed row stands where what it owes
/// puts it: `due/<state>` or `waiting/<state>` until nothing is left, read
/// from the same `owed_phase` the JSON reports.
fn work_label(row: &Value) -> String {
    let work = &row["work"];
    let Some(state) = work["state"].as_str() else {
        return "—".into();
    };
    match row["owed_phase"].as_str() {
        Some(phase @ ("due" | "waiting")) => format!("{phase}/{state}"),
        // Landed with nothing left: its gate held back a merge that happened.
        Some(_) => state.to_owned(),
        None if work["blockedOn"].is_string() => format!("blocked/{state}"),
        None => state.to_owned(),
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

    /// The report shows PRs from the events daemon's snapshot while that is
    /// fresh and covers the branches, and says how old it is; only otherwise
    /// does it read GitHub.
    #[tokio::test]
    async fn pr_data_comes_from_a_fresh_events_snapshot_with_its_age() {
        let fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        let mut ctx = fixture.ctx.clone();
        let mut config = (*ctx.config).clone();
        config.github.events = Some(Default::default());
        ctx.config = std::sync::Arc::new(config);
        let events = ctx.config.paths.cache_root.join("events");
        fs::create_dir_all(&events).unwrap();
        let write_paced = |age_ms: i64, next_in_ms: i64, remaining: u64| {
            let written = now_ms() - age_ms;
            let snapshot = json!({
                "updatedAt": written, "fetchStartedAtMs": written - 1_000,
                "nextFetchAtMs": now_ms() + next_in_ms,
                "rateLimit": {"limit": 5000, "remaining": remaining, "cost": 5,
                    "resetAtMs": now_ms() + 30 * 60_000},
                "branches": ["feature/a", "feature/b"],
                "prs": {"feature/a": {
                    "number": 7, "url": "https://github.com/acme/app/pull/7",
                    "headRefName": "feature/a", "baseRefName": "main", "title": "A",
                    "isDraft": false, "state": "OPEN", "checks": "pass", "failedChecks": [],
                    "review": "none", "reviewRequests": 0, "requestedReviewers": [],
                    "suggestedReviewers": [], "autoMerge": null, "comments": [],
                    "unresolvedThreads": 0, "unresolvedThreadsTotal": 0,
                    "mergedAt": null, "closedAt": null
                }},
                "mergeQueue": {},
                "writerSha": wt_app_core::build_info::build_id(),
                "repository": wt_ops::events::repository_identity(&ctx.config),
            });
            fs::write(events.join("github.json"), snapshot.to_string()).unwrap();
        };
        // A daemon on its ordinary floor planned its next read long ago.
        let write = |age_ms: i64| write_paced(age_ms, 9_000 - age_ms, 4_000);
        write(12_000);
        let (read, shown) = pull_requests(&ctx, &["feature/a".into()]).await;
        assert_eq!(read.unwrap().prs["feature/a"].number, 7);
        let shown = shown.unwrap();
        assert!(matches!(shown.age_ms, 12_000..14_000), "{shown:?}");
        assert_eq!(shown.held, None);
        // A branch the snapshot does not cover, or a snapshot gone stale,
        // is a live read with no age. `gh` has nothing to answer here.
        let (_, shown) = pull_requests(&ctx, &["feature/new".into()]).await;
        assert!(shown.is_none());
        write(10 * 60_000);
        let (_, shown) = pull_requests(&ctx, &["feature/a".into()]).await;
        assert!(shown.is_none());
        // The same age while the daemon's budget holds its next read, or
        // once that read is overdue with only the reserve left, is shown
        // with the reason instead of spending the budget on a table.
        write_paced(10 * 60_000, 4 * 60_000, 1_400);
        let (read, shown) = pull_requests(&ctx, &["feature/a".into()]).await;
        assert_eq!(read.unwrap().prs["feature/a"].number, 7);
        assert_eq!(
            shown.unwrap().held.as_deref(),
            Some("GitHub reads are paced, using data 10 min old")
        );
        write_paced(10 * 60_000, -5 * 60_000, 900);
        let (_, shown) = pull_requests(&ctx, &["feature/a".into()]).await;
        assert_eq!(
            shown.unwrap().held.as_deref(),
            Some("GitHub budget low, using data 10 min old")
        );
        fixture.close().await.unwrap();
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
    fn the_pr_column_counts_other_prs_that_are_open_or_unanswered() {
        let other =
            |state: &str| json!({"branch": "b", "number": null, "state": state, "draft": false});
        assert_eq!(
            pr_cell(&json!({"pr": {"number": 7}, "other_prs": []})),
            "#7"
        );
        assert_eq!(pr_cell(&json!({"pr": null})), "—");
        assert_eq!(
            pr_cell(&json!({"pr": {"number": 7}, "other_prs": [other("open"), other("merged")]})),
            "#7 +1 PR"
        );
        assert_eq!(
            pr_cell(&json!({"pr": null, "other_prs": [
                other("open"), other("unknown"), other("closed"), other("none")
            ]})),
            "— +2 PRs"
        );
    }

    #[test]
    fn work_column_never_shows_a_gated_or_owing_row_as_plain_state() {
        let row = |work: Value, phase: Value| json!({"work":work,"owed_phase":phase});
        let ready = json!({"state":"ready"});
        assert_eq!(work_label(&row(ready.clone(), Value::Null)), "ready");
        let gated = json!({"state":"ready","blockedOn":"mobile release"});
        assert_eq!(
            work_label(&row(gated.clone(), Value::Null)),
            "blocked/ready"
        );
        // Landed: the phase says where the row stands, and a gate is moot.
        assert_eq!(work_label(&row(ready.clone(), json!("due"))), "due/ready");
        assert_eq!(
            work_label(&row(ready.clone(), json!("waiting"))),
            "waiting/ready"
        );
        assert_eq!(work_label(&row(gated, json!("done"))), "ready");
        assert_eq!(work_label(&row(ready, json!("done"))), "ready");
        assert_eq!(work_label(&json!({"work":null})), "—");
    }

    #[tokio::test]
    async fn every_row_reports_what_it_owes_and_its_phase() {
        let fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        let ctx = &fixture.ctx;
        let git = |args: Vec<String>| async move {
            let output = wt_ops::commands::resolve::run_git(ctx, &ctx.cwd, args)
                .await
                .unwrap();
            assert!(output.status.success());
            output.stdout_text().trim().to_owned()
        };
        // Land "one" where Git alone proves it: its commit is on the base
        // branch the origin advertises.
        let origin = fixture._root.path().join("origin.git");
        let origin = origin.to_str().unwrap().to_owned();
        git(vec!["init".into(), "--bare".into(), origin.clone()]).await;
        git(vec!["remote".into(), "add".into(), "origin".into(), origin]).await;
        let anchor = git(vec!["rev-parse".into(), "HEAD".into()]).await;
        std::fs::write(ctx.cwd.join("work.txt"), "landed work\n").unwrap();
        git(vec!["add".into(), "work.txt".into()]).await;
        git(vec!["commit".into(), "-m".into(), "feature work".into()]).await;
        git(vec![
            "push".into(),
            "origin".into(),
            "HEAD:refs/heads/main".into(),
        ])
        .await;
        let clock = super::super::watch::clock();
        ctx.database
            .call(move |store| {
                store.set_slug_base("one", Some(("main", Some(&anchor))))?;
                // Asserted now: a fixed date would be overdue two days on.
                let owing = wt_core::WorkStatusRecord::new(
                    wt_core::WorkState::Ready,
                    time::OffsetDateTime::now_utc()
                        .format(&time::format_description::well_known::Rfc3339)?,
                )
                .with_check(wt_core::VerifyStage::Staging, "1. open the page");
                let stored: wt_store::WorkStatusRecord =
                    serde_json::from_value(serde_json::to_value(&owing)?)?;
                store.set_slug_work_status("one", Some(&stored))?;
                store
                    .add_watch(
                        wt_store::NewWatch {
                            slug: "one".into(),
                            by: "one".into(),
                            utc_offset_minutes: 0,
                            when: vec![wt_core::watch::parse("production", clock).unwrap()],
                            then: wt_core::watch::WatchActions {
                                wake: Some("retire the flag".into()),
                                ..Default::default()
                            },
                            before_release: true,
                        },
                        clock.now_ms,
                    )?
                    .unwrap();
                Ok(())
            })
            .await
            .unwrap();
        let args = FleetArgs {
            slugs: vec!["one".into(), "two".into()],
            json: true,
        };
        let reports = fleet(ctx, &args).await.unwrap().unwrap().reports;
        let report = |slug: &str| {
            reports
                .iter()
                .find(|row| row["slug"] == slug)
                .unwrap_or_else(|| panic!("{slug} is reported"))
        };
        let one = report("one");
        assert_eq!(one["landing"], "landed");
        assert_eq!(one["owed_phase"], "due");
        let owes = one["owes"].as_array().unwrap();
        assert_eq!(owes.len(), 2, "{owes:?}");
        // Every item has every key, whatever its kind.
        for item in owes {
            let mut keys = item
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect::<Vec<_>>();
            keys.sort();
            assert_eq!(
                keys,
                [
                    "before_release",
                    "due_at",
                    "id",
                    "kind",
                    "overdue",
                    "stage",
                    "state",
                    "what",
                    "when"
                ]
            );
        }
        assert_eq!(
            owes[0],
            json!({
                "id": null, "kind": "check", "state": "due", "stage": "staging",
                "when": "due in staging", "what": "1. open the page",
                "before_release": false, "overdue": false, "due_at": null,
            })
        );
        assert_eq!(owes[1]["kind"], "step");
        assert_eq!(owes[1]["state"], "waiting");
        assert_eq!(owes[1]["stage"], "production");
        assert_eq!(owes[1]["what"], "retire the flag");
        assert_eq!(owes[1]["before_release"], true);
        assert!(owes[1]["id"].is_string());
        // The legacy keys still mirror the first check.
        assert_eq!(one["work"]["verify_in"], "staging");
        // A row that owes nothing reports an empty list. Its GitHub read
        // failed here, so its landing is unknown and it has no phase.
        let two = report("two");
        assert_eq!(two["owes"], json!([]));
        assert_eq!(two["landing"], "unknown");
        assert!(two["owed_phase"].is_null());
        fixture.close().await.unwrap();
    }
}
