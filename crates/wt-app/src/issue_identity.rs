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
}
