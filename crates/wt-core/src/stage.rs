use regex::Regex;
use sha2::{Digest, Sha256};

/// Stable deployment identity, compatible with the original computeStage.
/// The configured prefix includes its own delimiter; digest lengths count
/// hexadecimal characters, and issue matching uses the directory slug.
pub fn stage_name(slug: &str, prefix: &str, issue_pattern: Option<&Regex>) -> String {
    let normalized = slug.to_lowercase();
    let digest = format!("{:x}", Sha256::digest(normalized.as_bytes()));
    let issue = issue_pattern
        .and_then(|pattern| pattern.captures(&normalized))
        .and_then(|captures| captures.get(1));
    match issue {
        Some(issue) => format!("{prefix}{}-{}", issue.as_str(), &digest[..6]),
        None => format!("{prefix}{}", &digest[..10]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_stage_identity_preserves_prefix_case_and_hex_lengths() {
        let pattern = Regex::new(r"(?i)^[a-z]+-(\d+)(?:-|$)").unwrap();
        assert_eq!(
            stage_name("one", "fixture-", Some(&pattern)),
            "fixture-7692c3ad35"
        );
        assert_eq!(
            stage_name("ONE", "fixture-", Some(&pattern)),
            "fixture-7692c3ad35"
        );
        assert_eq!(stage_name("one", "raw", None), "raw7692c3ad35");
        let expected = format!(
            "stage-42-{}",
            &format!("{:x}", Sha256::digest(b"eng-42-change"))[..6]
        );
        assert_eq!(
            stage_name("ENG-42-change", "stage-", Some(&pattern)),
            expected
        );
    }
}
