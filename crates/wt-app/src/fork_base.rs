//! Shared CLI/TUI fork-base mutation. Clearing a parent retains the replay
//! anchor that proves which commits belong to that parent after a squash merge.
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

use anyhow::{Context, Result, bail};
use wt_platform::lock::FileLock;
use wt_store::BaseUpdate;

use crate::{commands::resolve::run_git, context::AppContext};

/// Records `base` (`None` returns to trunk) for the worktree `key`. `seen` is
/// the recorded base the menu showed, when a menu asked; a mismatch is a stale
/// refusal so the menu reopens from current state.
pub async fn set(
    ctx: &AppContext,
    key: &str,
    base: Option<String>,
    seen: Option<Option<String>>,
) -> Result<(String, String)> {
    let clearing = base.is_none();
    let base = base.unwrap_or_else(|| ctx.config.branch.base.clone());
    let base = base.strip_prefix("origin/").unwrap_or(&base).to_owned();
    let inventory = ctx.repository.inventory(&ctx.cancellation).await?;
    let initial = inventory
        .iter()
        .find(|row| !row.is_main && wt_core::worktree_target_key(&row.target) == key)
        .context("worktree disappeared before recording its fork base")?;
    let parent = inventory
        .iter()
        .find(|row| !row.is_main && row.target.branch == base);
    let mut keys = BTreeSet::from([initial.target.slug()]);
    if let Some(parent) = parent {
        keys.insert(parent.target.slug());
    }
    // A parent removal reparents existing children. Hold both endpoints so a
    // delayed assignment cannot resurrect that removed relationship afterward.
    // All multi-worktree operations acquire in lexical order.
    let mut locks = Vec::new();
    for slug in keys {
        locks.push(
            FileLock::acquire(
                &ctx.config.paths.lock_dir,
                slug,
                "set fork base",
                &ctx.cancellation,
            )
            .await?,
        );
    }
    let fresh = ctx.repository.inventory(&ctx.cancellation).await?;
    let row = fresh
        .iter()
        .find(|row| !row.is_main && wt_core::worktree_target_key(&row.target) == key)
        .context("worktree disappeared while waiting to record its fork base")?;
    if row.target != initial.target {
        bail!("worktree changed while waiting to record its fork base");
    }
    let fresh_parent = fresh
        .iter()
        .find(|row| !row.is_main && row.target.branch == base);
    if fresh_parent.map(|row| &row.target) != parent.map(|row| &row.target) {
        bail!(
            "parent worktree changed while waiting to record the fork base; retry from the current inventory"
        );
    }
    let slug = row.target.slug().to_owned();
    let lookup = slug.clone();
    let previous = ctx
        .database
        .call(move |store| Ok(store.read_slug_state(&lookup)?))
        .await?;
    let previous = previous.unwrap_or_default();
    let trunk = ctx.config.branch.base.clone();
    let recorded = wt_core::recorded_fork_base(previous["baseBranch"].as_str(), &trunk);
    if let Some(seen) = &seen
        && seen.as_deref() != recorded
    {
        return Err(crate::controller::stale(format!(
            "fork base for {slug} changed since the picker was opened"
        )));
    }
    {
        let (state, archived) = ctx
            .database
            .call(|store| Ok((store.read_wt_state()?, store.read_archived_keys()?)))
            .await?;
        let records = fresh
            .iter()
            .map(|other| {
                let other_slug = other.target.slug();
                (
                    other.target.branch.as_str(),
                    other_slug,
                    state["slugs"][other_slug]["baseBranch"].as_str(),
                    other.is_main,
                    other.detached,
                    archived.contains(&wt_core::worktree_target_key(&other.target)),
                )
            })
            .collect::<Vec<_>>();
        let candidates = records
            .iter()
            .map(|&(branch, slug, base, is_main, detached, is_archived)| {
                wt_core::ForkBaseCandidate {
                    slug,
                    branch,
                    base,
                    is_main,
                    detached,
                    archived: is_archived,
                }
            })
            .collect::<Vec<_>>();
        let chooser = wt_core::ForkBaseChooser {
            slug: &slug,
            branch: &row.target.branch,
        };
        let requested = (!clearing).then_some(base.as_str());
        if !wt_core::fork_base_allowed(&trunk, chooser, recorded, &candidates, requested) {
            bail!("{base} is not an eligible fork base for {slug}");
        }
    }
    if base == row.target.branch {
        bail!("a worktree cannot be based on itself");
    }
    let path = Path::new(&row.target.path);
    let anchor = if clearing && previous["baseSha"].as_str().is_some() {
        previous["baseSha"].as_str().unwrap().to_owned()
    } else {
        let mut found = None;
        for reference in [base.clone(), format!("origin/{base}")] {
            let exists = run_git(
                ctx,
                path,
                [
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    "--end-of-options",
                    &format!("{reference}^{{commit}}"),
                ],
            )
            .await?;
            if exists.status.success() {
                found = Some(reference);
                break;
            }
        }
        let reference = found.context("fork base does not resolve to a commit")?;
        run_git(ctx, path, ["merge-base", "HEAD", &reference])
            .await?
            .checked("git")?
            .stdout_text()
            .trim()
            .to_owned()
    };
    let branches: BTreeMap<_, _> = fresh
        .into_iter()
        .map(|row| (row.target.branch.clone(), row.target.slug().to_owned()))
        .collect();
    let result = (base.clone(), anchor.clone());
    let outcome = ctx
        .database
        .call(move |store| {
            Ok(store.set_slug_base_checked(
                &slug,
                (
                    previous["baseBranch"].as_str(),
                    previous["baseSha"].as_str(),
                ),
                &base,
                &anchor,
                &branches,
            )?)
        })
        .await?;
    match outcome {
        BaseUpdate::Updated => Ok(result),
        BaseUpdate::Stale => {
            bail!("fork base changed during the operation; retry from the updated state")
        }
        BaseUpdate::Cycle => bail!("that fork base would create a stack cycle"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::test_support::CommandFixture;
    use std::time::Duration;

    #[tokio::test]
    async fn disappearing_parent_is_rechecked_after_its_lock_is_acquired() {
        let fixture = CommandFixture::new().await.unwrap();
        let held_parent =
            FileLock::try_acquire(&fixture.ctx.config.paths.lock_dir, "two", "fixture removal")
                .await
                .unwrap()
                .unwrap();
        let ctx = fixture.ctx.clone();
        let setter =
            tokio::spawn(async move { set(&ctx, "one", Some("feature/two".into()), None).await });
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if FileLock::try_acquire(&fixture.ctx.config.paths.lock_dir, "one", "fixture probe")
                    .await
                    .unwrap()
                    .is_none()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("setter should lock child while waiting for parent");
        assert!(
            !setter.is_finished(),
            "setter must wait for the parent's removal lock"
        );
        let path = fixture.ctx.config.paths.worktree_root.join("two");
        for args in [
            vec!["worktree", "remove", path.to_str().unwrap()],
            vec!["branch", "-D", "feature/two"],
        ] {
            run_git(&fixture.ctx, &fixture.ctx.config.paths.main_clone, args)
                .await
                .unwrap()
                .checked("remove fixture parent")
                .unwrap();
        }
        drop(held_parent);
        let result = tokio::time::timeout(Duration::from_secs(5), setter)
            .await
            .unwrap()
            .unwrap();
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("parent worktree changed")
        );
        let child = fixture
            .ctx
            .database
            .call(|store| Ok(store.read_slug_state("one")?))
            .await
            .unwrap();
        assert!(child.is_none());
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn opposite_parent_changes_do_not_deadlock_or_create_a_cycle() {
        let fixture = CommandFixture::new().await.unwrap();
        let (one, two) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(
                set(&fixture.ctx, "one", Some("feature/two".into()), None),
                set(&fixture.ctx, "two", Some("feature/one".into()), None),
            )
        })
        .await
        .expect("ordered parent/child locks must make progress");
        assert_ne!(
            one.is_ok(),
            two.is_ok(),
            "exactly one competing edge can be accepted"
        );
        let error = one.err().or(two.err()).unwrap();
        // The loser sees the winner's record under the same locks, so the
        // shared eligibility rule refuses it before the store's cycle check.
        assert!(
            error.to_string().contains("not an eligible fork base"),
            "{error:#}"
        );
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn a_base_the_menu_did_not_show_is_refused_as_stale() {
        let fixture = CommandFixture::new().await.unwrap();
        set(&fixture.ctx, "one", Some("feature/two".into()), None)
            .await
            .unwrap();
        // A menu that showed no recorded base must not clear the one recorded
        // since, and the refusal must reopen the menu rather than fail it.
        let error = set(&fixture.ctx, "one", None, Some(None))
            .await
            .unwrap_err();
        assert!(crate::controller::error_reply(&error).stale);
        let state = fixture
            .ctx
            .database
            .call(|store| Ok(store.read_slug_state("one")?))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(state["baseBranch"], "feature/two");
        set(&fixture.ctx, "one", None, Some(Some("feature/two".into())))
            .await
            .unwrap();
        fixture.close().await.unwrap();
    }

    #[tokio::test]
    async fn worktree_branches_must_be_eligible_fork_bases() {
        let fixture = CommandFixture::new().await.unwrap();
        // An archived sibling is never offered, so it is never accepted.
        let two = fixture
            .ctx
            .repository
            .inventory(&fixture.ctx.cancellation)
            .await
            .unwrap()
            .into_iter()
            .find(|row| row.target.slug() == "two")
            .unwrap();
        let archived = wt_core::worktree_target_key(&two.target);
        fixture
            .ctx
            .database
            .call(move |store| Ok(store.set_archived(&archived, true)?))
            .await
            .unwrap();
        let error = set(&fixture.ctx, "one", Some("feature/two".into()), None)
            .await
            .unwrap_err();
        assert!(!crate::controller::error_reply(&error).stale);
        assert!(
            error
                .to_string()
                .contains("not an eligible fork base for one"),
            "{error:#}"
        );
        fixture.close().await.unwrap();
    }
}
