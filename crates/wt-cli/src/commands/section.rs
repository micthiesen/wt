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
    let landed = if moves_stacks {
        let rows = ctx.repository.inventory(&ctx.cancellation).await?;
        let state = ctx
            .database
            .call(|store| Ok(store.read_wt_state()?))
            .await?;
        landed(&landing::read(ctx, &rows, &state, Want::Phases).await)
    } else {
        BTreeSet::new()
    };
    wt_ops::commands::section::run(ctx, args, &landed).await
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
