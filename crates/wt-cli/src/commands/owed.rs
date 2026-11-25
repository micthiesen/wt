//! What a worktree owes: the list every command prints, `wt owed` to read
//! it and `wt done` to close what is due. The list itself is derived by
//! wt_core::owed; nothing here decides what a row owes.

use std::collections::BTreeSet;

use anyhow::{Context, Result};
use clap::Args;
use serde_json::{Value, json};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use wt_app_core::context::AppContext;
use wt_core::{
    Landing, OwedItem, OwedKind, OwedPhase, OwedState, VerifyStage, WorkState, WorkStatusRecord,
    watch::{Condition, LocalClock},
};
use wt_ops::commands::resolve::resolve_worktree;
use wt_store::{CheckTargets, ChecksClosed, RemovedChecksClosed, RemovedWorktree};
use wt_vcs::WorktreeRecord;

use super::watch::{agent, author, clock};

/// One row's owed list, as every surface of the CLI shows it.
pub(crate) struct RowOwed {
    pub items: Vec<OwedItem>,
    /// None while the row's own work has not landed.
    pub phase: Option<OwedPhase>,
    /// Watches that only tell the person: not owed by the row.
    pub tells: usize,
}

/// What the live worktree `slug` owes under `landing`, from one state
/// snapshot.
pub(crate) fn derive(state: &Value, slug: &str, landing: Landing, now_ms: i64) -> RowOwed {
    let stored = &state["slugs"][slug];
    let record = wt_core::parse_work_status(&stored["work"]);
    let created_at = stored["createdAt"].as_str();
    let watches = wt_core::owed::row_watches(&wt_store::stored_watches(state), slug, created_at);
    let due = wt_core::owed::row_due(&wt_store::stored_owed(state), slug, created_at);
    let items = wt_core::owed_items(record.as_ref(), &landing, &watches, &due, now_ms);
    RowOwed {
        phase: wt_core::owed_phase(&items, landing.base),
        tells: watches
            .iter()
            .filter(|watch| {
                watch.then.notify.is_some()
                    && watch.then.wake.is_none()
                    && watch.then.unblock.is_none()
            })
            .count(),
        items,
    }
}

/// What a removed row still owes: the checks and due steps kept on its
/// removal record. Its deploys can no longer be read, so a check stands by
/// the landing recorded when the row was removed.
pub(crate) fn derive_removed(
    entry: &RemovedWorktree,
    separate_production: bool,
    now_ms: i64,
) -> RowOwed {
    let landed = wt_store::is_merged_removal(entry);
    let landing = Landing::new(landed, false, separate_production);
    let record = entry.work_with_checks().and_then(|work| work.to_core());
    let items = wt_core::owed_items(record.as_ref(), &landing, &[], &entry.owed(), now_ms);
    RowOwed {
        phase: wt_core::owed_phase(&items, landed),
        tells: 0,
        items,
    }
}

fn iso(ms: i64) -> Option<String> {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000)
        .ok()?
        .format(&Rfc3339)
        .ok()
}

fn now_iso() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".into())
}

/// The "owes" array of the JSON surfaces.
pub(crate) fn owes_json(items: &[OwedItem], clock: LocalClock) -> Value {
    items
        .iter()
        .map(|item| {
            json!({
                "id": item.id,
                "kind": item.kind.as_str(),
                "state": item.state.as_str(),
                "stage": item.stage.map(VerifyStage::as_str),
                "when": item.when_words(clock),
                "what": item.what,
                "before_release": item.before_release,
                "overdue": item.overdue,
                "due_at": item.due_at_ms.and_then(iso),
            })
        })
        .collect()
}

/// The "owed_phase" value of the JSON surfaces: due, waiting, done or null.
pub(crate) fn phase_json(phase: Option<OwedPhase>) -> Value {
    json!(phase.map(OwedPhase::as_str))
}

fn state_word(item: &OwedItem) -> &'static str {
    match (item.kind, item.state) {
        (_, OwedState::Due) if item.overdue => "overdue",
        (_, OwedState::Due) => "due",
        (OwedKind::Gate, _) => "gate",
        (_, OwedState::Waiting) => "waiting",
    }
}

/// One line of the list: state, headline, when, and what closes it.
fn item_line(item: &OwedItem, slug: &str, clock: LocalClock) -> String {
    let mut line = format!(
        "{:<8} {} ({})",
        state_word(item),
        wt_core::sanitize_terminal_text(&item.headline()),
        item.when_words(clock)
    );
    if item.before_release {
        line.push_str(" · before release");
    }
    if item.unconfirmed {
        line.push_str(" · wake sent, delivery not confirmed");
    }
    if let Some(by) = item.by.as_deref().filter(|by| *by != slug) {
        line.push_str(&format!(" · by {by}"));
    }
    if item.state == OwedState::Due
        && let Some(target) = item.done_target()
    {
        line.push_str(&format!(" · wt done {target}"));
    }
    line
}

/// The list as every list-changing command prints it. `who` is "This
/// worktree" or the slug of another one.
pub(crate) fn owed_text(who: &str, slug: &str, owed: &RowOwed, clock: LocalClock) -> String {
    if owed.items.is_empty() {
        return format!("{who} owes nothing.");
    }
    let mut text = format!("{who} owes:");
    for item in &owed.items {
        text.push_str(&format!("\n  {}", item_line(item, slug, clock)));
    }
    text
}

/// Whether the command runs inside this worktree: the list then reads
/// "This worktree owes".
async fn is_own(ctx: &AppContext, record: &WorktreeRecord) -> bool {
    resolve_worktree(ctx, None)
        .await
        .is_ok_and(|own| !own.is_main && own.target.slug() == record.target.slug())
}

/// How the list names the worktree it is about.
pub(crate) async fn who(ctx: &AppContext, record: &WorktreeRecord) -> String {
    if is_own(ctx, record).await {
        "This worktree".to_owned()
    } else {
        record.target.slug().to_owned()
    }
}

async fn landing_of(ctx: &AppContext, record: &WorktreeRecord, state: &Value) -> Landing {
    wt_ops::lifecycle_ops::row_landings(ctx, std::slice::from_ref(record), state, None)
        .await
        .remove(record.target.slug())
        .unwrap_or(Landing::NONE)
}

/// What the live worktree owes right now, read fresh.
pub(crate) async fn read(ctx: &AppContext, record: &WorktreeRecord) -> Result<RowOwed> {
    let state = ctx
        .database
        .call(|store| Ok(store.read_wt_state()?))
        .await?;
    let landing = landing_of(ctx, record, &state).await;
    Ok(derive(
        &state,
        record.target.slug(),
        landing,
        super::watch::now_ms(),
    ))
}

/// Print what `slug` owes: how every command that changed the list ends.
/// A worktree that is gone, or a list that cannot be read, prints nothing:
/// the command's own result stands.
pub(crate) async fn print_list(ctx: &AppContext, slug: &str) {
    let Ok(rows) = ctx.repository.inventory(&ctx.cancellation).await else {
        return;
    };
    let Some(record) = rows
        .into_iter()
        .find(|row| !row.is_main && row.target.slug() == slug)
    else {
        return;
    };
    print_record_list(ctx, &record).await;
}

pub(crate) async fn print_record_list(ctx: &AppContext, record: &WorktreeRecord) {
    let Ok(owed) = read(ctx, record).await else {
        return;
    };
    let slug = record.target.slug();
    let who = who(ctx, record).await;
    println!("{}", owed_text(&who, slug, &owed, clock()));
}

#[derive(Debug, Clone, Args)]
pub struct OwedArgs {
    /// Every worktree's list, grouped: due now, before the next production
    /// release, then waiting by what it waits for.
    #[arg(long)]
    pub all: bool,
    /// Print JSON: an array of items (id, kind, state, stage, when, what,
    /// before_release, overdue, due_at), or with --all an array of
    /// worktrees (slug, kind, owed_phase, owes).
    #[arg(long)]
    pub json: bool,
    /// The worktree whose list to show (default: the current one). A
    /// removed worktree's slug shows what its record still owes.
    #[arg(long, value_name = "SLUG", conflicts_with = "all")]
    pub slug: Option<String>,
}

async fn removed_entries(ctx: &AppContext, live: BTreeSet<String>) -> Result<Vec<RemovedWorktree>> {
    ctx.database
        .call(move |store| {
            let mut removed = store.read_removed_worktrees()?;
            removed.retain(|entry| !live.contains(&entry.slug));
            Ok(removed)
        })
        .await
}

async fn removed_entry(ctx: &AppContext, slug: &str) -> Result<Option<RemovedWorktree>> {
    let wanted = slug.to_owned();
    Ok(removed_entries(ctx, BTreeSet::new())
        .await?
        .into_iter()
        .find(|entry| entry.slug == wanted))
}

pub async fn run_owed(ctx: &AppContext, args: &OwedArgs) -> Result<i32> {
    let clock = clock();
    // Outside a worktree there is no "this worktree": show them all.
    let record = if args.all {
        None
    } else {
        match resolve_worktree(ctx, args.slug.as_deref()).await {
            Ok(record) if !record.is_main => Some(record),
            Ok(_) | Err(_) if args.slug.is_none() => None,
            Ok(_) => {
                eprintln!(
                    "the main clone is not a worktree and owes nothing: pass a worktree's slug"
                );
                return Ok(1);
            }
            Err(error) => {
                let slug = args.slug.as_deref().unwrap_or_default();
                let Some(entry) = removed_entry(ctx, slug).await? else {
                    eprintln!("{error}");
                    return Ok(1);
                };
                let owed = derive_removed(
                    &entry,
                    ctx.config.branch.separate_production(),
                    clock.now_ms,
                );
                if args.json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&owes_json(&owed.items, clock))?
                    );
                } else {
                    println!(
                        "{}",
                        owed_text(&format!("{slug} (removed)"), slug, &owed, clock)
                    );
                }
                return Ok(0);
            }
        }
    };
    if let Some(record) = record {
        let owed = read(ctx, &record).await?;
        let slug = record.target.slug();
        if args.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&owes_json(&owed.items, clock))?
            );
            return Ok(0);
        }
        let who = who(ctx, &record).await;
        println!("{}", owed_text(&who, slug, &owed, clock));
        if owed.tells > 0 {
            println!(
                "  {} watch{} only tell{} you and {} not owed (wt watch lists them)",
                owed.tells,
                if owed.tells == 1 { "" } else { "es" },
                if owed.tells == 1 { "s" } else { "" },
                if owed.tells == 1 { "is" } else { "are" },
            );
        }
        return Ok(0);
    }
    let rows = ctx.repository.inventory(&ctx.cancellation).await?;
    let rows = rows
        .into_iter()
        .filter(|row| !row.is_main)
        .collect::<Vec<_>>();
    let state = ctx
        .database
        .call(|store| Ok(store.read_wt_state()?))
        .await?;
    let landings = wt_ops::lifecycle_ops::row_landings(ctx, &rows, &state, None).await;
    let mut fleet = rows
        .iter()
        .map(|row| {
            let slug = row.target.slug();
            let landing = landings.get(slug).copied().unwrap_or(Landing::NONE);
            (
                slug.to_owned(),
                "live",
                derive(&state, slug, landing, clock.now_ms),
            )
        })
        .collect::<Vec<_>>();
    let live = fleet.iter().map(|(slug, ..)| slug.clone()).collect();
    for entry in removed_entries(ctx, live).await? {
        let owed = derive_removed(
            &entry,
            ctx.config.branch.separate_production(),
            clock.now_ms,
        );
        if !owed.items.is_empty() {
            fleet.push((entry.slug.clone(), "removed", owed));
        }
    }
    if args.json {
        let listing = fleet
            .iter()
            .map(|(slug, kind, owed)| {
                json!({
                    "slug": slug,
                    "kind": kind,
                    "owed_phase": phase_json(owed.phase),
                    "owes": owes_json(&owed.items, clock),
                })
            })
            .collect::<Vec<_>>();
        println!("{}", serde_json::to_string_pretty(&listing)?);
        return Ok(0);
    }
    let text = fleet_text(&fleet, clock);
    if text.is_empty() {
        println!("no worktree owes anything");
    } else {
        println!("{text}");
    }
    Ok(0)
}

/// The group an item is listed under in the fleet view.
fn group(item: &OwedItem) -> usize {
    match (item.state, item.before_release, item.stage) {
        (OwedState::Due, ..) => 0,
        (_, true, _) => 1,
        (_, _, Some(VerifyStage::Staging)) => 2,
        (_, _, Some(VerifyStage::Production)) => 3,
        _ => 4,
    }
}

const GROUPS: [&str; 5] = [
    "Due now",
    "Before the next production release",
    "Waiting for staging",
    "Waiting for production",
    "Waiting",
];

fn fleet_text(fleet: &[(String, &str, RowOwed)], clock: LocalClock) -> String {
    let width = fleet
        .iter()
        .filter(|(.., owed)| !owed.items.is_empty())
        .map(|(slug, ..)| slug.chars().count())
        .max()
        .unwrap_or(0);
    let mut sections = Vec::new();
    for (index, title) in GROUPS.iter().enumerate() {
        let lines = fleet
            .iter()
            .flat_map(|(slug, kind, owed)| {
                owed.items
                    .iter()
                    .filter(move |item| group(item) == index)
                    .map(move |item| {
                        let mut line =
                            format!("  {slug:<width$}  {}", item_line(item, slug, clock));
                        if *kind == "removed" {
                            line.push_str(" · removed row");
                        }
                        // Away from the worktree, closing takes its slug.
                        if line.contains(" · wt done ") {
                            line.push_str(&format!(" --slug {slug}"));
                        }
                        line
                    })
            })
            .collect::<Vec<_>>();
        if !lines.is_empty() {
            sections.push(format!("{title}\n{}", lines.join("\n")));
        }
    }
    sections.join("\n\n")
}

#[derive(Debug, Clone, Args)]
pub struct DoneArgs {
    /// What to close: a due step's id, or staging / production for the
    /// check owed there (wt owed lists both). Default: the one item that is
    /// due.
    #[arg(value_name = "ID_OR_STAGE")]
    pub targets: Vec<String>,
    /// What you did and what you saw. It is the evidence kept in the
    /// worktree's history, and required to close anything.
    #[arg(
        short = 'm',
        long = "message",
        value_name = "EVIDENCE",
        allow_hyphen_values = true
    )]
    pub message: Option<String>,
    /// Put it off instead of closing it; give this last. A step becomes a
    /// wake again with the same note and these conditions (as wt wake
    /// reads them). A check takes one time (3h, 17:00, "tomorrow 09:00"):
    /// it stays owed and is not due before then.
    #[arg(long, value_name = "CONDITION", num_args = 1..)]
    pub again: Vec<String>,
    /// The worktree that owes it (default: the current one). A removed
    /// worktree's slug closes what its record still owes.
    #[arg(long, value_name = "SLUG")]
    pub slug: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Target {
    Check(VerifyStage),
    Step(String),
}

impl Target {
    fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "staging" => Self::Check(VerifyStage::Staging),
            "production" => Self::Check(VerifyStage::Production),
            _ => Self::Step(raw.trim().to_owned()),
        }
    }

    fn of(item: &OwedItem) -> Option<Self> {
        item.done_target().map(|target| Self::parse(&target))
    }
}

/// The verified status that closing a row's last check asserts: the same
/// record `wt status verified -m` writes.
pub(crate) fn verified_record(
    head: Option<&str>,
    note: Option<String>,
) -> Result<wt_store::WorkStatusRecord> {
    let mut verified = WorkStatusRecord::new(WorkState::Verified, now_iso());
    verified.sha = head.map(str::to_owned);
    verified.by = agent();
    verified.note = note;
    serde_json::from_value(serde_json::to_value(&verified)?).context("encode status record")
}

fn stage_names(checks: &[(VerifyStage, wt_core::Check)]) -> String {
    checks
        .iter()
        .map(|(stage, _)| stage.as_str())
        .collect::<Vec<_>>()
        .join(" and ")
}

/// What closing checks did, as wt status verified and wt done both say it.
pub(crate) fn print_checks_closed(
    slug: &str,
    closed: &[(VerifyStage, wt_core::Check)],
    remaining: &[(VerifyStage, wt_core::Check)],
    kept_state: Option<&str>,
    removed: bool,
) {
    let row = if removed { " (removed row)" } else { "" };
    for (stage, check) in closed {
        println!(
            "✓ {slug} closed the check in {}{row}: {}",
            stage.as_str(),
            wt_app_core::watch_view::headline(&check.steps)
        );
    }
    if remaining.is_empty() {
        println!("✓ {slug} verified{row}");
    } else {
        println!(
            "  still owes the check in {}{}",
            stage_names(remaining),
            kept_state
                .map(|state| format!(": the status stays {state} until it is closed"))
                .unwrap_or_default()
        );
    }
}

pub async fn run_done(ctx: &AppContext, args: &DoneArgs) -> Result<i32> {
    let again = !args.again.is_empty();
    let evidence = args
        .message
        .as_deref()
        .map(wt_core::sanitize_work_note)
        .filter(|text| !text.is_empty());
    if again && args.message.is_some() {
        eprintln!(
            "--again puts it off instead of closing it, so there is no evidence to record: drop -m"
        );
        return Ok(2);
    }
    if !again && evidence.is_none() {
        eprintln!(
            "-m needs what you did and what you saw: wt done <id|staging|production> -m \"<evidence>\" (to put it off instead: --again <condition>)"
        );
        return Ok(2);
    }
    let clock = clock();
    let record = match resolve_worktree(ctx, args.slug.as_deref()).await {
        Ok(record) if !record.is_main => Some(record),
        Ok(_) => {
            eprintln!("the main clone is not a worktree and owes nothing: pass --slug <slug>");
            return Ok(1);
        }
        Err(error) => {
            let Some(slug) = args.slug.as_deref() else {
                eprintln!("not inside a worktree: run this in one, or pass --slug <slug>");
                return Ok(1);
            };
            // A removed row keeps what it owed: it is closed on its record.
            if removed_entry(ctx, slug).await?.is_none() {
                eprintln!("{error}");
                return Ok(1);
            }
            None
        }
    };
    let slug = match &record {
        Some(record) => record.target.slug().to_owned(),
        None => args.slug.clone().unwrap_or_default(),
    };
    let owed = match &record {
        Some(record) => read(ctx, record).await?,
        None => {
            let entry = removed_entry(ctx, &slug)
                .await?
                .context("the removal record disappeared")?;
            derive_removed(
                &entry,
                ctx.config.branch.separate_production(),
                clock.now_ms,
            )
        }
    };
    let who = match &record {
        Some(record) => who(ctx, record).await,
        None => format!("{slug} (removed)"),
    };
    let targets = if args.targets.is_empty() {
        // A removed row's deploys cannot be read: every check it still
        // owes can be closed.
        let closable = owed
            .items
            .iter()
            .filter(|item| {
                item.state == OwedState::Due || (record.is_none() && item.kind == OwedKind::Check)
            })
            .filter_map(Target::of)
            .collect::<Vec<_>>();
        match closable.as_slice() {
            [only] => vec![only.clone()],
            [] => {
                eprintln!(
                    "nothing {slug} owes is due. Name what to close: a check by its stage (wt done staging -m ...), a step by its id once it is due."
                );
                println!("{}", owed_text(&who, &slug, &owed, clock));
                return Ok(1);
            }
            _ => {
                eprintln!("{slug} has several things due: name the ones to close.");
                println!("{}", owed_text(&who, &slug, &owed, clock));
                return Ok(1);
            }
        }
    } else {
        let mut targets = Vec::new();
        for target in args.targets.iter().map(|raw| Target::parse(raw)) {
            if !targets.contains(&target) {
                targets.push(target);
            }
        }
        targets
    };
    let failed = if again {
        put_off(ctx, &slug, record.is_some(), &targets, &args.again, clock).await?
    } else {
        close(
            ctx,
            &slug,
            record.as_ref(),
            &targets,
            evidence.unwrap_or_default(),
        )
        .await?
    };
    match &record {
        Some(record) => print_record_list(ctx, record).await,
        None => {
            if let Some(entry) = removed_entry(ctx, &slug).await? {
                let owed = derive_removed(
                    &entry,
                    ctx.config.branch.separate_production(),
                    clock.now_ms,
                );
                println!("{}", owed_text(&who, &slug, &owed, clock));
            }
        }
    }
    Ok(i32::from(failed))
}

/// Why a step id closes nothing, when a watch with that id still waits.
async fn missing_step(ctx: &AppContext, slug: &str, id: &str) -> String {
    let wanted = slug.to_owned();
    let waiting = ctx
        .database
        .call(move |store| Ok(store.list_watches(Some(&wanted))?))
        .await
        .ok()
        .is_some_and(|watches| watches.iter().any(|watch| watch.id == id));
    if waiting {
        format!(
            "{id} is still waiting for its conditions, so there is nothing to close yet (to drop it: wt watch cancel {id})"
        )
    } else {
        format!("{slug} has no due step {id} (wt owed lists what it owes)")
    }
}

/// Close each target with the evidence. Returns whether any could not be.
async fn close(
    ctx: &AppContext,
    slug: &str,
    record: Option<&WorktreeRecord>,
    targets: &[Target],
    evidence: String,
) -> Result<bool> {
    let mut failed = false;
    let by = agent();
    for target in targets {
        let Target::Step(id) = target else {
            continue;
        };
        let (owner, step, text, who) = (slug.to_owned(), id.clone(), evidence.clone(), by.clone());
        let closed = ctx
            .database
            .call(move |store| Ok(store.close_owed(&owner, &step, &text, who.as_deref())?))
            .await?;
        match closed {
            Some(closed) => println!(
                "✓ {slug} closed {id}{}: {}",
                if closed.removed { " (removed row)" } else { "" },
                wt_app_core::watch_view::headline(&closed.step.what)
            ),
            None => {
                eprintln!("{}", missing_step(ctx, slug, id).await);
                failed = true;
            }
        }
    }
    let stages = targets
        .iter()
        .filter_map(|target| match target {
            Target::Check(stage) => Some(*stage),
            Target::Step(_) => None,
        })
        .collect::<Vec<_>>();
    if stages.is_empty() {
        return Ok(failed);
    }
    let verified = verified_record(
        record.and_then(|record| record.head_sha.as_deref()),
        Some(evidence.clone()),
    )?;
    let targets = CheckTargets::Stages(stages.clone());
    let owner = slug.to_owned();
    let not_owed = || {
        eprintln!(
            "{slug} owes no check in {} (wt owed lists what it owes)",
            stages
                .iter()
                .map(|stage| stage.as_str())
                .collect::<Vec<_>>()
                .join(" or ")
        );
    };
    if record.is_some() {
        let closed = ctx
            .database
            .call(move |store| {
                Ok(store.close_checks(&owner, &targets, &verified, Some(&evidence))?)
            })
            .await?;
        match closed {
            ChecksClosed::Closed {
                closed,
                remaining,
                record,
                cancelled_watches,
            } => {
                print_checks_closed(slug, &closed, &remaining, Some(&record.state), false);
                super::status::report_cancelled_watches(&cancelled_watches);
                failed |= closed.len() < stages.len();
            }
            ChecksClosed::Missing | ChecksClosed::NotOwed => {
                not_owed();
                failed = true;
            }
        }
    } else {
        let closed = ctx
            .database
            .call(move |store| {
                Ok(store.close_removed_checks(&owner, &targets, &verified, Some(&evidence))?)
            })
            .await?;
        match closed {
            RemovedChecksClosed::Closed { closed, remaining } => {
                print_checks_closed(slug, &closed, &remaining, None, true);
                failed |= closed.len() < stages.len();
            }
            RemovedChecksClosed::Missing | RemovedChecksClosed::NotOwed => {
                not_owed();
                failed = true;
            }
        }
    }
    Ok(failed)
}

/// Put each target off until the conditions. Returns whether any could not
/// be.
async fn put_off(
    ctx: &AppContext,
    slug: &str,
    live: bool,
    targets: &[Target],
    conditions: &[String],
    clock: LocalClock,
) -> Result<bool> {
    let read = match super::watch::read_conditions(ctx, conditions, clock).await {
        Ok(read) => read,
        Err(message) => {
            eprintln!("{message}");
            return Ok(true);
        }
    };
    let mut failed = false;
    for target in targets {
        match target {
            Target::Check(stage) => {
                // A check already waits for its stage; only a time can put
                // it off further.
                let [only] = read.when.as_slice() else {
                    eprintln!(
                        "a check is put off to one time: wt done {} --again 3h (or 17:00, \"tomorrow 09:00\")",
                        stage.as_str()
                    );
                    failed = true;
                    continue;
                };
                let Condition::At { at_ms } = only.cond else {
                    eprintln!(
                        "a check already waits for its stage, so --again takes a time for it: wt done {} --again 3h",
                        stage.as_str()
                    );
                    failed = true;
                    continue;
                };
                let when = wt_core::watch::when_described(&read.when, clock);
                let (owner, stage, words, by) = (slug.to_owned(), *stage, when.clone(), agent());
                let deferred = ctx
                    .database
                    .call(move |store| {
                        Ok(store.defer_check(&owner, stage, at_ms, &words, by.as_deref())?)
                    })
                    .await?;
                match deferred {
                    Some(_) => println!(
                        "✓ {slug} still owes the check in {}, not before {}",
                        stage.as_str(),
                        when.trim_start_matches("at ")
                    ),
                    None => {
                        eprintln!("{slug} owes no check in {}", stage.as_str());
                        failed = true;
                    }
                }
            }
            Target::Step(id) if !live => {
                eprintln!(
                    "{slug} is removed: there is no session left to wake for {id}. Close it with -m, or do it from another worktree."
                );
                failed = true;
            }
            Target::Step(id) => {
                let (owner, step, when, by) =
                    (slug.to_owned(), id.clone(), read.when.clone(), author());
                let set = ctx
                    .database
                    .call(move |store| {
                        Ok(store.rearm_owed(
                            &owner,
                            &step,
                            when,
                            &by,
                            clock.utc_offset_minutes,
                            clock.now_ms,
                        )?)
                    })
                    .await?;
                match set {
                    Some(Ok(watch)) => {
                        println!(
                            "✓ {slug} set {id} again: wakes {slug} {} (watch {})",
                            watch.when_summary(),
                            watch.id
                        );
                        read.print();
                    }
                    Some(Err(refusal)) => {
                        eprintln!("{id} was not set again: {refusal}");
                        failed = true;
                    }
                    None => {
                        eprintln!("{}", missing_step(ctx, slug, id).await);
                        failed = true;
                    }
                }
            }
        }
    }
    Ok(failed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::status::{StatusArgs, run as status};
    use wt_app_core::test_support::CommandFixture;
    use wt_core::OwnerLanding;

    async fn git(ctx: &AppContext, args: &[&str]) -> String {
        let output = wt_ops::commands::resolve::run_git(ctx, &ctx.cwd, args.iter().copied())
            .await
            .unwrap();
        assert!(output.status.success(), "git {args:?}");
        output.stdout_text().trim().to_owned()
    }

    /// Land worktree "one": its own commit is on the base branch the origin
    /// advertises.
    async fn land(fixture: &CommandFixture) {
        let ctx = &fixture.ctx;
        let origin = fixture._root.path().join("origin.git");
        git(ctx, &["init", "--bare", origin.to_str().unwrap()]).await;
        git(ctx, &["remote", "add", "origin", origin.to_str().unwrap()]).await;
        let anchor = git(ctx, &["rev-parse", "HEAD"]).await;
        ctx.database
            .call(move |store| {
                store.set_slug_base("one", Some(("main", Some(&anchor))))?;
                Ok(())
            })
            .await
            .unwrap();
        std::fs::write(ctx.cwd.join("work.txt"), "landed work\n").unwrap();
        git(ctx, &["add", "work.txt"]).await;
        git(ctx, &["commit", "-m", "feature work"]).await;
        git(ctx, &["push", "origin", "HEAD:refs/heads/main"]).await;
    }

    async fn work(ctx: &AppContext) -> Value {
        ctx.database
            .call(|store| Ok(store.read_wt_state()?))
            .await
            .unwrap()["slugs"]["one"]["work"]
            .clone()
    }

    fn owing(staging: Option<&str>, production: Option<&str>) -> StatusArgs {
        StatusArgs {
            positionals: vec!["ready".into()],
            risk: Some("low".into()),
            verify_in_staging: staging.map(str::to_owned),
            verify_in_production: production.map(str::to_owned),
            ..Default::default()
        }
    }

    fn done(targets: &[&str], message: Option<&str>) -> DoneArgs {
        DoneArgs {
            targets: targets.iter().map(|target| (*target).to_owned()).collect(),
            message: message.map(str::to_owned),
            again: Vec::new(),
            slug: None,
        }
    }

    /// A wake delivered after landing: a due step with this prompt.
    async fn due_step(ctx: &AppContext, prompt: &'static str) -> String {
        let clock = clock();
        ctx.database
            .call(move |store| {
                let watch = store
                    .add_watch(
                        wt_store::NewWatch {
                            slug: "one".into(),
                            by: "one".into(),
                            utc_offset_minutes: clock.utc_offset_minutes,
                            when: vec![wt_core::watch::parse("merged", clock).unwrap()],
                            then: wt_core::watch::WatchActions {
                                wake: Some(prompt.into()),
                                ..Default::default()
                            },
                            before_release: true,
                        },
                        clock.now_ms,
                    )?
                    .unwrap();
                store.complete_wake(&watch.id, OwnerLanding::Landed, false, clock.now_ms)?;
                Ok(watch.id)
            })
            .await
            .unwrap()
    }

    async fn events(ctx: &AppContext) -> Vec<String> {
        ctx.database
            .call(|store| Ok(store.read_worktree_events("one", 200)?))
            .await
            .unwrap()
            .iter()
            .rev()
            .filter_map(|event| wt_core::owed_sentence(&event.event))
            .collect()
    }

    #[tokio::test]
    async fn two_checks_close_one_at_a_time_and_the_row_is_verified_only_at_the_last() {
        let fixture = CommandFixture::new().await.unwrap();
        let ctx = &fixture.ctx;
        // Both flags together: each sets its own stage's check.
        let both = owing(Some("1. check staging"), Some("1. check prod"));
        assert_eq!(status(ctx, &both).await.unwrap(), 0);
        let stored = work(ctx).await;
        assert_eq!(stored["checks"]["staging"]["steps"], "1. check staging");
        assert_eq!(stored["checks"]["production"]["steps"], "1. check prod");
        // The legacy fields mirror the first check, for older builds.
        assert_eq!(stored["verifyAfterMerge"], "1. check staging");
        assert_eq!(stored["checksMirror"], "staging");
        // A fresh assertion carries both; new steps for one leave the other.
        let working = StatusArgs {
            positionals: vec!["working".into()],
            verify_in_production: Some("2. check prod again".into()),
            ..Default::default()
        };
        assert_eq!(status(ctx, &working).await.unwrap(), 0);
        let stored = work(ctx).await;
        assert_eq!(stored["checks"]["staging"]["steps"], "1. check staging");
        assert_eq!(
            stored["checks"]["production"]["steps"],
            "2. check prod again"
        );

        // Nothing has landed, so nothing is due: verified closes the
        // earliest check and the status stays what it was.
        let verified = |note: &str| StatusArgs {
            positionals: vec!["verified".into()],
            note: Some(note.into()),
            ..Default::default()
        };
        assert_eq!(status(ctx, &verified("staging is fine")).await.unwrap(), 0);
        let stored = work(ctx).await;
        assert_eq!(stored["state"], "working");
        assert!(stored["checks"].get("staging").is_none(), "{stored}");
        assert_eq!(stored["verifyIn"], "production");
        // The last one makes the row verified, with the evidence as its note.
        assert_eq!(status(ctx, &verified("prod is fine")).await.unwrap(), 0);
        let stored = work(ctx).await;
        assert_eq!(stored["state"], "verified");
        assert_eq!(stored["note"], "prod is fine");
        assert_eq!(stored["checks"], json!({}));
        assert_eq!(
            events(ctx).await,
            [
                "done: production check: 2. check prod again: prod is fine",
                "done: staging check: 1. check staging: staging is fine",
            ]
        );
        // Verified with nothing owed is an ordinary assertion.
        assert_eq!(status(ctx, &verified("again")).await.unwrap(), 0);
        assert_eq!(work(ctx).await["note"], "again");

        // wt done closes by stage through the same path.
        assert_eq!(status(ctx, &both).await.unwrap(), 0);
        assert_eq!(
            run_done(ctx, &done(&["production"], Some("prod first")))
                .await
                .unwrap(),
            0
        );
        let stored = work(ctx).await;
        assert_eq!(stored["state"], "ready");
        assert!(stored["checks"].get("production").is_none());
        // A stage that owes nothing is a failure, not a silent success.
        assert_eq!(
            run_done(ctx, &done(&["production"], Some("again")))
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            run_done(ctx, &done(&["staging"], Some("staging too")))
                .await
                .unwrap(),
            0
        );
        let stored = work(ctx).await;
        assert_eq!(stored["state"], "verified");
        assert_eq!(stored["note"], "staging too");

        // Dropping a check without running it leaves the other one.
        assert_eq!(status(ctx, &both).await.unwrap(), 0);
        let drop = StatusArgs {
            no_verify_in_staging: true,
            ..Default::default()
        };
        assert_eq!(status(ctx, &drop).await.unwrap(), 0);
        let stored = work(ctx).await;
        assert_eq!(stored["state"], "ready");
        assert!(stored["checks"].get("staging").is_none());
        assert_eq!(stored["checks"]["production"]["steps"], "1. check prod");
        assert_eq!(
            events(ctx).await[0],
            "dropped: staging check: 1. check staging"
        );
        // Dropping what is not owed says so with its exit code.
        assert_eq!(status(ctx, &drop).await.unwrap(), 1);
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn done_without_a_target_closes_the_one_thing_that_is_due() {
        let fixture = CommandFixture::new().await.unwrap();
        let ctx = &fixture.ctx;
        // Nothing owed, then a check that is not due yet: nothing to close.
        assert_eq!(run_done(ctx, &done(&[], Some("did it"))).await.unwrap(), 1);
        assert_eq!(
            status(ctx, &owing(Some("1. check staging"), None))
                .await
                .unwrap(),
            0
        );
        assert_eq!(run_done(ctx, &done(&[], Some("did it"))).await.unwrap(), 1);
        assert_eq!(work(ctx).await["state"], "ready");
        // Evidence is required, and --again takes none.
        assert_eq!(run_done(ctx, &done(&["staging"], None)).await.unwrap(), 2);
        assert_eq!(
            run_done(ctx, &done(&["staging"], Some("  ")))
                .await
                .unwrap(),
            2
        );

        land(&fixture).await;
        let record = resolve_worktree(ctx, None).await.unwrap();
        let owed = read(ctx, &record).await.unwrap();
        assert_eq!(owed.phase, Some(OwedPhase::Due));
        assert_eq!(
            owed_text("This worktree", "one", &owed, clock()),
            "This worktree owes:\n  due      check: 1. check staging (due in staging) · wt done staging"
        );
        // One due: closed without naming it.
        assert_eq!(
            run_done(ctx, &done(&[], Some("opened the page, 200")))
                .await
                .unwrap(),
            0
        );
        let stored = work(ctx).await;
        assert_eq!(stored["state"], "verified");
        assert_eq!(stored["note"], "opened the page, 200");
        let owed = read(ctx, &record).await.unwrap();
        assert_eq!(owed.phase, Some(OwedPhase::Done));
        assert_eq!(
            owed_text("This worktree", "one", &owed, clock()),
            "This worktree owes nothing."
        );

        // Two due: they must be named.
        let first = due_step(ctx, "confirm the logs\nthen report").await;
        let second = due_step(ctx, "retire the flag").await;
        assert_eq!(run_done(ctx, &done(&[], Some("both"))).await.unwrap(), 1);
        assert_eq!(read(ctx, &record).await.unwrap().items.len(), 2);
        assert_eq!(
            run_done(ctx, &done(&[&first, "nosuch"], Some("logs clean")))
                .await
                .unwrap(),
            1
        );
        let left = read(ctx, &record).await.unwrap();
        assert_eq!(left.items.len(), 1);
        assert_eq!(left.items[0].id.as_deref(), Some(second.as_str()));
        assert!(
            item_line(&left.items[0], "one", clock())
                .ends_with(&format!(") · before release · wt done {second}")),
            "{}",
            item_line(&left.items[0], "one", clock())
        );
        assert_eq!(
            run_done(ctx, &done(&[], Some("flag gone"))).await.unwrap(),
            0
        );
        assert!(read(ctx, &record).await.unwrap().items.is_empty());
        assert_eq!(
            events(ctx).await[..2],
            [
                "done: retire the flag: flag gone".to_owned(),
                "done: confirm the logs: logs clean".to_owned()
            ]
        );
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn again_puts_a_step_or_a_check_off_instead_of_closing_it() {
        let fixture = CommandFixture::new().await.unwrap();
        let ctx = &fixture.ctx;
        let again = |targets: &[&str], conditions: &[&str]| DoneArgs {
            again: conditions.iter().map(|arg| (*arg).to_owned()).collect(),
            ..done(targets, None)
        };
        let step = due_step(ctx, "rerun the renewal check").await;
        // Evidence and --again do not go together.
        let mut both = again(&[&step], &["3h"]);
        both.message = Some("later".into());
        assert_eq!(run_done(ctx, &both).await.unwrap(), 2);
        assert_eq!(run_done(ctx, &again(&[&step], &["3h"])).await.unwrap(), 0);
        let (watches, owed) = ctx
            .database
            .call(|store| {
                Ok((
                    store.list_watches(Some("one"))?,
                    store.list_owed(Some("one"))?,
                ))
            })
            .await
            .unwrap();
        // The step is a wake again, with the same note, and is not due.
        assert!(owed.is_empty());
        assert_eq!(watches.len(), 1);
        assert_eq!(
            watches[0].then.wake.as_deref(),
            Some("rerun the renewal check")
        );
        assert!(watches[0].before_release());
        assert!(
            events(ctx).await[0].starts_with("again at "),
            "{:?}",
            events(ctx).await
        );
        // It is no longer something to close.
        assert_eq!(
            run_done(ctx, &done(&[&step], Some("done"))).await.unwrap(),
            1
        );

        // A check takes one time and stays owed.
        assert_eq!(
            status(ctx, &owing(Some("1. check staging"), None))
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            run_done(ctx, &again(&["staging"], &["merged"]))
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            run_done(ctx, &again(&["staging"], &["3h", "4h"]))
                .await
                .unwrap(),
            1
        );
        assert!(
            work(ctx).await["checks"]["staging"]
                .get("notBeforeMs")
                .is_none()
        );
        let before = clock().now_ms;
        assert_eq!(
            run_done(ctx, &again(&["staging"], &["3h"])).await.unwrap(),
            0
        );
        let stored = work(ctx).await;
        assert_eq!(stored["state"], "ready");
        let not_before = stored["checks"]["staging"]["notBeforeMs"].as_i64().unwrap();
        assert!(
            not_before >= before + 3 * 3_600_000 - 60_000,
            "{not_before}"
        );
        // Landed or not, it reads waiting until then.
        land(&fixture).await;
        let record = resolve_worktree(ctx, None).await.unwrap();
        let owed = read(ctx, &record).await.unwrap();
        let check = owed
            .items
            .iter()
            .find(|item| item.kind == OwedKind::Check)
            .unwrap();
        assert_eq!(check.state, OwedState::Waiting);
        assert_eq!(owed.phase, Some(OwedPhase::Waiting));
        // No session is left to wake on a removed row.
        assert_eq!(
            run_done(ctx, &again(&["production"], &["3h"]))
                .await
                .unwrap(),
            1
        );
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn a_removed_row_closes_what_its_record_still_owes() {
        let fixture = CommandFixture::new().await.unwrap();
        let ctx = &fixture.ctx;
        let removed_at = now_iso();
        ctx.database
            .call(move |store| {
                let mut state = store.read_wt_state()?;
                state["removed"] = json!([{
                    "slug": "gone",
                    "branch": "feature/gone",
                    "removedAt": removed_at,
                    "landedOnAtRemoval": "base",
                    "work": {
                        "state": "ready", "at": "2026-01-01T00:00:00Z",
                        "verifyAfterMerge": "1. check staging", "verifyIn": "staging",
                        "checks": {},
                    },
                    "checks": {
                        "staging": {"steps": "1. check staging"},
                        "production": {"steps": "1. check prod"},
                    },
                    "checksMirror": "staging",
                    "owed": [{
                        "id": "abc234", "slug": "gone", "by": "gone",
                        "what": "confirm the logs", "whenText": "merged",
                        "dueAtMs": 1_700_000_000_000_i64, "utcOffsetMinutes": 0,
                    }],
                }]);
                store.write_repository_state_json(&state.to_string())?;
                Ok(())
            })
            .await
            .unwrap();
        let entry = || async { removed_entry(ctx, "gone").await.unwrap().unwrap() };
        let owed = derive_removed(&entry().await, false, clock().now_ms);
        assert_eq!(owed.items.len(), 3);
        assert_eq!(owed.phase, Some(OwedPhase::Due));
        let gone = |targets: &[&str], message: Option<&str>| DoneArgs {
            slug: Some("gone".into()),
            ..done(targets, message)
        };
        // Several things owed: they must be named.
        assert_eq!(run_done(ctx, &gone(&[], Some("all"))).await.unwrap(), 1);
        assert_eq!(
            run_done(ctx, &gone(&["abc234"], Some("logs clean")))
                .await
                .unwrap(),
            0
        );
        assert!(entry().await.owed().is_empty());
        // A step on a removed row cannot be set again: nothing to wake.
        assert_eq!(
            run_done(ctx, &gone(&["staging"], Some("page loads")))
                .await
                .unwrap(),
            0
        );
        let left = entry().await;
        assert_eq!(
            left.open_checks()
                .iter()
                .map(|(stage, _)| *stage)
                .collect::<Vec<_>>(),
            [VerifyStage::Production]
        );
        assert_eq!(left.work.as_ref().unwrap().state, "ready");
        // The one thing left is closed without naming it.
        assert_eq!(
            run_done(ctx, &gone(&[], Some("prod fine"))).await.unwrap(),
            0
        );
        let settled = entry().await;
        assert!(!settled.owes_anything());
        assert_eq!(settled.work.as_ref().unwrap().state, "verified");
        assert_eq!(run_done(ctx, &gone(&[], Some("again"))).await.unwrap(), 1);
        // A slug that names nothing is an error, not an empty list.
        let nowhere = DoneArgs {
            slug: Some("nowhere".into()),
            ..done(&[], Some("x"))
        };
        assert_eq!(run_done(ctx, &nowhere).await.unwrap(), 1);
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn the_list_and_its_json_say_the_same_thing_everywhere() {
        let fixture = CommandFixture::new().await.unwrap();
        let ctx = &fixture.ctx;
        let gated = StatusArgs {
            positionals: vec!["ready".into()],
            risk: Some("low".into()),
            blocked_on: Some("mobile release".into()),
            verify_in_production: Some("1. check prod\n2. check logs".into()),
            ..Default::default()
        };
        assert_eq!(status(ctx, &gated).await.unwrap(), 0);
        let step = due_step(ctx, "confirm the logs").await;
        let record = resolve_worktree(ctx, None).await.unwrap();
        let owed = read(ctx, &record).await.unwrap();
        // Not landed: the row's asserted status says where it stands.
        assert_eq!(owed.phase, None);
        let clock = clock();
        let listed = owes_json(&owed.items, clock);
        assert_eq!(listed.as_array().unwrap().len(), 3);
        assert_eq!(
            listed[0]
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
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
        assert_eq!(
            (&listed[0]["id"], &listed[0]["kind"], &listed[0]["state"]),
            (&json!(step), &json!("step"), &json!("due"))
        );
        assert_eq!(listed[0]["before_release"], true);
        assert!(listed[0]["due_at"].as_str().unwrap().ends_with('Z'));
        assert_eq!(
            listed[1],
            json!({
                "id": null, "kind": "gate", "state": "waiting", "stage": null,
                "when": "until a person clears it", "what": "mobile release",
                "before_release": false, "overdue": false, "due_at": null,
            })
        );
        assert_eq!(
            (&listed[2]["kind"], &listed[2]["stage"], &listed[2]["when"]),
            (
                &json!("check"),
                &json!("production"),
                &json!("once in production")
            )
        );
        assert_eq!(phase_json(owed.phase), Value::Null);
        assert_eq!(phase_json(Some(OwedPhase::Waiting)), "waiting");
        let text = owed_text("one", "one", &owed, clock);
        let lines = text.lines().collect::<Vec<_>>();
        assert_eq!(lines[0], "one owes:");
        assert!(lines[1].starts_with("  due      confirm the logs (due since "));
        assert!(lines[1].ends_with(&format!(" · before release · wt done {step}")));
        assert_eq!(
            lines[2..],
            [
                "  gate     mobile release (until a person clears it)",
                "  waiting  check: 1. check prod 2. check logs (once in production)"
            ]
        );
        // The fleet view groups by what is due and what each item waits for.
        let fleet = [("one".to_owned(), "live", owed)];
        let grouped = fleet_text(&fleet, clock);
        let titles = grouped
            .lines()
            .filter(|line| !line.starts_with(' ') && !line.is_empty())
            .collect::<Vec<_>>();
        assert_eq!(titles, ["Due now", "Waiting for production", "Waiting"]);
        assert!(grouped.contains(&format!(" · wt done {step} --slug one")));
        assert_eq!(
            run_owed(
                ctx,
                &OwedArgs {
                    all: true,
                    json: true,
                    slug: None
                }
            )
            .await
            .unwrap(),
            0
        );
        assert_eq!(
            run_owed(
                ctx,
                &OwedArgs {
                    all: false,
                    json: false,
                    slug: Some("two".into())
                }
            )
            .await
            .unwrap(),
            0
        );
        assert_eq!(
            run_owed(
                ctx,
                &OwedArgs {
                    all: false,
                    json: false,
                    slug: Some("nowhere".into())
                }
            )
            .await
            .unwrap(),
            1
        );
        fixture.close().await.unwrap();
    }
}
