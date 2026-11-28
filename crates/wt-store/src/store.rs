use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};
use thiserror::Error;

use crate::{ForeignRepositoryRow, RepositoryIdentity, WtState};

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("SQLite state store: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("I/O opening state store: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid repository identity: id and path must be non-empty")]
    InvalidIdentity,
    #[error("repository namespace collision: {id} belongs to {actual}, not {expected}")]
    RepositoryCollision {
        id: String,
        actual: String,
        expected: String,
    },
    #[error("repository path already belongs to a different state namespace: {0}")]
    PathCollision(String),
    #[error("state database requires a writable open to apply its schema")]
    NeedsMigration,
    #[error("state store was opened read-only")]
    ReadOnly,
    #[error("repository state contains invalid JSON: {0}")]
    InvalidStateJson(#[from] serde_json::Error),
    #[error("set title for {slug}: a title is one nonempty line")]
    InvalidTitle { slug: String },
    #[error("set title for {slug}: title revision exhausted")]
    TitleRevisionExhausted { slug: String },
}

/// The size the write-ahead log is cut back to once a checkpoint has emptied
/// it. One rewrite of a repository's state is a few hundred kilobytes.
const WAL_SIZE_LIMIT: i64 = 1024 * 1024;

/// A synchronous owner of a single repository's rows in the shared wt DB.
///
/// Methods requiring mutations take `&mut self`; this intentionally makes the
/// connection easy to confine to one runtime thread without an async mutex.
pub struct Store {
    db: Option<Connection>,
    path: PathBuf,
    identity: RepositoryIdentity,
    read_only: bool,
}

impl Store {
    /// Open (and, when necessary, create/migrate) a writable state database.
    pub fn open(path: impl AsRef<Path>, identity: RepositoryIdentity) -> Result<Self, StoreError> {
        validate_identity(&identity)?;
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut db = Connection::open(&path)?;
        enable_wal(&db, Duration::from_secs(3))?;
        db.busy_timeout(Duration::from_secs(3))?;
        db.pragma_update(None, "foreign_keys", "ON")?;
        // Without a limit the log keeps the size of its largest burst for
        // good: SQLite reuses the file and never shrinks it.
        db.pragma_update(None, "journal_size_limit", WAL_SIZE_LIMIT)?;
        initialize_schema(&mut db)?;
        register_repository(&mut db, &identity)?;
        Ok(Self {
            db: Some(db),
            path,
            identity,
            read_only: false,
        })
    }

    /// Open an existing database without creating files, changing pragmas,
    /// applying migrations, or touching repository timestamps. Missing files
    /// produce an empty read-only store.
    pub fn open_read_only(
        path: impl AsRef<Path>,
        identity: RepositoryIdentity,
    ) -> Result<Self, StoreError> {
        validate_identity(&identity)?;
        let path = path.as_ref().to_path_buf();
        let db = if path.exists() {
            let conn = Connection::open_with_flags(
                &path,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
            )?;
            conn.busy_timeout(std::time::Duration::from_millis(3_000))?;
            ensure_schema_present(&conn)?;
            ensure_identity_matches(&conn, &identity)?;
            Some(conn)
        } else {
            None
        };
        Ok(Self {
            db,
            path,
            identity,
            read_only: true,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn identity(&self) -> &RepositoryIdentity {
        &self.identity
    }

    pub fn has_repository_state(&self) -> Result<bool, StoreError> {
        let Some(db) = &self.db else { return Ok(false) };
        let found = db
            .query_row(
                "SELECT 1 FROM repository_state WHERE repo_id = ?1 LIMIT 1",
                [&self.identity.id],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        Ok(found)
    }

    pub fn read_repository_state_json(&self) -> Result<Option<String>, StoreError> {
        let Some(db) = &self.db else { return Ok(None) };
        let data = db
            .query_row(
                "SELECT data FROM repository_state WHERE repo_id = ?1 LIMIT 1",
                [&self.identity.id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(data)
    }

    /// Read and persist one forward migration when the payload is older than
    /// this build. The immediate transaction re-reads the row under SQLite's
    /// write lock, so two wt processes cannot overwrite one another's state
    /// with a stale pre-migration snapshot. Current and newer payloads are
    /// returned byte-for-byte unchanged. A read-only store only reads.
    pub fn read_repository_state_json_migrated(&mut self) -> Result<Option<String>, StoreError> {
        if self.read_only {
            return self.read_repository_state_json();
        }
        let initial = self.read_repository_state_json()?;
        let Some(initial) = initial else {
            return Ok(None);
        };
        let initial_value: serde_json::Value = serde_json::from_str(&initial)?;
        if crate::raw_wt_state_version(&initial_value) >= crate::CURRENT_WT_STATE_VERSION {
            return Ok(Some(initial));
        }

        let id = self.identity.id.clone();
        let db = self.writable_db()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current: Option<String> = tx
            .query_row(
                "SELECT data FROM repository_state WHERE repo_id = ?1 LIMIT 1",
                [&id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(current) = current else {
            tx.commit()?;
            return Ok(None);
        };
        let raw: serde_json::Value = serde_json::from_str(&current)?;
        let migration = crate::migrate_wt_state(raw);
        if migration.from < crate::CURRENT_WT_STATE_VERSION {
            let migrated = serde_json::to_string(&migration.value)?;
            tx.execute(
                "UPDATE repository_state SET data = ?2, updated_at = ?3 WHERE repo_id = ?1",
                params![id, migrated, now_ms()],
            )?;
            tx.commit()?;
            Ok(Some(migrated))
        } else {
            tx.commit()?;
            Ok(Some(current))
        }
    }

    /// Read the current lossless state envelope. Missing state yields the
    /// canonical empty shape. Older payloads are migrated transactionally;
    /// newer payloads remain untouched and retain all unknown fields.
    pub fn read_wt_state(&mut self) -> Result<WtState, StoreError> {
        let raw = self.read_repository_state_json_migrated()?;
        let Some(raw) = raw else {
            return Ok(crate::mutations::empty_state());
        };
        let value = serde_json::from_str(&raw)?;
        Ok(crate::mutations::normalize_state(value))
    }

    /// Apply one state mutation against a freshly-read snapshot while holding
    /// an SQLite IMMEDIATE transaction. The callback returns `(result, did
    /// change)` so no-op mutations avoid rewriting the durable row.
    pub(crate) fn mutate_wt_state<R>(
        &mut self,
        mutate: impl FnOnce(&mut WtState) -> Result<(R, bool), StoreError>,
    ) -> Result<R, StoreError> {
        self.mutate_wt_state_logged(|state, _| mutate(state))
    }

    /// [`Store::mutate_wt_state`] for a mutation that also records history.
    /// Events appended through the log share the mutation's transaction.
    pub(crate) fn mutate_wt_state_logged<R>(
        &mut self,
        mutate: impl FnOnce(&mut WtState, &crate::events::EventLog<'_>) -> Result<(R, bool), StoreError>,
    ) -> Result<R, StoreError> {
        let id = self.identity.id.clone();
        let db = self.writable_db()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let log = crate::events::EventLog::new(&tx, &id);
        let result = mutate_wt_state_tx(&tx, &id, |state| mutate(state, &log))?;
        tx.commit()?;
        Ok(result)
    }

    pub fn write_repository_state_json(&mut self, data: &str) -> Result<(), StoreError> {
        let id = self.identity.id.clone();
        let db = self.writable_db()?;
        db.execute(
            "INSERT INTO repository_state (repo_id, data, updated_at) VALUES (?1, ?2, ?3) \
             ON CONFLICT(repo_id) DO UPDATE SET data = excluded.data, updated_at = excluded.updated_at",
            params![id, data, now_ms()],
        )?;
        Ok(())
    }

    pub fn read_archived_keys(&self) -> Result<BTreeSet<String>, StoreError> {
        let Some(db) = &self.db else {
            return Ok(BTreeSet::new());
        };
        let mut statement = db.prepare(
            "SELECT worktree_key FROM archived_worktrees WHERE repo_id = ?1 ORDER BY worktree_key",
        )?;
        let keys = statement
            .query_map([&self.identity.id], |row| row.get::<_, String>(0))?
            .collect::<Result<BTreeSet<_>, _>>()?;
        Ok(keys)
    }

    /// The repository state and the archived keys as of one moment: both
    /// are read in one deferred transaction, so a write that lands between
    /// the two statements (from another connection of this process, or
    /// another process) shows in both or in neither.
    pub fn read_wt_state_and_archived_keys(
        &mut self,
    ) -> Result<(WtState, BTreeSet<String>), StoreError> {
        self.read_state_and_archived(|| {})
    }

    /// `between` runs after the state is read and before the keys are.
    pub(crate) fn read_state_and_archived(
        &mut self,
        between: impl FnOnce(),
    ) -> Result<(WtState, BTreeSet<String>), StoreError> {
        // A writable store migrates an older payload in a transaction of
        // its own, which cannot nest in the read below.
        if !self.read_only {
            self.read_repository_state_json_migrated()?;
        }
        let Some(db) = &self.db else {
            return Ok((crate::mutations::empty_state(), BTreeSet::new()));
        };
        let snapshot = db.unchecked_transaction()?;
        let state = match self.read_repository_state_json()? {
            Some(raw) => crate::mutations::normalize_state(serde_json::from_str(&raw)?),
            None => crate::mutations::empty_state(),
        };
        between();
        let archived = self.read_archived_keys()?;
        snapshot.commit()?;
        Ok((state, archived))
    }

    pub fn replace_archived_keys(&mut self, keys: &BTreeSet<String>) -> Result<(), StoreError> {
        let id = self.identity.id.clone();
        let db = self.writable_db()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        replace_archived_keys_tx(&tx, &id, keys)?;
        tx.commit()?;
        Ok(())
    }

    /// Atomically change one archive marker without replacing other keys.
    /// Returns whether the persisted marker changed. Reassertions retain age.
    pub fn set_archived(&mut self, key: &str, archived: bool) -> Result<bool, StoreError> {
        let id = self.identity.id.clone();
        let db = self.writable_db()?;
        let changed = if archived {
            db.execute(
                "INSERT OR IGNORE INTO archived_worktrees (repo_id, worktree_key, archived_at) VALUES (?1, ?2, ?3)",
                params![id, key, now_ms()],
            )?
        } else {
            db.execute(
                "DELETE FROM archived_worktrees WHERE repo_id = ?1 AND worktree_key = ?2",
                params![id, key],
            )?
        };
        Ok(changed != 0)
    }

    /// [`Self::begin_removal_of`] and the row's archive marker in one
    /// transaction. A removal archives its row as it begins, and the board
    /// draws a row being removed from both: written apart, a reader between
    /// the two writes saw a removal with its row still unarchived and drew
    /// it in the wrong place for a frame. Nothing is written, the marker
    /// included, while another attempt for `key` is active.
    #[allow(clippy::too_many_arguments)]
    pub fn begin_removal_archived(
        &mut self,
        key: &str,
        job_id: &str,
        path: &str,
        branch: &str,
        landed: bool,
        was_archived: bool,
    ) -> Result<bool, StoreError> {
        let id = self.identity.id.clone();
        let db = self.writable_db()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let begun = mutate_wt_state_tx(&tx, &id, |state| {
            let begun = crate::mutations::begin_removal_entry(
                state,
                key,
                job_id,
                path,
                branch,
                landed,
                was_archived,
            );
            Ok((begun, begun))
        })?;
        if begun {
            tx.execute(
                "INSERT OR IGNORE INTO archived_worktrees (repo_id, worktree_key, archived_at) VALUES (?1, ?2, ?3)",
                params![id, key, now_ms()],
            )?;
        }
        tx.commit()?;
        Ok(begun)
    }

    /// Archive or restore `key` from the state the user saw. With `seen`, the
    /// write is refused unless the marker still reads `seen`. Archiving folds
    /// `archived_section`; restoring files the row back in the Inbox. The
    /// compare, the marker and the layout share one transaction.
    pub fn toggle_archived_seen(
        &mut self,
        key: &str,
        seen: Option<bool>,
        archived_section: &str,
    ) -> Result<crate::SeenToggle, StoreError> {
        let id = self.identity.id.clone();
        let db = self.writable_db()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM archived_worktrees WHERE repo_id = ?1 AND worktree_key = ?2)",
            params![id, key],
            |row| row.get(0),
        )?;
        if let Some(stale) = crate::SeenToggle::stale(current, seen) {
            return Ok(stale);
        }
        let archived = !current;
        if archived {
            tx.execute(
                "INSERT OR IGNORE INTO archived_worktrees (repo_id, worktree_key, archived_at) VALUES (?1, ?2, ?3)",
                params![id, key, now_ms()],
            )?;
        } else {
            tx.execute(
                "DELETE FROM archived_worktrees WHERE repo_id = ?1 AND worktree_key = ?2",
                params![id, key],
            )?;
        }
        mutate_wt_state_tx(&tx, &id, |state| {
            let changed = if archived {
                crate::mutations::fold_section(state, archived_section, true)
            } else {
                !crate::mutations::move_to_section(state, &[key.to_owned()], None).is_empty()
            };
            Ok(((), changed))
        })?;
        tx.commit()?;
        Ok(crate::SeenToggle::Set(archived))
    }

    /// Reconcile archive markers against a complete inventory under the
    /// caller's inventory/lifecycle lock. Include remote ledger keys in `live`;
    /// a failed or partial enumeration must never call this method.
    pub fn reap_archived_keys(&mut self, live: &BTreeSet<String>) -> Result<usize, StoreError> {
        let id = self.identity.id.clone();
        let db = self.writable_db()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let keys = {
            let mut query =
                tx.prepare("SELECT worktree_key FROM archived_worktrees WHERE repo_id = ?1")?;
            query
                .query_map([&id], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?
        };
        let mut changed = 0;
        for key in keys {
            if !live.contains(&key) {
                changed += tx.execute(
                    "DELETE FROM archived_worktrees WHERE repo_id = ?1 AND worktree_key = ?2",
                    params![id, key],
                )?;
            }
        }
        tx.commit()?;
        Ok(changed)
    }

    /// Import a repository snapshot atomically. Existing archive markers are
    /// retained, matching `importRepositorySnapshot`'s INSERT OR IGNORE
    /// semantics; callers can use `replace_archived_keys` for replacement.
    pub fn import_repository_snapshot(
        &mut self,
        state_json: &str,
        archived: &BTreeSet<String>,
    ) -> Result<(), StoreError> {
        let id = self.identity.id.clone();
        let db = self.writable_db()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = now_ms();
        tx.execute(
            "INSERT INTO repository_state (repo_id, data, updated_at) VALUES (?1, ?2, ?3) \
             ON CONFLICT(repo_id) DO UPDATE SET data = excluded.data, updated_at = excluded.updated_at",
            params![id, state_json, now],
        )?;
        {
            let mut insert = tx.prepare(
                "INSERT OR IGNORE INTO archived_worktrees (repo_id, worktree_key, archived_at) VALUES (?1, ?2, ?3)",
            )?;
            for key in archived {
                insert.execute(params![id, key, now])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Merge a legacy import with current repository data under one write
    /// transaction. Reading before `import_repository_snapshot` would lose a
    /// concurrent process's status/title write between the read and commit.
    /// `None` distinguishes a repository with no prior state from defaults.
    pub fn merge_repository_snapshot(
        &mut self,
        archived: &BTreeSet<String>,
        merge: impl FnOnce(Option<WtState>) -> Result<WtState, StoreError>,
    ) -> Result<(), StoreError> {
        let id = self.identity.id.clone();
        let db = self.writable_db()?;
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let stored: Option<String> = tx
            .query_row(
                "SELECT data FROM repository_state WHERE repo_id = ?1 LIMIT 1",
                [&id],
                |row| row.get(0),
            )
            .optional()?;
        let current = stored
            .map(|text| {
                let parsed: serde_json::Value = serde_json::from_str(&text)?;
                Ok::<_, StoreError>(crate::mutations::normalize_state(
                    crate::migrate_wt_state(parsed).value,
                ))
            })
            .transpose()?;
        let merged = merge(current)?;
        let encoded = serde_json::to_string(&merged)?;
        let now = now_ms();
        tx.execute(
            "INSERT INTO repository_state (repo_id, data, updated_at) VALUES (?1, ?2, ?3) \
             ON CONFLICT(repo_id) DO UPDATE SET data = excluded.data, updated_at = excluded.updated_at",
            params![id, encoded, now],
        )?;
        {
            let mut insert = tx.prepare(
                "INSERT OR IGNORE INTO archived_worktrees (repo_id, worktree_key, archived_at) VALUES (?1, ?2, ?3)",
            )?;
            for key in archived {
                insert.execute(params![id, key, now])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Inspect candidate state databases without mutating or requiring that
    /// they belong to this store's repository.
    pub fn read_foreign_repository_rows(
        path: impl AsRef<Path>,
    ) -> Result<Vec<ForeignRepositoryRow>, StoreError> {
        let path = path.as_ref();
        if !path.exists() {
            return Ok(Vec::new());
        }
        let db = match Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        ) {
            Ok(db) => db,
            Err(_) => return Ok(Vec::new()),
        };
        let rows = (|| {
            let mut statement = db.prepare(
                "SELECT r.repo_id, r.repo_path, s.data, s.updated_at \
                 FROM repositories r JOIN repository_state s ON s.repo_id = r.repo_id",
            )?;
            let mut result = Vec::new();
            let mapped = statement.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })?;
            for row in mapped {
                let (repo_id, repo_path, data, updated_at) = row?;
                let mut archived = BTreeSet::new();
                let mut archived_statement =
                    db.prepare("SELECT worktree_key FROM archived_worktrees WHERE repo_id = ?1")?;
                for key in
                    archived_statement.query_map([&repo_id], |row| row.get::<_, String>(0))?
                {
                    archived.insert(key?);
                }
                result.push(ForeignRepositoryRow {
                    repo_id,
                    repo_path,
                    data,
                    updated_at,
                    archived,
                });
            }
            Ok::<_, rusqlite::Error>(result)
        })();
        match rows {
            Ok(rows) => Ok(rows),
            // Arbitrary files and older schemas are expected inputs here.
            Err(rusqlite::Error::SqliteFailure(_, _))
            | Err(rusqlite::Error::InvalidColumnType(_, _, _))
            | Err(rusqlite::Error::InvalidColumnName(_))
            | Err(rusqlite::Error::InvalidQuery) => Ok(Vec::new()),
            Err(err) => Err(StoreError::Sqlite(err)),
        }
    }

    /// Move what the write-ahead log holds into the database without
    /// waiting for any reader or writer, and return the frames left in the
    /// log. A log that was moved whole is cut to [`WAL_SIZE_LIMIT`] by the
    /// next write.
    pub fn checkpoint(&mut self) -> Result<i64, StoreError> {
        let db = self.writable_db()?;
        let (busy, log, moved): (i64, i64, i64) =
            db.query_row("PRAGMA wal_checkpoint(PASSIVE)", [], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?;
        tracing::debug!(busy, log, moved, "state database checkpoint");
        Ok(log.saturating_sub(moved).max(0))
    }

    pub(crate) fn connection(&self) -> Option<&Connection> {
        self.db.as_ref()
    }

    pub(crate) fn writable_db(&mut self) -> Result<&mut Connection, StoreError> {
        if self.read_only {
            return Err(StoreError::ReadOnly);
        }
        self.db.as_mut().ok_or(StoreError::ReadOnly)
    }
}

fn validate_identity(identity: &RepositoryIdentity) -> Result<(), StoreError> {
    if identity.id.trim().is_empty() || identity.path.trim().is_empty() {
        return Err(StoreError::InvalidIdentity);
    }
    Ok(())
}

fn enable_wal(db: &Connection, timeout: Duration) -> Result<(), rusqlite::Error> {
    // Changing journal mode can return BUSY without invoking SQLite's busy
    // handler when two first opens would deadlock while upgrading their locks.
    // Retry this idempotent, autocommit operation after its statement has been
    // dropped. Never retry an arbitrary transaction or hide a different error.
    let deadline = Instant::now() + timeout;
    loop {
        db.busy_timeout(deadline.saturating_duration_since(Instant::now()))?;
        match db.pragma_update(None, "journal_mode", "WAL") {
            Err(error)
                if error.sqlite_error_code() == Some(rusqlite::ErrorCode::DatabaseBusy)
                    && Instant::now() < deadline =>
            {
                std::thread::sleep(
                    Duration::from_millis(10)
                        .min(deadline.saturating_duration_since(Instant::now())),
                );
            }
            result => return result,
        }
    }
}

#[cfg(test)]
#[path = "wal_tests.rs"]
mod wal_tests;

/// The schema, as the batches that brought it there: entry `n` is version
/// `n + 1`. A new version is a new entry at the end and nothing else, so
/// what an open applies and what it takes for current cannot disagree.
///
/// Version 2 only adds a table. Builds that know version 1 alone check for
/// `version >= 1` and never read the new table, so they keep opening and
/// writing a migrated database.
const MIGRATIONS: [&str; 2] = [
    "CREATE TABLE IF NOT EXISTS repositories (
         repo_id TEXT PRIMARY KEY,
         repo_path TEXT NOT NULL UNIQUE,
         created_at INTEGER NOT NULL,
         updated_at INTEGER NOT NULL
     );
     CREATE TABLE IF NOT EXISTS repository_state (
         repo_id TEXT PRIMARY KEY REFERENCES repositories(repo_id) ON DELETE CASCADE,
         data TEXT NOT NULL,
         updated_at INTEGER NOT NULL
     );
     CREATE TABLE IF NOT EXISTS archived_worktrees (
         repo_id TEXT NOT NULL REFERENCES repositories(repo_id) ON DELETE CASCADE,
         worktree_key TEXT NOT NULL,
         archived_at INTEGER NOT NULL,
         PRIMARY KEY (repo_id, worktree_key)
     );",
    crate::events::EVENTS_SCHEMA,
];

/// Whether every migration this build knows is applied, by plain reads that
/// take no write lock. A database another process is still creating reads as
/// not current; [`initialize_schema`] then decides under the write lock.
fn schema_is_current(db: &Connection) -> Result<bool, StoreError> {
    let present = db
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'schema_migrations'",
            [],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !present {
        return Ok(false);
    }
    let versions = MIGRATIONS.len() as i64;
    let applied: i64 = db.query_row(
        "SELECT COUNT(*) FROM schema_migrations WHERE version BETWEEN 1 AND ?1",
        [versions],
        |row| row.get(0),
    )?;
    Ok(applied == versions)
}

fn initialize_schema(db: &mut Connection) -> Result<(), StoreError> {
    // Every command opens the store, and almost every open finds the schema
    // in place: that open must not queue behind a writer or append to the
    // log, which every running board watches.
    if schema_is_current(db)? {
        return Ok(());
    }
    // Serialize schema discovery and creation. Without this transaction two
    // first opens can both observe a missing migration row and race through DDL.
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (version INTEGER PRIMARY KEY, applied_at INTEGER NOT NULL);",
    )?;
    for (version, batch) in (1_i64..).zip(MIGRATIONS) {
        let applied = tx
            .query_row(
                "SELECT 1 FROM schema_migrations WHERE version = ?1",
                [version],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !applied {
            tx.execute_batch(batch)?;
            tx.execute(
                "INSERT OR IGNORE INTO schema_migrations (version, applied_at) VALUES (?1, ?2)",
                params![version, now_ms()],
            )?;
        }
    }
    tx.commit()?;
    Ok(())
}

fn ensure_schema_present(db: &Connection) -> Result<(), StoreError> {
    let present: bool = db
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'schema_migrations'",
            [],
            |_| Ok(true),
        )
        .optional()?
        .unwrap_or(false);
    if !present {
        return Err(StoreError::NeedsMigration);
    }
    let version: Option<i64> =
        db.query_row("SELECT MAX(version) FROM schema_migrations", [], |row| {
            row.get(0)
        })?;
    if version.unwrap_or(0) < 1 {
        return Err(StoreError::NeedsMigration);
    }
    Ok(())
}

fn register_repository(
    db: &mut Connection,
    identity: &RepositoryIdentity,
) -> Result<(), StoreError> {
    // A row with this id and path is the whole registration: `repo_path` is
    // unique, so no other id can hold the path. Anything else (no row yet,
    // or a collision to report) is decided under the write lock below.
    let registered: Option<String> = db
        .query_row(
            "SELECT repo_path FROM repositories WHERE repo_id = ?1",
            [&identity.id],
            |row| row.get(0),
        )
        .optional()?;
    if registered.as_deref() == Some(identity.path.as_str()) {
        return Ok(());
    }
    // Identity validation and registration must be one serialized operation.
    // A deferred transaction allowed concurrent opens to both read the old
    // identity snapshot, then one failed while upgrading to a writer.
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let existing: Option<String> = tx
        .query_row(
            "SELECT repo_path FROM repositories WHERE repo_id = ?1",
            [&identity.id],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(actual) = existing
        && actual != identity.path
    {
        return Err(StoreError::RepositoryCollision {
            id: identity.id.clone(),
            actual,
            expected: identity.path.clone(),
        });
    }
    let path_owner: Option<String> = tx
        .query_row(
            "SELECT repo_id FROM repositories WHERE repo_path = ?1",
            [&identity.path],
            |row| row.get(0),
        )
        .optional()?;
    if path_owner.as_deref().is_some_and(|id| id != identity.id) {
        return Err(StoreError::PathCollision(identity.path.clone()));
    }
    let now = now_ms();
    tx.execute(
        "INSERT INTO repositories (repo_id, repo_path, created_at, updated_at) VALUES (?1, ?2, ?3, ?4) \
         ON CONFLICT(repo_id) DO NOTHING",
        params![identity.id, identity.path, now, now],
    )?;
    tx.commit()?;
    Ok(())
}

fn ensure_identity_matches(
    db: &Connection,
    identity: &RepositoryIdentity,
) -> Result<(), StoreError> {
    let existing: Option<String> = db
        .query_row(
            "SELECT repo_path FROM repositories WHERE repo_id = ?1",
            [&identity.id],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(actual) = existing
        && actual != identity.path
    {
        return Err(StoreError::RepositoryCollision {
            id: identity.id.clone(),
            actual,
            expected: identity.path.clone(),
        });
    }
    Ok(())
}

/// Read, migrate, mutate and (when changed) write the repository's JSON state
/// inside the caller's transaction.
fn mutate_wt_state_tx<R>(
    tx: &Transaction<'_>,
    id: &str,
    mutate: impl FnOnce(&mut WtState) -> Result<(R, bool), StoreError>,
) -> Result<R, StoreError> {
    let stored: Option<String> = tx
        .query_row(
            "SELECT data FROM repository_state WHERE repo_id = ?1 LIMIT 1",
            [id],
            |row| row.get(0),
        )
        .optional()?;
    let (mut state, did_migrate) = match stored {
        Some(text) => {
            let parsed: serde_json::Value = serde_json::from_str(&text)?;
            let version = crate::raw_wt_state_version(&parsed);
            let migrated = crate::migrate_wt_state(parsed);
            (
                crate::mutations::normalize_state(migrated.value),
                version < crate::CURRENT_WT_STATE_VERSION,
            )
        }
        None => (crate::mutations::empty_state(), false),
    };
    let (result, changed) = mutate(&mut state)?;
    if changed || did_migrate {
        let encoded = serde_json::to_string(&state)?;
        tx.execute(
            "INSERT INTO repository_state (repo_id, data, updated_at) VALUES (?1, ?2, ?3) \
             ON CONFLICT(repo_id) DO UPDATE SET data = excluded.data, updated_at = excluded.updated_at",
            params![id, encoded, now_ms()],
        )?;
    }
    Ok(result)
}

fn replace_archived_keys_tx(
    tx: &Transaction<'_>,
    repo_id: &str,
    keys: &BTreeSet<String>,
) -> Result<(), rusqlite::Error> {
    tx.execute(
        "DELETE FROM archived_worktrees WHERE repo_id = ?1",
        [repo_id],
    )?;
    let mut insert = tx.prepare(
        "INSERT INTO archived_worktrees (repo_id, worktree_key, archived_at) VALUES (?1, ?2, ?3)",
    )?;
    let now = now_ms();
    for key in keys {
        insert.execute(params![repo_id, key, now])?;
    }
    Ok(())
}

pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

#[cfg(test)]
mod open_tests {
    use super::*;
    use tempfile::tempdir;

    fn log_state(path: &Path) -> (u64, SystemTime) {
        let mut wal = path.as_os_str().to_os_string();
        wal.push("-wal");
        let meta = fs::metadata(PathBuf::from(wal)).unwrap();
        (meta.len(), meta.modified().unwrap())
    }

    /// The store every command opens: a second open of a registered
    /// repository must neither wait for the writer nor write.
    #[test]
    fn opening_a_registered_repository_takes_no_write_lock_and_writes_nothing() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("wt.sqlite");
        let identity = || RepositoryIdentity::new("repo", "/repo");
        let mut first = Store::open(&path, identity()).unwrap();
        first.write_repository_state_json("{}").unwrap();
        let registered_at = |db: &Connection| -> i64 {
            db.query_row("SELECT updated_at FROM repositories", [], |row| row.get(0))
                .unwrap()
        };
        let before = (log_state(&path), registered_at(first.connection().unwrap()));

        // A writer mid-transaction, as a board or another command is.
        let writer = Connection::open(&path).unwrap();
        writer.execute_batch("BEGIN IMMEDIATE").unwrap();
        let started = Instant::now();
        let second = Store::open(&path, identity()).unwrap();
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(
            second.read_repository_state_json().unwrap().as_deref(),
            Some("{}")
        );
        drop(second);
        writer.execute_batch("ROLLBACK").unwrap();

        assert_eq!(
            (log_state(&path), registered_at(first.connection().unwrap())),
            before
        );
    }

    /// The fast path must not take an older database for current: every
    /// version this build knows is applied by the open that finds one missing.
    #[test]
    fn an_open_applies_each_version_a_database_lacks() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("wt.sqlite");
        let identity = || RepositoryIdentity::new("repo", "/repo");
        drop(Store::open(&path, identity()).unwrap());
        let versions = |db: &Connection| -> Vec<i64> {
            db.prepare("SELECT version FROM schema_migrations ORDER BY version")
                .unwrap()
                .query_map([], |row| row.get(0))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };
        let all = (1..=MIGRATIONS.len() as i64).collect::<Vec<_>>();
        for missing in all.iter().copied().skip(1) {
            let db = Connection::open(&path).unwrap();
            db.execute(
                "DELETE FROM schema_migrations WHERE version >= ?1",
                [missing],
            )
            .unwrap();
            assert!(!schema_is_current(&db).unwrap(), "version {missing}");
            drop(db);
            let store = Store::open(&path, identity()).unwrap();
            assert_eq!(versions(store.connection().unwrap()), all);
        }
    }

    #[test]
    fn a_collision_is_still_refused_and_a_new_repository_still_registered() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("wt.sqlite");
        drop(Store::open(&path, RepositoryIdentity::new("repo", "/repo")).unwrap());
        assert!(matches!(
            Store::open(&path, RepositoryIdentity::new("repo", "/elsewhere")),
            Err(StoreError::RepositoryCollision { .. })
        ));
        assert!(matches!(
            Store::open(&path, RepositoryIdentity::new("other", "/repo")),
            Err(StoreError::PathCollision(_))
        ));
        let mut other = Store::open(&path, RepositoryIdentity::new("other", "/other")).unwrap();
        other.write_repository_state_json("{}").unwrap();
        assert!(other.has_repository_state().unwrap());
    }
}
