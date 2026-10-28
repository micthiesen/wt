//! Whether a commit is a snapshot of a base branch rather than work of its
//! own. A fork anchor can lag behind a HEAD that only advanced along the base
//! (a fresh worktree created on a newer base tip, or re-anchored to an older
//! merge), so counting `anchor..HEAD` would call base commits its own work.
//! A commit on the base's first-parent line is one the base itself sat at; a
//! branch merged into the base is the second parent of its merge instead.

/// The longest first-parent walk taken. A longer one answers "not on the
/// line", leaving the caller's other landing evidence in charge.
pub const MAINLINE_WALK: usize = 2000;

/// `git rev-list` arguments whose output [`on_first_parent_line`] reads:
/// the base's first-parent commits newer than `head`, each with its parents.
pub fn mainline_args(base: &str, head: &str) -> Vec<String> {
    vec![
        "rev-list".into(),
        "--first-parent".into(),
        "--parents".into(),
        "-n".into(),
        MAINLINE_WALK.to_string(),
        base.into(),
        format!("^{head}"),
    ]
}

/// Read [`mainline_args`] output, given that `head` is already known to be an
/// ancestor of the base. Empty output means the base is at `head`; otherwise
/// the oldest listed commit must have `head` as its first parent.
pub fn on_first_parent_line(rev_list_parents: &str, head: &str) -> bool {
    let lines: Vec<&str> = rev_list_parents.lines().collect();
    match lines.last() {
        None => true,
        Some(_) if lines.len() >= MAINLINE_WALK => false,
        Some(oldest) => oldest.split_whitespace().nth(1) == Some(head),
    }
}

/// `git reflog` arguments whose output [`committed_on_branch`] reads: each
/// update of the branch as `<sha> <subject>`.
pub fn branch_reflog_args(branch: &str) -> Vec<String> {
    vec![
        "reflog".into(),
        "show".into(),
        "--format=%H %gs".into(),
        format!("refs/heads/{branch}"),
        "--".into(),
    ]
}

/// Whether the branch itself produced `head`: an update that wrote it was a
/// commit, a rebase or a cherry-pick. A fast-forward, reset, pull or branch
/// creation only moved the branch to a commit made elsewhere.
pub fn committed_on_branch(reflog: &str, head: &str) -> bool {
    reflog.lines().any(|line| {
        line.split_once(' ').is_some_and(|(sha, subject)| {
            sha == head
                && ["commit", "rebase", "cherry-pick", "revert", "am"]
                    .iter()
                    .any(|kind| {
                        subject
                            .strip_prefix(kind)
                            .is_some_and(|rest| rest.starts_with([':', ' ']))
                    })
        })
    })
}

/// Landing by containment: `head` is in the base and is either off the
/// base's own line (it was merged in) or a commit this branch made itself
/// (fast-forwarded in). A base snapshot the branch only moved to is not.
pub fn contained_head_landed(mainline: &str, reflog: &str, head: &str) -> bool {
    !on_first_parent_line(mainline, head) || committed_on_branch(reflog, head)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_branch_s_own_commit_counts_as_its_work() {
        let reflog = "h3 merge origin/main: Fast-forward\nh2 commit: own work\nh1 branch: Created from origin/main\n";
        assert!(committed_on_branch(reflog, "h2"));
        assert!(!committed_on_branch(reflog, "h3"));
        assert!(!committed_on_branch(reflog, "h1"));
        assert!(committed_on_branch(
            "h4 rebase (finish): refs/heads/x onto b\n",
            "h4"
        ));
        assert!(committed_on_branch("h5 commit (amend): fix\n", "h5"));
        // A snapshot the branch fast-forwarded to is not landed work; its own
        // fast-forwarded commit and a merged-in commit are.
        assert!(!contained_head_landed("", reflog, "h3"));
        assert!(contained_head_landed("", reflog, "h2"));
        assert!(contained_head_landed("m b h9\n", "", "h9"));
    }

    #[test]
    fn a_base_snapshot_is_on_the_line_and_a_merged_branch_is_not() {
        // The base at HEAD exactly: nothing newer on its line.
        assert!(on_first_parent_line("", "h"));
        // The base moved on from HEAD: c's first parent is HEAD.
        assert!(on_first_parent_line("d c\nc h\n", "h"));
        // HEAD was merged in: the merge's first parent is the base, HEAD is
        // only its second parent.
        assert!(!on_first_parent_line("m b h\n", "h"));
        // A walk that hit the bound proves nothing.
        let long = "x y\n".repeat(MAINLINE_WALK);
        assert!(!on_first_parent_line(&long, "y"));
    }
}
