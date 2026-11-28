//! `wt wake`, `wt notify` and `wt watch`: set, list and cancel watches. A
//! watch names conditions and what happens once they all hold; the engine
//! in the TUI's host evaluates and delivers them. `wt wait` (wait.rs) reads
//! the same conditions through [`read_conditions`] and judges them itself.

use anyhow::Result;
use clap::{Args, Subcommand};
use serde_json::{Value, json};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use wt_app_core::{
    context::AppContext,
    watch_view::{Shown, headline},
};
use wt_config::Config;
use wt_core::{
    VerifyStage,
    watch::{
        Condition, LocalClock, PrGoal, RowState, Watch, WatchAction, WatchActions, WatchCondition,
        parse,
    },
};
use wt_ops::commands::resolve::Inventory;
use wt_store::{NewWatch, WatchRefusal, WatchSelector};

pub(crate) const CONDITION_HELP: &str = "What to wait for; all must hold. One argument each: a local time (17:00, 5pm, 3h, \"tomorrow 09:00\", 2030-01-15T09:00), this worktree (merged, staging, production, ready, green, verified, closed), its open PR (settled: checks passed or failed; reviewed: the review bot finished the current head; evicted: the armed merge was dropped), a pull request (pr:3806, pr:3806:production), another worktree (wt:<slug>:merged, wt:<slug>:gone; <slug>:merged also reads), release:production, check:<id>[=<arg>]. Anything but a time may end in +<duration>: it must have held that long";

#[derive(Debug, Clone, Args)]
pub struct WakeArgs {
    // Not required by the parser: a command that lost its conditions to a
    // shell comment gets the explanation instead of a usage error.
    #[arg(value_name = "CONDITION", help = CONDITION_HELP)]
    pub conditions: Vec<String>,
    /// What the session should do then. It arrives as a prompt, with what
    /// held and this note.
    #[arg(
        short = 'm',
        long = "message",
        value_name = "PROMPT",
        allow_hyphen_values = true
    )]
    pub message: Option<String>,
    /// The worktree whose session to wake (default: the current one).
    #[arg(long, value_name = "SLUG")]
    pub slug: Option<String>,
    /// Mark the step as one that must happen before the next production
    /// release: `wt owed --all` lists it under that heading.
    #[arg(long)]
    pub before_release: bool,
}

#[derive(Debug, Clone, Args)]
pub struct NotifyArgs {
    #[arg(value_name = "CONDITION", help = CONDITION_HELP)]
    pub conditions: Vec<String>,
    /// What to tell the person (default: what held).
    #[arg(
        short = 'm',
        long = "message",
        value_name = "TEXT",
        allow_hyphen_values = true
    )]
    pub message: Option<String>,
    /// The worktree the watch belongs to (default: the current one).
    #[arg(long, value_name = "SLUG")]
    pub slug: Option<String>,
}

#[derive(Debug, Clone, Args)]
#[command(args_conflicts_with_subcommands = true)]
pub struct WatchArgs {
    #[command(subcommand)]
    pub command: Option<WatchCommand>,
    /// Every worktree's watches, not only this one's.
    #[arg(long)]
    pub all: bool,
    /// Print an array: id, slug, by, created_at, when (arg, description,
    /// state: holds, not_yet, unknown or not_evaluated_yet, and reason),
    /// then, met_at, last_error, evaluated_at, owner_gone, evaluating.
    #[arg(long)]
    pub json: bool,
    /// The worktree whose watches to list (default: the current one).
    #[arg(long, value_name = "SLUG", conflicts_with = "all")]
    pub slug: Option<String>,
}

#[derive(Debug, Clone, Subcommand)]
pub enum WatchCommand {
    /// Cancel watches by id, whoever set them, or (--mine) every watch you
    /// set on this worktree.
    Cancel {
        /// Watch ids, as `wt watch` lists them.
        #[arg(value_name = "ID")]
        ids: Vec<String>,
        /// Every watch set on the worktree by whoever runs this.
        #[arg(long, conflicts_with = "ids")]
        mine: bool,
        /// The worktree for --mine (default: the current one).
        #[arg(long, value_name = "SLUG")]
        slug: Option<String>,
    },
    /// Read conditions and say what they mean in this repository, without
    /// setting anything.
    Check {
        #[arg(value_name = "CONDITION", required = true, help = CONDITION_HELP)]
        conditions: Vec<String>,
    },
}

pub(crate) fn now_ms() -> i64 {
    i64::try_from(OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000).unwrap_or(i64::MAX)
}

/// The caller's wall clock, for reading and describing times.
pub(crate) fn clock() -> LocalClock {
    let now_ms = now_ms();
    LocalClock {
        now_ms,
        utc_offset_minutes: u64::try_from(now_ms)
            .ok()
            .and_then(wt_app_core::display_time::local_utc_offset_minutes)
            .unwrap_or(0),
    }
}

/// The agent identity this command runs as, when it runs in a session.
pub(crate) fn agent() -> Option<String> {
    std::env::var("WT_AGENT")
        .ok()
        .map(|by| by.trim().to_owned())
        .filter(|by| !by.is_empty())
}

/// Who is setting or cancelling: the agent's identity, or the person.
pub(crate) fn author() -> String {
    agent().unwrap_or_else(|| "human".into())
}

/// What a condition means under this configuration, and whether that is
/// worth saying unasked (it reads differently from what the word suggests).
/// `Err` when the configuration cannot answer the condition at all.
fn config_fact(config: &Config, cond: &Condition) -> Result<Option<(String, bool)>, String> {
    let branch = &config.branch;
    let stage_fact = |stage: VerifyStage, release: bool| {
        let name = stage.as_str();
        let probed = config.environments.get(stage).is_some();
        let stage_branch = match stage {
            VerifyStage::Staging => branch.base.as_str(),
            VerifyStage::Production => branch.production.as_deref().unwrap_or(&branch.base),
        };
        let text = match (release, probed, stage) {
            (true, true, _) => format!(
                "a {name} release is a new commit reported by the [environments.{name}] deploy probe"
            ),
            (true, false, _) => format!(
                "no [environments.{name}] deploy probe: a {name} release is any new commit on {stage_branch}"
            ),
            (false, true, _) => format!(
                "{name} means deployed there, as the [environments.{name}] deploy probe reports"
            ),
            (false, false, VerifyStage::Staging) => format!(
                "no [environments.staging] deploy probe: staging means merged to {stage_branch}"
            ),
            (false, false, VerifyStage::Production) if branch.separate_production() => format!(
                "no [environments.production] deploy probe: production means the landing reached {stage_branch}"
            ),
            (false, false, VerifyStage::Production) => format!(
                "no [branch] production and no [environments.production] deploy probe: production means merged to {stage_branch}"
            ),
        };
        Some((text, !probed))
    };
    let state_fact = |state: RowState| match state {
        RowState::Staging => stage_fact(VerifyStage::Staging, false),
        RowState::Production => stage_fact(VerifyStage::Production, false),
        _ => None,
    };
    Ok(match cond {
        Condition::Check { id, .. } => {
            let Some(check) = config.checks.iter().find(|check| check.id == *id) else {
                let known = config
                    .checks
                    .iter()
                    .map(|check| check.id.as_str())
                    .collect::<Vec<_>>();
                return Err(if known.is_empty() {
                    format!(
                        "no check is configured, so check:{id} can never hold. Add one to the configuration:\n  [[checks]]\n  id = \"{id}\"\n  run = [\"<argv that exits 0 when it holds, 1 when not yet>\", \"{{{{arg}}}}\"]"
                    )
                } else {
                    format!(
                        "no [[checks]] entry has id \"{id}\". Configured checks: {}",
                        known.join(", ")
                    )
                });
            };
            let what = if check.description.is_empty() {
                check.run.join(" ")
            } else {
                check.description.clone()
            };
            Some((
                format!("check {id}: {what}; runs every {}s", check.every_seconds),
                false,
            ))
        }
        Condition::Own(state) | Condition::Worktree { state, .. } => state_fact(*state),
        Condition::Pr {
            goal: PrGoal::Reached(stage),
            ..
        }
        | Condition::Commit { stage, .. } => stage_fact(*stage, false),
        Condition::Release(stage) => stage_fact(*stage, true),
        _ => None,
    })
}

/// What to say when a command names no condition. The usual cause is a pull
/// request written with `#`: unquoted, the shell takes it and the rest of
/// the line as a comment, so the command arrives without them.
pub(crate) const NO_CONDITIONS: &str = "name at least one condition, such as pr:3806:production, release:production+24h or 17:00.\nIf you wrote a pull request as #3806, the shell read # as the start of a comment and dropped it with the rest of the command: write pr:3806";

pub(crate) fn no_conditions(args: &[String]) -> bool {
    args.is_empty() || args.iter().any(|arg| arg.trim().is_empty())
}

pub(crate) struct ReadConditions {
    pub when: Vec<WatchCondition>,
    /// What the conditions mean here that their words do not say.
    pub notes: Vec<String>,
    /// What looks like a mistake but may be meant.
    pub warnings: Vec<String>,
}

impl ReadConditions {
    pub(crate) fn print(&self) {
        for warning in &self.warnings {
            println!("  warning: {warning}");
        }
        // Not "note": that is the status note's label.
        for note in &self.notes {
            println!("  hint: {note}");
        }
    }
}

/// The removal records, read only when a condition names a worktree that
/// is not there.
async fn removed_for(
    ctx: &AppContext,
    conditions: &[Condition],
    live: &[String],
) -> wt_sources::watch_facts::Removed {
    let absent = conditions
        .iter()
        .any(|cond| matches!(cond, Condition::Worktree { slug, .. } if !live.contains(slug)));
    if !absent {
        return Default::default();
    }
    let separate = ctx.config.branch.separate_production();
    ctx.database
        .call(|store| Ok(store.read_wt_state()?))
        .await
        .map(|state| wt_sources::watch_facts::removed_rows(&state, separate))
        .unwrap_or_default()
}

/// What to say about a condition on a worktree that is not there. `live`
/// is every live worktree's slug; `removed` says what the removal record of
/// one that is gone kept, which is what the condition is then judged by.
fn absent_worktree(
    cond: &Condition,
    live: &[String],
    removed: &wt_sources::watch_facts::Removed,
) -> Option<String> {
    let known = |slug: &String| live.contains(slug);
    match cond {
        Condition::Worktree { slug, state } if !known(slug) => {
            let follows_landing = matches!(
                state,
                RowState::Merged | RowState::Staging | RowState::Production
            );
            Some(match removed.get(slug).and_then(|row| row.merged) {
                None => format!(
                    "no worktree named {slug} right now; this waits until one exists and reaches {}",
                    state.as_str()
                ),
                Some(true) if *state == RowState::Merged => {
                    format!("{slug} was removed after its work merged, so this holds already")
                }
                Some(true) if follows_landing && removed.get(slug)?.get(*state) == Some(true) => {
                    format!(
                        "{slug} was removed after its work reached {}, so this holds already",
                        state.as_str()
                    )
                }
                Some(true) if follows_landing => match removed.landing_commit(slug) {
                    Some(_) => format!(
                        "{slug} was removed after its work merged; this follows the commit that landed it to {}",
                        state.as_str()
                    ),
                    None => format!(
                        "{slug} was removed after its work merged and the commit that landed it was not recorded, so whether it reached {} cannot be told",
                        state.as_str()
                    ),
                },
                Some(false) if follows_landing => format!(
                    "{slug} was removed without merging, so this cannot hold unless a worktree named {slug} is created again and merges"
                ),
                Some(_) => format!(
                    "{slug} was removed and can no longer become {}",
                    state.as_str()
                ),
            })
        }
        Condition::Gone { slug } if !known(slug) => Some(format!(
            "no worktree named {slug} right now, so this holds already"
        )),
        _ => None,
    }
}

/// The listing every later step of the command asks, or why there is none.
pub(crate) async fn inventory(ctx: &AppContext) -> Result<Inventory, String> {
    Inventory::read(ctx)
        .await
        .map_err(|error| format!("cannot list the worktrees: {error:#}"))
}

fn live_slugs(inventory: &Inventory) -> Vec<String> {
    inventory
        .worktrees()
        .map(|record| record.target.slug().to_owned())
        .collect()
}

/// Read condition arguments for a watch about to be set. A release
/// condition takes the stage's current release as its baseline when that is
/// known now; otherwise the engine takes the first one it sees. `Err` says
/// what is wrong with the arguments.
pub(crate) async fn read_conditions(
    ctx: &AppContext,
    inventory: &Inventory,
    args: &[String],
    clock: LocalClock,
) -> Result<ReadConditions, String> {
    if no_conditions(args) {
        return Err(NO_CONDITIONS.into());
    }
    let mut read = ReadConditions {
        when: Vec::new(),
        notes: Vec::new(),
        warnings: Vec::new(),
    };
    let live = live_slugs(inventory);
    let parsed = args
        .iter()
        .map(|arg| parse(arg, clock).map_err(|error| error.to_string()))
        .collect::<Result<Vec<_>, _>>()?;
    let named = parsed
        .iter()
        .map(|cond| cond.cond.clone())
        .collect::<Vec<_>>();
    let removed = removed_for(ctx, &named, &live).await;
    for mut cond in parsed {
        if let Some(warning) = absent_worktree(&cond.cond, &live, &removed)
            && !read.warnings.contains(&warning)
        {
            read.warnings.push(warning);
        }
        if let Some((note, true)) = config_fact(&ctx.config, &cond.cond)?
            && !read.notes.contains(&note)
        {
            read.notes.push(note);
        }
        if let Condition::Release(stage) = cond.cond {
            cond.baseline =
                wt_ops::watch_probe::release_marker(ctx, stage, &ctx.cancellation).await;
        }
        read.when.push(cond);
    }
    Ok(read)
}

/// The worktree `what` (a watch, a mark) is set on: the named one or the
/// current one.
pub(crate) async fn owner(
    ctx: &AppContext,
    slug: Option<&str>,
    what: &str,
) -> Result<String, String> {
    match Inventory::read(ctx).await {
        Ok(inventory) => owner_in(&inventory, slug, what),
        Err(_) if slug.is_none() => {
            Err("not inside a worktree: run this in one, or pass --slug <slug>".into())
        }
        Err(error) => Err(error.to_string()),
    }
}

/// [`owner`] against a listing the command already read.
pub(crate) fn owner_in(
    inventory: &Inventory,
    slug: Option<&str>,
    what: &str,
) -> Result<String, String> {
    match inventory.resolve(slug) {
        Ok(record) if !record.is_main => Ok(record.target.slug().to_owned()),
        Ok(_) => Err(format!(
            "{what} belongs to a worktree and the main clone is not one: pass --slug <slug>"
        )),
        Err(_) if slug.is_none() => {
            Err("not inside a worktree: run this in one, or pass --slug <slug>".into())
        }
        Err(error) => Err(error.to_string()),
    }
}

/// What [`set`] stored, or found stored.
pub(crate) struct SetWatch {
    pub watch: Watch,
    /// The same watch was pending already: nothing was written.
    pub already: bool,
}

/// Store a watch. `Err` carries what to do instead. A session cannot be
/// woken about its own worktree being gone, so that is refused here for
/// every command that sets a wake.
pub(crate) async fn set(
    ctx: &AppContext,
    slug: &str,
    when: Vec<WatchCondition>,
    then: WatchActions,
    before_release: bool,
    clock: LocalClock,
) -> Result<Result<SetWatch, String>> {
    if then.wake.is_some()
        && when
            .iter()
            .any(|cond| matches!(&cond.cond, Condition::Gone { slug: gone } if gone == slug))
    {
        return Ok(Err(format!(
            "{slug}'s session cannot be woken once {slug} is gone: there is nothing left to wake. To be told instead: wt notify wt:{slug}:gone"
        )));
    }
    let new = NewWatch {
        slug: slug.to_owned(),
        by: author(),
        utc_offset_minutes: clock.utc_offset_minutes,
        when,
        then,
        before_release,
    };
    let added = ctx
        .database
        .call(move |store| Ok(store.add_watch(new, clock.now_ms)?))
        .await?;
    Ok(match added {
        Ok(watch) => Ok(SetWatch {
            watch,
            already: false,
        }),
        // Agents retry commands: the second run is the first one's watch.
        Err(WatchRefusal::AlreadySet { watch }) => Ok(SetWatch {
            watch: *watch,
            already: true,
        }),
        Err(refusal @ (WatchRefusal::TooManyForSlug { .. } | WatchRefusal::TooMany)) => {
            Err(format!(
                "{refusal}: list them with `wt watch --all` and cancel with `wt watch cancel <id>`"
            ))
        }
        Err(refusal @ WatchRefusal::GateChanged { .. }) => Err(format!(
            "{refusal}; set the gate and its condition together: wt status --blocked-on \"<why>\" --until <condition>"
        )),
        Err(other) => Err(other.to_string()),
    })
}

async fn report_set(ctx: &AppContext, set: &SetWatch, does: &str, read: &ReadConditions) {
    let watch = &set.watch;
    println!("{}", set_text(watch, does, set.already));
    read.print();
    println!("  cancel: wt watch cancel {}", watch.id);
    if !super::automations::engine_running(ctx).await {
        println!(
            "  nothing is evaluating watches right now: they are checked while the wt TUI is open for this repository"
        );
    }
}

/// `✓ watch abc234: wakes one at 01:01 tomorrow`.
fn set_text(watch: &Watch, does: &str, already: bool) -> String {
    format!(
        "{} watch {}: {does} {}",
        if already { "already set:" } else { "✓" },
        watch.id,
        watch.when_summary()
    )
}

fn clean(text: &str) -> Option<String> {
    let text = wt_core::sanitize_work_note(text);
    (!text.is_empty()).then_some(text)
}

/// A prompt keeps its line breaks: steps written as lines arrive as lines.
pub(crate) fn clean_prompt(text: &str) -> Option<String> {
    let text = wt_core::sanitize_terminal_lines(text);
    (!text.is_empty()).then_some(text)
}

pub async fn run_wake(ctx: &AppContext, args: &WakeArgs) -> Result<i32> {
    // Before -m: a shell comment takes the conditions and -m together.
    if no_conditions(&args.conditions) {
        eprintln!("{NO_CONDITIONS}");
        return Ok(2);
    }
    let Some(prompt) = args.message.as_deref().and_then(clean_prompt) else {
        eprintln!("-m needs what the session should do once the conditions hold");
        return Ok(2);
    };
    let then = WatchActions {
        wake: Some(prompt),
        ..WatchActions::default()
    };
    set_and_report(
        ctx,
        args.slug.as_deref(),
        &args.conditions,
        then,
        args.before_release,
    )
    .await
}

pub async fn run_notify(ctx: &AppContext, args: &NotifyArgs) -> Result<i32> {
    if no_conditions(&args.conditions) {
        eprintln!("{NO_CONDITIONS}");
        return Ok(2);
    }
    let then = WatchActions {
        notify: Some(args.message.as_deref().and_then(clean).unwrap_or_default()),
        ..WatchActions::default()
    };
    set_and_report(ctx, args.slug.as_deref(), &args.conditions, then, false).await
}

async fn set_and_report(
    ctx: &AppContext,
    slug: Option<&str>,
    conditions: &[String],
    then: WatchActions,
    before_release: bool,
) -> Result<i32> {
    let inventory = match inventory(ctx).await {
        Ok(inventory) => inventory,
        Err(message) => {
            eprintln!("{message}");
            return Ok(1);
        }
    };
    let slug = match owner_in(&inventory, slug, "a watch") {
        Ok(slug) => slug,
        Err(message) => {
            eprintln!("{message}");
            return Ok(1);
        }
    };
    let clock = clock();
    let read = match read_conditions(ctx, &inventory, conditions, clock).await {
        Ok(read) => read,
        Err(message) => {
            eprintln!("{message}");
            return Ok(2);
        }
    };
    let wakes = then.wake.is_some();
    let does = if wakes {
        format!("wakes {slug}")
    } else {
        "tells you".to_owned()
    };
    let when = read.when.clone();
    match set(ctx, &slug, when, then, before_release, clock).await? {
        Ok(set) => {
            report_set(ctx, &set, &does, &read).await;
            // A wake is owed by the worktree; telling the person is not.
            if wakes {
                let state = ctx
                    .database
                    .call(|store| Ok(store.read_wt_state()?))
                    .await?;
                let record = wt_core::parse_work_status(&state["slugs"][slug.as_str()]["work"]);
                if let Some(warning) = duplicates_check(record.as_ref(), &set.watch) {
                    println!("  warning: {warning}");
                }
                // Only the note changes here, and no list is derived from it:
                // the state read above still says what the worktree owes.
                super::status::drop_ops_none(ctx, &slug).await?;
                super::owed::print_list_from(ctx, &inventory, &slug, &state).await;
            }
            Ok(0)
        }
        Err(message) => {
            eprintln!("{message}");
            Ok(1)
        }
    }
}

/// The warning for a wake whose prompt says it runs a check the worktree
/// already owes: it names verifying, or the owed check, or quotes the
/// check's own first line. Such a wake leaves the check due beside it;
/// putting the check off is `wt done <stage> --again <time>`. Waiting for
/// the check's stage is not enough, since a second step there is ordinary.
/// A warning only: the wake may still be a real second step.
fn duplicates_check(record: Option<&wt_core::WorkStatusRecord>, wake: &Watch) -> Option<String> {
    let prompt = wake.then.wake.as_deref()?.to_lowercase();
    let words = prompt
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>();
    // "verify-in" splits into words, so "verify" covers it.
    let says_so = words.iter().enumerate().any(|(index, word)| match *word {
        "verify" | "verification" => true,
        "owed" => words[index + 1..]
            .iter()
            .take(3)
            .any(|next| matches!(*next, "check" | "staging" | "production")),
        _ => false,
    });
    let quotes = |steps: &str| {
        let first = wt_core::watch::headline(steps).to_lowercase();
        let first = first.trim();
        first.chars().count() >= 12 && prompt.contains(first)
    };
    let checks = record?.open_checks();
    let (stage, _) = checks
        .iter()
        .find(|(stage, check)| quotes(&check.steps) || (says_so && words.contains(&stage.as_str())))
        .or_else(|| checks.first().filter(|_| says_so))?;
    let stage = stage.as_str();
    Some(format!(
        "this worktree already owes a check in {stage}, which wt brings to the session when it is due. To run it later, put the check itself off: wt done {stage} --again <time>. This wake leaves the check due beside it (to drop it: wt watch cancel {})",
        wake.id
    ))
}

/// What cancelling a watch leaves behind that its setter may not expect.
fn print_cancelled(watch: &Watch) {
    println!("✓ cancelled {} ({})", watch.id, watch.summary());
    if let Some(gate) = &watch.then.unblock {
        println!(
            "  the gate \"{gate}\" stays set and no longer clears by itself; clear it with: wt status {} --unblock",
            watch.slug
        );
    }
}

/// Cancelling a wake or a gate's watch changes what its worktree owes:
/// print each affected worktree's list once.
async fn print_lists_after_cancel(
    ctx: &AppContext,
    inventory: Option<Inventory>,
    cancelled: &[Watch],
) {
    let mut slugs = Vec::new();
    for watch in cancelled {
        let owed = watch.then.wake.is_some() || watch.then.unblock.is_some();
        if owed && !watch.owner_gone && !slugs.contains(&watch.slug) {
            slugs.push(watch.slug.clone());
        }
    }
    if slugs.is_empty() {
        return;
    }
    // A list that cannot be read prints nothing: the cancellation stands.
    let inventory = match inventory {
        Some(inventory) => inventory,
        None => match Inventory::read(ctx).await {
            Ok(inventory) => inventory,
            Err(_) => return,
        },
    };
    for slug in slugs {
        super::owed::print_list(ctx, &inventory, &slug).await;
    }
}

/// Why a condition reads unknown when nothing evaluates watches.
const NOT_EVALUATING: &str = "nothing is evaluating it: the wt TUI is not open for this repository";

fn shown(watch: &Watch, cond: &WatchCondition, now_ms: i64, evaluating: bool) -> Shown {
    wt_app_core::watch_view::shown(watch, cond, now_ms, (!evaluating).then_some(NOT_EVALUATING))
}

fn iso(ms: i64) -> Option<String> {
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000)
        .ok()?
        .format(&Rfc3339)
        .ok()
}

fn then_json(then: &WatchActions) -> Value {
    Value::Object(
        then.remaining()
            .filter_map(|action| Some((action.as_str().to_owned(), json!(then.get(action)?))))
            .collect(),
    )
}

/// One worktree's watches as the status surfaces carry them: who set each,
/// what it waits for (as arguments `wt wake` reads back, and in words), what
/// it will do, and when it was met.
pub(crate) fn watches_json(watches: &[Watch], slug: &str, clock: LocalClock) -> Value {
    watches
        .iter()
        .filter(|watch| watch.slug == slug && !watch.owner_gone)
        .map(|watch| {
            json!({
                "id": watch.id,
                "by": watch.by,
                "when": watch.when.iter().map(|cond| json!({
                    "arg": cond.to_arg(watch.utc_offset_minutes),
                    "description": cond.describe(clock),
                })).collect::<Vec<_>>(),
                "then": then_json(&watch.then),
                "met_at": watch.met_at_ms.and_then(iso),
            })
        })
        .collect()
}

fn listing_json(watch: &Watch, clock: LocalClock, evaluating: bool) -> Value {
    json!({
        "id": watch.id,
        "slug": watch.slug,
        "by": watch.by,
        "created_at": iso(watch.created_at_ms),
        "when": watch.when.iter().map(|cond| {
            let state = shown(watch, cond, clock.now_ms, evaluating);
            json!({
                "arg": cond.to_arg(watch.utc_offset_minutes),
                "description": cond.describe(clock),
                "state": state.word().replace(' ', "_"),
                "reason": state.reason(),
            })
        }).collect::<Vec<_>>(),
        "then": then_json(&watch.then),
        "met_at": watch.met_at_ms.and_then(iso),
        "last_error": watch.last_error,
        "evaluated_at": watch.evaluated_at_ms().and_then(iso),
        "owner_gone": watch.owner_gone,
        "evaluating": evaluating,
    })
}

fn listing_text(watch: &Watch, clock: LocalClock, evaluating: bool) -> String {
    let age = wt_core::work_age(&iso(watch.created_at_ms).unwrap_or_default(), clock.now_ms)
        .map(|age| format!(", {age} ago"))
        .unwrap_or_default();
    let mut out = format!(
        "{}  {}  set by {}{age}{}",
        watch.id,
        watch.slug,
        watch.by,
        if watch.is_met() {
            "  (met; delivering)"
        } else if watch.owner_gone {
            "  (its worktree is gone)"
        } else {
            ""
        }
    );
    for cond in &watch.when {
        let state = shown(watch, cond, clock.now_ms, evaluating);
        out.push_str(&format!(
            "\n  {:<8} {}{}",
            state.word(),
            cond.describe(clock),
            state
                .reason()
                .map(|reason| format!(" ({reason})"))
                .unwrap_or_default()
        ));
    }
    for action in watch.then.remaining() {
        let text = watch.then.get(action).unwrap_or_default();
        out.push_str(&match action {
            WatchAction::Unblock => format!("\n  then: clear the gate \"{text}\""),
            WatchAction::Notify if text.is_empty() => "\n  then: tell you".to_owned(),
            WatchAction::Notify => format!("\n  then: tell you: {}", headline(text)),
            WatchAction::Wake => format!("\n  then: wake the session: {}", headline(text)),
        });
    }
    // Said once: beside the condition it is about, or here.
    let beside_a_condition = |error: &String| {
        watch.when.iter().any(|cond| {
            let state = shown(watch, cond, clock.now_ms, evaluating);
            matches!(state, Shown::Unknown(_))
                && state
                    .reason()
                    .is_some_and(|reason| error == reason || error.ends_with(reason))
        })
    };
    if let Some(error) = watch
        .last_error
        .as_ref()
        .filter(|error| !beside_a_condition(error))
    {
        out.push_str(&format!("\n  problem: {error}"));
    }
    out
}

pub async fn run_watch(ctx: &AppContext, args: &WatchArgs) -> Result<i32> {
    match &args.command {
        Some(WatchCommand::Cancel { ids, mine, slug }) => {
            return cancel(ctx, ids, *mine, slug.as_deref()).await;
        }
        Some(WatchCommand::Check { conditions }) => return check(ctx, conditions).await,
        None => {}
    }
    // Outside a worktree there is no "this worktree": show them all.
    let scope = if args.all {
        None
    } else {
        match owner(ctx, args.slug.as_deref(), "a watch").await {
            Ok(slug) => Some(slug),
            Err(_) if args.slug.is_none() => None,
            Err(message) => {
                eprintln!("{message}");
                return Ok(1);
            }
        }
    };
    let wanted = scope.clone();
    let watches = ctx
        .database
        .call(move |store| Ok(store.list_watches(wanted.as_deref())?))
        .await?;
    let evaluating = super::automations::engine_running(ctx).await;
    let clock = clock();
    if args.json {
        let listing = watches
            .iter()
            .map(|watch| listing_json(watch, clock, evaluating))
            .collect::<Vec<_>>();
        println!("{}", serde_json::to_string_pretty(&listing)?);
        return Ok(0);
    }
    if watches.is_empty() {
        match scope {
            Some(slug) => println!("{slug} has no watches (wt watch --all lists every worktree's)"),
            None => println!("no watches"),
        }
        return Ok(0);
    }
    for watch in &watches {
        println!("{}", listing_text(watch, clock, evaluating));
    }
    if !evaluating {
        println!(
            "\nnothing is evaluating watches right now: they are checked while the wt TUI is open for this repository"
        );
    }
    Ok(0)
}

async fn cancel(ctx: &AppContext, ids: &[String], mine: bool, slug: Option<&str>) -> Result<i32> {
    let by = author();
    let cancelled_by = (by != "human").then(|| by.clone());
    if mine {
        let inventory = Inventory::read(ctx).await;
        let owner = match &inventory {
            Ok(inventory) => owner_in(inventory, slug, "a watch"),
            Err(_) if slug.is_none() => {
                Err("not inside a worktree: run this in one, or pass --slug <slug>".into())
            }
            Err(error) => Err(error.to_string()),
        };
        let slug = match owner {
            Ok(slug) => slug,
            Err(message) => {
                eprintln!("{message}");
                return Ok(1);
            }
        };
        let (owner_slug, setter) = (slug.clone(), by.clone());
        let cancelled = ctx
            .database
            .call(move |store| {
                Ok(store.cancel_watches(
                    WatchSelector::SetBy {
                        slug: &owner_slug,
                        by: &setter,
                    },
                    cancelled_by.as_deref(),
                )?)
            })
            .await?;
        if cancelled.is_empty() {
            println!("{by} has set no watches on {slug}");
        }
        cancelled.iter().for_each(print_cancelled);
        print_lists_after_cancel(ctx, inventory.ok(), &cancelled).await;
        return Ok(0);
    }
    if ids.is_empty() {
        eprintln!("name the watches to cancel (ids are listed by `wt watch`), or pass --mine");
        return Ok(2);
    }
    let wanted = ids.to_vec();
    let cancelled = ctx
        .database
        .call(move |store| {
            Ok(store.cancel_watches(WatchSelector::Ids(&wanted), cancelled_by.as_deref())?)
        })
        .await?;
    cancelled.iter().for_each(print_cancelled);
    print_lists_after_cancel(ctx, None, &cancelled).await;
    let missing = ids
        .iter()
        .filter(|id| !cancelled.iter().any(|watch| watch.id == **id))
        .collect::<Vec<_>>();
    let due = ctx
        .database
        .call(|store| {
            let mut due = store.list_owed(None)?;
            due.extend(
                store
                    .read_removed_worktrees()?
                    .iter()
                    .flat_map(wt_store::RemovedWorktree::owed),
            );
            Ok(due)
        })
        .await
        .unwrap_or_default();
    for id in &missing {
        eprintln!(
            "{}",
            missing_watch(id, due.iter().find(|step| step.id == **id))
        );
    }
    Ok(i32::from(!missing.is_empty()))
}

/// Why `id` names no watch to cancel. A wake that was delivered after its
/// worktree's work landed is no longer a watch: it is a step that is due,
/// and it is closed, not cancelled.
fn missing_watch(id: &str, due: Option<&wt_core::OwedRecord>) -> String {
    match due {
        Some(step) => format!(
            "{id} is not a pending watch: its wake was delivered and it is now a step {} owes (\"{}\"). Close it with: wt done {id} --slug {} -m \"<what you did and what you saw>\"",
            step.slug,
            wt_app_core::watch_view::headline(&step.what),
            step.slug
        ),
        None => format!(
            "no watch {id}: it may have fired or been cancelled already (wt watch --all lists the pending ones)"
        ),
    }
}

async fn check(ctx: &AppContext, conditions: &[String]) -> Result<i32> {
    let config = &ctx.config;
    let clock = clock();
    let live = match inventory(ctx).await {
        Ok(inventory) => live_slugs(&inventory),
        Err(message) => {
            eprintln!("{message}");
            return Ok(1);
        }
    };
    let named = conditions
        .iter()
        .filter_map(|arg| parse(arg, clock).ok())
        .map(|cond| cond.cond)
        .collect::<Vec<_>>();
    let removed = removed_for(ctx, &named, &live).await;
    let mut failed = false;
    for arg in conditions {
        let cond = match parse(arg, clock) {
            Ok(cond) => cond,
            Err(error) => {
                eprintln!("{error}");
                failed = true;
                continue;
            }
        };
        println!("{arg}: {}", cond.describe(clock));
        if let Condition::At { .. } = cond.cond {
            println!("  at {} local time", cond.to_arg(clock.utc_offset_minutes));
        }
        if let Some(warning) = absent_worktree(&cond.cond, &live, &removed) {
            println!("  warning: {warning}");
        }
        match config_fact(config, &cond.cond) {
            Ok(Some((fact, _))) => println!("  {fact}"),
            Ok(None) => {}
            Err(problem) => {
                eprintln!("  {problem}");
                failed = true;
            }
        }
    }
    Ok(if failed { 2 } else { 0 })
}

#[cfg(test)]
mod tests {
    use super::*;
    use wt_app_core::test_support::CommandFixture;
    use wt_core::watch::Delay;

    const NOON: i64 = 1_795_521_600_000;

    fn stored(conditions: &[&str]) -> Watch {
        let clock = LocalClock {
            now_ms: NOON,
            utc_offset_minutes: 60,
        };
        Watch {
            id: "aaaaaa".into(),
            slug: "one".into(),
            incarnation: None,
            by: "one".into(),
            created_at_ms: NOON,
            utc_offset_minutes: 60,
            when: conditions
                .iter()
                .map(|arg| parse(arg, clock).unwrap())
                .collect(),
            then: WatchActions {
                unblock: Some("the release".into()),
                wake: Some("Run the smoke test.\nThen report.".into()),
                ..WatchActions::default()
            },
            met_at_ms: None,
            last_error: None,
            owner_gone: false,
            extra: Default::default(),
        }
    }

    #[test]
    fn a_wake_that_stands_in_for_an_owed_check_is_warned_about() {
        let owing = wt_core::WorkStatusRecord::new(wt_core::WorkState::Ready, "t").with_check(
            wt_core::VerifyStage::Production,
            "Open /health and expect 200",
        );
        let wake = |conditions: &[&str], prompt: &str| {
            let mut watch = stored(conditions);
            watch.then.wake = Some(prompt.into());
            watch
        };
        // The prompt says it runs the owed check, whatever it waits for.
        let warning = duplicates_check(
            Some(&owing),
            &wake(&["3h"], "Run the verify-in-production steps"),
        )
        .unwrap();
        assert!(
            warning.contains("wt done production --again <time>"),
            "{warning}"
        );
        assert!(warning.contains("wt watch cancel aaaaaa"), "{warning}");
        for prompt in [
            "VERIFY the deploy",
            "do the verification",
            "do the owed check now",
            "run what is owed in production",
            "the owed post-merge check",
            "Open /health and expect 200",
        ] {
            assert!(
                duplicates_check(Some(&owing), &wake(&["3h"], prompt)).is_some(),
                "{prompt}"
            );
        }
        // Naming the check's stage, or words that only contain the others,
        // is a second step like any other.
        for (when, prompt) in [
            ("production", "rotate the old key"),
            ("production", "look at the logs"),
            ("3h", "run the checks again"),
            ("3h", "clean the checkout"),
            ("3h", "run the production check"),
            ("3h", "see what the reverification did"),
            (
                "3h",
                "the owed reply, then the long list of other things in production",
            ),
            ("staging", "tail the worker"),
            ("3h", "rebase onto main"),
        ] {
            assert_eq!(
                duplicates_check(Some(&owing), &wake(&[when], prompt)),
                None,
                "{prompt}"
            );
        }
        assert_eq!(
            duplicates_check(None, &wake(&["production"], "do the owed check")),
            None
        );
    }

    #[test]
    fn a_listed_condition_reads_from_its_stored_record() {
        let mut watch = stored(&["17:00", "merged", "green+1h", "check:secret=KEY", "#7"]);
        let state = |watch: &Watch, index: usize, now, evaluating| {
            shown(watch, &watch.when[index], now, evaluating)
        };
        // A time needs no engine; anything else is unknown without one.
        assert_eq!(state(&watch, 0, NOON, false), Shown::NotYet(None));
        assert_eq!(state(&watch, 0, NOON + 5 * 3_600_000, false), Shown::Holds);
        assert!(matches!(state(&watch, 1, NOON, false), Shown::Unknown(_)));
        // "Not yet" is an answer, and only an engine that judged the watch
        // has given one.
        assert_eq!(state(&watch, 1, NOON, true), Shown::NotEvaluated);
        let evaluated = wt_core::watch::WatchStamps {
            evaluated_at_ms: Some(NOON),
            ..wt_core::watch::WatchStamps::new(&watch.id)
        };
        evaluated.apply(&mut watch);
        assert_eq!(state(&watch, 1, NOON, true), Shown::NotYet(None));
        assert!(matches!(state(&watch, 1, NOON, false), Shown::Unknown(_)));

        watch.when[1].first_held_at_ms = Some(NOON);
        assert_eq!(state(&watch, 1, NOON, false), Shown::Holds);
        watch.when[2].first_held_at_ms = Some(NOON);
        assert_eq!(watch.when[2].delay, Delay::parse("1h"));
        assert_eq!(
            state(&watch, 2, NOON + 60_000, true),
            Shown::NotYet(Some("holding, 1h not yet over".into()))
        );
        assert_eq!(state(&watch, 2, NOON + 3_600_000, true), Shown::Holds);

        // The watch's one recorded problem is shown on the condition it is about.
        watch.last_error = Some("check secret exited 3: vault sealed".into());
        // The condition already names the check.
        assert_eq!(
            state(&watch, 3, NOON, true),
            Shown::Unknown("exited 3: vault sealed".into())
        );
        assert_eq!(state(&watch, 4, NOON, true), Shown::NotYet(None));
        watch.last_error = Some("#7 was closed without merging; still waiting".into());
        assert_eq!(state(&watch, 3, NOON, true), Shown::NotYet(None));
        assert!(matches!(state(&watch, 4, NOON, true), Shown::Unknown(_)));

        watch.met_at_ms = Some(NOON);
        assert_eq!(state(&watch, 4, NOON, false), Shown::Holds);
    }

    #[test]
    fn listings_show_each_condition_and_what_follows() {
        let mut watch = stored(&["17:00", "merged", "pr:7:production", "green"]);
        watch.when[1].first_held_at_ms = Some(NOON);
        let clock = LocalClock {
            now_ms: NOON + 60_000,
            utc_offset_minutes: 60,
        };
        assert_eq!(
            listing_text(&watch, clock, true),
            "aaaaaa  one  set by one, 1m ago\n  not yet  17:00\n  holds    merged\n  not evaluated yet #7 in production (the wt TUI evaluates watches)\n  not evaluated yet PR checks green (the wt TUI evaluates watches)\n  then: clear the gate \"the release\"\n  then: wake the session: Run the smoke test."
        );
        let listed = listing_json(&watch, clock, true);
        // A condition prints as it can be typed back, without a # for a
        // shell to take as a comment.
        assert_eq!(listed["when"][2]["arg"], "pr:7:production");
        assert_eq!(listed["when"][2]["state"], "not_evaluated_yet");
        assert_eq!(listed["evaluated_at"], Value::Null);
        let evaluated = wt_core::watch::WatchStamps {
            evaluated_at_ms: Some(NOON),
            ..wt_core::watch::WatchStamps::new(&watch.id)
        };
        evaluated.apply(&mut watch);
        let listed = listing_json(&watch, clock, true);
        assert_eq!(listed["when"][3]["state"], "not_yet");
        assert_eq!(listed["evaluated_at"], "2026-11-24T12:00:00Z");
        watch.when.truncate(2);
        assert_eq!(listed["when"][0]["arg"], "2026-11-24 17:00");
        assert_eq!(listed["when"][0]["state"], "not_yet");
        assert_eq!(listed["when"][1]["state"], "holds");
        assert_eq!(listed["then"]["unblock"], "the release");
        assert_eq!(listed["met_at"], Value::Null);
        assert_eq!(listed["created_at"], "2026-11-24T12:00:00Z");

        // The row surfaces carry the same watch without evaluation state,
        // and only for its own worktree.
        let row = watches_json(std::slice::from_ref(&watch), "one", clock);
        assert_eq!(row[0]["id"], "aaaaaa");
        assert_eq!(row[0]["by"], "one");
        assert_eq!(
            row[0]["when"][1],
            json!({"arg": "merged", "description": "merged"})
        );
        assert_eq!(row[0]["then"]["wake"], "Run the smoke test.\nThen report.");
        assert_eq!(row[0]["met_at"], Value::Null);
        assert_eq!(watches_json(&[watch], "two", clock), json!([]));
    }

    #[test]
    fn a_problem_shown_beside_its_condition_is_not_said_twice() {
        let mut watch = stored(&["check:secret=KEY", "merged"]);
        watch.then.unblock = None;
        watch.last_error = Some("check secret exited 3: vault sealed".into());
        let clock = LocalClock {
            now_ms: NOON + 60_000,
            utc_offset_minutes: 60,
        };
        let listed = listing_text(&watch, clock, true);
        assert!(
            listed.contains("unknown  check secret=KEY (exited 3: vault sealed)"),
            "{listed}"
        );
        assert!(!listed.contains("problem:"), "{listed}");
        // No engine runs, so no condition carries it: it gets its own line.
        let idle = listing_text(&watch, clock, false);
        assert!(
            idle.ends_with("\n  problem: check secret exited 3: vault sealed"),
            "{idle}"
        );
        // A problem about none of the conditions.
        watch.last_error = Some("#9 was closed without merging; still waiting".into());
        assert!(
            listing_text(&watch, clock, true)
                .ends_with("\n  problem: #9 was closed without merging; still waiting")
        );
    }

    #[test]
    fn setting_a_watch_says_at_for_a_time_and_when_for_an_event() {
        // Noon UTC is 13:00 here, so 01:01 is tomorrow's.
        let timed = stored(&["01:01"]);
        assert_eq!(
            set_text(&timed, "wakes wake-demo", false),
            "✓ watch aaaaaa: wakes wake-demo at 01:01 tomorrow"
        );
        assert_eq!(
            set_text(&stored(&["check:flag=go"]), "wakes one", true),
            "already set: watch aaaaaa: wakes one when check flag=go"
        );
        assert_eq!(
            set_text(&stored(&["merged", "17:00"]), "tells you", false),
            "✓ watch aaaaaa: tells you when merged and 17:00"
        );
    }

    #[tokio::test]
    async fn conditions_are_judged_against_the_configuration() {
        let fixture = CommandFixture::new().await.unwrap();
        let listed = inventory(&fixture.ctx).await.unwrap();
        let mut config = (*fixture.ctx.config).clone();
        let fact =
            |config: &Config, arg: &str| config_fact(config, &parse(arg, clock()).unwrap().cond);
        assert!(
            fact(&config, "check:secret=KEY")
                .unwrap_err()
                .contains("no check is configured")
        );
        config.checks.push(wt_config::CheckConfig {
            id: "secret".into(),
            description: "a secret is present".into(),
            run: vec!["vault".into(), "{{arg}}".into()],
            ..wt_config::CheckConfig::default()
        });
        assert_eq!(
            fact(&config, "check:secret=KEY").unwrap().unwrap(),
            (
                "check secret: a secret is present; runs every 300s".into(),
                false
            )
        );
        assert!(
            fact(&config, "check:other")
                .unwrap_err()
                .contains("Configured checks: secret")
        );
        assert_eq!(fact(&config, "merged").unwrap(), None);
        // A stage word that means less than it says is worth a note.
        let (note, unasked) = fact(&config, "production").unwrap().unwrap();
        assert!(
            unasked && note.contains("production means merged to main"),
            "{note}"
        );
        let (note, _) = fact(&config, "release:staging").unwrap().unwrap();
        assert!(note.contains("any new commit on main"), "{note}");
        config.environments.production = Some(wt_config::EnvironmentConfig {
            deployed: vec!["x".into()],
            ..wt_config::EnvironmentConfig::default()
        });
        let (note, unasked) = fact(&config, "#7:production").unwrap().unwrap();
        assert!(!unasked && note.contains("deploy probe reports"), "{note}");

        let mut ctx = fixture.ctx.clone();
        ctx.config = std::sync::Arc::new(config);
        assert!(
            read_conditions(&ctx, &listed, &["check:other".into()], clock())
                .await
                .is_err()
        );
        assert!(
            read_conditions(&ctx, &listed, &["tomorrow".into()], clock())
                .await
                .err()
                .unwrap()
                .to_string()
                .contains("A condition is one of")
        );
        // With a probe and nothing deployed yet there is no baseline to take.
        let read = read_conditions(
            &ctx,
            &listed,
            &["release:production".into(), "staging".into()],
            clock(),
        )
        .await
        .unwrap();
        assert_eq!(read.when[0].baseline, None);
        assert_eq!(read.notes.len(), 1);
        // Without one, the stage branch's tip is the release to wait past.
        let read = read_conditions(&fixture.ctx, &listed, &["release:staging".into()], clock())
            .await
            .unwrap();
        assert_eq!(read.when[0].baseline.as_deref().map(str::len), Some(40));
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn watches_are_set_listed_and_cancelled() {
        let fixture = CommandFixture::new().await.unwrap();
        let ctx = &fixture.ctx;
        let listed = inventory(ctx).await.unwrap();
        let wake = |conditions: &[&str], slug: Option<&str>| WakeArgs {
            conditions: conditions.iter().map(|arg| (*arg).to_owned()).collect(),
            message: Some("Run the smoke test".into()),
            slug: slug.map(str::to_owned),
            before_release: false,
        };
        assert_eq!(
            run_wake(ctx, &wake(&["+3h", "pr:7:production"], None))
                .await
                .unwrap(),
            0
        );
        // What a shell leaves of `wt wake #7:production -m "..."`.
        let lost = WakeArgs {
            conditions: Vec::new(),
            message: None,
            slug: None,
            before_release: false,
        };
        assert_eq!(run_wake(ctx, &lost).await.unwrap(), 2);
        assert_eq!(
            clean_prompt("\n1. Deploy.\x1b[2J\n\n2. Verify.\n").as_deref(),
            Some("1. Deploy.\n\n2. Verify.")
        );
        let lost = NotifyArgs {
            conditions: Vec::new(),
            message: None,
            slug: None,
        };
        assert_eq!(run_notify(ctx, &lost).await.unwrap(), 2);
        for lost in [&[][..], &[String::new()][..]] {
            assert!(
                read_conditions(ctx, &listed, lost, clock())
                    .await
                    .err()
                    .unwrap()
                    .to_string()
                    .contains("write pr:3806")
            );
        }
        assert_eq!(run_wake(ctx, &wake(&["soonish"], None)).await.unwrap(), 2);
        assert_eq!(
            run_wake(ctx, &wake(&["check:nosuch"], None)).await.unwrap(),
            2
        );
        assert_eq!(
            run_wake(ctx, &wake(&["+3h"], Some("absent")))
                .await
                .unwrap(),
            1
        );
        let notify = NotifyArgs {
            conditions: vec!["merged".into()],
            message: None,
            slug: Some("one".into()),
        };
        assert_eq!(run_notify(ctx, &notify).await.unwrap(), 0);
        // The same command again (agents retry) sets nothing new.
        assert_eq!(run_notify(ctx, &notify).await.unwrap(), 0);
        // A session cannot be woken once its own worktree is gone; being
        // told about it is fine.
        assert_eq!(
            run_wake(ctx, &wake(&["one:gone"], Some("one")))
                .await
                .unwrap(),
            1
        );
        // A worktree that is not there is accepted, with a warning.
        let read = read_conditions(
            ctx,
            &listed,
            &[
                "ghost:merged".into(),
                "wt:ghost:gone".into(),
                "two:ready".into(),
            ],
            clock(),
        )
        .await
        .unwrap();
        assert_eq!(
            read.warnings,
            [
                "no worktree named ghost right now; this waits until one exists and reaches merged",
                "no worktree named ghost right now, so this holds already",
            ]
        );
        // A worktree that was removed is judged by its removal record, and
        // the warning says how.
        ctx.database
            .call(|store| {
                let removed = |slug: &str, landed: &str, commit: Option<&str>| {
                    serde_json::from_value::<wt_store::RemovedWorktree>(json!({
                        "slug": slug, "branch": format!("feature/{slug}"),
                        "removedAt": "2026-11-27T12:00:00Z", "landedOnAtRemoval": landed,
                        "prMergeCommitOid": commit,
                    }))
                };
                store.record_removed_worktrees(
                    &[
                        removed("landed", "base", Some("c0ffee1"))?,
                        removed("abandoned", "unlanded", None)?,
                    ],
                    1_795_000_000_000,
                )?;
                Ok(())
            })
            .await
            .unwrap();
        let read = read_conditions(
            ctx,
            &listed,
            &[
                "wt:landed:merged".into(),
                "wt:landed:staging".into(),
                "wt:landed:ready".into(),
                "wt:abandoned:merged".into(),
            ],
            clock(),
        )
        .await
        .unwrap();
        assert_eq!(
            read.warnings,
            [
                "landed was removed after its work merged, so this holds already",
                "landed was removed after its work merged; this follows the commit that landed it to staging",
                "landed was removed and can no longer become ready",
                "abandoned was removed without merging, so this cannot hold unless a worktree named abandoned is created again and merges",
            ]
        );
        // The argument of a check is one argv entry of someone's command.
        assert_eq!(
            run_wake(ctx, &wake(&["check:secret=--force"], None))
                .await
                .unwrap(),
            2
        );

        let list = || async {
            ctx.database
                .call(|store| Ok(store.list_watches(Some("one"))?))
                .await
                .unwrap()
        };
        let watches = list().await;
        assert_eq!(watches.len(), 2);
        assert_eq!(watches[0].then.wake.as_deref(), Some("Run the smoke test"));
        assert_eq!(watches[0].by, author());
        assert_eq!(watches[0].when.len(), 2);
        assert_eq!(watches[1].then.notify.as_deref(), Some(""));

        let cancel_ids = |ids: &[&str]| WatchArgs {
            command: Some(WatchCommand::Cancel {
                ids: ids.iter().map(|id| (*id).to_owned()).collect(),
                mine: false,
                slug: None,
            }),
            all: false,
            json: false,
            slug: None,
        };
        assert_eq!(run_watch(ctx, &cancel_ids(&[])).await.unwrap(), 2);
        assert_eq!(
            run_watch(ctx, &cancel_ids(&[&watches[0].id, "nosuch"]))
                .await
                .unwrap(),
            1
        );
        assert_eq!(list().await.len(), 1);
        let mine = WatchArgs {
            command: Some(WatchCommand::Cancel {
                ids: Vec::new(),
                mine: true,
                slug: None,
            }),
            all: false,
            json: false,
            slug: None,
        };
        assert_eq!(run_watch(ctx, &mine).await.unwrap(), 0);
        assert_eq!(list().await, []);
        let show = WatchArgs {
            command: None,
            all: true,
            json: true,
            slug: None,
        };
        assert_eq!(run_watch(ctx, &show).await.unwrap(), 0);
        let checked = WatchArgs {
            command: Some(WatchCommand::Check {
                conditions: vec!["17:00".into(), "release:production+24h".into()],
            }),
            ..show
        };
        assert_eq!(run_watch(ctx, &checked).await.unwrap(), 0);
        fixture.close().await.unwrap();
    }

    #[test]
    fn cancelling_a_due_steps_id_says_it_is_due_and_how_to_close_it() {
        let step = wt_core::OwedRecord {
            id: "93co6q".into(),
            slug: "one".into(),
            incarnation: None,
            by: "one".into(),
            what: "confirm prod logs\nthen report".into(),
            when_text: "in production".into(),
            due_at_ms: 0,
            utc_offset_minutes: 0,
            unconfirmed: false,
            before_release: false,
            extra: Default::default(),
        };
        assert_eq!(
            missing_watch("93co6q", Some(&step)),
            "93co6q is not a pending watch: its wake was delivered and it is now a step one owes (\"confirm prod logs\"). Close it with: wt done 93co6q --slug one -m \"<what you did and what you saw>\""
        );
        assert!(missing_watch("zzzzzz", None).starts_with("no watch zzzzzz: "));
    }
}
