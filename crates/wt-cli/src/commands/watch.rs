//! `wt wake`, `wt notify` and `wt watch`: set, list and cancel watches. A
//! watch names conditions and what happens once they all hold; the engine
//! in the TUI's host evaluates and delivers them.

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
use wt_ops::commands::resolve::resolve_worktree;
use wt_store::{NewWatch, WatchRefusal, WatchSelector};

const CONDITION_HELP: &str = "What to wait for; all must hold. One argument each: a local time (17:00, +3h, 2026-11-25T17:00), this worktree (merged, staging, production, ready, green, verified, closed), a pull request (pr:3806, pr:3806:production), another worktree (<slug>:merged, <slug>:gone), release:production, check:<id>[=<arg>]. Anything but a time may end in +<duration>: it must have held that long";

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
    /// then, met_at, last_error, evaluated_at, evaluating.
    #[arg(long)]
    pub json: bool,
    /// The worktree whose watches to list (default: the current one).
    #[arg(long, value_name = "SLUG", conflicts_with = "all")]
    pub slug: Option<String>,
}

#[derive(Debug, Clone, Subcommand)]
pub enum WatchCommand {
    /// Cancel watches by id, or every watch you set on this worktree.
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

/// Who is setting or cancelling: the agent's identity, or the person.
pub(crate) fn author() -> String {
    std::env::var("WT_AGENT")
        .ok()
        .map(|by| by.trim().to_owned())
        .filter(|by| !by.is_empty())
        .unwrap_or_else(|| "human".into())
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

fn no_conditions(args: &[String]) -> bool {
    args.is_empty() || args.iter().any(|arg| arg.trim().is_empty())
}

pub(crate) struct ReadConditions {
    pub when: Vec<WatchCondition>,
    /// What the conditions mean here that their words do not say.
    pub notes: Vec<String>,
}

/// Read condition arguments for a watch about to be set. A release
/// condition takes the stage's current release as its baseline when that is
/// known now; otherwise the engine takes the first one it sees.
pub(crate) async fn read_conditions(
    ctx: &AppContext,
    args: &[String],
    clock: LocalClock,
) -> Result<ReadConditions, String> {
    if no_conditions(args) {
        return Err(NO_CONDITIONS.into());
    }
    let mut read = ReadConditions {
        when: Vec::new(),
        notes: Vec::new(),
    };
    for arg in args {
        let mut cond = parse(arg, clock).map_err(|error| error.to_string())?;
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

/// The worktree a watch is set on: the named one or the current one.
async fn owner(ctx: &AppContext, slug: Option<&str>) -> Result<String, String> {
    match resolve_worktree(ctx, slug).await {
        Ok(record) if !record.is_main => Ok(record.target.slug().to_owned()),
        Ok(_) => Err(
            "a watch belongs to a worktree and the main clone is not one: pass --slug <slug>"
                .into(),
        ),
        Err(_) if slug.is_none() => {
            Err("not inside a worktree: run this in one, or pass --slug <slug>".into())
        }
        Err(error) => Err(error.to_string()),
    }
}

/// Store a watch. `Err` carries what to do instead.
pub(crate) async fn set(
    ctx: &AppContext,
    slug: &str,
    when: Vec<WatchCondition>,
    then: WatchActions,
    clock: LocalClock,
) -> Result<Result<Watch, String>> {
    let new = NewWatch {
        slug: slug.to_owned(),
        by: author(),
        utc_offset_minutes: clock.utc_offset_minutes,
        when,
        then,
    };
    let added = ctx
        .database
        .call(move |store| Ok(store.add_watch(new, clock.now_ms)?))
        .await?;
    Ok(added.map_err(|refusal| match refusal {
        WatchRefusal::TooManyForSlug { .. } | WatchRefusal::TooMany => {
            format!("{refusal}: list them with `wt watch --all` and cancel with `wt watch cancel <id>`")
        }
        WatchRefusal::GateChanged { .. } => {
            format!("{refusal}; set the gate and its condition together: wt status --blocked-on \"<why>\" --until <condition>")
        }
        other => other.to_string(),
    }))
}

async fn report_set(ctx: &AppContext, watch: &Watch, does: &str, notes: &[String]) {
    println!("✓ watch {}: {does} when {}", watch.id, watch.summary());
    for note in notes {
        println!("  note: {note}");
    }
    println!("  cancel: wt watch cancel {}", watch.id);
    if !super::automations::engine_running(ctx).await {
        println!(
            "  nothing is evaluating watches right now: they are checked while the wt TUI is open for this repository"
        );
    }
}

fn clean(text: &str) -> Option<String> {
    let text = wt_core::sanitize_work_note(text);
    (!text.is_empty()).then_some(text)
}

pub async fn run_wake(ctx: &AppContext, args: &WakeArgs) -> Result<i32> {
    // Before -m: a shell comment takes the conditions and -m together.
    if no_conditions(&args.conditions) {
        eprintln!("{NO_CONDITIONS}");
        return Ok(2);
    }
    let Some(prompt) = args.message.as_deref().and_then(clean) else {
        eprintln!("-m needs what the session should do once the conditions hold");
        return Ok(2);
    };
    let then = WatchActions {
        wake: Some(prompt),
        ..WatchActions::default()
    };
    set_and_report(ctx, args.slug.as_deref(), &args.conditions, then).await
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
    set_and_report(ctx, args.slug.as_deref(), &args.conditions, then).await
}

async fn set_and_report(
    ctx: &AppContext,
    slug: Option<&str>,
    conditions: &[String],
    then: WatchActions,
) -> Result<i32> {
    let slug = match owner(ctx, slug).await {
        Ok(slug) => slug,
        Err(message) => {
            eprintln!("{message}");
            return Ok(1);
        }
    };
    let clock = clock();
    let read = match read_conditions(ctx, conditions, clock).await {
        Ok(read) => read,
        Err(message) => {
            eprintln!("{message}");
            return Ok(2);
        }
    };
    let does = if then.wake.is_some() {
        format!("wakes {slug}")
    } else {
        "tells you".to_owned()
    };
    match set(ctx, &slug, read.when, then, clock).await? {
        Ok(watch) => {
            report_set(ctx, &watch, &does, &read.notes).await;
            Ok(0)
        }
        Err(message) => {
            eprintln!("{message}");
            Ok(1)
        }
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
    if let Some(error) = &watch.last_error {
        out.push_str(&format!("\n  problem: {error}"));
    }
    out
}

pub async fn run_watch(ctx: &AppContext, args: &WatchArgs) -> Result<i32> {
    match &args.command {
        Some(WatchCommand::Cancel { ids, mine, slug }) => {
            return cancel(ctx, ids, *mine, slug.as_deref()).await;
        }
        Some(WatchCommand::Check { conditions }) => return Ok(check(&ctx.config, conditions)),
        None => {}
    }
    // Outside a worktree there is no "this worktree": show them all.
    let scope = if args.all {
        None
    } else {
        match owner(ctx, args.slug.as_deref()).await {
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
        let slug = match owner(ctx, slug).await {
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
        for watch in cancelled {
            println!("✓ cancelled {} ({})", watch.id, watch.summary());
        }
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
    for watch in &cancelled {
        println!("✓ cancelled {} ({})", watch.id, watch.summary());
    }
    let missing = ids
        .iter()
        .filter(|id| !cancelled.iter().any(|watch| watch.id == **id))
        .collect::<Vec<_>>();
    for id in &missing {
        eprintln!(
            "no watch {id}: it may have fired or been cancelled already (wt watch --all lists the pending ones)"
        );
    }
    Ok(i32::from(!missing.is_empty()))
}

fn check(config: &Config, conditions: &[String]) -> i32 {
    let clock = clock();
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
        match config_fact(config, &cond.cond) {
            Ok(Some((fact, _))) => println!("  {fact}"),
            Ok(None) => {}
            Err(problem) => {
                eprintln!("  {problem}");
                failed = true;
            }
        }
    }
    if failed { 2 } else { 0 }
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
        assert_eq!(
            state(&watch, 3, NOON, true),
            Shown::Unknown("check secret exited 3: vault sealed".into())
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
            "aaaaaa  one  set by one, 1m ago\n  not yet  17:00 today\n  holds    merged\n  not evaluated yet #7 in production (the wt TUI evaluates watches)\n  not evaluated yet PR checks green (the wt TUI evaluates watches)\n  then: clear the gate \"the release\"\n  then: wake the session: Run the smoke test."
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

    #[tokio::test]
    async fn conditions_are_judged_against_the_configuration() {
        let fixture = CommandFixture::new().await.unwrap();
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
            read_conditions(&ctx, &["check:other".into()], clock())
                .await
                .is_err()
        );
        assert!(
            read_conditions(&ctx, &["tomorrow".into()], clock())
                .await
                .err()
                .unwrap()
                .contains("A condition is one of")
        );
        // With a probe and nothing deployed yet there is no baseline to take.
        let read = read_conditions(
            &ctx,
            &["release:production".into(), "staging".into()],
            clock(),
        )
        .await
        .unwrap();
        assert_eq!(read.when[0].baseline, None);
        assert_eq!(read.notes.len(), 1);
        // Without one, the stage branch's tip is the release to wait past.
        let read = read_conditions(&fixture.ctx, &["release:staging".into()], clock())
            .await
            .unwrap();
        assert_eq!(read.when[0].baseline.as_deref().map(str::len), Some(40));
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn watches_are_set_listed_and_cancelled() {
        let fixture = CommandFixture::new().await.unwrap();
        let ctx = &fixture.ctx;
        let wake = |conditions: &[&str], slug: Option<&str>| WakeArgs {
            conditions: conditions.iter().map(|arg| (*arg).to_owned()).collect(),
            message: Some("Run the smoke test".into()),
            slug: slug.map(str::to_owned),
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
        };
        assert_eq!(run_wake(ctx, &lost).await.unwrap(), 2);
        let lost = NotifyArgs {
            conditions: Vec::new(),
            message: None,
            slug: None,
        };
        assert_eq!(run_notify(ctx, &lost).await.unwrap(), 2);
        for lost in [&[][..], &[String::new()][..]] {
            assert!(
                read_conditions(ctx, lost, clock())
                    .await
                    .err()
                    .unwrap()
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
}
