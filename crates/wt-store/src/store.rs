use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
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
    #[error("set title for {slug}: manual title must not be empty")]
    EmptyManualTitle { slug: String },
    #[error("set title for {slug}: manual title revision exhausted")]
    ManualTitleRevisionExhausted { slug: String },
}

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
        db.busy_timeout(std::time::Duration::from_millis(3_000))?;
        db.pragma_update(None, "journal_mode", "WAL")?;
        db.pragma_update(None, "foreign_keys", "ON")?;
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

    fn writable_db(&mut self) -> Result<&mut Connection, StoreError> {
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

fn initialize_schema(db: &mut Connection) -> Result<(), StoreError> {
    // Serialize schema discovery and creation. Without this transaction two
    // first opens can both observe a missing migration row and race through DDL.
    let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (version INTEGER PRIMARY KEY, applied_at INTEGER NOT NULL);",
    )?;
    let applied: bool = tx
        .query_row(
            "SELECT 1 FROM schema_migrations WHERE version = 1",
            [],
            |_| Ok(true),
        )
        .optional()?
        .unwrap_or(false);
    if !applied {
        tx.execute_batch(
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
        )?;
        tx.execute(
            "INSERT OR IGNORE INTO schema_migrations (version, applied_at) VALUES (1, ?1)",
            [now_ms()],
        )?;
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
         ON CONFLICT(repo_id) DO UPDATE SET updated_at = excluded.updated_at",
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

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}
