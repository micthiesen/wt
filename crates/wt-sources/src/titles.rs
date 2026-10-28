//! Row titles. A stored title (set by the user, an agent, or the issue
//! tracker; see `wt_store::TitleSource`) always shows. A worktree nobody
//! titled reads as its PR title, then its first commit, then its slug.

use std::sync::Arc;

use wt_runtime::{SourceHandle, TaskScope, derived_channel};
use wt_tui::{Board, BoardRow, TitleSource};

pub fn presented(source: wt_store::TitleSource) -> TitleSource {
    match source {
        wt_store::TitleSource::User => TitleSource::User,
        wt_store::TitleSource::Agent => TitleSource::Agent,
        wt_store::TitleSource::Issue => TitleSource::Issue,
        wt_store::TitleSource::Fallback => TitleSource::Fallback,
    }
}

pub fn overlay(scope: &TaskScope, board: SourceHandle<Board>) -> SourceHandle<Board> {
    let (output, mut publisher) = derived_channel(scope, "title-board");
    let cancellation = scope.token();
    scope.spawn(async move {
        let mut boards = board.subscribe();
        boards.mark_changed();
        loop {
            tokio::select! {
                biased;
                _ = cancellation.cancelled() => break,
                request = publisher.requested() => {
                    if request.is_none() { break; }
                    board.refresh();
                    continue;
                }
                changed = boards.changed() => if changed.is_err() { break; },
            }
            let base = boards.borrow_and_update().clone();
            let data = base.data().map(|board| {
                let mut board = board.as_ref().clone();
                board.rows.iter_mut().for_each(fall_back);
                Arc::new(board)
            });
            publisher.publish_if_changed(base.with_data(data));
        }
    });
    output
}

/// Title an untitled row from its PR or first commit. Remote rows can share
/// a slug with a local checkout; their title belongs to the remote host.
fn fall_back(row: &mut BoardRow) {
    if row.key.starts_with("@remote/") || row.title_source.is_stored() {
        return;
    }
    let found = [
        (row.git.pr_title.as_deref(), TitleSource::Pr),
        (row.git.first_commit_title.as_deref(), TitleSource::Commit),
    ]
    .into_iter()
    .find_map(|(title, source)| {
        title
            .map(str::trim)
            .filter(|title| !title.is_empty())
            .map(|title| (wt_core::sanitize_terminal_text(title), source))
    });
    if let Some((title, source)) = found {
        row.title = title;
        row.title_source = source;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(source: TitleSource, pr: Option<&str>, commit: Option<&str>) -> BoardRow {
        let mut row = BoardRow {
            key: "one".into(),
            slug: "coz-1-fix".into(),
            title: "Stored or slug".into(),
            title_source: source,
            ..Default::default()
        };
        row.git.pr_title = pr.map(Into::into);
        row.git.first_commit_title = commit.map(Into::into);
        row
    }

    #[test]
    fn a_stored_title_outranks_the_pr_and_commit() {
        for source in [
            TitleSource::User,
            TitleSource::Agent,
            TitleSource::Issue,
            TitleSource::Fallback,
        ] {
            let mut stored = row(source, Some("PR title"), Some("Commit"));
            fall_back(&mut stored);
            assert_eq!(
                (stored.title.as_str(), stored.title_source),
                ("Stored or slug", source)
            );
        }
    }

    #[test]
    fn an_untitled_row_reads_as_its_pr_then_first_commit_then_slug() {
        let mut titled = row(TitleSource::Slug, Some(" PR title "), Some("Commit"));
        fall_back(&mut titled);
        assert_eq!(
            (titled.title.as_str(), titled.title_source),
            ("PR title", TitleSource::Pr)
        );
        let mut committed = row(TitleSource::Slug, Some("  "), Some("Commit"));
        fall_back(&mut committed);
        assert_eq!(committed.title_source, TitleSource::Commit);
        let mut bare = row(TitleSource::Slug, None, None);
        fall_back(&mut bare);
        assert_eq!(
            (bare.title.as_str(), bare.title_source),
            ("Stored or slug", TitleSource::Slug)
        );
    }
}
