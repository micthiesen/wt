use std::{
    collections::{BTreeMap, BTreeSet},
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
pub const BREAKER_LIMIT: u64 = 2;
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DispatchKind {
    HeadlessAction,
    ExternalSend,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DispatchClaim {
    claim_id: String,
    keys: Vec<String>,
    rule_id: String,
    slug: String,
    kind: DispatchKind,
    claimed_at_ms: i64,
    breaker_reserved: bool,
}

impl DispatchClaim {
    pub fn keys(&self) -> &[String] {
        &self.keys
    }

    pub fn rule_id(&self) -> &str {
        &self.rule_id
    }

    pub fn slug(&self) -> &str {
        &self.slug
    }

    pub fn kind(&self) -> DispatchKind {
        self.kind
    }
}

/// One coherent, read-only ledger view for a whole evaluator pass.
#[derive(Clone, Debug, Default)]
pub struct LedgerSnapshot {
    fired: BTreeMap<String, Value>,
    last_dispatch: BTreeMap<String, Value>,
    breaker: BTreeMap<String, Value>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DispatchPolicy {
    /// Minimum time between claims for this `(rule, slug)` pair.
    pub cooldown_ms: Option<i64>,
    /// Consecutive-dispatch breaker limit. `None` exempts the fire.
    pub breaker_limit: Option<u64>,
}

#[derive(Clone, Copy, Debug)]
pub struct DispatchRequest<'a> {
    pub keys: &'a [String],
    pub rule_id: &'a str,
    pub slug: &'a str,
    pub kind: DispatchKind,
    pub policy: DispatchPolicy,
    pub now_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DispatchClaimResult {
    Claimed(DispatchClaim),
    AlreadyHandled,
    CoolingDown { until_ms: i64 },
    BreakerOpen { count: u64, tripped_at: i64 },
    BreakerReserved { count: u64, reserved: u64 },
}

impl LedgerSnapshot {
    pub fn has_handled(&self, key: &str, now_ms: i64) -> bool {
        let Some(entry) = self.fired.get(key) else {
            return false;
        };
        // Unknown or malformed persisted entries are claims until they can be
        // positively proven expired. Treating them as absent can duplicate an
        // external effect after a partial or forward-version write.
        let state = entry.get("state").and_then(Value::as_str);
        if !matches!(state, Some("delivered" | "skipped" | "cancelled")) {
            return true;
        }
        number(entry.get("at")).is_none_or(|at| now_ms.saturating_sub(at) <= RETENTION_MS)
    }

    pub fn last_dispatch(&self, rule_id: &str, slug: &str) -> Option<i64> {
        number(self.last_dispatch.get(&pair_key(rule_id, slug)))
    }

    pub fn breaker_state(&self, rule_id: &str, slug: &str, now_ms: i64) -> BreakerState {
        let Some(entry) = self.breaker.get(&pair_key(rule_id, slug)) else {
            return BreakerState {
                count: 0,
                tripped_at: None,
            };
        };
        let updated = number(entry.get("updatedAt")).unwrap_or(now_ms);
        if now_ms.saturating_sub(updated) > RETENTION_MS {
            return BreakerState {
                count: 0,
                tripped_at: None,
            };
        }
        BreakerState {
            count: number(entry.get("count")).unwrap_or(0).max(0) as u64,
            tripped_at: number(entry.get("trippedAt")),
        }
    }
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

    /// Persist an all-or-nothing claim before any external effect. A claim
    /// owns exactly `keys`; overlapping batches cannot partially launch and
    /// later mutate one another's ledger entries.
    pub async fn claim_dispatch(
        &self,
        keys: &[String],
        rule_id: &str,
        slug: &str,
        kind: DispatchKind,
        now_ms: i64,
        cancellation: &CancellationToken,
    ) -> Result<Option<DispatchClaim>, LedgerError> {
        match self
            .claim_dispatch_inner(
                DispatchRequest {
                    keys,
                    rule_id,
                    slug,
                    kind,
                    policy: DispatchPolicy::default(),
                    now_ms,
                },
                cancellation,
                true,
            )
            .await?
        {
            DispatchClaimResult::Claimed(claim) => Ok(Some(claim)),
            _ => Ok(None),
        }
    }

    /// Claim only if both the per-target cooldown and breaker still allow a
    /// dispatch. The checks and reservation share the file-lock mutation, so
    /// concurrent processes cannot each pass a stale snapshot.
    pub async fn claim_dispatch_checked(
        &self,
        request: DispatchRequest<'_>,
        cancellation: &CancellationToken,
    ) -> Result<DispatchClaimResult, LedgerError> {
        self.claim_dispatch_inner(request, cancellation, false)
            .await
    }

    async fn claim_dispatch_inner(
        &self,
        request: DispatchRequest<'_>,
        cancellation: &CancellationToken,
        commit_cooldown_at_claim: bool,
    ) -> Result<DispatchClaimResult, LedgerError> {
        let DispatchRequest {
            keys,
            rule_id,
            slug,
            kind,
            policy,
            now_ms,
        } = request;
        if keys.is_empty() || keys.iter().collect::<BTreeSet<_>>().len() != keys.len() {
            return Ok(DispatchClaimResult::AlreadyHandled);
        }
        let claim = DispatchClaim {
            claim_id: new_claim_id()?,
            keys: keys.to_vec(),
            rule_id: rule_id.to_owned(),
            slug: slug.to_owned(),
            kind,
            claimed_at_ms: now_ms,
            breaker_reserved: policy.breaker_limit.is_some(),
        };
        let claim_for_write = claim.clone();
        self.mutate("claim dispatch", cancellation, move |root| {
            let pair = pair_key(&claim_for_write.rule_id, &claim_for_write.slug);
            let active = {
                let fired = ensure_object(root, "fired");
                if claim_for_write
                    .keys
                    .iter()
                    .any(|key| fired.contains_key(key))
                {
                    return DispatchClaimResult::AlreadyHandled;
                }
                active_pair_claims(fired, &claim_for_write.rule_id, &claim_for_write.slug)
            };
            let last_dispatch = root
                .get("lastDispatch")
                .and_then(Value::as_object)
                .and_then(|last| last.get(&pair))
                .and_then(Value::as_i64);
            let last_pending = active.iter().map(|entry| entry.at_ms).max();
            let last = last_dispatch.into_iter().chain(last_pending).max();
            if let (Some(cooldown), Some(last)) = (policy.cooldown_ms, last) {
                let until_ms = last.saturating_add(cooldown.max(0));
                if now_ms < until_ms {
                    return DispatchClaimResult::CoolingDown { until_ms };
                }
            }

            let breaker = root
                .get("breaker")
                .and_then(Value::as_object)
                .and_then(|breakers| breakers.get(&pair))
                .and_then(Value::as_object);
            let count = breaker
                .and_then(|entry| entry.get("count"))
                .and_then(Value::as_i64)
                .unwrap_or(0)
                .max(0) as u64;
            let tripped_at = breaker
                .and_then(|entry| entry.get("trippedAt"))
                .and_then(Value::as_i64);
            if let Some(limit) = policy.breaker_limit {
                if let Some(tripped_at) = tripped_at {
                    return DispatchClaimResult::BreakerOpen { count, tripped_at };
                }
                if count >= limit {
                    let breaker = ensure_object(root, "breaker");
                    let mut entry = breaker
                        .get(&pair)
                        .and_then(Value::as_object)
                        .cloned()
                        .unwrap_or_default();
                    let tripped_at = number(entry.get("trippedAt")).unwrap_or(now_ms);
                    entry.insert("count".into(), json!(count));
                    entry.insert("trippedAt".into(), json!(tripped_at));
                    entry.insert("updatedAt".into(), json!(now_ms));
                    breaker.insert(pair.clone(), Value::Object(entry));
                    return DispatchClaimResult::BreakerOpen { count, tripped_at };
                }
                let reserved = active.iter().filter(|entry| entry.breaker_reserved).count() as u64;
                if count.saturating_add(reserved) >= limit {
                    return DispatchClaimResult::BreakerReserved { count, reserved };
                }
            }

            let fired = ensure_object(root, "fired");
            for key in &claim_for_write.keys {
                fired.insert(
                    key.clone(),
                    json!({
                        "state":"dispatched", "at":now_ms,
                        "ruleId":claim_for_write.rule_id, "slug":claim_for_write.slug,
                        "claimId":claim_for_write.claim_id, "kind":claim_for_write.kind,
                        "breakerReserved":claim_for_write.breaker_reserved,
                    }),
                );
            }
            if commit_cooldown_at_claim {
                let last = ensure_object(root, "lastDispatch");
                last.insert(pair, json!(now_ms));
            }
            DispatchClaimResult::Claimed(claim_for_write)
        })
        .await
    }

    pub async fn mark_delivered(
        &self,
        claim: &DispatchClaim,
        now_ms: i64,
        cancellation: &CancellationToken,
    ) -> Result<(), LedgerError> {
        self.transition_claim(claim, "delivered", None, now_ms, cancellation)
            .await
    }

    pub async fn mark_ambiguous(
        &self,
        claim: &DispatchClaim,
        now_ms: i64,
        reason: &str,
        cancellation: &CancellationToken,
    ) -> Result<(), LedgerError> {
        self.transition_claim(claim, "ambiguous", Some(reason), now_ms, cancellation)
            .await
    }

    /// Remove only a dispatch claim known not to have begun. A cancelled or
    /// terminal key written by another process is never overwritten.
    pub async fn drop_dispatched(
        &self,
        claim: &DispatchClaim,
        cancellation: &CancellationToken,
    ) -> Result<usize, LedgerError> {
        self.release_not_started(claim, cancellation).await
    }

    /// Release only this exact claim, and only when the caller has positive
    /// evidence that no external write was attempted. A crash skips this call
    /// and boot reconciliation therefore retains the claim as ambiguous.
    pub async fn release_not_started(
        &self,
        claim: &DispatchClaim,
        cancellation: &CancellationToken,
    ) -> Result<usize, LedgerError> {
        self.mutate("drop dispatch", cancellation, |root| {
            let fired = ensure_object(root, "fired");
            let removable: Vec<_> = claim
                .keys
                .iter()
                .filter(|key| {
                    fired
                        .get(key.as_str())
                        .and_then(Value::as_object)
                        .and_then(|entry| entry.get("state"))
                        .and_then(Value::as_str)
                        == Some("dispatched")
                        && fired
                            .get(key.as_str())
                            .and_then(Value::as_object)
                            .and_then(|entry| entry.get("claimId"))
                            .and_then(Value::as_str)
                            == Some(claim.claim_id.as_str())
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

    /// On boot, matching durable action runs prove delivery. Any unmatched
    /// dispatch remains handled as ambiguous: absence from a bounded run scan
    /// is not proof that no external write happened.
    pub async fn reconcile_dispatched(
        &self,
        delivered_keys: &BTreeSet<String>,
        now_ms: i64,
        cancellation: &CancellationToken,
    ) -> Result<(usize, usize), LedgerError> {
        self.mutate("reconcile dispatches", cancellation, |root| {
            let mut claims: BTreeMap<String, Vec<String>> = BTreeMap::new();
            {
                let fired = ensure_object(root, "fired");
                for (key, value) in fired.iter().filter(|(_, value)| {
                    value.get("state").and_then(Value::as_str) == Some("dispatched")
                }) {
                    let claim_id = value
                        .get("claimId")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                        .unwrap_or_else(|| format!("legacy:{key}"));
                    claims.entry(claim_id).or_default().push(key.clone());
                }
            }
            let mut delivered = 0;
            let mut ambiguous = 0;
            for keys in claims.values() {
                // Action metadata records the whole fire-key batch. Partial
                // evidence cannot prove that every key in the batch launched.
                let proven = keys.iter().all(|key| delivered_keys.contains(key));
                let (reservation, pair) = {
                    let fired = ensure_object(root, "fired");
                    let mut reservation = None;
                    let mut pair = None;
                    for key in keys {
                        if let Some(entry) = fired.get_mut(key).and_then(Value::as_object_mut) {
                            let rule_id = entry.get("ruleId").and_then(Value::as_str);
                            let slug = entry.get("slug").and_then(Value::as_str);
                            pair = pair.or_else(|| {
                                Some((
                                    rule_id?.to_owned(),
                                    slug?.to_owned(),
                                    entry.get("at").and_then(Value::as_i64).unwrap_or(now_ms),
                                ))
                            });
                            reservation = reservation.or_else(|| {
                                (entry.get("breakerReserved").and_then(Value::as_bool)
                                    == Some(true))
                                .then(|| Some((rule_id?.to_owned(), slug?.to_owned())))
                                .flatten()
                            });
                            entry.insert(
                                "state".into(),
                                Value::String(
                                    if proven { "delivered" } else { "ambiguous" }.into(),
                                ),
                            );
                            entry.insert("at".into(), json!(now_ms));
                            if !proven {
                                entry.entry("reason").or_insert_with(|| {
                                    Value::String("no durable delivery proof".into())
                                });
                            }
                        }
                    }
                    (reservation, pair)
                };
                if let Some((rule_id, slug)) = reservation {
                    finalize_breaker(root, &rule_id, &slug, now_ms);
                }
                if let Some((rule_id, slug, claimed_at)) = pair {
                    set_last_dispatch_max(root, &rule_id, &slug, claimed_at);
                }
                if proven {
                    delivered += keys.len();
                } else {
                    ambiguous += keys.len();
                }
            }
            (delivered, ambiguous)
        })
        .await
    }

    pub async fn snapshot(&self) -> Result<LedgerSnapshot, LedgerError> {
        let root = self.read_root().await?;
        Ok(LedgerSnapshot {
            fired: root
                .get("fired")
                .and_then(Value::as_object)
                .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                .unwrap_or_default(),
            last_dispatch: root
                .get("lastDispatch")
                .and_then(Value::as_object)
                .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                .unwrap_or_default(),
            breaker: root
                .get("breaker")
                .and_then(Value::as_object)
                .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                .unwrap_or_default(),
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

    async fn transition_claim(
        &self,
        claim: &DispatchClaim,
        state: &str,
        reason: Option<&str>,
        now_ms: i64,
        cancellation: &CancellationToken,
    ) -> Result<(), LedgerError> {
        self.mutate("update dispatch", cancellation, |root| {
            let transitioned = {
                let fired = ensure_object(root, "fired");
                let mut transitioned = false;
                for key in &claim.keys {
                    let Some(entry) = fired.get_mut(key).and_then(Value::as_object_mut) else {
                        continue;
                    };
                    if entry.get("state").and_then(Value::as_str) != Some("dispatched") {
                        continue;
                    }
                    if entry.get("claimId").and_then(Value::as_str) != Some(&claim.claim_id) {
                        continue;
                    }
                    entry.insert("state".into(), Value::String(state.into()));
                    entry.insert("at".into(), json!(now_ms));
                    transitioned = true;
                    if let Some(reason) = reason {
                        entry.insert("reason".into(), Value::String(reason.to_owned()));
                    }
                }
                transitioned
            };
            if transitioned {
                set_last_dispatch_max(root, &claim.rule_id, &claim.slug, claim.claimed_at_ms);
                if claim.breaker_reserved {
                    finalize_breaker(root, &claim.rule_id, &claim.slug, now_ms);
                }
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
        prune(&mut root, now_ms());
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
                for field in ["fired", "breaker", "lastDispatch"] {
                    if value.get(field).is_some_and(|entry| !entry.is_object()) {
                        return Err(LedgerError::Invalid {
                            path: self.path.clone(),
                            message: format!("{field} must be an object when present"),
                        });
                    }
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

fn number(value: Option<&Value>) -> Option<i64> {
    value.and_then(Value::as_i64)
}

fn pair_key(rule_id: &str, slug: &str) -> String {
    format!("{rule_id}|{slug}")
}

struct ActivePairClaim {
    at_ms: i64,
    breaker_reserved: bool,
}

fn active_pair_claims(
    fired: &Map<String, Value>,
    rule_id: &str,
    slug: &str,
) -> Vec<ActivePairClaim> {
    let mut claims: BTreeMap<String, ActivePairClaim> = BTreeMap::new();
    for (key, value) in fired {
        if value.get("state").and_then(Value::as_str) != Some("dispatched")
            || value.get("ruleId").and_then(Value::as_str) != Some(rule_id)
            || value.get("slug").and_then(Value::as_str) != Some(slug)
        {
            continue;
        }
        let claim_id = value
            .get("claimId")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| format!("legacy:{key}"));
        let entry = claims.entry(claim_id.clone()).or_insert(ActivePairClaim {
            // Missing timestamps must hold cooldown conservatively.
            at_ms: i64::MIN,
            breaker_reserved: false,
        });
        entry.at_ms = value
            .get("at")
            .and_then(Value::as_i64)
            .unwrap_or(i64::MAX)
            .max(entry.at_ms);
        entry.breaker_reserved |=
            value.get("breakerReserved").and_then(Value::as_bool) == Some(true);
    }
    claims.into_values().collect()
}

fn set_last_dispatch_max(root: &mut Value, rule_id: &str, slug: &str, at_ms: i64) {
    let pair = pair_key(rule_id, slug);
    let last = ensure_object(root, "lastDispatch");
    let previous = last.get(&pair).and_then(Value::as_i64).unwrap_or(i64::MIN);
    last.insert(pair, json!(previous.max(at_ms)));
}

fn finalize_breaker(root: &mut Value, rule_id: &str, slug: &str, now_ms: i64) {
    let key = pair_key(rule_id, slug);
    let breaker = ensure_object(root, "breaker");
    let mut entry = breaker
        .get(&key)
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let count = number(entry.get("count")).unwrap_or(0).max(0) as u64 + 1;
    let tripped_at =
        number(entry.get("trippedAt")).or_else(|| (count >= BREAKER_LIMIT).then_some(now_ms));
    entry.insert("count".into(), json!(count));
    entry.insert(
        "trippedAt".into(),
        tripped_at.map_or(Value::Null, |value| json!(value)),
    );
    entry.insert("updatedAt".into(), json!(now_ms));
    breaker.insert(key, Value::Object(entry));
}

fn new_claim_id() -> Result<String, LedgerError> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|error| LedgerError::Invalid {
        path: PathBuf::from("automation ledger"),
        message: format!("could not create dispatch claim identity: {error}"),
    })?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn prune(root: &mut Value, now_ms: i64) {
    let cutoff = now_ms.saturating_sub(RETENTION_MS);
    if let Some(fired) = root.get_mut("fired").and_then(Value::as_object_mut) {
        fired.retain(|_, entry| {
            let state = entry.get("state").and_then(Value::as_str);
            !matches!(state, Some("delivered" | "skipped" | "cancelled"))
                || number(entry.get("at")).is_none_or(|at| at >= cutoff)
        });
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

    fn request<'a>(
        keys: &'a [String],
        rule_id: &'a str,
        slug: &'a str,
        policy: DispatchPolicy,
        now_ms: i64,
    ) -> DispatchRequest<'a> {
        DispatchRequest {
            keys,
            rule_id,
            slug,
            kind: DispatchKind::HeadlessAction,
            policy,
            now_ms,
        }
    }

    #[tokio::test]
    async fn ledger_preserves_unknown_json_and_serializes_dispatch_claims() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("automations.json");
        let now = now_ms();
        fs::write(&path, serde_json::to_vec(&json!({"version":1,"fired":{"old":{"state":"delivered","at":now,"ruleId":"x","slug":"s","futureField":{"kept":true}}},"futureTop":{"keep":"yes"}})).unwrap()).await.unwrap();
        let ledger = AutomationLedger::new(&path);
        let cancel = CancellationToken::new();
        let claim = ledger
            .claim_dispatch(
                &["key".into()],
                "rule",
                "slug",
                DispatchKind::HeadlessAction,
                now + 1,
                &cancel,
            )
            .await
            .unwrap()
            .unwrap();
        assert!(
            ledger
                .claim_dispatch(
                    &["key".into()],
                    "rule",
                    "slug",
                    DispatchKind::HeadlessAction,
                    now + 2,
                    &cancel
                )
                .await
                .unwrap()
                .is_none()
        );
        ledger
            .mark_delivered(&claim, now + 3, &cancel)
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
            ledger
                .claim_dispatch(
                    &["cancel-me".into()],
                    "rule",
                    "slug",
                    DispatchKind::ExternalSend,
                    now + 3,
                    &cancel
                )
                .await
                .unwrap()
                .is_none()
        );
        let claim = ledger
            .claim_dispatch(
                &["ambiguous".into()],
                "rule",
                "slug",
                DispatchKind::ExternalSend,
                now + 4,
                &cancel,
            )
            .await
            .unwrap()
            .unwrap();
        ledger
            .mark_ambiguous(&claim, now + 5, "send reply lost", &cancel)
            .await
            .unwrap();
        ledger
            .mark_delivered(&claim, now + 6, &cancel)
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
    async fn claims_are_all_or_nothing_and_transitions_are_owner_scoped() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = AutomationLedger::new(dir.path().join("automations.json"));
        let cancel = CancellationToken::new();
        let now = now_ms();
        let first = ledger
            .claim_dispatch(
                &["a".into(), "b".into()],
                "r",
                "s",
                DispatchKind::HeadlessAction,
                now,
                &cancel,
            )
            .await
            .unwrap()
            .unwrap();
        assert!(
            ledger
                .claim_dispatch(
                    &["b".into(), "c".into()],
                    "r",
                    "s",
                    DispatchKind::ExternalSend,
                    now + 1,
                    &cancel
                )
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            ledger.snapshot().await.unwrap().last_dispatch("r", "s"),
            Some(now)
        );
        assert_eq!(
            ledger.release_not_started(&first, &cancel).await.unwrap(),
            2
        );
        let second = ledger
            .claim_dispatch(
                &["b".into(), "c".into()],
                "r",
                "s",
                DispatchKind::ExternalSend,
                now + 2,
                &cancel,
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            ledger.release_not_started(&first, &cancel).await.unwrap(),
            0
        );
        assert!(ledger.snapshot().await.unwrap().has_handled("b", now + 2));
        assert_eq!(second.keys(), &["b".to_owned(), "c".to_owned()]);
    }

    #[tokio::test]
    async fn unmatched_or_legacy_dispatches_become_durable_ambiguity() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("automations.json");
        let ledger = AutomationLedger::new(&path);
        let cancel = CancellationToken::new();
        let now = now_ms();
        fs::write(
            &path,
            serde_json::to_vec(&json!({
                "version": 1,
                "fired": {
                    "legacy": {"state":"dispatched", "at": 1, "ruleId":"old", "slug":"s"},
                    "future": {"state":"futureState", "at": 1, "opaque": true}
                },
                "lastDispatch": {}, "breaker": {}
            }))
            .unwrap(),
        )
        .await
        .unwrap();
        let claim = ledger
            .claim_dispatch(
                &["sent-before-crash".into()],
                "r",
                "s",
                DispatchKind::ExternalSend,
                now,
                &cancel,
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            ledger
                .reconcile_dispatched(&BTreeSet::new(), now + 1, &cancel)
                .await
                .unwrap(),
            (0, 2)
        );
        let snapshot = ledger.snapshot().await.unwrap();
        assert!(snapshot.has_handled("legacy", now + 100 * RETENTION_MS));
        assert!(snapshot.has_handled("future", now + 100 * RETENTION_MS));
        // A known external attempt cannot be claimed again after the ordinary
        // retention horizon, even when reconciliation had no delivery proof.
        assert!(
            ledger
                .claim_dispatch(
                    claim.keys(),
                    "r",
                    "s",
                    DispatchKind::ExternalSend,
                    now + 100 * RETENTION_MS,
                    &cancel
                )
                .await
                .unwrap()
                .is_none()
        );
        let root = ledger.read_root().await.unwrap();
        assert_eq!(root["fired"]["legacy"]["state"], "ambiguous");
        assert_eq!(root["fired"]["future"]["opaque"], true);
    }

    #[tokio::test]
    async fn malformed_ledger_section_fails_closed_without_erasing_claims() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("automations.json");
        let bytes = br#"{"version":1,"fired":"future encoding","futureTop":true}"#;
        fs::write(&path, bytes).await.unwrap();
        let ledger = AutomationLedger::new(&path);
        let cancel = CancellationToken::new();
        assert!(matches!(
            ledger
                .claim_dispatch(
                    &["key".into()],
                    "r",
                    "s",
                    DispatchKind::ExternalSend,
                    now_ms(),
                    &cancel
                )
                .await,
            Err(LedgerError::Invalid { .. })
        ));
        assert_eq!(fs::read(path).await.unwrap(), bytes);
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
            ledger
                .snapshot()
                .await
                .unwrap()
                .breaker_state("r", "s", now + 3)
                .count,
            0
        );
    }

    #[tokio::test]
    async fn checked_claim_serializes_cooldown_across_ledger_instances() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("automations.json");
        let first_ledger = AutomationLedger::new(&path);
        let second_ledger = AutomationLedger::new(&path);
        let first_cancel = CancellationToken::new();
        let second_cancel = CancellationToken::new();
        let now = now_ms();
        let first_keys = ["first-head".to_owned()];
        let second_keys = ["second-head".to_owned()];
        let policy = DispatchPolicy {
            cooldown_ms: Some(60_000),
            breaker_limit: Some(BREAKER_LIMIT),
        };
        let (first, second) = tokio::join!(
            first_ledger.claim_dispatch_checked(
                request(&first_keys, "ci-fix", "feature", policy, now),
                &first_cancel,
            ),
            second_ledger.claim_dispatch_checked(
                request(&second_keys, "ci-fix", "feature", policy, now),
                &second_cancel,
            ),
        );
        let first = first.unwrap();
        let second = second.unwrap();
        assert_eq!(
            usize::from(matches!(first, DispatchClaimResult::Claimed(_)))
                + usize::from(matches!(second, DispatchClaimResult::Claimed(_))),
            1
        );
        assert!(
            matches!(first, DispatchClaimResult::CoolingDown { .. })
                || matches!(second, DispatchClaimResult::CoolingDown { .. })
        );
        let claim = match (first, second) {
            (DispatchClaimResult::Claimed(claim), _) | (_, DispatchClaimResult::Claimed(claim)) => {
                claim
            }
            _ => unreachable!("one concurrent cooldown claim must win"),
        };
        first_ledger
            .mark_delivered(&claim, now + 1, &first_cancel)
            .await
            .unwrap();

        assert!(matches!(
            second_ledger
                .claim_dispatch_checked(
                    request(
                        &["third-head".into()],
                        "ci-fix",
                        "feature",
                        policy,
                        now + 60_001,
                    ),
                    &second_cancel,
                )
                .await
                .unwrap(),
            DispatchClaimResult::Claimed(_)
        ));
    }

    #[tokio::test]
    async fn checked_claim_reserves_breaker_capacity_until_dispatch_is_resolved() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = AutomationLedger::new(dir.path().join("automations.json"));
        let cancel = CancellationToken::new();
        let now = now_ms();
        let policy = DispatchPolicy {
            cooldown_ms: None,
            breaker_limit: Some(BREAKER_LIMIT),
        };
        let first = ledger
            .claim_dispatch_checked(request(&["one".into()], "r", "s", policy, now), &cancel)
            .await
            .unwrap();
        let first = match first {
            DispatchClaimResult::Claimed(claim) => claim,
            other => panic!("unexpected first claim result: {other:?}"),
        };
        let second = ledger
            .claim_dispatch_checked(request(&["two".into()], "r", "s", policy, now + 1), &cancel)
            .await
            .unwrap();
        let second = match second {
            DispatchClaimResult::Claimed(claim) => claim,
            other => panic!("unexpected second claim result: {other:?}"),
        };
        assert!(matches!(
            ledger
                .claim_dispatch_checked(
                    request(&["three".into()], "r", "s", policy, now + 2),
                    &cancel,
                )
                .await
                .unwrap(),
            DispatchClaimResult::BreakerReserved { reserved: 2, .. }
        ));
        ledger
            .mark_delivered(&first, now + 3, &cancel)
            .await
            .unwrap();
        ledger
            .mark_ambiguous(&second, now + 4, "reply lost", &cancel)
            .await
            .unwrap();
        assert!(matches!(
            ledger
                .claim_dispatch_checked(
                    request(&["four".into()], "r", "s", policy, now + 5),
                    &cancel,
                )
                .await
                .unwrap(),
            DispatchClaimResult::BreakerOpen { count: 2, .. }
        ));
    }

    #[tokio::test]
    async fn incomplete_active_claim_timestamp_blocks_cooldown_conservatively() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("automations.json");
        let ledger = AutomationLedger::new(&path);
        let cancel = CancellationToken::new();
        let now = now_ms();
        let policy = DispatchPolicy {
            cooldown_ms: Some(60_000),
            breaker_limit: None,
        };
        let first = ledger
            .claim_dispatch_checked(
                request(&["batch-a".into(), "batch-b".into()], "r", "s", policy, now),
                &cancel,
            )
            .await
            .unwrap();
        assert!(matches!(first, DispatchClaimResult::Claimed(_)));

        let mut root: Value = serde_json::from_slice(&fs::read(&path).await.unwrap()).unwrap();
        root["fired"]["batch-b"]
            .as_object_mut()
            .unwrap()
            .remove("at");
        fs::write(&path, serde_json::to_vec(&root).unwrap())
            .await
            .unwrap();

        assert!(matches!(
            ledger
                .claim_dispatch_checked(
                    request(&["next".into()], "r", "s", policy, now + 1),
                    &cancel,
                )
                .await
                .unwrap(),
            DispatchClaimResult::CoolingDown { until_ms } if until_ms == i64::MAX
        ));
    }
}
