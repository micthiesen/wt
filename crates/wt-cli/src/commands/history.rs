//! `wt history`: one worktree's recorded timeline.

use anyhow::Result;
use clap::Args;
use wt_core::{
    AutomationOutcomeKind, ChecksChange, HistoryLine, LandedOn, PrChange, QueueChange,
    ReviewChange, WatchChange, WorktreeEvent, WorktreeEventKind,
};

use wt_app_core::context::AppContext;
use wt_ops::commands::resolve::resolve_worktree;

#[derive(Debug, Clone, Args, Default)]
pub struct HistoryArgs {
    /// Worktree slug or branch; defaults to the worktree of the current
    /// directory, then WT_AGENT. A removed worktree is named by its slug.
    #[arg(value_name = "SLUG")]
    pub slug: Option<String>,
    /// Print the events as a JSON array of {at_ms, event}, oldest first.
    #[arg(long)]
    pub json: bool,
    /// Show only the newest N events (at most 500).
    #[arg(long, value_name = "N", default_value_t = 100, value_parser = clap::value_parser!(u16).range(1..=500))]
    pub limit: u16,
}

pub async fn run(ctx: &AppContext, args: &HistoryArgs) -> Result<i32> {
    let slug = match target(ctx, args.slug.as_deref()).await {
        Ok(slug) => slug,
        Err(error) => {
            eprintln!("{error}");
            return Ok(1);
        }
    };
    let limit = usize::from(args.limit);
    let read = slug.clone();
    let mut events = ctx
        .database
        .call(move |store| Ok(store.read_worktree_events(&read, limit)?))
        .await?;
    for event in &mut events {
        wt_app_core::worktree_history::sanitize(&mut event.event);
    }
    if args.json {
        println!("{}", serde_json::to_string_pretty(&events)?);
        return Ok(0);
    }
    let lines = text(&events);
    if lines.is_empty() {
        println!("no history recorded for {slug}");
    }
    for line in lines {
        println!("{line}");
    }
    Ok(0)
}

/// The slug whose history is asked for. A name that matches no live worktree
/// is taken as the slug of a removed one, whose last history is still kept.
async fn target(ctx: &AppContext, requested: Option<&str>) -> Result<String> {
    if let Some(requested) = requested {
        return Ok(match resolve_worktree(ctx, Some(requested)).await {
            Ok(record) => record.target.slug().to_owned(),
            Err(_) => requested.to_owned(),
        });
    }
    match resolve_worktree(ctx, None).await {
        Ok(record) if !record.is_main => Ok(record.target.slug().to_owned()),
        _ => std::env::var("WT_AGENT")
            .ok()
            .map(|agent| agent.trim().to_owned())
            .filter(|agent| !agent.is_empty() && agent != "manager")
            .ok_or_else(|| anyhow::anyhow!("not inside a worktree; pass a worktree slug")),
    }
}

/// Newest first, one line per event with the local time it was recorded;
/// what wt did about a met watch is one line.
fn text(events: &[WorktreeEvent]) -> Vec<String> {
    wt_core::fold_watch_history(events)
        .iter()
        .rev()
        .filter_map(|line| {
            let (at_ms, said) = match line {
                HistoryLine::Event(event) => (event.at_ms, describe(&event.event)?),
                HistoryLine::Fired(fired) => {
                    let did =
                        wt_core::watch_fired_words(&fired.done, fired.unconfirmed, fired.what)
                            .sentence();
                    let said = if fired.just_set {
                        format!("{did} (as soon as it was set)")
                    } else {
                        did
                    };
                    (fired.at_ms, said)
                }
                HistoryLine::Folded(_) => return None,
            };
            Some(format!("{}  {said}", stamp(at_ms)))
        })
        .collect()
}

fn stamp(at_ms: u64) -> String {
    match wt_app_core::display_time::local_time_components(at_ms) {
        Some((year, month, day, hour, minute, _)) => {
            format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}")
        }
        None => format!("{at_ms}ms"),
    }
}

/// One event in plain words. An event from a newer build has none.
fn describe(event: &WorktreeEventKind) -> Option<String> {
    let by = |who: &Option<String>| {
        who.as_ref()
            .map(|who| format!(" by {who}"))
            .unwrap_or_default()
    };
    // Bracketed where free text comes before it.
    let by_agent = |who: &Option<String>| {
        who.as_ref()
            .map(|who| format!(" (by {who})"))
            .unwrap_or_default()
    };
    let state = |state: &wt_core::WorkState| {
        serde_json::to_value(state)
            .ok()
            .and_then(|value| value.as_str().map(str::to_owned))
            .unwrap_or_default()
    };
    Some(match event {
        WorktreeEventKind::Created { base } => match base {
            Some(base) => format!("created from {base}"),
            None => "created".into(),
        },
        WorktreeEventKind::Status {
            from,
            to,
            by: who,
            risk,
            note,
            blocked_on,
            verify_in,
        } => {
            let mut line = match from {
                Some(from) if from != to => format!("status {} -> {}", state(from), state(to)),
                _ => format!("status {}", state(to)),
            };
            if let Some(risk) = risk {
                let risk = serde_json::to_value(risk).ok()?;
                line.push_str(&format!(", risk {}", risk.as_str().unwrap_or_default()));
            }
            if let Some(gate) = blocked_on {
                line.push_str(&format!(", blocked on {gate}"));
            }
            if let Some(stage) = verify_in {
                line.push_str(&format!(", check owed in {stage}"));
            }
            line.push_str(&by(who));
            if let Some(note) = note {
                line.push_str(&format!(": {note}"));
            }
            line
        }
        WorktreeEventKind::StatusCleared { from, by: who } => match from {
            Some(from) => format!("status cleared (was {}){}", state(from), by(who)),
            None => format!("status cleared{}", by(who)),
        },
        WorktreeEventKind::Pr { number, change } => format!(
            "PR #{number} {}",
            match change {
                PrChange::Opened => "opened",
                PrChange::OpenedDraft => "opened as a draft",
                PrChange::ReadyForReview => "marked ready for review",
                PrChange::BackToDraft => "moved back to draft",
                PrChange::Closed => "closed",
                PrChange::Reopened => "reopened",
                PrChange::Merged => "merged",
            }
        ),
        WorktreeEventKind::Checks { state, failed } => match state {
            ChecksChange::Passing => "checks passing".into(),
            ChecksChange::Failing if failed.is_empty() => "checks failing".into(),
            ChecksChange::Failing => format!("checks failing: {}", failed.join(", ")),
        },
        WorktreeEventKind::Review { reviewer, change } => {
            let who = reviewer.as_deref().unwrap_or("review");
            match change {
                ReviewChange::Findings { count: 1 } => format!("{who}: 1 unresolved finding"),
                ReviewChange::Findings { count } => format!("{who}: {count} unresolved findings"),
                ReviewChange::Clean => format!("{who}: nothing unresolved"),
                ReviewChange::ChangesRequested => format!("{who}: changes requested"),
                ReviewChange::Approved => format!("{who}: approved"),
            }
        }
        WorktreeEventKind::MergeQueue { change } => match change {
            QueueChange::Queued => "entered the merge queue".into(),
            QueueChange::Left => "left the merge queue without merging".into(),
        },
        WorktreeEventKind::Conflict { resolved: false } => "conflicts with its base".into(),
        WorktreeEventKind::Conflict { resolved: true } => {
            "no longer conflicts with its base".into()
        }
        WorktreeEventKind::Landed { on } => match on {
            LandedOn::Base => "landed on base".into(),
            LandedOn::Production => "landed on production".into(),
        },
        WorktreeEventKind::Automation {
            rule,
            outcome,
            reason,
        } => {
            let outcome = match outcome {
                AutomationOutcomeKind::Delivered => "ran",
                AutomationOutcomeKind::Skipped => "skipped",
                AutomationOutcomeKind::Failed => "failed",
                AutomationOutcomeKind::Unconfirmed => "sent, not confirmed",
            };
            match reason {
                Some(reason) => format!("automation {rule} {outcome}: {reason}"),
                None => format!("automation {rule} {outcome}"),
            }
        }
        WorktreeEventKind::Message {
            from,
            text,
            delivered,
        } => {
            let from = from.as_deref().unwrap_or("you");
            let delivery = match delivered {
                Some(true) => "",
                Some(false) => " (not delivered)",
                None => " (not confirmed)",
            };
            format!("message from {from}{delivery}: {text}")
        }
        WorktreeEventKind::Restack { ok, onto } => {
            let onto = onto
                .as_ref()
                .map(|onto| format!(" onto {onto}"))
                .unwrap_or_default();
            if *ok {
                format!("restacked{onto}")
            } else {
                format!("restack{onto} stopped on a conflict")
            }
        }
        WorktreeEventKind::Watch {
            change,
            what,
            by: who,
            reason,
            then,
            unconfirmed,
            id: _,
        } => match change {
            WatchChange::Set => {
                let set_by = who
                    .as_ref()
                    .map(|who| format!(" (set by {who})"))
                    .unwrap_or_default();
                format!(
                    "{}{set_by}",
                    wt_core::watch_set_words(then, what).sentence()
                )
            }
            WatchChange::Held => format!("held: {what}"),
            WatchChange::ClearedGate | WatchChange::Woken | WatchChange::Notified => {
                wt_core::watch_fired_words(&[*change], *unconfirmed, what).sentence()
            }
            WatchChange::Cancelled => format!("stopped waiting for {what}{}", by(who)),
            WatchChange::Dropped => match reason {
                Some(reason) => format!("gave up waiting for {what}: {reason}"),
                None => format!("gave up waiting for {what}"),
            },
        },
        owed @ WorktreeEventKind::Owed { by: who, .. } => format!(
            "{}{}",
            wt_core::owed_sentence(owed).unwrap_or_default(),
            by_agent(who)
        ),
        WorktreeEventKind::Skill {
            name,
            args,
            by: who,
        } => {
            let args = args
                .as_ref()
                .map(|args| format!(" {args}"))
                .unwrap_or_default();
            format!("ran /{name}{args}{}", by_agent(who))
        }
        WorktreeEventKind::Mark { text, by: who } => format!("{text}{}", by_agent(who)),
        WorktreeEventKind::Removed { landed: true } => "removed after landing".into(),
        WorktreeEventKind::Removed { landed: false } => "removed".into(),
        WorktreeEventKind::Unknown => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use wt_core::{WorkRisk, WorkState};

    #[derive(Parser)]
    struct TestCli {
        #[command(flatten)]
        args: HistoryArgs,
    }

    #[test]
    fn the_parser_bounds_the_limit() {
        let parsed = TestCli::try_parse_from(["wt", "one", "--json", "--limit", "5"]).unwrap();
        assert_eq!(parsed.args.slug.as_deref(), Some("one"));
        assert!(parsed.args.json);
        assert_eq!(parsed.args.limit, 5);
        assert_eq!(TestCli::try_parse_from(["wt"]).unwrap().args.limit, 100);
        assert!(TestCli::try_parse_from(["wt", "--limit", "0"]).is_err());
        assert!(TestCli::try_parse_from(["wt", "--limit", "501"]).is_err());
    }

    #[test]
    fn text_is_newest_first_in_plain_words_and_skips_unknown_events() {
        let events = [
            WorktreeEvent {
                at_ms: 1_700_000_000_000,
                event: WorktreeEventKind::Created {
                    base: Some("main".into()),
                },
            },
            WorktreeEvent {
                at_ms: 1_700_000_060_000,
                event: WorktreeEventKind::Unknown,
            },
            WorktreeEvent {
                at_ms: 1_700_000_120_000,
                event: WorktreeEventKind::Status {
                    from: Some(WorkState::Working),
                    to: WorkState::NeedsTesting,
                    by: Some("one".into()),
                    risk: Some(WorkRisk::Low),
                    note: Some("ready for a look".into()),
                    blocked_on: Some("deploy".into()),
                    verify_in: Some("staging".into()),
                },
            },
        ];
        let lines = text(&events);
        assert_eq!(lines.len(), 2);
        assert!(
            lines[0].ends_with(
                "  status working -> needs-testing, risk low, blocked on deploy, check owed in staging by one: ready for a look"
            ),
            "{}",
            lines[0]
        );
        assert!(lines[1].ends_with("  created from main"), "{}", lines[1]);
        // "YYYY-MM-DD HH:MM", then two spaces.
        assert_eq!(lines[1].find("  "), Some(16), "{}", lines[1]);
        assert!(!lines.iter().any(|line| line.contains('\u{2014}')));
    }

    #[test]
    fn a_skill_use_and_a_mark_say_who_only_when_an_agent_did_it() {
        let skill = |name: &str, args: Option<&str>, by: Option<&str>| {
            describe(&WorktreeEventKind::Skill {
                name: name.into(),
                args: args.map(str::to_owned),
                by: by.map(str::to_owned),
            })
            .unwrap()
        };
        assert_eq!(skill("fable-review", None, None), "ran /fable-review");
        assert_eq!(
            skill("check", Some("src/foo"), Some("one")),
            "ran /check src/foo (by one)"
        );
        let mark = |by: Option<&str>| {
            describe(&WorktreeEventKind::Mark {
                text: "handed to QA".into(),
                by: by.map(str::to_owned),
            })
            .unwrap()
        };
        assert_eq!(mark(None), "handed to QA");
        assert_eq!(mark(Some("manager")), "handed to QA (by manager)");
    }

    #[test]
    fn a_watch_reads_as_when_it_was_set_and_what_wt_did() {
        let event = |change, what: &str, by: Option<&str>, reason: Option<&str>, then: &[&str]| {
            WorktreeEventKind::Watch {
                id: Some("aaaaaa".into()),
                change,
                what: what.into(),
                by: by.map(str::to_owned),
                reason: reason.map(str::to_owned),
                then: then.iter().map(|action| (*action).to_owned()).collect(),
                unconfirmed: false,
            }
        };
        let said = |events: &[WorktreeEvent]| -> Vec<String> {
            text(events)
                .into_iter()
                .rev()
                .map(|line| line.split_once("  ").unwrap().1.to_owned())
                .collect()
        };
        let at = |at_ms: u64, event| WorktreeEvent { at_ms, event };
        let hour = 3_600_000;
        // A wake at a time as earlier builds recorded it: four events.
        let wake = [
            at(0, event(WatchChange::Set, "23:08", None, None, &["wake"])),
            at(hour, event(WatchChange::Held, "23:08", None, None, &[])),
            at(
                hour + 1,
                WorktreeEventKind::Message {
                    from: Some("wt".into()),
                    text: "[wt] What you were waiting for has happened: 23:08.".into(),
                    delivered: Some(false),
                },
            ),
            at(
                hour + 2,
                event(WatchChange::Woken, "23:08", None, None, &[]),
            ),
        ];
        assert_eq!(
            said(&wake),
            [
                "will wake the session at 23:08",
                "wake sent, delivery not confirmed at 23:08",
            ]
        );
        assert_eq!(
            said(&wake[..2]),
            ["will wake the session at 23:08", "held: 23:08"]
        );
        // A gate set by the manager that clears and wakes.
        let what = "#3806 in production";
        let gate = [
            at(
                0,
                event(
                    WatchChange::Set,
                    what,
                    Some("manager"),
                    None,
                    &["unblock", "wake"],
                ),
            ),
            at(hour, event(WatchChange::Held, what, None, None, &[])),
            at(
                hour + 1,
                event(WatchChange::ClearedGate, what, None, None, &[]),
            ),
            at(hour + 2, event(WatchChange::Woken, what, None, None, &[])),
        ];
        assert_eq!(
            said(&gate),
            [
                "gate clears when #3806 in production, then wakes the session (set by manager)",
                "cleared the gate and woke the session: #3806 in production",
            ]
        );
        let told = [
            at(
                0,
                event(WatchChange::Set, "merged", None, None, &["notify"]),
            ),
            at(1_000, event(WatchChange::Held, "merged", None, None, &[])),
            at(
                2_000,
                event(WatchChange::Notified, "merged", None, None, &[]),
            ),
        ];
        assert_eq!(said(&told), ["told you: merged (as soon as it was set)"]);

        let one = |change, by, reason| describe(&event(change, what, by, reason, &[])).unwrap();
        assert_eq!(
            one(WatchChange::Set, None, None),
            "waiting for #3806 in production"
        );
        assert_eq!(
            one(WatchChange::Cancelled, None, None),
            "stopped waiting for #3806 in production"
        );
        assert_eq!(
            one(WatchChange::Cancelled, Some("one"), None),
            "stopped waiting for #3806 in production by one"
        );
        assert_eq!(
            one(WatchChange::Dropped, None, Some("its worktree is gone")),
            "gave up waiting for #3806 in production: its worktree is gone"
        );
    }

    #[tokio::test]
    async fn json_is_the_stored_events_oldest_first_and_a_removed_slug_still_reads() {
        let fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        let ctx = &fixture.ctx;
        ctx.database
            .call(|store| {
                for (slug, at_ms) in [("one", 1), ("one", 2), ("gone", 3)] {
                    store.append_worktree_event(
                        slug,
                        at_ms,
                        &WorktreeEventKind::Conflict {
                            resolved: at_ms != 1,
                        },
                    )?;
                }
                store.append_worktree_event(
                    "gone",
                    4,
                    &WorktreeEventKind::Removed { landed: true },
                )?;
                Ok(())
            })
            .await
            .unwrap();
        // A branch resolves to its live worktree's slug.
        assert_eq!(target(ctx, Some("one")).await.unwrap(), "one");
        // No live worktree has this name: it is a removed slug.
        assert_eq!(target(ctx, Some("gone")).await.unwrap(), "gone");
        let events = ctx
            .database
            .call(|store| Ok(store.read_worktree_events("one", 100)?))
            .await
            .unwrap();
        assert_eq!(
            serde_json::to_value(&events).unwrap(),
            serde_json::json!([
                {"at_ms": 1, "event": {"kind": "conflict", "resolved": false}},
                {"at_ms": 2, "event": {"kind": "conflict", "resolved": true}},
            ])
        );
        let gone = ctx
            .database
            .call(|store| Ok(store.read_worktree_events("gone", 1)?))
            .await
            .unwrap();
        assert_eq!(text(&gone).len(), 1);
        assert!(text(&gone)[0].ends_with("removed after landing"));
        assert_eq!(
            run(
                ctx,
                &HistoryArgs {
                    slug: Some("gone".into()),
                    json: true,
                    limit: 100
                }
            )
            .await
            .unwrap(),
            0
        );
        fixture.close().await.unwrap();
    }
}
