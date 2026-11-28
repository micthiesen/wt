use serde_json::json;

use super::*;

/// 2026-11-24 12:00:00 UTC.
const NOON: i64 = 1_795_521_600_000;
const UTC: LocalClock = LocalClock {
    now_ms: NOON,
    utc_offset_minutes: 0,
};
/// The same instant where local time is 07:00.
const WEST: LocalClock = LocalClock {
    now_ms: NOON,
    utc_offset_minutes: -300,
};

fn cond(arg: &str) -> WatchCondition {
    parse(arg, UTC).unwrap_or_else(|error| panic!("{error}"))
}

fn problem(arg: &str) -> String {
    parse(arg, UTC).unwrap_err().problem
}

fn watch(conditions: &[&str]) -> Watch {
    Watch {
        id: "abc234".into(),
        slug: "one".into(),
        incarnation: Some("2026-11-01T00:00:00Z".into()),
        by: "one".into(),
        created_at_ms: NOON,
        utc_offset_minutes: 0,
        when: conditions.iter().map(|arg| cond(arg)).collect(),
        then: WatchActions {
            notify: Some(String::new()),
            ..WatchActions::default()
        },
        met_at_ms: None,
        last_error: None,
        owner_gone: false,
        extra: Map::new(),
    }
}

fn own_row(set: impl FnOnce(&mut RowFacts)) -> WatchFacts {
    let mut row = RowFacts {
        exists: true,
        incarnation: Some("2026-11-01T00:00:00Z".into()),
        ..RowFacts::default()
    };
    set(&mut row);
    WatchFacts {
        rows: [("one".to_owned(), row)].into(),
        ..WatchFacts::default()
    }
}

/// Evaluate and store, as the engine does each pass.
fn pass(watch: &mut Watch, facts: &WatchFacts, now_ms: i64) -> Evaluation {
    let evaluation = evaluate(watch, facts, now_ms);
    if let Some(stamps) = &evaluation.stamps {
        stamps.apply(watch);
    }
    evaluation
}

#[test]
fn noon_is_the_instant_the_fixtures_say() {
    assert_eq!(crate::parse_iso_millis("2026-11-24T12:00:00Z"), Some(NOON));
}

#[test]
fn every_form_of_the_grammar_parses() {
    use Condition::*;
    let stage = VerifyStage::Production;
    for (arg, expected) in [
        ("merged", Own(RowState::Merged)),
        ("green", Own(RowState::Green)),
        ("closed", Own(RowState::Closed)),
        (
            "#3806",
            Pr {
                number: 3806,
                goal: PrGoal::Merged,
            },
        ),
        (
            "#3806:merged",
            Pr {
                number: 3806,
                goal: PrGoal::Merged,
            },
        ),
        (
            "#3806:production",
            Pr {
                number: 3806,
                goal: PrGoal::Reached(stage),
            },
        ),
        (
            "#3806:closed",
            Pr {
                number: 3806,
                goal: PrGoal::Closed,
            },
        ),
        (
            "pr:3806",
            Pr {
                number: 3806,
                goal: PrGoal::Merged,
            },
        ),
        (
            "pr:3806:production",
            Pr {
                number: 3806,
                goal: PrGoal::Reached(stage),
            },
        ),
        // `wt:` names a worktree whatever its slug is.
        (
            "wt:pr:merged",
            Worktree {
                slug: "pr".into(),
                state: RowState::Merged,
            },
        ),
        (
            "wt:release:staging",
            Worktree {
                slug: "release".into(),
                state: RowState::Staging,
            },
        ),
        ("wt:wt:gone", Gone { slug: "wt".into() }),
        (
            "wt:other:verified",
            Worktree {
                slug: "other".into(),
                state: RowState::Verified,
            },
        ),
        (
            "other:verified",
            Worktree {
                slug: "other".into(),
                state: RowState::Verified,
            },
        ),
        (
            "other:gone",
            Gone {
                slug: "other".into(),
            },
        ),
        ("release:staging", Release(VerifyStage::Staging)),
        (
            "check:secret",
            Check {
                id: "secret".into(),
                arg: None,
            },
        ),
        (
            "check:secret=FOO",
            Check {
                id: "secret".into(),
                arg: Some("FOO".into()),
            },
        ),
        (
            "check:url=https://x/a+b",
            Check {
                id: "url".into(),
                arg: Some("https://x/a+b".into()),
            },
        ),
        (
            "commit:0123abc:staging",
            Commit {
                oid: "0123abc".into(),
                stage: VerifyStage::Staging,
            },
        ),
    ] {
        assert_eq!(cond(arg), WatchCondition::new(expected, None), "{arg}");
    }
    assert_eq!(cond("  ready "), cond("ready"));
}

#[test]
fn a_worktree_named_like_the_grammar_needs_its_prefix_and_old_records_still_read() {
    // Bare, each of these could mean two things; the refusal shows the
    // spelling that cannot.
    for (arg, explicit) in [
        ("pr:merged", "wt:pr:merged"),
        ("wt:merged", "wt:wt:merged"),
        ("at:gone", "wt:at:gone"),
        ("3h:merged", "wt:3h:merged"),
        ("5pm:ready", "wt:5pm:ready"),
    ] {
        let problem = problem(arg);
        assert!(
            problem.contains("could mean two things"),
            "{arg}: {problem}"
        );
        assert!(problem.ends_with(explicit), "{arg}: {problem}");
        assert!(parse(explicit, UTC).is_ok(), "{explicit}");
    }
    // These read as their own form first; the refusal still shows the way.
    assert!(problem("release:merged").ends_with("a worktree named release is wt:release:merged"));
    assert!(problem("commit:gone").ends_with("a worktree named commit is wt:commit:gone"));
    // The stored spelling is the explicit one, so no two conditions share a
    // text; what an earlier build stored bare reads back as it did.
    let stored = |cond: &WatchCondition| serde_json::to_value(cond).unwrap()["cond"].clone();
    assert_eq!(stored(&cond("other:merged")), "wt:other:merged");
    assert_eq!(stored(&cond("other:gone")), "wt:other:gone");
    for (record, slug) in [
        ("pr:merged", "pr"),
        ("wt:merged", "wt"),
        ("other:merged", "other"),
    ] {
        let read: WatchCondition = serde_json::from_value(json!({ "cond": record })).unwrap();
        assert_eq!(
            read.cond,
            Condition::Worktree {
                slug: slug.into(),
                state: RowState::Merged
            },
            "{record}"
        );
    }
}

#[test]
fn times_are_local_wall_clock_and_strictly_in_the_future() {
    let at = |arg: &str, clock| match parse(arg, clock).unwrap().cond {
        Condition::At { at_ms } => at_ms,
        other => panic!("{other:?}"),
    };
    assert_eq!(at("17:00", UTC), NOON + 5 * HOUR_MS);
    assert_eq!(at("9:30", UTC), NOON + 21 * HOUR_MS + 30 * MINUTE_MS);
    // Exactly now is not in the future, so it is tomorrow's.
    assert_eq!(at("12:00", UTC), NOON + DAY_MS);
    // 07:00 local: 17:00 local is ten hours away.
    assert_eq!(at("17:00", WEST), NOON + 10 * HOUR_MS);
    assert_eq!(at("06:59", WEST), NOON + DAY_MS - MINUTE_MS);
    assert_eq!(at("+3h", UTC), NOON + 3 * HOUR_MS);
    assert_eq!(at("+90m", UTC), NOON + 90 * MINUTE_MS);
    assert_eq!(at("+2d", UTC), NOON + 2 * DAY_MS);
    assert_eq!(at("2026-11-25T17:00", UTC), NOON + 29 * HOUR_MS);
    assert_eq!(at("2026-11-25 17:00", WEST), NOON + 34 * HOUR_MS);
    assert_eq!(at("2026-11-25T17:00:30", UTC), NOON + 29 * HOUR_MS + 30_000);
    // The spellings people and agents reach for first.
    assert_eq!(at("5pm", UTC), NOON + 5 * HOUR_MS);
    assert_eq!(at("5:30PM", UTC), NOON + 5 * HOUR_MS + 30 * MINUTE_MS);
    assert_eq!(at("12am", UTC), NOON + 12 * HOUR_MS);
    assert_eq!(at("12pm", UTC), NOON + DAY_MS);
    assert_eq!(at("9am", UTC), NOON + 21 * HOUR_MS);
    assert_eq!(at("3h", UTC), at("+3h", UTC));
    assert_eq!(at("90m", UTC), at("+90m", UTC));
    assert_eq!(at("2d", UTC), at("+2d", UTC));
    assert_eq!(at("tomorrow 09:00", UTC), NOON + 21 * HOUR_MS);
    assert_eq!(at("tomorrow 5pm", UTC), NOON + 29 * HOUR_MS);
    assert_eq!(at("tomorrow 13:00", UTC), NOON + 25 * HOUR_MS);
    assert_eq!(at("today 17:00", UTC), NOON + 5 * HOUR_MS);
    assert_eq!(at("tomorrow 09:00", WEST), NOON + 26 * HOUR_MS);
    // A relative time lands on a whole second so it prints exactly.
    let ragged = LocalClock {
        now_ms: NOON + 1,
        ..UTC
    };
    assert_eq!(at("+90m", ragged), NOON + 90 * MINUTE_MS + SECOND_MS);
}

#[test]
fn what_cannot_be_meant_is_refused_with_the_reason() {
    assert_eq!(problem("2026-11-24T11:59"), "that time has already passed");
    assert_eq!(problem("at:5"), "that time has already passed");
    assert!(problem("2026-02-30T10:00").contains("does not exist"));
    assert!(problem("2026-11-25").contains("2030-01-15T09:00"));
    assert!(problem("25:00").contains("00:00 to 23:59"));
    assert!(problem("+3w").contains("+90m"));
    assert_eq!(problem("today 09:00"), "that time has already passed");
    assert!(problem("13pm").contains("12-hour"));
    assert!(problem("tomorrow morning").contains("\"tomorrow 09:00\""));
    // What was meant as a time is answered with how a time is written.
    for meant in [
        "in 3 hours",
        "5 o'clock",
        "tomorrow",
        "5 pm tomorrow",
        "noon",
        "3 hours",
    ] {
        let problem = problem(meant);
        assert!(
            problem.starts_with("a time is a clock time"),
            "{meant}: {problem}"
        );
    }
    // A slug may start with a digit.
    assert!(problem("123-fix:mergd").contains("is not a state"));
    assert!(problem("+0h").contains("+90m"));
    assert!(problem("mergd").contains("is not a state"));
    assert!(problem("other:mergd").contains("is not a state"));
    assert!(problem("gone").contains("<slug>:gone"));
    assert!(problem(":merged").contains("is not a worktree"));
    for pr in ["#", "#0", "#12a", "#-3", "#:merged", "pr:0", "pr:12a"] {
        assert!(problem(pr).contains("is not a pull request number"), "{pr}");
    }
    assert!(problem("#12:ready").contains("a pull request can be"));
    assert!(problem("pr:12:ready").contains("a pull request can be"));
    assert!(problem("release:canary").contains("release:staging"));
    for check in ["check:", "check:=x", "check:secret="] {
        assert!(problem(check).contains("check:<id>"), "{check}");
    }
    // The argument is one argv entry of someone's command.
    assert!(problem("check:secret=--help").contains("cannot start with `-`"));
    assert!(problem("check:secret=-rf+1h").contains("cannot start with `-`"));
    assert!(problem("check:secret=a\u{1b}[2Jb").contains("control characters"));
    assert!(problem("check:secret=a\nb").contains("control characters"));
    assert!(problem("merged+soon").contains("a delay looks like"));
    assert!(problem("17:00+1h").contains("a time takes no delay"));
    assert!(problem("+3h+1h").contains("+90m"));
    assert_eq!(problem("  "), "it is empty");
}

#[test]
fn a_refusal_teaches_the_grammar() {
    let text = parse("mergd", UTC).unwrap_err().to_string();
    assert!(text.starts_with("cannot read the condition `mergd`: `mergd` is not a state"));
    for form in [
        "17:00 | 5pm | 5:30pm | 3h",
        "pr:3806:merged|staging|production|closed",
        "an unquoted # starts a shell comment",
        "wt:<slug>:merged",
        "settled | reviewed | evicted",
        "release:staging | release:production",
        "check:<id>=<arg>",
        "+<duration>",
        "Examples: pr:3806:production   release:production+24h   17:00",
    ] {
        assert!(text.contains(form), "{form}\n{text}");
    }
}

#[test]
fn a_delay_follows_anything_but_a_time() {
    let delayed = cond("release:production+24h");
    assert_eq!(delayed.cond, Condition::Release(VerifyStage::Production));
    assert_eq!(delayed.delay, Delay::from_ms(DAY_MS));
    assert_eq!(cond("#3823:production+1h").delay, Delay::from_ms(HOUR_MS));
    assert_eq!(cond("pr:3823:production+1h"), cond("#3823:production+1h"));
    assert_eq!(cond("check:secret=FOO+90m").delay, Delay::parse("90m"));
    for (text, ms) in [
        ("1d", DAY_MS),
        ("36h", 36 * HOUR_MS),
        ("90m", 90 * MINUTE_MS),
        ("45s", 45_000),
    ] {
        let delay = Delay::parse(text).unwrap();
        assert_eq!((delay.as_ms(), delay.to_string().as_str()), (ms, text));
    }
    assert_eq!(Delay::parse("48h").unwrap().to_string(), "2d");
    assert_eq!(Delay::parse("367d"), None);
    assert_eq!(Delay::from_ms(0), None);
}

#[test]
fn what_is_printed_parses_back_to_the_same_condition() {
    for arg in [
        "merged",
        "staging+2h",
        "#3806:merged",
        "#7:staging+30m",
        "pr:7:closed",
        "other:green+1h",
        "other:gone",
        "release:production+24h",
        "check:secret=FOO",
        "check:up",
        "commit:0123abcdef:production",
        "17:00",
        "+90m",
        "2026-12-31T23:59:59",
    ] {
        let parsed = cond(arg);
        for clock in [UTC, WEST] {
            let printed = parsed.to_arg(clock.utc_offset_minutes);
            assert_eq!(
                parse(&printed, clock).as_ref(),
                Ok(&parsed),
                "{arg} as {printed}"
            );
        }
        let stored = parsed.cond.to_string();
        assert_eq!(cond(&stored).cond, parsed.cond, "{arg} stored as {stored}");
    }
    assert_eq!(cond("2026-11-25T17:00").to_arg(0), "2026-11-25 17:00");
    assert_eq!(cond("2026-11-25T17:00").to_arg(-300), "2026-11-25 12:00");
    // What is printed for a shell never starts a comment.
    assert_eq!(cond("#3806").to_arg(0), "pr:3806:merged");
    assert_eq!(cond("#7:staging+30m").to_arg(0), "pr:7:staging+30m");
    assert_eq!(cond("pr:7").cond.to_string(), "pr:7:merged");
    assert_eq!(cond("other:green+1h").to_arg(0), "wt:other:green+1h");
    // A record keeps the spelling earlier builds read.
    assert_eq!(
        serde_json::to_value(cond("pr:7:production")).unwrap()["cond"],
        "#7:production"
    );
}

#[test]
fn conditions_read_in_words() {
    for (arg, words) in [
        ("#3806:production", "#3806 in production"),
        ("#3806", "#3806 merged"),
        ("release:production", "next production release"),
        ("release:production+24h", "1d after next production release"),
        ("17:00", "17:00"),
        ("09:00", "09:00 tomorrow"),
        ("2026-11-30T09:00:30", "Nov 30 09:00"),
        ("2027-01-02T09:00", "Jan 2 09:00, 2027"),
        ("green+1h", "PR checks green for 1h"),
        ("check:secret=FOO", "check secret=FOO"),
        ("check:up+10m", "check up for 10m"),
        ("other:staging", "other on staging"),
        ("other:gone", "other removed"),
        ("commit:0123abcdef:staging", "0123abc on staging"),
        ("merged+2h", "2h after merged"),
        // The row's own stage reads "in production"; after a delay it is
        // the moment it got there.
        ("production", "in production"),
        ("production+1m", "1m after it reaches production"),
        ("staging+2h", "2h after it reaches staging"),
        ("closed", "PR closed"),
    ] {
        assert_eq!(cond(arg).describe(UTC), words, "{arg}");
    }
    // A headline names a check by what it proves: it stops at the first
    // line break and before steps written inline.
    for (text, line) in [
        (
            "Prod proves beta export. STEPS: 1. export an invoice 2. open the CSV",
            "Prod proves beta export.",
        ),
        ("\n  Look.\nSTEPS: 1. open /health", "Look."),
        (
            "STEPS: 1. open /health 2. sign in",
            "STEPS: 1. open /health 2. sign in",
        ),
        ("1. check prod 2. check logs", "1. check prod 2. check logs"),
    ] {
        assert_eq!(headline(text), line);
    }
    // Local 07:00: 01:00 UTC tomorrow is still today, 20:00 local.
    assert_eq!(cond("at:1795568400000").describe(WEST), "20:00");
    let mut both = watch(&["#3806:production", "17:00"]);
    assert_eq!(both.describe(UTC), "#3806 in production and 17:00");
    // Kept text reads from the day the watch was set, in its setter's time.
    both.utc_offset_minutes = 60;
    assert_eq!(both.summary(), "#3806 in production and 18:00");
    assert_eq!(watch(&["09:00"]).summary(), "09:00 tomorrow");
    assert_eq!(watch(&["2026-11-26T17:00:05"]).summary(), "Nov 26 17:00");
    // What is typed back keeps the whole instant.
    assert_eq!(cond("2026-11-26T17:00:05").to_arg(0), "2026-11-26 17:00:05");
}

#[test]
fn a_time_reads_at_and_anything_else_reads_when() {
    for (conditions, kept) in [
        (&["17:00"][..], "at 17:00"),
        (&["09:00"], "at 09:00 tomorrow"),
        (&["2026-11-30T09:00"], "at Nov 30 09:00"),
        (&["2027-01-02T09:00"], "at Jan 2 09:00, 2027"),
        (&["merged"], "when merged"),
        (&["check:flag=go"], "when check flag=go"),
        (
            &["#3806:production", "17:00"],
            "when #3806 in production and 17:00",
        ),
        (&["17:00", "18:00"], "when 17:00 and 18:00"),
    ] {
        let watch = watch(conditions);
        assert_eq!(watch.when_summary(), kept);
        assert_eq!(watch.when_described(UTC), kept);
        // History keeps only the words, and reads them the same way.
        assert_eq!(when_phrase(&watch.summary()), kept);
    }
    // As earlier builds wrote the day.
    assert_eq!(when_phrase("tomorrow 09:00"), "at tomorrow 09:00");
    for words in ["1h after merged", "May flowers", "Nov 30", "17:00 sharp"] {
        assert!(!is_time_phrase(words), "{words}");
    }
}

#[test]
fn what_a_met_watch_did_reads_from_the_day_it_was_met() {
    let mut early = watch(&["09:00"]);
    assert_eq!(early.summary_met(), "09:00 tomorrow");
    early.met_at_ms = Some(NOON + 21 * HOUR_MS);
    assert_eq!(early.summary_met(), "09:00");
    assert_eq!(early.summary(), "09:00 tomorrow");
}

#[test]
fn only_a_wake_still_owed_to_a_live_worktree_wakes_its_owner() {
    let mut watch = watch(&["#3806:production", "09:00"]);
    assert!(!watch.wakes_owner());
    watch.then.wake = Some("\nRun the smoke test.\nThen report.".into());
    assert!(watch.wakes_owner());
    assert_eq!(
        when_described(&watch.when, UTC),
        "when #3806 in production and 09:00 tomorrow"
    );
    // A gate that wakes afterwards counts; the gate alone does not.
    watch.then.unblock = Some("the release".into());
    assert!(watch.wakes_owner());
    watch.then.wake = None;
    assert!(!watch.wakes_owner());
    watch.then.wake = Some("Report.".into());
    watch.owner_gone = true;
    assert!(!watch.wakes_owner());
}

#[test]
fn before_release_is_kept_beside_the_keys_older_builds_know() {
    let mut watch = watch(&["staging"]);
    assert!(!watch.before_release());
    watch.set_before_release(true);
    assert_eq!(serde_json::to_value(&watch).unwrap()["beforeRelease"], true);
    assert!(watch.before_release());
    watch.set_before_release(false);
    assert!(
        serde_json::to_value(&watch)
            .unwrap()
            .get("beforeRelease")
            .is_none()
    );
}

#[test]
fn a_record_round_trips_with_keys_this_build_does_not_know() {
    let stored = json!({
        "id": "abc234", "slug": "one", "incarnation": "2026-11-01T00:00:00Z", "by": "one",
        "createdAtMs": NOON, "utcOffsetMinutes": -300, "futureTop": {"x": 1},
        "when": [
            {"cond": "#3806:production", "delayMs": 3_600_000, "firstHeldAtMs": 5, "futureCond": 2},
            {"cond": "release:staging", "baseline": "abc", "latched": true},
            {"cond": "moon:full"},
            {"cond": "at:1795568400000"}
        ],
        "then": {"unblock": "after #3806", "wake": "test it", "futureAction": "x"},
        "metAtMs": 9, "lastError": "probe failed", "ownerGone": true
    });
    let watch: Watch = serde_json::from_value(stored.clone()).unwrap();
    assert_eq!(watch.when[0].delay, Delay::from_ms(HOUR_MS));
    assert_eq!(watch.when[2].cond, Condition::Unknown("moon:full".into()));
    assert_eq!(watch.when[2].describe(UTC), "moon:full");
    assert_eq!(
        watch.when[3].cond,
        Condition::At {
            at_ms: 1_795_568_400_000
        }
    );
    assert_eq!(
        watch.then.remaining().collect::<Vec<_>>(),
        [WatchAction::Unblock, WatchAction::Wake]
    );
    assert_eq!(serde_json::to_value(&watch).unwrap(), stored);

    let minimal: Watch = serde_json::from_value(json!({
        "id": "a", "slug": "one", "by": "human", "createdAtMs": 1,
        "when": [{"cond": "merged"}], "then": {"notify": ""}
    }))
    .unwrap();
    assert!(minimal.then.notify_only() && !minimal.is_met());
    // No recorded incarnation is the worktree that has none, not any
    // worktree that later takes the slug.
    assert!(minimal.owned_by(None) && !minimal.owned_by(Some("any")));
    assert_eq!(
        serde_json::to_value(&minimal).unwrap(),
        json!({
            "id": "a", "slug": "one", "by": "human", "createdAtMs": 1, "utcOffsetMinutes": 0,
            "when": [{"cond": "merged"}], "then": {"notify": ""}
        })
    );
}

#[test]
fn a_condition_from_a_newer_build_never_holds() {
    let mut newer = watch(&["merged"]);
    newer.when.push(WatchCondition::new(
        Condition::Unknown("moon:full".into()),
        None,
    ));
    let evaluation = pass(&mut newer, &own_row(|row| row.merged = Some(true)), NOON);
    assert_eq!(evaluation.truths[0], Truth::Holds);
    assert!(matches!(&evaluation.truths[1], Truth::Unknown(why) if why.contains("moon:full")));
    assert!(!evaluation.met && !newer.is_met());
    assert!(needs([&newer]).is_empty());
}

#[test]
fn every_condition_must_hold_and_unknown_is_not_holding() {
    let mut both = watch(&["merged", "green"]);
    let partly = own_row(|row| row.merged = Some(true));
    let evaluation = pass(&mut both, &partly, NOON);
    assert_eq!(evaluation.truths[0], Truth::Holds);
    assert!(matches!(evaluation.truths[1], Truth::Unknown(_)));
    assert!(!evaluation.met);
    assert_eq!(both.when[0].first_held_at_ms, Some(NOON));

    let all = own_row(|row| {
        row.merged = Some(true);
        row.green = Some(true);
    });
    assert!(pass(&mut both, &all, NOON + 5).met);
    assert_eq!(both.met_at_ms, Some(NOON + 5));
    // Met is never taken back, and nothing more is stamped.
    let later = evaluate(&both, &own_row(|row| row.green = Some(false)), NOON + 10);
    assert!(later.met && later.stamps.is_none());

    // No facts at all: nothing holds, nothing is written.
    let blind = evaluate(
        &watch(&["merged", "other:gone", "#4"]),
        &WatchFacts::default(),
        NOON,
    );
    assert!(
        blind
            .truths
            .iter()
            .all(|truth| matches!(truth, Truth::Unknown(_)))
    );
    assert_eq!(blind.stamps, None);
    // A watch with no conditions is broken data, not a met watch.
    assert!(!evaluate(&watch(&[]), &WatchFacts::default(), NOON).met);
}

#[test]
fn own_conditions_belong_to_the_incarnation_that_set_them() {
    let own = watch(&["merged"]);
    let reborn = own_row(|row| {
        row.merged = Some(true);
        row.incarnation = Some("2026-11-20T00:00:00Z".into());
    });
    assert_eq!(
        evaluate(&own, &reborn, NOON).truths,
        [Truth::Unknown("its worktree is gone".into())]
    );
    let gone = own_row(|row| {
        row.exists = false;
        row.merged = Some(true);
    });
    assert!(!evaluate(&own, &gone, NOON).met);
    // Another worktree is named by slug alone.
    let mut facts = own_row(|_| {});
    facts.rows.insert(
        "other".into(),
        RowFacts {
            exists: false,
            merged: Some(true),
            ..RowFacts::default()
        },
    );
    assert!(evaluate(&watch(&["other:merged", "other:gone"]), &facts, NOON).met);
    facts.rows.get_mut("other").unwrap().exists = true;
    assert_eq!(
        evaluate(&watch(&["other:gone"]), &facts, NOON).truths,
        [Truth::NotYet]
    );
}

#[test]
fn a_delay_counts_one_unbroken_stretch() {
    let mut green = watch(&["green+1h"]);
    let yes = own_row(|row| row.green = Some(true));
    let no = own_row(|row| row.green = Some(false));
    let unknown = own_row(|_| {});
    assert_eq!(pass(&mut green, &yes, NOON).truths, [Truth::NotYet]);
    assert_eq!(green.when[0].first_held_at_ms, Some(NOON));
    // Unknown neither counts nor breaks the stretch.
    assert!(matches!(
        pass(&mut green, &unknown, NOON + HOUR_MS).truths[0],
        Truth::Unknown(_)
    ));
    assert_eq!(green.when[0].first_held_at_ms, Some(NOON));
    // Known false breaks it.
    pass(&mut green, &no, NOON + 10 * MINUTE_MS);
    assert_eq!(green.when[0].first_held_at_ms, None);
    pass(&mut green, &yes, NOON + 20 * MINUTE_MS);
    assert!(!pass(&mut green, &yes, NOON + HOUR_MS).met);
    assert!(pass(&mut green, &yes, NOON + 80 * MINUTE_MS).met);
    assert_eq!(green.met_at_ms, Some(NOON + 80 * MINUTE_MS));
}

#[test]
fn an_unchanged_pass_stamps_nothing() {
    let mut waiting = watch(&["green+1h", "17:00"]);
    let facts = own_row(|row| row.green = Some(true));
    assert!(pass(&mut waiting, &facts, NOON).stamps.is_some());
    assert_eq!(evaluate(&waiting, &facts, NOON + 1).stamps, None);
    let stamps = evaluate(&watch(&["green+1h"]), &facts, NOON)
        .stamps
        .unwrap();
    let mut fresh = watch(&["green+1h"]);
    assert!(stamps.apply(&mut fresh).changed);
    assert_eq!(stamps.apply(&mut fresh), Applied::default());
}

#[test]
fn a_release_is_the_next_one_after_its_baseline() {
    let stage = VerifyStage::Production;
    let released = |sha: &str| WatchFacts {
        releases: [(stage, sha.to_owned())].into(),
        ..WatchFacts::default()
    };
    let mut next = watch(&["release:production+1h"]);
    assert_eq!(needs([&next]).releases, [stage]);
    assert!(matches!(
        pass(&mut next, &WatchFacts::default(), NOON).truths[0],
        Truth::Unknown(_)
    ));
    // First sight only records what to compare against.
    assert_eq!(
        pass(&mut next, &released("aaa"), NOON).truths,
        [Truth::NotYet]
    );
    assert_eq!(next.when[0].baseline.as_deref(), Some("aaa"));
    assert_eq!(pass(&mut next, &released("aaa"), NOON + 1).stamps, None);
    assert_eq!(
        pass(&mut next, &released("bbb"), NOON + 2).truths,
        [Truth::NotYet]
    );
    assert!(next.when[0].latched);
    // The release happened: a rollback or lost probe does not undo it.
    assert!(needs([&next]).is_empty());
    assert!(!pass(&mut next, &released("aaa"), NOON + 3).met);
    assert!(pass(&mut next, &WatchFacts::default(), NOON + 2 + HOUR_MS).met);

    let mut seeded = watch(&["release:production"]);
    seeded.when[0].baseline = Some("aaa".into());
    assert!(pass(&mut seeded, &released("bbb"), NOON).met);
}

#[test]
fn a_merged_pr_is_followed_by_its_merge_commit() {
    let stage = VerifyStage::Production;
    let pr = |facts: PrFacts| WatchFacts {
        prs: [(7, facts)].into(),
        ..WatchFacts::default()
    };
    let mut reach = watch(&["#7:production"]);
    assert_eq!(needs([&reach]).prs, [7].into());
    let open = pr(PrFacts {
        life: Some(PrLife::Open),
        ..PrFacts::default()
    });
    assert_eq!(pass(&mut reach, &open, NOON).truths, [Truth::NotYet]);
    let merged = pr(PrFacts {
        life: Some(PrLife::Merged),
        merge_oid: Some("0123abcdef".into()),
        ..PrFacts::default()
    });
    assert!(matches!(
        pass(&mut reach, &merged, NOON).truths[0],
        Truth::Unknown(_)
    ));
    let commit = Condition::Commit {
        oid: "0123abcdef".into(),
        stage,
    };
    assert_eq!(reach.when[0].cond, commit);
    let wanted = needs([&reach]);
    assert!(wanted.prs.is_empty());
    assert_eq!(wanted.commits, [("0123abcdef".to_owned(), stage)]);
    let reached = |reached| WatchFacts {
        commits: [(("0123abcdef".to_owned(), stage), reached)].into(),
        ..WatchFacts::default()
    };
    assert_eq!(
        pass(&mut reach, &reached(false), NOON).truths,
        [Truth::NotYet]
    );
    assert!(pass(&mut reach, &reached(true), NOON).met);

    for (goal, life, expected) in [
        ("#7", PrLife::Merged, Truth::Holds),
        ("#7", PrLife::Closed, Truth::NotYet),
        ("#7:closed", PrLife::Closed, Truth::Holds),
        ("#7:closed", PrLife::Merged, Truth::NotYet),
        ("#7:staging", PrLife::Closed, Truth::NotYet),
    ] {
        let facts = pr(PrFacts {
            life: Some(life),
            ..PrFacts::default()
        });
        assert_eq!(
            evaluate(&watch(&[goal]), &facts, NOON).truths,
            [expected],
            "{goal} {life:?}"
        );
    }
    let known = pr(PrFacts {
        life: Some(PrLife::Merged),
        merge_oid: Some("0123abcdef".into()),
        production: Some(true),
        ..PrFacts::default()
    });
    assert!(evaluate(&watch(&["#7:production"]), &known, NOON).met);
}

#[test]
fn a_failing_check_keeps_waiting_and_reports_once() {
    let key = CheckKey {
        slug: "one".into(),
        id: "secret".into(),
        arg: Some("FOO".into()),
    };
    let checked = |result: Result<bool, &str>| WatchFacts {
        checks: [(key.clone(), result.map_err(str::to_owned))].into(),
        ..WatchFacts::default()
    };
    let mut check = watch(&["check:secret=FOO"]);
    assert_eq!(needs([&check]).checks, [key.clone()].into());
    assert!(matches!(
        pass(&mut check, &WatchFacts::default(), NOON).truths[0],
        Truth::Unknown(_)
    ));
    let failed = pass(&mut check, &checked(Err("exit 2: no vault")), NOON);
    assert_eq!(failed.truths, [Truth::Unknown("exit 2: no vault".into())]);
    assert_eq!(check.last_error.as_deref(), Some("exit 2: no vault"));
    assert_eq!(
        evaluate(&check, &checked(Err("exit 2: no vault")), NOON + 1).stamps,
        None
    );
    assert_eq!(
        pass(&mut check, &checked(Ok(false)), NOON).truths,
        [Truth::NotYet]
    );
    assert_eq!(check.last_error, None);
    assert!(pass(&mut check, &checked(Ok(true)), NOON).met);
    // Another worktree's run of the same check says nothing about this one.
    let mut other = watch(&["check:secret=FOO"]);
    other.slug = "two".into();
    assert!(!evaluate(&other, &checked(Ok(true)), NOON).met);
}

#[test]
fn stamps_do_not_land_on_a_condition_that_was_replaced() {
    let merged = WatchFacts {
        prs: [(
            7,
            PrFacts {
                life: Some(PrLife::Merged),
                ..PrFacts::default()
            },
        )]
        .into(),
        ..WatchFacts::default()
    };
    let stamps = evaluate(&watch(&["#7"]), &merged, NOON).stamps.unwrap();
    assert_eq!(stamps.met_at_ms, Some(NOON));
    let mut replaced = watch(&["#8"]);
    assert_eq!(stamps.apply(&mut replaced), Applied::default());
    assert!(!replaced.is_met());
    let mut other = watch(&["#7"]);
    other.id = "zzz".into();
    assert_eq!(stamps.apply(&mut other), Applied::default());
    let mut same = watch(&["#7"]);
    assert_eq!(
        stamps.apply(&mut same),
        Applied {
            changed: true,
            newly_met: true
        }
    );
    assert_eq!(stamps.apply(&mut same), Applied::default());
}

#[test]
fn only_pending_watches_ask_for_facts() {
    let mut met = watch(&[
        "#7",
        "check:up",
        "release:staging",
        "commit:0123abc:staging",
    ]);
    assert!(!needs([&met]).is_empty());
    met.met_at_ms = Some(NOON);
    assert!(needs([&met]).is_empty());
    assert!(needs([&watch(&["merged", "other:green", "other:gone", "17:00"])]).is_empty());
    let twice = [
        watch(&["#7", "release:staging"]),
        watch(&["#7:closed", "release:staging"]),
    ];
    let wanted = needs(&twice);
    assert_eq!((wanted.prs.len(), wanted.releases.len()), (1, 1));
}

#[test]
fn a_notify_watch_outlives_its_landed_owner_on_the_landing_commit() {
    let landed = OwnerRemoval {
        landed: true,
        landing_commit: Some("0123abcdef"),
    };
    let own = watch(&["merged", "production+1h", "other:gone"]);
    let kept = own.without_owner(landed, NOON).unwrap();
    assert!(kept.owner_gone);
    assert!(kept.when[0].latched);
    assert_eq!(kept.when[0].first_held_at_ms, Some(NOON));
    assert_eq!(
        kept.when[1].cond,
        Condition::Commit {
            oid: "0123abcdef".into(),
            stage: VerifyStage::Production
        }
    );
    assert_eq!(kept.when[1].delay, Delay::from_ms(HOUR_MS));
    assert_eq!(kept.when[2], own.when[2]);
    assert_eq!(
        kept.without_owner(
            OwnerRemoval {
                landed: false,
                landing_commit: None
            },
            NOON
        ),
        Ok(kept.clone())
    );
    // With no row left, the latched condition still holds.
    let facts = WatchFacts {
        rows: [("other".to_owned(), RowFacts::default())].into(),
        commits: [(("0123abcdef".to_owned(), VerifyStage::Production), true)].into(),
        ..WatchFacts::default()
    };
    let mut kept = kept;
    assert!(!pass(&mut kept, &facts, NOON).met);
    assert!(pass(&mut kept, &facts, NOON + HOUR_MS).met);

    let no_commit = OwnerRemoval {
        landed: true,
        landing_commit: None,
    };
    assert!(watch(&["merged"]).without_owner(no_commit, NOON).is_ok());
    assert!(
        own.without_owner(no_commit, NOON)
            .unwrap_err()
            .contains("no landing commit")
    );
    let unlanded = OwnerRemoval {
        landed: false,
        landing_commit: Some("0123abcdef"),
    };
    assert!(
        own.without_owner(unlanded, NOON)
            .unwrap_err()
            .contains("before landing")
    );
    assert!(
        watch(&["green"])
            .without_owner(landed, NOON)
            .unwrap_err()
            .contains("green")
    );
    let mut wakes = watch(&["merged"]);
    wakes.then.wake = Some("test it".into());
    assert!(wakes.without_owner(landed, NOON).is_err());
}

#[test]
fn actions_are_keyed_and_removed_one_at_a_time() {
    assert_eq!(
        action_key("abc234", WatchAction::Unblock),
        "wt.watch:abc234:unblock"
    );
    assert_eq!(
        action_key("abc234", WatchAction::Notify),
        "wt.watch:abc234:notify"
    );
    assert_eq!(
        action_key("abc234", WatchAction::Wake),
        "wt.watch:abc234:wake"
    );
    let mut then = WatchActions {
        unblock: Some("gate".into()),
        wake: Some("go".into()),
        ..WatchActions::default()
    };
    assert!(!then.notify_only() && !then.is_empty());
    assert_eq!(then.take(WatchAction::Unblock).as_deref(), Some("gate"));
    assert_eq!(then.take(WatchAction::Unblock), None);
    assert_eq!(then.get(WatchAction::Wake), Some("go"));
    then.take(WatchAction::Wake);
    assert!(then.is_empty());
}

#[test]
fn civil_dates_invert_across_eras_and_leap_days() {
    for (year, month, day) in [
        (1970, 1, 1),
        (2000, 2, 29),
        (2026, 11, 24),
        (2026, 12, 31),
        (1969, 12, 31),
        (2100, 3, 1),
    ] {
        assert_eq!(
            civil_from_days(days_from_civil(year, month, day)),
            (year, month, day)
        );
    }
}

/// The engine remembers a row's landing commit only while it runs. A watch
/// that carries the commit goes on after a restart between the landing and
/// the worktree's removal.
#[test]
fn a_stamped_landing_commit_outlives_the_process_that_saw_the_landing() {
    let mut tells = watch(&["merged", "production+1h"]);
    assert!(tells.follows_own_landing());
    assert!(!watch(&["merged", "green"]).follows_own_landing());
    let stamp = WatchStamps {
        landing_commit: Some("0123abcdef".into()),
        ..WatchStamps::new(&tells.id)
    };
    assert!(!stamp.is_empty());
    assert!(stamp.apply(&mut tells).changed);
    assert!(!stamp.apply(&mut tells).changed);
    assert_eq!(tells.landing_commit(), Some("0123abcdef"));

    // Through the store and back, as a new process reads it.
    let restarted: Watch = serde_json::from_value(serde_json::to_value(&tells).unwrap()).unwrap();
    let unknown = OwnerRemoval {
        landed: true,
        landing_commit: None,
    };
    let kept = restarted.without_owner(unknown, NOON).unwrap();
    assert_eq!(
        kept.when[1].cond,
        Condition::Commit {
            oid: "0123abcdef".into(),
            stage: VerifyStage::Production
        }
    );
    // What this process read itself is newer than the stamp.
    let seen = OwnerRemoval {
        landed: true,
        landing_commit: Some("fedcba9876"),
    };
    assert_eq!(
        restarted.without_owner(seen, NOON).unwrap().when[1].cond,
        Condition::Commit {
            oid: "fedcba9876".into(),
            stage: VerifyStage::Production
        }
    );
    assert_eq!(
        watch(&["production"]).without_owner(unknown, NOON),
        Err("its worktree was removed and no landing commit is known".into())
    );
}

#[test]
fn an_evaluation_is_recorded_at_first_and_then_hourly() {
    let mut pending = watch(&["merged"]);
    assert_eq!(pending.evaluated_at_ms(), None);
    let at = NOON + 5 * MINUTE_MS + 30 * SECOND_MS;
    let minute = NOON + 5 * MINUTE_MS;
    assert_eq!(WatchStamps::evaluation_due(&pending, at), Some(minute));
    let stamp = WatchStamps {
        evaluated_at_ms: Some(minute),
        ..WatchStamps::new(&pending.id)
    };
    assert!(stamp.apply(&mut pending).changed);
    assert_eq!(pending.evaluated_at_ms(), Some(minute));
    // Not again every minute: each stamp rewrites the shared state.
    assert_eq!(
        WatchStamps::evaluation_due(&pending, at + 30 * SECOND_MS),
        None
    );
    assert_eq!(
        WatchStamps::evaluation_due(&pending, minute + HOUR_MS + 59 * SECOND_MS),
        None
    );
    assert_eq!(
        WatchStamps::evaluation_due(&pending, minute + HOUR_MS + MINUTE_MS),
        Some(minute + HOUR_MS + MINUTE_MS)
    );
    // An older stamp never moves the record back.
    let older = WatchStamps {
        evaluated_at_ms: Some(NOON),
        ..WatchStamps::new(&pending.id)
    };
    assert!(!older.apply(&mut pending).changed);
    assert!(WatchStamps::new(&pending.id).is_empty());
}

#[test]
fn a_pull_requests_progress_reads_as_states_of_its_worktree() {
    for (arg, state, words) in [
        ("settled", RowState::Settled, "PR checks settled"),
        (
            "reviewed",
            RowState::Reviewed,
            "bot review of the current head done",
        ),
        ("evicted", RowState::Evicted, "merge dropped"),
    ] {
        assert_eq!(cond(arg).cond, Condition::Own(state));
        assert_eq!(cond(arg).describe(UTC), words);
        assert_eq!(cond(arg).to_arg(0), arg);
        assert_eq!(
            cond(&format!("wt:two:{arg}")).cond,
            Condition::Worktree {
                slug: "two".into(),
                state
            }
        );
    }
    assert_eq!(cond("wt:two:settled").describe(UTC), "two checks settled");
    assert_eq!(cond("settled+5m").describe(UTC), "PR checks settled for 5m");
    assert!(problem("pr:7:settled").contains("a pull request can be"));

    let mut watch = watch(&["settled", "reviewed", "evicted"]);
    // No read of the pull request yet: unknown, never "not yet".
    let unread = pass(&mut watch, &own_row(|_| {}), NOON);
    assert!(
        unread
            .truths
            .iter()
            .all(|truth| matches!(truth, Truth::Unknown(_)))
    );
    let running = own_row(|row| {
        row.settled = Some(true);
        row.reviewed = Some(false);
        row.evicted = Some(false);
    });
    assert_eq!(
        pass(&mut watch, &running, NOON).truths,
        [Truth::Holds, Truth::NotYet, Truth::NotYet]
    );
    // A push starts the checks again: what held stops holding.
    let pushed = own_row(|row| {
        row.settled = Some(false);
        row.reviewed = Some(true);
        row.evicted = Some(true);
    });
    assert_eq!(
        pass(&mut watch, &pushed, NOON + 1).truths,
        [Truth::NotYet, Truth::Holds, Truth::Holds]
    );
    assert!(!watch.is_met());
}

/// A worktree merged and removed before anything judged the watch: its
/// removal record answers for it, and a stage follows its landing commit.
#[test]
fn a_removed_worktree_answers_from_its_removal_record() {
    let removed = |merged: bool, production: bool| {
        let mut facts = WatchFacts::default();
        facts.rows.insert(
            "two".into(),
            RowFacts {
                merged: Some(merged),
                production: production.then_some(true),
                ..RowFacts::default()
            },
        );
        facts
    };
    let mut landed = removed(true, false);
    let mut merged = watch(&["wt:two:merged"]);
    assert_eq!(pass(&mut merged, &landed, NOON).truths, [Truth::Holds]);
    assert!(merged.is_met());

    // A stage with no landing commit on the record cannot be followed.
    let mut staged = watch(&["wt:two:staging"]);
    assert!(matches!(
        &pass(&mut staged, &landed, NOON).truths[..],
        [Truth::Unknown(reason)] if reason.contains("commit that landed it was not recorded")
    ));
    assert!(never_holds(&staged.when[0], &landed).is_some());
    // With one, the condition becomes that commit reaching the stage.
    landed
        .removed_landings
        .insert("two".into(), "c0ffee1".into());
    assert_eq!(
        pass(&mut staged, &landed, NOON).truths,
        [Truth::Unknown(
            "not known whether c0ffee1 reached staging".into()
        )]
    );
    assert_eq!(
        staged.when[0].cond,
        Condition::Commit {
            oid: "c0ffee1".into(),
            stage: VerifyStage::Staging
        }
    );
    landed
        .commits
        .insert(("c0ffee1".into(), VerifyStage::Staging), true);
    assert_eq!(pass(&mut staged, &landed, NOON).truths, [Truth::Holds]);
    // The record itself may already say production.
    let mut shipped = watch(&["wt:two:production"]);
    assert_eq!(
        pass(&mut shipped, &removed(true, true), NOON).truths,
        [Truth::Holds]
    );

    // Removed without merging: never merged, and the reason says why.
    for arg in ["wt:two:merged", "wt:two:production"] {
        let mut waiting = watch(&[arg]);
        let unlanded = removed(false, false);
        let truths = pass(&mut waiting, &unlanded, NOON).truths;
        assert!(
            matches!(&truths[..], [Truth::Unknown(reason)] if reason.contains("removed without merging")),
            "{arg}: {truths:?}"
        );
        assert!(never_holds(&waiting.when[0], &unlanded).is_some());
        assert!(!waiting.is_met());
    }
    // Nothing but a landing outlives the worktree.
    let ready = cond("wt:two:ready");
    assert_eq!(
        never_holds(&ready, &removed(true, false)).as_deref(),
        Some("two was removed and can no longer become ready")
    );
    assert_eq!(
        never_holds(&cond("wt:two:merged"), &removed(true, false)),
        None
    );

    // A later worktree with the slug is a live row: the old record is not
    // asked, and a slug with no record is only unknown.
    let mut reborn = WatchFacts::default();
    reborn.rows.insert(
        "two".into(),
        RowFacts {
            exists: true,
            merged: Some(false),
            ..RowFacts::default()
        },
    );
    let mut again = watch(&["wt:two:merged"]);
    assert_eq!(pass(&mut again, &reborn, NOON).truths, [Truth::NotYet]);
    assert_eq!(never_holds(&again.when[0], &reborn), None);
    let mut gone = WatchFacts::default();
    gone.rows.insert("two".into(), RowFacts::default());
    assert!(matches!(
        &pass(&mut again, &gone, NOON).truths[..],
        [Truth::Unknown(reason)] if reason == "not known whether two is merged"
    ));
    assert_eq!(never_holds(&again.when[0], &gone), None);
}
