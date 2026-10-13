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
    let InputResult::Action(action) = model.input(KeyEvent::from(KeyCode::Char('d')), 20) else {
        panic!("expected remove preparation")
    };
    model.begin_action(id, &action);
    model.toast = Some(("Loading menu… Esc cancels".into(), ToastTone::Info));
    model.ui_generation
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
                revision: RemovalRevision {
                    key: key.into(),
                    path: format!("/fixture/{key}"),
                    branch: key.into(),
                    head: "head".into(),
                    digest: "digest".into(),
                    hazards: vec![],
                    published_base: None,
                },
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
    model.input(KeyEvent::from(KeyCode::Char('j')), 20);
    assert!(cancellation.is_cancelled());
    assert_eq!(model.selected_row().unwrap().key, "two");
    assert!(model.pending_request.is_none());
}

#[test]
fn repeated_delete_cancels_preparation_without_enqueuing_duplicate_work() {
    let mut model = Model::default();
    model.apply(board(&["one"]));
    let generation = request_remove(&mut model, 7);
    assert_eq!(
        model.input(KeyEvent::from(KeyCode::Char('d')), 20),
        InputResult::Draw
    );
    assert!(model.pending_request.is_none());
    assert_eq!(model.toast.as_ref().unwrap().0, "Menu cancelled");
    model.reply(confirmation(7, generation, "one"));
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
