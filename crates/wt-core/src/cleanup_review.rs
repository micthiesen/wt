//! What a cleanup review says about the rows it keeps. The board's review
//! and `wt clean` both read this, so they list the same rows for the same
//! reasons.

/// Why a row whose work may still land is kept.
pub const NOT_LANDED: &str = "landing of its own work is not proven";

/// What a cleanup review knows of a row it will not remove.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeptRow<'a> {
    /// Its own work is proven landed.
    pub landed: bool,
    /// Its status is `dropped`.
    pub dropped: bool,
    /// Its pull request's state, as GitHub names it.
    pub pr_state: Option<&'a str>,
}

impl KeptRow<'_> {
    /// Whether the row belongs in the review: one a person could be
    /// surprised to see kept, because its work landed, was dropped, or its
    /// pull request merged or closed. Work that is simply unfinished or not
    /// started is not cleanup's business and is left out.
    pub fn surprising(&self) -> bool {
        self.landed || self.dropped || matches!(self.pr_state, Some("MERGED" | "CLOSED"))
    }

    /// Why a row whose work is not proven landed is kept. A branch that
    /// will never land (dropped, or its pull request closed unmerged) is
    /// still never removed by cleanup, because its commits exist nowhere
    /// else; the reason says how to remove it deliberately. `base` is the
    /// branch it would have landed on and `remove_with` what removes it.
    pub fn unlanded_reason(&self, base: &str, remove_with: &str) -> String {
        if self.dropped {
            format!("dropped, but its commits are not on {base}: remove it with {remove_with}")
        } else if self.pr_state == Some("CLOSED") {
            format!(
                "its PR was closed unmerged, and its commits are not on {base}: remove it with {remove_with}"
            )
        } else {
            NOT_LANDED.to_owned()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(landed: bool, dropped: bool, pr_state: Option<&str>) -> KeptRow<'_> {
        KeptRow {
            landed,
            dropped,
            pr_state,
        }
    }

    #[test]
    fn only_rows_a_person_expects_gone_are_listed_and_abandoned_ones_say_what_to_do() {
        // Unstarted or unfinished work, with or without an open PR.
        assert!(!row(false, false, None).surprising());
        assert!(!row(false, false, Some("OPEN")).surprising());
        // Landed and held, dropped, or a PR that is over.
        assert!(row(true, false, None).surprising());
        assert!(row(false, true, None).surprising());
        assert!(row(false, false, Some("CLOSED")).surprising());
        assert!(row(false, false, Some("MERGED")).surprising());

        assert_eq!(
            row(false, true, Some("CLOSED")).unlanded_reason("main", "d"),
            "dropped, but its commits are not on main: remove it with d"
        );
        assert_eq!(
            row(false, false, Some("CLOSED")).unlanded_reason("main", "wt rm one"),
            "its PR was closed unmerged, and its commits are not on main: remove it with wt rm one"
        );
        // Commits made after a merge, or work still open, may yet land.
        for pr in [Some("MERGED"), Some("OPEN"), None] {
            assert_eq!(
                row(false, false, pr).unlanded_reason("main", "d"),
                NOT_LANDED
            );
        }
    }
}
