use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use serde_json::{Map, Value, json};
use thiserror::Error;
use tokio::{
    fs,
    io::{AsyncReadExt, AsyncWriteExt},
    sync::OnceCell,
};
use tokio_util::sync::CancellationToken;
use wt_platform::lock::{FileLock, LockError};

const RETENTION_MS: i64 = 30 * 24 * 60 * 60 * 1000;
pub const BREAKER_LIMIT: u64 = 2;
/// Coarsest file timestamp wt can meet (FAT stores two-second mtimes).
const STAMP_SETTLE_SECS: i64 = 2;
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
    /// Taken at this handle's first claim and held until its last clone
    /// drops. Engines in other processes share the ledger; this is how one
    /// starting tells a claim still in flight from one whose owner died.
    owner: Arc<OnceCell<ClaimOwner>>,
    local: Arc<LocalState>,
}

/// Process-local helpers shared by a handle's clones; never authoritative.
#[derive(Debug, Default)]
struct LocalState {
    view: Mutex<Option<(FileStamp, Arc<LedgerView>)>>,
    swept_temporaries: AtomicBool,
}

/// Identity of one ledger file. Every write renames a fresh file into place,
/// so a write by any process changes the inode even when the length and a
/// coarse mtime do not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileStamp {
    dev: u64,
    ino: u64,
    len: u64,
    mtime: (i64, i64),
}

impl FileStamp {
    fn of(metadata: &std::fs::Metadata) -> Self {
        Self {
            dev: metadata.dev(),
            ino: metadata.ino(),
            len: metadata.len(),
            mtime: (metadata.mtime(), metadata.mtime_nsec()),
        }
    }

    /// A freed inode can be handed to a later write. While the mtime is too
    /// recent for the filesystem to tell two such writes apart, the stamp
    /// proves nothing and the file is read.
    fn settled(&self) -> bool {
        (now_ms() / 1000).saturating_sub(self.mtime.0) >= STAMP_SETTLE_SECS
    }
}

struct ClaimOwner {
    id: String,
    lock_path: PathBuf,
    _lock: FileLock,
}

impl std::fmt::Debug for ClaimOwner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClaimOwner").field("id", &self.id).finish()
    }
}

impl Drop for ClaimOwner {
    fn drop(&mut self) {
        // The identity is never reused, so its lock file has no later user.
        let _ = std::fs::remove_file(&self.lock_path);
    }
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
    view: Arc<LedgerView>,
}

#[derive(Debug, Default)]
struct LedgerView {
    fired: Map<String, Value>,
    last_dispatch: Map<String, Value>,
    breaker: Map<String, Value>,
    deferred: Map<String, Value>,
}

impl LedgerView {
    fn from_root(mut root: Value) -> Self {
        let mut take = |field: &str| match root.get_mut(field).map(Value::take) {
            Some(Value::Object(map)) => map,
            _ => Map::new(),
        };
        Self {
            fired: take("fired"),
            last_dispatch: take("lastDispatch"),
            breaker: take("breaker"),
            deferred: take("deferred"),
        }
    }
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

/// One recorded fire: what ran (or was skipped or cancelled), when, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LedgerRecord {
    pub key: String,
    pub rule_id: String,
    pub slug: String,
    pub state: String,
    pub at_ms: i64,
    pub reason: Option<String>,
}

impl LedgerSnapshot {
    /// The newest recorded fires first, for display.
    pub fn recent(&self, limit: usize) -> Vec<LedgerRecord> {
        let mut records = self
            .view
            .fired
            .iter()
            .filter_map(|(key, entry)| {
                Some(LedgerRecord {
                    key: key.clone(),
                    rule_id: entry.get("ruleId")?.as_str()?.to_owned(),
                    slug: entry
                        .get("slug")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    state: entry
                        .get("state")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                        .to_owned(),
                    at_ms: number(entry.get("at"))?,
                    reason: entry
                        .get("reason")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                })
            })
            .collect::<Vec<_>>();
        records.sort_by(|a, b| b.at_ms.cmp(&a.at_ms).then_with(|| a.key.cmp(&b.key)));
        records.truncate(limit);
        records
    }

    pub fn has_handled(&self, key: &str, now_ms: i64) -> bool {
        let Some(entry) = self.view.fired.get(key) else {
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

    /// Whether `key`'s fire has ended, however it ended. A dispatch still in
    /// flight, here or in another process, has not: it may yet be released.
    pub fn settled(&self, key: &str) -> bool {
        self.view
            .fired
            .get(key)
            .is_some_and(|entry| entry.get("state").and_then(Value::as_str) != Some("dispatched"))
    }

    /// Whether `key`'s fire ended with proof that it was delivered. A fire
    /// that ended any other way (sent without proof, skipped, cancelled)
    /// may never have reached its target.
    pub fn delivered(&self, key: &str) -> bool {
        self.view
            .fired
            .get(key)
            .is_some_and(|entry| entry.get("state").and_then(Value::as_str) == Some("delivered"))
    }

    /// Whether `key` was recorded no longer than `window_ms` ago. An entry
    /// without a readable time is not recent.
    pub fn recorded_within(&self, key: &str, now_ms: i64, window_ms: i64) -> bool {
        self.view
            .fired
            .get(key)
            .and_then(|entry| number(entry.get("at")))
            .is_some_and(|at| now_ms.saturating_sub(at) <= window_ms)
    }

    pub fn last_dispatch(&self, rule_id: &str, slug: &str) -> Option<i64> {
        number(self.view.last_dispatch.get(&pair_key(rule_id, slug)))
    }

    /// Whether any breaker record exists for the pair, tripped or not.
    pub fn has_breaker(&self, rule_id: &str, slug: &str) -> bool {
        self.view.breaker.contains_key(&pair_key(rule_id, slug))
    }

    pub fn breaker_state(&self, rule_id: &str, slug: &str, now_ms: i64) -> BreakerState {
        let Some(entry) = self.view.breaker.get(&pair_key(rule_id, slug)) else {
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
            owner: Arc::default(),
            local: Arc::default(),
        }
    }

    fn owner_lock_key(&self, id: &str) -> String {
        format!("{}.owner-{id}", self.lock_key)
    }

    async fn owner_id(&self) -> Result<&str, LedgerError> {
        let owner = self
            .owner
            .get_or_try_init(|| async {
                let id = new_claim_id()?;
                let key = self.owner_lock_key(&id);
                let lock = FileLock::try_acquire(&self.lock_dir, &key, "own automation claims")
                    .await?
                    .ok_or_else(|| LedgerError::Invalid {
                        path: self.path.clone(),
                        message: "claim owner identity is already locked".into(),
                    })?;
                Ok::<_, LedgerError>(ClaimOwner {
                    id,
                    lock_path: self.lock_dir.join(format!("{key}.lock")),
                    _lock: lock,
                })
            })
            .await?;
        Ok(&owner.id)
    }

    /// Owners of unfinished claims whose lock is free, so their process is
    /// gone. An owner that still holds its lock finishes its own claims.
    async fn dead_owners(&self) -> Result<BTreeSet<String>, LedgerError> {
        let root = self.read_root().await?;
        let owners: BTreeSet<String> = root
            .get("fired")
            .and_then(Value::as_object)
            .into_iter()
            .flat_map(Map::values)
            .filter(|entry| entry.get("state").and_then(Value::as_str) == Some("dispatched"))
            .filter_map(|entry| entry.get("owner")?.as_str().map(str::to_owned))
            .collect();
        let mut dead = BTreeSet::new();
        for id in owners {
            let key = self.owner_lock_key(&id);
            if FileLock::try_acquire(&self.lock_dir, &key, "probe automation claim owner")
                .await?
                .is_some()
            {
                let _ = fs::remove_file(self.lock_dir.join(format!("{key}.lock"))).await;
                dead.insert(id);
            }
        }
        Ok(dead)
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
        let owner = self.owner_id().await?.to_owned();
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
                        "owner":owner,
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

    /// Release a claim proven not started and, in the same write, record the
    /// fire as deferred under `id`. A deferred fire must outlive the process:
    /// its row may be gone, so nothing would evaluate it again after a
    /// restart, and its work (a remote branch delete) would never happen.
    pub async fn release_deferred(
        &self,
        claim: &DispatchClaim,
        id: &str,
        record: Value,
        cancellation: &CancellationToken,
    ) -> Result<(), LedgerError> {
        let claim_id = claim.claim_id.clone();
        let keys = claim.keys.clone();
        self.mutate("defer dispatch", cancellation, move |root| {
            let fired = ensure_object(root, "fired");
            for key in &keys {
                let ours = fired
                    .get(key.as_str())
                    .and_then(Value::as_object)
                    .is_some_and(|entry| {
                        entry.get("state").and_then(Value::as_str) == Some("dispatched")
                            && entry.get("claimId").and_then(Value::as_str)
                                == Some(claim_id.as_str())
                    });
                if ours {
                    fired.remove(key);
                }
            }
            ensure_object(root, "deferred").insert(id.to_owned(), record);
        })
        .await
    }

    /// Forget a deferred fire once it reached a terminal outcome.
    pub async fn clear_deferred(
        &self,
        id: &str,
        cancellation: &CancellationToken,
    ) -> Result<(), LedgerError> {
        self.mutate("clear deferral", cancellation, |root| {
            ensure_object(root, "deferred").remove(id);
        })
        .await
    }

    /// Deferred fires recorded by any process, by identity.
    pub async fn deferred(&self) -> Result<BTreeMap<String, Value>, LedgerError> {
        let view = self.view().await?;
        Ok(view
            .deferred
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect())
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
    /// is not proof that no external write happened. A claim whose owner is
    /// still running is left alone: rewriting it would turn that owner's
    /// later release or deferral into a no-op and consume the fire.
    pub async fn reconcile_dispatched(
        &self,
        delivered_keys: &BTreeSet<String>,
        now_ms: i64,
        cancellation: &CancellationToken,
    ) -> Result<(usize, usize), LedgerError> {
        let dead = self.dead_owners().await?;
        self.mutate("reconcile dispatches", cancellation, |root| {
            let mut claims: BTreeMap<String, Vec<String>> = BTreeMap::new();
            {
                let fired = ensure_object(root, "fired");
                for (key, value) in fired.iter().filter(|(_, value)| {
                    value.get("state").and_then(Value::as_str) == Some("dispatched")
                        // An owner first seen after the probe is alive.
                        && value
                            .get("owner")
                            .and_then(Value::as_str)
                            .is_none_or(|owner| dead.contains(owner))
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
        Ok(LedgerSnapshot {
            view: self.view().await?,
        })
    }

    /// The file's current contents, parsed once per file identity. An
    /// evaluator pass asks several times a second and the ledger rarely
    /// changes; the file on disk stays the authority for every answer.
    async fn view(&self) -> Result<Arc<LedgerView>, LedgerError> {
        if let Ok(metadata) = fs::metadata(&self.path).await {
            let current = FileStamp::of(&metadata);
            if current.settled()
                && let Ok(cached) = self.local.view.lock()
                && let Some((stamp, view)) = cached.as_ref()
                && *stamp == current
            {
                return Ok(Arc::clone(view));
            }
        }
        let (root, stamp) = self.read_root_stamped().await?;
        let view = Arc::new(LedgerView::from_root(root));
        self.remember(stamp, &view);
        Ok(view)
    }

    /// Keep a view only when its file was already settled as it was read.
    /// A file read while still fresh could be replaced, and its inode reused
    /// by a same-length write in the same timestamp tick, without the stamp
    /// ever changing; a settled one cannot.
    fn remember(&self, stamp: Option<FileStamp>, view: &Arc<LedgerView>) {
        if let Ok(mut cached) = self.local.view.lock() {
            *cached = stamp
                .filter(FileStamp::settled)
                .map(|stamp| (stamp, Arc::clone(view)));
        }
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
        if !self.local.swept_temporaries.swap(true, Ordering::Relaxed) {
            self.remove_abandoned_temporaries().await;
        }
        let mut root = self.read_root().await?;
        let stored = root.clone();
        prune(&mut root, now_ms());
        let result = mutate(&mut root);
        prune(&mut root, now_ms());
        // Most transitions an evaluation pass asks for change nothing. Each
        // rewrite is a whole-file fsync and rename that every filesystem
        // watcher on the machine then has to process, so an unchanged
        // ledger stays untouched.
        if root != stored {
            self.write_root(&root).await?;
            if let Ok(mut cached) = self.local.view.lock() {
                *cached = None;
            }
        }
        Ok(result)
    }

    /// A writer killed between creating its temporary file and renaming it
    /// leaves a full ledger copy behind. Every writer holds the ledger lock
    /// for the whole life of its temporary file, so under that lock any
    /// temporary file present belongs to a writer that is gone.
    async fn remove_abandoned_temporaries(&self) {
        let prefix = self.path.with_extension("json.tmp-");
        let (Some(parent), Some(prefix)) = (
            self.path.parent(),
            prefix.file_name().and_then(|name| name.to_str()),
        ) else {
            return;
        };
        let Ok(mut entries) = fs::read_dir(parent).await else {
            return;
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let name = entry.file_name();
            let numbered = name
                .to_str()
                .and_then(|name| name.strip_prefix(prefix))
                .and_then(|suffix| suffix.split_once('-'))
                .is_some_and(|(pid, counter)| {
                    [pid, counter].iter().all(|part| {
                        !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit())
                    })
                });
            if numbered {
                let _ = fs::remove_file(entry.path()).await;
            }
        }
    }

    async fn read_root(&self) -> Result<Value, LedgerError> {
        Ok(self.read_root_stamped().await?.0)
    }

    /// The parsed file with the identity of the file those bytes came from;
    /// no identity when there is no file.
    async fn read_root_stamped(&self) -> Result<(Value, Option<FileStamp>), LedgerError> {
        let opened = match fs::File::open(&self.path).await {
            Ok(mut file) => {
                // Stamp the open file, not the path: a rename landing during
                // the read must not label these bytes as the newer file.
                let stamp = file.metadata().await.ok().map(|m| FileStamp::of(&m));
                let mut bytes = Vec::new();
                file.read_to_end(&mut bytes).await.map(|_| (bytes, stamp))
            }
            Err(error) => Err(error),
        };
        match opened {
            Ok((bytes, stamp)) => {
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
                for field in ["fired", "breaker", "lastDispatch", "deferred"] {
                    if value.get(field).is_some_and(|entry| !entry.is_object()) {
                        return Err(LedgerError::Invalid {
                            path: self.path.clone(),
                            message: format!("{field} must be an object when present"),
                        });
                    }
                }
                Ok((value, stamp))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok((
                json!({"version":1,"fired":{},"breaker":{},"lastDispatch":{}}),
                None,
            )),
            Err(source) => Err(io_error("read", &self.path, source)),
        }
    }

    /// Returns the identity of the file now in place.
    async fn write_root(&self, value: &Value) -> Result<Option<FileStamp>, LedgerError> {
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
        let written = async {
            file.write_all(&bytes)
                .await
                .map_err(|source| io_error("write temporary", &temp, source))?;
            file.sync_all()
                .await
                .map_err(|source| io_error("sync temporary", &temp, source))?;
            // Rename keeps the inode, length and mtime read here.
            let stamp = file.metadata().await.ok().map(|m| FileStamp::of(&m));
            drop(file);
            fs::rename(&temp, &self.path)
                .await
                .map_err(|source| io_error("replace", &self.path, source))?;
            Ok(stamp)
        }
        .await;
        if written.is_err() {
            // A failed write must not strand a pretty-printed ledger copy.
            let _ = fs::remove_file(&temp).await;
        }
        written
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
    // A deferral older than the retention window is waiting on something
    // that will not change (an unreachable GitHub, an abandoned child PR).
    if let Some(deferred) = root.get_mut("deferred").and_then(Value::as_object_mut) {
        deferred.retain(|_, entry| number(entry.get("queuedAtMs")).is_none_or(|at| at >= cutoff));
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
        // The claiming process crashed; the next one to start reconciles.
        drop(ledger);
        let ledger = AutomationLedger::new(&path);
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

    /// Two engines share one ledger (a local TUI and an SSH worker, or two
    /// controllers). One starting must not consume the other's dispatch.
    #[tokio::test]
    async fn reconcile_leaves_a_live_owners_claim_until_the_owner_is_gone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("automations.json");
        let running = AutomationLedger::new(&path);
        let starting = AutomationLedger::new(&path);
        let cancel = CancellationToken::new();
        let now = now_ms();
        let claim = |key: &str, at| {
            let keys = [key.to_owned()];
            let running = running.clone();
            let cancel = cancel.clone();
            async move {
                running
                    .claim_dispatch(&keys, "r", "s", DispatchKind::ExternalSend, at, &cancel)
                    .await
                    .unwrap()
                    .unwrap()
            }
        };

        let in_flight = claim("in-flight", now).await;
        assert_eq!(
            starting
                .reconcile_dispatched(&BTreeSet::new(), now + 1, &cancel)
                .await
                .unwrap(),
            (0, 0)
        );
        // The owner's own outcome still applies: a retry releases the key.
        assert_eq!(
            running
                .release_not_started(&in_flight, &cancel)
                .await
                .unwrap(),
            1
        );
        assert!(
            !starting
                .snapshot()
                .await
                .unwrap()
                .has_handled("in-flight", now + 2)
        );

        // An owner that died mid-dispatch proves nothing about delivery.
        claim("crashed", now + 3).await;
        drop(running);
        assert_eq!(
            starting
                .reconcile_dispatched(&BTreeSet::new(), now + 4, &cancel)
                .await
                .unwrap(),
            (0, 1)
        );
        let root = starting.read_root().await.unwrap();
        assert_eq!(root["fired"]["crashed"]["state"], "ambiguous");
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".owner-"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
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

    #[cfg(unix)]
    #[tokio::test]
    async fn transitions_that_change_nothing_leave_the_file_untouched() {
        use std::os::unix::fs::MetadataExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("automations.json");
        let ledger = AutomationLedger::new(&path);
        let cancel = CancellationToken::new();
        let now = now_ms();
        let keys = vec!["k".to_owned()];
        assert!(
            ledger
                .mark_skipped(&keys, "r", "s", now, &cancel)
                .await
                .unwrap()
        );
        // Every write replaces the file, so its inode identifies a write.
        let written = std::fs::metadata(&path).unwrap().ino();

        assert!(!ledger.reset_breaker("r", "s", &cancel).await.unwrap());
        assert!(
            !ledger
                .mark_skipped(&keys, "r", "s", now + 1, &cancel)
                .await
                .unwrap()
        );
        assert_eq!(std::fs::metadata(&path).unwrap().ino(), written);

        ledger
            .bump_breaker("r", "s", now + 2, &cancel)
            .await
            .unwrap();
        assert_ne!(std::fs::metadata(&path).unwrap().ino(), written);
        assert!(ledger.snapshot().await.unwrap().has_breaker("r", "s"));
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

    fn set_mtime(path: &Path, at: std::time::SystemTime) {
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(at)
            .unwrap();
    }

    fn long_ago() -> std::time::SystemTime {
        std::time::SystemTime::now() - std::time::Duration::from_secs(600)
    }

    #[tokio::test]
    async fn unchanged_ledger_is_parsed_once_and_own_writes_show_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("automations.json");
        let ledger = AutomationLedger::new(&path);
        let cancel = CancellationToken::new();
        let now = now_ms();
        ledger.cancel(&["one".into()], now, &cancel).await.unwrap();

        // Too fresh for the stamp to prove anything: read every time.
        let fresh = ledger.snapshot().await.unwrap();
        assert!(!Arc::ptr_eq(
            &fresh.view,
            &ledger.snapshot().await.unwrap().view
        ));

        set_mtime(&path, long_ago());
        let first = ledger.snapshot().await.unwrap();
        let again = ledger.clone().snapshot().await.unwrap();
        assert!(Arc::ptr_eq(&first.view, &again.view));
        assert!(again.has_handled("one", now));

        ledger.cancel(&["two".into()], now, &cancel).await.unwrap();
        let after = ledger.snapshot().await.unwrap();
        assert!(after.has_handled("two", now));
        assert!(!first.has_handled("two", now));
    }

    #[tokio::test]
    async fn another_writers_change_is_seen_on_the_next_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("automations.json");
        let reader = AutomationLedger::new(&path);
        let writer = AutomationLedger::new(&path);
        let cancel = CancellationToken::new();
        let now = now_ms();
        writer.cancel(&["one".into()], now, &cancel).await.unwrap();
        set_mtime(&path, long_ago());
        assert!(reader.snapshot().await.unwrap().has_handled("one", now));
        assert!(!reader.snapshot().await.unwrap().has_handled("two", now));

        writer.cancel(&["two".into()], now, &cancel).await.unwrap();
        assert!(reader.snapshot().await.unwrap().has_handled("two", now));
        assert!(reader.deferred().await.unwrap().is_empty());

        let claim = writer
            .claim_dispatch(
                &["three".into()],
                "rule",
                "slug",
                DispatchKind::HeadlessAction,
                now,
                &cancel,
            )
            .await
            .unwrap()
            .unwrap();
        writer
            .release_deferred(&claim, "later", json!({"queuedAtMs": now}), &cancel)
            .await
            .unwrap();
        assert!(reader.deferred().await.unwrap().contains_key("later"));
    }

    #[tokio::test]
    async fn replacement_with_the_same_length_and_mtime_is_still_seen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("automations.json");
        let reader = AutomationLedger::new(&path);
        let writer = AutomationLedger::new(&path);
        let now = now_ms();
        let root = |key: &str| json!({"version":1,"fired":{key:{"state":"cancelled","at":now,"ruleId":"","slug":""}}});
        let at = long_ago();
        writer.write_root(&root("aaa")).await.unwrap();
        set_mtime(&path, at);
        let before = std::fs::metadata(&path).unwrap();
        let cached = reader.snapshot().await.unwrap();
        assert!(Arc::ptr_eq(
            &cached.view,
            &reader.snapshot().await.unwrap().view
        ));

        writer.write_root(&root("bbb")).await.unwrap();
        set_mtime(&path, at);
        let after = std::fs::metadata(&path).unwrap();
        assert_eq!(before.len(), after.len());
        assert_eq!(before.modified().unwrap(), after.modified().unwrap());

        let seen = reader.snapshot().await.unwrap();
        assert!(seen.has_handled("bbb", now));
        assert!(!seen.has_handled("aaa", now));
    }

    #[tokio::test]
    async fn first_write_removes_temporaries_a_dead_writer_left() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("automations.json");
        let abandoned = dir.path().join("automations.json.tmp-4194304-7");
        let unrelated = [
            dir.path().join("automations.json.tmp-notes"),
            dir.path().join("automations.json.tmp-12-"),
            dir.path().join("harness.json.tmp-12-0"),
        ];
        for file in unrelated.iter().chain([&abandoned]) {
            std::fs::write(file, b"{}").unwrap();
        }
        let ledger = AutomationLedger::new(&path);
        let cancel = CancellationToken::new();
        ledger.snapshot().await.unwrap();
        assert!(abandoned.exists());
        ledger
            .cancel(&["one".into()], now_ms(), &cancel)
            .await
            .unwrap();
        assert!(!abandoned.exists());
        assert!(unrelated.iter().all(|file| file.exists()));
        assert!(path.exists());
    }
}
