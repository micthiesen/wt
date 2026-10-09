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
    WorkRisk, WorkState, WorkStatusRecord, parse_work_status, resolve_work_state,
    sanitize_work_note, work_age,
};
use wt_platform::process::CommandSpec;
use wt_store::WorkStatusRecord as StoredWorkStatusRecord;

use crate::{commands::resolve::resolve_worktree, context::AppContext, database::Database};

const VOCAB: &str = "states: todo, working, review, needs-testing, needs-human, ready, verified, dropped\nUse unique prefixes; `nh` and `nt` are aliases. Ready requires --risk. --blocked-on gates ready/todo; --verify-after-merge records checks owed after deployment.";

#[derive(Debug, Clone, Args, Default)]
pub struct StatusArgs {
    #[arg(value_name = "TARGET_OR_STATE", num_args = 0..=2)]
    pub positionals: Vec<String>,
    #[arg(short = 'm', long = "note", allow_hyphen_values = true)]
    pub note: Option<String>,
    #[arg(long, allow_hyphen_values = true)]
    pub note_only: Option<String>,
    #[arg(long, value_name = "RISK")]
    pub risk: Option<String>,
    #[arg(long, allow_hyphen_values = true)]
    pub blocked_on: Option<String>,
    #[arg(long, allow_hyphen_values = true)]
    pub verify_after_merge: Option<String>,
    #[arg(long, allow_hyphen_values = true)]
    pub examined: Option<String>,
    #[arg(long)]
    pub unblock: bool,
    #[arg(long)]
    pub append: bool,
    #[arg(long)]
    pub clear: bool,
    #[arg(long)]
    pub all: bool,
    #[arg(long)]
    pub json: bool,
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
        verify: Option<String>,
        append: bool,
    },
    Amend {
        target: Option<String>,
        note: Option<String>,
        risk: Option<WorkRisk>,
        blocked: Option<Option<String>>,
        verify: Option<String>,
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
    let verify = match args.verify_after_merge.as_deref() {
        Some(value) => match sanitize(value) {
            Some(value) => Some(value),
            None => return fail("--verify-after-merge requires non-empty steps"),
        },
        None => None,
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
        return fail("--verify-after-merge doesn't apply to verified or dropped");
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
                entries.push(json!({"slug":slug,"branch":w.target.branch,"kind":"live","section":if ctx.config.instance.role==InstanceRole::Worker {None}else{record.and_then(|r|r.get("section")).and_then(Value::as_str)},"state":work.as_ref().map(|x|x.state),"note":work.as_ref().and_then(|x|x.note.as_deref()),"risk":work.as_ref().and_then(|x|x.risk),"blocked_on":work.as_ref().and_then(|x|x.blocked_on.as_deref()),"verify_after_merge":work.as_ref().and_then(|x|x.verify_after_merge.as_deref()),"at":work.as_ref().map(|x|x.at.as_str()),"examined":examined,"examined_current":examined_current,"by":work.as_ref().and_then(|x|x.by.as_deref()),"stale":work.as_ref().is_some_and(|x|x.sha.as_deref().zip(head.as_deref()).is_some_and(|(old,new)|old!=new))}));
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
                Err(e) => {
                    eprintln!("{e}");
                    return Ok(1);
                }
            };
            let slug = record.target.slug().to_owned();
            let previous = state
                .get("slugs")
                .and_then(|s| s.get(&slug))
                .and_then(|v| v.get("work"))
                .and_then(parse_work_status);
            let previous_note = previous.as_ref().and_then(|previous| previous.note.clone());
            let mut next = WorkStatusRecord::new(work_state, now_iso());
            next.sha = record.head_sha.clone();
            next.by = std::env::var("WT_AGENT")
                .ok()
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty());
            next.note = merge_note(
                previous.as_ref().and_then(|p| p.note.as_deref()),
                note.as_deref(),
                append,
                false,
            );
            next.risk = risk;
            next.blocked_on = blocked;
            next.verify_after_merge =
                if matches!(work_state, WorkState::Verified | WorkState::Dropped) {
                    None
                } else {
                    verify.or_else(|| previous.and_then(|p| p.verify_after_merge))
                };
            let stored: StoredWorkStatusRecord =
                serde_json::from_value(serde_json::to_value(&next)?)
                    .context("encode status record")?;
            let db_slug = slug.clone();
            let wrote = ctx
                .database
                .call(move |s| Ok(s.set_slug_work_status(&db_slug, Some(&stored))?))
                .await?;
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
            print_guidance(
                work_state,
                next.blocked_on.as_deref(),
                next.verify_after_merge.as_deref(),
            );
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
            let Some(mut next) = state
                .get("slugs")
                .and_then(|s| s.get(&slug))
                .and_then(|v| v.get("work"))
                .and_then(parse_work_status)
            else {
                eprintln!("{slug} has no status asserted; set a state first");
                return Ok(2);
            };
            let previous_note = next.note.clone();
            if risk.is_some() && next.state != WorkState::Ready {
                eprintln!("--risk only applies to ready");
                return Ok(2);
            }
            if let Some(Some(_)) = blocked
                && !matches!(next.state, WorkState::Ready | WorkState::Todo)
            {
                eprintln!("--blocked-on applies to ready and todo");
                return Ok(2);
            }
            if blocked == Some(None) && next.blocked_on.is_none() {
                eprintln!("{slug} has no gate to clear");
                return Ok(2);
            }
            if verify.is_some() && matches!(next.state, WorkState::Verified | WorkState::Dropped) {
                eprintln!(
                    "--verify-after-merge does not apply to {}",
                    work_state_name(next.state)
                );
                return Ok(2);
            }
            let old_gate = next.blocked_on.clone();
            let old_verify = next.verify_after_merge.clone();
            next.note = merge_note(next.note.as_deref(), note.as_deref(), append, true);
            if let Some(risk) = risk {
                next.risk = Some(risk);
            }
            if let Some(blocked) = blocked {
                next.blocked_on = blocked;
            }
            if let Some(verify) = verify {
                next.verify_after_merge = Some(verify);
            }
            if next.risk.is_some_and(|r| r != WorkRisk::Low) && next.note.is_none() {
                eprintln!("--risk medium|high requires -m explaining uncertainty");
                return Ok(2);
            }
            let stored: StoredWorkStatusRecord =
                serde_json::from_value(serde_json::to_value(&next)?)?;
            let db_slug = slug.clone();
            let wrote = ctx
                .database
                .call(move |s| Ok(s.set_slug_work_status(&db_slug, Some(&stored))?))
                .await?;
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
            if let Some(verify) = &next.verify_after_merge {
                println!("  verify after merge: {verify}");
            }
            if let Some(note) = &next.note {
                println!("  note: {note}");
            }
            report_replaced_note(previous_note.as_deref(), next.note.as_deref(), append);
            note_budget_hint(next.state, next.note.as_deref());
            if old_gate != next.blocked_on || old_verify != next.verify_after_merge {
                print_guidance(
                    next.state,
                    next.blocked_on.as_deref(),
                    next.verify_after_merge.as_deref(),
                );
            }
            Ok(0)
        }
        Action::Error(_) => unreachable!(),
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

fn print_guidance(state: WorkState, blocked_on: Option<&str>, verify: Option<&str>) {
    if hints_off() {
        return;
    }
    if let Some(steps) = verify {
        println!(
            "after this merges, the row becomes needs-testing and stays out of `wt clean` until verified."
        );
        println!("owed: {steps}");
        println!(
            "first confirm the deploy carrying this change landed; then use `wt status verified -m <what you checked and where>`."
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
    if let Some(verify) = &record.verify_after_merge {
        out.push_str(&format!("\n  verify after merge: {verify}"));
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
    let verification_owed = entry.work.as_ref().is_some_and(|work| {
        work.verify_after_merge.is_some() && work.state != "verified" && work.state != "dropped"
    });
    let merged = entry.extra.get("prState").and_then(Value::as_str) == Some("MERGED")
        || entry.extra.get("gitState").and_then(Value::as_str) == Some("merged");
    json!({"slug":entry.slug,"branch":entry.branch,"kind":if merged {"merged"} else {"removed"},"pr":entry.extra.get("prNumber").cloned().unwrap_or(Value::Null),"pr_url":entry.extra.get("prUrl").cloned().unwrap_or(Value::Null),"title":entry.extra.get("title").cloned().unwrap_or(Value::Null),"archived_at":entry.removed_at,"work_state":work_state,"verify_after_merge":entry.work.and_then(|work|work.verify_after_merge),"verification_owed":verification_owed})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn base_lookup_distinguishes_missing_ref_from_cancellation() {
        let fixture = crate::commands::test_support::CommandFixture::new()
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

    #[test]
    fn status_record_serialization_matches_store_shape() {
        let record = WorkStatusRecord::new(WorkState::Ready, "2026-10-09T00:00:00Z");
        let value = serde_json::to_value(record).unwrap();
        assert_eq!(value["state"], "ready");
        assert_eq!(value["at"], "2026-10-09T00:00:00Z");
    }

    #[tokio::test]
    async fn status_writes_gate_then_unblocks_without_changing_assertion_time() {
        use crate::commands::test_support::CommandFixture;
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
}
