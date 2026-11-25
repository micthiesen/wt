//! One worktree's history: an append-only log beside the repository state.
//!
//! The log is a record, not an authority. Nothing reads it to decide whether
//! a write is safe, and clearing status or layout never "refreshes" it. A
//! slug can be removed and created again, so a slug's visible history starts
//! at its latest `Created` event.
//!
//! Events are ordered by when they happened, then by insertion. A creation
//! that is recorded late (see `log_incarnation` in `mutations.rs`) therefore
//! still sorts before the events of the incarnation it opens. A `created`
//! event is dated when creation began, so what happens while the checkout is
//! being prepared belongs to the new incarnation.
//!
//! An event that rides on a state write is best effort (`EventLog::best_effort`):
//! the state write lands whether or not the event could be stored.

use std::collections::BTreeMap;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params, params_from_iter};
use wt_core::{WorktreeEvent, WorktreeEventKind};

use crate::{Store, StoreError};

/// Events older than this are dropped.
pub const WORKTREE_EVENT_MAX_AGE_MS: i64 = 90 * 24 * 60 * 60 * 1000;
/// The newest events one slug keeps, across its incarnations.
pub const WORKTREE_EVENTS_PER_SLUG: i64 = 500;

pub(crate) const EVENTS_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS worktree_events (
         id INTEGER PRIMARY KEY AUTOINCREMENT,
         repo_id TEXT NOT NULL REFERENCES repositories(repo_id) ON DELETE CASCADE,
         slug TEXT NOT NULL,
         at_ms INTEGER NOT NULL,
         kind TEXT NOT NULL,
         data TEXT NOT NULL
     );
     CREATE INDEX IF NOT EXISTS worktree_events_by_slug
         ON worktree_events (repo_id, slug, at_ms);";

/// Failing check names one event keeps; the rest are counted.
pub const WORKTREE_EVENT_CHECKS_MAX: usize = 6;

/// Cut an event's free text to what the log stores: one bounded line per
/// field and a bounded list of check names. Applying it twice changes
/// nothing, so an observer can bound what it saw before comparing it with
/// what is stored.
pub fn bound_worktree_event(event: &mut WorktreeEventKind) {
    let line = |text: &mut String| *text = wt_core::event_text(text);
    let some = |text: &mut Option<String>| {
        if let Some(text) = text {
            *text = wt_core::event_text(text);
        }
    };
    match event {
        WorktreeEventKind::Created { base } => some(base),
        WorktreeEventKind::Status {
            by,
            note,
            blocked_on,
            verify_in,
            from: _,
            to: _,
            risk: _,
        } => [by, note, blocked_on, verify_in].into_iter().for_each(some),
        WorktreeEventKind::StatusCleared { by, from: _ } => some(by),
        WorktreeEventKind::Checks { failed, state: _ } => {
            failed.iter_mut().for_each(line);
            let counted = failed.len() == WORKTREE_EVENT_CHECKS_MAX + 1
                && failed
                    .last()
                    .is_some_and(|last| last.starts_with('+') && last.ends_with(" more"));
            if failed.len() > WORKTREE_EVENT_CHECKS_MAX && !counted {
                let more = failed.len() - WORKTREE_EVENT_CHECKS_MAX;
                failed.truncate(WORKTREE_EVENT_CHECKS_MAX);
                failed.push(format!("+{more} more"));
            }
        }
        WorktreeEventKind::Review {
            reviewer,
            change: _,
        } => some(reviewer),
        WorktreeEventKind::Automation {
            rule,
            reason,
            outcome: _,
        } => {
            line(rule);
            some(reason);
        }
        WorktreeEventKind::Message {
            from,
            text,
            delivered: _,
        } => {
            some(from);
            line(text);
        }
        WorktreeEventKind::Restack { onto, ok: _ } => some(onto),
        WorktreeEventKind::Watch {
            what,
            by,
            reason,
            then,
            change: _,
            id: _,
            unconfirmed: _,
        } => {
            line(what);
            some(by);
            some(reason);
            then.iter_mut().for_each(line);
        }
        WorktreeEventKind::Skill { name, args, by } => {
            line(name);
            some(args);
            some(by);
        }
        WorktreeEventKind::Mark { text, by } => {
            line(text);
            some(by);
        }
        WorktreeEventKind::Pr { .. }
        | WorktreeEventKind::MergeQueue { .. }
        | WorktreeEventKind::Conflict { .. }
        | WorktreeEventKind::Landed { .. }
        | WorktreeEventKind::Removed { .. }
        | WorktreeEventKind::Unknown => {}
    }
}

/// The slug's most recently inserted `created` or `removed` event. Insertion
/// order, not time, says which lifecycle event is the latest.
pub(crate) struct Lifecycle {
    pub(crate) removed: bool,
    pub(crate) at_ms: i64,
    /// The `createdAt` a `created` event was written for.
    pub(crate) created_at: Option<String>,
}

/// Set once a history write has failed, so a broken log warns once per
/// process and not on every status write.
static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The event log as one write transaction sees it. State mutations append
/// through this so an event commits, or rolls back, with the change it
/// describes.
pub(crate) struct EventLog<'a> {
    db: &'a Connection,
    repo_id: &'a str,
}

impl<'a> EventLog<'a> {
    pub(crate) fn new(db: &'a Connection, repo_id: &'a str) -> Self {
        Self { db, repo_id }
    }

    /// Run history writes that ride on a state write. History must never
    /// cost that write: sessions stay pinned to a build for days, and a later
    /// build may change this table under them. A failure rolls back only
    /// what `write` did to the log and the state write goes on.
    pub(crate) fn best_effort(&self, write: impl FnOnce(&Self) -> Result<(), StoreError>) {
        let result = self
            .db
            .execute_batch("SAVEPOINT wt_history")
            .map_err(StoreError::from)
            .and_then(|()| {
                let written = write(self);
                let settle = if written.is_ok() {
                    "RELEASE wt_history"
                } else {
                    "ROLLBACK TO wt_history; RELEASE wt_history"
                };
                let settled = self.db.execute_batch(settle).map_err(StoreError::from);
                written.and(settled)
            });
        if let Err(error) = result
            && !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            tracing::warn!(%error, "could not record worktree history; state writes continue without it");
        }
    }

    pub(crate) fn append(&self, slug: &str, event: &WorktreeEventKind) -> Result<(), StoreError> {
        self.append_at(slug, crate::store::now_ms(), event)
    }

    pub(crate) fn append_at(
        &self,
        slug: &str,
        at_ms: i64,
        event: &WorktreeEventKind,
    ) -> Result<(), StoreError> {
        // A kind this build cannot name would be stored as an empty object.
        if matches!(event, WorktreeEventKind::Unknown) {
            return Ok(());
        }
        let mut event = event.clone();
        bound_worktree_event(&mut event);
        self.insert(slug, at_ms, serde_json::to_value(&event)?)
    }

    /// Record a creation. `created_at` is the `createdAt` the state holds for
    /// this incarnation; it is kept beside the event (readers ignore it) so
    /// a later write can tell this incarnation's creation from an earlier
    /// one's.
    pub(crate) fn append_created(
        &self,
        slug: &str,
        at_ms: i64,
        base: Option<String>,
        created_at: &str,
    ) -> Result<(), StoreError> {
        let mut event = WorktreeEventKind::Created { base };
        bound_worktree_event(&mut event);
        let mut data = serde_json::to_value(&event)?;
        data["created_at"] = serde_json::Value::String(created_at.to_owned());
        self.insert(slug, at_ms, data)
    }

    fn insert(&self, slug: &str, at_ms: i64, data: serde_json::Value) -> Result<(), StoreError> {
        let kind = data
            .get("kind")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned();
        self.db.execute(
            "INSERT INTO worktree_events (repo_id, slug, at_ms, kind, data) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![self.repo_id, slug, at_ms, kind, data.to_string()],
        )?;
        // Bounded to this slug's rows through the index: at most the cap
        // plus the one just inserted.
        self.db.execute(
            "DELETE FROM worktree_events WHERE repo_id = ?1 AND slug = ?2 AND (at_ms < ?3 OR (at_ms, id) <= \
                 (SELECT at_ms, id FROM worktree_events WHERE repo_id = ?1 AND slug = ?2 \
                  ORDER BY at_ms DESC, id DESC LIMIT 1 OFFSET ?4))",
            params![
                self.repo_id,
                slug,
                at_ms.saturating_sub(WORKTREE_EVENT_MAX_AGE_MS),
                WORKTREE_EVENTS_PER_SLUG
            ],
        )?;
        Ok(())
    }

    pub(crate) fn lifecycle(&self, slug: &str) -> Result<Option<Lifecycle>, StoreError> {
        let latest: Option<(String, i64, String)> = self
            .db
            .query_row(
                "SELECT kind, at_ms, data FROM worktree_events WHERE repo_id = ?1 AND slug = ?2 \
                 AND kind IN ('created', 'removed') ORDER BY id DESC LIMIT 1",
                params![self.repo_id, slug],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        Ok(latest.map(|(kind, at_ms, data)| Lifecycle {
            removed: kind == "removed",
            at_ms,
            created_at: serde_json::from_str::<serde_json::Value>(&data)
                .ok()
                .and_then(|data| data.get("created_at")?.as_str().map(str::to_owned)),
        }))
    }

    /// Whether the slug's latest lifecycle event says it was removed.
    pub(crate) fn removed(&self, slug: &str) -> Result<bool, StoreError> {
        Ok(self.lifecycle(slug)?.is_some_and(|latest| latest.removed))
    }

    /// Whether the slug already holds as many events as it may keep.
    pub(crate) fn full(&self, slug: &str) -> Result<bool, StoreError> {
        let count: i64 = self.db.query_row(
            "SELECT COUNT(*) FROM worktree_events WHERE repo_id = ?1 AND slug = ?2",
            params![self.repo_id, slug],
            |row| row.get(0),
        )?;
        Ok(count >= WORKTREE_EVENTS_PER_SLUG)
    }
}

impl Store {
    /// Record one event that happened now.
    pub fn append_worktree_event(
        &mut self,
        slug: &str,
        at_ms: u64,
        event: &WorktreeEventKind,
    ) -> Result<(), StoreError> {
        let id = self.identity().id.clone();
        let db = self.writable_db()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        EventLog::new(&tx, &id).append_at(slug, clamp_ms(at_ms), event)?;
        tx.commit()?;
        Ok(())
    }

    /// Append what `decide` derives from each slug's current history, and
    /// only that. `decide` sees the slug's events since its latest
    /// `Created`, oldest first, and returns the events to add; returning the
    /// same facts twice adds nothing, so observers in two processes, or one
    /// observer across a restart, converge on one record.
    ///
    /// The first pass reads without the write lock, so an observation that
    /// changes nothing never contends with writers or touches the database
    /// file. Slugs with something to add are decided again under an
    /// immediate transaction, against the history as it is then.
    pub fn reconcile_worktree_events(
        &mut self,
        at_ms: u64,
        slugs: &[String],
        mut decide: impl FnMut(&str, &[WorktreeEvent]) -> Vec<WorktreeEventKind>,
    ) -> Result<usize, StoreError> {
        self.reconcile_worktree_history(at_ms, slugs, |slug, events, _| {
            decide(slug, events)
                .into_iter()
                .map(|event| (None, event))
                .collect()
        })
    }

    /// [`Store::reconcile_worktree_events`] for a decision that also depends
    /// on the slug's stored work status, and that may date an event itself.
    ///
    /// `decide` is handed the status the state holds in the same read as the
    /// events, and in the deciding pass both come from the one write
    /// transaction. A caller's own snapshot of the status can be older than
    /// the log (another process wrote both since), and comparing that with
    /// the log would record a change that already has its event.
    ///
    /// An event's own time is used when given, but never earlier than the
    /// newest event the slug already shows: an event must not sort before
    /// the creation or the history it follows. Otherwise it is dated
    /// `at_ms`.
    pub fn reconcile_worktree_history(
        &mut self,
        at_ms: u64,
        slugs: &[String],
        mut decide: impl FnMut(
            &str,
            &[WorktreeEvent],
            Option<&crate::WorkStatusRecord>,
        ) -> Vec<(Option<u64>, WorktreeEventKind)>,
    ) -> Result<usize, StoreError> {
        if slugs.is_empty() {
            return Ok(0);
        }
        let seen = self.read_worktree_events_for(slugs, WORKTREE_EVENTS_PER_SLUG as usize)?;
        let id = self.identity().id.clone();
        let work = match self.connection() {
            Some(db) => work_records(db, &id)?,
            None => BTreeMap::new(),
        };
        let empty = Vec::new();
        let changed: Vec<String> = slugs
            .iter()
            .filter(|slug| {
                !decide(slug, seen.get(*slug).unwrap_or(&empty), work.get(*slug)).is_empty()
            })
            .cloned()
            .collect();
        if changed.is_empty() {
            return Ok(0);
        }
        let db = self.writable_db()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = read_many(&tx, &id, &changed, WORKTREE_EVENTS_PER_SLUG as usize)?;
        let work = work_records(&tx, &id)?;
        let log = EventLog::new(&tx, &id);
        let now = clamp_ms(at_ms);
        let mut appended = 0;
        for slug in &changed {
            let events = current.get(slug).unwrap_or(&empty);
            let mut newest = events.last().map_or(0, |event| clamp_ms(event.at_ms));
            for (at, event) in decide(slug, events, work.get(slug)) {
                let at = at.map_or(now, clamp_ms).min(now).max(newest);
                log.append_at(slug, at, &event)?;
                newest = at;
                appended += 1;
            }
        }
        tx.commit()?;
        Ok(appended)
    }

    /// A slug's newest `limit` events since its latest `Created`, oldest
    /// first. A removed slug reads as its last incarnation.
    pub fn read_worktree_events(
        &self,
        slug: &str,
        limit: usize,
    ) -> Result<Vec<WorktreeEvent>, StoreError> {
        Ok(self
            .read_worktree_events_for(std::slice::from_ref(&slug.to_owned()), limit)?
            .remove(slug)
            .unwrap_or_default())
    }

    /// [`Store::read_worktree_events`] for many slugs in one query. A slug
    /// with no events is absent.
    pub fn read_worktree_events_for(
        &self,
        slugs: &[String],
        limit: usize,
    ) -> Result<BTreeMap<String, Vec<WorktreeEvent>>, StoreError> {
        let Some(db) = self.connection() else {
            return Ok(BTreeMap::new());
        };
        missing_table_is_empty(read_many(db, &self.identity().id, slugs, limit))
    }

    /// Changes whenever an event is appended. Lets a reader skip a query
    /// that would return what it already holds.
    pub fn worktree_events_revision(&self) -> Result<i64, StoreError> {
        let Some(db) = self.connection() else {
            return Ok(0);
        };
        let revision = db
            .query_row(
                "SELECT seq FROM sqlite_sequence WHERE name = 'worktree_events'",
                [],
                |row| row.get(0),
            )
            .optional();
        missing_table_is_empty(revision.map(Option::unwrap_or_default).map_err(Into::into))
    }

    /// Drop every event past the retention age. Appends already prune their
    /// own slug; this reaches slugs that are never written again.
    pub fn prune_worktree_events(&mut self, now_ms: u64) -> Result<usize, StoreError> {
        let id = self.identity().id.clone();
        let db = self.writable_db()?;
        Ok(db.execute(
            "DELETE FROM worktree_events WHERE repo_id = ?1 AND at_ms < ?2",
            params![
                id,
                clamp_ms(now_ms).saturating_sub(WORKTREE_EVENT_MAX_AGE_MS)
            ],
        )?)
    }
}

/// Each slug's stored work status, as the state row holds it now.
fn work_records(
    db: &Connection,
    repo_id: &str,
) -> Result<BTreeMap<String, crate::WorkStatusRecord>, StoreError> {
    let stored: Option<String> = db
        .query_row(
            "SELECT data FROM repository_state WHERE repo_id = ?1 LIMIT 1",
            [repo_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(stored) = stored else {
        return Ok(BTreeMap::new());
    };
    let state: serde_json::Value = serde_json::from_str(&stored)?;
    Ok(state
        .get("slugs")
        .and_then(serde_json::Value::as_object)
        .into_iter()
        .flatten()
        .filter_map(|(slug, entry)| {
            let work = serde_json::from_value(entry.get("work")?.clone()).ok()?;
            Some((slug.clone(), work))
        })
        .collect())
}

fn clamp_ms(at_ms: u64) -> i64 {
    i64::try_from(at_ms).unwrap_or(i64::MAX)
}

/// A database last migrated by an older build has no event table, and a
/// read-only open cannot add it. That reads as no history.
fn missing_table_is_empty<T: Default>(result: Result<T, StoreError>) -> Result<T, StoreError> {
    match result {
        Err(StoreError::Sqlite(rusqlite::Error::SqliteFailure(_, Some(message))))
            if message.contains("no such table") =>
        {
            Ok(T::default())
        }
        result => result,
    }
}

/// Each slug's current incarnation, newest `limit` events, oldest first.
/// Every reader goes through here, so they agree on where it starts.
///
/// The incarnation is opened by the slug's most recently inserted `created`
/// event. Lifecycle events are taken in insertion order, never by their
/// timestamps: a `removed`, or an older `created`, that was inserted before
/// it belongs to an earlier incarnation even when the clocks say otherwise
/// (the same millisecond, a clock stepped back, a creation dated from its
/// start). Nothing inserted up to the lifecycle event that closed the
/// previous incarnation is included either. Other events inserted since then
/// are placed by time, so a `created` that is recorded late still comes
/// before what was observed while the worktree was being created.
fn read_many(
    db: &Connection,
    repo_id: &str,
    slugs: &[String],
    limit: usize,
) -> Result<BTreeMap<String, Vec<WorktreeEvent>>, StoreError> {
    let mut events = BTreeMap::<String, Vec<WorktreeEvent>>::new();
    // Stays under SQLite's bound-parameter limit on every supported build.
    for chunk in slugs.chunks(500) {
        // Numbered, because the slug list is named twice.
        let placeholders = (0..chunk.len())
            .map(|index| format!("?{}", index + 2))
            .collect::<Vec<_>>()
            .join(", ");
        let mut statement = db.prepare(&format!(
            "WITH opened AS (
                 SELECT slug, MAX(id) AS id FROM worktree_events
                 WHERE repo_id = ?1 AND slug IN ({placeholders}) AND kind = 'created'
                 GROUP BY slug
             ),
             bounds AS (
                 SELECT opened.slug, opened.id, created.at_ms,
                        COALESCE((SELECT MAX(earlier.id) FROM worktree_events earlier
                                  WHERE earlier.repo_id = ?1 AND earlier.slug = opened.slug
                                    AND earlier.kind IN ('created', 'removed')
                                    AND earlier.id < opened.id), 0) AS closed
                 FROM opened JOIN worktree_events created ON created.id = opened.id
             )
             SELECT slug, at_ms, data FROM (
                 SELECT e.slug, e.id, e.at_ms, e.data,
                        ROW_NUMBER() OVER (
                            PARTITION BY e.slug ORDER BY e.at_ms DESC, e.id DESC) AS newest
                 FROM worktree_events e
                 LEFT JOIN bounds ON bounds.slug = e.slug
                 WHERE e.repo_id = ?1 AND e.slug IN ({placeholders})
                   AND (bounds.id IS NULL OR (
                       e.id > bounds.closed
                       AND CASE WHEN e.kind IN ('created', 'removed')
                                THEN e.id >= bounds.id
                                ELSE (e.at_ms, e.id) >= (bounds.at_ms, bounds.id) END))
             ) WHERE newest <= ?{} ORDER BY slug, at_ms, id",
            chunk.len() + 2
        ))?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let values = std::iter::once(rusqlite::types::Value::from(repo_id.to_owned()))
            .chain(chunk.iter().cloned().map(rusqlite::types::Value::from))
            .chain(std::iter::once(rusqlite::types::Value::from(limit)));
        let rows = statement.query_map(params_from_iter(values), |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        for row in rows {
            let (slug, at_ms, data) = row?;
            // An unreadable row is one lost line, not a lost history.
            let event = serde_json::from_str(&data).unwrap_or(WorktreeEventKind::Unknown);
            events.entry(slug).or_default().push(WorktreeEvent {
                at_ms: u64::try_from(at_ms).unwrap_or_default(),
                event,
            });
        }
    }
    Ok(events)
}
