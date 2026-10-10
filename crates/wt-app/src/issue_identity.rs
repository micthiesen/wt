//! One issue identity for links, actions, status readers and CLI overrides.
use std::sync::LazyLock;

static SLUG_ID: LazyLock<regex::Regex> =
    LazyLock::new(|| regex::Regex::new(r"(?i)([a-z]+-\d+)(?:-|$)").expect("constant issue regex"));

pub fn resolve(slug: &str, explicit: Option<&str>) -> Option<String> {
    if let Some(explicit) = explicit {
        return (!explicit.trim().is_empty()).then(|| explicit.trim().to_ascii_uppercase());
    }
    SLUG_ID
        .captures(slug)?
        .get(1)
        .map(|id| id.as_str().to_ascii_uppercase())
}

/// A readable title from a slug: a leading issue id is dropped (the list
/// shows the id separately), dashes become spaces, and the first letter is
/// capitalized. A slug that is only an id falls back to the id.
pub fn slug_title(slug: &str) -> String {
    let rest = SLUG_ID
        .captures(slug)
        .and_then(|captures| captures.get(0))
        .filter(|matched| matched.start() == 0)
        .map_or(slug, |matched| &slug[matched.end()..]);
    let words = rest.replace('-', " ");
    let words = words.trim();
    let mut chars = words.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => resolve(slug, None).unwrap_or_else(|| slug.to_owned()),
    }
}

pub fn is_tracker(id: &str, prefix: Option<&str>) -> bool {
    let Some((team, number)) = id.split_once('-') else {
        return false;
    };
    !team.is_empty()
        && team.bytes().all(|c| c.is_ascii_alphabetic())
        && !number.is_empty()
        && number.bytes().all(|c| c.is_ascii_digit())
        && !team.eq_ignore_ascii_case("GH")
        && prefix.is_none_or(|prefix| team.eq_ignore_ascii_case(prefix))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn explicit_empty_suppresses_slug_and_prefix_excludes_github() {
        assert_eq!(resolve("eng-123-fix", None).as_deref(), Some("ENG-123"));
        assert_eq!(resolve("fix-eng-123", None).as_deref(), Some("ENG-123"));
        assert_eq!(resolve("eng-123-fix", Some("  ")), None);
        assert_eq!(
            resolve("eng-123-fix", Some("other-2")).as_deref(),
            Some("OTHER-2")
        );
        assert!(!is_tracker("GH-12", None));
        assert!(!is_tracker("ENG-12", Some("OTHER")));
        assert!(is_tracker("ENG-12", Some("eng")));
    }

    #[test]
    fn slug_titles_drop_the_leading_id_and_read_as_text() {
        assert_eq!(slug_title("fresh-task"), "Fresh task");
        assert_eq!(slug_title("eng-123-fix-login"), "Fix login");
        assert_eq!(slug_title("eng-123"), "ENG-123");
        assert_eq!(slug_title("fix-eng-123"), "Fix eng 123");
    }
}
