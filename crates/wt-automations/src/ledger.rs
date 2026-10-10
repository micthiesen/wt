use std::{
    collections::BTreeSet,
    io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use serde_json::{Map, Value, json};
use thiserror::Error;
use tokio::{fs, io::AsyncWriteExt};
use tokio_util::sync::CancellationToken;
use wt_platform::lock::{FileLock, LockError};

const RETENTION_MS: i64 = 30 * 24 * 60 * 60 * 1000;
const BREAKER_LIMIT: u64 = 2;
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Error)]
pub enum LedgerError {
    #[error("automation ledger {operation} at {path}: {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("automation ledger at {path} is invalid: {message}")]
    Invalid { path: PathBuf, message: String },
    #[error(transparent)]
    Lock(#[from] LockError),
}

#[derive(Clone, Debug)]
pub struct AutomationLedger {
    path: PathBuf,
    lock_dir: PathBuf,
    lock_key: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BreakerState {
    pub count: u64,
    pub tripped_at: Option<i64>,
}

impl AutomationLedger {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let lock_dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
        let file = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("ledger");
        let lock_key = file.replace(['/', '\\'], "_");
        Self {
            path,
            lock_dir,
            lock_key,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub async fn has_handled(
        &self,
        key: &str,
        now_ms: i64,
        cancellation: &CancellationToken,
    ) -> Result<bool, LedgerError> {
        if cancellation.is_cancelled() {
            return Err(LockError::Cancelled.into());
        }
        let root = self.read_root().await?;
        Ok(fired_entry(&root, key).is_some_and(|entry| {
            number(entry.get("at")).is_some_and(|at| now_ms.saturating_sub(at) <= RETENTION_MS)
        }))
    }

    /// Persist the dispatch claim before any asynchronous launch work. Only
    /// previously unseen keys are inserted; false means a concurrent writer
    /// already owns every key.
    pub async fn begin_dispatch(
        &self,
        keys: &[String],
        rule_id: &str,
        slug: &str,
        now_ms: i64,
        cancellation: &CancellationToken,
    ) -> Result<bool, LedgerError> {
        self.mutate("begin dispatch", cancellation, |root| {
            let fired = ensure_object(root, "fired");
            let unseen: Vec<_> = keys
                .iter()
                .filter(|key| !fired.contains_key(key.as_str()))
                .cloned()
                .collect();
            if unseen.is_empty() {
                return false;
            }
            for key in unseen {
                fired.insert(
                    key,
                    json!({"state":"dispatched", "at":now_ms, "ruleId":rule_id, "slug":slug}),
                );
            }
            true
        })
        .await
    }

    pub async fn mark_delivered(
        &self,
        keys: &[String],
        now_ms: i64,
        cancellation: &CancellationToken,
    ) -> Result<(), LedgerError> {
        self.update_dispatched(keys, "delivered", now_ms, cancellation)
            .await
    }

    pub async fn mark_ambiguous(
        &self,
        keys: &[String],
        now_ms: i64,
        reason: &str,
        cancellation: &CancellationToken,
    ) -> Result<(), LedgerError> {
        self.mutate("mark ambiguous", cancellation, |root| {
            let fired = ensure_object(root, "fired");
            for key in keys {
                let Some(entry) = fired.get_mut(key).and_then(Value::as_object_mut) else {
                    continue;
                };
                if entry.get("state").and_then(Value::as_str) != Some("dispatched") {
                    continue;
                }
                entry.insert("state".into(), Value::String("ambiguous".into()));
                entry.insert("at".into(), json!(now_ms));
                entry.insert("reason".into(), Value::String(reason.to_owned()));
            }
            true
        })
        .await
        .map(|_| ())
    }

    /// Remove only a dispatch claim known not to have begun. A cancelled or
    /// terminal key written by another process is never overwritten.
    pub async fn drop_dispatched(
        &self,
        keys: &[String],
        cancellation: &CancellationToken,
    ) -> Result<usize, LedgerError> {
        self.mutate("drop dispatch", cancellation, |root| {
            let fired = ensure_object(root, "fired");
            let removable: Vec<_> = keys
                .iter()
                .filter(|key| {
                    fired
                        .get(key.as_str())
                        .and_then(Value::as_object)
                        .and_then(|entry| entry.get("state"))
                        .and_then(Value::as_str)
                        == Some("dispatched")
                })
                .cloned()
                .collect();
            for key in &removable {
                fired.remove(key);
            }
            removable.len()
        })
        .await
    }

    pub async fn mark_skipped(
        &self,
        keys: &[String],
        rule_id: &str,
        slug: &str,
        now_ms: i64,
        cancellation: &CancellationToken,
    ) -> Result<bool, LedgerError> {
        self.mutate("mark skipped", cancellation, |root| {
            let fired = ensure_object(root, "fired");
            let unseen: Vec<_> = keys
                .iter()
                .filter(|key| !fired.contains_key(key.as_str()))
                .cloned()
                .collect();
            for key in &unseen {
                fired.insert(
                    key.clone(),
                    json!({"state":"skipped", "at":now_ms, "ruleId":rule_id, "slug":slug}),
                );
            }
            !unseen.is_empty()
        })
        .await
    }

    pub async fn cancel(
        &self,
        keys: &[String],
        now_ms: i64,
        cancellation: &CancellationToken,
    ) -> Result<usize, LedgerError> {
        self.mutate("cancel fires", cancellation, |root| {
            let fired = ensure_object(root, "fired");
            let unseen: Vec<_> = keys
                .iter()
                .filter(|key| !fired.contains_key(key.as_str()))
                .cloned()
                .collect();
            for key in &unseen {
                fired.insert(
                    key.clone(),
                    json!({"state":"cancelled", "at":now_ms, "ruleId":"", "slug":""}),
                );
            }
            unseen.len()
        })
        .await
    }

    /// On boot, action runs prove a dispatched key was delivered. An
    /// unmatched dispatched key is removed so a safe headless action can be
    /// re-derived; callers separately retain `ambiguous` keys forever.
    pub async fn reconcile_dispatched(
        &self,
        delivered_keys: &BTreeSet<String>,
        now_ms: i64,
        cancellation: &CancellationToken,
    ) -> Result<(usize, usize), LedgerError> {
        self.mutate("reconcile dispatches", cancellation, |root| {
            let fired = ensure_object(root, "fired");
            let dispatched: Vec<_> = fired
                .iter()
                .filter(|(_, value)| {
                    value.get("state").and_then(Value::as_str) == Some("dispatched")
                })
                .map(|(key, _)| key.clone())
                .collect();
            let mut delivered = 0;
            let mut dropped = 0;
            for key in dispatched {
                if delivered_keys.contains(&key) {
                    if let Some(entry) = fired.get_mut(&key).and_then(Value::as_object_mut) {
                        entry.insert("state".into(), Value::String("delivered".into()));
                        entry.insert("at".into(), json!(now_ms));
                    }
                    delivered += 1;
                } else {
                    fired.remove(&key);
                    dropped += 1;
                }
            }
            (delivered, dropped)
        })
        .await
    }

    pub async fn breaker_state(
        &self,
        rule_id: &str,
        slug: &str,
        now_ms: i64,
    ) -> Result<BreakerState, LedgerError> {
        let root = self.read_root().await?;
        let key = pair_key(rule_id, slug);
        let Some(entry) = root
            .get("breaker")
            .and_then(Value::as_object)
            .and_then(|entries| entries.get(&key))
        else {
            return Ok(BreakerState {
                count: 0,
                tripped_at: None,
            });
        };
        let updated = number(entry.get("updatedAt")).unwrap_or(now_ms);
        if now_ms.saturating_sub(updated) > RETENTION_MS {
            return Ok(BreakerState {
                count: 0,
                tripped_at: None,
            });
        }
        Ok(BreakerState {
            count: number(entry.get("count")).unwrap_or(0).max(0) as u64,
            tripped_at: number(entry.get("trippedAt")),
        })
    }

    pub async fn bump_breaker(
        &self,
        rule_id: &str,
        slug: &str,
        now_ms: i64,
        cancellation: &CancellationToken,
    ) -> Result<BreakerState, LedgerError> {
        self.mutate("bump breaker", cancellation, |root| {
            let key = pair_key(rule_id, slug);
            let breaker = ensure_object(root, "breaker");
            let mut entry = breaker
                .get(&key)
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            let count = number(entry.get("count")).unwrap_or(0).max(0) as u64 + 1;
            let trip = number(entry.get("trippedAt"));
            let tripped_at = trip.or_else(|| (count >= BREAKER_LIMIT).then_some(now_ms));
            entry.insert("count".into(), json!(count));
            entry.insert(
                "trippedAt".into(),
                tripped_at.map_or(Value::Null, |value| json!(value)),
            );
            entry.insert("updatedAt".into(), json!(now_ms));
            breaker.insert(key, Value::Object(entry));
            BreakerState { count, tripped_at }
        })
        .await
    }

    /// Only call after the condition itself is observed false, never merely
    /// because its row was busy, paused, archived, or stale.
    pub async fn reset_breaker(
        &self,
        rule_id: &str,
        slug: &str,
        cancellation: &CancellationToken,
    ) -> Result<bool, LedgerError> {
        self.mutate("reset breaker", cancellation, |root| {
            ensure_object(root, "breaker")
                .remove(&pair_key(rule_id, slug))
                .is_some()
        })
        .await
    }

    pub async fn last_dispatch(
        &self,
        rule_id: &str,
        slug: &str,
    ) -> Result<Option<i64>, LedgerError> {
        let root = self.read_root().await?;
        Ok(number(
            root.get("lastDispatch")
                .and_then(Value::as_object)
                .and_then(|map| map.get(&pair_key(rule_id, slug))),
        ))
    }

    pub async fn record_dispatch_time(
        &self,
        rule_id: &str,
        slug: &str,
        now_ms: i64,
        cancellation: &CancellationToken,
    ) -> Result<(), LedgerError> {
        self.mutate("record dispatch time", cancellation, |root| {
            ensure_object(root, "lastDispatch").insert(pair_key(rule_id, slug), json!(now_ms));
        })
        .await
    }

    async fn update_dispatched(
        &self,
        keys: &[String],
        state: &str,
        now_ms: i64,
        cancellation: &CancellationToken,
    ) -> Result<(), LedgerError> {
        self.mutate("update dispatch", cancellation, |root| {
            let fired = ensure_object(root, "fired");
            for key in keys {
                let Some(entry) = fired.get_mut(key).and_then(Value::as_object_mut) else {
                    continue;
                };
                if entry.get("state").and_then(Value::as_str) != Some("dispatched") {
                    continue;
                }
                entry.insert("state".into(), Value::String(state.into()));
                entry.insert("at".into(), json!(now_ms));
            }
        })
        .await
    }

    async fn mutate<T>(
        &self,
        operation: &'static str,
        cancellation: &CancellationToken,
        mutate: impl FnOnce(&mut Value) -> T,
    ) -> Result<T, LedgerError> {
        let _lock =
            FileLock::acquire(&self.lock_dir, &self.lock_key, operation, cancellation).await?;
        let mut root = self.read_root().await?;
        let result = mutate(&mut root);
        prune(&mut root, now_ms());
        self.write_root(&root).await?;
        Ok(result)
    }

    async fn read_root(&self) -> Result<Value, LedgerError> {
        match fs::read(&self.path).await {
            Ok(bytes) => {
                let value: Value =
                    serde_json::from_slice(&bytes).map_err(|error| LedgerError::Invalid {
                        path: self.path.clone(),
                        message: error.to_string(),
                    })?;
                if !value.is_object() {
                    return Err(LedgerError::Invalid {
                        path: self.path.clone(),
                        message: "root must be an object".into(),
                    });
                }
                Ok(value)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                Ok(json!({"version":1,"fired":{},"breaker":{},"lastDispatch":{}}))
            }
            Err(source) => Err(io_error("read", &self.path, source)),
        }
    }

    async fn write_root(&self, value: &Value) -> Result<(), LedgerError> {
        let parent = self.path.parent().unwrap_or(Path::new("."));
        fs::create_dir_all(parent)
            .await
            .map_err(|source| io_error("create directory", parent, source))?;
        let temp = self.path.with_extension(format!(
            "json.tmp-{}-{}",
            std::process::id(),
            TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let bytes = serde_json::to_vec_pretty(value).map_err(|error| LedgerError::Invalid {
            path: self.path.clone(),
            message: error.to_string(),
        })?;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .await
            .map_err(|source| io_error("create temporary", &temp, source))?;
        file.write_all(&bytes)
            .await
            .map_err(|source| io_error("write temporary", &temp, source))?;
        file.sync_all()
            .await
            .map_err(|source| io_error("sync temporary", &temp, source))?;
        drop(file);
        fs::rename(&temp, &self.path)
            .await
            .map_err(|source| io_error("replace", &self.path, source))?;
        Ok(())
    }
}

fn ensure_object<'a>(root: &'a mut Value, key: &str) -> &'a mut Map<String, Value> {
    if !root.is_object() {
        *root = Value::Object(Map::new());
    }
    let object = root.as_object_mut().expect("root made object");
    if !object.get(key).is_some_and(Value::is_object) {
        object.insert(key.to_owned(), Value::Object(Map::new()));
    }
    object
        .get_mut(key)
        .and_then(Value::as_object_mut)
        .expect("child made object")
}

fn fired_entry<'a>(root: &'a Value, key: &str) -> Option<&'a Value> {
    root.get("fired")?.as_object()?.get(key)
}

fn number(value: Option<&Value>) -> Option<i64> {
    value.and_then(Value::as_i64)
}

fn pair_key(rule_id: &str, slug: &str) -> String {
    format!("{rule_id}|{slug}")
}

fn prune(root: &mut Value, now_ms: i64) {
    let cutoff = now_ms.saturating_sub(RETENTION_MS);
    if let Some(fired) = root.get_mut("fired").and_then(Value::as_object_mut) {
        fired.retain(|_, entry| number(entry.get("at")).is_none_or(|at| at >= cutoff));
    }
    if let Some(breaker) = root.get_mut("breaker").and_then(Value::as_object_mut) {
        breaker.retain(|_, entry| number(entry.get("updatedAt")).unwrap_or(now_ms) >= cutoff);
    }
    if let Some(last) = root.get_mut("lastDispatch").and_then(Value::as_object_mut) {
        last.retain(|_, timestamp| number(Some(timestamp)).is_none_or(|at| at >= cutoff));
    }
}

fn now_ms() -> i64 {
    (time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000) as i64
}

fn io_error(operation: &'static str, path: &Path, source: io::Error) -> LedgerError {
    LedgerError::Io {
        operation,
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn ledger_preserves_unknown_json_and_serializes_dispatch_claims() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("automations.json");
        let now = now_ms();
        fs::write(&path, serde_json::to_vec(&json!({"version":1,"fired":{"old":{"state":"delivered","at":now,"ruleId":"x","slug":"s","futureField":{"kept":true}}},"futureTop":{"keep":"yes"}})).unwrap()).await.unwrap();
        let ledger = AutomationLedger::new(&path);
        let cancel = CancellationToken::new();
        assert!(
            ledger
                .begin_dispatch(&["key".into()], "rule", "slug", now + 1, &cancel)
                .await
                .unwrap()
        );
        assert!(
            !ledger
                .begin_dispatch(&["key".into()], "rule", "slug", now + 2, &cancel)
                .await
                .unwrap()
        );
        ledger
            .mark_delivered(&["key".into()], now + 3, &cancel)
            .await
            .unwrap();
        let value: Value = serde_json::from_slice(&fs::read(path).await.unwrap()).unwrap();
        assert_eq!(value["futureTop"]["keep"], "yes");
        assert_eq!(value["fired"]["old"]["futureField"]["kept"], true);
        assert_eq!(value["fired"]["key"]["state"], "delivered");
    }

    #[tokio::test]
    async fn cancellation_beats_stale_dispatch_writers_and_ambiguous_is_not_replayed() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = AutomationLedger::new(dir.path().join("automations.json"));
        let cancel = CancellationToken::new();
        let now = now_ms();
        ledger
            .cancel(&["cancel-me".into()], now + 2, &cancel)
            .await
            .unwrap();
        assert!(
            !ledger
                .begin_dispatch(&["cancel-me".into()], "rule", "slug", now + 3, &cancel)
                .await
                .unwrap()
        );
        let keys = vec!["ambiguous".into()];
        assert!(
            ledger
                .begin_dispatch(&keys, "rule", "slug", now + 4, &cancel)
                .await
                .unwrap()
        );
        ledger
            .mark_ambiguous(&["ambiguous".into()], now + 5, "send reply lost", &cancel)
            .await
            .unwrap();
        ledger
            .mark_delivered(&keys, now + 6, &cancel)
            .await
            .unwrap();
        assert_eq!(
            ledger.read_root().await.unwrap()["fired"]["cancel-me"]["state"],
            "cancelled"
        );
        // Cancel only inserts unseen keys; it cannot cancel a dispatched key.
        // The ambiguity remains terminal across a later stale delivered write.
        assert_eq!(
            ledger.read_root().await.unwrap()["fired"]["ambiguous"]["state"],
            "ambiguous"
        );
        assert_eq!(
            ledger.read_root().await.unwrap()["fired"]["ambiguous"]["reason"],
            "send reply lost"
        );
    }

    #[tokio::test]
    async fn breaker_trips_on_second_dispatch_and_resets_only_explicitly() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = AutomationLedger::new(dir.path().join("automations.json"));
        let cancel = CancellationToken::new();
        let now = now_ms();
        assert_eq!(
            ledger
                .bump_breaker("r", "s", now + 1, &cancel)
                .await
                .unwrap()
                .tripped_at,
            None
        );
        assert_eq!(
            ledger
                .bump_breaker("r", "s", now + 2, &cancel)
                .await
                .unwrap()
                .tripped_at,
            Some(now + 2)
        );
        assert!(ledger.reset_breaker("r", "s", &cancel).await.unwrap());
        assert_eq!(
            ledger.breaker_state("r", "s", now + 3).await.unwrap().count,
            0
        );
    }
}
