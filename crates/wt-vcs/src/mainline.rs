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
/// creation only moved the branch to a commit made elsewhere, and so did a
/// rebase that replayed nothing.
pub fn committed_on_branch(reflog: &str, head: &str) -> bool {
    reflog.lines().any(|line| {
        line.split_once(' ').is_some_and(|(sha, subject)| {
            sha == head
                && !rebase_replayed_nothing(sha, subject)
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

/// A rebase that finished on the very commit it rebased onto: the branch had
/// nothing to replay (it was empty, or all of it was already upstream), so
/// the rebase only moved it to a base commit. Git writes the target as
/// `onto <sha>`, abbreviated or full depending on how it was named.
fn rebase_replayed_nothing(sha: &str, subject: &str) -> bool {
    if !subject.starts_with("rebase") {
        return false;
    }
    subject
        .rsplit_once(" onto ")
        .map(|(_, onto)| onto.trim())
        .is_some_and(|onto| !onto.is_empty() && (sha.starts_with(onto) || onto.starts_with(sha)))
}

/// Landing by containment: `head` is in the base and is either off the
/// base's own line (it was merged in) or a commit this branch made itself
/// (fast-forwarded in). A base snapshot the branch only moved to is not.
pub fn contained_head_landed(mainline: &str, reflog: &str, head: &str) -> bool {
    !on_first_parent_line(mainline, head) || committed_on_branch(reflog, head)
}

/// `git cherry <base> <head>` arguments: every commit of `head` the base
/// does not contain, marked `-` when the base holds a commit with the same
/// patch and `+` when it does not.
pub fn cherry_args(base: &str, head: &str) -> Vec<String> {
    vec!["cherry".into(), base.into(), head.into()]
}

/// The commits `git cherry` found in no form on the other side.
pub fn cherry_unmatched(cherry: &str) -> u32 {
    let unmatched = cherry
        .lines()
        .filter(|line| !line.starts_with("- "))
        .count();
    u32::try_from(unmatched).unwrap_or(u32::MAX)
}

/// Whether the branch has commits beyond the base and the base holds the
/// patch of every one: it was rebased after its work merged. A squash merge
/// never reads so, because a squashed patch matches none of its commits.
pub fn cherry_all_in_base(cherry: &str) -> bool {
    cherry.lines().next().is_some() && cherry_unmatched(cherry) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_branch_whose_every_commit_is_matched_reads_as_in_the_base() {
        assert!(cherry_all_in_base("- a\n- b\n"));
        assert!(!cherry_all_in_base(""));
        assert!(!cherry_all_in_base("- a\n+ b\n"));
        assert_eq!(cherry_unmatched("- a\n+ b\n+ c\n"), 2);
        // An unreadable line is never a match.
        assert_eq!(cherry_unmatched("garbage\n"), 1);
    }

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
        assert!(committed_on_branch(
            "h6 rebase (finish): returning to refs/heads/x\n",
            "h6"
        ));
        // A snapshot the branch fast-forwarded to is not landed work; its own
        // fast-forwarded commit and a merged-in commit are.
        assert!(!contained_head_landed("", reflog, "h3"));
        assert!(contained_head_landed("", reflog, "h2"));
        assert!(contained_head_landed("m b h9\n", "", "h9"));
    }

    /// An empty branch rebased onto trunk sits on a trunk commit it never
    /// made; calling that landed let cleanup and branch deletion act on a
    /// worktree whose agent had not committed yet.
    #[test]
    fn a_rebase_that_replayed_nothing_is_a_move_not_own_work() {
        let onto = "affe40e1359e0c8f6a1d2b3c4d5e6f708192a3b4";
        let full = format!(
            "{onto} rebase (finish): refs/heads/feature onto {onto}\n{onto} branch: Created from origin/main\n"
        );
        assert!(!committed_on_branch(&full, onto));
        assert!(!contained_head_landed("", &full, onto));
        let abbreviated = format!("{onto} rebase (finish): refs/heads/feature onto affe40e135\n");
        assert!(!committed_on_branch(&abbreviated, onto));
        let short_head = "affe40e135 rebase (finish): refs/heads/feature onto affe40e1359e0c8f\n";
        assert!(!committed_on_branch(short_head, "affe40e135"));

        // Replayed commits end on a new hash, which is the branch's own.
        let real = format!(
            "b00c1e5d7f9a1b3c5d7e9f0a2b4c6d8e0f1a3b5c rebase (finish): refs/heads/feature onto {onto}\n"
        );
        assert!(committed_on_branch(
            &real,
            "b00c1e5d7f9a1b3c5d7e9f0a2b4c6d8e0f1a3b5c"
        ));
        // A later commit on that same base commit is still own work.
        let then_committed = format!("c0ffee commit: work\n{full}");
        assert!(committed_on_branch(&then_committed, "c0ffee"));
        // Merged in rather than fast-forwarded: off the base's own line.
        assert!(contained_head_landed("m b h9\n", &full, "h9"));
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
