//! The notify menu's commits: a watch that tells the person, set on or
//! cancelled from a board row.
//!
//! The menu was built from a board that may be older than the store. Its
//! commit carries the creation stamp of the worktree it showed, and the store
//! compares that with the slug's current one inside the write's own
//! transaction, so a menu left open across a remove-and-recreate never sets
//! a watch on the new worktree or cancels one of its watches.

use anyhow::{Result, bail};
use wt_app_core::{context::AppContext, host_errors::stale};
use wt_core::watch::{Condition, LocalClock, WatchActions, parse};
use wt_ops::lifecycle_ops::resolve_key;
use wt_store::{NewWatch, WatchRefusal};
use wt_tui::{NotifyChange, UiReply};

/// Who a watch set from the board belongs to.
const PERSON: &str = "human";

pub async fn set_notify(
    ctx: &AppContext,
    key: &str,
    change: NotifyChange,
    seen: Option<String>,
) -> Result<UiReply> {
    let row = resolve_key(ctx, key).await?;
    if row.is_main {
        bail!("the main clone has nothing to wait for");
    }
    let slug = row.target.slug().to_owned();
    let text = match change {
        NotifyChange::Add { state } => add(ctx, slug, &state, seen).await?,
        NotifyChange::Cancel { id } => {
            let owner = slug.clone();
            let cancelled = ctx
                .database
                .call(move |store| {
                    Ok(store.cancel_watch_seen(&id, &owner, PERSON, seen.as_deref())?)
                })
                .await?;
            match cancelled {
                Some(watch) => format!("{slug}: no longer waiting for {}", watch.summary()),
                None => return Err(stale("that notification is no longer pending")),
            }
        }
    };
    Ok(UiReply {
        message: text,
        ..Default::default()
    })
}

async fn add(ctx: &AppContext, slug: String, state: &str, seen: Option<String>) -> Result<String> {
    let now = wt_app_core::feeds::epoch_ms();
    let clock = LocalClock {
        now_ms: i64::try_from(now).unwrap_or(i64::MAX),
        utc_offset_minutes: wt_app_core::display_time::local_utc_offset_minutes(now).unwrap_or(0),
    };
    // The menu offers the row's own states. Anything else was not built by
    // it, and `wt notify` is where the full grammar is checked.
    let cond = match parse(state, clock) {
        Ok(cond) if matches!(cond.cond, Condition::Own(_)) && cond.delay.is_none() => cond,
        _ => bail!("the notify menu cannot wait for `{state}`; use `wt notify`"),
    };
    let new = NewWatch {
        slug: slug.clone(),
        by: PERSON.into(),
        utc_offset_minutes: clock.utc_offset_minutes,
        when: vec![cond],
        then: WatchActions {
            notify: Some(String::new()),
            ..WatchActions::default()
        },
    };
    let added = ctx
        .database
        .call(move |store| Ok(store.add_watch_seen(new, clock.now_ms, seen.as_deref())?))
        .await?;
    match added {
        Ok(watch) => Ok(format!("{slug}: wt tells you {}", watch.when_summary())),
        Err(WatchRefusal::OwnerChanged) => Err(stale(
            "the worktree was removed or created again since the menu opened",
        )),
        Err(WatchRefusal::AlreadySet { .. }) => Err(stale("that notification is already set")),
        Err(refusal) => bail!("{refusal}"),
    }
}

#[cfg(test)]
mod tests {
    use wt_app_core::host_errors::error_reply;

    use super::*;

    async fn fixture() -> (wt_app_core::test_support::CommandFixture, String) {
        let fixture = wt_app_core::test_support::CommandFixture::new()
            .await
            .unwrap();
        let row = wt_ops::commands::resolve::resolve_named_worktree(&fixture.ctx, "one")
            .await
            .unwrap();
        let key = wt_core::worktree_target_key(&row.target);
        fixture
            .ctx
            .database
            .call(|store| Ok(store.record_slug_created("one", "2026-11-01T00:00:00Z")?))
            .await
            .unwrap();
        (fixture, key)
    }

    fn add_state(state: &str) -> NotifyChange {
        NotifyChange::Add {
            state: state.into(),
        }
    }

    async fn watches(ctx: &AppContext) -> Vec<wt_core::watch::Watch> {
        ctx.database
            .call(|store| Ok(store.list_watches(Some("one"))?))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn the_menu_sets_and_cancels_the_persons_watch_on_the_row_it_showed() {
        let (fixture, key) = fixture().await;
        let ctx = &fixture.ctx;
        let seen = Some("2026-11-01T00:00:00Z".to_owned());
        let reply = set_notify(ctx, &key, add_state("merged"), seen.clone())
            .await
            .unwrap();
        assert_eq!(reply.message, "one: wt tells you when merged");
        let [watch] = watches(ctx).await.try_into().unwrap();
        assert_eq!(watch.by, "human");
        assert!(watch.then.notify_only());
        assert_eq!(watch.incarnation, seen);

        // The same choice again: the menu was older than the store.
        let error = set_notify(ctx, &key, add_state("merged"), seen.clone())
            .await
            .unwrap_err();
        assert!(error_reply(&error).stale);
        assert_eq!(watches(ctx).await.len(), 1);

        let reply = set_notify(
            ctx,
            &key,
            NotifyChange::Cancel {
                id: watch.id.clone(),
            },
            seen.clone(),
        )
        .await
        .unwrap();
        assert_eq!(reply.message, "one: no longer waiting for merged");
        assert!(watches(ctx).await.is_empty());
        // It is gone now: a second cancel has nothing to act on.
        let error = set_notify(ctx, &key, NotifyChange::Cancel { id: watch.id }, seen)
            .await
            .unwrap_err();
        assert!(error_reply(&error).stale);
    }

    #[tokio::test]
    async fn a_menu_about_an_earlier_worktree_writes_nothing() {
        let (fixture, key) = fixture().await;
        let ctx = &fixture.ctx;
        let agents = ctx
            .database
            .call(|store| {
                Ok(store.add_watch(
                    NewWatch {
                        slug: "one".into(),
                        by: "one".into(),
                        utc_offset_minutes: 0,
                        when: vec![wt_core::watch::WatchCondition::new(
                            Condition::Own(wt_core::watch::RowState::Ready),
                            None,
                        )],
                        then: WatchActions {
                            notify: Some(String::new()),
                            ..WatchActions::default()
                        },
                    },
                    1,
                )?)
            })
            .await
            .unwrap()
            .unwrap();
        for earlier in [Some("2026-01-01T00:00:00Z".to_owned()), None] {
            let error = set_notify(ctx, &key, add_state("ready"), earlier.clone())
                .await
                .unwrap_err();
            assert!(error_reply(&error).stale, "{error:#}");
            let error = set_notify(
                ctx,
                &key,
                NotifyChange::Cancel {
                    id: agents.id.clone(),
                },
                earlier,
            )
            .await
            .unwrap_err();
            assert!(error_reply(&error).stale, "{error:#}");
        }
        // Nor is an agent's watch the person's to cancel from this menu.
        let error = set_notify(
            ctx,
            &key,
            NotifyChange::Cancel {
                id: agents.id.clone(),
            },
            Some("2026-11-01T00:00:00Z".into()),
        )
        .await
        .unwrap_err();
        assert!(error_reply(&error).stale);
        assert_eq!(watches(ctx).await.len(), 1);
    }

    #[tokio::test]
    async fn only_the_rows_own_states_can_be_asked_for() {
        let (fixture, key) = fixture().await;
        for state in ["+1m", "#3806", "other:merged", "ready+1h", "nonsense"] {
            let error = set_notify(
                &fixture.ctx,
                &key,
                add_state(state),
                Some("2026-11-01T00:00:00Z".into()),
            )
            .await
            .unwrap_err();
            assert!(!error_reply(&error).stale, "{state}");
            assert!(error.to_string().contains("wt notify"), "{error:#}");
        }
        assert!(watches(&fixture.ctx).await.is_empty());
    }
}
