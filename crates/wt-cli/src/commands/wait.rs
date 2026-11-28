//! `wt wait`: block until conditions hold. The conditions are a watch's,
//! read by the same helper `wt wake` uses and judged by the same
//! evaluator, but in this process and against facts it reads itself, so it
//! works while no TUI is evaluating watches. Nothing is stored: no watch,
//! nothing owed, no history. The wait ends with the process.

use std::time::{Duration, Instant};

use anyhow::Result;
use clap::Args;
use serde_json::{Value, json};
use wt_app_core::context::AppContext;
use wt_core::watch::{
    Condition, Delay, LocalClock, RowState, Truth, Watch, WatchActions, WatchCondition, evaluate,
    needs,
};
use wt_sources::{
    wait_facts::{Changed, RowReader, RowsRead, Signals},
    watch_facts::{Externals, GIT_INTERVAL, facts},
};

use super::watch::{CONDITION_HELP, NO_CONDITIONS, author, clock, no_conditions, now_ms, owner_in};

const TIMEOUT_DEFAULT: &str = "30m";
const TIMEOUT_MAX_MS: i64 = 24 * 60 * 60 * 1_000;
/// What `timeout(1)` exits with.
const EXIT_TIMED_OUT: i32 = 124;
const EXIT_INTERRUPTED: i32 = 130;
/// A change on disk is acted on at once, and the next one no sooner than
/// this: a busy fleet writes its state many times a second, and each look
/// parses that state and may run Git.
const SIGNAL_FLOOR: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Args)]
pub struct WaitArgs {
    // Not required by the parser, for the reason `wt wake` gives.
    #[arg(value_name = "CONDITION", help = CONDITION_HELP)]
    pub conditions: Vec<String>,
    /// Return when the first condition holds, instead of when all do.
    #[arg(long)]
    pub any: bool,
    /// Give up after this long with exit 124: 45s, 20m, 2h, at most 24h.
    #[arg(long, value_name = "DURATION", default_value = TIMEOUT_DEFAULT)]
    pub timeout: String,
    /// The worktree the conditions are about (default: the current one).
    #[arg(long, value_name = "SLUG")]
    pub slug: Option<String>,
    /// Print one object: result (held, timed_out, unknown or interrupted),
    /// any, held (the arguments that hold), waited_seconds, conditions
    /// (arg, description, state: holds, not_yet or unknown, and reason).
    #[arg(long)]
    pub json: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ended {
    Held,
    TimedOut,
    Interrupted,
    /// What is asked can no longer hold, so there is nothing to wait for.
    Cannot,
}

/// How a wait ended, with each condition's last truth.
#[derive(Debug)]
struct Outcome {
    ended: Ended,
    truths: Vec<Truth>,
    /// The conditions as last judged: one may have been replaced by the
    /// commit it came to follow.
    when: Vec<WatchCondition>,
    /// Why pull-request answers were missing at the end, when they were.
    problem: Option<String>,
    /// What GitHub said of a condition that holds, where it said more than
    /// the condition does: why a merge was dropped.
    why: Vec<Option<String>>,
    /// Why a condition can no longer hold, where it cannot.
    never: Vec<Option<String>>,
    waited: Duration,
}

/// Whether waiting on is pointless: a condition that must hold cannot, or
/// with `--any` none of them can.
fn cannot_hold(never: &[Option<String>], any: bool) -> bool {
    let mut cannot = never.iter().map(Option::is_some);
    !never.is_empty()
        && if any {
            cannot.all(|cannot| cannot)
        } else {
            cannot.any(|cannot| cannot)
        }
}

/// A duration from now, to the millisecond. `wt wake` stores a whole second
/// so the time reads back as it prints; a wait stores nothing, and rounding
/// up would hold it for up to a second past what was asked.
fn exact_duration(arg: &str, cond: &mut WatchCondition, clock: LocalClock) {
    let text = arg.trim();
    let text = text.strip_prefix('+').unwrap_or(text);
    if matches!(cond.cond, Condition::At { .. })
        && text.starts_with(|first: char| first.is_ascii_digit())
        && let Some(delay) = Delay::parse(text)
    {
        cond.cond = Condition::At {
            at_ms: clock.now_ms.saturating_add(delay.as_ms()),
        };
    }
}

fn holds(truths: &[Truth], any: bool) -> bool {
    let mut held = truths.iter().map(|truth| *truth == Truth::Holds);
    !truths.is_empty()
        && if any {
            held.any(|held| held)
        } else {
            held.all(|held| held)
        }
}

impl Outcome {
    /// 0 held; 124 timed out; 1 timed out with nothing that still fails
    /// ever having been told, which waiting longer may not fix.
    fn exit_code(&self) -> i32 {
        match self.ended {
            Ended::Held => 0,
            Ended::Interrupted => EXIT_INTERRUPTED,
            Ended::Cannot => 1,
            Ended::TimedOut if self.untold() => 1,
            Ended::TimedOut => EXIT_TIMED_OUT,
        }
    }

    fn untold(&self) -> bool {
        self.truths
            .iter()
            .filter(|truth| **truth != Truth::Holds)
            .all(|truth| matches!(truth, Truth::Unknown(_)))
    }

    fn result(&self) -> &'static str {
        match self.ended {
            Ended::Held => "held",
            Ended::Interrupted => "interrupted",
            Ended::Cannot => "cannot_hold",
            Ended::TimedOut if self.untold() => "unknown",
            Ended::TimedOut => "timed_out",
        }
    }

    /// Each condition's state word and reason.
    fn state(&self, index: usize, now_ms: i64) -> (&'static str, Option<String>) {
        let cond = &self.when[index];
        match &self.truths[index] {
            Truth::Holds => ("holds", self.why.get(index).cloned().flatten()),
            Truth::NotYet => (
                "not_yet",
                cond.delay
                    .filter(|_| cond.first_held_at_ms.is_some_and(|since| since <= now_ms))
                    .map(|delay| format!("holding, {delay} not yet over")),
            ),
            Truth::Unknown(_) if self.never.get(index).is_some_and(Option::is_some) => {
                ("unknown", self.never[index].clone())
            }
            Truth::Unknown(reason) => {
                let about_row = matches!(cond.cond, Condition::Own(_) | Condition::Worktree { .. });
                let reason = self
                    .problem
                    .as_ref()
                    .filter(|_| about_row)
                    .unwrap_or(reason);
                ("unknown", Some(reason.clone()))
            }
        }
    }

    fn headline(&self, args: &WaitArgs) -> String {
        let waited = elapsed(self.waited);
        let held = self.held(&args.conditions);
        match self.ended {
            Ended::Held if args.any => format!("held after {waited}: {}", held.join(", ")),
            Ended::Held if held.len() == 1 => format!("held after {waited}: {}", held[0]),
            Ended::Held => format!("all {} conditions hold after {waited}", held.len()),
            Ended::Interrupted => format!("interrupted after {waited}"),
            Ended::Cannot => {
                format!("cannot hold, stopped after {waited}: see each condition's reason")
            }
            Ended::TimedOut if self.untold() => {
                format!("could not tell after {waited}: see each condition's reason")
            }
            Ended::TimedOut if args.any => format!("timed out after {waited}: none holds"),
            Ended::TimedOut => format!(
                "timed out after {waited}: {} of {} still does not hold",
                self.truths.len() - held.len(),
                self.truths.len()
            ),
        }
    }

    fn held<'a>(&self, args: &'a [String]) -> Vec<&'a str> {
        args.iter()
            .zip(&self.truths)
            .filter(|(_, truth)| **truth == Truth::Holds)
            .map(|(arg, _)| arg.as_str())
            .collect()
    }

    fn text(&self, args: &WaitArgs, clock: LocalClock) -> String {
        let mut lines = vec![self.headline(args)];
        for (index, arg) in args.conditions.iter().enumerate() {
            let (state, reason) = self.state(index, clock.now_ms);
            let state = state.replace('_', " ");
            // The argument is named beside its reading only where the
            // reading says more than the argument did.
            let described = self.when[index].describe(clock);
            let named = if *arg == described {
                described
            } else {
                format!("{arg}: {described}")
            };
            lines.push(match reason {
                Some(reason) => format!("  {named}: {state} ({reason})"),
                None => format!("  {named}: {state}"),
            });
        }
        if self.ended == Ended::TimedOut {
            lines.push(
                "  still likely soon: run wt wait again. Longer than this session: wt wake <condition>... -m \"<what to do then>\" and end the turn"
                    .into(),
            );
        }
        lines.join("\n")
    }

    fn json(&self, args: &WaitArgs, clock: LocalClock) -> Value {
        json!({
            "result": self.result(),
            "any": args.any,
            "held": self.held(&args.conditions),
            "waited_seconds": self.waited.as_secs(),
            "conditions": args.conditions.iter().enumerate().map(|(index, arg)| {
                let (state, reason) = self.state(index, clock.now_ms);
                json!({
                    "arg": arg,
                    "description": self.when[index].describe(clock),
                    "state": state,
                    "reason": reason,
                })
            }).collect::<Vec<_>>(),
        })
    }
}

/// `45s`, `12m`, `12m30s`, `2h05m`.
fn elapsed(waited: Duration) -> String {
    let seconds = waited.as_secs();
    match (seconds / 3600, seconds / 60 % 60, seconds % 60) {
        (0, 0, seconds) => format!("{seconds}s"),
        (0, minutes, 0) => format!("{minutes}m"),
        (0, minutes, seconds) => format!("{minutes}m{seconds:02}s"),
        (hours, minutes, _) => format!("{hours}h{minutes:02}m"),
    }
}

/// The timeout in milliseconds, or what to write instead.
fn timeout_ms(text: &str) -> Result<i64, String> {
    Delay::parse(text.trim())
        .map(Delay::as_ms)
        .filter(|ms| *ms <= TIMEOUT_MAX_MS)
        .ok_or_else(|| {
            format!(
                "--timeout takes a duration such as 45s, 20m or 2h, at most 24h, not `{text}`. A wait that outlives the session is wt wake <condition>... -m \"<what to do then>\""
            )
        })
}

/// A condition that cannot hold before the timeout, with what to do.
fn outlasts(
    arg: &str,
    cond: &WatchCondition,
    clock: LocalClock,
    timeout_ms: i64,
) -> Option<String> {
    let deadline = clock.now_ms.saturating_add(timeout_ms);
    let late = match cond.cond {
        Condition::At { at_ms } => at_ms > deadline,
        _ => cond.delay.is_some_and(|delay| delay.as_ms() > timeout_ms),
    };
    late.then(|| {
        format!(
            "`{arg}` cannot hold within the timeout: pass a longer --timeout (at most 24h), or wake the session then instead of holding it open: wt wake {arg} -m \"<what to do then>\""
        )
    })
}

/// Judge `watch` until it holds, the deadline passes or the process is
/// told to stop. `Err` when the first read fails outright.
async fn wait_until(
    ctx: &AppContext,
    mut watch: Watch,
    any: bool,
    timeout: Duration,
) -> Result<Outcome, String> {
    let started = Instant::now();
    let mut reader = RowReader::default();
    let mut externals = Externals::default();
    let mut rows = RowsRead::default();
    let mut rows_at: Option<Instant> = None;
    let mut rows_known = false;
    let mut problem = None;
    let reads_rows = watch
        .when
        .iter()
        .any(|cond| !matches!(cond.cond, Condition::At { .. }));
    // Before the first read, so nothing written during it is missed.
    let mut signals = Signals::new(ctx);
    let mut changed = Changed::default();
    let mut signals_at: Option<Instant> = None;
    // Read the rows again without waiting out the interval.
    let mut rows_due = false;
    // When the shared GitHub read may ask again.
    let mut github_due: Option<Instant> = None;
    loop {
        if changed.any() && signals_at.is_none_or(|at| at.elapsed() >= SIGNAL_FLOOR) {
            let changed = std::mem::take(&mut changed);
            signals_at = Some(Instant::now());
            if changed.git {
                externals.git_changed();
            }
            rows_due |= reads_rows
                && reader
                    .concerns(ctx, std::slice::from_ref(&watch), changed)
                    .await;
        }
        rows_due |= github_due.is_some_and(|due| Instant::now() >= due);
        if reads_rows && (rows_due || rows_at.is_none_or(|at| at.elapsed() >= GIT_INTERVAL)) {
            rows_due = false;
            github_due = None;
            match reader.read(ctx, std::slice::from_ref(&watch)).await {
                Ok(read) => {
                    problem.clone_from(&read.problem);
                    // No sooner than the pace allows, so the read this
                    // prompts finds GitHub askable.
                    github_due = read.github_due_ms.map(|due_ms| {
                        let wait = due_ms.saturating_sub(u64::try_from(now_ms()).unwrap_or(0));
                        Instant::now() + Duration::from_millis(wait.saturating_add(1))
                    });
                    rows = read;
                    rows_known = true;
                }
                Err(_) if ctx.cancellation.is_cancelled() => {}
                Err(error) if rows_at.is_none() => return Err(error),
                // What was read before is no longer current.
                Err(error) => {
                    rows = RowsRead::default();
                    rows_known = false;
                    problem = Some(error);
                }
            }
            rows_at = Some(Instant::now());
        }
        externals.request(ctx, &needs([&watch]), &rows.prs, &rows.branches);
        let now = now_ms();
        let known = facts(
            std::slice::from_ref(&watch),
            &rows.rows,
            rows_known,
            &rows.prs,
            &externals,
            &rows.removed,
        );
        let evaluation = evaluate(&watch, &known, now);
        if let Some(stamps) = &evaluation.stamps {
            stamps.apply(&mut watch);
        }
        let never = watch
            .when
            .iter()
            .map(|cond| wt_core::watch::never_holds(cond, &known))
            .collect::<Vec<_>>();
        let waited = started.elapsed();
        let ended = if holds(&evaluation.truths, any) {
            Some(Ended::Held)
        } else if ctx.cancellation.is_cancelled() {
            Some(Ended::Interrupted)
        } else if cannot_hold(&never, any) {
            Some(Ended::Cannot)
        } else if waited >= timeout {
            Some(Ended::TimedOut)
        } else {
            None
        };
        if let Some(ended) = ended {
            let why = watch
                .when
                .iter()
                .map(|cond| {
                    let slug = match &cond.cond {
                        Condition::Own(RowState::Evicted) => &watch.slug,
                        Condition::Worktree {
                            slug,
                            state: RowState::Evicted,
                        } => slug,
                        _ => return None,
                    };
                    rows.removals.get(slug).cloned()
                })
                .collect();
            return Ok(Outcome {
                ended,
                truths: evaluation.truths,
                when: watch.when,
                problem,
                why,
                never,
                waited,
            });
        }
        // The next moment something can change without a file changing:
        // the deadline, a time, a delay running out, the backstop read of
        // the rows, GitHub becoming askable, a held-back change coming due.
        let instants = watch
            .when
            .iter()
            .filter_map(WatchCondition::known_at_ms)
            .filter(|at| *at > now)
            .map(|at| Duration::from_millis(u64::try_from(at - now).unwrap_or(0)));
        let until = |at: Instant| at.saturating_duration_since(Instant::now());
        let backstop = rows_at
            .filter(|_| reads_rows)
            .map(|at| GIT_INTERVAL.saturating_sub(at.elapsed()));
        let held_back = signals_at
            .filter(|_| changed.any())
            .map(|at| SIGNAL_FLOOR.saturating_sub(at.elapsed()));
        let pause = instants
            .chain(backstop)
            .chain(github_due.filter(|_| reads_rows).map(until))
            .chain(held_back)
            .chain([timeout.saturating_sub(waited)])
            .min()
            .unwrap_or(GIT_INTERVAL);
        tokio::select! {
            () = ctx.cancellation.cancelled() => {}
            arrived = externals.arrived(), if externals.busy() => {
                if let Some(arrived) = arrived {
                    externals.absorb(arrived);
                }
            }
            seen = signals.changed() => changed.absorb(seen),
            () = tokio::time::sleep(pause) => {}
        }
    }
}

/// The exit code, and how the wait ended unless it never started.
async fn wait(ctx: &AppContext, args: &WaitArgs) -> Result<(i32, Option<Outcome>)> {
    let refuse = |code: i32, message: String| {
        eprintln!("{message}");
        Ok((code, None))
    };
    if no_conditions(&args.conditions) {
        return refuse(2, NO_CONDITIONS.into());
    }
    let timeout = match timeout_ms(&args.timeout) {
        Ok(timeout) => timeout,
        Err(message) => return refuse(2, message),
    };
    let clock = clock();
    // The worktrees could not be listed: nothing was wrong with the
    // conditions.
    let inventory = match super::watch::inventory(ctx).await {
        Ok(inventory) => inventory,
        Err(message) => return refuse(1, message),
    };
    let mut read =
        match super::watch::read_conditions(ctx, &inventory, &args.conditions, clock).await {
            Ok(read) => read,
            Err(message) => return refuse(2, message),
        };
    for (arg, cond) in args.conditions.iter().zip(&mut read.when) {
        exact_duration(arg, cond, clock);
    }
    if let Some(message) = args
        .conditions
        .iter()
        .zip(&read.when)
        .find_map(|(arg, cond)| outlasts(arg, cond, clock, timeout))
    {
        return refuse(2, message);
    }
    let about_owner = read
        .when
        .iter()
        .any(|cond| matches!(cond.cond, Condition::Own(_) | Condition::Check { .. }));
    let slug = match owner_in(&inventory, args.slug.as_deref(), "a wait on this worktree") {
        Ok(slug) => slug,
        // A time, a pull request or another worktree needs no owner.
        Err(_) if !about_owner && args.slug.is_none() => String::new(),
        Err(message) => return refuse(1, message),
    };
    let state = ctx
        .database
        .call(|store| Ok(store.read_wt_state()?))
        .await?;
    let watch = Watch {
        id: "wait".into(),
        incarnation: state["slugs"][slug.as_str()]["createdAt"]
            .as_str()
            .map(str::to_owned),
        slug,
        by: author(),
        created_at_ms: clock.now_ms,
        utc_offset_minutes: clock.utc_offset_minutes,
        when: read.when.clone(),
        then: WatchActions::default(),
        met_at_ms: None,
        last_error: None,
        owner_gone: false,
        extra: serde_json::Map::new(),
    };
    if !args.json {
        read.print();
    }
    let timeout = Duration::from_millis(u64::try_from(timeout).unwrap_or(0));
    match wait_until(ctx, watch, args.any, timeout).await {
        Ok(outcome) => Ok((outcome.exit_code(), Some(outcome))),
        Err(message) => refuse(1, message),
    }
}

pub async fn run(ctx: &AppContext, args: &WaitArgs) -> Result<i32> {
    let (code, outcome) = wait(ctx, args).await?;
    if let Some(outcome) = outcome {
        let clock = clock();
        if args.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&outcome.json(args, clock))?
            );
        } else {
            println!("{}", outcome.text(args, clock));
        }
    }
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wt_app_core::test_support::CommandFixture;
    use wt_core::watch::parse;

    fn args(conditions: &[&str], any: bool, timeout: &str) -> WaitArgs {
        WaitArgs {
            conditions: conditions.iter().map(|arg| (*arg).to_owned()).collect(),
            any,
            timeout: timeout.into(),
            slug: None,
            json: false,
        }
    }

    fn outcome(ended: Ended, conditions: &[&str], truths: Vec<Truth>) -> Outcome {
        Outcome {
            ended,
            truths,
            when: conditions
                .iter()
                .map(|arg| parse(arg, clock()).unwrap())
                .collect(),
            problem: None,
            why: Vec::new(),
            never: Vec::new(),
            waited: Duration::from_secs(750),
        }
    }

    #[test]
    fn all_must_hold_unless_any_is_asked_for() {
        let one = [Truth::Holds, Truth::NotYet, Truth::Unknown("why".into())];
        assert!(holds(&one, true));
        assert!(!holds(&one, false));
        assert!(holds(&[Truth::Holds, Truth::Holds], false));
        assert!(!holds(&[Truth::Unknown("why".into())], true));
        assert!(!holds(&[], false) && !holds(&[], true));
    }

    #[test]
    fn the_exit_code_tells_held_from_timed_out_from_could_not_tell() {
        let conditions = ["settled", "reviewed"];
        let unknown = || Truth::Unknown("no current data for this worktree".into());
        let code = |ended, truths| outcome(ended, &conditions, truths).exit_code();
        assert_eq!(code(Ended::Held, vec![Truth::Holds, Truth::NotYet]), 0);
        assert_eq!(
            code(Ended::TimedOut, vec![Truth::Holds, Truth::NotYet]),
            124
        );
        assert_eq!(code(Ended::TimedOut, vec![Truth::NotYet, unknown()]), 124);
        // Nothing that still fails was ever told: waiting again may not help.
        assert_eq!(code(Ended::TimedOut, vec![Truth::Holds, unknown()]), 1);
        assert_eq!(code(Ended::TimedOut, vec![unknown(), unknown()]), 1);
        assert_eq!(
            code(Ended::Interrupted, vec![Truth::NotYet, Truth::NotYet]),
            130
        );
    }

    /// A condition whose reading is its own word is printed once.
    #[test]
    fn a_condition_that_reads_as_its_argument_is_named_once() {
        let conditions = ["merged", "settled"];
        let mut ended = outcome(
            Ended::TimedOut,
            &conditions,
            vec![Truth::Unknown("no current data".into()), Truth::NotYet],
        );
        ended.problem = Some("GitHub is switched off (WT_GITHUB=off)".into());
        let text = ended.text(&args(&conditions, false, "30m"), clock());
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines[1..3],
            [
                "  merged: unknown (GitHub is switched off (WT_GITHUB=off))",
                "  settled: PR checks settled: not yet",
            ]
        );
    }

    #[test]
    fn the_report_names_what_held_and_why_the_rest_did_not() {
        let conditions = ["settled", "reviewed", "evicted"];
        let mut ended = outcome(
            Ended::Held,
            &conditions,
            vec![
                Truth::NotYet,
                Truth::Holds,
                Truth::Unknown("not known whether this worktree is evicted".into()),
            ],
        );
        ended.problem = Some("GitHub could not be read: timeout".into());
        let args = args(&conditions, true, "30m");
        assert_eq!(
            ended.text(&args, clock()),
            "held after 12m30s: reviewed\n  settled: PR checks settled: not yet\n  reviewed: bot review of the current head done: holds\n  evicted: merge dropped: unknown (GitHub could not be read: timeout)"
        );
        let json = ended.json(&args, clock());
        assert_eq!(json["result"], "held");
        assert_eq!(json["held"], json!(["reviewed"]));
        // A merge GitHub dropped holds with GitHub's reason beside it.
        let mut dropped = outcome(Ended::Held, &["evicted"], vec![Truth::Holds]);
        dropped.why = vec![Some("removed from the merge queue: merge_conflict".into())];
        let one = super::tests::args(&["evicted"], false, "30m");
        assert_eq!(
            dropped.text(&one, clock()),
            "held after 12m30s: evicted\n  evicted: merge dropped: holds (removed from the merge queue: merge_conflict)"
        );
        assert_eq!(
            dropped.json(&one, clock())["conditions"][0]["reason"],
            "removed from the merge queue: merge_conflict"
        );
        assert_eq!(json["conditions"][0]["state"], "not_yet");
        assert_eq!(
            json["conditions"][2]["reason"],
            "GitHub could not be read: timeout"
        );

        let timed_out = outcome(
            Ended::TimedOut,
            &["green", "merged"],
            vec![Truth::Holds, Truth::NotYet],
        );
        let text = timed_out.text(
            &super::tests::args(&["green", "merged"], false, "30m"),
            clock(),
        );
        assert!(
            text.starts_with("timed out after 12m30s: 1 of 2 still does not hold\n"),
            "{text}"
        );
        assert!(text.contains("wt wake <condition>"), "{text}");
        assert_eq!(elapsed(Duration::from_secs(45)), "45s");
        assert_eq!(elapsed(Duration::from_secs(1200)), "20m");
        assert_eq!(elapsed(Duration::from_secs(7500)), "2h05m");
    }

    #[test]
    fn a_duration_is_waited_to_the_millisecond_and_a_clock_time_as_read() {
        let now = LocalClock {
            now_ms: 1_795_000_000_250,
            utc_offset_minutes: 0,
        };
        for (arg, exact) in [("3s", 3_000), ("+90m", 5_400_000)] {
            let mut cond = parse(arg, now).unwrap();
            // As stored for a wake: the next whole second.
            assert_eq!(cond.known_at_ms(), Some(now.now_ms + exact + 750), "{arg}");
            exact_duration(arg, &mut cond, now);
            assert_eq!(cond.known_at_ms(), Some(now.now_ms + exact), "{arg}");
        }
        for arg in ["17:00", "2030-01-15T09:00", "merged", "merged+5m"] {
            let mut cond = parse(arg, now).unwrap();
            let before = cond.clone();
            exact_duration(arg, &mut cond, now);
            assert_eq!(cond, before, "{arg}");
        }
    }

    #[test]
    fn a_wait_stops_when_what_must_hold_cannot() {
        let cannot = Some("gone".to_owned());
        assert!(cannot_hold(&[None, cannot.clone()], false));
        assert!(!cannot_hold(&[None, cannot.clone()], true));
        assert!(cannot_hold(&[cannot.clone(), cannot], true));
        assert!(!cannot_hold(&[None], false) && !cannot_hold(&[], true));
    }

    #[test]
    fn a_timeout_is_a_duration_of_at_most_a_day() {
        assert_eq!(timeout_ms("30m"), Ok(1_800_000));
        assert_eq!(timeout_ms("24h"), Ok(TIMEOUT_MAX_MS));
        for refused in ["25h", "2d", "soon", "30", ""] {
            assert!(
                timeout_ms(refused).unwrap_err().contains("wt wake"),
                "{refused}"
            );
        }
    }

    #[test]
    fn a_condition_that_cannot_hold_in_time_points_at_wake() {
        let clock = clock();
        let late = |arg: &str, timeout: &str| {
            outlasts(
                arg,
                &parse(arg, clock).unwrap(),
                clock,
                timeout_ms(timeout).unwrap(),
            )
        };
        assert!(late("2h", "30m").unwrap().contains("wt wake 2h -m"));
        assert!(late("release:production+24h", "30m").is_some());
        assert_eq!(late("10m", "30m"), None);
        assert_eq!(late("green+5m", "30m"), None);
        assert_eq!(late("merged", "30m"), None);
    }

    #[tokio::test]
    async fn a_wait_blocks_until_its_conditions_hold_or_the_timeout_passes() {
        let fixture = CommandFixture::new().await.unwrap();
        let ctx = &fixture.ctx;
        let ended = |(code, outcome): (i32, Option<Outcome>)| {
            let outcome = outcome.unwrap();
            (code, outcome.ended, outcome.truths)
        };

        // Usage: no condition, an unknown word, a time past the timeout.
        for refused in [
            args(&[], false, "30m"),
            args(&["mergd"], false, "30m"),
            args(&["2h"], false, "30m"),
            args(&["merged"], false, "soon"),
        ] {
            let (code, outcome) = wait(ctx, &refused).await.unwrap();
            assert_eq!((code, outcome.is_none()), (2, true), "{refused:?}");
        }

        // A worktree that is not there is gone already; nothing else holds.
        let (code, how, truths) = ended(
            wait(ctx, &args(&["ready", "wt:absent:gone"], true, "30m"))
                .await
                .unwrap(),
        );
        assert_eq!((code, how), (0, Ended::Held));
        assert_eq!(truths, [Truth::NotYet, Truth::Holds]);

        // All must hold by default, so the same pair times out.
        let (code, how, truths) = ended(
            wait(ctx, &args(&["ready", "wt:absent:gone"], false, "1s"))
                .await
                .unwrap(),
        );
        assert_eq!((code, how), (124, Ended::TimedOut));
        assert_eq!(truths, [Truth::NotYet, Truth::Holds]);

        // A time is waited out in the process.
        let started = Instant::now();
        let (code, how, _) = ended(wait(ctx, &args(&["1s"], false, "30m")).await.unwrap());
        assert_eq!((code, how), (0, Ended::Held));
        assert!(started.elapsed() >= Duration::from_millis(900));

        // A worktree merged and removed before the wait began holds from
        // its removal record; one removed unmerged ends the wait at once,
        // since nothing is left to wait for.
        ctx.database
            .call(|store| {
                let removed = |slug: &str, landed: &str| {
                    serde_json::from_value::<wt_store::RemovedWorktree>(json!({
                        "slug": slug, "branch": format!("feature/{slug}"),
                        "removedAt": "2026-11-27T12:00:00Z", "landedOnAtRemoval": landed,
                    }))
                };
                store.record_removed_worktrees(
                    &[
                        removed("landed", "base")?,
                        removed("abandoned", "unlanded")?,
                    ],
                    1_795_000_000_000,
                )?;
                Ok(())
            })
            .await
            .unwrap();
        let (code, how, truths) = ended(
            wait(ctx, &args(&["wt:landed:merged"], false, "30m"))
                .await
                .unwrap(),
        );
        assert_eq!((code, how, truths), (0, Ended::Held, vec![Truth::Holds]));
        let (code, outcome) = wait(ctx, &args(&["wt:abandoned:merged"], false, "30m"))
            .await
            .unwrap();
        let outcome = outcome.unwrap();
        assert_eq!((code, outcome.ended), (1, Ended::Cannot));
        assert!(
            outcome
                .text(&args(&["wt:abandoned:merged"], false, "30m"), clock())
                .contains("abandoned was removed without merging"),
        );
        // With --any another condition may still hold: the wait goes on.
        let (code, how, _) = ended(
            wait(ctx, &args(&["wt:abandoned:merged", "1s"], true, "30m"))
                .await
                .unwrap(),
        );
        assert_eq!((code, how), (0, Ended::Held));

        // Becoming ready ends a wait that was already running.
        let waiting = tokio::spawn({
            let ctx = ctx.clone();
            async move { wait(&ctx, &args(&["wt:two:ready"], false, "30m")).await }
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        let status = super::super::status::StatusArgs {
            positionals: vec!["two".into(), "ready".into()],
            risk: Some("low".into()),
            ..Default::default()
        };
        assert_eq!(super::super::status::run(ctx, &status).await.unwrap(), 0);
        let (code, outcome) = waiting.await.unwrap().unwrap();
        let outcome = outcome.unwrap();
        assert_eq!((code, outcome.ended), (0, Ended::Held));
        // The write itself ended the wait. The timed read of the rows comes
        // a whole interval after the first one, so a wait that had to be
        // ended by it has always waited longer than that, however loaded
        // the machine is.
        assert!(outcome.waited < GIT_INTERVAL, "{:?}", outcome.waited);

        // Nothing was stored for any of it.
        let state = ctx
            .database
            .call(|store| Ok(store.read_wt_state()?))
            .await
            .unwrap();
        assert!(wt_store::stored_watches(&state).is_empty());

        // An interrupt ends the wait at once.
        let waiting = tokio::spawn({
            let ctx = ctx.clone();
            async move { wait(&ctx, &args(&["wt:two:verified"], false, "30m")).await }
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        ctx.cancellation.cancel();
        let (code, how, _) = ended(waiting.await.unwrap().unwrap());
        assert_eq!((code, how), (130, Ended::Interrupted));
        fixture.close().await.unwrap();
    }
}
