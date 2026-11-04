//! Who set a worktree's title, and which setter may replace it.
//!
//! A title is only ever set deliberately. Each stored title records its
//! source; a lower source never overwrites a higher one, and a user rename is
//! final. Agents may retitle as often as the outcome changes. Only starting a
//! new PR in the worktree (`wt next`) replaces a person's title, because that
//! title names work that has landed.

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
    /// A person: the TUI `t` rename, or creating from the TUI.
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
        current.is_none_or(|current| self >= current)
    }
}

/// Capitalize a title's first word when it is plain lowercase. Agents and
/// trackers often pass words shaped like a slug, which read inconsistently
/// beside PR titles. Anything else in first place may be an identifier with
/// its own casing (`iOS`, `wt.toml`, `v2`), so it stays as written.
pub fn sentence_case(title: &str) -> String {
    let title = title.trim();
    let first = title.split_whitespace().next().unwrap_or_default();
    let plain = first.chars().next().is_some_and(char::is_lowercase)
        && first
            .chars()
            .all(|c| c.is_lowercase() || matches!(c, '-' | '\''));
    if !plain {
        return title.to_owned();
    }
    let mut chars = title.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

/// A slug's stored title and its source. Titles saved before sources were
/// recorded were set with the TUI rename or an explicit request, so they read
/// as the user's. A person's casing is kept; every other title reads in
/// sentence case, including those saved before that rule existed.
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
    let title = if source == TitleSource::User {
        title.to_owned()
    } else {
        sentence_case(title)
    };
    Some((title, source))
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
    fn only_a_higher_or_equal_source_replaces_a_title() {
        let all = [Fallback, Issue, Agent, User];
        for new in all {
            assert!(new.may_replace(None), "{new:?} sets an untitled worktree");
            for current in all {
                let expected = new >= current;
                assert_eq!(
                    new.may_replace(Some(current)),
                    expected,
                    "{new:?} over {current:?}"
                );
            }
        }
        // An agent retitles freely, but never over a person.
        assert!(Agent.may_replace(Some(Issue)) && Agent.may_replace(Some(Agent)));
        assert!(!Agent.may_replace(Some(User)) && User.may_replace(Some(User)));
    }

    #[test]
    fn plain_lowercase_titles_read_in_sentence_case_and_identifiers_keep_theirs() {
        for (raw, shown) in [
            ("better background effects", "Better background effects"),
            ("  follow-up on blur ", "Follow-up on blur"),
            ("Already cased", "Already cased"),
            ("iOS share sheet", "iOS share sheet"),
            ("wt.toml loading", "wt.toml loading"),
            ("v2 migration", "v2 migration"),
            ("ship: calendar", "ship: calendar"),
            ("", ""),
        ] {
            assert_eq!(super::sentence_case(raw), shown);
        }
    }

    #[test]
    fn stored_titles_read_in_sentence_case_except_a_persons() {
        let state = serde_json::json!({"slugs": {
            "agent": {"manualTitle": "chat name search", "titleSource": "agent"},
            "issue": {"manualTitle": "calendar renders twice", "titleSource": "issue"},
            "person": {"manualTitle": "chat name search", "titleSource": "user"},
        }});
        let title = |slug| super::stored_title(&state, slug).unwrap().0;
        assert_eq!(title("agent"), "Chat name search");
        assert_eq!(title("issue"), "Calendar renders twice");
        assert_eq!(title("person"), "chat name search");
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
