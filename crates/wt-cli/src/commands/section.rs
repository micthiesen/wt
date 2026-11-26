//! `wt section`: the sections themselves live in wt-ops; this resolves
//! which worktrees have landed, so a move here takes the rows a move on
//! the board does.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use wt_app_core::context::AppContext;
use wt_config::InstanceRole;
use wt_ops::commands::section::{SectionArgs, SectionCommand};

use super::landing::{self, RowLanding, Want};

pub async fn run(ctx: &AppContext, args: &SectionArgs) -> Result<i32> {
    let moves_stacks = matches!(args.command, Some(SectionCommand::Mv { only: false, .. }))
        && ctx.config.instance.role != InstanceRole::Worker;
    // With `[ui] owed_sections` on, a landed row is drawn by what it owes
    // wherever it is filed: a move of one is told so.
    let hints = matches!(args.command, Some(SectionCommand::Mv { .. }))
        && ctx.config.ui.owed_sections
        && ctx.config.instance.role != InstanceRole::Worker;
    if !moves_stacks && !hints {
        return wt_ops::commands::section::run(ctx, args, &BTreeSet::new()).await;
    }
    let rows = ctx.repository.inventory(&ctx.cancellation).await?;
    let state = ctx
        .database
        .call(|store| Ok(store.read_wt_state()?))
        .await?;
    let landings = landing::read(ctx, &rows, &state, Want::Phases).await;
    let detached = if moves_stacks {
        landed(&landings)
    } else {
        BTreeSet::new()
    };
    let code = wt_ops::commands::section::run(ctx, args, &detached).await?;
    if let (0, true, Some(SectionCommand::Mv { positionals, .. })) = (code, hints, &args.command) {
        let now_ms = super::watch::now_ms();
        for name in &positionals[..positionals.len().saturating_sub(1)] {
            let Some((slug, landing)) = landings.get_key_value(name) else {
                continue;
            };
            let owed = super::owed::derive(&state, slug, *landing, now_ms);
            if let Some(hint) = block_hint(slug, owed.phase, &owed.items) {
                println!("{hint}");
            }
        }
    }
    Ok(code)
}

/// What a move of a landed row is told while `[ui] owed_sections` is on:
/// the board draws it under a block derived from what it owes. `None` for
/// a row that has not landed.
fn block_hint(
    slug: &str,
    phase: Option<wt_core::OwedPhase>,
    items: &[wt_core::OwedItem],
) -> Option<String> {
    let block = wt_tui::OwedBlock::placed(phase?, items.first().and_then(|item| item.stage));
    Some(format!(
        "  {slug} has landed: the board shows it under {} while {} is on, whatever section it is filed in",
        block.title(),
        wt_tui::OWED_SECTIONS_OPTION
    ))
}

/// The worktrees a move detaches from their stacks: those known to have
/// landed. An unknown landing detaches nothing, so the whole stack moves.
fn landed(landings: &BTreeMap<String, RowLanding>) -> BTreeSet<String> {
    landings
        .iter()
        .filter(|(_, landing)| landing.is_landed())
        .map(|(slug, _)| slug.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use wt_core::Landing;

    #[test]
    fn moving_a_landed_row_says_where_the_board_shows_it() {
        use wt_core::{OwedPhase, VerifyStage};
        assert_eq!(block_hint("open", None, &[]), None);
        assert_eq!(
            block_hint("done", Some(OwedPhase::Done), &[]).as_deref(),
            Some(
                "  done has landed: the board shows it under Done while [ui] owed_sections is on, whatever section it is filed in"
            )
        );
        let mut record = wt_core::WorkStatusRecord::new(wt_core::WorkState::Ready, "t");
        record.set_check(VerifyStage::Production, "watch the logs");
        let landing = Landing::new(true, false, true);
        let items = wt_core::owed_items(Some(&record), &landing, &[], &[], 0);
        assert!(
            block_hint("beta", Some(OwedPhase::Waiting), &items)
                .unwrap()
                .contains("under Waiting for production while")
        );
    }

    #[test]
    fn only_a_known_landing_detaches_a_row_from_its_stack() {
        let landings = BTreeMap::from([
            (
                "squashed".to_owned(),
                RowLanding::known(Landing::new(true, false, true)),
            ),
            ("open".to_owned(), RowLanding::known(Landing::NONE)),
            ("unread".to_owned(), RowLanding::unknown(Landing::NONE)),
        ]);
        assert_eq!(landed(&landings), BTreeSet::from(["squashed".to_owned()]));
    }
}
