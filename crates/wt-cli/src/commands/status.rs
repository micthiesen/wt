use std::{
    collections::BTreeSet,
    ffi::OsString,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use clap::Args;
use serde_json::{Value, json};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use wt_config::InstanceRole;
use wt_core::{
    VerifyStage, WorkRisk, WorkState, WorkStatusRecord, parse_work_status, resolve_work_state,
    sanitize_work_note, work_age,
};
use wt_platform::process::CommandSpec;
use wt_store::WorkStatusRecord as StoredWorkStatusRecord;
use wt_vcs::WorktreeRecord;

use wt_app_core::{context::AppContext, database::Database};
use wt_ops::commands::resolve::{Inventory, may_be_inside_a_worktree};

use super::landing::{self, RowLanding};

const VOCAB: &str = "states: todo, working, review, needs-testing, needs-human, ready, verified, dropped\nUse unique prefixes; `nh` and `nt` are aliases. Ready requires --risk. --blocked-on gates ready/todo, and --until <condition> clears the gate by itself once the condition holds (e.g. --until pr:3806:production; see `wt watch check`); --verify-in-staging / --verify-in-production record a check owed once the change deploys there (both may be given; --no-verify-in-<stage> drops one). `wt owed` lists what the worktree owes and `wt done` closes it.";

#[derive(Debug, Clone, Args, Default)]
pub struct StatusArgs {
    /// Optional worktree slug or branch (default: current worktree) and/or a
    /// state; a unique prefix works, `nh` and `nt` are aliases. Bare shows the
    /// status or, outside a worktree, the vocabulary.
    #[arg(value_name = "TARGET_OR_STATE", num_args = 0..=2)]
    pub positionals: Vec<String>,
    /// Note for the assertion; replaces the existing note (the old one is echoed back).
    #[arg(short = 'm', long = "note", allow_hyphen_values = true)]
    pub note: Option<String>,
    /// Rewrite only the note of the current status, leaving state and timestamp.
    #[arg(long, allow_hyphen_values = true)]
    pub note_only: Option<String>,
    /// Confidence after testing: low, medium or high (required for `ready`).
    /// Without a state, re-judges the risk alone.
    #[arg(long, value_name = "RISK")]
    pub risk: Option<String>,
    /// External gate that must happen before merging (for `ready` or `todo`).
    #[arg(long, allow_hyphen_values = true)]
    pub blocked_on: Option<String>,
    /// Numbered steps for a check only staging can prove, once this lands on
    /// the base branch; the worktree returns as needs-testing then. Without a
    /// state, amends in place. A check owed in production is left alone.
    #[arg(long, allow_hyphen_values = true, value_name = "STEPS")]
    pub verify_in_staging: Option<String>,
    /// Numbered steps for a check only production can prove, once the landing
    /// reaches `[branch] production` (the base landing when there is none).
    /// Without a state, amends in place. A check owed in staging is left
    /// alone.
    #[arg(long, allow_hyphen_values = true, value_name = "STEPS")]
    pub verify_in_production: Option<String>,
    /// Stop owing the check in staging without running it.
    #[arg(long, conflicts_with = "verify_in_staging")]
    pub no_verify_in_staging: bool,
    /// Stop owing the check in production without running it.
    #[arg(long, conflicts_with = "verify_in_production")]
    pub no_verify_in_production: bool,
    /// Replaced: say where the check runs with --verify-in-staging or
    /// --verify-in-production.
    #[arg(long, allow_hyphen_values = true, hide = true)]
    pub verify_after_merge: Option<String>,
    /// Record that you examined this row and your verdict, without changing its status.
    #[arg(long, allow_hyphen_values = true)]
    pub examined: Option<String>,
    /// Clear the `--blocked-on` gate, keeping state, risk and note.
    #[arg(long)]
    pub unblock: bool,
    /// When the `--blocked-on` gate clears by itself: a condition as
    /// `wt wake` reads it (pr:3806:production, release:production+24h,
    /// 17:00, check:<id>). Repeat it; all must hold. Needs a gate, given
    /// here or already set.
    // The value is optional to the parser so that one a shell took as a
    // comment (`--until #3806`) is answered with why, not a usage error.
    #[arg(
        long,
        value_name = "CONDITION",
        num_args = 0..=1,
        default_missing_value = ""
    )]
    pub until: Vec<String>,
    /// With `--until`: also wake the session with this prompt once the
    /// gate has cleared.
    #[arg(long, allow_hyphen_values = true, value_name = "PROMPT")]
    pub then: Option<String>,
    /// Add to the existing note instead of replacing it (with `-m` or `--note-only`).
    #[arg(long)]
    pub append: bool,
    /// Drop the status record.
    #[arg(long)]
    pub clear: bool,
    /// Print the fleet overview of every worktree's status.
    #[arg(long)]
    pub all: bool,
    /// Print machine-readable JSON: an array with `--all`, one object of the
    /// same shape when showing a single worktree.
    #[arg(long)]
    pub json: bool,
}

/// An owed post-merge check and where it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Verify {
    stage: VerifyStage,
    /// The text on one line, which is what names the check.
    steps: String,
    /// The text as written, for its line breaks.
    lines: String,
}

impl Verify {
    fn edit(self) -> wt_ops::status_checks::CheckEdit {
        wt_ops::status_checks::CheckEdit::Owe {
            stage: self.stage,
            steps: self.steps,
            lines: Some(self.lines),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Action {
    All {
        json: bool,
    },
    Show {
        target: Option<String>,
        json: bool,
    },
    Clear(Option<String>),
    Examine {
        target: Option<String>,
        verdict: String,
    },
    Set {
        target: Option<String>,
        state: WorkState,
        note: Option<String>,
        risk: Option<WorkRisk>,
        blocked: Option<String>,
        verify: Vec<Verify>,
        unverify: Vec<VerifyStage>,
        append: bool,
    },
    Amend {
        target: Option<String>,
        note: Option<String>,
        risk: Option<WorkRisk>,
        blocked: Option<Option<String>>,
        verify: Vec<Verify>,
        unverify: Vec<VerifyStage>,
        append: bool,
    },
    Error(String),
}

/// Whether `token` is spelled exactly like one of this command's flags.
fn is_status_flag(token: &str) -> bool {
    let command = StatusArgs::augment_args(clap::Command::new("status"));
    matches!(token, "-h" | "--help")
        || command.get_arguments().any(|arg| {
            arg.get_long()
                .is_some_and(|long| token.strip_prefix("--") == Some(long))
                || arg.get_short().is_some_and(|short| {
                    token
                        .strip_prefix('-')
                        .is_some_and(|rest| rest.chars().eq([short]))
                })
        })
}

/// These flags accept values that start with a hyphen, so a flag written
/// without its value takes the next flag as the value and pushes that flag's
/// own value into the positionals, where it fails as an unknown state.
fn swallowed_flag(args: &StatusArgs) -> Option<String> {
    let valued = [
        (
            "--verify-in-staging",
            &args.verify_in_staging,
            "steps",
            "--verify-in-staging \"1. ...\" -m \"<note>\"",
        ),
        (
            "--verify-in-production",
            &args.verify_in_production,
            "steps",
            "--verify-in-production \"1. ...\" -m \"<note>\"",
        ),
        (
            "--blocked-on",
            &args.blocked_on,
            "gate",
            "--blocked-on \"<gate>\" -m \"<note>\"",
        ),
        (
            "--examined",
            &args.examined,
            "verdict",
            "--examined \"<verdict>\"",
        ),
        (
            "--note-only",
            &args.note_only,
            "note",
            "--note-only \"<note>\"",
        ),
        (
            "--then",
            &args.then,
            "prompt",
            "--until <condition> --then \"<prompt>\"",
        ),
        ("-m", &args.note, "note", "-m \"<note>\""),
    ];
    valued.into_iter().find_map(|(flag, value, what, usage)| {
        let value = value.as_deref()?.trim();
        is_status_flag(value).then(|| {
            format!("{flag} needs its {what} as the next argument, not `{value}`: {usage}")
        })
    })
}

fn parse_status_args(args: &StatusArgs) -> Action {
    if let Some(message) = swallowed_flag(args) {
        return Action::Error(message);
    }
    if let Some(message) = until_misuse(args) {
        return Action::Error(message.into());
    }
    let action = parse_status_action(args);
    if args.json
        && !matches!(
            action,
            Action::All { .. } | Action::Show { .. } | Action::Error(_)
        )
    {
        return Action::Error(
            "--json only prints: use it with --all or when showing one worktree's status".into(),
        );
    }
    action
}

/// The hand-written phase lines a ready note used to carry. What a worktree
/// owes is tracked, so a note that restates it is refused once, with where
/// each kind goes.
const OPS_PHASE_LINES: [&str; 4] = [
    "OPS BEFORE MERGE:",
    "OPS AFTER MERGE:",
    "OPS BEFORE RELEASE:",
    "OPS AFTER RELEASE:",
];
const OPS_NONE: &str = "OPS: none";

fn ops_lines_refusal(supplied: &str) -> Option<String> {
    let line = OPS_PHASE_LINES
        .iter()
        .find(|line| supplied.contains(**line))?;
    Some(format!(
        "the note has \"{line}\": what this worktree still owes is tracked, not written in the note. Leave it out and record each step where wt can see it:\n  a gate before merging: --blocked-on \"<why>\" [--until <condition>]\n  a check once deployed: --verify-in-staging \"<steps>\" / --verify-in-production \"<steps>\"\n  any other step to come back for: wt wake <condition> -m \"<what to do>\"\n  a step before the next production release: wt wake staging --before-release -m \"<what to do>\"\n`wt owed` shows the list; a note may say \"{OPS_NONE}\" when it is empty."
    ))
}

/// A note with its "OPS: none" taken out: the claim stopped being true.
fn without_ops_none(note: &str) -> Option<String> {
    let cleaned = sanitize_work_note(&note.replace(OPS_NONE, " "));
    (!cleaned.is_empty()).then_some(cleaned)
}

/// What the note of a status about to be written may say about the list.
/// `supplied` is the text this command gave; `owes` whether the worktree
/// will owe anything once the status is written.
fn settle_ops_none(
    note: Option<String>,
    supplied: Option<&str>,
    owes: bool,
) -> Result<Option<String>, String> {
    if !owes {
        return Ok(note);
    }
    if supplied.is_some_and(|supplied| supplied.contains(OPS_NONE)) {
        return Err(format!(
            "the note says \"{OPS_NONE}\" but this worktree owes something (`wt owed` lists it): drop \"{OPS_NONE}\" from the note, or close what is owed first"
        ));
    }
    // Carried from an earlier status, when the list was empty.
    Ok(note.and_then(|note| {
        if note.contains(OPS_NONE) {
            without_ops_none(&note)
        } else {
            Some(note)
        }
    }))
}

/// `--until` belongs to a gate and `--then` to `--until`.
fn until_misuse(args: &StatusArgs) -> Option<&'static str> {
    if args.until.is_empty() {
        return args
            .then
            .is_some()
            .then_some("--then says what to do once --until holds; add --until <condition>");
    }
    if args.unblock {
        return Some("--until clears the gate later and --unblock clears it now; pick one");
    }
    if args.clear || args.all || args.examined.is_some() || args.note_only.is_some() {
        return Some(
            "--until says when a --blocked-on gate clears; it goes with a state or with --blocked-on",
        );
    }
    if args
        .then
        .as_deref()
        .is_some_and(|then| sanitize_work_note(then).is_empty())
    {
        return Some("--then requires a non-empty prompt");
    }
    None
}

fn parse_status_action(args: &StatusArgs) -> Action {
    let positions = &args.positionals;
    let fail = |s: &str| Action::Error(s.into());
    let sanitize = |value: &str| {
        let cleaned = sanitize_work_note(value);
        (!cleaned.is_empty()).then_some(cleaned)
    };
    let note = args.note.as_deref().and_then(sanitize);
    let gate = match args.blocked_on.as_deref() {
        Some(value) => match sanitize(value) {
            Some(value) => Some(value),
            None => return fail("--blocked-on requires a non-empty gate"),
        },
        None => None,
    };
    if args.verify_after_merge.is_some() {
        return fail(
            "--verify-after-merge is replaced: say where the check runs with --verify-in-staging <steps> (once it lands on the base branch) or --verify-in-production <steps> (once it reaches production)",
        );
    }
    let mut verify = Vec::new();
    for (stage, flag, value) in [
        (
            VerifyStage::Staging,
            "--verify-in-staging",
            args.verify_in_staging.as_deref(),
        ),
        (
            VerifyStage::Production,
            "--verify-in-production",
            args.verify_in_production.as_deref(),
        ),
    ] {
        let Some(value) = value else { continue };
        match sanitize(value) {
            Some(steps) => verify.push(Verify {
                stage,
                steps,
                lines: value.to_owned(),
            }),
            None => return Action::Error(format!("{flag} requires non-empty steps")),
        }
    }
    let unverify = [
        (VerifyStage::Staging, args.no_verify_in_staging),
        (VerifyStage::Production, args.no_verify_in_production),
    ]
    .into_iter()
    .filter_map(|(stage, drop)| drop.then_some(stage))
    .collect::<Vec<_>>();
    // Only what this command supplies is judged: a note carried or appended
    // to from an earlier status is not.
    for supplied in [args.note.as_deref(), args.note_only.as_deref()]
        .into_iter()
        .flatten()
    {
        if let Some(message) = ops_lines_refusal(supplied) {
            return Action::Error(message);
        }
    }
    let checks_touched = !verify.is_empty() || !unverify.is_empty();
    let verdict = match args.examined.as_deref() {
        Some(value) => match sanitize(value) {
            Some(value) => Some(value),
            None => return fail("--examined requires a non-empty verdict"),
        },
        None => None,
    };
    let risk = match args.risk.as_deref() {
        Some(raw) => match parse_risk(raw) {
            Some(risk) => Some(risk),
            None => return fail("--risk must be one of low|medium|high"),
        },
        None => None,
    };
    let append = args.append;
    if args.note.is_some() && note.is_none() {
        return fail("-m requires a non-empty note");
    }
    if args.note_only.is_some() && args.note.is_some() {
        return fail("--note-only conflicts with -m");
    }
    if args.note_only.is_some() {
        if args.clear
            || args.all
            || risk.is_some()
            || gate.is_some()
            || checks_touched
            || args.unblock
            || verdict.is_some()
            || positions.len() > 1
        {
            return fail("--note-only only amends a note on an existing status");
        }
        if positions
            .first()
            .is_some_and(|p| resolve_work_state(p).is_some())
        {
            return fail("--note-only keeps the current state; drop the state argument");
        }
        let Some(note) = sanitize(args.note_only.as_deref().unwrap_or_default()) else {
            return fail("--note-only requires a non-empty note");
        };
        return Action::Amend {
            target: positions.first().cloned(),
            note: Some(note),
            risk: None,
            blocked: None,
            verify: Vec::new(),
            unverify: Vec::new(),
            append,
        };
    }
    if args.unblock && gate.is_some() {
        return fail("--blocked-on sets a gate and --unblock removes one; pick one");
    }
    if let Some(verdict) = verdict {
        if args.clear
            || args.all
            || note.is_some()
            || risk.is_some()
            || gate.is_some()
            || checks_touched
            || args.unblock
            || append
            || positions.len() > 1
        {
            return fail("--examined records a verdict on its own");
        }
        return Action::Examine {
            target: positions.first().cloned(),
            verdict,
        };
    }
    if append && note.is_none() {
        return fail("--append needs -m <text>");
    }
    if args.all {
        if args.clear
            || note.is_some()
            || risk.is_some()
            || gate.is_some()
            || checks_touched
            || args.unblock
            || !positions.is_empty()
            || args.note_only.is_some()
        {
            return fail("--all only combines with --json");
        }
        return Action::All { json: args.json };
    }
    if args.clear {
        if note.is_some()
            || risk.is_some()
            || gate.is_some()
            || checks_touched
            || args.unblock
            || append
            || positions.len() > 1
        {
            return fail("--clear takes only an optional target");
        }
        return Action::Clear(positions.first().cloned());
    }
    if positions.len() > 2 {
        return fail("too many arguments");
    }
    let (target, state) = match positions.as_slice() {
        [] => (None, None),
        [one] if resolve_work_state(one).is_some() => (None, Some(one.as_str())),
        [one] if is_ambiguous_state(one) => {
            return Action::Error(format!("ambiguous status prefix: `{one}`"));
        }
        [one] => (Some(one.clone()), None),
        [target, state] => (Some(target.clone()), Some(state.as_str())),
        _ => return fail("too many arguments"),
    };
    let Some(raw_state) = state else {
        if gate.is_some()
            || args.unblock
            || checks_touched
            || risk.is_some()
            || !args.until.is_empty()
        {
            return Action::Amend {
                target,
                note,
                risk,
                blocked: if args.unblock {
                    Some(None)
                } else {
                    gate.map(Some)
                },
                verify,
                unverify,
                append,
            };
        }
        if note.is_some() {
            return fail("-m needs a state; to amend a note use --note-only");
        }
        return Action::Show {
            target,
            json: args.json,
        };
    };
    let Some(state) = resolve_work_state(raw_state) else {
        return Action::Error(format!("unknown or ambiguous status state: `{raw_state}`"));
    };
    if risk.is_some() && state != WorkState::Ready {
        return fail("--risk only applies to ready");
    }
    if args.unblock {
        return fail(
            "--unblock clears the gate on its own, and a status change keeps it: run wt status --unblock, then assert the state",
        );
    }
    if gate.is_some() && !matches!(state, WorkState::Ready | WorkState::Todo) {
        return fail("--blocked-on applies only to ready and todo");
    }
    if state == WorkState::Ready && risk.is_none() {
        return fail("ready requires --risk low|medium|high");
    }
    if state == WorkState::Ready && risk != Some(WorkRisk::Low) && note.is_none() {
        return fail("ready --risk medium|high requires -m to explain uncertainty");
    }
    if state == WorkState::NeedsHuman && note.is_none() {
        return fail("needs-human requires -m explaining what you need");
    }
    if state == WorkState::Dropped && note.is_none() {
        return fail("dropped requires -m explaining why");
    }
    if state == WorkState::Verified && note.is_none() {
        return fail("verified requires -m explaining what you checked and where");
    }
    if !args.until.is_empty() && matches!(state, WorkState::Verified | WorkState::Dropped) {
        return fail(
            "--until says when a gate clears, and verified and dropped hold no gate: drop --until",
        );
    }
    if checks_touched && matches!(state, WorkState::Verified | WorkState::Dropped) {
        return fail(
            "--verify-in-* doesn't apply to verified or dropped: verified closes the checks that are due and dropped drops them all",
        );
    }
    Action::Set {
        target,
        state,
        note,
        risk,
        blocked: gate,
        verify,
        unverify,
        append,
    }
}

pub async fn run(ctx: &AppContext, args: &StatusArgs) -> Result<i32> {
    let action = parse_status_args(args);
    if let Action::Error(message) = action {
        eprintln!("{message}\n{VOCAB}");
        return Ok(2);
    }
    // Outside every worktree a bare `wt status` can only print what the
    // words mean: that needs neither the worktrees nor the state.
    if matches!(
        &action,
        Action::Show {
            target: None,
            json: false
        }
    ) && args.until.is_empty()
        && !may_be_inside_a_worktree(ctx)
    {
        println!("{VOCAB}");
        return Ok(0);
    }
    let inventory = Inventory::read(ctx).await?;
    // Read before anything is written: a condition that cannot be read
    // must not leave a gate behind that nothing clears.
    let clock = super::watch::clock();
    let until = if args.until.is_empty() {
        None
    } else {
        match super::watch::read_conditions(ctx, &inventory, &args.until, clock).await {
            Ok(read) => Some(read),
            Err(message) => {
                eprintln!("{message}");
                return Ok(2);
            }
        }
    };
    let then = args.then.as_deref().and_then(super::watch::clean_prompt);
    match action {
        Action::All { json: wants_json } => {
            let worktrees = inventory.worktrees().collect::<Vec<_>>();
            let state = read_state(&ctx.database).await?;
            let now = now_ms();
            if wants_json {
                let rows = worktrees.iter().map(|w| (*w).clone()).collect::<Vec<_>>();
                let mut state = state.clone();
                wt_ops::other_branches::refresh(
                    ctx,
                    &rows,
                    &mut state,
                    None,
                    wt_ops::other_branches::Ask::Nothing,
                )
                .await;
                let landings = landing::read(ctx, &rows, &state, landing::Want::Phases).await;
                let mut entries = Vec::with_capacity(worktrees.len());
                for w in &worktrees {
                    let landing = landings
                        .get(w.target.slug())
                        .copied()
                        .unwrap_or(RowLanding::known(wt_core::Landing::NONE));
                    entries.push(live_json(ctx, &state, w, landing).await?);
                }
                let live = worktrees
                    .iter()
                    .map(|w| w.target.slug().to_owned())
                    .collect::<BTreeSet<_>>();
                let removed = ctx
                    .database
                    .call(move |store| Ok(store.recently_removed_worktrees(&live, now)?))
                    .await?;
                let mut output = entries;
                let separate = ctx.config.branch.separate_production();
                output.extend(
                    removed
                        .into_iter()
                        .map(|entry| removed_json(entry, separate)),
                );
                println!("{}", serde_json::to_string_pretty(&output)?);
            } else {
                for w in worktrees {
                    let work = state
                        .get("slugs")
                        .and_then(|s| s.get(w.target.slug()))
                        .and_then(|v| v.get("work"))
                        .and_then(parse_work_status);
                    println!(
                        "{}",
                        describe(w.target.slug(), work.as_ref(), w.head_sha.as_deref(), now)
                    );
                }
            }
            Ok(0)
        }
        Action::Show {
            target,
            json: wants_json,
        } => {
            let outside = |error: &anyhow::Error| {
                target.is_none() && error.to_string().starts_with("not inside")
            };
            let record = match inventory.resolve(target.as_deref()) {
                Ok(record) if !record.is_main => record,
                // JSON has no vocabulary to fall back to: a reader would
                // parse the text as a status.
                Ok(_) if wants_json => {
                    eprintln!("--json shows one worktree: pass a slug or run inside a worktree");
                    return Ok(1);
                }
                Err(error) if wants_json && outside(&error) => {
                    eprintln!("--json shows one worktree: pass a slug or run inside a worktree");
                    return Ok(1);
                }
                Ok(_) => {
                    println!("{VOCAB}");
                    return Ok(0);
                }
                Err(error) if outside(&error) => {
                    println!("{VOCAB}");
                    return Ok(0);
                }
                Err(error) if wants_json => {
                    eprintln!("{error}");
                    return Ok(1);
                }
                Err(error) => {
                    eprintln!("{error}\n{VOCAB}");
                    return Ok(1);
                }
            };
            let state = read_state(&ctx.database).await?;
            if wants_json {
                let mut state = state.clone();
                wt_ops::other_branches::refresh(
                    ctx,
                    std::slice::from_ref(&record),
                    &mut state,
                    None,
                    wt_ops::other_branches::Ask::Nothing,
                )
                .await;
                let landing = landing::read_one(ctx, &record, &state, landing::Want::Phases).await;
                println!(
                    "{}",
                    serde_json::to_string_pretty(&live_json(ctx, &state, &record, landing).await?)?
                );
                return Ok(0);
            }
            let work = state
                .get("slugs")
                .and_then(|s| s.get(record.target.slug()))
                .and_then(|v| v.get("work"))
                .and_then(parse_work_status);
            println!(
                "{}",
                describe(
                    record.target.slug(),
                    work.as_ref(),
                    record.head_sha.as_deref(),
                    now_ms()
                )
            );
            if !hints_off() {
                println!("\n{VOCAB}");
            }
            Ok(0)
        }
        Action::Clear(target) => {
            let record = match inventory.resolve(target.as_deref()) {
                Ok(w) if !w.is_main => w,
                Ok(_) => {
                    eprintln!("not inside a worktree");
                    return Ok(1);
                }
                Err(e) => {
                    eprintln!("{e}");
                    return Ok(1);
                }
            };
            let slug = record.target.slug().to_owned();
            let db_slug = slug.clone();
            let by = std::env::var("WT_AGENT")
                .ok()
                .map(|by| by.trim().to_owned())
                .filter(|by| !by.is_empty());
            let cancelled = ctx
                .database
                .call(move |s| {
                    // Clearing the status clears its gate, and with it the
                    // watch that was to clear it.
                    let mut gate_watches = s.list_watches(Some(&db_slug))?;
                    gate_watches.retain(|watch| watch.then.unblock.is_some());
                    s.clear_slug_work_status(&db_slug, by.as_deref())?;
                    let left = s.list_watches(Some(&db_slug))?;
                    gate_watches.retain(|watch| !left.iter().any(|kept| kept.id == watch.id));
                    Ok(gate_watches)
                })
                .await?;
            println!("✓ {slug} status cleared");
            report_cancelled_watches(&cancelled);
            Ok(0)
        }
        Action::Examine { target, verdict } => {
            let record = match inventory.resolve(target.as_deref()) {
                Ok(w) if !w.is_main => w,
                Ok(_) => {
                    eprintln!("not inside a worktree");
                    return Ok(1);
                }
                Err(e) => {
                    eprintln!("{e}");
                    return Ok(1);
                }
            };
            let Some(sha) = record.head_sha.clone() else {
                eprintln!("could not resolve HEAD for {}", record.target.slug());
                return Ok(2);
            };
            let slug = record.target.slug().to_owned();
            let state = read_state(&ctx.database).await?;
            let branch = state
                .get("slugs")
                .and_then(|s| s.get(&slug))
                .and_then(|v| v.get("baseBranch"))
                .and_then(Value::as_str)
                .map(str::to_owned);
            let base_sha =
                base_tip_sha(ctx, branch.as_deref().unwrap_or(&ctx.config.branch.base)).await?;
            let at = now_iso();
            let mut examined = json!({"sha":sha,"verdict":verdict,"at":at});
            if let Some(base_sha) = base_sha {
                examined["baseSha"] = json!(base_sha);
            }
            if let Ok(by) = std::env::var("WT_AGENT")
                && !by.trim().is_empty()
            {
                examined["by"] = json!(by.trim());
            }
            let out = examined.clone();
            let db_slug = slug.clone();
            ctx.database
                .call(move |s| Ok(s.set_slug_examined(&db_slug, Some(out))?))
                .await?;
            println!(
                "✓ {slug} examined at {}\n  verdict: {}",
                &sha[..sha.len().min(7)],
                examined["verdict"].as_str().unwrap_or_default()
            );
            Ok(0)
        }
        Action::Set {
            target,
            state: work_state,
            note,
            risk,
            blocked,
            verify,
            unverify,
            append,
        } => {
            let record = match inventory.resolve(target.as_deref()) {
                Ok(w) if !w.is_main => w,
                Ok(_) => {
                    eprintln!("not inside a worktree");
                    return Ok(1);
                }
                // A removed row can still owe its post-merge check; only its
                // outcome may be recorded, never a new claim about live work.
                Err(e) => {
                    if let Some(slug) = target.as_deref()
                        && matches!(work_state, WorkState::Verified | WorkState::Dropped)
                    {
                        return settle_removed(ctx, slug, work_state, note, e).await;
                    }
                    eprintln!("{e}");
                    return Ok(1);
                }
            };
            let slug = record.target.slug().to_owned();
            let state = read_state(&ctx.database).await?;
            let steps_owed = steps_owed(&state, &slug);
            if work_state == WorkState::Verified
                && let Some(code) = verify_checks(
                    ctx,
                    &inventory,
                    &record,
                    &state,
                    note.clone(),
                    append,
                    steps_owed,
                )
                .await?
            {
                return Ok(code);
            }
            let mut next = WorkStatusRecord::new(work_state, now_iso());
            next.sha = record.head_sha.clone();
            next.by = std::env::var("WT_AGENT")
                .ok()
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty());
            next.risk = risk;
            let gate_asked = blocked.is_some();
            let list_asked = !verify.is_empty() || !unverify.is_empty() || gate_asked;
            let needs_gate = until.is_some();
            let gate_owner = slug.clone();
            // A dropped check goes in the same write as the rest, so a
            // command that is refused drops nothing.
            let drops = unverify.clone();
            let write = update_status(&ctx.database, &slug, move |previous| {
                let merged = merge_note(
                    previous.as_ref().and_then(|p| p.note.as_deref()),
                    note.as_deref(),
                    append,
                    false,
                );
                wt_ops::status_checks::assert_gate(&mut next, previous.as_ref(), blocked);
                if needs_gate && next.blocked_on.is_none() {
                    return Err(format!(
                        "{gate_owner} has no gate for --until to clear: add --blocked-on \"<why>\""
                    ));
                }
                wt_ops::status_checks::assert_checks(
                    &mut next,
                    previous.as_ref(),
                    verify
                        .into_iter()
                        .map(Verify::edit)
                        .chain(drops.into_iter().map(drop_edit)),
                );
                let owes = steps_owed || record_owes(&next);
                next.note = settle_ops_none(merged, note.as_deref(), owes)?;
                Ok(next)
            })
            .await?;
            let StatusWrite {
                previous,
                next,
                wrote,
                cancelled,
                stamped,
                displaced,
            } = match write {
                Ok(write) => write,
                Err(refusal) => {
                    eprintln!("{refusal}");
                    return Ok(2);
                }
            };
            report_dropped_checks(&slug, previous.as_ref(), &unverify);
            let (previous_note, previous_gate) = previous
                .map(|previous| (previous.note, previous.blocked_on))
                .unwrap_or_default();
            println!(
                "✓ {} {}{}",
                record.target.slug(),
                work_state_name(work_state),
                if wrote { "" } else { " (already asserted)" }
            );
            if let Some(stamp) = &stamped {
                println!("  {} stamped {}", wt_core::STAMP_CURRENT, stamp.skill);
            }
            if let Some(line) = gate_line(
                previous_gate.as_deref(),
                next.blocked_on.as_deref(),
                gate_asked,
            ) {
                println!("{line}");
            }
            report_cancelled_watches(&cancelled);
            report_displaced_checks(&displaced);
            report_replaced_note(previous_note.as_deref(), next.note.as_deref(), append);
            if let Some(note) = &next.note {
                println!("  note: {note}");
            }
            note_budget_hint(work_state, next.note.as_deref());
            let clears = match gate_watch(ctx, &slug, &next, until, then, clock).await? {
                Ok(clears) => clears,
                Err(code) => return Ok(code),
            };
            print_guidance(work_state, next.blocked_on.as_deref(), clears.as_deref());
            // A plain assertion does not change the list; `ready` repeats
            // it, since that is where what is owed gets handed over.
            let ended = matches!(work_state, WorkState::Verified | WorkState::Dropped);
            let changed = list_asked || clears.is_some() || previous_gate != next.blocked_on;
            if !reads_list(work_state, changed) {
                return Ok(0);
            }
            let owed = super::owed::read(ctx, &record).await?;
            if changed || !owed.items.is_empty() {
                print_owed(&inventory, &record, &owed);
                if ended && !owed.items.is_empty() {
                    println!("{STILL_HOLDS}");
                }
            } else if work_state == WorkState::Ready
                && owed.items.is_empty()
                && !hints_off()
                && !next
                    .note
                    .as_deref()
                    .is_some_and(|note| note.contains(OPS_NONE))
            {
                println!(
                    "nothing is recorded as owed after this merges: say \"{OPS_NONE}\" in the note, or record what is (--verify-in-<stage>, wt wake <condition> -m, --blocked-on)."
                );
            }
            Ok(0)
        }
        Action::Amend {
            target,
            note,
            risk,
            blocked,
            verify,
            unverify,
            append,
        } => {
            let record = match inventory.resolve(target.as_deref()) {
                Ok(w) if !w.is_main => w,
                Ok(_) => {
                    eprintln!("not inside a worktree");
                    return Ok(1);
                }
                Err(e) => {
                    eprintln!("{e}");
                    return Ok(1);
                }
            };
            let slug = record.target.slug().to_owned();
            let state = read_state(&ctx.database).await?;
            let steps_owed = steps_owed(&state, &slug);
            // Dropping a check is the whole amendment when nothing else was
            // asked for.
            if !unverify.is_empty()
                && note.is_none()
                && risk.is_none()
                && blocked.is_none()
                && verify.is_empty()
                && until.is_none()
            {
                let dropped = drop_checks(ctx, &slug, &unverify).await?;
                print_owed(&inventory, &record, &super::owed::read(ctx, &record).await?);
                return Ok(i32::from(!dropped));
            }
            // Otherwise it goes in the same write as the rest, so an
            // amendment that is refused drops nothing.
            let drops = unverify.clone();
            let needs_gate = until.is_some();
            let write = update_status(&ctx.database, &slug, {
                let slug = slug.clone();
                move |previous| {
                    let supplied = note.clone();
                    let mut next =
                        amend_record(&slug, previous, note, risk, blocked, verify, append)?;
                    for stage in drops {
                        next.drop_check(stage);
                    }
                    if needs_gate && next.blocked_on.is_none() {
                        return Err(format!(
                            "{slug} has no gate for --until to clear: add --blocked-on \"<why>\""
                        ));
                    }
                    let owes = steps_owed || record_owes(&next);
                    next.note = settle_ops_none(next.note.take(), supplied.as_deref(), owes)?;
                    Ok(next)
                }
            })
            .await?;
            let StatusWrite {
                previous,
                next,
                wrote,
                cancelled,
                stamped: _,
                displaced,
            } = match write {
                Ok(write) => write,
                Err(refusal) => {
                    eprintln!("{refusal}");
                    return Ok(2);
                }
            };
            let previous = previous.expect("an amend is refused without a previous status");
            report_dropped_checks(&slug, Some(&previous), &unverify);
            let dropped = unverify
                .iter()
                .any(|stage| previous.open_check(*stage).is_some());
            let old_checks = previous.open_checks();
            let previous_note = previous.note;
            let old_gate = previous.blocked_on;
            println!(
                "✓ {slug} {}{}",
                work_state_name(next.state),
                if wrote {
                    " amended (state + timestamp kept)"
                } else {
                    " unchanged"
                }
            );
            if let Some(gate) = &next.blocked_on {
                println!("  blocked on: {gate}");
            }
            report_cancelled_watches(&cancelled);
            for (stage, check) in next.open_checks() {
                println!("  verify in {}: {}", stage.as_str(), check.steps);
            }
            report_displaced_checks(&displaced);
            if let Some(note) = &next.note {
                println!("  note: {note}");
            }
            report_replaced_note(previous_note.as_deref(), next.note.as_deref(), append);
            note_budget_hint(next.state, next.note.as_deref());
            let clears = match gate_watch(ctx, &slug, &next, until, then, clock).await? {
                Ok(clears) => clears,
                Err(code) => return Ok(code),
            };
            let list_changed = dropped
                || clears.is_some()
                || old_gate != next.blocked_on
                || old_checks != next.open_checks();
            if clears.is_some() || old_gate != next.blocked_on {
                print_guidance(next.state, next.blocked_on.as_deref(), clears.as_deref());
            }
            if list_changed {
                print_owed(&inventory, &record, &super::owed::read(ctx, &record).await?);
            }
            Ok(0)
        }
        Action::Error(_) => unreachable!(),
    }
}

/// The worktree came to owe something outside its status (a wake was set):
/// a note that still says "OPS: none" stops saying so.
pub(crate) async fn drop_ops_none(ctx: &AppContext, slug: &str) -> Result<()> {
    let wanted = slug.to_owned();
    let stored = ctx
        .database
        .call(move |store| Ok(store.read_slug_state(&wanted)?))
        .await?;
    let claims = stored
        .as_ref()
        .and_then(|stored| stored["work"]["note"].as_str())
        .is_some_and(|note| note.contains(OPS_NONE));
    if !claims {
        return Ok(());
    }
    let write = update_status(&ctx.database, slug, |previous| {
        let mut next = previous.ok_or_else(String::new)?;
        next.note = next.note.as_deref().and_then(without_ops_none);
        Ok(next)
    })
    .await?;
    if write.is_ok_and(|write| write.wrote) {
        println!(
            "  the status note no longer says \"{OPS_NONE}\": this worktree owes something now"
        );
    }
    Ok(())
}

/// Whether the status record itself holds something owed: a gate or a
/// check.
fn record_owes(record: &WorkStatusRecord) -> bool {
    record.blocked_on.is_some() || !record.open_checks().is_empty()
}

/// Whether the worktree owes steps outside its status: a wake still to
/// come, or one that came due and was not closed.
fn steps_owed(state: &Value, slug: &str) -> bool {
    let created_at = state["slugs"][slug]["createdAt"].as_str();
    wt_core::owes_anything(
        None,
        &wt_core::owed::row_watches(&wt_store::stored_watches(state), slug, created_at),
        &wt_core::owed::row_due(&wt_store::stored_owed(state), slug, created_at),
    )
}

/// Whether asserting `state` ends with the owed list: when the command
/// `changed` it, and at `ready` or an ending, where what is owed is handed
/// over. Reading the list reads the row's landing (Git, and GitHub for a row
/// that owes a check), which the assertions agents make all day skip.
fn reads_list(state: WorkState, changed: bool) -> bool {
    changed
        || matches!(
            state,
            WorkState::Ready | WorkState::Verified | WorkState::Dropped
        )
}

/// The list, as every command that changed it ends.
fn print_owed(inventory: &Inventory, record: &WorktreeRecord, owed: &super::owed::RowOwed) {
    println!(
        "{}",
        super::owed::owed_text(
            &super::owed::who(inventory, record),
            record.target.slug(),
            owed,
            super::watch::clock()
        )
    );
}

/// What a worktree is told when it ends (verified or dropped) while it
/// still owes something: the status does not let go of it.
const STILL_HOLDS: &str = "  these still hold cleanup: cancel a wake you no longer need (wt watch cancel --mine), close a due step (wt done <id> -m \"<evidence>\")";

fn drop_edit(stage: VerifyStage) -> wt_ops::status_checks::CheckEdit {
    wt_ops::status_checks::CheckEdit::Drop { stage }
}

/// Say what became of each check a status write was asked to drop.
/// `previous` is the status the write replaced.
fn report_dropped_checks(slug: &str, previous: Option<&WorkStatusRecord>, stages: &[VerifyStage]) {
    for stage in stages {
        match previous.and_then(|previous| previous.open_check(*stage)) {
            Some(check) => println!(
                "✓ {slug} no longer owes the check in {}: {}",
                stage.as_str(),
                wt_app_core::watch_view::headline(&check.steps)
            ),
            None => println!("  {slug} owed no check in {}", stage.as_str()),
        }
    }
}

/// Stop owing the checks in `stages` without running them. Returns whether
/// any was owed.
async fn drop_checks(ctx: &AppContext, slug: &str, stages: &[VerifyStage]) -> Result<bool> {
    let mut any = false;
    for stage in stages.iter().copied() {
        let (owner, by) = (slug.to_owned(), super::watch::agent());
        let dropped = ctx
            .database
            .call(move |store| Ok(store.drop_check(&owner, stage, by.as_deref())?))
            .await?;
        match dropped {
            Some(check) => {
                any = true;
                println!(
                    "✓ {slug} no longer owes the check in {}: {}",
                    stage.as_str(),
                    wt_app_core::watch_view::headline(&check.steps)
                );
            }
            None => println!("  {slug} owed no check in {}", stage.as_str()),
        }
    }
    Ok(any)
}

/// `wt status verified` on a row that owes checks: close every check that
/// is due, or the earliest one when none is (its agent may have confirmed
/// the deploy itself). The status becomes verified only once no check
/// remains. `None` when the row owes no check: the assertion is then an
/// ordinary one.
async fn verify_checks(
    ctx: &AppContext,
    inventory: &Inventory,
    record: &WorktreeRecord,
    state: &Value,
    note: Option<String>,
    append: bool,
    steps_owed: bool,
) -> Result<Option<i32>> {
    let slug = record.target.slug().to_owned();
    let Some(previous) = parse_work_status(&state["slugs"][slug.as_str()]["work"])
        .filter(|previous| !previous.open_checks().is_empty())
    else {
        return Ok(None);
    };
    if steps_owed && note.as_deref().is_some_and(|note| note.contains(OPS_NONE)) {
        eprintln!(
            "the note says \"{OPS_NONE}\" but this worktree owes something (`wt owed` lists it): drop \"{OPS_NONE}\" from the note, or close what is owed first"
        );
        return Ok(Some(2));
    }
    // Under a landing that cannot be read nothing is known to be due, so
    // only the earliest check closes: never more than was asserted.
    let landing = landing::read_one_live(ctx, record, state, landing::Want::Lists).await;
    if let Some(refusal) = super::owed::not_merged_refusal(
        &slug,
        landing,
        &previous
            .open_checks()
            .iter()
            .map(|(stage, _)| *stage)
            .collect::<Vec<_>>(),
    ) {
        eprintln!("{refusal}");
        return Ok(Some(1));
    }
    let landing = landing.shown();
    let merged = merge_note(previous.note.as_deref(), note.as_deref(), append, false);
    let verified = super::owed::verified_record(record.head_sha.as_deref(), merged)?;
    let (owner, evidence) = (slug.clone(), note.clone());
    let closed = ctx
        .database
        .call(move |store| {
            Ok(wt_ops::status_checks::close_verified(
                store,
                &owner,
                landing,
                now_ms(),
                &verified,
                evidence.as_deref(),
                |_| true,
            )?)
        })
        .await?;
    let Some(wt_store::ChecksClosed::Closed {
        closed,
        remaining,
        record: written,
        cancelled_watches,
    }) = closed
    else {
        // Closed by someone else since the state was read.
        return Ok(None);
    };
    super::owed::print_checks_closed(&slug, &closed, &remaining, Some(&written.state), false);
    report_cancelled_watches(&cancelled_watches);
    if remaining.is_empty() {
        report_replaced_note(previous.note.as_deref(), written.note.as_deref(), append);
        if let Some(note) = &written.note {
            println!("  note: {note}");
        }
    }
    let owed = super::owed::read(ctx, record).await?;
    print_owed(inventory, record, &owed);
    if remaining.is_empty() && !owed.items.is_empty() {
        println!("{STILL_HOLDS}");
    }
    Ok(Some(0))
}

/// Set the watch that clears the gate just written. Returns what will clear
/// it, in words, or the exit code after saying why no watch was set.
async fn gate_watch(
    ctx: &AppContext,
    slug: &str,
    written: &WorkStatusRecord,
    until: Option<super::watch::ReadConditions>,
    then: Option<String>,
    clock: wt_core::watch::LocalClock,
) -> Result<Result<Option<String>, i32>> {
    let Some(until) = until else {
        return Ok(Ok(None));
    };
    let Some(gate) = written.blocked_on.clone() else {
        eprintln!("the status has no gate, so --until set nothing");
        return Ok(Err(1));
    };
    let actions = wt_core::watch::WatchActions {
        unblock: Some(gate),
        wake: then,
        ..Default::default()
    };
    let asked_to_wake = actions.wake.is_some();
    let when = until.when.clone();
    match super::watch::set(ctx, slug, when, actions, false, clock).await? {
        Ok(set) => {
            until.print();
            let watch = set.watch;
            if let Some(note) = gate_note(written.blocked_on.as_deref().unwrap_or_default(), &watch)
            {
                println!("  {note}");
            }
            if let Some(kept) = watch.then.wake.as_deref().filter(|_| !asked_to_wake) {
                println!(
                    "  keeps the wake set with this gate earlier: {} (to drop it: wt watch cancel {}, then set --until again)",
                    wt_app_core::watch_view::headline(kept),
                    watch.id
                );
            }
            Ok(Ok(Some(clears_text(&watch, set.already))))
        }
        // The status is written; only its self-clearing is missing.
        Err(message) => {
            eprintln!("the gate is set but will not clear by itself: {message}");
            Ok(Err(1))
        }
    }
}

/// The note for a gate that clears on less than its text says: on a time
/// alone, or on conditions while the text names something more (another
/// pull request, or a second thing joined with "and", "then", "+" or ";").
/// wt checks only the conditions.
fn gate_note(gate: &str, watch: &wt_core::watch::Watch) -> Option<String> {
    use wt_core::watch::Condition;
    let summary = watch.when_summary();
    let only_time = watch
        .when
        .iter()
        .all(|cond| matches!(cond.cond, Condition::At { .. }));
    let lower = gate.to_lowercase();
    let numbers = |text: &str| -> Vec<String> {
        text.split('#')
            .skip(1)
            .map(|rest| {
                rest.chars()
                    .take_while(char::is_ascii_digit)
                    .collect::<String>()
            })
            .filter(|number| !number.is_empty())
            .collect()
    };
    let watched = numbers(&summary);
    let names_more = numbers(gate).iter().any(|number| !watched.contains(number))
        || [" and ", " then ", " + ", ";", " & ", " plus "]
            .iter()
            .any(|joined| lower.contains(joined));
    (only_time || names_more)
        .then(|| format!("this gate clears {summary}; anything else it names is not checked"))
}

/// What clears a gate by itself, to follow "wt clears the gate": `at 17:00
/// (watch abc234)`, `when #3806 in production, and then wakes this session
/// (watch abc234)`.
fn clears_text(watch: &wt_core::watch::Watch, already: bool) -> String {
    format!(
        "{}{} (watch {}{})",
        watch.when_summary(),
        if watch.then.wake.is_some() {
            ", and then wakes this session"
        } else {
            ""
        },
        watch.id,
        if already { ", already set" } else { "" }
    )
}

struct StatusWrite {
    previous: Option<WorkStatusRecord>,
    next: WorkStatusRecord,
    wrote: bool,
    /// Watches that were to clear the gate this write removed or replaced.
    cancelled: Vec<wt_core::watch::Watch>,
    /// The review stamp this assertion earned.
    stamped: Option<wt_core::Stamp>,
    /// Checks of an earlier pull request the write kept as steps.
    displaced: Vec<wt_store::DisplacedCheck>,
}

/// A check set where an earlier pull request's check was still owed does
/// not replace it: say what became of the earlier one and how to end it.
fn report_displaced_checks(displaced: &[wt_store::DisplacedCheck]) {
    for kept in displaced {
        let (stage, id) = (kept.stage.as_str(), &kept.watch.id);
        println!(
            "  the {stage} check of {} is still owed, now as step {id} ({}): {}",
            kept.left.label(),
            kept.watch.when_summary(),
            wt_app_core::watch_view::headline(&kept.check.steps)
        );
        println!(
            "    once it is due: wt done {id} -m \"<evidence>\"; if the new check covers it: wt watch cancel {id}"
        );
    }
}

/// A status write that changes the gate cancels the watch that was to clear
/// it, with the wake that was to follow: say so, or the wake is lost quietly.
pub(crate) fn report_cancelled_watches(cancelled: &[wt_core::watch::Watch]) {
    for watch in cancelled {
        let wake = watch
            .then
            .wake
            .as_deref()
            .map(|prompt| {
                format!(
                    "; it would then have woken the session with: {}",
                    wt_app_core::watch_view::headline(prompt)
                )
            })
            .unwrap_or_default();
        println!(
            "  cancelled watch {}, which cleared the old gate {}{wake}",
            watch.id,
            watch.when_summary()
        );
    }
}

/// Write the status `build` computes from the record stored at the moment of
/// writing. Agents assert statuses constantly, so a record read before the
/// write's transaction may already be replaced; building from it would drop
/// the newer claim's owed check, note or risk. `Err` from `build` refuses the
/// write with a usage message.
async fn update_status(
    database: &Database,
    slug: &str,
    build: impl FnOnce(Option<WorkStatusRecord>) -> Result<WorkStatusRecord, String> + Send + 'static,
) -> Result<Result<StatusWrite, String>> {
    enum Refusal {
        Usage(String),
        Failed(anyhow::Error),
    }
    let slug = slug.to_owned();
    let utc_offset_minutes = super::watch::clock().utc_offset_minutes;
    database
        .call(move |store| {
            let mut built = None;
            let write =
                store.update_slug_work_status_local(&slug, utc_offset_minutes, |stored| {
                    let previous = stored
                        .and_then(|stored| serde_json::to_value(stored).ok())
                        .as_ref()
                        .and_then(parse_work_status);
                    let next = build(previous.clone()).map_err(Refusal::Usage)?;
                    let stored: StoredWorkStatusRecord = serde_json::to_value(&next)
                        .and_then(serde_json::from_value)
                        .context("encode status record")
                        .map_err(Refusal::Failed)?;
                    built = Some((previous, next));
                    Ok(Some(stored))
                })?;
            match write {
                Ok(write) => {
                    let (previous, next) = built.expect("a written status was built");
                    Ok(Ok(StatusWrite {
                        previous,
                        next,
                        wrote: write.changed,
                        cancelled: write.cancelled_watches,
                        stamped: write.stamped,
                        displaced: write.displaced,
                    }))
                }
                Err(Refusal::Usage(message)) => Ok(Err(message)),
                Err(Refusal::Failed(error)) => Err(error),
            }
        })
        .await
}

/// The current status with only its note, risk, gate or owed check changed.
fn amend_record(
    slug: &str,
    previous: Option<WorkStatusRecord>,
    note: Option<String>,
    risk: Option<WorkRisk>,
    blocked: Option<Option<String>>,
    verify: Vec<Verify>,
    append: bool,
) -> Result<WorkStatusRecord, String> {
    let Some(mut next) = previous else {
        return Err(format!("{slug} has no status asserted; set a state first"));
    };
    if risk.is_some() && next.state != WorkState::Ready {
        return Err("--risk only applies to ready".into());
    }
    if let Some(Some(_)) = blocked
        && !matches!(next.state, WorkState::Ready | WorkState::Todo)
    {
        return Err("--blocked-on applies to ready and todo".into());
    }
    if blocked == Some(None) && next.blocked_on.is_none() {
        return Err(format!("{slug} has no gate to clear"));
    }
    if !verify.is_empty() && matches!(next.state, WorkState::Verified | WorkState::Dropped) {
        return Err(format!(
            "--verify-in-* does not apply to {}",
            work_state_name(next.state)
        ));
    }
    next.note = merge_note(next.note.as_deref(), note.as_deref(), append, true);
    if let Some(risk) = risk {
        next.risk = Some(risk);
    }
    if let Some(blocked) = blocked {
        next.blocked_on = blocked;
    }
    let amended = next.clone();
    wt_ops::status_checks::assert_checks(
        &mut next,
        Some(&amended),
        verify.into_iter().map(Verify::edit),
    );
    if next.risk.is_some_and(|r| r != WorkRisk::Low) && next.note.is_none() {
        return Err("--risk medium|high requires -m explaining uncertainty".into());
    }
    Ok(next)
}

/// Record the outcome of a removed row's owed post-merge check.
async fn settle_removed(
    ctx: &AppContext,
    slug: &str,
    work_state: WorkState,
    note: Option<String>,
    unresolved: anyhow::Error,
) -> Result<i32> {
    let mut next = WorkStatusRecord::new(work_state, now_iso());
    next.by = std::env::var("WT_AGENT")
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty());
    next.note = note;
    let stored: StoredWorkStatusRecord =
        serde_json::from_value(serde_json::to_value(&next)?).context("encode status record")?;
    let db_slug = slug.to_owned();
    if work_state == WorkState::Verified {
        // Nothing can be read of a removed row's deploys: verified closes
        // its earliest check, and the row reads verified once none remains.
        let evidence = next.note.clone();
        let closed = ctx
            .database
            .call(move |s| {
                Ok(s.close_removed_checks(
                    &db_slug,
                    &wt_store::CheckTargets::Earliest,
                    &stored,
                    evidence.as_deref(),
                )?)
            })
            .await?;
        return Ok(match closed {
            wt_store::RemovedChecksClosed::Closed { closed, remaining } => {
                super::owed::print_checks_closed(slug, &closed, &remaining, None, true);
                if let Some(note) = &next.note {
                    println!("  note: {note}");
                }
                0
            }
            wt_store::RemovedChecksClosed::NotOwed => {
                eprintln!("{slug} was removed and owes no post-merge check");
                1
            }
            wt_store::RemovedChecksClosed::Missing => {
                eprintln!("{unresolved}");
                1
            }
        });
    }
    let settled = ctx
        .database
        .call(move |s| Ok(s.settle_removed_verification(&db_slug, &stored)?))
        .await?;
    match settled {
        wt_store::RemovedSettle::Settled { verify_after_merge } => {
            println!("✓ {slug} {} (removed row)", work_state_name(work_state));
            if let Some(steps) = verify_after_merge {
                println!("  settled check: {steps}");
            }
            if let Some(note) = &next.note {
                println!("  note: {note}");
            }
            Ok(0)
        }
        wt_store::RemovedSettle::NotOwed => {
            eprintln!("{slug} was removed and owes no post-merge check");
            Ok(1)
        }
        wt_store::RemovedSettle::Missing => {
            eprintln!("{unresolved}");
            Ok(1)
        }
    }
}

fn parse_risk(raw: &str) -> Option<WorkRisk> {
    let q = raw.trim().to_ascii_lowercase();
    let values = [
        ("low", WorkRisk::Low),
        ("medium", WorkRisk::Medium),
        ("high", WorkRisk::High),
    ];
    let matches = values
        .iter()
        .filter(|(name, _)| name.starts_with(&q))
        .collect::<Vec<_>>();
    if matches.len() == 1 {
        Some(matches[0].1)
    } else {
        None
    }
}

fn is_ambiguous_state(raw: &str) -> bool {
    let query = raw.trim().to_lowercase();
    let matches = wt_core::WORK_STATES
        .into_iter()
        .filter(|state| state.as_str().starts_with(&query))
        .count();
    !query.is_empty() && matches > 1
}

fn merge_note(
    previous: Option<&str>,
    incoming: Option<&str>,
    append: bool,
    keep_absent: bool,
) -> Option<String> {
    match incoming {
        Some(note) if append && previous.is_some() => {
            Some(format!("{} {note}", previous.unwrap_or_default()))
        }
        Some(note) => Some(note.to_owned()),
        None if keep_absent => previous.map(str::to_owned),
        None => None,
    }
}

/// What an assertion did with the gate the previous status held, where the
/// command did not say: a status change keeps it, and an ending (verified
/// or dropped) clears it. Both are said, so neither happens unseen.
fn gate_line(previous: Option<&str>, current: Option<&str>, asked: bool) -> Option<String> {
    let gate = previous?;
    match current {
        None => Some(format!("  gate cleared (was: {gate})")),
        Some(kept) if kept == gate && !asked => Some(format!(
            "  still blocked on: {gate} (a status change keeps the gate; wt status --unblock clears it)"
        )),
        Some(_) => None,
    }
}

pub(crate) fn report_replaced_note(previous: Option<&str>, current: Option<&str>, append: bool) {
    if !append && previous.is_some() && previous != current {
        println!(
            "  previous note (now gone): {}",
            previous.unwrap_or_default()
        );
    }
}

fn note_budget_hint(state: WorkState, note: Option<&str>) {
    if state == WorkState::Ready && note.is_some_and(|note| note.chars().count() > 500) {
        println!(
            "note is long; a concise ready note is easier to review. Keep one line of outcome, then REVERT / IF WRONG / UNTESTED; what is still owed goes in the owed list, not the note."
        );
    }
}

fn hints_off() -> bool {
    std::env::var("WT_NO_HINTS").is_ok_and(|value| value == "1")
}

fn print_guidance(state: WorkState, blocked_on: Option<&str>, clears: Option<&str>) {
    // Not a hint: it is the one line that says the gate will clear itself.
    if let (Some(gate), Some(clears)) = (blocked_on, clears) {
        println!(
            "wt clears the gate \"{gate}\" {clears}; `wt status --unblock` clears it by hand."
        );
    }
    if hints_off() {
        return;
    }
    if blocked_on.is_some() && clears.is_some() {
        return;
    }
    if let Some(gate) = blocked_on {
        if state == WorkState::Todo {
            println!(
                "deliberately not started until {gate}; when it clears, use `wt status --unblock`."
            );
        } else {
            println!("do not merge until {gate}; when it clears, use `wt status --unblock`.");
        }
        return;
    }
    let next = match state {
        WorkState::Todo => "when you pick this up: wt status working",
        WorkState::Working => {
            "finish review and testing yourself, then wt status ready --risk <low|medium|high>"
        }
        WorkState::Review => {
            "after review, complete manual/browser testing, then wt status ready --risk <r>"
        }
        WorkState::NeedsTesting => {
            "you own the dev/browser testing; when it passes, assert wt status ready --risk <r>"
        }
        WorkState::NeedsHuman => {
            "the note should say what you need and what you already tried; keep working on unblocked parts"
        }
        WorkState::Ready => "leave the PR ready for the human to merge; do not merge it yourself",
        WorkState::Verified => "every check owed after merging is closed",
        WorkState::Dropped => {
            "close any open PR and explain why; leave worktree removal to the human"
        }
    };
    println!("{next}");
}

fn work_state_name(state: WorkState) -> &'static str {
    match state {
        WorkState::Todo => "todo",
        WorkState::Working => "working",
        WorkState::Review => "review",
        WorkState::NeedsTesting => "needs-testing",
        WorkState::NeedsHuman => "needs-human",
        WorkState::Ready => "ready",
        WorkState::Verified => "verified",
        WorkState::Dropped => "dropped",
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

fn now_iso() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".into())
}

fn describe(slug: &str, record: Option<&WorkStatusRecord>, head: Option<&str>, now: i64) -> String {
    let Some(record) = record else {
        return format!("{slug}  no status asserted");
    };
    let state = work_state_name(record.state);
    let blocked = if record.blocked_on.is_some() {
        format!("blocked/{state}")
    } else {
        state.to_owned()
    };
    let age = work_age(&record.at, now)
        .map(|age| format!("  {age} ago"))
        .unwrap_or_default();
    let stale = if record
        .sha
        .as_deref()
        .zip(head)
        .is_some_and(|(old, new)| old != new)
    {
        "  (stale: commits since)"
    } else {
        ""
    };
    let via = record
        .by
        .as_deref()
        .filter(|by| *by != slug)
        .map(|by| format!("  via {by}"))
        .unwrap_or_default();
    let mut out = format!(
        "{slug}  {blocked}{}{}{}",
        record
            .risk
            .map(|risk| format!("  risk: {}", risk.as_str()))
            .unwrap_or_default(),
        age,
        stale
    );
    out.push_str(&via);
    if let Some(gate) = &record.blocked_on {
        out.push_str(&format!("\n  blocked on: {gate}"));
    }
    for (stage, check) in record.open_checks() {
        out.push_str(&format!(
            "\n  verify in {}: {}",
            stage.as_str(),
            check.steps
        ));
    }
    if let Some(note) = &record.note {
        out.push_str(&format!("\n  note: {note}"));
    }
    out
}

async fn read_state(database: &Database) -> Result<Value> {
    database.call(|s| Ok(s.read_wt_state()?)).await
}

async fn rev_parse(
    ctx: &AppContext,
    cwd: &std::path::Path,
    reference: &str,
) -> Result<Option<String>> {
    let spec = CommandSpec::new("git")
        .args([
            OsString::from("rev-parse"),
            OsString::from("--verify"),
            OsString::from("--quiet"),
            OsString::from("--end-of-options"),
            OsString::from(format!("{reference}^{{commit}}")),
        ])
        .cwd(cwd);
    let output = ctx.processes.run(spec, &ctx.cancellation).await?;
    match output.status.code() {
        Some(0) => Ok(Some(output.stdout_text().trim().to_owned()).filter(|s| !s.is_empty())),
        Some(1) => Ok(None),
        _ => {
            output.checked("git")?;
            unreachable!("successful exit handled above")
        }
    }
}

async fn base_tip_sha(ctx: &AppContext, branch: &str) -> Result<Option<String>> {
    let remote = format!("origin/{branch}");
    if let Some(sha) = rev_parse(ctx, &ctx.config.paths.main_clone, &remote).await? {
        return Ok(Some(sha));
    }
    rev_parse(ctx, &ctx.config.paths.main_clone, branch).await
}

/// One live worktree's entry of `wt status --all --json`.
async fn live_json(
    ctx: &AppContext,
    state: &Value,
    w: &WorktreeRecord,
    landing: RowLanding,
) -> Result<Value> {
    let slug = w.target.slug();
    let clock = super::watch::clock();
    let owed = super::owed::derive(state, slug, landing, clock.now_ms);
    let record = state.get("slugs").and_then(|s| s.get(slug));
    let work = record
        .and_then(|r| r.get("work"))
        .and_then(parse_work_status);
    let head = w.head_sha.clone();
    let examined = record.and_then(|r| r.get("examined"));
    let base_branch = record
        .and_then(|r| r.get("baseBranch"))
        .and_then(Value::as_str)
        .unwrap_or(&ctx.config.branch.base);
    let base_sha = base_tip_sha(ctx, base_branch).await?;
    let examined_current = match (
        examined.and_then(|e| e.get("sha")).and_then(Value::as_str),
        head.as_deref(),
        examined
            .and_then(|e| e.get("baseSha"))
            .and_then(Value::as_str),
        base_sha.as_deref(),
    ) {
        (None, _, _, _) | (_, None, _, _) => None,
        (Some(_), Some(_), None, _) => Some(false),
        (Some(_), Some(_), Some(_), None) => None,
        (Some(sha), Some(head), Some(old_base), Some(base)) => {
            Some(sha == head && old_base == base)
        }
    };
    let stamp = work.as_ref().and_then(wt_core::Stamp::of).map(|stamp| {
        json!({"skill": stamp.skill, "sha": stamp.sha, "at": stamp.at, "current": stamp.current(head.as_deref())})
    });
    Ok(
        json!({"stamp":stamp,"slug":slug,"branch":w.target.branch,"kind":"live","other_prs":wt_core::other_prs_json(record.unwrap_or(&Value::Null)),"section":if ctx.config.instance.role==InstanceRole::Worker {None}else{record.and_then(|r|r.get("section")).and_then(Value::as_str)},"state":work.as_ref().map(|x|x.state),"note":work.as_ref().and_then(|x|x.note.as_deref()),"risk":work.as_ref().and_then(|x|x.risk),"blocked_on":work.as_ref().and_then(|x|x.blocked_on.as_deref()),"verify_after_merge":work.as_ref().and_then(|x|x.verify_after_merge.as_deref()),"verify_in":work.as_ref().and_then(|x|x.verification()).map(|(stage,_)|stage),"at":work.as_ref().map(|x|x.at.as_str()),"watches":super::watch::watches_json(&wt_store::stored_watches(state), slug, clock),"owes":super::owed::owes_json(&owed.items, clock),"owed_phase":super::owed::phase_json(owed.phase),"landing":owed.landing.word(),"examined":examined,"examined_current":examined_current,"by":work.as_ref().and_then(|x|x.by.as_deref()),"stale":work.as_ref().is_some_and(|x|x.sha.as_deref().zip(head.as_deref()).is_some_and(|(old,new)|old!=new))}),
    )
}

fn removed_json(entry: wt_store::RemovedWorktree, separate_production: bool) -> Value {
    let clock = super::watch::clock();
    let owed = super::owed::derive_removed(&entry, separate_production, clock.now_ms);
    let work_state = entry.work.as_ref().map(|work| work.state.as_str());
    let verification_owed = wt_store::verification_owed_at_removal(&entry);
    let merged = wt_store::is_merged_removal(&entry);
    json!({"slug":entry.slug,"branch":entry.branch,"kind":if merged {"merged"} else {"removed"},"pr":entry.extra.get("prNumber").cloned().unwrap_or(Value::Null),"pr_url":entry.extra.get("prUrl").cloned().unwrap_or(Value::Null),"title":entry.extra.get("title").cloned().unwrap_or(Value::Null),"archived_at":entry.removed_at,"work_state":work_state,"verify_in":entry.work.as_ref().and_then(|work|work.verify_after_merge.as_ref().map(|_|VerifyStage::of_stored(work.verify_in.as_deref()).as_str())),"verify_after_merge":entry.work.as_ref().and_then(|work|work.verify_after_merge.as_deref()),"verification_owed":verification_owed,"owes":super::owed::owes_json(&owed.items, clock),"owed_phase":super::owed::phase_json(owed.phase),"landing":owed.landing.word()})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn base_lookup_distinguishes_missing_ref_from_cancellation() {
        let fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        assert!(base_tip_sha(&fixture.ctx, "main").await.unwrap().is_some());
        assert!(
            base_tip_sha(&fixture.ctx, "missing")
                .await
                .unwrap()
                .is_none()
        );
        fixture.ctx.cancellation.cancel();
        assert!(
            base_tip_sha(&fixture.ctx, "main")
                .await
                .unwrap_err()
                .to_string()
                .contains("cancelled")
        );
        fixture.close().await.unwrap();
    }

    #[test]
    fn status_parser_preserves_required_gate_risk_and_amendment_rules() {
        let args = |positionals: &[&str], risk: Option<&str>, note: Option<&str>| StatusArgs {
            positionals: positionals.iter().map(|s| s.to_string()).collect(),
            risk: risk.map(str::to_owned),
            note: note.map(str::to_owned),
            ..Default::default()
        };
        assert!(matches!(
            parse_status_args(&args(&["ready"], None, None)),
            Action::Error(_)
        ));
        assert!(matches!(
            parse_status_args(&args(&["ready"], Some("low"), None)),
            Action::Set { .. }
        ));
        assert!(matches!(
            parse_status_args(&args(&["ready"], Some("medium"), None)),
            Action::Error(_)
        ));
        assert!(matches!(
            parse_status_args(&args(&["x", "todo"], None, None)),
            Action::Set {
                target: Some(_),
                ..
            }
        ));
    }

    #[tokio::test]
    async fn verify_flags_record_a_stage_and_the_stageless_flag_is_refused() {
        use wt_app_core::test_support::CommandFixture;
        let legacy = StatusArgs {
            positionals: vec!["ready".into()],
            risk: Some("low".into()),
            verify_after_merge: Some("check it".into()),
            ..Default::default()
        };
        let Action::Error(message) = parse_status_args(&legacy) else {
            panic!("the stage-less flag must be refused");
        };
        assert!(message.contains("--verify-in-staging"), "{message}");
        let fixture = CommandFixture::new().await.unwrap();
        let production = StatusArgs {
            positionals: vec!["ready".into()],
            risk: Some("low".into()),
            verify_in_production: Some("STEPS: 1. check prod".into()),
            ..Default::default()
        };
        assert_eq!(run(&fixture.ctx, &production).await.unwrap(), 0);
        let state = read_state(&fixture.ctx.database).await.unwrap();
        assert_eq!(state["slugs"]["one"]["work"]["verifyIn"], "production");
        // A later assertion carries the obligation and its stage.
        let working = StatusArgs {
            positionals: vec!["working".into()],
            ..Default::default()
        };
        assert_eq!(run(&fixture.ctx, &working).await.unwrap(), 0);
        let state = read_state(&fixture.ctx.database).await.unwrap();
        assert_eq!(state["slugs"]["one"]["work"]["verifyIn"], "production");
        assert_eq!(
            state["slugs"]["one"]["work"]["verifyAfterMerge"],
            "STEPS: 1. check prod"
        );
        // Amending moves it to staging in place.
        let amend = StatusArgs {
            verify_in_staging: Some("STEPS: 1. check staging".into()),
            ..Default::default()
        };
        assert_eq!(run(&fixture.ctx, &amend).await.unwrap(), 0);
        let state = read_state(&fixture.ctx.database).await.unwrap();
        assert_eq!(state["slugs"]["one"]["work"]["verifyIn"], "staging");
        assert_eq!(state["slugs"]["one"]["work"]["state"], "working");
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn amend_decides_from_the_status_stored_when_it_writes() {
        use wt_app_core::test_support::CommandFixture;
        let fixture = CommandFixture::new().await.unwrap();
        let gated = StatusArgs {
            positionals: vec!["todo".into()],
            blocked_on: Some("release train lands".into()),
            ..Default::default()
        };
        assert_eq!(run(&fixture.ctx, &gated).await.unwrap(), 0);
        // `--unblock` was valid against the gated `todo` its caller saw, but
        // the worktree's agent has asked for a human and the gate has been
        // cleared since.
        let claim = StatusArgs {
            positionals: vec!["needs-human".into()],
            note: Some("approve the migration".into()),
            verify_in_production: Some("STEPS: 1. check prod".into()),
            ..Default::default()
        };
        assert_eq!(run(&fixture.ctx, &claim).await.unwrap(), 0);
        let unblock = StatusArgs {
            unblock: true,
            ..Default::default()
        };
        assert_eq!(run(&fixture.ctx, &unblock).await.unwrap(), 0);
        let refused = update_status(&fixture.ctx.database, "one", |previous| {
            amend_record("one", previous, None, None, Some(None), Vec::new(), false)
        })
        .await
        .unwrap();
        assert_eq!(refused.err().as_deref(), Some("one has no gate to clear"));
        // A new assertion carries the check owed by the record it replaces.
        let write = update_status(&fixture.ctx.database, "one", |previous| {
            let mut next = WorkStatusRecord::new(WorkState::Review, "t");
            next.verify_after_merge = previous.and_then(|previous| previous.verify_after_merge);
            Ok(next)
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(write.previous.unwrap().state, WorkState::NeedsHuman);
        let state = read_state(&fixture.ctx.database).await.unwrap();
        assert_eq!(state["slugs"]["one"]["work"]["state"], "review");
        assert_eq!(
            state["slugs"]["one"]["work"]["verifyAfterMerge"],
            "STEPS: 1. check prod"
        );
        fixture.close().await.unwrap();
    }

    fn parsed(argv: &[&str]) -> Action {
        #[derive(clap::Parser)]
        struct Harness {
            #[command(flatten)]
            args: StatusArgs,
        }
        use clap::Parser;
        parse_status_args(
            &Harness::try_parse_from(std::iter::once("status").chain(argv.iter().copied()))
                .unwrap()
                .args,
        )
    }

    /// `--until #7:production` reaches wt as a bare `--until`. It must
    /// parse, so the command can say what happened.
    #[test]
    fn until_parses_with_its_condition_lost_to_a_shell_comment() {
        #[derive(clap::Parser)]
        struct Harness {
            #[command(flatten)]
            args: StatusArgs,
        }
        use clap::Parser;
        let until = |argv: &[&str]| {
            Harness::try_parse_from(argv.iter().copied())
                .unwrap()
                .args
                .until
        };
        assert_eq!(
            until(&[
                "status",
                "ready",
                "--risk",
                "low",
                "--blocked-on",
                "x",
                "--until"
            ]),
            [""]
        );
        assert_eq!(
            until(&["status", "--until", "pr:7", "--until", "+1h"]),
            ["pr:7", "+1h"]
        );
    }

    #[test]
    fn a_flag_swallowed_as_another_flags_value_is_named() {
        let Action::Error(message) = parsed(&[
            "ready",
            "--risk",
            "low",
            "--verify-in-staging",
            "-m",
            "note",
        ]) else {
            panic!("a value flag given another flag as its value must be refused");
        };
        assert!(
            message.starts_with("--verify-in-staging needs its steps as the next argument"),
            "{message}"
        );
        assert!(message.contains("`-m`"), "{message}");
        for (flag, what) in [
            ("--blocked-on", "gate"),
            ("--verify-in-production", "steps"),
            ("--examined", "verdict"),
            ("--note-only", "note"),
        ] {
            let Action::Error(message) = parsed(&[flag, "--json"]) else {
                panic!("{flag} took a flag as its value");
            };
            assert!(
                message.starts_with(&format!("{flag} needs its {what}")),
                "{message}"
            );
        }
        // Hyphenated prose is still a value.
        assert!(matches!(
            parsed(&["todo", "--blocked-on", "-m is not a gate, this is"]),
            Action::Set { .. }
        ));
        assert!(matches!(
            parsed(&["ready", "--risk", "low", "--verify-in-staging", "- check"]),
            Action::Set { .. }
        ));
        let Action::Error(message) = parsed(&["one", "bogus"]) else {
            panic!("an unknown state must be refused");
        };
        assert!(message.contains("`bogus`"), "{message}");
    }

    #[test]
    fn json_applies_to_reads_only() {
        assert_eq!(
            parsed(&["one", "--json"]),
            Action::Show {
                target: Some("one".into()),
                json: true
            }
        );
        assert_eq!(parsed(&["--all", "--json"]), Action::All { json: true });
        for write in [
            &["working", "--json"][..],
            &["--clear", "--json"],
            &["--unblock", "--json"],
            &["--examined", "fine", "--json"],
        ] {
            assert!(matches!(parsed(write), Action::Error(_)), "{write:?}");
        }
    }

    #[tokio::test]
    async fn one_row_json_has_the_shape_of_an_all_entry() {
        use wt_app_core::test_support::CommandFixture;
        let fixture = CommandFixture::new().await.unwrap();
        let gated = StatusArgs {
            positionals: vec!["one".into(), "todo".into()],
            blocked_on: Some("release train lands".into()),
            ..Default::default()
        };
        assert_eq!(run(&fixture.ctx, &gated).await.unwrap(), 0);
        let show = StatusArgs {
            positionals: vec!["one".into()],
            json: true,
            ..Default::default()
        };
        assert_eq!(run(&fixture.ctx, &show).await.unwrap(), 0);
        let records = fixture
            .ctx
            .repository
            .inventory(&fixture.ctx.cancellation)
            .await
            .unwrap();
        let one = records.iter().find(|w| w.target.slug() == "one").unwrap();
        let state = read_state(&fixture.ctx.database).await.unwrap();
        let entry = live_json(
            &fixture.ctx,
            &state,
            one,
            RowLanding::known(wt_core::Landing::NONE),
        )
        .await
        .unwrap();
        assert_eq!(entry["slug"], "one");
        assert_eq!(entry["state"], "todo");
        assert_eq!(entry["blocked_on"], "release train lands");
        assert_eq!(entry["kind"], "live");
        // What the row owes rides along: here its gate, and no phase until
        // its own work has landed.
        assert_eq!(entry["owes"].as_array().unwrap().len(), 1);
        assert_eq!(
            (&entry["owes"][0]["kind"], &entry["owes"][0]["what"]),
            (&json!("gate"), &json!("release train lands"))
        );
        assert!(entry.as_object().unwrap().contains_key("owed_phase"));
        assert!(entry["owed_phase"].is_null());
        let missing = StatusArgs {
            positionals: vec!["absent".into()],
            json: true,
            ..Default::default()
        };
        assert_eq!(run(&fixture.ctx, &missing).await.unwrap(), 1);
        fixture.close().await.unwrap();
    }

    #[test]
    fn a_gate_kept_or_cleared_without_being_asked_is_said() {
        assert_eq!(
            gate_line(Some("mobile release"), None, false).as_deref(),
            Some("  gate cleared (was: mobile release)")
        );
        assert!(
            gate_line(Some("mobile release"), Some("mobile release"), false)
                .unwrap()
                .starts_with("  still blocked on: mobile release")
        );
        // The command named the gate itself: nothing to add.
        assert_eq!(gate_line(Some("old"), Some("new"), true), None);
        assert_eq!(gate_line(Some("same"), Some("same"), true), None);
        assert_eq!(gate_line(None, Some("new"), true), None);
        assert_eq!(gate_line(None, None, false), None);
    }

    #[tokio::test]
    async fn a_status_change_keeps_the_gate_and_the_watch_that_clears_it() {
        use wt_app_core::test_support::CommandFixture;
        let fixture = CommandFixture::new().await.unwrap();
        let ctx = &fixture.ctx;
        let stated = |positionals: &[&str], extra: StatusArgs| StatusArgs {
            positionals: positionals.iter().map(|word| (*word).to_owned()).collect(),
            ..extra
        };
        let ready = || StatusArgs {
            risk: Some("low".into()),
            ..Default::default()
        };
        let gated = StatusArgs {
            blocked_on: Some("the mobile release".into()),
            until: vec!["3h".into()],
            ..ready()
        };
        assert_eq!(run(ctx, &stated(&["ready"], gated)).await.unwrap(), 0);
        let gate = || async {
            let state = read_state(&ctx.database).await.unwrap();
            let clears = wt_store::stored_watches(&state)
                .iter()
                .filter(|watch| watch.slug == "one")
                .filter_map(|watch| watch.then.unblock.clone())
                .collect::<Vec<_>>();
            (
                state["slugs"]["one"]["work"]["blockedOn"]
                    .as_str()
                    .map(str::to_owned),
                clears,
            )
        };
        let held = (
            Some("the mobile release".to_owned()),
            vec!["the mobile release".to_owned()],
        );
        assert_eq!(gate().await, held);

        // A review round, and the ready that follows it, say nothing of the
        // gate: it is a prerequisite of merging, not a claim of one status.
        for next in [
            stated(&["review"], StatusArgs::default()),
            stated(&["working"], StatusArgs::default()),
            stated(&["ready"], ready()),
        ] {
            assert_eq!(run(ctx, &next).await.unwrap(), 0, "{next:?}");
            assert_eq!(gate().await, held, "{next:?}");
        }

        // --until on a later assertion attaches to the gate that is there,
        // and is refused where there is none, with nothing written.
        let later = StatusArgs {
            until: vec!["4h".into()],
            ..ready()
        };
        // One watch still clears the gate, and it is the later one.
        let clears_at = || async {
            let state = read_state(&ctx.database).await.unwrap();
            wt_store::stored_watches(&state)
                .iter()
                .filter(|watch| watch.then.unblock.as_deref() == held.0.as_deref())
                .map(|watch| {
                    watch
                        .when
                        .iter()
                        .filter_map(wt_core::watch::WatchCondition::known_at_ms)
                        .max()
                })
                .collect::<Vec<_>>()
        };
        let before = clears_at().await;
        assert_eq!(
            run(ctx, &stated(&["ready"], later.clone())).await.unwrap(),
            0
        );
        assert_eq!(gate().await, held);
        let after = clears_at().await;
        assert_eq!((before.len(), after.len()), (1, 1));
        assert!(after[0] > before[0], "{before:?} {after:?}");
        assert_eq!(
            run(ctx, &stated(&["two", "ready"], later)).await.unwrap(),
            2
        );
        let state = read_state(&ctx.database).await.unwrap();
        assert!(state["slugs"]["two"]["work"].is_null());

        // A new gate replaces it and cancels the watch set for the old text.
        let replaced = StatusArgs {
            blocked_on: Some("the web release".into()),
            ..ready()
        };
        assert_eq!(run(ctx, &stated(&["ready"], replaced)).await.unwrap(), 0);
        assert_eq!(
            gate().await,
            (Some("the web release".to_owned()), Vec::new())
        );

        // Dropping the work ends what the gate held back.
        let dropped = StatusArgs {
            note: Some("superseded".into()),
            ..Default::default()
        };
        assert_eq!(run(ctx, &stated(&["dropped"], dropped)).await.unwrap(), 0);
        assert_eq!(gate().await, (None, Vec::new()));
        fixture.close().await.unwrap();
    }

    #[test]
    fn status_record_serialization_matches_store_shape() {
        let record = WorkStatusRecord::new(WorkState::Ready, "2026-10-09T00:00:00Z");
        let value = serde_json::to_value(record).unwrap();
        assert_eq!(value["state"], "ready");
        assert_eq!(value["at"], "2026-10-09T00:00:00Z");
    }

    #[tokio::test]
    async fn status_writes_gate_then_unblocks_without_changing_assertion_time() {
        use wt_app_core::test_support::CommandFixture;
        let fixture = CommandFixture::new().await.unwrap();
        let mut set = StatusArgs {
            positionals: vec!["todo".into()],
            blocked_on: Some("release train lands".into()),
            ..Default::default()
        };
        assert_eq!(run(&fixture.ctx, &set).await.unwrap(), 0);
        let before = read_state(&fixture.ctx.database).await.unwrap();
        let at = before["slugs"]["one"]["work"]["at"].clone();
        assert_eq!(
            before["slugs"]["one"]["work"]["blockedOn"],
            "release train lands"
        );

        set = StatusArgs {
            unblock: true,
            ..Default::default()
        };
        assert_eq!(run(&fixture.ctx, &set).await.unwrap(), 0);
        let after = read_state(&fixture.ctx.database).await.unwrap();
        assert_eq!(after["slugs"]["one"]["work"]["state"], "todo");
        assert_eq!(after["slugs"]["one"]["work"]["at"], at);
        assert!(after["slugs"]["one"]["work"].get("blockedOn").is_none());
        fixture.close().await.unwrap();
    }
    #[tokio::test]
    async fn removed_row_settles_only_an_owed_post_merge_check() {
        use wt_app_core::test_support::CommandFixture;
        let fixture = CommandFixture::new().await.unwrap();
        let removed = |slug: &str, state: &str, verify: Option<&str>| wt_store::RemovedWorktree {
            slug: slug.into(),
            branch: format!("feature/{slug}"),
            removed_at: now_iso(),
            work: Some(StoredWorkStatusRecord {
                state: state.into(),
                at: now_iso(),
                note: Some("shipped".into()),
                risk: Some("low".into()),
                sha: None,
                by: None,
                blocked_on: None,
                verify_after_merge: verify.map(str::to_owned),
                verify_in: None,
                extra: Default::default(),
            }),
            automations_paused: None,
            extra: Default::default(),
        };
        let entries = vec![
            removed("owed", "ready", Some("check prod")),
            removed("clean", "ready", None),
        ];
        fixture
            .ctx
            .database
            .call(move |s| Ok(s.record_removed_worktrees(&entries, now_ms())?))
            .await
            .unwrap();
        let settle = |slug: &str, state: &str| StatusArgs {
            positionals: vec![slug.into(), state.into()],
            note: Some("owner confirmed".into()),
            ..Default::default()
        };
        // A claim about live work is never written to a removed row.
        assert_eq!(
            run(&fixture.ctx, &settle("owed", "working")).await.unwrap(),
            1
        );
        assert_eq!(
            run(&fixture.ctx, &settle("clean", "verified"))
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            run(&fixture.ctx, &settle("absent", "verified"))
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            run(&fixture.ctx, &settle("owed", "verified"))
                .await
                .unwrap(),
            0
        );
        let history = fixture
            .ctx
            .database
            .call(|s| Ok(s.read_removed_worktrees()?))
            .await
            .unwrap();
        let owed = history.iter().find(|entry| entry.slug == "owed").unwrap();
        let clean = history.iter().find(|entry| entry.slug == "clean").unwrap();
        assert!(!wt_store::verification_owed_at_removal(owed));
        let work = owed.work.as_ref().unwrap();
        assert_eq!(work.state, "verified");
        assert_eq!(work.note.as_deref(), Some("owner confirmed"));
        assert_eq!(clean.work.as_ref().unwrap().state, "ready");
        // Settling twice finds nothing owed.
        assert_eq!(
            run(&fixture.ctx, &settle("owed", "verified"))
                .await
                .unwrap(),
            1
        );
        fixture.close().await.unwrap();
    }

    #[test]
    fn a_gate_that_clears_on_less_than_it_names_says_so() {
        let clock = wt_core::watch::LocalClock {
            now_ms: 1_795_521_600_000,
            utc_offset_minutes: 0,
        };
        let watch = |conditions: &[&str]| wt_core::watch::Watch {
            id: "abc234".into(),
            slug: "one".into(),
            incarnation: None,
            by: "one".into(),
            created_at_ms: clock.now_ms,
            utc_offset_minutes: 0,
            when: conditions
                .iter()
                .map(|arg| wt_core::watch::parse(arg, clock).unwrap())
                .collect(),
            then: Default::default(),
            met_at_ms: None,
            last_error: None,
            owner_gone: false,
            extra: Default::default(),
        };
        // A bare time checks nothing the text names.
        let timed = gate_note("migration 0042 applied", &watch(&["3h"])).unwrap();
        assert!(timed.starts_with("this gate clears at "), "{timed}");
        assert!(timed.ends_with("; anything else it names is not checked"));
        // The condition covers one of two things the text names.
        assert!(
            gate_note(
                "#3806 in production and the flag flipped",
                &watch(&["pr:3806:production"])
            )
            .is_some()
        );
        assert!(gate_note("needs #3806 and #3900", &watch(&["pr:3806:merged"])).is_some());
        // The text names what the condition checks: nothing to add.
        assert_eq!(
            gate_note("needs #3806 in production", &watch(&["pr:3806:production"])),
            None
        );
    }

    #[test]
    fn until_needs_a_gate_and_then_needs_until() {
        let base = || StatusArgs {
            until: vec!["merged".into()],
            ..Default::default()
        };
        let error = |args: StatusArgs| match parse_status_args(&args) {
            Action::Error(message) => message,
            other => panic!("expected a refusal, got {other:?}"),
        };
        // On its own it amends: the gate may already be on the record.
        assert!(matches!(
            parse_status_args(&base()),
            Action::Amend { blocked: None, .. }
        ));
        assert!(matches!(
            parse_status_args(&StatusArgs {
                positionals: vec!["ready".into()],
                risk: Some("low".into()),
                blocked_on: Some("the release".into()),
                then: Some("verify it".into()),
                ..base()
            }),
            Action::Set {
                blocked: Some(_),
                ..
            }
        ));
        // An ending holds no gate, so there is none to add one to.
        for ending in ["verified", "dropped"] {
            let refusal = error(StatusArgs {
                positionals: vec![ending.into()],
                note: Some("done".into()),
                ..base()
            });
            assert!(
                refusal.contains("hold no gate") && !refusal.contains("add --blocked-on"),
                "{refusal}"
            );
        }
        // With a state too: a status change keeps the gate, so whether one
        // is there is known only at the write.
        assert!(matches!(
            parse_status_args(&StatusArgs {
                positionals: vec!["ready".into()],
                risk: Some("low".into()),
                ..base()
            }),
            Action::Set { blocked: None, .. }
        ));
        assert!(
            error(StatusArgs {
                then: Some("verify it".into()),
                ..Default::default()
            })
            .contains("add --until")
        );
        assert!(
            error(StatusArgs {
                unblock: true,
                ..base()
            })
            .contains("pick one")
        );
        assert!(
            error(StatusArgs {
                then: Some("--until".into()),
                ..base()
            })
            .contains("--then needs its prompt")
        );
        for args in [
            StatusArgs {
                clear: true,
                ..base()
            },
            StatusArgs {
                all: true,
                ..base()
            },
        ] {
            assert!(error(args).contains("--until says when"));
        }
    }

    #[tokio::test]
    async fn a_gate_set_with_until_gets_the_watch_that_clears_it() {
        use wt_app_core::test_support::CommandFixture;
        let fixture = CommandFixture::new().await.unwrap();
        let watches = || async {
            fixture
                .ctx
                .database
                .call(|store| Ok(store.list_watches(Some("one"))?))
                .await
                .unwrap()
        };
        // No gate on the record yet: nothing is written at all.
        let bare = StatusArgs {
            until: vec!["merged".into()],
            ..Default::default()
        };
        assert_eq!(run(&fixture.ctx, &bare).await.unwrap(), 2);
        // A condition that cannot be read refuses before the status is written.
        let unreadable = StatusArgs {
            positionals: vec!["ready".into()],
            risk: Some("low".into()),
            blocked_on: Some("#7 ships".into()),
            until: vec!["whenever".into()],
            ..Default::default()
        };
        assert_eq!(run(&fixture.ctx, &unreadable).await.unwrap(), 2);
        // What a shell leaves of `--until #7:production`: the flag alone.
        let lost = StatusArgs {
            until: vec![String::new()],
            ..unreadable.clone()
        };
        assert_eq!(run(&fixture.ctx, &lost).await.unwrap(), 2);
        let state = read_state(&fixture.ctx.database).await.unwrap();
        assert!(state["slugs"]["one"].get("work").is_none());

        let gated = StatusArgs {
            until: vec!["pr:7:production".into(), "+1h".into()],
            then: Some("run the smoke test".into()),
            ..unreadable.clone()
        };
        assert_eq!(run(&fixture.ctx, &gated).await.unwrap(), 0);
        let set = watches().await;
        assert_eq!(set.len(), 1);
        assert_eq!(set[0].then.unblock.as_deref(), Some("#7 ships"));
        assert_eq!(set[0].then.wake.as_deref(), Some("run the smoke test"));
        assert_eq!(set[0].when.len(), 2);
        let id = &set[0].id;
        assert!(
            clears_text(&set[0], false).starts_with("when #7 in production and "),
            "{}",
            clears_text(&set[0], false)
        );
        assert!(clears_text(&set[0], true).ends_with(&format!(
            ", and then wakes this session (watch {id}, already set)"
        )));
        // A gate that waits only for a time clears "at" it.
        let mut timed = set[0].clone();
        timed.when.remove(0);
        timed.then.wake = None;
        let said = clears_text(&timed, false);
        assert!(
            said.starts_with("at ") && said.ends_with(&format!(" (watch {id})")),
            "{said}"
        );

        // The row's JSON carries it.
        let state = read_state(&fixture.ctx.database).await.unwrap();
        let record = wt_ops::commands::resolve::resolve_worktree(&fixture.ctx, Some("one"))
            .await
            .unwrap();
        let row = live_json(
            &fixture.ctx,
            &state,
            &record,
            RowLanding::known(wt_core::Landing::NONE),
        )
        .await
        .unwrap();
        assert_eq!(row["watches"][0]["id"], set[0].id.as_str());
        assert_eq!(row["watches"][0]["when"][0]["arg"], "pr:7:production");
        assert_eq!(row["watches"][0]["then"]["unblock"], "#7 ships");

        // --until on the existing gate replaces the watch that clears it.
        assert_eq!(run(&fixture.ctx, &bare).await.unwrap(), 0);
        let replaced = watches().await;
        assert_eq!(replaced.len(), 1);
        assert_ne!(replaced[0].id, set[0].id);
        assert_eq!(replaced[0].then.unblock.as_deref(), Some("#7 ships"));
        // The wake set with the gate is not dropped by a command that does
        // not mention it.
        assert_eq!(replaced[0].then.wake.as_deref(), Some("run the smoke test"));
        // The same command again (agents retry) is the same watch.
        assert_eq!(run(&fixture.ctx, &bare).await.unwrap(), 0);
        assert_eq!(watches().await, replaced);
        // A session cannot be woken about its own worktree being gone.
        let own_removal = StatusArgs {
            until: vec!["one:gone".into()],
            then: Some("clean up".into()),
            ..Default::default()
        };
        assert_eq!(run(&fixture.ctx, &own_removal).await.unwrap(), 1);
        assert_eq!(watches().await, replaced);
        // A prompt written as steps keeps its lines.
        let steps = StatusArgs {
            until: vec!["merged".into()],
            then: Some("1. Deploy.\n2. Verify.\n".into()),
            ..Default::default()
        };
        assert_eq!(run(&fixture.ctx, &steps).await.unwrap(), 0);
        assert_eq!(
            watches().await[0].then.wake.as_deref(),
            Some("1. Deploy.\n2. Verify.")
        );

        // Clearing the gate by hand leaves nothing to clear it later.
        let unblock = StatusArgs {
            unblock: true,
            ..Default::default()
        };
        assert_eq!(run(&fixture.ctx, &unblock).await.unwrap(), 0);
        assert_eq!(watches().await, []);
        fixture.close().await.unwrap();
    }

    #[test]
    fn a_note_restating_what_is_owed_is_refused_with_where_each_step_goes() {
        for line in OPS_PHASE_LINES {
            let note = format!("ships the fix. {line} run the backfill");
            for argv in [
                vec!["ready", "--risk", "low", "-m", note.as_str()],
                vec!["--note-only", note.as_str()],
            ] {
                let Action::Error(message) = parsed(&argv) else {
                    panic!("{line} must be refused");
                };
                for teaches in [
                    line,
                    "--blocked-on",
                    "--verify-in-staging",
                    "wt wake <condition> -m",
                    "--before-release",
                    "wt owed",
                ] {
                    assert!(message.contains(teaches), "{teaches}: {message}");
                }
            }
        }
        // Saying nothing is owed is a note like any other.
        assert!(matches!(
            parsed(&["ready", "--risk", "low", "-m", "ships the fix. OPS: none"]),
            Action::Set { .. }
        ));
        // Both verify flags go together now; a stage cannot be set and
        // dropped at once.
        let Action::Set { verify, .. } = parsed(&[
            "ready",
            "--risk",
            "low",
            "--verify-in-staging",
            "1. a",
            "--verify-in-production",
            "1. b",
        ]) else {
            panic!("both flags must parse");
        };
        assert_eq!(
            verify.iter().map(|verify| verify.stage).collect::<Vec<_>>(),
            [VerifyStage::Staging, VerifyStage::Production]
        );
        assert!(matches!(
            parsed(&["--no-verify-in-production"]),
            Action::Amend { unverify, .. } if unverify == [VerifyStage::Production]
        ));
        assert!(matches!(
            parsed(&["verified", "-m", "ok", "--no-verify-in-staging"]),
            Action::Error(_)
        ));
    }

    /// `wt next` marks a merged pull request's open checks as its own. The
    /// next pull request's check takes the stage, and the earlier check
    /// stays owed as a step instead of being replaced unseen.
    #[tokio::test]
    async fn a_check_for_the_next_pr_keeps_the_earlier_prs_check_as_a_step() {
        use wt_app_core::test_support::CommandFixture;
        let fixture = CommandFixture::new().await.unwrap();
        let ctx = &fixture.ctx;
        let ready = |staging: &str, production: &str| StatusArgs {
            positionals: vec!["ready".into()],
            risk: Some("low".into()),
            note: Some("ships".into()),
            verify_in_staging: Some(staging.into()),
            verify_in_production: Some(production.into()),
            ..Default::default()
        };
        let leave = |pr: u64| async move {
            ctx.database
                .call(move |store| Ok(store.leave_checks("one", &wt_core::LeftFor::Pr(pr))?))
                .await
                .unwrap()
        };
        let state = || async { read_state(&ctx.database).await.unwrap() };
        let first = ready("1. open the page", "1. watch the logs");
        assert_eq!(run(ctx, &first).await.unwrap(), 0);
        assert_eq!(leave(7).await.len(), 2);

        // The same staging steps are taken up for the next PR; the other
        // production steps are its own check, beside the earlier one.
        let second = ready("1. open the page", "1. watch the other logs");
        assert_eq!(run(ctx, &second).await.unwrap(), 0);
        let stored = state().await;
        let checks = &stored["slugs"]["one"]["work"]["checks"];
        assert_eq!(checks["staging"]["steps"], "1. open the page");
        assert_eq!(checks["staging"]["forPr"], Value::Null);
        assert_eq!(checks["production"]["steps"], "1. watch the other logs");
        assert_eq!(checks["production"]["forPr"], Value::Null);
        let watches = stored["watches"].as_array().unwrap();
        assert_eq!(watches.len(), 1, "{watches:?}");
        assert_eq!(watches[0]["when"][0]["cond"], "#7:production");
        assert_eq!(watches[0]["owedOnDelivery"], true);
        assert!(
            watches[0]["then"]["wake"]
                .as_str()
                .unwrap()
                .starts_with("#7 production check: 1. watch the logs\n")
        );

        // An amendment keeps the earlier check the same way.
        assert_eq!(leave(9).await.len(), 2);
        let amend = StatusArgs {
            verify_in_staging: Some("2. open the next page".into()),
            ..Default::default()
        };
        assert_eq!(run(ctx, &amend).await.unwrap(), 0);
        let stored = state().await;
        let checks = &stored["slugs"]["one"]["work"]["checks"];
        assert_eq!(checks["staging"]["steps"], "2. open the next page");
        assert_eq!(checks["production"]["forPr"], 9);
        let watches = stored["watches"].as_array().unwrap();
        assert_eq!(watches.len(), 2, "{watches:?}");
        assert_eq!(watches[1]["when"][0]["cond"], "#9:staging");
        fixture.close().await.unwrap();
    }

    /// The note is judged by what this command supplies. Text carried from
    /// an earlier status is never refused: a stale "OPS: none" is dropped.
    #[tokio::test]
    async fn ops_none_is_refused_while_something_is_owed_and_a_carried_one_is_dropped() {
        use wt_app_core::test_support::CommandFixture;
        let fixture = CommandFixture::new().await.unwrap();
        let ctx = &fixture.ctx;
        let ready = |note: &str, staging: Option<&str>| StatusArgs {
            positionals: vec!["ready".into()],
            risk: Some("low".into()),
            note: Some(note.into()),
            verify_in_staging: staging.map(str::to_owned),
            ..Default::default()
        };
        let work =
            || async { read_state(&ctx.database).await.unwrap()["slugs"]["one"]["work"].clone() };
        // Nothing owed: the claim stands.
        assert_eq!(
            run(ctx, &ready("ships the fix. OPS: none", None))
                .await
                .unwrap(),
            0
        );
        assert_eq!(work().await["note"], "ships the fix. OPS: none");
        // Claiming it while recording a check is refused, and nothing is
        // written.
        assert_eq!(
            run(ctx, &ready("again. OPS: none", Some("1. check")))
                .await
                .unwrap(),
            2
        );
        let stored = work().await;
        assert_eq!(stored["note"], "ships the fix. OPS: none");
        assert_eq!(stored["checks"], json!({}));
        // An amendment that makes the row owe something carries the note,
        // and the claim that stopped being true goes.
        let amend = StatusArgs {
            verify_in_staging: Some("1. check".into()),
            ..Default::default()
        };
        assert_eq!(run(ctx, &amend).await.unwrap(), 0);
        let stored = work().await;
        assert_eq!(stored["note"], "ships the fix.");
        assert_eq!(stored["checks"]["staging"]["steps"], "1. check");
        assert_eq!(stored["checks"]["staging"]["text"], Value::Null);
        // Line breaks are kept beside the one line that names the check.
        let lined = StatusArgs {
            verify_in_staging: Some("Proves it\nwhy\nSTEPS: 1. check".into()),
            ..Default::default()
        };
        assert_eq!(run(ctx, &lined).await.unwrap(), 0);
        let stored = work().await;
        assert_eq!(
            stored["checks"]["staging"]["steps"],
            "Proves it why STEPS: 1. check"
        );
        assert_eq!(
            stored["checks"]["staging"]["text"],
            "Proves it\nwhy\nSTEPS: 1. check"
        );
        assert_eq!(stored["verifyAfterMerge"], "Proves it why STEPS: 1. check");
        assert_eq!(run(ctx, &amend).await.unwrap(), 0);
        // Appending to it is judged by the appended text alone.
        let append = |text: &str| StatusArgs {
            note_only: Some(text.into()),
            append: true,
            ..Default::default()
        };
        assert_eq!(run(ctx, &append("REVERT: safe")).await.unwrap(), 0);
        assert_eq!(work().await["note"], "ships the fix. REVERT: safe");
        assert_eq!(run(ctx, &append("OPS: none")).await.unwrap(), 2);
        assert_eq!(work().await["note"], "ships the fix. REVERT: safe");

        // A wake is owed too: setting one drops the claim from the note.
        let drop = StatusArgs {
            no_verify_in_staging: true,
            ..Default::default()
        };
        assert_eq!(run(ctx, &drop).await.unwrap(), 0);
        assert_eq!(
            run(ctx, &ready("ships the fix. OPS: none", None))
                .await
                .unwrap(),
            0
        );
        let wake = super::super::watch::WakeArgs {
            conditions: vec!["3h".into()],
            message: Some("rerun the renewal check".into()),
            slug: None,
            before_release: true,
        };
        assert_eq!(super::super::watch::run_wake(ctx, &wake).await.unwrap(), 0);
        assert_eq!(work().await["note"], "ships the fix.");
        let watches = ctx
            .database
            .call(|store| Ok(store.list_watches(Some("one"))?))
            .await
            .unwrap();
        assert!(watches[0].before_release());
        // And with the wake pending, the claim is refused again.
        assert_eq!(
            run(ctx, &ready("ships the fix. OPS: none", None))
                .await
                .unwrap(),
            2
        );

        // A command that is refused drops nothing: the check it also asked
        // to drop is still owed, for an assertion and for an amendment.
        let owe = StatusArgs {
            verify_in_staging: Some("1. check".into()),
            verify_in_production: Some("1. prod".into()),
            ..Default::default()
        };
        assert_eq!(run(ctx, &owe).await.unwrap(), 0);
        let mut refused = ready("ships the fix. OPS: none", None);
        refused.no_verify_in_staging = true;
        assert_eq!(run(ctx, &refused).await.unwrap(), 2);
        assert_eq!(work().await["checks"]["staging"]["steps"], "1. check");
        let refused = StatusArgs {
            note_only: Some("OPS: none".into()),
            no_verify_in_staging: true,
            ..Default::default()
        };
        assert_eq!(run(ctx, &refused).await.unwrap(), 2);
        assert_eq!(work().await["checks"]["staging"]["steps"], "1. check");
        // One that goes through drops it in the same write, and says so in
        // the history.
        let mut accepted = ready("ships the fix.", None);
        accepted.no_verify_in_staging = true;
        assert_eq!(run(ctx, &accepted).await.unwrap(), 0);
        let stored = work().await;
        assert!(stored["checks"].get("staging").is_none(), "{stored}");
        assert_eq!(stored["checks"]["production"]["steps"], "1. prod");
        let dropped = ctx
            .database
            .call(|store| Ok(store.read_worktree_events("one", 50)?))
            .await
            .unwrap()
            .iter()
            .filter_map(|event| wt_core::owed_sentence(&event.event))
            .filter(|line| line == "dropped: staging check: 1. check")
            .count();
        assert!(dropped >= 1);
        fixture.close().await.unwrap();
    }

    #[test]
    fn only_an_assertion_that_ends_with_the_list_reads_it() {
        for state in [
            WorkState::Todo,
            WorkState::Working,
            WorkState::Review,
            WorkState::NeedsTesting,
            WorkState::NeedsHuman,
        ] {
            assert!(!reads_list(state, false), "{state:?}");
            assert!(reads_list(state, true), "{state:?}");
        }
        for state in [WorkState::Ready, WorkState::Verified, WorkState::Dropped] {
            assert!(reads_list(state, false), "{state:?}");
        }
    }

    #[tokio::test]
    async fn bare_status_outside_every_worktree_lists_nothing() {
        use wt_app_core::test_support::CommandFixture;
        let fixture = CommandFixture::new().await.unwrap();
        let mut ctx = fixture.ctx.clone();
        // A cancelled listing fails, as the one inside a worktree shows.
        ctx.cancellation = ctx.cancellation.child_token();
        ctx.cancellation.cancel();
        assert!(run(&ctx, &StatusArgs::default()).await.is_err());
        // Outside the worktree root the words need no listing at all.
        ctx.cwd = fixture._root.path().to_path_buf();
        assert_eq!(run(&ctx, &StatusArgs::default()).await.unwrap(), 0);
        // Naming a worktree from there still needs one.
        let named = StatusArgs {
            positionals: vec!["one".into()],
            ..Default::default()
        };
        assert!(run(&ctx, &named).await.is_err());
        fixture.close().await.unwrap();
    }
}
