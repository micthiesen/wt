//! `e` / `E`: confirm marking a pull request ready, or shipping it, from the
//! PR the board shows for the row.

use super::{LocalMenu, Menu};
use crate::{Board, ConfirmAction, PrPin, PrPresentation, UiModal};

#[derive(Debug, PartialEq, Eq)]
enum GithubPrompt {
    /// Nothing left to do; show this message instead of a confirmation.
    Done(String),
    /// Confirmation title listing only the remaining steps.
    Confirm(String),
}

pub(super) fn menu(board: &Board, key: &str, ship: bool) -> Menu {
    let Some(row) = board.rows.iter().find(|row| row.key == key) else {
        return Menu::Host {
            reason: "worktree is not on this board yet",
        };
    };
    // Without a PR number the board cannot tell "no PR" from "not loaded
    // yet"; the host answers that from confirmed GitHub data.
    let host = Menu::Host {
        reason: "needs confirmed GitHub data to tell loading from no PR",
    };
    let Some(pr) = row.pr.as_ref() else {
        return host;
    };
    let Some(number) = pr.number else {
        return host;
    };
    if !pr.is_open() {
        return Menu::Local(LocalMenu::Notice(format!("PR #{number} is not open")));
    }
    match github_prompt(pr, number, ship, board.display.default_reviewer.as_deref()) {
        GithubPrompt::Done(text) => Menu::Local(LocalMenu::Notice(text)),
        GithubPrompt::Confirm(title) => Menu::Local(LocalMenu::modal(UiModal::Confirm {
            // The write executes only against this same PR and head.
            action: ConfirmAction::Github {
                key: key.to_owned(),
                ship,
                pin: Some(PrPin {
                    number,
                    head_sha: pr.head_sha.clone(),
                }),
            },
            title,
            lines: vec![format!(
                "{}: {}",
                row.branch,
                pr.title.as_deref().unwrap_or_default()
            )],
            cancel_key: Some(if ship { 'E' } else { 'e' }),
        })),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BoardRow, DisplayPolicy};

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
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn notice(menu: Menu) -> String {
        match menu {
            Menu::Local(LocalMenu::Notice(text)) => text,
            other => panic!("expected a notice, got {other:?}"),
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
        let Menu::Local(LocalMenu::Modal { modal, .. }) = menu(&board, "one", true) else {
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

    #[test]
    fn missing_pr_data_is_left_to_the_host() {
        for board in [
            Board::default(),
            board(None, None),
            board(
                Some(PrPresentation {
                    error: Some("gh failed".into()),
                    ..Default::default()
                }),
                None,
            ),
        ] {
            assert!(matches!(menu(&board, "one", true), Menu::Host { .. }));
        }
    }
}
