//! Which branches a worktree may record as its fork base. The TUI builder
//! offers these choices and the owning host refuses anything else, so both
//! apply the same rule.

use std::collections::{HashMap, HashSet};

/// A worktree that another worktree may be stacked on.
#[derive(Clone, Copy, Debug)]
pub struct ForkBaseCandidate<'a> {
    pub slug: &'a str,
    pub branch: &'a str,
    /// The branch this worktree records as its own base, if any.
    pub base: Option<&'a str>,
    pub is_main: bool,
    pub detached: bool,
    pub archived: bool,
}

/// The worktree whose fork base is being chosen.
#[derive(Clone, Copy, Debug)]
pub struct ForkBaseChooser<'a> {
    pub slug: &'a str,
    pub branch: &'a str,
}

/// The recorded parent of a worktree. Trunk and an empty record both mean
/// no parent.
pub fn recorded_fork_base<'a>(base: Option<&'a str>, trunk: &str) -> Option<&'a str> {
    base.filter(|base| !base.is_empty() && *base != trunk)
}

/// The trunk a worktree finally lands on: follow recorded fork bases through
/// parent worktrees until a base no worktree owns. `parent_base(branch)` is the
/// recorded base of the worktree on `branch` (`Some(None)` for trunk), or
/// `None` when no worktree has that branch.
pub fn stack_trunk(
    own_base: Option<&str>,
    trunk: &str,
    parent_base: impl Fn(&str) -> Option<Option<String>>,
) -> String {
    let normal = |base: &str| base.strip_prefix("origin/").unwrap_or(base).to_owned();
    let mut base = normal(recorded_fork_base(own_base, trunk).unwrap_or(trunk));
    let mut seen = std::collections::BTreeSet::new();
    while base != trunk && seen.insert(base.clone()) {
        match parent_base(&base) {
            Some(next) => {
                base = normal(recorded_fork_base(next.as_deref(), trunk).unwrap_or(trunk))
            }
            None => break,
        }
    }
    base
}

/// Branches `chooser` may record as its fork base, in candidate order.
///
/// Excluded: the chooser itself, main and detached checkouts, archived
/// worktrees, and anything stacked on the chooser directly or transitively,
/// since picking one would close a record cycle. Archived worktrees still
/// contribute their recorded links, so restoring one cannot close a cycle.
/// The recorded base stays eligible even when its worktree is gone, so a
/// menu can show and keep it.
pub fn fork_base_choices(
    trunk: &str,
    chooser: ForkBaseChooser<'_>,
    recorded: Option<&str>,
    candidates: &[ForkBaseCandidate<'_>],
) -> Vec<String> {
    let usable = |candidate: &ForkBaseCandidate<'_>| {
        !candidate.is_main && !candidate.detached && !candidate.branch.is_empty()
    };
    let parent_of = candidates
        .iter()
        .filter(|candidate| usable(candidate))
        .filter_map(|candidate| Some((candidate.branch, candidate.base?)))
        .collect::<HashMap<_, _>>();
    let stacked_on_chooser = |start: &str| {
        let mut seen = HashSet::new();
        let mut next = Some(start);
        while let Some(current) = next {
            if current == chooser.branch {
                return true;
            }
            if !seen.insert(current) {
                return false;
            }
            next = parent_of.get(current).copied();
        }
        false
    };
    let mut choices = Vec::<String>::new();
    for candidate in candidates {
        if candidate.slug != chooser.slug
            && usable(candidate)
            && !candidate.archived
            && !stacked_on_chooser(candidate.branch)
            && !choices.iter().any(|choice| choice == candidate.branch)
        {
            choices.push(candidate.branch.to_owned());
        }
    }
    if let Some(recorded) = recorded_fork_base(recorded, trunk)
        && !choices.iter().any(|choice| choice == recorded)
    {
        choices.insert(0, recorded.to_owned());
    }
    choices
}

/// Whether `base` may be recorded for `chooser`. `None` and trunk return to
/// trunk, which is always allowed. A branch that belongs to a worktree must
/// be one of [`fork_base_choices`], so a record never closes a cycle or
/// stacks on the row itself, an archived, main or detached checkout. A branch
/// with no worktree (a teammate's or an upstream branch) is allowed; the
/// caller still requires the ref to exist.
pub fn fork_base_allowed(
    trunk: &str,
    chooser: ForkBaseChooser<'_>,
    recorded: Option<&str>,
    candidates: &[ForkBaseCandidate<'_>],
    base: Option<&str>,
) -> bool {
    let Some(base) = base.filter(|base| *base != trunk) else {
        return true;
    };
    if base == chooser.branch {
        return false;
    }
    if !candidates.iter().any(|candidate| candidate.branch == base) {
        return true;
    }
    fork_base_choices(trunk, chooser, recorded, candidates)
        .iter()
        .any(|choice| choice == base)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stack_trunk_follows_parent_worktrees_to_the_base_none_of_them_own() {
        let parents = |branch: &str| match branch {
            "m/parent" => Some(Some("m/root".to_owned())),
            "m/root" => Some(None),
            "m/hotfix-parent" => Some(Some("origin/main".to_owned())),
            "m/loop-a" => Some(Some("m/loop-b".to_owned())),
            "m/loop-b" => Some(Some("m/loop-a".to_owned())),
            _ => None,
        };
        assert_eq!(stack_trunk(Some("m/parent"), "staging", parents), "staging");
        assert_eq!(stack_trunk(None, "staging", parents), "staging");
        assert_eq!(stack_trunk(Some("main"), "staging", parents), "main");
        assert_eq!(
            stack_trunk(Some("m/hotfix-parent"), "staging", parents),
            "main"
        );
        assert_eq!(
            stack_trunk(Some("m/loop-a"), "staging", parents),
            "m/loop-a"
        );
    }

    fn candidate<'a>(
        slug: &'a str,
        branch: &'a str,
        base: Option<&'a str>,
    ) -> ForkBaseCandidate<'a> {
        ForkBaseCandidate {
            slug,
            branch,
            base,
            is_main: false,
            detached: false,
            archived: false,
        }
    }

    #[test]
    fn worktree_branches_follow_the_menu_rule_and_others_stay_allowed() {
        let candidates = [
            candidate("one", "feature/one", None),
            candidate("two", "feature/two", Some("feature/one")),
            candidate("three", "feature/three", None),
        ];
        let chooser = ForkBaseChooser {
            slug: "one",
            branch: "feature/one",
        };
        let allowed = |base| fork_base_allowed("main", chooser, None, &candidates, base);
        assert!(allowed(None));
        assert!(allowed(Some("main")));
        assert!(allowed(Some("feature/three")));
        // A teammate's branch with no worktree here.
        assert!(allowed(Some("someone/else")));
        assert!(!allowed(Some("feature/one")));
        // Stacked on the chooser: recording it would close a cycle.
        assert!(!allowed(Some("feature/two")));
        assert_eq!(
            fork_base_choices("main", chooser, None, &candidates),
            vec!["feature/three".to_owned()]
        );
    }
}
