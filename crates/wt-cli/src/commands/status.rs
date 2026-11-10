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

use wt_app_core::{context::AppContext, database::Database};
use wt_ops::commands::resolve::resolve_worktree;

const VOCAB: &str = "states: todo, working, review, needs-testing, needs-human, ready, verified, dropped\nUse unique prefixes; `nh` and `nt` are aliases. Ready requires --risk. --blocked-on gates ready/todo; --verify-in-staging / --verify-in-production record a check owed once the change deploys there.";

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
    /// state, amends in place.
    #[arg(
        long,
        allow_hyphen_values = true,
        value_name = "STEPS",
        conflicts_with = "verify_in_production"
    )]
    pub verify_in_staging: Option<String>,
    /// Numbered steps for a check only production can prove, once the landing
    /// reaches `[branch] production` (the base landing when there is none).
    /// Without a state, amends in place.
    #[arg(long, allow_hyphen_values = true, value_name = "STEPS")]
    pub verify_in_production: Option<String>,
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
    /// Add to the existing note instead of replacing it (with `-m` or `--note-only`).
    #[arg(long)]
    pub append: bool,
    /// Drop the status record.
    #[arg(long)]
    pub clear: bool,
    /// Print the fleet overview of every worktree's status.
    #[arg(long)]
    pub all: bool,
    /// Print machine-readable JSON (with `--all` or a show).
    #[arg(long)]
    pub json: bool,
}

/// An owed post-merge check and where it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Verify {
    stage: VerifyStage,
    steps: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Action {
    All {
        json: bool,
    },
    Show(Option<String>),
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
        verify: Option<Verify>,
        append: bool,
    },
    Amend {
        target: Option<String>,
        note: Option<String>,
        risk: Option<WorkRisk>,
        blocked: Option<Option<String>>,
        verify: Option<Verify>,
        append: bool,
    },
    Error(String),
}

fn parse_status_args(args: &StatusArgs) -> Action {
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
    let verify = match (
        args.verify_in_staging.as_deref(),
        args.verify_in_production.as_deref(),
    ) {
        (Some(_), Some(_)) => {
            return fail("pick one of --verify-in-staging and --verify-in-production");
        }
        (Some(value), None) | (None, Some(value)) => {
            let (stage, flag) = if args.verify_in_staging.is_some() {
                (VerifyStage::Staging, "--verify-in-staging")
            } else {
                (VerifyStage::Production, "--verify-in-production")
            };
            match sanitize(value) {
                Some(steps) => Some(Verify { stage, steps }),
                None => return Action::Error(format!("{flag} requires non-empty steps")),
            }
        }
        (None, None) => None,
    };
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
            || verify.is_some()
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
            verify: None,
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
            || verify.is_some()
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
            || verify.is_some()
            || args.unblock
            || !positions.is_empty()
            || args.note_only.is_some()
        {
            return fail("--all only combines with --json");
        }
        return Action::All { json: args.json };
    }
    if args.json {
        return fail("--json requires --all");
    }
    if args.clear {
        if note.is_some()
            || risk.is_some()
            || gate.is_some()
            || verify.is_some()
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
        [one] if is_ambiguous_state(one) => return fail("ambiguous status prefix"),
        [one] => (Some(one.clone()), None),
        [target, state] => (Some(target.clone()), Some(state.as_str())),
        _ => return fail("too many arguments"),
    };
    let Some(raw_state) = state else {
        if gate.is_some() || args.unblock || verify.is_some() || risk.is_some() {
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
                append,
            };
        }
        if note.is_some() {
            return fail("-m needs a state; to amend a note use --note-only");
        }
        return Action::Show(target);
    };
    let Some(state) = resolve_work_state(raw_state) else {
        return fail("unknown or ambiguous status state");
    };
    if risk.is_some() && state != WorkState::Ready {
        return fail("--risk only applies to ready");
    }
    if args.unblock {
        return fail("a new assertion clears any old gate; drop --unblock");
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
    if verify.is_some() && matches!(state, WorkState::Verified | WorkState::Dropped) {
        return fail("--verify-in-* doesn't apply to verified or dropped");
    }
    Action::Set {
        target,
        state,
        note,
        risk,
        blocked: gate,
        verify,
        append,
    }
}

pub async fn run(ctx: &AppContext, args: &StatusArgs) -> Result<i32> {
    let action = parse_status_args(args);
    if let Action::Error(message) = action {
        eprintln!("{message}\n{VOCAB}");
        return Ok(2);
    }
    let records = ctx.repository.inventory(&ctx.cancellation).await?;
    let worktrees = records.iter().filter(|w| !w.is_main).collect::<Vec<_>>();
    let state = read_state(&ctx.database).await?;
    match action {
        Action::All { json: wants_json } => {
            let now = now_ms();
            let mut entries = Vec::with_capacity(worktrees.len());
            for w in &worktrees {
                let slug = w.target.slug();
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
                entries.push(json!({"slug":slug,"branch":w.target.branch,"kind":"live","section":if ctx.config.instance.role==InstanceRole::Worker {None}else{record.and_then(|r|r.get("section")).and_then(Value::as_str)},"state":work.as_ref().map(|x|x.state),"note":work.as_ref().and_then(|x|x.note.as_deref()),"risk":work.as_ref().and_then(|x|x.risk),"blocked_on":work.as_ref().and_then(|x|x.blocked_on.as_deref()),"verify_after_merge":work.as_ref().and_then(|x|x.verify_after_merge.as_deref()),"verify_in":work.as_ref().and_then(|x|x.verification()).map(|(stage,_)|stage),"at":work.as_ref().map(|x|x.at.as_str()),"examined":examined,"examined_current":examined_current,"by":work.as_ref().and_then(|x|x.by.as_deref()),"stale":work.as_ref().is_some_and(|x|x.sha.as_deref().zip(head.as_deref()).is_some_and(|(old,new)|old!=new))}));
            }
            if wants_json {
                let live = worktrees
                    .iter()
                    .map(|w| w.target.slug().to_owned())
                    .collect::<BTreeSet<_>>();
                let removed = ctx
                    .database
                    .call(move |store| Ok(store.recently_removed_worktrees(&live, now)?))
                    .await?;
                let mut output = entries;
                output.extend(removed.into_iter().map(removed_json));
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
        Action::Show(target) => {
            let record = match resolve_worktree(ctx, target.as_deref()).await {
                Ok(record) if !record.is_main => record,
                Ok(_) => {
                    println!("{VOCAB}");
                    return Ok(0);
                }
                Err(error) if target.is_none() && error.to_string().starts_with("not inside") => {
                    println!("{VOCAB}");
                    return Ok(0);
                }
                Err(error) => {
                    eprintln!("{error}\n{VOCAB}");
                    return Ok(1);
                }
            };
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
            let record = match resolve_worktree(ctx, target.as_deref()).await {
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
            ctx.database
                .call(move |s| Ok(s.set_slug_work_status(&db_slug, None)?))
                .await?;
            println!("✓ {slug} status cleared");
            Ok(0)
        }
        Action::Examine { target, verdict } => {
            let record = match resolve_worktree(ctx, target.as_deref()).await {
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
            append,
        } => {
            let record = match resolve_worktree(ctx, target.as_deref()).await {
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
            let mut next = WorkStatusRecord::new(work_state, now_iso());
            next.sha = record.head_sha.clone();
            next.by = std::env::var("WT_AGENT")
                .ok()
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty());
            next.risk = risk;
            next.blocked_on = blocked;
            let StatusWrite {
                previous,
                next,
                wrote,
            } = update_status(&ctx.database, &slug, move |previous| {
                next.note = merge_note(
                    previous.as_ref().and_then(|p| p.note.as_deref()),
                    note.as_deref(),
                    append,
                    false,
                );
                if !matches!(work_state, WorkState::Verified | WorkState::Dropped) {
                    match verify {
                        Some(verify) => {
                            next.verify_after_merge = Some(verify.steps);
                            next.verify_in = Some(verify.stage);
                        }
                        None => {
                            if let Some(previous) = previous {
                                next.verify_after_merge = previous.verify_after_merge;
                                next.verify_in = previous.verify_in;
                            }
                        }
                    }
                }
                Ok(next)
            })
            .await?
            .map_err(anyhow::Error::msg)?;
            let previous_note = previous.and_then(|previous| previous.note);
            println!(
                "✓ {} {}{}",
                record.target.slug(),
                work_state_name(work_state),
                if wrote { "" } else { " (already asserted)" }
            );
            report_replaced_note(previous_note.as_deref(), next.note.as_deref(), append);
            if let Some(note) = &next.note {
                println!("  note: {note}");
            }
            note_budget_hint(work_state, next.note.as_deref());
            print_guidance(work_state, next.blocked_on.as_deref(), next.verification());
            Ok(0)
        }
        Action::Amend {
            target,
            note,
            risk,
            blocked,
            verify,
            append,
        } => {
            let record = match resolve_worktree(ctx, target.as_deref()).await {
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
            let write = update_status(&ctx.database, &slug, {
                let slug = slug.clone();
                move |previous| amend_record(&slug, previous, note, risk, blocked, verify, append)
            })
            .await?;
            let StatusWrite {
                previous,
                next,
                wrote,
            } = match write {
                Ok(write) => write,
                Err(refusal) => {
                    eprintln!("{refusal}");
                    return Ok(2);
                }
            };
            let previous = previous.expect("an amend is refused without a previous status");
            let previous_note = previous.note;
            let old_gate = previous.blocked_on;
            let old_verify = (previous.verify_after_merge, previous.verify_in);
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
            if let Some((stage, steps)) = next.verification() {
                println!("  verify in {}: {steps}", stage.as_str());
            }
            if let Some(note) = &next.note {
                println!("  note: {note}");
            }
            report_replaced_note(previous_note.as_deref(), next.note.as_deref(), append);
            note_budget_hint(next.state, next.note.as_deref());
            if old_gate != next.blocked_on
                || old_verify != (next.verify_after_merge.clone(), next.verify_in)
            {
                print_guidance(next.state, next.blocked_on.as_deref(), next.verification());
            }
            Ok(0)
        }
        Action::Error(_) => unreachable!(),
    }
}

struct StatusWrite {
    previous: Option<WorkStatusRecord>,
    next: WorkStatusRecord,
    wrote: bool,
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
    database
        .call(move |store| {
            let mut built = None;
            let write = store.update_slug_work_status(&slug, |stored| {
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
    verify: Option<Verify>,
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
    if verify.is_some() && matches!(next.state, WorkState::Verified | WorkState::Dropped) {
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
    if let Some(verify) = verify {
        next.verify_after_merge = Some(verify.steps);
        next.verify_in = Some(verify.stage);
    }
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

fn report_replaced_note(previous: Option<&str>, current: Option<&str>, append: bool) {
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
            "note is long; a concise ready note is easier to review. Keep one line of outcome, then OPS / REVERT / IF WRONG / UNTESTED."
        );
    }
}

fn hints_off() -> bool {
    std::env::var("WT_NO_HINTS").is_ok_and(|value| value == "1")
}

fn print_guidance(state: WorkState, blocked_on: Option<&str>, verify: Option<(VerifyStage, &str)>) {
    if hints_off() {
        return;
    }
    if let Some((stage, steps)) = verify {
        let due = match stage {
            VerifyStage::Staging => "once this lands on the base branch",
            VerifyStage::Production => "once this reaches production",
        };
        println!(
            "{due}, the row becomes needs-testing; it stays out of `wt clean` from the merge until verified."
        );
        println!("owed in {}: {steps}", stage.as_str());
        println!(
            "first confirm the {} deploy carrying this change finished; then use `wt status verified -m <what you checked and where>`.",
            stage.as_str()
        );
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
        WorkState::Verified => "deployed verification is complete; nothing is owed on this branch",
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
    if let Some((stage, steps)) = record.verification() {
        out.push_str(&format!("\n  verify in {}: {steps}", stage.as_str()));
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

fn removed_json(entry: wt_store::RemovedWorktree) -> Value {
    let work_state = entry.work.as_ref().map(|work| work.state.as_str());
    let verification_owed = wt_store::verification_owed_at_removal(&entry);
    let merged = wt_store::is_merged_removal(&entry);
    json!({"slug":entry.slug,"branch":entry.branch,"kind":if merged {"merged"} else {"removed"},"pr":entry.extra.get("prNumber").cloned().unwrap_or(Value::Null),"pr_url":entry.extra.get("prUrl").cloned().unwrap_or(Value::Null),"title":entry.extra.get("title").cloned().unwrap_or(Value::Null),"archived_at":entry.removed_at,"work_state":work_state,"verify_in":entry.work.as_ref().and_then(|work|work.verify_after_merge.as_ref().map(|_|work.verify_in.clone().unwrap_or_else(||"staging".into()))),"verify_after_merge":entry.work.and_then(|work|work.verify_after_merge),"verification_owed":verification_owed})
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
        // the worktree's agent has asked for a human since.
        let claim = StatusArgs {
            positionals: vec!["needs-human".into()],
            note: Some("approve the migration".into()),
            verify_in_production: Some("STEPS: 1. check prod".into()),
            ..Default::default()
        };
        assert_eq!(run(&fixture.ctx, &claim).await.unwrap(), 0);
        let refused = update_status(&fixture.ctx.database, "one", |previous| {
            amend_record("one", previous, None, None, Some(None), None, false)
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
}
