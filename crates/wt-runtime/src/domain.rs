//! Which part of the world a source observes. Domains connect three things
//! that must agree: the refresh a mutation requests, the observation that may
//! settle it, and the status a pane shows.
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
