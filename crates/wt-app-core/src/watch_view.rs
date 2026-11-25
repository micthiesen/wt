//! How a stored watch reads to a person: each condition's truth as the
//! engine last recorded it. `wt watch` and the board both show this, so a
//! condition never says one thing in a shell and another on a row.

use wt_core::watch::{Condition, Watch, WatchCondition};

/// A condition's truth as the engine last recorded it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Shown {
    Holds,
    NotYet(Option<String>),
    Unknown(String),
    /// No engine has judged the watch since it was set, so nothing is
    /// recorded to read: not "not yet", which is an answer.
    NotEvaluated,
}

impl Shown {
    pub fn word(&self) -> &'static str {
        match self {
            Self::Holds => "holds",
            Self::NotYet(_) => "not yet",
            Self::Unknown(_) => "unknown",
            Self::NotEvaluated => "not evaluated yet",
        }
    }

    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Holds => None,
            Self::NotYet(reason) => reason.as_deref(),
            Self::Unknown(reason) => Some(reason),
            Self::NotEvaluated => Some("the wt TUI evaluates watches"),
        }
    }
}

/// Read a condition's stored record. The record keeps since when a
/// condition has held and the watch's last problem, not every "unknown", so
/// a condition nothing has reported on reads "not yet" once an engine has
/// judged the watch, and "unknown" while none runs. `idle` says why none
/// does.
pub fn shown(watch: &Watch, cond: &WatchCondition, now_ms: i64, idle: Option<&str>) -> Shown {
    if cond.latched || watch.is_met() {
        return Shown::Holds;
    }
    if let Condition::At { at_ms } = cond.cond {
        return if now_ms >= at_ms {
            Shown::Holds
        } else {
            Shown::NotYet(None)
        };
    }
    if let Some(since) = cond.first_held_at_ms {
        return match cond.delay {
            Some(delay) if now_ms < since.saturating_add(delay.as_ms()) => {
                Shown::NotYet(Some(format!("holding, {delay} not yet over")))
            }
            _ => Shown::Holds,
        };
    }
    if let Some(idle) = idle {
        return Shown::Unknown(idle.to_owned());
    }
    let about_this = |error: &&String| match &cond.cond {
        Condition::Check { .. } => !error.starts_with('#'),
        Condition::Pr { number, .. } => error.starts_with(&format!("#{number} ")),
        _ => false,
    };
    match watch.last_error.as_ref().filter(about_this) {
        Some(error) => Shown::Unknown(error.clone()),
        None if watch.evaluated_at_ms().is_some() => Shown::NotYet(None),
        None => Shown::NotEvaluated,
    }
}

/// The first line of a prompt or a notification text, cut to 80 cells.
pub fn headline(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default();
    if line.chars().count() > 80 {
        format!("{}…", line.chars().take(79).collect::<String>())
    } else {
        line.to_owned()
    }
}
