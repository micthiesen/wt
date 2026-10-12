//! Pull request writes and review-request dismissals, confirmed by GitHub
//! reads.
use wt_core::Domain;
use wt_runtime::Target;

use super::{Family, target_row, target_row_mut};
use crate::{Board, PrPresentation};

#[derive(Clone, Debug, PartialEq)]
pub enum GithubChange {
    AutoMerge { enable: bool },
    MarkReady,
    Ship,
    Reviewers { reviewers: Vec<String> },
    DismissReview { url: String },
}

fn pr<'a>(board: &'a Board, target: &Target) -> Option<&'a PrPresentation> {
    target_row(board, target).and_then(|row| row.pr.as_ref())
}

fn pr_mut<'a>(board: &'a mut Board, target: &Target) -> Option<&'a mut PrPresentation> {
    target_row_mut(board, target).and_then(|row| row.pr.as_mut())
}

fn merging(pr: &PrPresentation) -> bool {
    pr.auto_merge_armed || pr.merge_queue.is_some()
}

impl Family for GithubChange {
    fn field(&self) -> String {
        match self {
            Self::AutoMerge { .. } | Self::Ship => "merge".into(),
            Self::MarkReady => "draft".into(),
            Self::Reviewers { .. } => "reviewers".into(),
            Self::DismissReview { url } => format!("dismiss:{url}"),
        }
    }

    fn label(&self) -> String {
        match self {
            Self::AutoMerge { enable: true } => "arming merge",
            Self::AutoMerge { enable: false } => "disarming merge",
            Self::MarkReady => "marking ready",
            Self::Ship => "shipping",
            Self::Reviewers { .. } => "requesting reviewers",
            Self::DismissReview { .. } => "dismissing review",
        }
        .into()
    }

    fn domains(&self) -> &'static [Domain] {
        match self {
            Self::DismissReview { .. } => &[Domain::Reviews],
            _ => &[Domain::Github],
        }
    }

    fn apply(&self, target: &Target, board: &mut Board) {
        if let Self::DismissReview { url } = self {
            board.review_requests.retain(|row| &row.url != url);
            return;
        }
        let Some(pr) = pr_mut(board, target) else {
            return;
        };
        match self {
            Self::AutoMerge { enable } => {
                pr.auto_merge_armed = *enable;
                if !enable {
                    pr.merge_queue = None;
                }
            }
            Self::MarkReady => pr.draft = false,
            Self::Ship => {
                pr.draft = false;
                if pr.merge_queue.is_none() {
                    pr.auto_merge_armed = true;
                }
            }
            Self::Reviewers { reviewers } => pr.reviewers = reviewers.clone(),
            Self::DismissReview { .. } => {}
        }
    }

    fn observed(&self, target: &Target, board: &Board) -> bool {
        // A row or PR that is gone has nothing left to show.
        let pr = pr(board, target);
        match self {
            Self::AutoMerge { enable } => pr.is_none_or(|pr| merging(pr) == *enable),
            Self::MarkReady => pr.is_none_or(|pr| !pr.draft),
            Self::Ship => pr.is_none_or(|pr| !pr.draft && merging(pr)),
            Self::Reviewers { reviewers } => pr.is_none_or(|pr| {
                let mut shown = pr.reviewers.clone();
                let mut wanted = reviewers.clone();
                shown.sort();
                wanted.sort();
                shown == wanted
            }),
            Self::DismissReview { url } => !board.review_requests.iter().any(|row| &row.url == url),
        }
    }
}
