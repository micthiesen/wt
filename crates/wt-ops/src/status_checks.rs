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

/// Give `next`, a status about to replace `previous`, its merge gate. A gate
/// is a prerequisite of merging, not a claim of one assertion: a status
/// change that does not mention it keeps it, and with it the watch that
/// clears it. `asked` replaces it. Verified and dropped end the work the
/// gate held back, so neither carries one. Clearing a gate is its own write.
pub fn assert_gate(
    next: &mut WorkStatusRecord,
    previous: Option<&WorkStatusRecord>,
    asked: Option<String>,
) {
    next.blocked_on = if next.is_terminal() {
        None
    } else {
        asked.or_else(|| previous.and_then(|previous| previous.blocked_on.clone()))
    };
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
}
