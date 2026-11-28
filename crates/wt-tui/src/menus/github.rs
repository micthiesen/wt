//! `e` / `E`, `v` and `f`: marking a pull request ready or shipping it,
//! choosing its reviewers, and reading its failed checks, from the PR the
//! board shows for the row.

use super::{LoadingOverlay, LocalMenu, Menu, row_display};
use crate::{
    Board, BoardRow, CheckState, ConfirmAction, PrFact, PrPin, PrPresentation, ReviewerOption,
    ReviewerPool, UiModal,
};

#[derive(Debug, PartialEq, Eq)]
enum GithubPrompt {
    /// Nothing left to do; show this message instead of a confirmation.
    Done(String),
    /// Confirmation title listing only the remaining steps.
    Confirm(String),
}

const ROW_GONE: &str = "worktree is no longer on the board";

/// The pull request the board shows for a row, with its number.
fn shown_pr(row: &BoardRow) -> Option<(&PrPresentation, u64)> {
    let pr = row.pr.as_ref()?;
    Some((pr, pr.number?))
}

/// The answer for a row whose pull request the board does not show. Its
/// host's last live read of the branch tells a branch without one from a
/// read that has not happened yet; only the second is worth asking again.
fn no_shown_pr(row: &BoardRow) -> LocalMenu {
    if let Some(error) = row.pr.as_ref().and_then(|pr| pr.error.as_ref()) {
        return LocalMenu::NotYet(format!("GitHub data is unavailable: {error}"));
    }
    match row.action_facts.pr {
        PrFact::Absent => LocalMenu::Notice("no PR for this branch".into()),
        PrFact::Unknown | PrFact::Present { .. } => {
            LocalMenu::NotYet("GitHub data is still loading for this branch".into())
        }
    }
}

pub(super) fn menu(board: &Board, key: &str, ship: bool) -> LocalMenu {
    let Some(row) = board.rows.iter().find(|row| row.key == key) else {
        return LocalMenu::Notice(ROW_GONE.into());
    };
    let Some((pr, number)) = shown_pr(row) else {
        return no_shown_pr(row);
    };
    // A pin without a head could never match confirmed data.
    let Some(head_sha) = pr.head_sha.as_deref() else {
        return no_shown_pr(row);
    };
    if !pr.is_open() {
        return LocalMenu::Notice(format!("PR #{number} is not open"));
    }
    let reviewer = row_display(board, row).default_reviewer.as_deref();
    match github_prompt(pr, number, ship, reviewer) {
        GithubPrompt::Done(text) => LocalMenu::Notice(text),
        GithubPrompt::Confirm(title) => LocalMenu::modal(UiModal::Confirm {
            // The write executes only against this same PR and head.
            action: ConfirmAction::Github {
                key: key.to_owned(),
                ship,
                pin: Some(PrPin {
                    number,
                    head_sha: Some(head_sha.to_owned()),
                }),
            },
            title,
            lines: vec![format!(
                "{}: {}",
                row.branch,
                pr.title.as_deref().unwrap_or_default()
            )],
            cancel_key: Some(if ship { 'E' } else { 'e' }),
        }),
    }
}

/// `e` / `E` pre-checks from the TS keymap: mark-ready refuses a PR that is
/// already ready, and ship lists only the steps still needed.
fn github_prompt(
    pr: &PrPresentation,
    number: u64,
    ship: bool,
    reviewer: Option<&str>,
) -> GithubPrompt {
    if !ship {
        return if pr.draft {
            GithubPrompt::Confirm(format!("Mark #{number} ready for review?"))
        } else {
            GithubPrompt::Done(format!("PR #{number} is already ready"))
        };
    }
    let mut steps = Vec::new();
    if pr.draft {
        steps.push("mark ready".to_owned());
    }
    if let Some(reviewer) = reviewer
        && !pr.reviewers.iter().any(|login| login == reviewer)
    {
        steps.push(format!("request {reviewer}"));
    }
    if !pr.auto_merge_armed {
        steps.push("arm auto-merge".to_owned());
    }
    if steps.is_empty() {
        GithubPrompt::Done(format!("#{number} already shipped"))
    } else {
        GithubPrompt::Confirm(format!("Ship #{number}? ({})", steps.join(", ")))
    }
}

/// `v`: the reviewers of the row's pull request, from the people the PR
/// names and its host's last read of the repository's contributors. The
/// commit carries the PR and the requested list shown here, which its host
/// compares with a live read before it writes.
pub(super) fn reviewers(board: &Board, key: &str) -> LocalMenu {
    let Some(row) = board.rows.iter().find(|row| row.key == key) else {
        return LocalMenu::Notice(ROW_GONE.into());
    };
    if !row_display(board, row).reviewers {
        return LocalMenu::Notice("reviewers are disabled for this repository".into());
    }
    let Some((pr, number)) = shown_pr(row) else {
        return no_shown_pr(row);
    };
    if !pr.is_open() {
        return LocalMenu::Notice(format!("PR #{number} is not open"));
    }
    if pr.draft {
        return LocalMenu::Notice(format!("PR #{number} is a draft (mark it ready first)"));
    }
    let pool = match &row.host {
        Some(host) => board.host_reviewers.get(host),
        None => board.reviewers.as_ref(),
    };
    // Without the pool the signed-in login is unknown, and the picker
    // would offer the person themselves.
    let Some(pool) = pool else {
        return LocalMenu::NotYet("reviewer list is still loading".into());
    };
    let candidates = reviewer_candidates(pr, pool);
    if candidates.is_empty() {
        return LocalMenu::Notice("no reviewer candidates".into());
    }
    LocalMenu::modal(UiModal::Reviewers {
        key: key.to_owned(),
        pr_number: number,
        original: pr.reviewers.clone(),
        candidates,
    })
}

/// Suggested reviewers first, then anyone else already requested, then the
/// repository's contributors; never the signed-in person.
fn reviewer_candidates(pr: &PrPresentation, pool: &ReviewerPool) -> Vec<ReviewerOption> {
    let requested = |login: &str| pr.reviewers.iter().any(|reviewer| reviewer == login);
    let mut candidates: Vec<ReviewerOption> = Vec::new();
    let mut add = |login: &str, label: String, selected: bool| {
        if login != pool.viewer && candidates.iter().all(|option| option.login != login) {
            candidates.push(ReviewerOption {
                login: login.to_owned(),
                label,
                selected,
            });
        }
    };
    for suggestion in &pr.suggested_reviewers {
        let mut tags = Vec::new();
        if requested(&suggestion.login) {
            tags.push("requested");
        }
        tags.push("suggested");
        if suggestion.author {
            tags.push("author");
        }
        if suggestion.commenter {
            tags.push("commenter");
        }
        add(
            &suggestion.login,
            format!("{} ({})", suggestion.login, tags.join(", ")),
            requested(&suggestion.login),
        );
    }
    for login in &pr.reviewers {
        add(login, format!("{login} (requested)"), true);
    }
    for contributor in &pool.contributors {
        add(
            &contributor.login,
            format!(
                "{} ({} commits)",
                contributor.login, contributor.contributions
            ),
            requested(&contributor.login),
        );
    }
    candidates
}

/// `f`: the log of the row's failing CI run. The board says whether a check
/// is failing; the log itself is fetched from GitHub by the row's host, into
/// an overlay that is open while it loads.
pub(super) fn failed_checks(board: &Board, key: &str) -> Menu {
    let overlay = |branch: &str| {
        Menu::Overlay(LoadingOverlay {
            title: format!("Failed checks: {branch}"),
            close_key: Some('f'),
        })
    };
    // A request a host can answer says so for the empty board, which is
    // what `UiAction::is_board_menu` asks; the host then names the row.
    let Some(row) = board.rows.iter().find(|row| row.key == key) else {
        return overlay("pull request");
    };
    let Some((pr, number)) = shown_pr(row) else {
        return Menu::Local(no_shown_pr(row));
    };
    if !pr.is_open() {
        return Menu::Local(LocalMenu::Notice(format!("PR #{number} is not open")));
    }
    if pr.checks != CheckState::Fail {
        return Menu::Local(LocalMenu::Notice("no failing checks".into()));
    }
    overlay(&row.branch)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ActionFacts, ContributorView, DisplayPolicy, SuggestedReviewerView};

    fn pr(draft: bool, reviewers: &[&str], armed: bool) -> PrPresentation {
        PrPresentation {
            number: Some(7),
            head_sha: Some("head".into()),
            state: Some("OPEN".into()),
            draft,
            title: Some("Title".into()),
            reviewers: reviewers.iter().map(|login| (*login).to_owned()).collect(),
            auto_merge_armed: armed,
            ..Default::default()
        }
    }

    fn board(pr: Option<PrPresentation>, reviewer: Option<&str>) -> Board {
        Board {
            rows: vec![BoardRow {
                key: "one".into(),
                branch: "feature/one".into(),
                pr,
                ..Default::default()
            }],
            display: DisplayPolicy {
                default_reviewer: reviewer.map(str::to_owned),
                reviewers: true,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn notice(menu: LocalMenu) -> String {
        match menu {
            LocalMenu::Notice(text) => text,
            other => panic!("expected a notice, got {other:?}"),
        }
    }

    fn pool() -> ReviewerPool {
        ReviewerPool {
            viewer: "viewer".into(),
            contributors: [("contributor", 12), ("suggested", 99), ("viewer", 2)]
                .into_iter()
                .map(|(login, contributions)| ContributorView {
                    login: login.into(),
                    contributions,
                })
                .collect(),
            fetched_at_ms: 1,
        }
    }

    fn reviewed() -> PrPresentation {
        PrPresentation {
            suggested_reviewers: vec![
                SuggestedReviewerView {
                    login: "suggested".into(),
                    author: false,
                    commenter: true,
                },
                SuggestedReviewerView {
                    login: "maintainer".into(),
                    author: true,
                    commenter: false,
                },
            ],
            ..pr(false, &["maintainer"], false)
        }
    }

    #[test]
    fn prompts_list_only_the_steps_still_needed() {
        assert_eq!(
            github_prompt(&pr(false, &[], false), 7, false, None),
            GithubPrompt::Done("PR #7 is already ready".into())
        );
        assert_eq!(
            github_prompt(&pr(true, &[], false), 7, false, None),
            GithubPrompt::Confirm("Mark #7 ready for review?".into())
        );
        assert_eq!(
            github_prompt(&pr(true, &[], false), 7, true, Some("ana")),
            GithubPrompt::Confirm("Ship #7? (mark ready, request ana, arm auto-merge)".into())
        );
        assert_eq!(
            github_prompt(&pr(false, &["ana"], false), 7, true, Some("ana")),
            GithubPrompt::Confirm("Ship #7? (arm auto-merge)".into())
        );
        assert_eq!(
            github_prompt(&pr(false, &["ana"], true), 7, true, Some("ana")),
            GithubPrompt::Done("#7 already shipped".into())
        );
    }

    #[test]
    fn confirmation_pins_the_pr_and_head_the_board_showed() {
        let board = board(Some(pr(true, &[], false)), Some("ana"));
        let LocalMenu::Modal { modal, .. } = menu(&board, "one", true) else {
            panic!("expected a confirmation");
        };
        let UiModal::Confirm {
            action,
            title,
            lines,
            cancel_key,
        } = *modal
        else {
            panic!("expected a confirmation");
        };
        assert_eq!(title, "Ship #7? (mark ready, request ana, arm auto-merge)");
        assert_eq!(lines, vec!["feature/one: Title".to_owned()]);
        assert_eq!(cancel_key, Some('E'));
        assert_eq!(
            action,
            ConfirmAction::Github {
                key: "one".into(),
                ship: true,
                pin: Some(PrPin {
                    number: 7,
                    head_sha: Some("head".into()),
                }),
            }
        );
    }

    #[test]
    fn closed_and_ready_prs_are_notices() {
        let merged = PrPresentation {
            state: Some("MERGED".into()),
            ..pr(false, &[], false)
        };
        assert_eq!(
            notice(menu(&board(Some(merged), None), "one", false)),
            "PR #7 is not open"
        );
        assert_eq!(
            notice(menu(
                &board(Some(pr(false, &[], false)), None),
                "one",
                false
            )),
            "PR #7 is already ready"
        );
    }

    /// The host used to hold the key for up to twelve seconds to tell these
    /// apart. The board carries its host's last live read of the branch.
    #[test]
    fn a_row_without_a_shown_pr_is_answered_at_once() {
        let facts = |pr| ActionFacts {
            pr,
            ..Default::default()
        };
        let mut absent = board(None, None);
        absent.rows[0].action_facts = facts(PrFact::Absent);
        let mut failed = board(
            Some(PrPresentation {
                error: Some("gh failed".into()),
                ..Default::default()
            }),
            None,
        );
        failed.rows[0].action_facts = facts(PrFact::Absent);
        // A live read that found a PR the display stage has not drawn yet.
        let mut arriving = board(None, None);
        arriving.rows[0].action_facts = facts(PrFact::Present {
            open: true,
            draft: true,
        });
        for ship in [false, true] {
            assert_eq!(
                menu(&absent, "one", ship),
                LocalMenu::Notice("no PR for this branch".into())
            );
            assert_eq!(
                menu(&board(None, None), "one", ship),
                LocalMenu::NotYet("GitHub data is still loading for this branch".into())
            );
            assert_eq!(
                menu(&arriving, "one", ship),
                LocalMenu::NotYet("GitHub data is still loading for this branch".into())
            );
            assert_eq!(
                menu(&failed, "one", ship),
                LocalMenu::NotYet("GitHub data is unavailable: gh failed".into())
            );
            assert_eq!(
                menu(&Board::default(), "one", ship),
                LocalMenu::Notice(ROW_GONE.into())
            );
        }
        assert_eq!(reviewers(&absent, "one"), menu(&absent, "one", false));
        assert_eq!(
            failed_checks(&absent, "one"),
            Menu::Local(menu(&absent, "one", false))
        );
    }

    #[test]
    fn reviewers_follow_suggestion_requested_contributor_tiers_and_skip_self() {
        let mut board = board(Some(reviewed()), None);
        board.reviewers = Some(pool());
        let LocalMenu::Modal { modal, host, .. } = reviewers(&board, "one") else {
            panic!("expected the picker");
        };
        let UiModal::Reviewers {
            key,
            pr_number,
            original,
            candidates,
        } = *modal
        else {
            panic!("expected the picker");
        };
        // The commit names the PR and the requested list the picker showed.
        assert_eq!((key.as_str(), pr_number), ("one", 7));
        assert_eq!(original, ["maintainer"]);
        assert_eq!(host, None);
        assert_eq!(
            candidates
                .iter()
                .map(|option| (option.login.as_str(), option.selected))
                .collect::<Vec<_>>(),
            [
                ("suggested", false),
                ("maintainer", true),
                ("contributor", false)
            ]
        );
        assert_eq!(candidates[0].label, "suggested (suggested, commenter)");
        assert_eq!(
            candidates[1].label,
            "maintainer (requested, suggested, author)"
        );
        assert_eq!(candidates[2].label, "contributor (12 commits)");
    }

    #[test]
    fn reviewers_answer_every_board() {
        // Empty and partial boards never panic and never ask a host.
        assert_eq!(
            reviewers(&Board::default(), "one"),
            LocalMenu::Notice(ROW_GONE.into())
        );
        let mut disabled = board(Some(reviewed()), None);
        disabled.display.reviewers = false;
        assert_eq!(
            notice(reviewers(&disabled, "one")),
            "reviewers are disabled for this repository"
        );
        assert_eq!(
            reviewers(&board(Some(reviewed()), None), "one"),
            LocalMenu::NotYet("reviewer list is still loading".into())
        );
        let mut draft = board(Some(pr(true, &[], false)), None);
        draft.reviewers = Some(pool());
        assert_eq!(
            notice(reviewers(&draft, "one")),
            "PR #7 is a draft (mark it ready first)"
        );
        let mut alone = board(Some(pr(false, &[], false)), None);
        alone.reviewers = Some(ReviewerPool {
            viewer: "viewer".into(),
            ..Default::default()
        });
        assert_eq!(notice(reviewers(&alone, "one")), "no reviewer candidates");
    }

    #[test]
    fn a_remote_rows_reviewers_come_from_its_own_host() {
        let mut board = board(Some(reviewed()), None);
        board.rows[0].host = Some("builder".into());
        // This machine's pool and policy say nothing about that host.
        board.reviewers = Some(pool());
        board.display.reviewers = false;
        assert_eq!(
            notice(reviewers(&board, "one")),
            "reviewers are disabled for this repository"
        );
        board.host_display.insert(
            "builder".into(),
            DisplayPolicy {
                reviewers: true,
                ..Default::default()
            },
        );
        assert_eq!(
            reviewers(&board, "one"),
            LocalMenu::NotYet("reviewer list is still loading".into())
        );
        board.host_reviewers.insert(
            "builder".into(),
            ReviewerPool {
                viewer: "contributor".into(),
                ..pool()
            },
        );
        let LocalMenu::Modal { modal, .. } = reviewers(&board, "one") else {
            panic!("expected the picker");
        };
        let UiModal::Reviewers { candidates, .. } = *modal else {
            panic!("expected the picker");
        };
        assert_eq!(
            candidates
                .iter()
                .map(|option| option.login.as_str())
                .collect::<Vec<_>>(),
            ["suggested", "maintainer", "viewer"]
        );
    }

    #[test]
    fn failed_checks_fetch_only_what_the_board_shows_failing() {
        let failing = PrPresentation {
            checks: CheckState::Fail,
            ..pr(false, &[], false)
        };
        assert_eq!(
            failed_checks(&board(Some(failing.clone()), None), "one"),
            Menu::Overlay(LoadingOverlay {
                title: "Failed checks: feature/one".into(),
                close_key: Some('f'),
            })
        );
        assert_eq!(
            failed_checks(&board(Some(pr(false, &[], false)), None), "one"),
            Menu::Local(LocalMenu::Notice("no failing checks".into()))
        );
        let merged = PrPresentation {
            state: Some("MERGED".into()),
            ..failing
        };
        assert_eq!(
            failed_checks(&board(Some(merged), None), "one"),
            Menu::Local(LocalMenu::Notice("PR #7 is not open".into()))
        );
        // The host answers for a row this board does not list, so the
        // request is never mistaken for a board menu.
        assert!(matches!(
            failed_checks(&Board::default(), "one"),
            Menu::Overlay(_)
        ));
    }
}
