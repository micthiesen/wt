//! One pull request per worktree. A worktree records the branch it was made
//! for; any other branch its checkout has been on is an "other branch", and
//! a pull request on one has no row of its own to show its state. Those
//! branches are found in the checkout's own HEAD reflog, recorded in the
//! slug's state so they outlive the reflog and the worktree, and held
//! against removal while one may still carry an open pull request.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Slug-state key of the branch the worktree was created for (or moved to
/// by `wt next`).
pub const RECORDED_BRANCH: &str = "branch";
/// Slug-state key of the other branches seen, by name.
pub const OTHER_BRANCHES: &str = "otherBranches";
/// Slug-state key of the branches `wt next` left behind after they landed.
pub const LEFT_BRANCHES: &str = "leftBranches";
/// Slug-state key of the branch the session was last told about.
pub const OFF_BRANCH_NOTICE: &str = "offBranchNotice";

/// How long a "no open pull request" answer stays evidence. A pull request
/// can be opened or reopened at any time, so only a recent answer clears a
/// branch; a merge is final and never expires.
pub const PR_VERDICT_FRESH_MS: i64 = 10 * 60 * 1000;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OtherPrState {
    Open,
    Merged,
    Closed,
    /// GitHub answered for the branch and it has no pull request.
    None,
    /// GitHub has not answered for the branch.
    #[default]
    Unknown,
}

impl OtherPrState {
    /// The state of a GitHub pull request as GitHub spells it.
    pub fn of_github(state: &str) -> Self {
        match state.to_ascii_uppercase().as_str() {
            "OPEN" => Self::Open,
            "MERGED" => Self::Merged,
            "CLOSED" => Self::Closed,
            _ => Self::Unknown,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Merged => "merged",
            Self::Closed => "closed",
            Self::None => "none",
            Self::Unknown => "unknown",
        }
    }
}

/// A branch other than the recorded one that a worktree's checkout was on.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct OtherBranch {
    #[serde(skip)]
    pub branch: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub number: Option<u64>,
    pub state: OtherPrState,
    pub draft: bool,
    /// When GitHub last answered for the branch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checked_at_ms: Option<i64>,
}

/// What a recorded answer is worth to a removal at `now_ms`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OtherPrVerdict {
    /// The branch holds this open pull request.
    Open(Option<u64>),
    /// Nothing recent says whether it does.
    Unknown,
    /// It holds none.
    Clear,
}

impl OtherBranch {
    pub fn unknown(branch: impl Into<String>) -> Self {
        Self {
            branch: branch.into(),
            ..Self::default()
        }
    }

    pub fn verdict(&self, now_ms: i64) -> OtherPrVerdict {
        let fresh = self
            .checked_at_ms
            .is_some_and(|at| now_ms.saturating_sub(at) <= PR_VERDICT_FRESH_MS);
        match self.state {
            OtherPrState::Open => OtherPrVerdict::Open(self.number),
            OtherPrState::Merged => OtherPrVerdict::Clear,
            OtherPrState::Closed | OtherPrState::None if fresh => OtherPrVerdict::Clear,
            _ => OtherPrVerdict::Unknown,
        }
    }

    /// The JSON the listings print for one entry of `other_prs`.
    pub fn to_json(&self) -> Value {
        serde_json::json!({
            "number": self.number,
            "branch": self.branch,
            "state": self.state.as_str(),
            "draft": self.draft,
        })
    }
}

/// The `other_prs` value the listings print for a row: its recorded other
/// branches with what is known of the pull request on each.
pub fn other_prs_json(slug_state: &Value) -> Value {
    Value::Array(
        stored_other_branches(slug_state)
            .iter()
            .map(OtherBranch::to_json)
            .collect(),
    )
}

/// The branch `slug_state` records for its worktree.
pub fn recorded_branch(slug_state: &Value) -> Option<&str> {
    slug_state
        .get(RECORDED_BRANCH)
        .and_then(Value::as_str)
        .filter(|branch| !branch.is_empty())
}

/// The other branches `slug_state` records, by name.
pub fn stored_other_branches(slug_state: &Value) -> Vec<OtherBranch> {
    slug_state
        .get(OTHER_BRANCHES)
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .map(|(branch, entry)| {
            let mut other: OtherBranch = serde_json::from_value(entry.clone()).unwrap_or_default();
            other.branch.clone_from(branch);
            other
        })
        .collect()
}

fn left_branches(slug_state: &Value) -> impl Iterator<Item = &str> {
    slug_state
        .get(LEFT_BRANCHES)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
}

/// Every name a checkout's HEAD reflog (the text of its `logs/HEAD`) says
/// it was switched from or to. These are candidates: a detached HEAD is
/// named by its commit and a checked-out tag by the tag, and a branch may be
/// named like either, so only a ref that exists (or the durable record) says
/// which names are branches.
pub fn reflog_branches(head_log: &str) -> BTreeSet<String> {
    let mut branches = BTreeSet::new();
    for line in head_log.lines() {
        let Some((_, message)) = line.split_once('\t') else {
            continue;
        };
        let Some(moved) = message.strip_prefix("checkout: moving from ") else {
            continue;
        };
        let Some((from, to)) = moved.rsplit_once(" to ") else {
            continue;
        };
        for name in [from, to] {
            if !name.is_empty() && !name.contains(' ') {
                branches.insert(name.to_owned());
            }
        }
    }
    branches
}

/// The other branches of one worktree: every branch recorded for it before,
/// seen in its reflog now, or checked out in it now, that is not the one it
/// records (`current` stands in until one is recorded) and was not left
/// behind by `wt next`.
pub fn other_branch_names(
    slug_state: &Value,
    current: &str,
    reflog: &BTreeSet<String>,
) -> BTreeSet<String> {
    let recorded = recorded_branch(slug_state).unwrap_or(current);
    let left: BTreeSet<&str> = left_branches(slug_state).collect();
    stored_other_branches(slug_state)
        .into_iter()
        .map(|other| other.branch)
        .chain(reflog.iter().cloned())
        .chain((!current.is_empty()).then(|| current.to_owned()))
        .filter(|branch| branch != recorded && !left.contains(branch.as_str()))
        .collect()
}

/// Removal hazard for an other branch with an open pull request. `here` is
/// the recorded branch when the other one is checked out in the worktree
/// right now, where `wt new` cannot take it until the worktree lets go.
/// `exists` is false when the branch is neither a local branch nor one of
/// origin's here, where there is nothing for `wt new` to adopt.
pub fn open_pr_hazard(
    branch: &str,
    number: Option<u64>,
    here: Option<&str>,
    exists: bool,
) -> String {
    let pr = number.map_or_else(
        || "an open PR".to_owned(),
        |number| format!("open PR #{number}"),
    );
    if !exists {
        return format!(
            "holds {pr} on {branch}, a branch that is gone here: merge or close the PR, or retry once GitHub answers"
        );
    }
    match here {
        Some(recorded) => format!(
            "holds {pr} on {branch}: run git switch {recorded} here, then give it its own worktree with wt new {branch}"
        ),
        None => format!("holds {pr} on {branch}: give it its own worktree with wt new {branch}"),
    }
}

/// Removal hazard for an other branch nothing recent vouches for.
pub fn unknown_pr_hazard(branch: &str) -> String {
    format!(
        "could not tell whether {branch}, used in this worktree, has an open PR: retry once GitHub answers, or give it its own worktree with wt new {branch}"
    )
}

/// The two sanctioned routes to a second pull request, as told to a session
/// whose worktree is on `current` instead of `recorded`.
pub fn off_branch_notice(slug: &str, recorded: &str, current: &str) -> String {
    format!(
        "wt: this worktree ({slug}) is for {recorded}, but its checkout is now on {current}. A worktree carries one PR; a PR opened from {current} here has no row, so nobody sees its checks or reviews. Switch back with `git switch {recorded}`. For more work: after this worktree's PR merges, `wt next` keeps the worktree on a fresh branch; for a follow-up that must wait or stack, `wt new <slug> --base {recorded}`, gated there with `wt status --blocked-on <gate> --until <condition>` when it must wait for a later release."
    )
}

/// The refusal `gh pr create` gets inside a worktree that already has its
/// pull request or is not on its branch.
pub fn second_pr_refusal(reason: &str, recorded: &str) -> String {
    format!(
        "wt: refusing gh pr create: {reason}. One PR per worktree.\nwt: after this worktree's PR merges, `wt next` keeps the worktree on a fresh branch; for a follow-up that must wait or stack, `wt new <slug> --base {recorded}`, gated there with `wt status --blocked-on <gate> --until <condition>` for a removal that waits a release."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const LOG: &str = "0000000000000000000000000000000000000000 1111111111111111111111111111111111111111 A <a@example.invalid> 1795730000 +0000\tbranch: Created from HEAD\n\
1111111111111111111111111111111111111111 2222222222222222222222222222222222222222 A <a@example.invalid> 1795730001 +0000\tcommit: work\n\
2222222222222222222222222222222222222222 2222222222222222222222222222222222222222 A <a@example.invalid> 1795730002 +0000\tcheckout: moving from michael/one to michael/retire-x\n\
2222222222222222222222222222222222222222 3333333333333333333333333333333333333333 A <a@example.invalid> 1795730003 +0000\tcheckout: moving from michael/retire-x to 3333333333333333333333333333333333333333\n\
3333333333333333333333333333333333333333 2222222222222222222222222222222222222222 A <a@example.invalid> 1795730004 +0000\tcheckout: moving from 3333333333333333333333333333333333333333 to michael/one\n\
2222222222222222222222222222222222222222 2222222222222222222222222222222222222222 A <a@example.invalid> 1795730005 +0000\trebase (start): checkout origin/main\n";

    #[test]
    fn the_reflog_names_everything_the_checkout_moved_between() {
        assert_eq!(
            reflog_branches(LOG),
            BTreeSet::from([
                "3".repeat(40),
                "michael/one".to_owned(),
                "michael/retire-x".to_owned()
            ])
        );
        // A name is not judged by its shape: these are all legal branches.
        let odd =
            "0 1 A <a@example.invalid> 1 +0000\tcheckout: moving from deadbeef to release@2\n";
        assert_eq!(
            reflog_branches(odd),
            BTreeSet::from(["deadbeef".to_owned(), "release@2".to_owned()])
        );
    }

    #[test]
    fn other_branches_are_everything_but_the_recorded_and_the_left_behind() {
        let reflog = BTreeSet::from(["michael/one".to_owned(), "michael/retire-x".to_owned()]);
        // Until a branch is recorded, the checked-out one is the worktree's.
        assert_eq!(
            other_branch_names(&json!({}), "michael/one", &reflog),
            BTreeSet::from(["michael/retire-x".to_owned()])
        );
        // A recorded branch stays the worktree's while HEAD is elsewhere,
        // and a recorded other branch outlives the reflog.
        let state = json!({
            "branch": "michael/one-2",
            "leftBranches": ["michael/one"],
            "otherBranches": {"michael/gone": {"state": "open", "number": 7}},
        });
        assert_eq!(
            other_branch_names(&state, "michael/retire-x", &reflog),
            BTreeSet::from(["michael/gone".to_owned(), "michael/retire-x".to_owned()])
        );
        let stored = stored_other_branches(&state);
        assert_eq!(stored[0].branch, "michael/gone");
        assert_eq!(stored[0].verdict(0), OtherPrVerdict::Open(Some(7)));
    }

    #[test]
    fn only_a_merge_or_a_recent_answer_clears_a_branch() {
        let at = |state, checked_at_ms| OtherBranch {
            branch: "b".into(),
            state,
            checked_at_ms,
            ..OtherBranch::default()
        };
        let now = 100 * PR_VERDICT_FRESH_MS;
        assert_eq!(
            at(OtherPrState::Merged, None).verdict(now),
            OtherPrVerdict::Clear
        );
        assert_eq!(
            at(OtherPrState::None, Some(now - 1)).verdict(now),
            OtherPrVerdict::Clear
        );
        assert_eq!(
            at(OtherPrState::Closed, Some(0)).verdict(now),
            OtherPrVerdict::Unknown
        );
        assert_eq!(
            at(OtherPrState::None, None).verdict(now),
            OtherPrVerdict::Unknown
        );
        assert_eq!(
            at(OtherPrState::Unknown, Some(now)).verdict(now),
            OtherPrVerdict::Unknown
        );
        assert_eq!(
            at(OtherPrState::Open, Some(0)).verdict(now),
            OtherPrVerdict::Open(None)
        );
    }
}
