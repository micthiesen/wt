//! Names of the backup branches the stack engine leaves behind.

/// The branch a restack or stack-sync backup preserves the prior tip of, or
/// `None` for any other name, including unrecognized `backup/` branches.
pub fn backup_owner(reference: &str) -> Option<&str> {
    if let Some(rest) = reference.strip_prefix("backup/restack-") {
        let (timestamp, branch) = rest.split_once('-')?;
        return (timestamp.parse::<u128>().is_ok() && !branch.is_empty()).then_some(branch);
    }
    let rest = reference.strip_prefix("backup/stack-sync-")?;
    let suffix = rest.split_once("Z-")?;
    (!suffix.1.is_empty()).then_some(suffix.1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_owner_keeps_dashed_branch_names() {
        assert_eq!(
            backup_owner("backup/restack-1234-feature/one-a"),
            Some("feature/one-a")
        );
        assert_eq!(
            backup_owner("backup/stack-sync-2026-10-01T120000000Z-feature/a"),
            Some("feature/a")
        );
        assert_eq!(backup_owner("backup/other-feature"), None);
    }
}
