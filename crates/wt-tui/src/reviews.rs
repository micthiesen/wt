//! Requested reviews are not worktrees. Their keys never enter row mutations.
use crate::{
    Model, ReviewRequestRow, UiAction,
    model::{InputResult, VisualItem},
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

impl Model {
    pub(crate) fn selected_review(&self) -> Option<&ReviewRequestRow> {
        match self.selected_item()? {
            VisualItem::ReviewRequest(index) => self.board.review_requests.get(index),
            _ => None,
        }
    }

    pub(crate) fn review_input(&mut self, key: KeyEvent) -> Option<InputResult> {
        if self.history.active || key.modifiers.contains(KeyModifiers::CONTROL) {
            return None;
        }
        if key.code == KeyCode::Tab
            && matches!(
                self.selected_item(),
                Some(VisualItem::ReviewHeader | VisualItem::ReviewRequest(_))
            )
        {
            self.reviews_folded = !self.reviews_folded;
            self.rebuild_items();
            self.selected = Some(0);
            return Some(InputResult::Draw);
        }
        let row = self.selected_review()?;
        let action = match key.code {
            KeyCode::Char('w') => UiAction::PrepareReviewCheckout {
                url: row.url.clone(),
                updated_at: row.updated_at.clone(),
                branch: row.branch.clone(),
            },
            KeyCode::Char('d') => UiAction::DismissReviewRequest {
                url: row.url.clone(),
                updated_at: row.updated_at.clone(),
            },
            KeyCode::Char('p') => {
                return Some(InputResult::Action(UiAction::OpenLink {
                    url: row.url.clone(),
                }));
            }
            KeyCode::Char('l' | 'i') => {
                return Some(
                    row.issue_url
                        .as_ref()
                        .map_or(InputResult::Unchanged, |url| {
                            InputResult::Action(UiAction::OpenLink { url: url.clone() })
                        }),
                );
            }
            _ => return None,
        };
        Some(InputResult::Action(UiAction::OnHost {
            host: row.host.clone(),
            action: Box::new(action),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Board;
    use std::sync::Arc;
    use wt_runtime::{SourceSnapshot, SourceState};

    #[test]
    fn review_identity_survives_refresh_and_cannot_address_a_hidden_worktree() {
        let mut model = Model::default();
        let board = Board {
            review_requests: vec![ReviewRequestRow {
                host: Some("builder".into()),
                url: "https://github.com/a/b/pull/7".into(),
                updated_at: "revision".into(),
                branch: "review".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        model.apply(SourceSnapshot {
            data: Some(Arc::new(board)),
            state: SourceState::Ready,
            ..Default::default()
        });
        model.selected = Some(1);
        assert!(model.selected_row().is_none());
        assert!(
            matches!(model.input(KeyEvent::from(KeyCode::Char('w')), 20),
            InputResult::Action(UiAction::OnHost { host: Some(host), .. }) if host == "builder")
        );
        model.apply(SourceSnapshot {
            data: Some(model.board.clone()),
            state: SourceState::Ready,
            ..Default::default()
        });
        assert_eq!(model.selected, Some(1));
        assert_eq!(
            model.input(KeyEvent::from(KeyCode::Char('t')), 20),
            InputResult::Unchanged
        );
    }
}
