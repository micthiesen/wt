//! `W`: what the person asks to be told about a row, from the row's facts
//! and the watches its host stores.

use wt_core::{Landing, VerifyStage, WorkState};

use super::LocalMenu;
use crate::{
    Board, BoardRow, CheckState, DisplayPolicy, LandingKind, NotifyChange, PickerAction,
    PickerOption, Seen, UiModal, WatchView,
};

/// The row states the menu offers, as watch conditions, with their words
/// and chords. The words match what a condition reads as everywhere else.
/// No chord is a picker movement key (`g` jumps to the top).
pub(crate) const STATES: [(&str, &str, char); 5] = [
    ("ready", "ready", 'r'),
    ("green", "checks green", 'c'),
    ("merged", "merged", 'm'),
    ("staging", "on staging", 's'),
    ("production", "in production", 'p'),
];

/// A state's words, for a watch shown before its host has stored it.
pub(crate) fn state_words(state: &str) -> &str {
    STATES
        .iter()
        .find(|(name, ..)| *name == state)
        .map_or(state, |(_, words, _)| words)
}

/// The person's own watch that waits for exactly this state.
pub(crate) fn set_for<'a>(row: &'a BoardRow, state: &str) -> Option<&'a WatchView> {
    row.watches.iter().find(|watch| {
        watch.is_human_notify()
            && matches!(watch.conditions.as_slice(), [only] if only.key == state)
    })
}

pub(super) fn menu(board: &Board, key: &str) -> LocalMenu {
    let Some(row) = board.rows.iter().find(|row| row.key == key) else {
        return LocalMenu::Notice("worktree is no longer on the board".into());
    };
    if row.is_main {
        return LocalMenu::Notice("the main clone has nothing to wait for".into());
    }
    if row.pending_creation {
        return LocalMenu::Notice("the worktree is still being created".into());
    }
    if row.host.is_some() {
        // The watch would be evaluated and delivered on that host, whose
        // notifications do not reach this desktop.
        return LocalMenu::Notice(
            "a remote host cannot notify this desktop; run `wt notify` on it instead".into(),
        );
    }
    let mut options = Vec::new();
    for (state, words, chord) in STATES {
        let set = set_for(row, state);
        let unavailable = set
            .is_none()
            .then(|| unavailable(row, &board.display, state))
            .flatten();
        let change = match set {
            Some(watch) => Some(NotifyChange::Cancel {
                id: watch.id.clone(),
            }),
            None if unavailable.is_none() => Some(NotifyChange::Add {
                state: state.to_owned(),
            }),
            None => None,
        };
        options.push(PickerOption {
            // A watch this controller is still storing has no id to cancel.
            value: change
                .filter(|change| !matches!(change, NotifyChange::Cancel { id } if id.is_empty()))
                .map(|change| change.value()),
            label: words.to_owned(),
            chord: Some(chord),
            note: None,
            verify_after_merge: None,
            verify_in: None,
            detail: match (set, unavailable) {
                (Some(watch), _) if watch.id.is_empty() => Some("(being set)".into()),
                (Some(_), _) => Some("set · pick to cancel".into()),
                (None, Some(reason)) => Some(format!("({reason})")),
                (None, None) => None,
            },
        });
    }
    // The person's other notifications (`wt notify` from a shell), to cancel.
    let listed = |watch: &WatchView| {
        STATES
            .iter()
            .any(|(state, ..)| set_for(row, state).is_some_and(|set| set.id == watch.id))
    };
    for watch in row
        .watches
        .iter()
        .filter(|watch| watch.is_human_notify() && !watch.id.is_empty() && !listed(watch))
    {
        let when = watch
            .conditions
            .iter()
            .map(|cond| cond.text.as_str())
            .collect::<Vec<_>>()
            .join(" and ");
        options.push(PickerOption {
            value: Some(
                NotifyChange::Cancel {
                    id: watch.id.clone(),
                }
                .value(),
            ),
            label: when,
            chord: None,
            note: None,
            verify_after_merge: None,
            verify_in: None,
            detail: Some("set · pick to cancel".into()),
        });
    }
    let selected = options
        .iter()
        .position(|option| option.value.is_some() && option.detail.is_none())
        .or_else(|| options.iter().position(|option| option.value.is_some()))
        .unwrap_or(0);
    LocalMenu::modal(UiModal::Picker {
        action: PickerAction::Notify {
            key: key.to_owned(),
            seen: Seen(row.created_at.clone()),
        },
        title: format!("Notify me when {} is…", row.slug),
        options,
        selected,
    })
}

/// Why waiting for `state` would be pointless or impossible on this row.
fn unavailable(row: &BoardRow, policy: &DisplayPolicy, state: &str) -> Option<String> {
    let pr = row.pr.as_ref().filter(|pr| pr.number.is_some());
    let merged_pr = pr.is_some_and(|pr| pr.state.as_deref() == Some("MERGED"));
    let landed = row.git.landed_on.is_some() || merged_pr;
    let landing = Landing::new(
        landed,
        row.git.landed_on == Some(LandingKind::Production),
        policy.production,
    )
    // A probed stage the row carries no deploy answer for is not known to
    // be running the change.
    .with_deploys(row.git.deploys.for_probes(|stage| match stage {
        VerifyStage::Staging => policy.staging_probe,
        VerifyStage::Production => policy.production_probe,
    }));
    let stage = |stage: VerifyStage, tracked: bool| {
        let probed = match stage {
            VerifyStage::Staging => policy.staging_probe,
            VerifyStage::Production => policy.production_probe,
        };
        if !tracked && !probed {
            Some("same as merged here".to_owned())
        } else if landing.reached(stage) {
            Some(format!("already {}", state_words(state)))
        } else {
            None
        }
    };
    match state {
        "ready" => {
            let work = row.work.as_ref()?;
            (work.effective_state == Some(WorkState::Ready) && !work.blocked)
                .then(|| "already ready".to_owned())
        }
        "green" => match pr {
            None => Some("no PR".into()),
            Some(_) if landed => Some("already merged".into()),
            Some(pr) if !pr.is_open() => Some("PR is not open".into()),
            Some(pr) if pr.checks == CheckState::Pass => Some("already green".into()),
            Some(_) => None,
        },
        "merged" => landed.then(|| "already merged".to_owned()),
        "staging" => stage(VerifyStage::Staging, false),
        // A separate production branch is tracked by Git alone.
        "production" => stage(VerifyStage::Production, policy.production),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        GitPresentation, PrPresentation, WatchActionKind, WatchConditionView, WorkPresentation,
    };

    fn row() -> BoardRow {
        BoardRow {
            key: "one".into(),
            slug: "one".into(),
            created_at: Some("2026-11-01T00:00:00Z".into()),
            pr: Some(PrPresentation {
                number: Some(7),
                state: Some("OPEN".into()),
                checks: CheckState::Pending,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn notify(id: &str, by: &str, key: &str, text: &str) -> WatchView {
        WatchView {
            id: id.into(),
            by: by.into(),
            conditions: vec![WatchConditionView {
                key: key.into(),
                text: text.into(),
                ..Default::default()
            }],
            actions: vec![WatchActionKind::Notify],
            ..Default::default()
        }
    }

    fn board(row: BoardRow, policy: DisplayPolicy) -> Board {
        Board {
            rows: vec![row],
            display: policy,
            ..Default::default()
        }
    }

    type Choice = (String, Option<String>, Option<String>);

    fn picker(board: &Board) -> (PickerAction, Vec<Choice>, usize) {
        let LocalMenu::Modal { modal, .. } = menu(board, "one") else {
            panic!("expected a picker")
        };
        let UiModal::Picker {
            action,
            options,
            selected,
            ..
        } = *modal
        else {
            panic!("expected a picker")
        };
        let choices = options
            .into_iter()
            .map(|option| (option.label, option.value, option.detail))
            .collect();
        (action, choices, selected)
    }

    fn choice(label: &str, value: Option<&str>, detail: Option<&str>) -> Choice {
        (
            label.to_owned(),
            value.map(str::to_owned),
            detail.map(str::to_owned),
        )
    }

    #[test]
    fn offers_what_can_still_happen_and_says_why_the_rest_cannot() {
        let probed = DisplayPolicy {
            production: true,
            staging_probe: true,
            ..Default::default()
        };
        let (action, choices, selected) = picker(&board(row(), probed.clone()));
        assert_eq!(
            action,
            PickerAction::Notify {
                key: "one".into(),
                seen: Seen(Some("2026-11-01T00:00:00Z".into())),
            }
        );
        assert_eq!(
            choices,
            [
                choice("ready", Some("add:ready"), None),
                choice("checks green", Some("add:green"), None),
                choice("merged", Some("add:merged"), None),
                choice("on staging", Some("add:staging"), None),
                choice("in production", Some("add:production"), None),
            ]
        );
        assert_eq!(selected, 0);

        // No PR, already ready, and nothing that tells a stage from merged.
        let mut bare = row();
        bare.pr = None;
        bare.work = Some(WorkPresentation {
            effective_state: Some(WorkState::Ready),
            ..Default::default()
        });
        let (_, choices, selected) = picker(&board(bare, DisplayPolicy::default()));
        assert_eq!(
            choices,
            [
                choice("ready", None, Some("(already ready)")),
                choice("checks green", None, Some("(no PR)")),
                choice("merged", Some("add:merged"), None),
                choice("on staging", None, Some("(same as merged here)")),
                choice("in production", None, Some("(same as merged here)")),
            ]
        );
        assert_eq!(
            selected, 2,
            "the cursor starts on a choice that can be made"
        );

        // Landed on base, staging probe says deployed, production not yet.
        let mut landed = row();
        landed.pr.as_mut().unwrap().state = Some("MERGED".into());
        landed.git = GitPresentation {
            landed_on: Some(LandingKind::Base),
            deploys: wt_core::StageDeploys {
                staging: Some(true),
                production: None,
            },
            ..Default::default()
        };
        let (_, choices, _) = picker(&board(landed.clone(), probed.clone()));
        assert_eq!(
            choices[1..],
            [
                choice("checks green", None, Some("(already merged)")),
                choice("merged", None, Some("(already merged)")),
                choice("on staging", None, Some("(already on staging)")),
                choice("in production", Some("add:production"), None),
            ]
        );
        // A probed stage whose deploy is not known is not "already there".
        landed.git.deploys = wt_core::StageDeploys::default();
        let (_, choices, _) = picker(&board(landed, probed));
        assert_eq!(choices[3], choice("on staging", Some("add:staging"), None));
    }

    #[test]
    fn a_gated_ready_row_can_still_wait_for_ready() {
        let mut gated = row();
        gated.work = Some(WorkPresentation {
            effective_state: Some(WorkState::Ready),
            blocked: true,
            ..Default::default()
        });
        let (_, choices, _) = picker(&board(gated, DisplayPolicy::default()));
        assert_eq!(choices[0], choice("ready", Some("add:ready"), None));
    }

    #[test]
    fn the_persons_own_watches_are_offered_for_cancelling_by_id() {
        let mut waiting = row();
        waiting.watches = vec![
            notify("aaa222", "human", "merged", "merged"),
            // An agent's watch is not the person's to cancel here.
            notify("bbb333", "one", "ready", "ready"),
            notify("ccc444", "human", "at:1795539600000", "17:00 today"),
            // Stored after the menu's own request, not yet read back.
            notify("", "human", "green", "checks green"),
            WatchView {
                actions: vec![WatchActionKind::Wake],
                ..notify("ddd555", "human", "production", "in production")
            },
        ];
        let policy = DisplayPolicy {
            production: true,
            ..Default::default()
        };
        let (_, choices, selected) = picker(&board(waiting, policy));
        assert_eq!(
            choices,
            [
                choice("ready", Some("add:ready"), None),
                choice("checks green", None, Some("(being set)")),
                choice(
                    "merged",
                    Some("cancel:aaa222"),
                    Some("set · pick to cancel")
                ),
                choice("on staging", None, Some("(same as merged here)")),
                choice("in production", Some("add:production"), None),
                choice(
                    "17:00 today",
                    Some("cancel:ccc444"),
                    Some("set · pick to cancel")
                ),
            ]
        );
        assert_eq!(selected, 0);
        for (label, value, _) in &choices {
            if let Some(value) = value {
                assert!(
                    NotifyChange::from_value(value).is_some(),
                    "{label}: {value}"
                );
            }
        }
    }

    #[test]
    fn answers_every_board_and_refuses_rows_it_cannot_serve() {
        let notice = |board: &Board| match menu(board, "one") {
            LocalMenu::Notice(text) => text,
            other => panic!("expected a notice, got {other:?}"),
        };
        assert_eq!(
            notice(&Board::default()),
            "worktree is no longer on the board"
        );
        let mut remote = row();
        remote.host = Some("builder".into());
        assert!(notice(&board(remote, DisplayPolicy::default())).contains("remote host"));
        let mut main = row();
        main.is_main = true;
        assert!(notice(&board(main, DisplayPolicy::default())).contains("main clone"));
        let mut creating = row();
        creating.pending_creation = true;
        assert!(notice(&board(creating, DisplayPolicy::default())).contains("being created"));
        assert_eq!(NotifyChange::from_value("add:"), None);
        assert_eq!(NotifyChange::from_value("3"), None);
        assert_eq!(
            NotifyChange::from_value("cancel:abc234"),
            Some(NotifyChange::Cancel {
                id: "abc234".into()
            })
        );
    }
}
