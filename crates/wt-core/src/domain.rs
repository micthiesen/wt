//! Which part of the world a source observes. Domains connect three things
//! that must agree: the refresh a mutation requests, the observation that may
//! settle it, and the status a pane shows. The taxonomy is wt's; the
//! scheduling and caching machinery in `wt-runtime` only carries it.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Domain {
    /// Worktree inventory and per-worktree Git status.
    Git,
    /// Durable wt state: statuses, sections, bases, archive (SQLite).
    Metadata,
    /// Pull requests and merge queue for board branches.
    Github,
    /// Pull requests awaiting the viewer's review.
    Reviews,
    /// External issue-tracker statuses.
    Issues,
    /// tmux sessions and coding-agent discoveries.
    Sessions,
    /// App logs, manager reports and attention.
    Activity,
    /// Durable action runs and their log tails.
    Actions,
    /// Supervised development servers.
    Dev,
    /// `git fetch origin` maintenance.
    Origin,
    /// Generated worktree titles.
    Naming,
    /// Extra per-row Git facts (ahead/behind, last commit, diff stat).
    Presentation,
    /// Removed-worktree history.
    History,
    /// Process and machine sampling.
    Perf,
    /// Automation engine state.
    Automations,
    /// A remote host's connection and snapshot stream.
    Host,
}

impl Domain {
    /// Every domain, for summaries that must not silently miss one.
    pub const ALL: [Domain; 16] = [
        Self::Git,
        Self::Metadata,
        Self::Github,
        Self::Reviews,
        Self::Issues,
        Self::Sessions,
        Self::Activity,
        Self::Actions,
        Self::Dev,
        Self::Origin,
        Self::Naming,
        Self::Presentation,
        Self::History,
        Self::Perf,
        Self::Automations,
        Self::Host,
    ];

    /// Reads of this domain go to a remote service whose read model lags
    /// writes and is rate limited, so a change waits longer to be observed.
    pub const fn slow_reads(self) -> bool {
        matches!(self, Self::Github | Self::Reviews | Self::Issues)
    }

    /// The name shown in pane status, e.g. "GitHub failed".
    pub const fn label(self) -> &'static str {
        match self {
            Self::Git => "Git",
            Self::Metadata => "state",
            Self::Github => "GitHub",
            Self::Reviews => "reviews",
            Self::Issues => "tracker",
            Self::Sessions => "sessions",
            Self::Activity => "activity",
            Self::Actions => "actions",
            Self::Dev => "dev",
            Self::Origin => "origin",
            Self::Naming => "titles",
            Self::Presentation => "Git details",
            Self::History => "history",
            Self::Perf => "perf",
            Self::Automations => "automations",
            Self::Host => "host",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Domain;

    #[test]
    fn all_lists_every_domain_once() {
        // Adding a variant without listing it fails to compile here.
        for domain in Domain::ALL {
            match domain {
                Domain::Git
                | Domain::Metadata
                | Domain::Github
                | Domain::Reviews
                | Domain::Issues
                | Domain::Sessions
                | Domain::Activity
                | Domain::Actions
                | Domain::Dev
                | Domain::Origin
                | Domain::Naming
                | Domain::Presentation
                | Domain::History
                | Domain::Perf
                | Domain::Automations
                | Domain::Host => {}
            }
        }
        let unique: std::collections::BTreeSet<_> = Domain::ALL.into_iter().collect();
        assert_eq!(unique.len(), Domain::ALL.len());
    }
}
