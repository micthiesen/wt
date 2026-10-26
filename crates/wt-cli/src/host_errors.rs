//! Errors a host request ends with that are not plain failures: a notice
//! (nothing to do) and a stale refusal (the state a menu showed has changed),
//! and how every host maps an error to its reply.

use wt_tui::UiReply;

/// A key with nothing to act on (no PR, already at the edge). It answers
/// with a toast, like a success, and is not logged as a failed action.
#[derive(Debug)]
pub struct Notice(pub String);

impl std::fmt::Display for Notice {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for Notice {}

pub fn notice(message: impl Into<String>) -> anyhow::Error {
    Notice(message.into()).into()
}

/// A commit refused because the state its menu showed has changed: a PR
/// moved, a live slot was replaced, a status or removal hazard changed.
/// Nothing was written. The reply tells the model to reopen the menu from
/// its newer board instead of applying the old view.
#[derive(Debug)]
pub struct Stale(pub String);

impl std::fmt::Display for Stale {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for Stale {}

/// Refuse a commit whose witness no longer matches the host's state.
pub fn stale(message: impl Into<String>) -> anyhow::Error {
    Stale(message.into()).into()
}

/// The reply for a failed host request. Every host, local or remote, maps
/// errors here so a stale refusal keeps its meaning across SSH.
pub fn error_reply(error: &anyhow::Error) -> UiReply {
    if error.chain().any(|cause| cause.is::<Stale>()) {
        UiReply::stale(format!("Changed since you looked: {error:#}"))
    } else {
        UiReply::failure(format!("{error:#}"))
    }
}
