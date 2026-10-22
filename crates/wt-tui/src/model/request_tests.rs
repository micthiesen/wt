use super::*;
use crate::{ConfirmAction, RemovalRevision, UiModal, UiReply};
use crossterm::event::{KeyCode, KeyEvent};
use wt_runtime::{SourceSnapshot, SourceState};

fn board(keys: &[&str]) -> SourceSnapshot<Board> {
    SourceSnapshot::test_live(
        Some(Board {
            rows: keys
                .iter()
                .map(|key| BoardRow {
                    key: (*key).into(),
                    title: (*key).into(),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }),
        SourceState::Ready,
        wt_core::Domain::Git,
    )
}

fn request_remove(model: &mut Model, id: u64) -> u64 {
    // Host-picked rows can be absent from this board and still need a host
    // preparation. Ordinary delete keys now open their menu locally.
    let action = UiAction::OnHost {
        host: Some("builder".into()),
        action: Box::new(UiAction::PrepareRemove {
            key: model.selected_row().unwrap().key.clone(),
        }),
    };
    model.begin_action(id, &action);
    model.toast = Some(("Loading menu… Esc cancels".into(), ToastTone::Info));
    model.ui_generation
}

#[test]
fn progress_is_nonterminal_and_cannot_overwrite_a_newer_interaction() {
    let mut model = Model::default();
    model.apply(board(&["one"]));
    let generation = request_remove(&mut model, 7);
    let progress = || UiReply {
        operation_id: Some(7),
        ui_generation: Some(generation),
        progress: Some("prepare removal: waiting for command capacity (3s)".into()),
        ..Default::default()
    };
    model.reply(progress());
    assert_eq!(model.pending_request.as_ref().unwrap().id, 7);
    assert!(
        model
            .toast
            .as_ref()
            .unwrap()
            .0
            .contains("waiting for command capacity")
    );
    model.reply(confirmation(7, generation, "one"));
    assert!(model.pending_request.is_none());
    assert!(matches!(model.interaction, Interaction::Confirm(_)));
    let feedback = model.toast.clone();
    model.reply(progress());
    assert_eq!(model.toast, feedback);
    assert!(matches!(model.interaction, Interaction::Confirm(_)));
}

#[test]
fn default_harness_key_remains_available_while_creation_is_pending() {
    let mut model = Model::default();
    model.apply(board(&["one"]));
    model.begin_action(
        1,
        &crate::UiAction::Create {
            placement: None,
            input: "new".into(),
        },
    );
    assert_eq!(
        model.input(KeyEvent::from(KeyCode::BackTab), 20),
        InputResult::Action(crate::UiAction::CyclePrimary)
    );
    model.begin_action(2, &crate::UiAction::CyclePrimary);
    let generation = model.ui_generation;
    model.reply(UiReply {
        operation_id: Some(1),
        ui_generation: Some(generation),
        progress: Some("creating…".into()),
        ..Default::default()
    });
    assert_eq!(model.pending_request.as_ref().unwrap().id, 2);
    model.reply(UiReply {
        operation_id: Some(2),
        ui_generation: Some(generation),
        primary_harness: Some("Pi".into()),
        message: "Primary harness: Pi".into(),
        ..Default::default()
    });
    assert_eq!(model.toast.as_ref().unwrap().0, "Primary harness: Pi");
    assert!(model.pending.contains(1));
}

fn confirmation(id: u64, generation: u64, key: &str) -> UiReply {
    UiReply {
        operation_id: Some(id),
        ui_generation: Some(generation),
        modal: Some(UiModal::Confirm {
            title: "Remove worktree?".into(),
            lines: vec![],
            cancel_key: Some('d'),
            action: ConfirmAction::Remove {
                key: key.into(),
                force: false,
                revision: Some(RemovalRevision {
                    key: key.into(),
                    path: format!("/fixture/{key}"),
                    branch: key.into(),
                    head: "head".into(),
                    digest: "digest".into(),
                    hazards: vec![],
                    published_base: None,
                }),
                seen: None,
            },
        }),
        ..Default::default()
    }
}

#[test]
fn second_removal_menu_survives_first_removal_completion_and_inventory_refresh() {
    let mut model = Model::default();
    model.apply(board(&["first", "second"]));
    model.input(KeyEvent::from(KeyCode::Char('j')), 20);
    let generation = request_remove(&mut model, 2);
    let feedback = model.toast.clone();
    model.reply(UiReply {
        operation_id: Some(1),
        message: "Removed first".into(),
        ..Default::default()
    });
    model.apply(board(&["second"]));
    model.expire_toast();
    assert_eq!(model.toast, feedback);
    assert_eq!(model.pending_request.as_ref().unwrap().id, 2);
    model.reply(confirmation(2, generation, "second"));
    assert!(model.pending_request.is_none());
    assert!(
        matches!(&model.interaction, Interaction::Confirm(prompt) if matches!(&prompt.action, ConfirmAction::Remove {key,..} if key=="second"))
    );
}

#[test]
fn failed_preparation_finishes_loading_and_reports_the_error() {
    let mut model = Model::default();
    model.apply(board(&["one"]));
    let generation = request_remove(&mut model, 7);
    model.reply(UiReply {
        operation_id: Some(7),
        ui_generation: Some(generation),
        ..UiReply::failure("Could not inspect checkout")
    });
    assert!(model.pending_request.is_none());
    assert_eq!(
        model.toast,
        Some(("Could not inspect checkout".into(), ToastTone::Failed))
    );
    model.expire_toast();
    assert_eq!(
        model.toast,
        Some(("Could not inspect checkout".into(), ToastTone::Failed))
    );
    model.reply(UiReply::default());
    assert_eq!(
        model.toast,
        Some(("Could not inspect checkout".into(), ToastTone::Failed))
    );
}

#[test]
fn abandoning_a_menu_cancels_its_read() {
    let mut model = Model::default();
    model.apply(board(&["one", "two"]));
    request_remove(&mut model, 7);
    let cancellation = tokio_util::sync::CancellationToken::new();
    model.pending_request.as_mut().unwrap()._cancel_on_drop =
        Some(cancellation.clone().drop_guard());
    // Navigation typed while the menu loads belongs to the menu.
    model.input(KeyEvent::from(KeyCode::Char('j')), 20);
    assert!(!cancellation.is_cancelled());
    assert_eq!(model.selected_row().unwrap().key, "one");
    model.input(KeyEvent::from(KeyCode::Esc), 20);
    assert!(cancellation.is_cancelled());
    assert!(model.pending_request.is_none());
}

#[test]
fn repeated_delete_closes_the_local_menu_without_enqueuing_work() {
    let mut model = Model::default();
    model.apply(board(&["one"]));
    assert_eq!(
        model.input(KeyEvent::from(KeyCode::Char('d')), 20),
        InputResult::Draw
    );
    assert!(matches!(model.interaction, Interaction::Log { .. }));
    assert!(model.pending_request.is_none());
    assert_eq!(
        model.input(KeyEvent::from(KeyCode::Char('d')), 20),
        InputResult::Draw
    );
    assert!(model.pending_request.is_none());
    assert!(matches!(model.interaction, Interaction::None));
}

#[test]
fn all_foreground_requests_keep_feedback_until_their_own_reply() {
    let mut model = Model::default();
    model.apply(board(&["one"]));
    model.begin_action(8, &UiAction::OpenEditor { key: "one".into() });
    model.toast = Some(("Working…".into(), ToastTone::Info));
    model.reply(UiReply {
        operation_id: Some(9),
        ..UiReply::failure("unrelated failure")
    });
    model.expire_toast();
    assert_eq!(model.toast.as_ref().unwrap().0, "Working…");
    model.reply(UiReply {
        operation_id: Some(8),
        ..Default::default()
    });
    assert_eq!(model.toast.as_ref().unwrap().0, "Done");
}

#[test]
fn leaving_session_entry_cancels_the_connection() {
    for key in [KeyCode::Esc, KeyCode::Down, KeyCode::F(10)] {
        let mut model = Model::default();
        model.apply(board(&["one", "two"]));
        let InputResult::Action(action) = model.input(KeyEvent::from(KeyCode::F(12)), 20) else {
            panic!("session action")
        };
        assert!(action.is_cancellable());
        model.begin_action(7, &action);
        let cancellation = tokio_util::sync::CancellationToken::new();
        model.pending_request.as_mut().unwrap()._cancel_on_drop =
            Some(cancellation.clone().drop_guard());

        model.input(KeyEvent::from(key), 20);

        assert!(cancellation.is_cancelled(), "{key:?}");
        assert!(model.pending_request.is_none());
    }
}

#[test]
fn accepted_terminal_handoff_keeps_the_attach_client_alive() {
    let mut model = Model::default();
    model.apply(board(&["one"]));
    let InputResult::Action(action) = model.input(KeyEvent::from(KeyCode::F(12)), 20) else {
        panic!("session action")
    };
    model.begin_action(7, &action);
    let cancellation = tokio_util::sync::CancellationToken::new();
    model.pending_request.as_mut().unwrap()._cancel_on_drop =
        Some(cancellation.clone().drop_guard());
    let (ready, mut suspended) = tokio::sync::oneshot::channel();
    let (_resume, resumed) = tokio::sync::oneshot::channel();

    let handoff = model
        .reply(UiReply {
            operation_id: Some(7),
            ui_generation: Some(model.ui_generation),
            handoff: Some(crate::TerminalHandoff { ready, resumed }),
            ..Default::default()
        })
        .unwrap();

    assert!(model.pending_request.is_none());
    assert!(!cancellation.is_cancelled());
    handoff.ready.send(()).unwrap();
    assert_eq!(suspended.try_recv(), Ok(()));
}
