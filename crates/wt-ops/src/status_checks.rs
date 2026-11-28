//! The checks a status assertion owes. The CLI and the board's status menu
//! both write statuses; this is the one rule for what happens to the
//! post-merge checks when they do.

use wt_core::{Check, Landing, VerifyStage, WorkStatusRecord};
use wt_store::{CheckTargets, ChecksClosed, Store, StoreError};

/// What an assertion says about the check owed in one stage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CheckEdit {
    /// Owe these steps there; a check owed in the other stage is left alone.
    /// `lines` is the text as written, for its line breaks; `None` where
    /// the writer has only the one line.
    Owe {
        stage: VerifyStage,
        steps: String,
        lines: Option<String>,
    },
    /// Rewrite the steps of the check owed there in place: the same check,
    /// with its dates and the work it was left for (`wt next`), as someone
    /// editing the check they were shown means. Other steps given with
    /// [`Self::Owe`] are another check.
    Reword { stage: VerifyStage, steps: String },
    /// Stop owing the check there.
    Drop { stage: VerifyStage },
}

/// Give `next`, a status about to replace `previous`, the checks it owes. A
/// fresh assertion does not discharge a check: every check still owed is
/// carried forward, then the edits apply. Verified and dropped discharge
/// them all.
pub fn assert_checks(
    next: &mut WorkStatusRecord,
    previous: Option<&WorkStatusRecord>,
    edits: impl IntoIterator<Item = CheckEdit>,
) {
    if next.is_terminal() {
        next.clear_checks();
        return;
    }
    if let Some(previous) = previous {
        next.carry_checks(previous);
    }
    for edit in edits {
        match edit {
            CheckEdit::Owe {
                stage,
                steps,
                lines,
            } => next.set_check_lines(stage, steps, lines.as_deref()),
            CheckEdit::Reword { stage, steps } => next.reword_check(stage, steps),
            CheckEdit::Drop { stage } => {
                next.drop_check(stage);
            }
        }
    }
}

use wt_store::GATE_ORIGIN;

/// The state `record`'s gate was asserted in. The record says so only while
/// what it recorded is about the gate now set: a session on a build that
/// does not know the field can set another gate in place and leave the old
/// origin beside it. A gate with no origin of its own has only the record's
/// state to go by, and that state is taken: such a gate found on `todo`
/// reads as a start gate, which every gate on `todo` was until gates began
/// to be carried, and one found on any other state as a merge gate.
fn gate_state(record: &WorkStatusRecord) -> wt_core::WorkState {
    record
        .extra
        .get(GATE_ORIGIN)
        .filter(|origin| {
            origin["gate"].as_str().is_some()
                && origin["gate"].as_str() == record.blocked_on.as_deref()
        })
        .and_then(|origin| serde_json::from_value(origin["state"].clone()).ok())
        .unwrap_or(record.state)
}

/// Put `gate`, asserted in `state`, on `record`, or clear its gate.
fn put_gate(record: &mut WorkStatusRecord, gate: Option<(String, wt_core::WorkState)>) {
    record.extra.remove(GATE_ORIGIN);
    record.blocked_on = gate.map(|(gate, state)| {
        record.extra.insert(
            GATE_ORIGIN.to_owned(),
            serde_json::json!({"state": state, "gate": gate}),
        );
        gate
    });
}

/// Set or clear `record`'s gate as asserted now, in the record's own state.
pub fn set_gate(record: &mut WorkStatusRecord, gate: Option<String>) {
    let state = record.state;
    put_gate(record, gate.map(|gate| (gate, state)));
}

/// Give `next`, a status about to replace `previous`, its gate. A gate on
/// work under way is a prerequisite of merging, not a claim of one
/// assertion: a status change that does not mention it keeps it, and with it
/// the watch that clears it. `asked` replaces it. Verified and dropped end
/// the work the gate held back, so neither carries one.
///
/// A gate asserted on `todo` is a different thing: why the work has not
/// started. Leaving `todo` is the decision that it no longer holds, so it
/// is not carried into another state, where it would read as "do not
/// merge". A merge gate carried through `todo` is still a merge gate: what
/// decides is the state the gate was asserted in, which is carried with it.
pub fn assert_gate(
    next: &mut WorkStatusRecord,
    previous: Option<&WorkStatusRecord>,
    asked: Option<String>,
) {
    if next.is_terminal() {
        return set_gate(next, None);
    }
    if asked.is_some() {
        return set_gate(next, asked);
    }
    let carried = previous
        .filter(|previous| !start_gate_ends(previous, next))
        .and_then(|previous| Some((previous.blocked_on.clone()?, gate_state(previous))));
    put_gate(next, carried);
}

/// Whether `next` starts work that `previous` held back with a gate
/// asserted on `todo`.
pub fn start_gate_ends(previous: &WorkStatusRecord, next: &WorkStatusRecord) -> bool {
    previous.blocked_on.is_some()
        && gate_state(previous) == wt_core::WorkState::Todo
        && next.state != wt_core::WorkState::Todo
        && !next.is_terminal()
}

/// What to tell whoever asserted `next` over `previous` when that cleared a
/// start gate without their saying so: the one notice for the command line
/// and the board.
pub fn start_gate_notice(
    previous: Option<&WorkStatusRecord>,
    next: &WorkStatusRecord,
) -> Option<String> {
    let previous = previous.filter(|previous| start_gate_ends(previous, next))?;
    let gate = previous.blocked_on.as_deref()?;
    next.blocked_on
        .is_none()
        .then(|| format!("start gate cleared (was: {gate})"))
}

/// `verified` asserted for a worktree that owes checks: close every check
/// that is due under `landing`, or the earliest stage's when none is (who
/// asserts it may have confirmed the deploy themselves). The status becomes
/// `verified` only once no check remains; until then state, note and time
/// stay as they were. `wt status verified` and the board's status menu both
/// close through here.
///
/// `admit` is asked about the status as it stands, under the write's own
/// transaction; `None` comes back, with nothing written, when it refuses.
pub fn close_verified(
    store: &mut Store,
    slug: &str,
    landing: Landing,
    now_ms: i64,
    verified: &wt_store::WorkStatusRecord,
    evidence: Option<&str>,
    admit: impl FnOnce(Option<&wt_store::WorkStatusRecord>) -> bool,
) -> Result<Option<ChecksClosed>, StoreError> {
    let targets = CheckTargets::Verified { landing, now_ms };
    store.close_checks_seen(slug, &targets, verified, evidence, admit)
}

fn stage_names(checks: &[(VerifyStage, Check)]) -> String {
    checks
        .iter()
        .map(|(stage, _)| stage.as_str())
        .collect::<Vec<_>>()
        .join(" and ")
}

/// What closing checks did, one line per closed check and one for where
/// the worktree stands now. `kept_state` is the state a live row keeps
/// while a check remains.
pub fn checks_closed_lines(
    slug: &str,
    closed: &[(VerifyStage, Check)],
    remaining: &[(VerifyStage, Check)],
    kept_state: Option<&str>,
    removed: bool,
) -> Vec<String> {
    let row = if removed { " (removed row)" } else { "" };
    let mut lines: Vec<String> = closed
        .iter()
        .map(|(stage, check)| {
            format!(
                "✓ {slug} closed the check in {}{row}: {}",
                stage.as_str(),
                wt_core::watch::headline(&check.steps)
            )
        })
        .collect();
    lines.push(if remaining.is_empty() {
        format!("✓ {slug} verified{row}")
    } else {
        format!(
            "  still owes the check in {}{}",
            stage_names(remaining),
            kept_state
                .map(|state| format!(": the status stays {state} until it is closed"))
                .unwrap_or_default()
        )
    });
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use wt_core::WorkState;

    fn stages(record: &WorkStatusRecord) -> Vec<(VerifyStage, String)> {
        record
            .open_checks()
            .into_iter()
            .map(|(stage, check)| (stage, check.steps))
            .collect()
    }

    #[test]
    fn an_assertion_carries_every_check_and_edits_one_stage_at_a_time() {
        let mut previous = WorkStatusRecord::new(WorkState::Ready, "t0");
        assert_checks(
            &mut previous,
            None,
            [
                CheckEdit::Owe {
                    stage: VerifyStage::Staging,
                    steps: "1. staging".into(),
                    lines: None,
                },
                CheckEdit::Owe {
                    stage: VerifyStage::Production,
                    steps: "1. production".into(),
                    lines: None,
                },
            ],
        );
        // A plain assertion keeps both.
        let mut working = WorkStatusRecord::new(WorkState::Working, "t1");
        assert_checks(&mut working, Some(&previous), []);
        assert_eq!(stages(&working), stages(&previous));
        // New steps for one stage leave the other; a drop removes only its own.
        let mut edited = WorkStatusRecord::new(WorkState::Review, "t2");
        assert_checks(
            &mut edited,
            Some(&working),
            [CheckEdit::Owe {
                stage: VerifyStage::Production,
                steps: "2. production".into(),
                lines: None,
            }],
        );
        assert_eq!(
            stages(&edited),
            [
                (VerifyStage::Staging, "1. staging".to_owned()),
                (VerifyStage::Production, "2. production".to_owned()),
            ]
        );
        let mut dropped = WorkStatusRecord::new(WorkState::Review, "t3");
        assert_checks(
            &mut dropped,
            Some(&edited),
            [CheckEdit::Drop {
                stage: VerifyStage::Staging,
            }],
        );
        assert_eq!(
            stages(&dropped),
            [(VerifyStage::Production, "2. production".to_owned())]
        );
        // The legacy fields mirror the first check still owed.
        assert_eq!(dropped.verify_in, Some(VerifyStage::Production));
        // Verified and dropped discharge everything.
        for state in [WorkState::Verified, WorkState::Dropped] {
            let mut terminal = WorkStatusRecord::new(state, "t4");
            assert_checks(&mut terminal, Some(&edited), []);
            assert!(stages(&terminal).is_empty());
        }
    }

    fn asserted(
        state: WorkState,
        previous: Option<&WorkStatusRecord>,
        gate: Option<&str>,
    ) -> WorkStatusRecord {
        let mut next = WorkStatusRecord::new(state, "t");
        assert_gate(&mut next, previous, gate.map(str::to_owned));
        // As stored and read back: the origin must survive the round trip.
        wt_core::parse_work_status(&serde_json::to_value(&next).unwrap()).unwrap()
    }

    #[test]
    fn starting_clears_a_gate_asserted_on_todo_and_never_a_merge_gate() {
        // A start gate: kept while not started, cleared by starting.
        let todo = asserted(WorkState::Todo, None, Some("secrets land"));
        let still = asserted(WorkState::Todo, Some(&todo), None);
        assert_eq!(still.blocked_on.as_deref(), Some("secrets land"));
        let working = asserted(WorkState::Working, Some(&still), None);
        assert_eq!(working.blocked_on, None);
        assert_eq!(
            start_gate_notice(Some(&still), &working).as_deref(),
            Some("start gate cleared (was: secrets land)")
        );
        assert!(!working.extra.contains_key(GATE_ORIGIN));

        // A merge gate set on ready, carried through todo, is not one.
        let ready = asserted(WorkState::Ready, None, Some("the release"));
        let parked = asserted(WorkState::Todo, Some(&ready), None);
        assert_eq!(parked.blocked_on.as_deref(), Some("the release"));
        let again = asserted(WorkState::Todo, Some(&parked), None);
        let resumed = asserted(WorkState::Working, Some(&again), None);
        assert_eq!(resumed.blocked_on.as_deref(), Some("the release"));
        assert_eq!(start_gate_notice(Some(&again), &resumed), None);
        let ready = asserted(WorkState::Ready, Some(&resumed), None);
        assert_eq!(ready.blocked_on.as_deref(), Some("the release"));

        // A new gate is asserted where it is set, whatever it replaces.
        let regated = asserted(WorkState::Todo, Some(&ready), Some("a decision"));
        assert_eq!(
            asserted(WorkState::Working, Some(&regated), None).blocked_on,
            None
        );
        let merge = asserted(WorkState::Ready, Some(&todo), Some("the release"));
        assert_eq!(start_gate_notice(Some(&todo), &merge), None);
        assert_eq!(
            asserted(WorkState::Review, Some(&merge), None)
                .blocked_on
                .as_deref(),
            Some("the release")
        );

        // An ending clears either kind, and that is not "starting".
        let dropped = asserted(WorkState::Dropped, Some(&todo), None);
        assert_eq!(dropped.blocked_on, None);
        assert_eq!(start_gate_notice(Some(&todo), &dropped), None);
    }

    #[test]
    fn a_gate_with_no_recorded_origin_is_read_by_the_state_it_is_found_on() {
        let mut old_todo = WorkStatusRecord::new(WorkState::Todo, "t");
        old_todo.blocked_on = Some("secrets land".into());
        assert_eq!(
            asserted(WorkState::Working, Some(&old_todo), None).blocked_on,
            None
        );
        // Found on ready it is a merge gate, and stays one through todo.
        let mut old_ready = WorkStatusRecord::new(WorkState::Ready, "t");
        old_ready.blocked_on = Some("the release".into());
        let parked = asserted(WorkState::Todo, Some(&old_ready), None);
        assert_eq!(
            asserted(WorkState::Working, Some(&parked), None)
                .blocked_on
                .as_deref(),
            Some("the release")
        );
        // Amending a gate in place records where it was asserted.
        let mut amended = old_ready.clone();
        set_gate(&mut amended, Some("another".into()));
        assert_eq!(gate_state(&amended), WorkState::Ready);
        set_gate(&mut amended, None);
        assert!(amended.blocked_on.is_none() && !amended.extra.contains_key(GATE_ORIGIN));
    }

    /// Sessions keep the build that started them and share the state: one
    /// that does not know the origin edits `blockedOn` and carries the rest
    /// of the record along.
    #[test]
    fn an_origin_left_by_a_build_that_does_not_know_it_is_not_this_gates() {
        // A merge gate parked on todo, then re-gated there in place by an
        // older build: the origin still says "ready", of the gate before.
        let ready = asserted(WorkState::Ready, None, Some("the release"));
        let mut parked = asserted(WorkState::Todo, Some(&ready), None);
        assert_eq!(gate_state(&parked), WorkState::Ready);
        parked.blocked_on = Some("secrets land".into());
        assert_eq!(gate_state(&parked), WorkState::Todo);
        let working = asserted(WorkState::Working, Some(&parked), None);
        assert_eq!(working.blocked_on, None);
        assert_eq!(
            start_gate_notice(Some(&parked), &working).as_deref(),
            Some("start gate cleared (was: secrets land)")
        );
        // Cleared by such a build and set again later, even with the old
        // text: the leftover is dropped by the write that sets the new one.
        let mut cleared = asserted(WorkState::Todo, Some(&ready), None);
        cleared.blocked_on = None;
        assert_eq!(
            asserted(WorkState::Todo, Some(&cleared), None)
                .extra
                .get(GATE_ORIGIN),
            None
        );
        let again = asserted(WorkState::Todo, Some(&cleared), Some("the release"));
        assert_eq!(gate_state(&again), WorkState::Todo);
        // A value in an earlier shape (the bare state) names no gate.
        let mut bare = WorkStatusRecord::new(WorkState::Todo, "t");
        bare.blocked_on = Some("secrets land".into());
        bare.extra
            .insert(GATE_ORIGIN.to_owned(), serde_json::json!("ready"));
        assert_eq!(gate_state(&bare), WorkState::Todo);
    }
}
