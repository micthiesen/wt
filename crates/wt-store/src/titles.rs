//! Who set a worktree's title, and which setter may replace it.
//!
//! A title is only ever set deliberately. Each stored title records its
//! source; a lower source never overwrites a higher one, and a user rename is
//! final. The agent can replace an issue or fallback title, which also makes
//! its rename a one-time step: afterwards the title is the agent's own.

use serde_json::Value;

/// Title sources in ascending precedence.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum TitleSource {
    /// The issue id alone, when the tracker could not supply a title.
    Fallback,
    /// The tracker's title for the worktree's issue.
    Issue,
    /// The worktree's agent, or an agent creating it.
    Agent,
    /// A person: the TUI `t` rename or `wt new` typed at a terminal.
    User,
}

impl TitleSource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Fallback => "fallback",
            Self::Issue => "issue",
            Self::Agent => "agent",
            Self::User => "user",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "fallback" => Some(Self::Fallback),
            "issue" => Some(Self::Issue),
            "agent" => Some(Self::Agent),
            "user" => Some(Self::User),
            _ => None,
        }
    }

    /// Whether a title from `self` may replace one from `current`.
    pub fn may_replace(self, current: Option<Self>) -> bool {
        self == Self::User || current.is_none_or(|current| self > current)
    }
}

/// A slug's stored title and its source. Titles saved before sources were
/// recorded were set with the TUI rename or an explicit request, so they read
/// as the user's.
pub fn stored_title(state: &Value, slug: &str) -> Option<(String, TitleSource)> {
    let entry = &state["slugs"][slug];
    let title = entry["manualTitle"]
        .as_str()
        .map(str::trim)
        .filter(|title| !title.is_empty())?;
    let source = entry["titleSource"]
        .as_str()
        .and_then(TitleSource::parse)
        .unwrap_or(TitleSource::User);
    Some((title.to_owned(), source))
}

/// The outcome of a title write.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TitleWrite {
    Saved,
    /// The title changed since the expected revision; nothing was written.
    Stale,
    /// A title from a higher source (or the user's) is kept.
    Outranked(TitleSource),
}

#[cfg(test)]
mod tests {
    use super::TitleSource::{self, *};

    #[test]
    fn a_lower_source_never_replaces_a_higher_one_and_the_user_always_can() {
        let all = [Fallback, Issue, Agent, User];
        for new in all {
            assert!(new.may_replace(None), "{new:?} sets an untitled worktree");
            for current in all {
                let expected = new == User || new > current;
                assert_eq!(
                    new.may_replace(Some(current)),
                    expected,
                    "{new:?} over {current:?}"
                );
            }
        }
        // The agent's one rename: an issue or fallback title, then never again.
        assert!(Agent.may_replace(Some(Issue)) && Agent.may_replace(Some(Fallback)));
        assert!(!Agent.may_replace(Some(Agent)) && !Agent.may_replace(Some(User)));
    }

    #[test]
    fn sources_round_trip_and_unrecorded_titles_read_as_the_users() {
        for source in [Fallback, Issue, Agent, User] {
            assert_eq!(TitleSource::parse(source.as_str()), Some(source));
        }
        let state = serde_json::json!({"slugs": {
            "old": {"manualTitle": "Renamed long ago"},
            "seeded": {"manualTitle": "From the tracker", "titleSource": "issue"},
            "blank": {"manualTitle": "  ", "titleSource": "agent"},
        }});
        assert_eq!(
            super::stored_title(&state, "old"),
            Some(("Renamed long ago".into(), User))
        );
        assert_eq!(
            super::stored_title(&state, "seeded"),
            Some(("From the tracker".into(), Issue))
        );
        assert_eq!(super::stored_title(&state, "blank"), None);
    }
}
